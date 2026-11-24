//! Synthetic HRIT broadcast, for loopback tests and the "GOES HRIT loopback"
//! template: what a GOES-R satellite sends, made up but in the real format.
//!
//! Each round (scan time `start + 600 s × round`) has a full disk in
//! channels 13 and 2 (segmented, Rice coded line by line, segments of the
//! two interleaved), a mesoscale picture in channel 7 (one file), an EMWIN
//! tornado warning (zipped) and area forecast discussion (plain text), and an
//! administrative message, with headers as GOES-19 sends them.

use super::emwin;
use super::header::{
    Cds, Headers, ImageStructure, Navigation, Noaa, RiceHeader, Segment, file_type,
    noaa_compression,
};
use super::products::civil;

/// One file to send.
#[derive(Debug, Clone)]
pub struct SynthFile {
    pub vcid: u8,
    pub apid: u16,
    pub headers: Headers,
    pub data: Vec<u8>,
}

/// What to make.
#[derive(Debug, Clone, Copy)]
pub struct Synth {
    /// Full-disk size (pixels square); the mesoscale picture is half of it.
    pub width: u16,
    /// Segments per full disk.
    pub segments: u16,
    /// Scan time of round 0, seconds since 1970.
    pub start: f64,
}

impl Default for Synth {
    fn default() -> Self {
        Self {
            width: 512,
            segments: 4,
            // 2026-11-23T12:00:00Z
            start: 1_795_435_200.0,
        }
    }
}

/// A made-up Earth for channel `ch`: a disk with shading and banded clouds,
/// white (255) space around it as GOES sends it. Channel 2 (visible) is dark
/// on the night side.
pub fn picture(ch: u8, w: u16, h: u16, round: u32) -> Vec<u8> {
    let (w, h) = (f64::from(w), f64::from(h));
    let r0 = w.min(h) * 0.47;
    let mut out = Vec::with_capacity((w * h) as usize);
    let phase = f64::from(round) * 0.7 + f64::from(ch);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let (dx, dy) = (x as f64 - w / 2.0, y as f64 - h / 2.0);
            let d = (dx * dx + dy * dy).sqrt() / r0;
            if d >= 1.0 {
                out.push(255);
                continue;
            }
            let (u, v) = (dx / r0, dy / r0);
            let cloud = ((u * 9.0 + phase).sin() * (v * 7.0 - phase).cos()).max(0.0);
            let limb = (1.0 - d * d).sqrt();
            let px = match ch {
                2 => {
                    let day = ((1.0 - u) * 0.8).clamp(0.0, 1.0);
                    (40.0 + 60.0 * limb + 150.0 * cloud) * day
                }
                _ => 70.0 + 50.0 * (1.0 - limb) + 130.0 * cloud,
            };
            out.push(px.clamp(0.0, 254.0) as u8);
        }
    }
    out
}

/// `picture` as the decoder shows it: GOES's white space around a full disk
/// turned black.
pub fn shown(ch: u8, w: u16, h: u16, round: u32) -> Vec<u8> {
    picture(ch, w, h, round)
        .into_iter()
        .map(|p| if p == 255 { 0 } else { p })
        .collect()
}

fn stamp_goesr(t: f64) -> String {
    let (y, m, d, hh, mm, ss) = civil(t);
    let doy =
        super::products::days_from_civil(y, m, d) - super::products::days_from_civil(y, 1, 1) + 1;
    format!("{y:04}{doy:03}{hh:02}{mm:02}{ss:02}0")
}

fn iso(t: f64) -> String {
    let (y, m, d, hh, mm, ss) = civil(t);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.0Z")
}

/// The EMWIN tornado warning of a round: (file name, text).
pub fn warning(round: u32, t: f64) -> (String, String) {
    let (y, m, d, hh, mm, ss) = civil(t);
    let name = format!(
        "A_WFUS54KBMX{d:02}{hh:02}{mm:02}_C_KWIN_{y:04}{m:02}{d:02}{hh:02}{mm:02}{ss:02}_{:06}-1-TORBMXAL.ZIS",
        round * 2 + 1
    );
    let text = format!(
        "\u{1}\r\r\n{:03} \r\r\nWFUS54 KBMX {d:02}{hh:02}{mm:02}\r\r\nTORBMX\r\r\nALC073-{d:02}{hh:02}45-\r\r\n\r\r\nBULLETIN - EAS ACTIVATION REQUESTED\r\r\nTornado Warning\r\r\nNational Weather Service Birmingham AL\r\r\n\r\r\nTHIS IS A FOXYGNAL LOOPBACK TEST, ROUND {round}.\r\r\n\r\r\n$$\r\r\n\u{3}",
        round % 1000
    );
    (name, text)
}

