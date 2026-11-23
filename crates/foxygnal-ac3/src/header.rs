//! Sync information and the start of the bit stream information: enough to
//! find frames and say what they hold, for AC-3 and E-AC-3 alike.

use crate::tables::{BITRATES, FBW_CHANNELS, RATES, WORDS_44K};

/// The bytes [`Header::parse`] needs.
pub const HEADER_LEN: usize = 8;

/// A frame header: syncinfo and the fields of the BSI every frame has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Bit stream id: up to 8 for AC-3 (9 and 10 for its half and quarter
    /// rates), 11 to 16 for E-AC-3.
    pub bsid: u8,
    /// Sample rate code, 0 to 2 (48, 44.1, 32 kHz, divided for bsid 9 and 10
    /// or E-AC-3's reduced rates).
    pub fscod: u8,
    pub sample_rate: u32,
    /// Bits per second.
    pub bitrate: u32,
    /// Bytes.
    pub frame_len: usize,
    /// Audio coding mode: 0 is 1+1 (two mono programmes), 1 mono (C), 2
    /// stereo, then L C R, L R S, L C R S, L R Ls Rs, L C R Ls Rs.
    pub acmod: u8,
    /// A low-frequency effects channel follows the others.
    pub lfe: bool,
    /// Audio blocks of 256 samples: 6 for AC-3, 1, 2, 3 or 6 for E-AC-3.
    pub blocks: u8,
    /// E-AC-3: stream type (0 independent, 1 dependent, 2 converted from
    /// AC-3) and substream id. 0 and 0 for AC-3.
    pub strmtyp: u8,
    pub substreamid: u8,
}

impl Header {
    /// Parse the first [`HEADER_LEN`] bytes of `b`. `None` for anything but
    /// a valid header.
    pub fn parse(b: &[u8]) -> Option<Header> {
        if b.len() < HEADER_LEN || b[0] != 0x0B || b[1] != 0x77 {
            return None;
        }
        let bsid = b[5] >> 3;
        if bsid > 16 {
            return None;
        }
        if bsid <= 10 {
            let fscod = b[4] >> 6;
            let frmsizecod = usize::from(b[4] & 0x3f);
            if fscod == 3 || frmsizecod > 37 {
                return None;
            }
            let kbps = u32::from(BITRATES[frmsizecod / 2]);
            let words = match fscod {
                0 => 2 * kbps as usize,
                1 => usize::from(WORDS_44K[frmsizecod / 2]) + (frmsizecod & 1),
                _ => 3 * kbps as usize,
            };
            let shift = bsid.saturating_sub(8);
            let acmod = b[6] >> 5;
            // lfeon follows the mix levels this mode has.
            let mut bit = 3 + 3;
            if acmod & 1 != 0 && acmod != 1 {
                bit += 2;
            }
            if acmod & 4 != 0 {
                bit += 2;
            }
            if acmod == 2 {
                bit += 2;
            }
            let lfe = (b[5..7].iter().fold(0u32, |a, &v| (a << 8) | u32::from(v)) << (5 + bit))
                & 0x8000
                != 0;
            Some(Header {
                bsid,
                fscod,
                sample_rate: RATES[fscod as usize] >> shift,
                bitrate: (kbps * 1000) >> shift,
                frame_len: words * 2,
                acmod,
                lfe,
                blocks: 6,
                strmtyp: 0,
                substreamid: 0,
            })
        } else {
            // E-AC-3 (A/52 annex E): strmtyp 2, substreamid 3, frmsiz 11,
            // fscod 2, fscod2 or numblkscod 2, acmod 3, lfeon 1.
            let strmtyp = b[2] >> 6;
            if strmtyp == 3 {
                return None;
            }
            let substreamid = (b[2] >> 3) & 7;
            let frame_len = ((usize::from(b[2] & 7) << 8 | usize::from(b[3])) + 1) * 2;
            if frame_len < HEADER_LEN {
                return None;
            }
            let fscod = b[4] >> 6;
            let (rate, blocks) = if fscod == 3 {
                let fscod2 = (b[4] >> 4) & 3;
                if fscod2 == 3 {
                    return None;
                }
                (RATES[fscod2 as usize] / 2, 6)
            } else {
                (
                    RATES[fscod as usize],
                    [1, 2, 3, 6][usize::from((b[4] >> 4) & 3)],
                )
            };
            Some(Header {
                bsid,
                fscod: if fscod == 3 { (b[4] >> 4) & 3 } else { fscod },
                sample_rate: rate,
                bitrate: (frame_len as u64 * 8 * u64::from(rate) / (u64::from(blocks) * 256))
                    as u32,
                frame_len,
                acmod: (b[4] >> 1) & 7,
                lfe: b[4] & 1 != 0,
                blocks,
                strmtyp,
                substreamid,
            })
        }
    }

    pub fn eac3(&self) -> bool {
        self.bsid > 10
    }

    /// Full-bandwidth channels.
    pub fn fbw_channels(&self) -> usize {
        usize::from(FBW_CHANNELS[usize::from(self.acmod)])
    }

    /// Channels coded, the LFE included (5.1 is 6).
    pub fn coded_channels(&self) -> usize {
        self.fbw_channels() + usize::from(self.lfe)
    }

    /// Channels decoded: mono stays mono, the rest come out as two
    /// (downmixed when there are more, without the LFE).
    pub fn channels(&self) -> usize {
        if self.acmod == 1 { 1 } else { 2 }
    }

    /// Samples per channel.
    pub fn samples(&self) -> usize {
        usize::from(self.blocks) * 256
    }

    /// This frame starts a new time slot: an independent E-AC-3 substream 0
    /// (dependent and further substreams carry the same time), or any AC-3 frame.
    pub fn starts_slot(&self) -> bool {
        !self.eac3() || (self.strmtyp != 1 && self.substreamid == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        // 48 kHz, 448 kbit/s (frmsizecod 30), bsid 8, bsmod 0, acmod 7, cmix,
        // surmix, lfeon 1.
        let b = [0x0B, 0x77, 0, 0, 0x1E, 0x40, 0xE0 | 0b0000_0001, 0];
        let h = Header::parse(&b).unwrap();
        assert_eq!(
            (h.sample_rate, h.bitrate, h.frame_len),
            (48000, 448000, 1792)
        );
        assert_eq!(
            (h.acmod, h.lfe, h.coded_channels(), h.channels()),
            (7, true, 6, 2)
        );
        // 44.1 kHz, 192 kbit/s, odd code: 418 words.
        let b = [0x0B, 0x77, 0, 0, 0x40 | 21, 0x40, 0x40, 0];
        let h = Header::parse(&b).unwrap();
        assert_eq!((h.sample_rate, h.frame_len), (44100, 836));
        // Stereo: dsurmod, then lfeon.
        let b = [0x0B, 0x77, 0, 0, 0x80 | 8, 0x40, 0x40 | 0b0000_0100, 0];
        let h = Header::parse(&b).unwrap();
        assert_eq!((h.sample_rate, h.frame_len, h.lfe), (32000, 384, true));
        // E-AC-3, 48 kHz, 6 blocks, 5.1, 1536 bytes.
        let b = [0x0B, 0x77, 0x02, 0xFF, 0x30 | (7 << 1) | 1, 16 << 3, 0, 0];
        let h = Header::parse(&b).unwrap();
        assert!(h.eac3());
        assert_eq!((h.frame_len, h.blocks, h.coded_channels()), (1536, 6, 6));
        assert!(Header::parse(&[0x0B, 0x77, 0, 0, 0xC0, 0x40, 0, 0]).is_none());
    }
}
