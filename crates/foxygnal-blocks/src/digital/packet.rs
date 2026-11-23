//! Packet framing: finding a sync word in a bit stream, collecting the
//! payload after it, and a sink for text messages.

use std::io::Write;

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, TagPropagation, WorkIo, WorkStatus,
};
use serde_json::json;

use super::{MsgQueue, idle_status, text_meta};
use crate::design;

/// Key of the tag that marks the first payload bit after a sync word.
pub const PACKET_TAG: &str = "packet";

fn u8_port(name: &'static str) -> PortSpec {
    PortSpec {
        name,
        ty: Some(StreamType::U8),
    }
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "correlate_access_code",
            label: "Correlate Access Code",
            category: "Digital",
            help: "Looks for a sync word in a bit stream (one bit per byte, as from a binary \
                   slicer) and tags the first bit after it `packet` (the value is the number \
                   of mismatched bits). The bits pass through unchanged.",
            inputs: vec![u8_port("in")],
            outputs: vec![u8_port("out")],
            params: vec![
                ParamSpec {
                    name: "access_code",
                    label: "Access code",
                    kind: ParamKind::String,
                    default: json!("0x1ACFFC1D"),
                    help: "The sync word, first bit first: hex with a 0x prefix (4 bits per \
                           digit), or a string of 0s and 1s. Up to 64 bits; spaces and _ \
                           are ignored.",
                    live: false,
                },
                design::int(
                    "threshold",
                    "Allowed errors",
                    2,
                    0,
                    "Bits that may differ from the access code and still count as a match.",
                ),
            ],
        },
        build_correlate,
    );
    r.register(
        BlockDescriptor {
            kind: "packet_decoder",
            label: "Packet Decoder",
            category: "Digital",
            help: "Collects the payload after each `packet` tag (from Correlate Access Code): \
                   `length` bytes, bits packed most significant first. Hex: one line of text \
                   per packet, for the Messages instrument or a Message Log. Raw: the bytes, \
                   the first of each packet tagged `packet` with the length. Tags that arrive \
                   while a packet is being collected are ignored.",
            inputs: vec![u8_port("in")],
            outputs: vec![u8_port("out")],
            params: vec![
                ParamSpec {
                    name: "length",
                    label: "Payload length",
                    kind: ParamKind::Int {
                        min: Some(1),
                        max: Some(MAX_PACKET as i64),
                    },
                    default: json!(16),
                    help: "Payload bytes per packet.",
                    live: false,
                },
                design::choice(
                    "format",
                    "Output",
                    vec!["hex", "raw"],
                    "hex",
                    "Hex: text lines (`len=16 errors=0 data=…`). Raw: the payload bytes.",
                ),
            ],
        },
        build_packet,
    );
    r.register(
        BlockDescriptor {
            kind: "message_log",
            label: "Message Log",
            category: "Sinks",
            help: "The end of a decoder's text output (one message per line). Click the wire \
                   into it and choose Messages to read them in the browser; for headless runs \
                   it can also print every line on the engine's standard output.",
            inputs: vec![u8_port("in")],
            outputs: vec![],
            params: vec![ParamSpec {
                name: "stdout",
                label: "Print to stdout",
                kind: ParamKind::Bool,
                default: json!(false),
                help: "Also write each line to the engine's standard output (the terminal \
                       of `foxygnal run`).",
                live: true,
            }],
        },
        build_log,
    );
}

// ---- Correlate access code -----------------------------------------------------

