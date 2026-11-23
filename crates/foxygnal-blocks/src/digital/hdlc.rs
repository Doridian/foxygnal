//! HDLC deframing: bits in (one per byte, e.g. from the AFSK demodulator),
//! frames out. NRZI decoding (a change is 0, no change is 1), flag (0x7E)
//! detection, removal of the 0 stuffed after five 1s, an abort on seven 1s,
//! and the FCS check (CRC-16-CCITT as in X.25 / AX.25: reflected, initial
//! value and final XOR 0xFFFF, sent low byte first). Bits of each byte arrive
//! least significant first.
//!
//! Frames go out as AX.25/APRS text lines, as hex text lines, or as raw bytes
//! (the first byte of each frame tagged `packet` with its length) for other
//! HDLC users.

use std::fmt::Write;

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, TagPropagation, WorkIo, WorkStatus,
};
use serde_json::json;

use super::packet::PACKET_TAG;
use super::{MsgQueue, ax25, idle_status, text_meta};
use crate::design;

/// Longest frame kept, FCS included; longer ones are dropped.
pub const MAX_FRAME: usize = 2048;
/// Shortest frame: one byte and the FCS.
const MIN_FRAME: usize = 3;

pub fn register(r: &mut Registry) {
    let u8_port = |name| PortSpec {
        name,
        ty: Some(StreamType::U8),
    };
    r.register(
        BlockDescriptor {
            kind: "hdlc_deframer",
            label: "HDLC Deframer",
            category: "Digital",
            help: "Finds HDLC frames in a bit stream (one bit per byte, e.g. from the AFSK \
                   1200 Demodulator): NRZI decoding, 0x7E flags, bit unstuffing, FCS \
                   (CRC-16-CCITT) check. AX.25: one text line per frame with the call signs, \
                   path and the decoded APRS report, e.g. `t=12.345 src=N0CALL-9 dst=APRS \
                   path=WIDE1-1,WIDE2-1 type=position lat=49.058333 lon=-72.029167 sym=/> \
                   comment=\"…\"`. Hex: `t=… len=… fcs=ok data=…`. Raw: the frame bytes \
                   without FCS, the first tagged `packet` (the length).",
            inputs: vec![u8_port("in")],
            outputs: vec![u8_port("out")],
            params: vec![
                design::choice(
                    "format",
                    "Output",
                    vec!["ax25", "hex", "raw"],
                    "ax25",
                    "AX.25: decoded AX.25 / APRS text lines (frames that aren't AX.25 come \
                     out as hex). Hex: every frame as a hex text line. Raw: frame bytes.",
                ),
                ParamSpec {
                    name: "nrzi",
                    label: "NRZI",
                    kind: ParamKind::Bool,
                    default: json!(true),
                    help: "Undo NRZI coding (AX.25 and most HDLC links use it). Off: the \
                           input bits are the data bits.",
                    live: false,
                },
                ParamSpec {
                    name: "check_fcs",
                    label: "Check FCS",
                    kind: ParamKind::Bool,
                    default: json!(true),
                    help: "Drop frames whose FCS is wrong. Off: pass them too, marked \
                           `fcs=bad` (text formats), for debugging a weak signal.",
                    live: true,
                },
            ],
        },
        build,
    );
}

/// The FCS of `data`: CRC-16-CCITT, reflected, as in X.25 and AX.25.
pub fn fcs(data: &[u8]) -> u16 {
    !data.iter().fold(0xFFFFu16, |crc, &b| {
        (crc >> 8) ^ FCS_TABLE[usize::from((crc as u8) ^ b)]
    })
}

const fn fcs_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x8408
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static FCS_TABLE: [u16; 256] = fcs_table();

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Ax25,
    Hex,
    Raw,
}

pub struct HdlcDeframer {
    format: Format,
    nrzi: bool,
    check_fcs: bool,
    rate: Option<f64>,
    /// Previous line bit (NRZI).
    prev: u8,
    /// Consecutive 1s.
    ones: u32,
    /// Between two flags, collecting.
    in_frame: bool,
    buf: Vec<u8>,
    nbits: usize,
    /// Bits seen so far (the time base of `t=`).
    pos: u64,
    /// Length of the raw frame queued (for its tag).
    raw_len: usize,
    text: String,
    queue: MsgQueue,
}

