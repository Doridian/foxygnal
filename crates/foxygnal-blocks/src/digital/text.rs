//! Helpers shared by the decoders that turn bits into text lines: escaping
//! decoded text so a message always stays on one line, and a bit clock.

use std::fmt;

/// Decoded bytes shown inside double quotes. `"` and `\` are escaped, CR,
/// LF and tab become `\r` `\n` `\t`, other control characters `\xNN` (or
/// `\u{…}`), and bytes that aren't UTF-8 `\xNN`; other text passes through.
/// The result never contains a line break.
pub(crate) struct Escaped<'a>(pub &'a [u8]);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write;
        for chunk in self.0.utf8_chunks() {
            for c in chunk.valid().chars() {
                match c {
                    '"' => f.write_str("\\\"")?,
                    '\\' => f.write_str("\\\\")?,
                    '\n' => f.write_str("\\n")?,
                    '\r' => f.write_str("\\r")?,
                    '\t' => f.write_str("\\t")?,
                    c if (c as u32) < 0x80 && c.is_control() => write!(f, "\\x{:02x}", c as u32)?,
                    c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                        write!(f, "\\u{{{:x}}}", c as u32)?
                    }
                    c => f.write_char(c)?,
                }
            }
            for b in chunk.invalid() {
                write!(f, "\\x{b:02x}")?;
            }
        }
        Ok(())
    }
}

/// A digital PLL that finds the bit timing of a two-level signal from its
/// zero crossings, which should fall halfway between two decision instants.
/// Feed it one soft sample (the decision threshold already subtracted) per
/// input sample; it returns the value at each decision instant, interpolated
/// between the two samples around it.
pub(crate) struct BitClock {
    /// Bits per sample.
    step: f64,
    /// Fraction of the timing error corrected at each crossing.
    gain: f64,
    /// Bit phase at the previous sample: a decision at 1.0, crossings at 0.5.
    phase: f64,
    prev: f32,
}

impl BitClock {
    pub fn new(samples_per_bit: f64, gain: f64) -> Self {
        Self {
            step: 1.0 / samples_per_bit,
            gain,
            phase: 0.0,
            prev: 0.0,
        }
    }

    #[inline]
    pub fn sample(&mut self, x: f32) -> Option<f32> {
        let prev = self.prev;
        self.prev = x;
        if (x >= 0.0) != (prev >= 0.0) {
            // Where between the two samples the signal crossed zero.
            let d = prev - x;
            let frac = if d != 0.0 {
                f64::from(prev / d).clamp(0.0, 1.0)
            } else {
                0.5
            };
            let at = self.phase + frac * self.step;
            let err = (at - 0.5) - (at - 0.5).round();
            self.phase -= self.gain * err;
        }
        let next = self.phase + self.step;
        if next >= 1.0 {
            let g = ((1.0 - self.phase) / self.step).clamp(0.0, 1.0) as f32;
            self.phase = next - 1.0;
            Some(prev + g * (x - prev))
        } else {
            self.phase = next;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_keeps_one_line() {
        let s = Escaped(b"a\"b\\c\nd\re\tf\x01\x7f\xff \xc3\xa4").to_string();
        assert_eq!(s, "a\\\"b\\\\c\\nd\\re\\tf\\x01\\x7f\\xff \u{e4}");
        assert_eq!(Escaped("x\u{2028}y".as_bytes()).to_string(), "x\\u{2028}y");
    }

    #[test]
    fn bit_clock_locks_to_a_drifting_square_wave() {
        // Alternating bits at 10.03 samples per bit, nominal 10.
        let mut c = BitClock::new(10.0, 0.2);
        let mut got = Vec::new();
        for i in 0..5000 {
            let bit = ((i as f64 + 3.3) / 10.03) as u64 & 1;
            let x = if bit == 1 { 1.0 } else { -1.0 };
            if let Some(v) = c.sample(x) {
                got.push(v);
            }
        }
        // After settling, every decision is a full ±1, alternating.
        let tail = &got[100..];
        assert!(tail.iter().all(|v| v.abs() > 0.99), "{tail:?}");
        assert!(tail.windows(2).all(|w| w[0] * w[1] < 0.0));
    }
}
