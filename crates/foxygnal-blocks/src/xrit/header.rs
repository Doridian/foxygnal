//! xRIT header records (CGMS LRIT/HRIT Global Specification, plus the NOAA
//! records GOES adds).
//!
//! A file starts with header records, each `type: u8`, `length: u16` (the
//! whole record, these 3 bytes included), then the fields. The primary
//! header (type 0) comes first and gives the length of all headers and of the
//! data field (in bits). Big-endian throughout.
//!
//! | type | record | fields |
//! |---|---|---|
//! | 0 | primary | file type u8, total header length u32, data length u64 (bits) |
//! | 1 | image structure | bits per pixel u8, columns u16, lines u16, compression u8 |
//! | 2 | image navigation | projection name (32 chars), CFAC, LFAC, COFF, LOFF (i32) |
//! | 3 | image data function | text (calibration table) |
//! | 4 | annotation | text: the file name |
//! | 5 | time stamp | CCSDS CDS: P-field u8, days since 1958 u16, ms of day u32 |
//! | 6 | ancillary text | text, `key=value;` pairs on GOES |
//! | 7 | key header | key index u32 (GK-2A; 0 = not encrypted) |
//! | 128 | segment identification (NOAA) | image id, segment number, start column, start line, segments, columns, lines (u16) |
//! | 129 | NOAA product | agency (4 chars), product id, sub-id, parameter (u16), compression u8 |
//! | 130 | header structure (NOAA) | text |
//! | 131 | Rice compression (NOAA) | szip flags u16, pixels per block u8, lines per packet u8 |
//! | 132 | DCS file name (NOAA) | text |
//!
//! Types 128 and up are mission-specific. The NOAA records (128 with 17
//! bytes, 130–132) are only read as such in files with a NOAA record (129,
//! agency `NOAA`); GK-2A's segment record (128 with 7 bytes: segment u8,
//! segments u8, start line u16) and EUMETSAT's / Elektro-L's (128 with 13
//! bytes: spacecraft u16, channel u8, segment u16, first and last planned
//! segment u16, data representation u8) are read into the same [`Segment`].
//! Other records are ignored, so they don't stop a file from being saved.

/// File type codes of the primary header.
pub mod file_type {
    pub const IMAGE: u8 = 0;
    pub const GTS_MESSAGE: u8 = 1;
    pub const TEXT: u8 = 2;
    pub const ENCRYPTION_KEY: u8 = 3;
    pub const DCS: u8 = 130;
}

/// NOAA compression codes (record 129).
pub mod noaa_compression {
    pub const NONE: u8 = 0;
    pub const RICE: u8 = 1;
    pub const JPEG: u8 = 2;
    pub const GIF: u8 = 5;
    pub const ZIP: u8 = 10;
}

