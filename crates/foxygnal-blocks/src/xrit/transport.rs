//! Files from CCSDS packets: the xRIT transport and session layers.
//!
//! A file is a run of space packets on one virtual channel and APID: the
//! first has sequence flags `01` (or `11` if it is the only one), the last
//! `10`, the ones between `00`. Every packet's data field ends with a
//! CRC-16 (CCITT: polynomial 0x1021, initial value 0xFFFF) over the rest of
//! it. The first packet's data field starts with a 10-byte transport header
//! (file counter u16, file length u64 in bits), then the file: header
//! records ([`super::header`]), then the data field.
//!
//! GOES Rice-codes images line by line: after the packet with the headers,
//! each packet holds one compressed line (szip parameters from the Rice and
//! image structure records), and the primary header's data length is that of
//! the uncompressed image. [`FileAssembler`] decompresses as packets arrive,
//! so a lost packet costs one line (left black) rather than the file.

use std::collections::HashMap;

use super::header::{self, HeaderError, Headers};
use super::rice::Szip;
use crate::ccsds::vcdu::{PacketHeader, VCID_FILL};

/// Bytes of the transport header in a file's first packet.
pub const TP_HEADER: usize = 10;
/// The longest packet data field the encoder makes (GOES uses up to 8190).
pub const MAX_DATA: usize = 8190;

const fn crc_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut b = 0;
        while b < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x1021
            } else {
                c << 1
            };
            b += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u16; 256] = crc_table();

/// CRC-16/CCITT-FALSE, as xRIT packets carry it.
pub fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0xFFFF, |crc, &b| {
        (crc << 8) ^ CRC_TABLE[usize::from((crc >> 8) as u8 ^ b)]
    })
}

/// A file put back together.
#[derive(Debug, Clone)]
pub struct File {
    pub spacecraft: u8,
    pub vcid: u8,
    pub apid: u16,
    pub headers: Headers,
    /// The header records as received.
    pub header_bytes: Vec<u8>,
    /// The data field (decompressed if it was Rice coded), exactly as long as
    /// the primary header says.
    pub data: Vec<u8>,
    /// Every packet arrived with a good CRC and the length adds up.
    pub complete: bool,
    /// The data was Rice coded and has been decompressed.
    pub rice_decoded: bool,
    /// Lines of a Rice-coded image that were lost (left black).
    pub lines_lost: u32,
}

impl File {
    /// The whole file as it would be stored: headers, then data. A Rice
    /// image is stored decompressed, with its image structure record saying
    /// so (compression 0), so other software can read it.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = self.header_bytes.clone();
        if self.rice_decoded {
            mark_uncompressed(&mut out);
        }
        out.extend_from_slice(&self.data);
        out
    }
}

/// Set the image structure record's compression to 0 in raw headers.
fn mark_uncompressed(h: &mut [u8]) {
    let mut at = header::PRIMARY_LEN;
    while at + 3 <= h.len() {
        let len = usize::from(u16::from_be_bytes([h[at + 1], h[at + 2]]));
        if len < 3 {
            return;
        }
        if h[at] == 1 && len >= 9 && at + 9 <= h.len() {
            h[at + 8] = 0;
        }
        at += len;
    }
}

/// Counters for the status output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub packets: u64,
    pub crc_errors: u64,
    /// Continuation packets with no file to continue (its start was lost).
    pub orphans: u64,
    pub files: u64,
    pub files_incomplete: u64,
    /// Files whose headers never arrived intact.
    pub files_lost: u64,
}

struct Partial {
    spacecraft: u8,
    buf: Vec<u8>,
    headers: Option<Headers>,
    szip: Option<Szip>,
    line_bytes: usize,
    lines: u32,
    lines_lost: u32,
    next_seq: u16,
    damaged: bool,
}

/// Reassembles files from packets, per virtual channel and APID.
#[derive(Default)]
pub struct FileAssembler {
    open: HashMap<(u8, u16), Partial>,
    stats: Stats,
}

