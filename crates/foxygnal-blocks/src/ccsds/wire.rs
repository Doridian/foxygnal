//! How frames and packets travel on `u8` wires.
//!
//! **Frames (CADUs).** A frames wire carries channel access data units back
//! to back: the 4-byte ASM `1A CF FC 1D`, then the codeblock (`frame_len`
//! bytes: the transfer frame and its Reed-Solomon parity), derandomized and
//! corrected. That is the `.cadu` format other tools (SatDump, …) read and
//! write, so a `file_sink` on the wire records one and a `file_source` plays
//! one back. A frame Reed-Solomon couldn't correct is dropped, or (if the
//! deframer is asked to) sent with the inverted marker `E5 30 03 E2`, which
//! [`CaduReader`] reports as not [`Cadu::ok`] and other tools skip.
//!
//! **Packets.** A packets wire carries records, each an 8-byte header and one
//! whole CCSDS space packet:
//!
//! | bytes | |
//! |---|---|
//! | 0–3 | magic `FXSP` |
//! | 4 | spacecraft id |
//! | 5 | virtual channel id |
//! | 6 | flags: bit 0 = data was lost on this virtual channel before this packet |
//! | 7 | 0 |
//! | 8… | the space packet, primary header first (its length field gives the record's length) |
//!
//! A reader that starts mid-stream or loses bytes finds the next record by
//! its magic and a header that makes sense ([`RecordReader`]).

use super::ASM;
use super::vcdu::{Packet, PacketHeader};

/// The ASM as bytes.
pub const ASM_BYTES: [u8; 4] = ASM.to_be_bytes();
/// The inverted ASM that marks an uncorrectable frame.
pub const ASM_BAD: [u8; 4] = (!ASM).to_be_bytes();

/// A CADU read from a frames wire.
#[derive(Debug)]
pub struct Cadu<'a> {
    /// The codeblock after the marker.
    pub data: &'a [u8],
    /// The marker was the ASM (not the inverted one).
    pub ok: bool,
}

/// Finds CADUs of a known length in a byte stream.
pub struct CaduReader {
    buf: Vec<u8>,
    /// Bytes of `buf` filled (0..4: the marker being matched).
    have: usize,
    /// Last 4 bytes seen while searching.
    window: u32,
    synced: bool,
    /// Bytes skipped while searching.
    pub skipped: u64,
}

impl CaduReader {
    /// `frame_len`: bytes after the marker.
    pub fn new(frame_len: usize) -> Self {
        Self {
            buf: vec![0; 4 + frame_len],
            have: 0,
            window: 0,
            synced: false,
            skipped: 0,
        }
    }

    /// Read bytes; complete CADUs go to `out`. Takes everything.
    pub fn push(&mut self, mut input: &[u8], mut out: impl FnMut(Cadu<'_>)) {
        while !input.is_empty() {
            let n = self.push_one(input, &mut out);
            input = &input[n..];
        }
    }

    /// Read bytes up to the end of the first CADU they complete (which goes
    /// to `out`); returns how many were taken.
    pub fn push_one(&mut self, input: &[u8], mut out: impl FnMut(Cadu<'_>)) -> usize {
        let mut used = 0;
        while used < input.len() {
            let rest = &input[used..];
            if self.have < 4 {
                if self.synced {
                    // On the grid: the next 4 bytes must be a marker.
                    let n = (4 - self.have).min(rest.len());
                    self.buf[self.have..self.have + n].copy_from_slice(&rest[..n]);
                    self.have += n;
                    used += n;
                    if self.have == 4 {
                        let m = [self.buf[0], self.buf[1], self.buf[2], self.buf[3]];
                        if m != ASM_BYTES && m != ASM_BAD {
                            self.synced = false;
                            self.window = u32::from_be_bytes(m);
                            self.have = 0;
                            self.skipped += 4;
                        }
                    }
                    continue;
                }
                // Searching, a byte at a time.
                self.window = (self.window << 8) | u32::from(rest[0]);
                used += 1;
                self.skipped += 1;
                if self.window == ASM || self.window == !ASM {
                    self.buf[..4].copy_from_slice(&self.window.to_be_bytes());
                    self.have = 4;
                    self.synced = true;
                    self.skipped -= 4;
                    self.window = 0;
                }
                continue;
            }
            let n = (self.buf.len() - self.have).min(rest.len());
            self.buf[self.have..self.have + n].copy_from_slice(&rest[..n]);
            self.have += n;
            used += n;
            if self.have == self.buf.len() {
                out(Cadu {
                    data: &self.buf[4..],
                    ok: self.buf[..4] == ASM_BYTES,
                });
                self.have = 0;
                break;
            }
        }
        used
    }
}

/// The record header's magic.
pub const RECORD_MAGIC: [u8; 4] = *b"FXSP";
/// Record header bytes.
pub const RECORD_HEADER: usize = 8;
/// Record flag: data was lost before this packet.
pub const FLAG_GAP: u8 = 1;

/// Write a packet record's 8-byte header.
pub fn record_header(p: &Packet<'_>) -> [u8; RECORD_HEADER] {
    let mut h = [0u8; RECORD_HEADER];
    h[..4].copy_from_slice(&RECORD_MAGIC);
    h[4] = p.spacecraft;
    h[5] = p.vcid;
    h[6] = if p.after_gap { FLAG_GAP } else { 0 };
    h
}

/// A packet read from a packets wire.
#[derive(Debug)]
pub struct Record<'a> {
    pub spacecraft: u8,
    pub vcid: u8,
    pub after_gap: bool,
    pub header: PacketHeader,
    /// The whole space packet.
    pub packet: &'a [u8],
}

/// Reads packet records from a byte stream.
pub struct RecordReader {
    buf: Vec<u8>,
    /// Bytes skipped looking for a record.
    pub skipped: u64,
}

impl Default for RecordReader {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordReader {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(RECORD_HEADER + super::vcdu::MAX_PACKET),
            skipped: 0,
        }
    }

