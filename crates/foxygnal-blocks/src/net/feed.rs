//! Bytes from an I/O thread into a source block's output, without `work()`
//! ever waiting.
//!
//! The I/O thread owns a [`Feeder`]; the block owns the [`Feed`]. Between them
//! is a byte ring (`foxygnal_sdr::ring`, the one the SDR drivers use) plus a
//! small ring of events (`dropped`, `connected`) stamped with the byte offset
//! they apply to. The feeder only ever pushes whole items: it keeps a partial
//! item back until the rest arrives (TCP segments and UDP datagrams don't
//! have to end on item boundaries), and after a loss it skips ahead to the
//! sender's item grid so later items stay aligned.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use foxygnal_core::{Error, OutputPort, WorkStatus};
use foxygnal_sdr::ring::{self, Consumer, Producer};

use super::POLL;

#[derive(Clone, Copy, Default)]
struct Event {
    /// Byte offset in the ring's stream of the first item it applies to.
    at: u64,
    kind: u8,
    value: i64,
}

const DROPPED: u8 = 1;
const CONNECTED: u8 = 2;

/// State shared by the block and its I/O thread.
#[derive(Default)]
pub(super) struct Shared {
    stop: AtomicBool,
    /// Why the stream ended, if it failed. Read once, at the end.
    error: Mutex<Option<String>>,
}

impl Shared {
    pub(super) fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    pub(super) fn stop_flag(&self) -> &AtomicBool {
        &self.stop
    }
}

/// The I/O thread's end.
pub(super) struct Feeder {
    data: Producer<u8>,
    events: Producer<Event>,
    item: usize,
    /// Bytes pushed into the ring.
    pushed: u64,
    /// Bytes the sender sent on this connection (received or lost), to find
    /// its item grid after a loss.
    sent: u64,
    /// A partial item waiting for the rest (< `item` bytes).
    carry: Vec<u8>,
    /// Bytes still to skip to get back onto the item grid after a loss.
    skip: usize,
    /// Items lost and not yet reported with a `dropped` event.
    lost: u64,
    connections: i64,
}

impl Feeder {
    fn event(&mut self, kind: u8, value: i64) {
        // A full event ring (the block is far behind) loses the event, not data.
        self.events.push(&[Event {
            at: self.pushed,
            kind,
            value,
        }]);
    }

    /// A new TCP connection: forget any partial item and tag the next item
    /// `connected` (the connection's number, from 1).
    pub(super) fn connected(&mut self) {
        self.carry.clear();
        self.skip = 0;
        self.sent = 0;
        self.connections += 1;
        self.event(CONNECTED, self.connections);
    }

    /// The sender sent `bytes` that never arrived (or that didn't fit).
    pub(super) fn lost(&mut self, bytes: u64) {
        let item = self.item as u64;
        let gone = bytes.saturating_add(self.carry.len() as u64);
        self.lost = self.lost.saturating_add((gone / item).max(1));
        self.carry.clear();
        self.sent = self.sent.wrapping_add(bytes);
        self.skip = ((item - self.sent % item) % item) as usize;
    }

    /// One UDP datagram's payload. All of it goes into the ring, or none
    /// (then it counts as lost): a receiver can't make the sender wait.
    pub(super) fn push_datagram(&mut self, payload: &[u8]) -> bool {
        let len = payload.len() as u64;
        let s = self.skip.min(payload.len());
        let payload = &payload[s..];
        let total = self.carry.len() + payload.len();
        let whole = total / self.item * self.item;
        if whole > self.data.free() {
            self.lost(len);
            return false;
        }
        self.skip -= s;
        self.sent = self.sent.wrapping_add(len);
        if whole == 0 {
            self.carry.extend_from_slice(payload);
            return true;
        }
        if self.lost > 0 {
            self.event(DROPPED, self.lost.min(i64::MAX as u64) as i64);
            self.lost = 0;
        }
        let mut rest = payload;
        if !self.carry.is_empty() {
            let need = self.item - self.carry.len();
            self.carry.extend_from_slice(&rest[..need]);
            self.data.push(&self.carry);
            self.carry.clear();
            rest = &rest[need..];
        }
        let w = rest.len() / self.item * self.item;
        self.data.push(&rest[..w]);
        self.carry.extend_from_slice(&rest[w..]);
        self.pushed += whole as u64;
        true
    }

