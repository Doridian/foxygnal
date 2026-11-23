//! Decoded-message lines: `t=… model=… field=value …`, one per event.

use std::fmt::Display;
use std::io::Write as _;

/// The lines decoded from one pulse train, with what every line ends with.
pub struct Out {
    buf: Vec<u8>,
    /// Time of the train (seconds).
    pub t: f64,
    /// Signal level, signal-to-noise ratio (dB) and frequency offset (Hz).
    pub rssi: f32,
    pub snr: f32,
    pub offset: f32,
    /// Lines written for this train.
    pub events: usize,
    /// Hashes and times of recent lines: the same message from the same
    /// packet (a repeat within it, or the other detector) is written once.
    seen: [(u64, f64); 32],
    nseen: usize,
}

impl Default for Out {
    fn default() -> Self {
        Self::new()
    }
}

impl Out {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(4096),
            t: 0.0,
            rssi: 0.0,
            snr: 0.0,
            offset: 0.0,
            events: 0,
            seen: [(0, f64::MIN); 32],
            nseen: 0,
        }
    }

    /// Forget the previous train's lines.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.events = 0;
    }

    pub fn text(&self) -> &[u8] {
        &self.buf
    }

    /// Start a line for `model`; fields are added with the [`Line`]
    /// methods, and the line ends when it is dropped.
    pub fn begin(&mut self, model: &str) -> Line<'_> {
        let start = self.buf.len();
        let _ = write!(self.buf, "t={:.3} ", self.t);
        let body = self.buf.len();
        let _ = write!(self.buf, "model={model}");
        Line {
            out: self,
            start,
            body,
        }
    }
}

/// One line being written. Dropping it finishes the line (or removes it, if
/// the same message was already written for this train).
pub struct Line<'a> {
    out: &'a mut Out,
    start: usize,
    body: usize,
}

impl Line<'_> {
    /// ` key=value`.
    pub fn kv(self, key: &str, v: impl Display) -> Self {
        let _ = write!(self.out.buf, " {key}={v}");
        self
    }

    /// ` key=value` with `digits` decimals.
    pub fn f(self, key: &str, v: f64, digits: usize) -> Self {
        let _ = write!(self.out.buf, " {key}={v:.digits$}");
        self
    }

    /// ` key=0x…` with `width` hex digits.
    pub fn hex(self, key: &str, v: u64, width: usize) -> Self {
        let _ = write!(self.out.buf, " {key}={v:0width$X}");
        self
    }

    /// ` key=…` as hex bytes.
    pub fn bytes(self, key: &str, b: &[u8]) -> Self {
        let _ = write!(self.out.buf, " {key}=");
        for x in b {
            let _ = write!(self.out.buf, "{x:02x}");
        }
        self
    }

    /// ` key=width:count,…` of a width histogram.
    pub fn hist(self, key: &str, bins: &[(f64, u32)]) -> Self {
        let _ = write!(self.out.buf, " {key}=");
        for (i, (w, n)) in bins.iter().enumerate() {
            let sep = if i == 0 { "" } else { "," };
            let _ = write!(self.out.buf, "{sep}{w:.0}:{n}");
        }
        self
    }

    /// The integrity check that passed (`CRC`, `CHECKSUM`, `PARITY`, …).
    pub fn mic(self, kind: &str) -> Self {
        self.kv("mic", kind)
    }
}

impl Drop for Line<'_> {
    fn drop(&mut self) {
        let o = &mut *self.out;
        let h = fnv(&o.buf[self.body..]);
        if o.seen
            .iter()
            .any(|&(x, t)| x == h && (t - o.t).abs() < SAME_PACKET)
        {
            o.buf.truncate(self.start);
            return;
        }
        o.seen[o.nseen] = (h, o.t);
        o.nseen = (o.nseen + 1) % o.seen.len();
        let _ = write!(
            o.buf,
            " rssi_dB={:.1} snr_dB={:.1} offset_kHz={:.1}",
            o.rssi,
            o.snr,
            o.offset * 1e-3
        );
        o.buf.push(b'\n');
        o.events += 1;
    }
}

/// Lines from trains starting this close (seconds) are from the same packet.
const SAME_PACKET: f64 = 0.005;

fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &x| {
        (h ^ u64::from(x)).wrapping_mul(0x100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_repeats() {
        let mut o = Out::new();
        o.t = 1.5;
        o.begin("Test")
            .kv("id", 12)
            .f("temperature_C", 21.3, 1)
            .mic("CRC");
        o.begin("Test")
            .kv("id", 12)
            .f("temperature_C", 21.3, 1)
            .mic("CRC");
        o.begin("Test").kv("id", 13);
        let s = std::str::from_utf8(o.text()).unwrap();
        assert_eq!(o.events, 2);
        assert!(
            s.starts_with("t=1.500 model=Test id=12 temperature_C=21.3 mic=CRC rssi_dB="),
            "{s}"
        );
        assert_eq!(s.lines().count(), 2);
    }
}