    /// Read bytes; complete records go to `out`. Takes everything.
    pub fn push(&mut self, mut input: &[u8], mut out: impl FnMut(Record<'_>)) {
        while !input.is_empty() {
            let n = self.push_one(input, &mut out);
            input = &input[n..];
        }
    }

    /// Read bytes up to the end of the next complete record (which goes to
    /// `out`), or all of them; returns how many were taken.
    pub fn push_one(&mut self, all: &[u8], mut out: impl FnMut(Record<'_>)) -> usize {
        const HEAD: usize = RECORD_HEADER + 6;
        let mut input = all;
        while !input.is_empty() {
            if self.buf.len() < HEAD {
                let n = (HEAD - self.buf.len()).min(input.len());
                self.buf.extend_from_slice(&input[..n]);
                input = &input[n..];
                // Resynchronize on the magic.
                while !self.buf.is_empty() && !self.plausible() {
                    self.buf.remove(0);
                    self.skipped += 1;
                }
                continue;
            }
            let len = HEAD + usize::from(u16::from_be_bytes([self.buf[12], self.buf[13]])) + 1;
            let n = (len - self.buf.len()).min(input.len());
            self.buf.extend_from_slice(&input[..n]);
            input = &input[n..];
            if self.buf.len() == len {
                let packet = &self.buf[RECORD_HEADER..];
                if let Some(header) = PacketHeader::parse(packet) {
                    out(Record {
                        spacecraft: self.buf[4],
                        vcid: self.buf[5],
                        after_gap: self.buf[6] & FLAG_GAP != 0,
                        header,
                        packet,
                    });
                }
                self.buf.clear();
                break;
            }
        }
        all.len() - input.len()
    }

    /// Whether what's buffered so far can be the start of a record.
    fn plausible(&self) -> bool {
        let b = &self.buf;
        let m = b.len().min(4);
        if b[..m] != RECORD_MAGIC[..m] {
            return false;
        }
        (b.len() <= 5 || b[5] < 64)
            && (b.len() <= 6 || b[6] & !FLAG_GAP == 0)
            && (b.len() <= 7 || b[7] == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadu_reader_finds_the_grid_in_pieces() {
        let len = 20;
        let mut stream = vec![0x11u8, 0x1A, 0xCF, 0x99];
        for k in 0..5u8 {
            stream.extend_from_slice(if k == 2 { &ASM_BAD } else { &ASM_BYTES });
            stream.extend(std::iter::repeat_n(k, len));
            if k == 3 {
                stream.extend_from_slice(&[1, 2, 3]); // junk: lose and find the grid
            }
        }
        for piece in [1, 3, 7, 1000] {
            let mut r = CaduReader::new(len);
            let mut got = Vec::new();
            for c in stream.chunks(piece) {
                r.push(c, |cadu| got.push((cadu.data[0], cadu.ok)));
            }
            assert_eq!(
                got,
                [(0, true), (1, true), (2, false), (3, true), (4, true)],
                "{piece}"
            );
            assert_eq!(r.skipped, 7);
        }
    }

    #[test]
    fn record_reader_resyncs() {
        let mut pkt = vec![0u8; 6 + 10];
        PacketHeader {
            version: 0,
            kind: 0,
            secondary_header: false,
            apid: 77,
            seq_flags: 3,
            seq: 5,
            data_len: 10,
        }
        .write(&mut pkt);
        let p = Packet {
            spacecraft: 9,
            vcid: 3,
            header: PacketHeader::parse(&pkt).unwrap(),
            after_gap: true,
            bytes: &pkt,
        };
        let mut stream = b"FXjunkFX".to_vec();
        for _ in 0..3 {
            stream.extend_from_slice(&record_header(&p));
            stream.extend_from_slice(&pkt);
        }
        for piece in [1, 5, 100] {
            let mut r = RecordReader::new();
            let mut n = 0;
            for c in stream.chunks(piece) {
                r.push(c, |rec| {
                    assert_eq!((rec.spacecraft, rec.vcid, rec.after_gap), (9, 3, true));
                    assert_eq!(rec.header.apid, 77);
                    assert_eq!(rec.packet, &pkt[..]);
                    n += 1;
                });
            }
            assert_eq!(n, 3, "{piece}");
            assert_eq!(r.skipped, 8);
        }
    }
}