/// The EMWIN area forecast discussion of a round: (file name, text).
pub fn discussion(round: u32, t: f64) -> (String, String) {
    let (y, m, d, hh, mm, ss) = civil(t);
    let name = format!(
        "A_FXUS61KOKX{d:02}{hh:02}{mm:02}_C_KWIN_{y:04}{m:02}{d:02}{hh:02}{mm:02}{ss:02}_{:06}-3-AFDOKXNY.TXT",
        round * 2 + 2
    );
    let text = format!(
        "\u{1}\r\r\n000 \r\r\nFXUS61 KOKX {d:02}{hh:02}{mm:02}\r\r\nAFDOKX\r\r\n\r\r\nArea Forecast Discussion\r\r\nNational Weather Service New York NY\r\r\n\r\r\n.SYNOPSIS...\r\r\nQuiet loopback weather, round {round}.\r\r\n\r\r\n$$\r\r\n\u{3}"
    );
    (name, text)
}

/// The administrative message of a round.
pub fn admin(round: u32) -> String {
    format!("FOXYGNAL HRIT LOOPBACK\nThis is round {round} of the synthetic broadcast.\n")
}

impl Synth {
    /// Scan time of a round.
    pub fn time(&self, round: u32) -> f64 {
        self.start + 600.0 * f64::from(round)
    }

    fn full_disk(&self, ch: u8, round: u32) -> Vec<SynthFile> {
        let t = self.time(round);
        let w = self.width;
        let px = picture(ch, w, w, round);
        let segs = self.segments.clamp(1, w);
        let per = w.div_ceil(segs);
        let name = format!(
            "OR_ABI-L2-CMIPF-M6C{ch:02}_G19_s{}_e{}_c{}.lrit",
            stamp_goesr(t),
            stamp_goesr(t + 540.0),
            stamp_goesr(t + 560.0)
        );
        (0..segs)
            .filter_map(|s| {
                let first = s * per;
                let lines = per.min(w.checked_sub(first)?);
                if lines == 0 {
                    return None;
                }
                let at = usize::from(first) * usize::from(w);
                Some(SynthFile {
                    vcid: ch,
                    apid: u16::from(ch) * 32,
                    headers: Headers {
                        file_type: file_type::IMAGE,
                        image: Some(ImageStructure {
                            bits_per_pixel: 8,
                            columns: w,
                            lines,
                            compression: 1,
                        }),
                        navigation: Some(Navigation {
                            projection: "geos(-137.0)".into(),
                            cfac: 20_425_862,
                            lfac: 20_425_862,
                            coff: i32::from(w / 2),
                            loff: i32::from(w / 2) - i32::from(first),
                        }),
                        annotation: Some(name.clone()),
                        time: Some(Cds::from_unix(t)),
                        ancillary: Some(format!(
                            "Time of frame start = {};Satellite = G19;Instrument = FM4;Channel = {ch};Imaging Mode = ABI Mode 6;Region = Full Disk;Resolution = 2km at nadir;Segmented = yes",
                            iso(t)
                        )),
                        segment: Some(Segment {
                            image_id: (round as u16).wrapping_mul(16).wrapping_add(u16::from(ch)),
                            number: s,
                            start_column: 0,
                            start_line: first,
                            segments: segs,
                            columns: w,
                            lines: w,
                        }),
                        noaa: Some(Noaa {
                            agency: *b"NOAA",
                            product: 19,
                            sub_product: u16::from(ch),
                            parameter: 0,
                            compression: noaa_compression::RICE,
                        }),
                        rice: Some(RiceHeader {
                            flags: 49,
                            pixels_per_block: 16,
                            lines_per_packet: 1,
                        }),
                        ..Default::default()
                    },
                    data: px[at..at + usize::from(lines) * usize::from(w)].to_vec(),
                })
            })
            .collect()
    }

