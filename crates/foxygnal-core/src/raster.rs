//! Pictures on a byte stream: the raster row record.
//!
//! Decoders that produce pictures (SSTV, weather fax, …) write them to a `u8`
//! output one row at a time, each row a self-describing record. Like decoded
//! text lines, this needs nothing from the runtime: the *Image* instrument
//! shows any such wire, `image_sink` saves the pictures as PNG files, and a
//! plain `file_sink` keeps the raw records.
//!
//! A record, little-endian:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `FXIM` |
//! | 4 | 4 | picture number (`u32`, counts the decoder's pictures from 1) |
//! | 8 | 2 | row (`u16`, from 0) |
//! | 10 | 2 | height (`u16`, rows the picture will have; 0 = not known yet) |
//! | 12 | 2 | width (`u16`, pixels, 1…[`MAX_WIDTH`]) |
//! | 14 | 1 | channels (1 = grey, 3 = RGB) |
//! | 15 | 1 | flags (bit 0: the picture's last row) |
//! | 16 | width × channels | pixels, 8 bits per channel, left to right |
//!
//! A picture may also have a name (a satellite product, say
//! `G18/FD/CH13/G18_FD_CH13_20261123T120020Z`), sent as a name record before
//! its first row:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `FXNM` |
//! | 4 | 4 | picture number |
//! | 8 | 2 | length of the name in bytes (1…[`MAX_NAME`]) |
//! | 10 | 6 | zero |
//! | 16 | length | the name, UTF-8: `/`-separated parts, each of letters, digits, `.`, `_`, `-` (see [`valid_name`]) |
//!
//! `image_sink` saves a named picture under its name (the parts before the
//! last are folders), the *Image* instrument shows it.
//!
//! A reader that joins mid-stream, or loses bytes, finds the next record by
//! its magic and a header that makes sense ([`next`]).

/// The first four bytes of every record.
pub const MAGIC: [u8; 4] = *b"FXIM";
/// Bytes before the pixels.
pub const HEADER: usize = 16;
/// The widest row a record may carry.
pub const MAX_WIDTH: u16 = 8192;
/// Flag: this is the picture's last row.
pub const LAST: u8 = 1;
/// The first four bytes of a name record.
pub const NAME_MAGIC: [u8; 4] = *b"FXNM";
/// The longest picture name, in bytes.
pub const MAX_NAME: usize = 200;

/// True if `name` may be a picture name: `/`-separated parts, none empty,
/// `.` or `..`, each made of ASCII letters, digits, `.`, `_` and `-`. Safe to
/// use as a relative path.
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        })
}

/// A name record for picture `picture`. `name` must be [`valid_name`].
pub fn name_record(picture: u32, name: &str) -> Vec<u8> {
    debug_assert!(valid_name(name), "{name}");
    let mut v = Vec::with_capacity(HEADER + name.len());
    v.extend_from_slice(&NAME_MAGIC);
    v.extend_from_slice(&picture.to_le_bytes());
    v.extend_from_slice(&(name.len() as u16).to_le_bytes());
    v.extend_from_slice(&[0; 6]);
    v.extend_from_slice(name.as_bytes());
    v
}

/// The picture number and name of a whole name record.
pub fn parse_name(b: &[u8]) -> Option<(u32, &str)> {
    if b.len() < HEADER || b[..4] != NAME_MAGIC || b[10..16] != [0; 6] {
        return None;
    }
    let len = usize::from(u16::from_le_bytes([b[8], b[9]]));
    let name = std::str::from_utf8(b.get(HEADER..HEADER + len)?).ok()?;
    valid_name(name).then(|| (u32::from_le_bytes([b[4], b[5], b[6], b[7]]), name))
}

/// One row's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub picture: u32,
    pub row: u16,
    pub height: u16,
    pub width: u16,
    pub channels: u8,
    pub flags: u8,
}

impl Row {
    /// Bytes of pixels after the header.
    pub fn pixel_bytes(&self) -> usize {
        usize::from(self.width) * usize::from(self.channels)
    }

    /// Bytes of the whole record.
    #[allow(clippy::len_without_is_empty)] // a record is never empty
    pub fn len(&self) -> usize {
        HEADER + self.pixel_bytes()
    }

    pub fn is_last(&self) -> bool {
        self.flags & LAST != 0
    }

    /// The header's bytes; the pixels follow them.
    pub fn header(&self) -> [u8; HEADER] {
        let mut h = [0; HEADER];
        h[..4].copy_from_slice(&MAGIC);
        h[4..8].copy_from_slice(&self.picture.to_le_bytes());
        h[8..10].copy_from_slice(&self.row.to_le_bytes());
        h[10..12].copy_from_slice(&self.height.to_le_bytes());
        h[12..14].copy_from_slice(&self.width.to_le_bytes());
        h[14] = self.channels;
        h[15] = self.flags;
        h
    }