impl FileAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Feed one whole packet (primary header included).
    pub fn push(&mut self, spacecraft: u8, vcid: u8, packet: &[u8], mut out: impl FnMut(File)) {
        let Some(h) = PacketHeader::parse(packet) else {
            return;
        };
        if vcid == VCID_FILL || h.apid == crate::ccsds::vcdu::APID_IDLE {
            return;
        }
        self.stats.packets += 1;
        let data = &packet[6..packet.len().min(6 + h.data_len)];
        if data.len() < 2 {
            return;
        }
        let (payload, crc) = data.split_at(data.len() - 2);
        let crc_ok = crc16(payload) == u16::from_be_bytes([crc[0], crc[1]]);
        if !crc_ok {
            self.stats.crc_errors += 1;
        }
        let key = (vcid, h.apid);
        let first = h.seq_flags & 1 != 0;
        let last = h.seq_flags & 2 != 0;
        if first {
            if let Some(p) = self.open.remove(&key) {
                self.finish(key, p, false, &mut out);
            }
            if !crc_ok || payload.len() < TP_HEADER {
                self.stats.files_lost += 1;
                return;
            }
            let mut p = Partial {
                spacecraft,
                buf: Vec::with_capacity(payload.len()),
                headers: None,
                szip: None,
                line_bytes: 0,
                lines: 0,
                lines_lost: 0,
                next_seq: (h.seq + 1) & 0x3FFF,
                damaged: false,
            };
            p.buf.extend_from_slice(&payload[TP_HEADER..]);
            if let Err(HeaderError::Malformed(_)) = Self::try_headers(&mut p) {
                self.stats.files_lost += 1;
                return;
            }
            if last {
                self.finish(key, p, true, &mut out);
            } else {
                self.open.insert(key, p);
            }
            return;
        }
        let Some(mut p) = self.open.remove(&key) else {
            self.stats.orphans += 1;
            return;
        };
        let missing = h.seq.wrapping_sub(p.next_seq) & 0x3FFF;
        p.next_seq = (h.seq + 1) & 0x3FFF;
        if missing > 0 {
            if p.szip.is_some() {
                Self::lose_lines(&mut p, u32::from(missing));
            } else {
                p.damaged = true;
            }
        }
        if !crc_ok {
            if p.szip.is_some() {
                Self::lose_lines(&mut p, 1);
            } else {
                p.damaged = true;
            }
        } else if let Some(sz) = p.szip {
            let (line, r) = sz.decompress(payload, p.line_bytes);
            p.buf.extend_from_slice(&line);
            p.lines += 1;
            if r.is_err() {
                p.lines_lost += 1;
            }
        } else {
            p.buf.extend_from_slice(payload);
            if p.headers.is_none()
                && matches!(Self::try_headers(&mut p), Err(HeaderError::Malformed(_)))
            {
                self.stats.files_lost += 1;
                return;
            }
        }
        if last {
            self.finish(key, p, true, &mut out);
        } else {
            self.open.insert(key, p);
        }
    }

    fn lose_lines(p: &mut Partial, n: u32) {
        let room = p.headers.as_ref().map_or(0, |h| {
            (h.header_len as usize + h.data_len()).saturating_sub(p.buf.len()) / p.line_bytes.max(1)
        }) as u32;
        let n = n.min(room);
        p.buf.resize(p.buf.len() + n as usize * p.line_bytes, 0);
        p.lines += n;
        p.lines_lost += n;
    }

    /// Parse the headers once they have all arrived; set up Rice decoding.
    fn try_headers(p: &mut Partial) -> Result<(), HeaderError> {
        if p.headers.is_some() {
            return Ok(());
        }
        let h = Headers::parse(&p.buf)?;
        if h.rice_coded()
            && let Some(img) = h.image
        {
            let rice = h.rice.unwrap_or(header::RiceHeader {
                flags: 49,
                pixels_per_block: 16,
                lines_per_packet: 1,
            });
            let sz = Szip {
                options: rice.flags,
                bits_per_pixel: img.bits_per_pixel,
                pixels_per_block: u16::from(rice.pixels_per_block),
                pixels_per_scanline: usize::from(img.columns),
            };
            p.line_bytes = usize::from(img.columns) * sz.sample_bytes();
            p.szip = Some(sz);
            // Anything after the headers in the first packet isn't a line.
            p.buf.truncate(h.header_len as usize);
        }
        p.headers = Some(h);
        Ok(())
    }

    fn finish(&mut self, key: (u8, u16), mut p: Partial, ended: bool, out: &mut impl FnMut(File)) {
        let Some(headers) = p.headers.take() else {
            self.stats.files_lost += 1;
            return;
        };
        let hl = headers.header_len as usize;
        let want = hl + headers.data_len();
        let mut complete = ended && !p.damaged && p.buf.len() == want && p.lines_lost == 0;
        if p.szip.is_some() {
            let lines = headers.image.map_or(0, |i| u32::from(i.lines));
            if p.lines < lines {
                complete = false;
                p.lines_lost += lines - p.lines;
            }
        }
        p.buf.resize(want.max(hl), 0);
        let data = p.buf.split_off(hl);
        self.stats.files += 1;
        if !complete {
            self.stats.files_incomplete += 1;
        }
        out(File {
            spacecraft: p.spacecraft,
            vcid: key.0,
            apid: key.1,
            rice_decoded: p.szip.is_some(),
            lines_lost: p.lines_lost,
            headers,
            header_bytes: p.buf,
            data,
            complete,
        });
    }

    /// The end of the stream: hand over what has arrived of open files.
    pub fn flush(&mut self, mut out: impl FnMut(File)) {
        let mut keys: Vec<_> = self.open.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            if let Some(p) = self.open.remove(&key) {
                self.finish(key, p, false, &mut out);
            }
        }
    }
}