/// Parse an access code: `0x…` hex or a bit string. Returns (bits, length).
pub fn parse_access_code(s: &str) -> Result<(u64, u32), String> {
    let clean: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .collect();
    let (mut code, mut len) = (0u64, 0u32);
    let mut push = |v: u64, bits: u32| {
        len += bits;
        if len > 64 {
            return Err(format!("access code `{s}` is longer than 64 bits"));
        }
        code = (code << bits) | v;
        Ok(())
    };
    if let Some(hex) = clean
        .strip_prefix("0x")
        .or_else(|| clean.strip_prefix("0X"))
    {
        for c in hex.chars() {
            let d = c
                .to_digit(16)
                .ok_or_else(|| format!("access code `{s}`: `{c}` is not a hex digit"))?;
            push(u64::from(d), 4)?;
        }
    } else {
        for c in clean.chars() {
            match c {
                '0' | '1' => push(u64::from(c == '1'), 1)?,
                _ => {
                    return Err(format!(
                        "access code `{s}`: write it as hex with a 0x prefix or as 0s and 1s \
                         (found `{c}`)"
                    ));
                }
            }
        }
    }
    if len == 0 {
        return Err("the access code is empty".into());
    }
    Ok((code, len))
}

pub struct CorrelateAccessCode {
    code: u64,
    mask: u64,
    len: u32,
    threshold: u32,
    /// The last bits seen, newest in bit 0.
    reg: u64,
    /// Bits seen so far (saturating at `len`).
    seen: u32,
    /// A match ended on the last item of the previous call: tag the next one.
    pending: Option<u32>,
}

