//! Known-answer tests against independent sources:
//!
//! - Rice decompression against libaec 1.1.7 (the library goestools and
//!   SatDump use): `tests/fixtures/xrit/rice.bin` from
//!   `scripts/xrit-vectors.py` (see its header for the format).
//! - JPEG against libjpeg (`jpeg-*.jpg`, same script).
//! - Headers of real GOES-16 HRIT files: `goes16-*.hdr` are the header
//!   records (data cut off) of the sample `.lrit` files NOAA published on
//!   its HRIT sample imagery page (full disk channels 2 and 13, mesoscale 1
//!   channel 2, mesoscale 2 channel 7; 23 February 2018).

use super::rice::Szip;

struct Case {
    sz: Szip,
    raw: Vec<u8>,
    compressed: Vec<u8>,
}

fn cases() -> Vec<Case> {
    let path = format!(
        "{}/tests/fixtures/xrit/rice.bin",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut at = 0;
    let mut out = Vec::new();
    while at < data.len() {
        let h = &data[at..at + 12];
        let sz = Szip {
            options: u16::from_be_bytes([h[0], h[1]]),
            bits_per_pixel: h[2],
            pixels_per_block: u16::from(h[3]),
            pixels_per_scanline: usize::from(u16::from_be_bytes([h[4], h[5]])),
        };
        let lines = usize::from(u16::from_be_bytes([h[6], h[7]]));
        let n = u32::from_be_bytes([h[8], h[9], h[10], h[11]]) as usize;
        at += 12;
        let size = sz.pixels_per_scanline * lines * sz.sample_bytes();
        out.push(Case {
            sz,
            raw: data[at..at + size].to_vec(),
            compressed: data[at + size..at + size + n].to_vec(),
        });
        at += size + n;
    }
    out
}

#[test]
fn rice_matches_libaec() {
    let cases = cases();
    assert_eq!(cases.len(), 10);
    for (i, c) in cases.iter().enumerate() {
        let (got, r) = c.sz.decompress(&c.compressed, c.raw.len());
        assert_eq!(r, Ok(()), "case {i}: {:?}", c.sz);
        assert!(got == c.raw, "case {i}: {:?} decodes differently", c.sz);
    }
}

/// Our encoder's output for the same pixels, for `scripts/xrit-vectors.py
/// --check` to decode with libaec: set `FOXYGNAL_RICE_OUT` to a file name.
#[test]
fn rice_encoder_output() {
    let mut blob = Vec::new();
    for c in cases() {
        let ours = c.sz.compress(&c.raw).expect("compress");
        let (back, r) = c.sz.decompress(&ours, c.raw.len());
        assert_eq!(r, Ok(()));
        assert!(back == c.raw);
        // Never much longer than libaec's (which also merges zero blocks).
        assert!(
            ours.len() <= c.compressed.len() * 11 / 10 + 64,
            "{:?}",
            c.sz
        );
        let lines = c.raw.len() / c.sz.sample_bytes() / c.sz.pixels_per_scanline;
        blob.extend_from_slice(&c.sz.options.to_be_bytes());
        blob.push(c.sz.bits_per_pixel);
        blob.push(c.sz.pixels_per_block as u8);
        blob.extend_from_slice(&(c.sz.pixels_per_scanline as u16).to_be_bytes());
        blob.extend_from_slice(&(lines as u16).to_be_bytes());
        blob.extend_from_slice(&(ours.len() as u32).to_be_bytes());
        blob.extend_from_slice(&c.raw);
        blob.extend_from_slice(&ours);
    }
    if let Some(path) = std::env::var_os("FOXYGNAL_RICE_OUT") {
        std::fs::write(path, blob).expect("write");
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/xrit/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Pillow's libjpeg made and decoded these; IDCTs differ by a level or two.
#[test]
fn jpeg_matches_libjpeg() {
    for (name, ch) in [("grey", 1), ("rgb", 3)] {
        let (w, h, c, px) = super::decoder::jpeg(&fixture(&format!("jpeg-{name}.jpg"))).unwrap();
        assert_eq!((w, h, c), (48, 32, ch));
        let want = fixture(&format!("jpeg-{name}.raw"));
        assert_eq!(px.len(), want.len());
        let worst = px
            .iter()
            .zip(&want)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 3, "{name}: off by up to {worst}");
    }
    assert!(super::decoder::jpeg(b"\xFF\xD8garbage").is_none());
}

fn goes_file(name: &str) -> super::transport::File {
    let bytes = fixture(name);
    let headers = super::header::Headers::parse(&bytes).unwrap();
    assert_eq!(headers.header_len as usize, bytes.len());
    super::transport::File {
        spacecraft: 0,
        vcid: 0,
        apid: 0,
        headers,
        header_bytes: bytes,
        data: Vec::new(),
        complete: true,
        rice_decoded: false,
        lines_lost: 0,
    }
}

#[test]
fn real_goes16_headers() {
    use super::products::{Class, classify, image_info};

    let f = goes_file("goes16-fd-ch13.hdr");
    let h = &f.headers;
    let img = h.image.unwrap();
    assert_eq!(
        (img.bits_per_pixel, img.columns, img.lines, img.compression),
        (8, 5424, 339, 1)
    );
    assert_eq!(h.data_len(), 5424 * 339);
    let s = h.segment.unwrap();
    assert_eq!(
        (s.number, s.segments, s.start_line, s.columns, s.lines),
        (13, 16, 13 * 339, 5424, 5424)
    );
    let n = h.noaa.unwrap();
    assert_eq!(
        (&n.agency, n.product, n.sub_product, n.compression),
        (b"NOAA", 16, 13, 1)
    );
    let r = h.rice.unwrap();
    assert_eq!(
        (r.flags, r.pixels_per_block, r.lines_per_packet),
        (177, 32, 1)
    );
    assert!(h.rice_coded());
    let nav = h.navigation.as_ref().unwrap();
    assert_eq!(
        (nav.projection.as_str(), nav.cfac, nav.coff),
        ("geos(-75.0)", 20_425_862, 2712)
    );
    assert!(
        h.data_function
            .as_deref()
            .unwrap()
            .starts_with("$HALFTONE:=8")
    );
    assert_eq!(classify(&f), Class::Image);
    assert_eq!(
        image_info(&f).name(),
        "G16/FD/CH13/2018-02-23/G16_FD_CH13_20180223T173039Z"
    );

    let f = goes_file("goes16-fd-ch02.hdr");
    assert_eq!(f.headers.segment.unwrap().number, 2);
    assert_eq!(
        image_info(&f).name(),
        "G16/FD/CH02/2018-02-23/G16_FD_CH02_20180223T173039Z"
    );

    let f = goes_file("goes16-m1-ch02.hdr");
    assert!(f.headers.segment.is_none());
    let img = f.headers.image.unwrap();
    assert_eq!((img.columns, img.lines), (2000, 2000));
    assert_eq!(
        image_info(&f).name(),
        "G16/M1/CH02/2018-02-23/G16_M1_CH02_20180223T185828Z"
    );

    let f = goes_file("goes16-m2-ch07.hdr");
    assert_eq!(
        image_info(&f).name(),
        "G16/M2/CH07/2018-02-23/G16_M2_CH07_20180223T185958Z"
    );
}