    /// Parse a header (the first [`HEADER`] bytes of a record). `None` if the
    /// magic is missing or a field is out of range.
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < HEADER || b[..4] != MAGIC {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let r = Self {
            picture: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            row: u16_at(8),
            height: u16_at(10),
            width: u16_at(12),
            channels: b[14],
            flags: b[15],
        };
        let sane = (1..=MAX_WIDTH).contains(&r.width)
            && matches!(r.channels, 1 | 3)
            && (r.height == 0 || r.row < r.height)
            && r.flags & !LAST == 0;
        sane.then_some(r)
    }
}

/// What [`next`] found at the start of a buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// A whole record: its header, and its length in bytes.
    Row(Row, usize),
    /// A whole name record (read it with [`parse_name`]), and its length.
    Name(u32, usize),
    /// Skip this many bytes (not a record), then look again.
    Skip(usize),
    /// The buffer ends inside what may be a record: wait for more bytes.
    More,
}

/// Find the next record at the start of `b`.
pub fn next(b: &[u8]) -> Next {
    // Skip to the next possible magic.
    let start = (0..b.len())
        .find(|&i| {
            let tail = &b[i..b.len().min(i + 4)];
            tail == &MAGIC[..tail.len()] || tail == &NAME_MAGIC[..tail.len()]
        })
        .unwrap_or(b.len());
    if start > 0 {
        return Next::Skip(start);
    }
    if b.len() < HEADER {
        return Next::More;
    }
    if b[..4] == NAME_MAGIC {
        let len = usize::from(u16::from_le_bytes([b[8], b[9]]));
        if b[10..16] != [0; 6] || !(1..=MAX_NAME).contains(&len) {
            return Next::Skip(1);
        }
        if b.len() < HEADER + len {
            return Next::More;
        }
        return match parse_name(b) {
            Some((picture, _)) => Next::Name(picture, HEADER + len),
            None => Next::Skip(1),
        };
    }
    match Row::parse(b) {
        None => Next::Skip(1),
        Some(r) if b.len() < r.len() => Next::More,
        Some(r) => Next::Row(r, r.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(row: u16, last: bool) -> Vec<u8> {
        let mut v = Row {
            picture: 7,
            row,
            height: 2,
            width: 3,
            channels: 3,
            flags: if last { LAST } else { 0 },
        }
        .header()
        .to_vec();
        v.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        v
    }

    #[test]
    fn round_trips_and_resyncs() {
        let mut b = b"junkFX".to_vec();
        b.extend(record(0, false));
        b.extend(record(1, true));
        // "FX" followed by "FX…" isn't the magic: skip straight to the record.
        assert_eq!(next(&b), Next::Skip(6), "up to the magic");
        assert_eq!(
            next(b"junkFX"),
            Next::Skip(4),
            "a magic may start at the end"
        );
        let b = &b[6..];
        let Next::Row(r, n) = next(b) else { panic!() };
        assert_eq!((r.picture, r.row, r.width, r.channels, n), (7, 0, 3, 3, 25));
        assert!(!r.is_last());
        let Next::Row(r, _) = next(&b[n..]) else {
            panic!()
        };
        assert!(r.is_last());
        assert_eq!(next(&b[n..n + 20]), Next::More, "cut inside the pixels");
        assert_eq!(next(&b[..3]), Next::More, "cut inside the magic");
    }

    #[test]
    fn names() {
        assert!(valid_name("G18/FD/CH13/G18_FD_CH13_20261123T120020Z"));
        for bad in ["", "a//b", "../x", "/abs", "a/./b", "sp ace", "a\\b", "ü"] {
            assert!(!valid_name(bad), "{bad}");
        }
        let mut b = b"xx".to_vec();
        b.extend(name_record(9, "G18/FD/x"));
        b.extend(record(0, true));
        assert_eq!(next(&b), Next::Skip(2));
        let b = &b[2..];
        let Next::Name(9, n) = next(b) else { panic!() };
        assert_eq!(parse_name(&b[..n]), Some((9, "G18/FD/x")));
        assert_eq!(next(&b[..n - 1]), Next::More);
        assert!(matches!(next(&b[n..]), Next::Row(..)));
        let mut bad = name_record(1, "ok");
        bad[16] = b'/';
        assert_eq!(next(&bad), Next::Skip(1));
    }

    #[test]
    fn rejects_nonsense_headers() {
        let mut b = record(0, false);
        b[14] = 2; // two channels
        assert_eq!(next(&b), Next::Skip(1));
        let mut b = record(0, false);
        b[8] = 5; // row past the height
        assert_eq!(next(&b), Next::Skip(1));
    }
}
