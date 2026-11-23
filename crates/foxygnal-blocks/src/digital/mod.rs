//! Digital modes: synchronisation (clock recovery, Costas loop), modems
//! (slicer, PSK demapper, FSK demodulator), packet framing and decoders.
//!
//! ## Decoded messages are text streams
//!
//! A decoder that finds something a person wants to read (an ADS-B position,
//! a packet) writes it as one line of UTF-8 text, `\n`-terminated, to a `u8`
//! output. Such a wire can be watched with the *Messages* instrument (the
//! `text` probe, available on any `u8` wire), ended in a `message_log` block
//! (which can echo lines to the engine's stdout for headless runs), or written
//! to disk with a plain `file_sink`. Nothing about this is decoder-specific:
//! any block that emits text lines on a `u8` port gets all three for free.
//!
//! Formatting may allocate only when a message is actually decoded; the
//! queue below is preallocated, so in practice it doesn't.

pub mod acars;
mod adsb;
mod afsk;
pub mod ais;
pub mod ax25;
mod flex;
pub mod hdlc;
mod modem;
mod packet;
pub mod pocsag;
mod sync;
mod text;
mod uat;

use std::time::Duration;

use foxygnal_core::{Complex32, Configure, Registry, Sample, WorkIo, WorkStatus};

pub use adsb::{AdsbDecoder, Cpr, ModeS, cpr_global};
pub use uat::UatDecoder;

pub fn register(r: &mut Registry) {
    sync::register(r);
    modem::register(r);
    packet::register(r);
    adsb::register(r);
    acars::register(r);
    ais::register(r);
    uat::register(r);
    pocsag::register(r);
    flex::register(r);
    afsk::register(r);
    hdlc::register(r);
}

/// Text (or packet bytes) waiting to go out on a `u8` port, one message at a
/// time. Decoders only take new input while the queue is empty, so it never
/// grows past one message and a stalled consumer back-pressures the decoder.
pub(crate) struct MsgQueue {
    buf: Vec<u8>,
    sent: usize,
}

/// Room reserved for one message; longer ones still work (the Vec grows once).
const MSG_CAPACITY: usize = 4096;

impl MsgQueue {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(MSG_CAPACITY),
            sent: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sent == self.buf.len()
    }

    /// Bytes queued and not sent yet.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.sent
    }

    /// Queue text (no newline: end the message with [`Self::line`]).
    pub fn text(&mut self, args: std::fmt::Arguments<'_>) {
        use std::io::Write;
        // Writing to a Vec can't fail.
        let _ = self.buf.write_fmt(args);
    }

    /// Queue a line of text (a `\n` is added).
    pub fn line(&mut self, args: std::fmt::Arguments<'_>) {
        self.text(args);
        self.buf.push(b'\n');
    }

    /// Queue raw bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// True if nothing of the queued message has been sent yet.
    pub fn at_start(&self) -> bool {
        self.sent == 0 && !self.buf.is_empty()
    }

    /// Copy as much as fits into `out`; returns how many bytes were written.
    pub fn flush(&mut self, out: &mut [u8]) -> usize {
        let n = (self.buf.len() - self.sent).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.sent..self.sent + n]);
        self.sent += n;
        if self.sent == self.buf.len() {
            self.buf.clear();
            self.sent = 0;
        }
        n
    }
}

/// What a decoder returns after a `work` call that made no progress.
///
/// When input has run out but a message is still queued (the consumer is
/// full), plain `Blocked` would end the block and lose the message, so it
/// waits for the consumer instead, unless the consumer has left.
pub(crate) fn idle_status(io: &WorkIo<'_>, queue: &MsgQueue, out_port: usize) -> WorkStatus {
    if !queue.is_empty()
        && io.inputs.iter().any(|i| i.exhausted())
        && !io.outputs[out_port].abandoned()
    {
        WorkStatus::Wait(Duration::from_millis(20))
    } else {
        WorkStatus::Blocked
    }
}

/// A text output has no sample rate or centre frequency.
pub(crate) fn text_meta(cx: &mut Configure<'_>, port: usize) {
    if let Some(o) = cx.outputs.get_mut(port) {
        o.sample_rate = None;
        o.center_freq = None;
    }
}

/// Proportional and integral gains of a second-order loop with damping 1/√2,
/// for a normalized loop bandwidth `bw` (per update) and a detector of unit
/// gain.
pub(crate) fn loop_gains(bw: f64) -> (f64, f64) {
    let zeta = std::f64::consts::FRAC_1_SQRT_2;
    let theta = bw / (zeta + 0.25 / zeta);
    let d = 1.0 + 2.0 * zeta * theta + theta * theta;
    (4.0 * zeta * theta / d, 4.0 * theta * theta / d)
}

/// Sample types the symbol-level blocks work on (f32 and c32).
pub trait Symbol:
    Sample
    + Default
    + std::ops::Add<Output = Self>
    + std::ops::Sub<Output = Self>
    + std::ops::Mul<f32, Output = Self>
{
    /// `Re{a·conj(b)}`: the correlation of two symbols.
    fn dot(self, other: Self) -> f32;
    /// The nearest point of a ±1 (per axis) constellation.
    fn decide(self) -> Self;
}

impl Symbol for f32 {
    fn dot(self, other: Self) -> f32 {
        self * other
    }
    fn decide(self) -> Self {
        if self >= 0.0 { 1.0 } else { -1.0 }
    }
}

impl Symbol for Complex32 {
    fn dot(self, other: Self) -> f32 {
        self.re * other.re + self.im * other.im
    }
    fn decide(self) -> Self {
        Complex32::new(self.re.decide(), self.im.decide())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_flushes_in_pieces_and_resets() {
        let mut q = MsgQueue::new();
        assert!(q.is_empty());
        q.line(format_args!("abc {}", 12));
        assert!(q.at_start());
        let mut out = [0u8; 4];
        assert_eq!(q.flush(&mut out), 4);
        assert_eq!(&out, b"abc ");
        assert!(!q.at_start() && !q.is_empty());
        assert_eq!(q.flush(&mut out), 3);
        assert_eq!(&out[..3], b"12\n");
        assert!(q.is_empty());
        assert_eq!(q.flush(&mut out), 0);
    }

    #[test]
    fn loop_gains_are_small_and_positive() {
        let (kp, ki) = loop_gains(0.01);
        assert!(kp > 0.0 && kp < 0.05, "{kp}");
        assert!(ki > 0.0 && ki < kp, "{ki}");
    }
}