/// Splits files into packets: the transmit side of [`FileAssembler`], for
/// one APID.
pub struct FileEncoder {
    pub apid: u16,
    seq: u16,
    counter: u16,
}

impl FileEncoder {
    pub fn new(apid: u16) -> Self {
        Self {
            apid,
            seq: 0,
            counter: 0,
        }
    }

    fn packet(&mut self, flags: u8, payload: &[u8], out: &mut impl FnMut(&[u8])) {
        let mut p = vec![0u8; 6 + payload.len() + 2];
        PacketHeader {
            version: 0,
            kind: 0,
            secondary_header: false,
            apid: self.apid,
            seq_flags: flags,
            seq: self.seq,
            data_len: payload.len() + 2,
        }
        .write(&mut p);
        p[6..6 + payload.len()].copy_from_slice(payload);
        let crc = crc16(payload);
        p[6 + payload.len()..].copy_from_slice(&crc.to_be_bytes());
        self.seq = (self.seq + 1) & 0x3FFF;
        out(&p);
    }

    /// Send one file: `headers` (their lengths are filled in) and the data
    /// field. If the headers say Rice, the image is coded line by line.
    pub fn encode(&mut self, headers: &Headers, data: &[u8], out: &mut impl FnMut(&[u8])) {
        let mut h = headers.clone();
        h.data_bits = data.len() as u64 * 8;
        let hb = h.write();
        let mut first = Vec::with_capacity(TP_HEADER + hb.len());
        first.extend_from_slice(&self.counter.to_be_bytes());
        first.extend_from_slice(&((hb.len() + data.len()) as u64 * 8).to_be_bytes());
        self.counter = self.counter.wrapping_add(1);
        first.extend_from_slice(&hb);
        let rice = h.rice_coded().then(|| {
            let img = h.image.expect("rice_coded checks it");
            let r = h.rice.unwrap_or(header::RiceHeader {
                flags: 49,
                pixels_per_block: 16,
                lines_per_packet: 1,
            });
            Szip {
                options: r.flags,
                bits_per_pixel: img.bits_per_pixel,
                pixels_per_block: u16::from(r.pixels_per_block),
                pixels_per_scanline: usize::from(img.columns),
            }
        });
        if let Some(sz) = rice {
            let line = sz.pixels_per_scanline * sz.sample_bytes();
            let lines = data.len() / line.max(1);
            self.packet(if lines == 0 { 3 } else { 1 }, &first, out);
            for (i, l) in data.chunks(line).enumerate() {
                let c = sz.compress(l).expect("valid szip parameters");
                self.packet(if i + 1 == lines { 2 } else { 0 }, &c, out);
            }
            return;
        }
        first.extend_from_slice(data);
        let chunks: Vec<&[u8]> = first.chunks(MAX_DATA - 2).collect();
        let n = chunks.len();
        for (i, c) in chunks.into_iter().enumerate() {
            let flags = match (i == 0, i + 1 == n) {
                (true, true) => 3,
                (true, false) => 1,
                (false, true) => 2,
                (false, false) => 0,
            };
            self.packet(flags, c, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xrit::header::{ImageStructure, Noaa, RiceHeader, file_type};

    #[test]
    fn crc_is_ccitt_false() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    fn image_headers(cols: u16, lines: u16) -> Headers {
        Headers {
            file_type: file_type::IMAGE,
            image: Some(ImageStructure {
                bits_per_pixel: 8,
                columns: cols,
                lines,
                compression: 1,
            }),
            annotation: Some("test.lrit".into()),
            noaa: Some(Noaa {
                agency: *b"NOAA",
                product: 18,
                sub_product: 0,
                parameter: 13,
                compression: 1,
            }),
            rice: Some(RiceHeader {
                flags: 49,
                pixels_per_block: 16,
                lines_per_packet: 1,
            }),
            ..Default::default()
        }
    }

    fn packets(h: &Headers, data: &[u8], apid: u16) -> Vec<Vec<u8>> {
        let mut enc = FileEncoder::new(apid);
        let mut v = Vec::new();
        enc.encode(h, data, &mut |p: &[u8]| v.push(p.to_vec()));
        v
    }

    #[test]
    fn rice_image_round_trips_and_survives_a_lost_line() {
        let (w, l) = (100u16, 20u16);
        let img: Vec<u8> = (0..usize::from(w) * usize::from(l))
            .map(|i| (i % 100 + i / 100) as u8)
            .collect();
        let h = image_headers(w, l);
        let ps = packets(&h, &img, 5);
        assert_eq!(ps.len(), 1 + usize::from(l));
        let mut fa = FileAssembler::new();
        let mut files = Vec::new();
        for p in &ps {
            fa.push(1, 13, p, |f| files.push(f));
        }
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert!(f.complete && f.rice_decoded);
        assert_eq!(f.data, img);
        assert_eq!(f.headers.annotation.as_deref(), Some("test.lrit"));
        // Stored decompressed.
        let stored = Headers::parse(&f.bytes()).unwrap();
        assert_eq!(stored.image.unwrap().compression, 0);

        // Drop line 4's packet and corrupt line 7's.
        let mut files = Vec::new();
        for (i, p) in ps.iter().enumerate() {
            let mut p = p.clone();
            if i == 5 {
                continue;
            }
            if i == 8 {
                p[10] ^= 0x40;
            }
            fa.push(1, 13, &p, |f| files.push(f));
        }
        let f = &files[0];
        assert!(!f.complete);
        assert_eq!(f.lines_lost, 2);
        let row = |r: usize| &f.data[r * 100..(r + 1) * 100];
        assert!(row(4).iter().all(|&b| b == 0) && row(7).iter().all(|&b| b == 0));
        assert_eq!(row(5), &img[500..600]);
        assert_eq!(f.data.len(), img.len());
    }

    #[test]
    fn plain_files_interleave_by_apid() {
        let text = Headers {
            file_type: file_type::TEXT,
            annotation: Some("a.txt".into()),
            ..Default::default()
        };
        let a: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        let b = b"short".to_vec();
        let pa = packets(&text, &a, 1);
        let pb = packets(&text, &b, 2);
        assert_eq!(pa.len(), 3);
        let mut fa = FileAssembler::new();
        let mut files = Vec::new();
        fa.push(0, 20, &pa[0], |f| files.push(f));
        fa.push(0, 20, &pb[0], |f| files.push(f));
        fa.push(0, 20, &pa[1], |f| files.push(f));
        fa.push(0, 20, &pa[2], |f| files.push(f));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].data, b);
        assert_eq!(files[1].data, a);
        assert!(files.iter().all(|f| f.complete));

        // A lost middle packet: the file comes out incomplete.
        let mut files = Vec::new();
        fa.push(0, 20, &pa[0], |f| files.push(f));
        fa.push(0, 20, &pa[2], |f| files.push(f));
        assert_eq!(files.len(), 1);
        assert!(!files[0].complete);
        assert_eq!(fa.stats().files_incomplete, 1);
    }
}