    fn mesoscale(&self, round: u32) -> SynthFile {
        let t = self.time(round) + 30.0;
        let w = (self.width / 2).max(16);
        SynthFile {
            vcid: 1,
            apid: 32,
            headers: Headers {
                file_type: file_type::IMAGE,
                image: Some(ImageStructure {
                    bits_per_pixel: 8,
                    columns: w,
                    lines: w,
                    compression: 1,
                }),
                annotation: Some(format!(
                    "OR_ABI-L2-CMIPM1-M6C07_G19_s{}_e{}_c{}.lrit",
                    stamp_goesr(t),
                    stamp_goesr(t + 50.0),
                    stamp_goesr(t + 55.0)
                )),
                time: Some(Cds::from_unix(t)),
                ancillary: Some(format!(
                    "Time of frame start = {};Satellite = G19;Instrument = FM4;Channel = 7;Imaging Mode = ABI Mode 6;Region = Mesoscale;Resolution = 2km at nadir;Segmented = no",
                    iso(t)
                )),
                noaa: Some(Noaa {
                    agency: *b"NOAA",
                    product: 19,
                    sub_product: 0,
                    parameter: 0,
                    compression: noaa_compression::RICE,
                }),
                rice: Some(RiceHeader {
                    flags: 49,
                    pixels_per_block: 16,
                    lines_per_packet: 1,
                }),
                ..Default::default()
            },
            data: picture(7, w, w, round),
        }
    }

    fn text_file(
        vcid: u8,
        product: u16,
        compression: u8,
        name: String,
        t: f64,
        data: Vec<u8>,
    ) -> SynthFile {
        SynthFile {
            vcid,
            apid: u16::from(vcid) * 32,
            headers: Headers {
                file_type: file_type::TEXT,
                annotation: Some(name),
                time: Some(Cds::from_unix(t)),
                noaa: Some(Noaa {
                    agency: *b"NOAA",
                    product,
                    sub_product: 0,
                    parameter: 0,
                    compression,
                }),
                ..Default::default()
            },
            data,
        }
    }

    /// Every file of a round, in the order they are sent.
    pub fn round(&self, round: u32) -> Vec<SynthFile> {
        let t = self.time(round);
        let mut files = Vec::new();
        let ir = self.full_disk(13, round);
        let vis = self.full_disk(2, round);
        // Segments of the two channels take turns.
        let n = ir.len().max(vis.len());
        let (mut ir, mut vis) = (ir.into_iter(), vis.into_iter());
        for i in 0..n {
            files.extend(ir.next());
            files.extend(vis.next());
            if i == 0 {
                let (name, text) = warning(round, t + 60.0);
                let zip = emwin::zip_one("TORBMXAL.TXT", text.as_bytes());
                files.push(Self::text_file(
                    20,
                    9,
                    noaa_compression::ZIP,
                    name,
                    t + 60.0,
                    zip,
                ));
            }
        }
        files.push(self.mesoscale(round));
        let (name, text) = discussion(round, t + 120.0);
        files.push(Self::text_file(
            22,
            9,
            noaa_compression::NONE,
            name,
            t + 120.0,
            text.into_bytes(),
        ));
        files.push(Self::text_file(
            0,
            1,
            noaa_compression::NONE,
            format!("FOXYGNAL_ADMIN_{round:04}.TXT"),
            t + 180.0,
            admin(round).into_bytes(),
        ));
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_has_what_it_says() {
        let s = Synth::default();
        let files = s.round(0);
        assert_eq!(files.len(), 4 + 4 + 1 + 1 + 1 + 1);
        let fd = &files[0];
        assert_eq!(fd.headers.segment.unwrap().segments, 4);
        assert_eq!(fd.data.len(), 512 * 128);
        assert!(fd.headers.rice_coded());
        assert!(stamp_goesr(s.time(0)).starts_with("20263271200"));
        let px = picture(13, 64, 64, 0);
        assert_eq!(px[0], 255);
        assert!(px[32 * 64 + 32] < 255);
    }
}
