//! Transport stream: the MPEG-TS packets on a `u8` wire (a "TS wire": whole
//! 188-byte packets, each starting with 0x47, no other framing), for the
//! Video instrument, which demuxes and decodes them in the browser.
//!
//! Video runs at 1–20 Mbit/s, far faster than what the shared probe thread
//! can follow on a default ring: a `u8` ring holds 32 KiB, of which a tap can
//! rely on the newest half, so at 60 polls a second it stays contiguous only
//! up to about 1 MB/s. This probe therefore reads its tap on a thread of its
//! own that polls as often as the measured byte rate needs (every 1–4 ms;
//! about 8 MB/s at the fastest), and hands whole packets to the probe thread,
//! which sends them at the instrument's frame rate. Like every probe it never
//! back-pressures the graph: if it falls behind anyway, the bytes are gone
//! and the next frame is marked as a gap.
//!
//! Frame payload: `u32 packets, u8 flags (bit 0: packets were lost before
//! these), u8[3] 0`, then the packets, 188 bytes each, padded to 4 bytes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::Instrument;
use super::source::{Frame, Source};

pub const PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// At most this many bytes of packets wait for the next frame (about 1.6 s at
/// 20 Mbit/s); past that the oldest packets are dropped and the frame is
/// marked as a gap.
const MAX_PENDING: usize = 4 << 20;

/// Splits a byte stream into aligned packets.
///
/// Locks on when it sees the sync byte at three packet starts in a row, and
/// then expects one every 188 bytes. A packet that doesn't start with it
/// loses the lock (counted as a gap) and the search starts again.
#[derive(Default)]
pub struct Packetizer {
    /// Bytes not yet emitted: a cut packet, or the window being searched.
    buf: Vec<u8>,
    locked: bool,
    /// Whole packets, ready to send.
    pub pending: Vec<u8>,
    /// Packets were lost (gap in the stream, lost lock or overflow).
    pub gap: bool,
    /// Bytes skipped while searching for the sync, in total.
    pub skipped: u64,
}

impl Packetizer {
    /// Take the next bytes of the stream; `contiguous` is false if bytes were
    /// lost before them.
    pub fn take(&mut self, bytes: &[u8], contiguous: bool) {
        if !contiguous {
            self.buf.clear();
            self.locked = false;
            self.gap = true;
        }
        self.buf.extend_from_slice(bytes);
        let mut at = 0;
        loop {
            if self.locked {
                if self.buf.len() - at < PACKET {
                    break;
                }
                if self.buf[at] == SYNC {
                    self.pending.extend_from_slice(&self.buf[at..at + PACKET]);
                    at += PACKET;
                    continue;
                }
                self.locked = false;
                self.gap = true;
            }
            // Searching: three sync bytes 188 apart.
            let window = &self.buf[at..];
            if window.len() < 2 * PACKET + 1 {
                break;
            }
            let found = (0..window.len() - 2 * PACKET).find(|&i| {
                window[i] == SYNC && window[i + PACKET] == SYNC && window[i + 2 * PACKET] == SYNC
            });
            match found {
                Some(i) => {
                    self.skipped += i as u64;
                    at += i;
                    self.locked = true;
                }
                None => {
                    // Keep the tail: a packet start may be in it.
                    let keep = 2 * PACKET;
                    let drop = window.len() - keep;
                    self.skipped += drop as u64;
                    at += drop;
                    break;
                }
            }
        }
        self.buf.drain(..at);
        if self.pending.len() > MAX_PENDING {
            // Keep the newest half, in whole packets.
            let cut = (self.pending.len() - MAX_PENDING / 2).next_multiple_of(PACKET);
            self.pending.drain(..cut.min(self.pending.len()));
            self.gap = true;
        }
    }

    /// Move the pending packets into a frame payload.
    pub fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        if self.pending.is_empty() && !self.gap {
            return false;
        }
        Frame(out)
            .u32((self.pending.len() / PACKET) as u32)
            .bytes4([u8::from(self.gap), 0, 0, 0]);
        out.append(&mut self.pending);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        self.gap = false;
        true
    }
}

struct Shared {
    packets: Mutex<Packetizer>,
    stop: AtomicBool,
}

pub struct Ts {
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
}

impl Ts {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                packets: Mutex::new(Packetizer::default()),
                stop: AtomicBool::new(false),
            }),
            reader: None,
        }
    }
}

const MIN_POLL: Duration = Duration::from_millis(1);
const MAX_POLL: Duration = Duration::from_millis(4);

/// The reader thread: poll the tap often enough that the writer never laps
/// it, which needs a read at least every (capacity / 2) / rate.
fn read_loop(mut src: Source, shared: Arc<Shared>) {
    // Bytes the tap can be trusted to still hold, with a safety margin.
    let budget = (src.capacity() / 4).max(PACKET) as f64;
    let mut rate = 0.0f64; // bytes per second, smoothed
    let mut last = Instant::now();
    let mut first = true;
    while !shared.stop.load(Ordering::Relaxed) {
        let (contiguous, bytes) = src.fresh_bytes(usize::MAX);
        // The oldest history may be gone already: not a gap.
        let contiguous = contiguous || std::mem::take(&mut first);
        let n = bytes.len();
        if n > 0 || !contiguous {
            let mut p = shared.packets.lock().unwrap_or_else(|e| e.into_inner());
            p.take(bytes, contiguous);
        }
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f64().max(1e-4);
        last = now;
        // Speed up at once, slow down gently.
        let now_rate = n as f64 / dt;
        rate = now_rate.max(0.9 * rate + 0.1 * now_rate);
        let wait = if rate > 0.0 {
            Duration::from_secs_f64(budget / rate).clamp(MIN_POLL, MAX_POLL)
        } else {
            MAX_POLL
        };
        std::thread::sleep(wait);
    }
}