    /// Bytes from a TCP stream. Waits for room in the ring (which, through
    /// the socket, makes the sender wait). Returns false if stopping.
    pub(super) fn push_stream(&mut self, mut bytes: &[u8], sh: &Shared) -> bool {
        if !self.carry.is_empty() {
            let need = self.item - self.carry.len();
            if bytes.len() < need {
                self.carry.extend_from_slice(bytes);
                return true;
            }
            self.carry.extend_from_slice(&bytes[..need]);
            bytes = &bytes[need..];
            let carry = std::mem::take(&mut self.carry);
            let ok = self.write_waiting(&carry, sh);
            self.carry = carry;
            self.carry.clear();
            if !ok {
                return false;
            }
        }
        let w = bytes.len() / self.item * self.item;
        if !self.write_waiting(&bytes[..w], sh) {
            return false;
        }
        self.carry.extend_from_slice(&bytes[w..]);
        true
    }

    /// Write whole items, parking while the ring is full (the block's `pop`
    /// unparks us).
    fn write_waiting(&mut self, mut b: &[u8], sh: &Shared) -> bool {
        while !b.is_empty() {
            let n = (self.data.free() / self.item * self.item).min(b.len());
            if n == 0 {
                if sh.stopping() {
                    return false;
                }
                std::thread::park_timeout(POLL);
                continue;
            }
            self.data.push_partial(&b[..n]);
            self.pushed += n as u64;
            b = &b[n..];
        }
        true
    }
}