/// Bytes of the primary header record.
pub const PRIMARY_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageStructure {
    pub bits_per_pixel: u8,
    pub columns: u16,
    pub lines: u16,
    /// 0 none, 1 lossless (Rice on GOES, wavelet elsewhere), 2 lossy (JPEG).
    pub compression: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Navigation {
    pub projection: String,
    pub cfac: i32,
    pub lfac: i32,
    pub coff: i32,
    pub loff: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub image_id: u16,
    /// Segment number, from 0.
    pub number: u16,
    pub start_column: u16,
    /// The segment's first line in the whole image; `u16::MAX` if the record
    /// doesn't say (then it is `number` × the lines of a segment).
    pub start_line: u16,
    pub segments: u16,
    /// The whole image's size; 0 if the record doesn't say (then it is the
    /// segment's width and `segments` × its lines).
    pub columns: u16,
    pub lines: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Noaa {
    pub agency: [u8; 4],
    pub product: u16,
    pub sub_product: u16,
    pub parameter: u16,
    pub compression: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiceHeader {
    pub flags: u16,
    pub pixels_per_block: u8,
    pub lines_per_packet: u8,
}

/// CCSDS day segmented time: days since 1958-01-01 and milliseconds of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cds {
    pub days: u16,
    pub ms: u32,
}

/// Days from 1958-01-01 to 1970-01-01.
const CDS_UNIX_DAYS: i64 = 4383;

impl Cds {
    /// Seconds since 1970-01-01 UTC (ignoring leap seconds, as everyone does).
    pub fn unix(&self) -> f64 {
        (i64::from(self.days) - CDS_UNIX_DAYS) as f64 * 86_400.0 + f64::from(self.ms) / 1000.0
    }

    pub fn from_unix(t: f64) -> Self {
        let ms_total = (t * 1000.0).round() as i64 + CDS_UNIX_DAYS * 86_400_000;
        Self {
            days: ms_total.div_euclid(86_400_000) as u16,
            ms: ms_total.rem_euclid(86_400_000) as u32,
        }
    }
}

/// Every header of a file, parsed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Headers {
    pub file_type: u8,
    pub header_len: u32,
    pub data_bits: u64,
    pub image: Option<ImageStructure>,
    pub navigation: Option<Navigation>,
    pub data_function: Option<String>,
    pub annotation: Option<String>,
    pub time: Option<Cds>,
    pub ancillary: Option<String>,
    pub key: Option<u32>,
    pub segment: Option<Segment>,
    pub noaa: Option<Noaa>,
    pub header_structure: Option<String>,
    pub rice: Option<RiceHeader>,
    pub dcs_filename: Option<String>,
}

/// Why headers couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderError {
    /// Fewer bytes than the headers claim.
    Short,
    /// Not a primary header first, or a record that runs past the end.
    Malformed(&'static str),
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderError::Short => f.write_str("headers cut short"),
            HeaderError::Malformed(m) => write!(f, "bad headers: {m}"),
        }
    }
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn text(b: &[u8]) -> String {
    // Trailing NULs and spaces are padding.
    let end = b
        .iter()
        .rposition(|&c| c != 0 && c != b' ')
        .map_or(0, |i| i + 1);
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// The primary header at the start of `b`: (file type, total header length,
/// data length in bits).
pub fn primary(b: &[u8]) -> Result<(u8, u32, u64), HeaderError> {
    if b.len() < PRIMARY_LEN {
        return Err(HeaderError::Short);
    }
    if b[0] != 0 || be16(&b[1..]) as usize != PRIMARY_LEN {
        return Err(HeaderError::Malformed("no primary header"));
    }
    let total = be32(&b[4..]);
    if (total as usize) < PRIMARY_LEN {
        return Err(HeaderError::Malformed("header length"));
    }
    let bits = u64::from_be_bytes(b[8..16].try_into().expect("8 bytes"));
    Ok((b[3], total, bits))
}

impl Headers {
    /// Parse the headers at the start of a file (`b` may go on into the
    /// data field).
    pub fn parse(b: &[u8]) -> Result<Self, HeaderError> {
        let (file_type, header_len, data_bits) = primary(b)?;
        let total = header_len as usize;
        if b.len() < total {
            return Err(HeaderError::Short);
        }
        let mut h = Headers {
            file_type,
            header_len,
            data_bits,
            ..Default::default()
        };
        let mut at = PRIMARY_LEN;
        while at + 3 <= total {
            let kind = b[at];
            let len = usize::from(be16(&b[at + 1..]));
            if len < 3 || at + len > total {
                return Err(HeaderError::Malformed("record length"));
            }
            let f = &b[at + 3..at + len];
            match (kind, f.len()) {
                (1, 6..) => {
                    h.image = Some(ImageStructure {
                        bits_per_pixel: f[0],
                        columns: be16(&f[1..]),
                        lines: be16(&f[3..]),
                        compression: f[5],
                    })
                }
                (2, 48..) => {
                    h.navigation = Some(Navigation {
                        projection: text(&f[..32]),
                        cfac: be32(&f[32..]) as i32,
                        lfac: be32(&f[36..]) as i32,
                        coff: be32(&f[40..]) as i32,
                        loff: be32(&f[44..]) as i32,
                    })
                }
                (3, _) => h.data_function = Some(text(f)),
                (4, _) => h.annotation = Some(text(f)),
                (5, 7..) => {
                    h.time = Some(Cds {
                        days: be16(&f[1..]),
                        ms: be32(&f[3..]),
                    })
                }
                (6, _) => h.ancillary = Some(text(f)),
                (7, 4..) => h.key = Some(be32(f)),
                (7, _) => h.key = Some(0),
                (128, 4) => {
                    // GK-2A.
                    h.segment = Some(Segment {
                        image_id: 0,
                        number: u16::from(f[0]).saturating_sub(1),
                        start_column: 0,
                        start_line: u16::MAX,
                        segments: u16::from(f[1]),
                        columns: 0,
                        lines: 0,
                    })
                }
                (128, 10) => {
                    // EUMETSAT / Elektro-L.
                    let (seq, first, last) = (be16(&f[3..]), be16(&f[5..]), be16(&f[7..]));
                    h.segment = Some(Segment {
                        image_id: u16::from(f[2]),
                        number: seq.saturating_sub(first),
                        start_column: 0,
                        start_line: u16::MAX,
                        segments: last.saturating_sub(first) + 1,
                        columns: 0,
                        lines: 0,
                    })
                }
                (128, 14..) => {
                    h.segment = Some(Segment {
                        image_id: be16(f),
                        number: be16(&f[2..]),
                        start_column: be16(&f[4..]),
                        start_line: be16(&f[6..]),
                        segments: be16(&f[8..]),
                        columns: be16(&f[10..]),
                        lines: be16(&f[12..]),
                    })
                }
                (129, 11..) => {
                    h.noaa = Some(Noaa {
                        agency: [f[0], f[1], f[2], f[3]],
                        product: be16(&f[4..]),
                        sub_product: be16(&f[6..]),
                        parameter: be16(&f[8..]),
                        compression: f[10],
                    })
                }
                (130, _) => h.header_structure = Some(text(f)),
                (131, 4..) => {
                    h.rice = Some(RiceHeader {
                        flags: be16(f),
                        pixels_per_block: f[2],
                        lines_per_packet: f[3],
                    })
                }
                (132, _) => h.dcs_filename = Some(text(f)),
                _ => {}
            }
            at += len;
        }
        // Records 130–132 mean other things outside NOAA.
        if !h.noaa.is_some_and(|n| &n.agency == b"NOAA") {
            h.rice = None;
            h.header_structure = None;
            h.dcs_filename = None;
            h.noaa = None;
        }
        Ok(h)
    }

    /// A value from the ancillary text (`key=value;key=value`), trimmed.
    pub fn ancillary(&self, key: &str) -> Option<&str> {
        self.ancillary.as_deref()?.split(';').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k.trim() == key).then(|| v.trim())
        })
    }

    /// Data field length in bytes.
    pub fn data_len(&self) -> usize {
        self.data_bits.div_ceil(8) as usize
    }

    /// True if the image data is Rice coded one packet per line (GOES).
    pub fn rice_coded(&self) -> bool {
        self.file_type == file_type::IMAGE
            && self.image.is_some_and(|i| i.compression == 1)
            && (self.rice.is_some()
                || self
                    .noaa
                    .is_some_and(|n| n.compression == noaa_compression::RICE))
    }

    /// Serialize (for test files and the transmit side). The total header
    /// length is computed.
    pub fn write(&self) -> Vec<u8> {
        let mut out = vec![0u8; PRIMARY_LEN];
        let mut rec = |kind: u8, f: &[u8]| {
            out.push(kind);
            out.extend_from_slice(&((f.len() + 3) as u16).to_be_bytes());
            out.extend_from_slice(f);
        };
        let pad = |s: &str, n: usize| {
            let mut v = s.as_bytes().to_vec();
            v.resize(n, b' ');
            v
        };
        if let Some(i) = self.image {
            let mut f = vec![i.bits_per_pixel];
            f.extend_from_slice(&i.columns.to_be_bytes());
            f.extend_from_slice(&i.lines.to_be_bytes());
            f.push(i.compression);
            rec(1, &f);
        }
        if let Some(n) = &self.navigation {
            let mut f = pad(&n.projection, 32);
            for v in [n.cfac, n.lfac, n.coff, n.loff] {
                f.extend_from_slice(&v.to_be_bytes());
            }
            rec(2, &f);
        }
        if let Some(t) = &self.data_function {
            rec(3, t.as_bytes());
        }
        if let Some(t) = &self.annotation {
            rec(4, t.as_bytes());
        }
        if let Some(t) = self.time {
            let mut f = vec![0x40];
            f.extend_from_slice(&t.days.to_be_bytes());
            f.extend_from_slice(&t.ms.to_be_bytes());
            rec(5, &f);
        }
        if let Some(t) = &self.ancillary {
            rec(6, t.as_bytes());
        }
        if let Some(k) = self.key {
            rec(7, &k.to_be_bytes());
        }
        if let Some(s) = self.segment {
            let mut f = Vec::new();
            for v in [
                s.image_id,
                s.number,
                s.start_column,
                s.start_line,
                s.segments,
                s.columns,
                s.lines,
            ] {
                f.extend_from_slice(&v.to_be_bytes());
            }
            rec(128, &f);
        }
        if let Some(n) = self.noaa {
            let mut f = n.agency.to_vec();
            for v in [n.product, n.sub_product, n.parameter] {
                f.extend_from_slice(&v.to_be_bytes());
            }
            f.push(n.compression);
            rec(129, &f);
        }
        if let Some(t) = &self.header_structure {
            rec(130, t.as_bytes());
        }
        if let Some(r) = self.rice {
            let mut f = r.flags.to_be_bytes().to_vec();
            f.push(r.pixels_per_block);
            f.push(r.lines_per_packet);
            rec(131, &f);
        }
        if let Some(t) = &self.dcs_filename {
            rec(132, t.as_bytes());
        }
        let total = out.len() as u32;
        out[0] = 0;
        out[1..3].copy_from_slice(&(PRIMARY_LEN as u16).to_be_bytes());
        out[3] = self.file_type;
        out[4..8].copy_from_slice(&total.to_be_bytes());
        out[8..16].copy_from_slice(&self.data_bits.to_be_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let h = Headers {
            file_type: file_type::IMAGE,
            header_len: 0,
            data_bits: 8 * 5424 * 226,
            image: Some(ImageStructure {
                bits_per_pixel: 8,
                columns: 5424,
                lines: 226,
                compression: 1,
            }),
            navigation: Some(Navigation {
                projection: "GEOS(-137.0)".into(),
                cfac: 20_466_275,
                lfac: -20_466_275,
                coff: 2712,
                loff: 2712,
            }),
            annotation: Some("OR_ABI-L2-CMIPF-M6C13_G18_s20263271200207_e20263271209527_c20263271209583.lrit".into()),
            time: Some(Cds::from_unix(1_795_435_200.5)),
            ancillary: Some("Time of frame start = 2026-11-23T12:00:20.7Z;Satellite = G18;Instrument = ABI;Channel = 13;Region = Full Disk".into()),
            segment: Some(Segment {
                image_id: 7,
                number: 3,
                start_column: 0,
                start_line: 678,
                segments: 24,
                columns: 5424,
                lines: 5424,
            }),
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
        };
        let bytes = h.write();
        let back = Headers::parse(&bytes).unwrap();
        assert_eq!(back.header_len as usize, bytes.len());
        assert_eq!(
            Headers {
                header_len: back.header_len,
                ..h.clone()
            },
            back
        );
        assert_eq!(back.ancillary("Region"), Some("Full Disk"));
        assert_eq!(back.ancillary("Channel"), Some("13"));
        assert!(back.rice_coded());
        assert!((back.time.unwrap().unix() - 1_795_435_200.5).abs() < 1e-6);
        assert_eq!(Headers::parse(&bytes[..20]), Err(HeaderError::Short));
    }
}