impl Instrument for Ts {
    fn poll(&mut self, src: &mut Source) {
        if self.reader.is_some() {
            return;
        }
        // Start with what the ring still holds: the first packets arrive at
        // once and a decoder may find a key frame sooner.
        let mut own = src.detach();
        own.rewind();
        let shared = self.shared.clone();
        self.reader = std::thread::Builder::new()
            .name("fox:probe-ts".into())
            .spawn(move || read_loop(own, shared))
            .ok();
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        let mut p = self
            .shared
            .packets
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        p.frame(out)
    }
}

impl Drop for Ts {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.reader.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u16, cc: u8) -> Vec<u8> {
        let mut p = vec![0xFFu8; PACKET];
        p[0] = SYNC;
        p[1] = (pid >> 8) as u8 & 0x1F;
        p[2] = pid as u8;
        p[3] = 0x10 | (cc & 15);
        p
    }

    fn stream(n: usize) -> Vec<u8> {
        (0..n).flat_map(|i| packet(0x100, i as u8)).collect()
    }

    fn ccs(p: &Packetizer) -> Vec<u8> {
        p.pending.chunks(PACKET).map(|c| c[3] & 15).collect()
    }

    #[test]
    fn passes_aligned_packets_through_in_any_pieces() {
        let s = stream(10);
        let mut p = Packetizer::default();
        for piece in s.chunks(97) {
            p.take(piece, true);
        }
        assert_eq!(p.pending, s, "every packet, in order");
        assert!(!p.gap);
        assert_eq!(p.skipped, 0);
    }

    #[test]
    fn finds_the_packets_after_junk() {
        let mut s = vec![0x47, 1, 2, 0x47, 9, 9, 9];
        s.extend(stream(6));
        let mut p = Packetizer::default();
        p.take(&s, true);
        assert_eq!(ccs(&p), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(p.skipped, 7);
    }

    #[test]
    fn resyncs_after_a_cut_and_marks_the_gap() {
        let s = stream(12);
        let mut p = Packetizer::default();
        p.take(&s[..3 * PACKET], true);
        assert_eq!(ccs(&p), vec![0, 1, 2]);
        let mut out = Vec::new();
        assert!(p.frame(&mut out));
        assert_eq!(&out[..4], &3u32.to_le_bytes());
        assert_eq!(out[4], 0, "no gap");
        assert_eq!(out.len(), 8 + (3 * PACKET).next_multiple_of(4));
        // Lost bytes: the stream picks up mid-packet.
        p.take(&s[5 * PACKET + 50..], false);
        assert_eq!(ccs(&p), vec![6, 7, 8, 9, 10, 11]);
        out.clear();
        assert!(p.frame(&mut out));
        assert_eq!(out[4], 1, "gap");
        out.clear();
        assert!(!p.frame(&mut out), "nothing new");
    }

    #[test]
    fn a_broken_packet_drops_the_lock() {
        let mut s = stream(8);
        // Packet 3 loses a byte: everything after it is shifted by one.
        s.remove(3 * PACKET + 10);
        let mut p = Packetizer::default();
        p.take(&s, true);
        assert!(p.gap);
        // 3 still starts with the sync, so it passes, one byte short and
        // with 4's sync byte at its end; 4 is lost with the lock, which is
        // found again at 5.
        assert_eq!(ccs(&p), vec![0, 1, 2, 3, 5, 6, 7]);
    }

    #[test]
    fn overflow_keeps_the_newest_whole_packets() {
        let n = MAX_PENDING / PACKET + 100;
        let s = stream(n);
        let mut p = Packetizer::default();
        p.take(&s, true);
        assert!(p.gap);
        assert!(p.pending.len() <= MAX_PENDING);
        assert_eq!(p.pending.len() % PACKET, 0);
        assert_eq!(p.pending[0], SYNC);
        assert_eq!(
            &p.pending[p.pending.len() - PACKET..],
            &s[s.len() - PACKET..]
        );
    }

    #[test]
    fn the_reader_thread_keeps_up_with_a_fast_writer() {
        use foxygnal_core::StreamType;
        use foxygnal_core::buffer;
        // A default-size u8 ring written at 16 Mbit/s for about half a second.
        let (mut w, mut rs) = buffer::channel(1, buffer::DEFAULT_CAPACITY, 1);
        let mut src = Source::new(w.tap(), StreamType::U8);
        let mut ts = Ts::new();
        ts.poll(&mut src);
        std::thread::sleep(Duration::from_millis(20));
        let s = stream(5_000);
        let start = Instant::now();
        let mut done = 0;
        let chunk = 25 * PACKET;
        while done < s.len() {
            let buf = w.slice_mut();
            let n = buf.len().min(chunk).min(s.len() - done);
            buf[..n].copy_from_slice(&s[done..done + n]);
            w.commit(n);
            done += n;
            for r in rs.iter_mut() {
                let k = r.slice().len();
                r.consume(k);
            }
            // Pace: 2 MB/s.
            let due = Duration::from_secs_f64(done as f64 / 2e6);
            if let Some(d) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(d);
            }
        }
        std::thread::sleep(Duration::from_millis(30));
        let mut out = Vec::new();
        assert!(ts.frame(&mut out));
        let n = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        assert_eq!(out[4], 0, "no gap");
        assert_eq!(n, 5_000, "every packet");
    }
}