/// The block's end: owns the I/O thread and stops it when dropped.
pub(super) struct Feed {
    data: Consumer<u8>,
    events: Consumer<Event>,
    item: usize,
    /// Bytes popped from the ring.
    popped: u64,
    /// An event for an item not popped yet.
    next: Option<Event>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Feed {
    /// Start `io` on a thread called `name`, feeding items of `item` bytes
    /// through a ring of at least `capacity` bytes. Call from the block's
    /// (dedicated) thread: that is the thread new data unparks. When `io`
    /// returns, the stream ends, with its error if it failed.
    pub(super) fn spawn<F>(name: &str, item: usize, capacity: usize, io: F) -> Result<Self, Error>
    where
        F: FnOnce(&mut Feeder, &Shared) -> Result<(), String> + Send + 'static,
    {
        let (data_p, data) = ring::ring::<u8>(capacity.max(item * 2));
        let (events_p, events) = ring::ring::<Event>(256);
        data.set_waker(std::thread::current());
        let shared = Arc::new(Shared::default());
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                data_p.set_waker(std::thread::current());
                let mut f = Feeder {
                    data: data_p,
                    events: events_p,
                    item,
                    pushed: 0,
                    sent: 0,
                    carry: Vec::with_capacity(item),
                    skip: 0,
                    lost: 0,
                    connections: 0,
                };
                if let Err(msg) = io(&mut f, &sh)
                    && !sh.stopping()
                {
                    *sh.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(msg);
                }
                // Dropping the feeder closes the ring (after the error is set)
                // and wakes the block.
            })?;
        Ok(Feed {
            data,
            events,
            item,
            popped: 0,
            next: None,
            shared,
            thread: Some(thread),
        })
    }

    /// Move what has arrived into `port` and tag it. Never waits.
    pub(super) fn work(&mut self, port: &mut OutputPort) -> Result<WorkStatus, Error> {
        let item = self.item;
        let out = port.bytes_mut();
        let n = (out.len() / item).min(self.data.len() / item) * item;
        if n == 0 {
            if self.data.finished() {
                let err = self
                    .shared
                    .error
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take();
                return match err {
                    Some(msg) => Err(Error::Other(msg)),
                    None => Ok(WorkStatus::Done),
                };
            }
            return Ok(WorkStatus::Blocked);
        }
        self.data.pop(&mut out[..n]);
        // Data first, then events: an event is pushed before its data, so
        // every event for the items just popped is visible now.
        let (start, end) = (self.popped, self.popped + n as u64);
        loop {
            let ev = match self.next.take() {
                Some(e) => e,
                None => {
                    let mut one = [Event::default()];
                    if self.events.pop(&mut one) == 0 {
                        break;
                    }
                    one[0]
                }
            };
            if ev.at >= end {
                self.next = Some(ev);
                break;
            }
            let index = (ev.at.saturating_sub(start) / item as u64) as usize;
            match ev.kind {
                DROPPED => port.add_tag(index, "dropped", ev.value),
                CONNECTED => port.add_tag(index, "connected", ev.value),
                _ => {}
            }
        }
        self.popped = end;
        port.produce(n / item);
        Ok(WorkStatus::Ok)
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feeder(item: usize, cap: usize) -> (Feeder, Consumer<u8>, Consumer<Event>) {
        let (dp, dc) = ring::ring::<u8>(cap);
        let (ep, ec) = ring::ring::<Event>(16);
        let f = Feeder {
            data: dp,
            events: ep,
            item,
            pushed: 0,
            sent: 0,
            carry: Vec::new(),
            skip: 0,
            lost: 0,
            connections: 0,
        };
        (f, dc, ec)
    }

    fn drain(c: &mut Consumer<u8>) -> Vec<u8> {
        let mut out = vec![0; c.len()];
        c.pop(&mut out);
        out
    }

    #[test]
    fn partial_items_across_datagrams() {
        let (mut f, mut c, _) = feeder(4, 64);
        assert!(f.push_datagram(&[0, 1, 2]));
        assert!(c.is_empty());
        assert!(f.push_datagram(&[3, 4, 5, 6, 7, 8]));
        assert_eq!(drain(&mut c), (0..8).collect::<Vec<u8>>());
        assert!(f.push_datagram(&[9, 10, 11]));
        assert_eq!(drain(&mut c), vec![8, 9, 10, 11]);
    }

    #[test]
    fn loss_realigns_to_the_item_grid() {
        let (mut f, mut c, mut e) = feeder(4, 64);
        assert!(f.push_datagram(&[0, 1, 2, 3, 4, 5])); // 1 item + 2 carried
        f.lost(6); // bytes 6..12 never came
        // The sender's next datagram starts at byte 12: on the grid.
        assert!(f.push_datagram(&[12, 13, 14, 15]));
        assert_eq!(drain(&mut c), vec![0, 1, 2, 3, 12, 13, 14, 15]);
        let mut ev = [Event::default(); 4];
        assert_eq!(e.pop(&mut ev), 1);
        assert_eq!((ev[0].at, ev[0].kind, ev[0].value), (4, DROPPED, 2));
        // Lose 3 bytes (16..19): the next datagram (19..) has 1 byte to skip.
        f.lost(3);
        assert!(f.push_datagram(&[19, 20, 21, 22, 23]));
        assert_eq!(drain(&mut c), vec![20, 21, 22, 23]);
    }

    #[test]
    fn full_ring_drops_whole_datagrams() {
        let (mut f, mut c, mut e) = feeder(4, 8);
        assert!(f.push_datagram(&[1; 8]));
        assert!(!f.push_datagram(&[2; 8]));
        drain(&mut c);
        assert!(f.push_datagram(&[3; 8]));
        assert_eq!(drain(&mut c), vec![3; 8]);
        let mut ev = [Event::default(); 4];
        assert_eq!(e.pop(&mut ev), 1);
        assert_eq!((ev[0].at, ev[0].value), (8, 2));
    }
}