impl HdlcDeframer {
    pub fn new(format: &str, nrzi: bool, check_fcs: bool) -> Self {
        Self {
            format: match format {
                "hex" => Format::Hex,
                "raw" => Format::Raw,
                _ => Format::Ax25,
            },
            nrzi,
            check_fcs,
            rate: None,
            prev: 0,
            ones: 0,
            in_frame: false,
            buf: vec![0; MAX_FRAME + 1],
            nbits: 0,
            pos: 0,
            raw_len: 0,
            text: String::with_capacity(4 * MAX_FRAME + 256),
            queue: MsgQueue::new(),
        }
    }

    #[inline]
    fn push(&mut self, b: u8) {
        if self.nbits >= 8 * MAX_FRAME + 7 {
            self.in_frame = false; // too long: wait for the next flag
            return;
        }
        let (byte, bit) = (self.nbits / 8, self.nbits % 8);
        if bit == 0 {
            self.buf[byte] = 0;
        }
        self.buf[byte] |= b << bit;
        self.nbits += 1;
    }

    /// One line bit. Returns true when a frame was queued.
    #[inline]
    fn bit(&mut self, line: u8) -> bool {
        self.pos += 1;
        let line = line & 1;
        let b = if self.nrzi {
            let v = u8::from(line == self.prev);
            self.prev = line;
            v
        } else {
            line
        };
        if b == 1 {
            self.ones += 1;
            if self.ones >= 7 {
                self.in_frame = false; // abort, or an idle line
            } else if self.in_frame {
                self.push(1);
            }
            return false;
        }
        let ones = std::mem::replace(&mut self.ones, 0);
        match ones {
            6 => {
                // A flag: the frame (if any) ends, the next one may start.
                let done = self.in_frame && self.finish();
                self.in_frame = true;
                self.nbits = 0;
                done
            }
            5 => false, // stuffed
            _ => {
                if self.in_frame {
                    self.push(0);
                }
                false
            }
        }
    }

    /// The closing flag arrived: check the frame and queue it.
    fn finish(&mut self) -> bool {
        // The flag's first seven bits were collected as data.
        let Some(n) = self.nbits.checked_sub(7) else {
            return false;
        };
        if !n.is_multiple_of(8) || n / 8 < MIN_FRAME {
            return false;
        }
        let len = n / 8 - 2;
        let got = u16::from(self.buf[len]) | u16::from(self.buf[len + 1]) << 8;
        let ok = fcs(&self.buf[..len]) == got;
        if !ok && self.check_fcs {
            return false;
        }
        let data = &self.buf[..len];
        if self.format == Format::Raw {
            self.raw_len = len;
            self.queue.bytes(data);
            return true;
        }
        let out = &mut self.text;
        out.clear();
        if let Some(rate) = self.rate {
            let _ = write!(out, "t={:.3} ", self.pos as f64 / rate);
        }
        if !ok {
            out.push_str("fcs=bad ");
        }
        if !(self.format == Format::Ax25 && ax25::describe(data, out)) {
            if self.format == Format::Ax25 {
                out.push_str("type=hdlc ");
            }
            let _ = write!(out, "len={len} data=");
            for b in data {
                let _ = write!(out, "{b:02x}");
            }
        }
        out.push('\n');
        self.queue.bytes(self.text.as_bytes());
        true
    }
}

impl Block for HdlcDeframer {
    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        self.rate = cx.inputs.first().and_then(|m| m.sample_rate);
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (mut used, mut made) = (0, 0);
        loop {
            let out = io.outputs[0].slice_mut::<u8>();
            let start = self.queue.at_start();
            let n = self.queue.flush(&mut out[made..]);
            if n > 0 && start && self.format == Format::Raw {
                io.outputs[0].add_tag(made, PACKET_TAG, self.raw_len as i64);
            }
            made += n;
            if !self.queue.is_empty() {
                break;
            }
            let inp = io.inputs[0].slice::<u8>();
            if used == inp.len() {
                break;
            }
            while used < inp.len() {
                let done = self.bit(inp[used]);
                used += 1;
                if done {
                    break;
                }
            }
        }
        if used == 0 && made == 0 {
            return Ok(idle_status(io, &self.queue, 0));
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "check_fcs" => self.check_fcs = value.as_bool().unwrap_or(true),
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(HdlcDeframer::new(
            p.str("format")?,
            p.bool("nrzi")?,
            p.bool("check_fcs")?,
        )),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fcs_matches_the_x25_check_value() {
        // CRC-16/X-25 of "123456789" is 0x906E.
        assert_eq!(fcs(b"123456789"), 0x906E);
    }
}