impl Block for CorrelateAccessCode {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let inp = io.inputs[0].slice::<u8>();
        let port = &mut io.outputs[0];
        let out = port.slice_mut::<u8>();
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        out[..n].copy_from_slice(&inp[..n]);
        if let Some(errors) = self.pending.take() {
            port.add_tag(0, PACKET_TAG, i64::from(errors));
        }
        let (mut reg, mut seen) = (self.reg, self.seen);
        for (i, &b) in inp[..n].iter().enumerate() {
            reg = (reg << 1) | u64::from(b & 1);
            seen = (seen + 1).min(self.len);
            if seen < self.len {
                continue;
            }
            let errors = ((reg ^ self.code) & self.mask).count_ones();
            if errors <= self.threshold {
                if i + 1 < n {
                    port.add_tag(i + 1, PACKET_TAG, i64::from(errors));
                } else {
                    self.pending = Some(errors);
                }
                // Don't match again on bits of this sync word.
                seen = 0;
            }
        }
        (self.reg, self.seen) = (reg, seen);
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_correlate(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let (code, len) = parse_access_code(p.str("access_code")?).map_err(Error::Other)?;
    let threshold = p.u64("threshold")?.min(64) as u32;
    Ok(Instance {
        block: Box::new(CorrelateAccessCode {
            code,
            mask: if len == 64 { u64::MAX } else { (1 << len) - 1 },
            len,
            threshold,
            reg: 0,
            seen: 0,
            pending: None,
        }),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- Packet decoder ------------------------------------------------------------

const MAX_PACKET: usize = 4096;

pub struct PacketDecoder {
    length: usize,
    hex: bool,
    /// Payload being collected, and the bits in it so far.
    payload: Vec<u8>,
    bits: usize,
    collecting: bool,
    errors: i64,
    queue: MsgQueue,
}

impl PacketDecoder {
    fn finish_packet(&mut self) {
        self.collecting = false;
        if self.hex {
            let mut hex = [0u8; 2 * MAX_PACKET];
            for (i, b) in self.payload.iter().enumerate() {
                const DIGITS: &[u8; 16] = b"0123456789abcdef";
                hex[2 * i] = DIGITS[usize::from(b >> 4)];
                hex[2 * i + 1] = DIGITS[usize::from(b & 15)];
            }
            let hex = std::str::from_utf8(&hex[..2 * self.length]).unwrap_or_default();
            self.queue.line(format_args!(
                "len={} errors={} data={hex}",
                self.length, self.errors
            ));
        } else {
            self.queue.bytes(&self.payload);
        }
    }
}

impl Block for PacketDecoder {
    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let mut made = 0;
        let mut used = 0;
        loop {
            // Send what's queued first; take input only once it's all out.
            let out = io.outputs[0].slice_mut::<u8>();
            let start = self.queue.at_start();
            let n = self.queue.flush(&mut out[made..]);
            if n > 0 && start && !self.hex {
                io.outputs[0].add_tag(made, PACKET_TAG, self.length as i64);
            }
            made += n;
            if !self.queue.is_empty() {
                break;
            }
            let input = &io.inputs[0];
            let inp = input.slice::<u8>();
            if used == inp.len() {
                break;
            }
            let base = input.position();
            let tags = input.tags();
            while used < inp.len() {
                if !self.collecting {
                    let at = base + used as u64;
                    let lo = tags.partition_point(|t| t.offset < at);
                    let next = tags[lo..]
                        .iter()
                        .find(|t| &*t.key == PACKET_TAG && t.offset < base + inp.len() as u64);
                    match next {
                        Some(t) => {
                            used = (t.offset - base) as usize;
                            self.collecting = true;
                            self.bits = 0;
                            self.payload.fill(0);
                            self.errors = match t.value {
                                foxygnal_core::TagValue::Int(e) => e,
                                _ => 0,
                            };
                        }
                        None => {
                            used = inp.len();
                            break;
                        }
                    }
                }
                let take = (self.length * 8 - self.bits).min(inp.len() - used);
                for &b in &inp[used..used + take] {
                    self.payload[self.bits / 8] |= (b & 1) << (7 - self.bits % 8);
                    self.bits += 1;
                }
                used += take;
                if self.bits == self.length * 8 {
                    self.finish_packet();
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
}

fn build_packet(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let length = (p.u64("length")? as usize).clamp(1, MAX_PACKET);
    Ok(Instance {
        block: Box::new(PacketDecoder {
            length,
            hex: p.str("format")? == "hex",
            payload: vec![0; length],
            bits: 0,
            collecting: false,
            errors: 0,
            queue: MsgQueue::new(),
        }),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- Message log ---------------------------------------------------------------

/// Longest partial line kept back for stdout before it's printed anyway.
const LINE_MAX: usize = 4096;

pub struct MessageLog {
    stdout: bool,
    /// The start of a line whose end hasn't arrived yet.
    partial: Vec<u8>,
}

impl Block for MessageLog {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let inp = io.inputs[0].slice::<u8>();
        let n = inp.len();
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        if self.stdout {
            // Print whole lines only, so two logs don't interleave mid-line.
            match inp.iter().rposition(|&b| b == b'\n') {
                Some(end) => {
                    let mut o = std::io::stdout().lock();
                    let _ = o.write_all(&self.partial);
                    let _ = o.write_all(&inp[..=end]);
                    let _ = o.flush();
                    self.partial.clear();
                    self.partial.extend_from_slice(&inp[end + 1..]);
                }
                None => self.partial.extend_from_slice(inp),
            }
            if self.partial.len() > LINE_MAX {
                let mut o = std::io::stdout().lock();
                let _ = o.write_all(&self.partial);
                let _ = o.flush();
                self.partial.clear();
            }
        }
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "stdout" => {
                self.stdout = value.as_bool().unwrap_or(false);
                self.partial.clear();
                Ok(())
            }
            _ => Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
    }
}

fn build_log(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(MessageLog {
            stdout: p.bool("stdout")?,
            partial: Vec::with_capacity(LINE_MAX),
        }),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_codes_parse_as_hex_or_bits() {
        assert_eq!(parse_access_code("0x1ACF").unwrap(), (0x1ACF, 16));
        assert_eq!(parse_access_code("1011 0_1").unwrap(), (0b101101, 6));
        assert_eq!(
            parse_access_code("0xFFFFFFFFFFFFFFFF").unwrap(),
            (u64::MAX, 64)
        );
        assert!(
            parse_access_code("0x1FFFFFFFFFFFFFFFF")
                .unwrap_err()
                .contains("64 bits")
        );
        assert!(parse_access_code("0x1G").unwrap_err().contains("hex digit"));
        assert!(parse_access_code("10a").unwrap_err().contains("0s and 1s"));
        assert!(parse_access_code(" ").is_err());
    }
}
