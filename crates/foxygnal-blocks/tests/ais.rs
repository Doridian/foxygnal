//! AIS end to end. Payload decoding is checked against public `!AIVDM`
//! sentences (from the gpsd AIVDM documentation and common decoder test
//! suites) whose fields are well known. A small independent transmitter
//! (its own six-bit dearmouring, bitwise FCS, bit stuffing, NRZI and a GMSK
//! modulator with a Gaussian frequency pulse) makes bursts that go through a
//! channel filter, `fm_demod` and `ais_decoder` with a frequency offset and
//! noise, and the sensitivity is measured against Eb/N0.

use std::f64::consts::TAU;

use foxygnal_blocks::digital::ais;
use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{BufferKind, Complex32, Flowgraph, Instance, StreamMeta};
use serde_json::json;

fn block(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry()
        .build(kind, p.as_object().unwrap())
        .unwrap()
}

// ---- Sentences -------------------------------------------------------------------

/// A sentence's payload and fill bits, after checking its checksum.
fn sentence(s: &str) -> (&str, usize) {
    let (body, sum) = s[1..].split_once('*').unwrap();
    let x = body.bytes().fold(0u8, |a, c| a ^ c);
    assert_eq!(format!("{x:02X}"), sum, "checksum of {s}");
    let f: Vec<&str> = body.split(',').collect();
    (f[5], f[6].parse().unwrap())
}

/// Payload characters → bits (one per byte), done independently of the
/// decoder.
fn payload_bits(sentences: &[&str]) -> Vec<u8> {
    let mut bits = Vec::new();
    let mut fill = 0;
    for s in sentences {
        let (p, f) = sentence(s);
        for c in p.bytes() {
            let mut v = c - 48;
            if v > 40 {
                v -= 8;
            }
            bits.extend((0..6).rev().map(|i| (v >> i) & 1));
        }
        fill = f;
    }
    bits.truncate(bits.len() - fill);
    bits
}

fn pack(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| (0..8).fold(0, |a, i| a << 1 | c.get(i).copied().unwrap_or(0)))
        .collect()
}

fn describe(sentences: &[&str]) -> String {
    let bits = payload_bits(sentences);
    let mut out = String::new();
    ais::describe(&pack(&bits), bits.len(), &mut out);
    out
}

/// Public test vectors and what they hold.
const VECTORS: &[(&[&str], &str)] = &[
    (
        &["!AIVDM,1,1,,B,177KQJ5000G?tO`K>RA1wUbN0TKH,0*5C"],
        "mmsi=477553000 type=1 status=moored lat=47.582833 lon=-122.345833 sog=0.0 cog=51.0 \
         heading=181 rot=0 second=15",
    ),
    (
        &["!AIVDM,1,1,,A,15RTgt0PAso;90TKcjM8h6g208CQ,0*4A"],
        "mmsi=371798000 type=1 status=under_way lat=48.381633 lon=-123.395383 sog=12.3 \
         cog=224.0 heading=215 rot=left second=33",
    ),
    (
        &["!AIVDM,1,1,,B,15M67FC000G?ufbE`FepT@3n00Sa,0*5C"],
        "mmsi=366053209 type=1 status=restricted lat=37.802118 lon=-122.341618 sog=0.0 \
         cog=219.3 heading=1 rot=0 second=59",
    ),
    (
        &["!AIVDM,1,1,,A,403OviQuMGCqWrRO9>E6fE700@GO,0*4D"],
        "mmsi=003669702 type=4 time=2007-05-14T19:57:39Z lat=36.883767 lon=-76.352362 \
         epfd=surveyed",
    ),
    (
        &[
            "!AIVDM,2,1,1,A,55?MbV02;H;s<HtKR20EHE:0@T4@Dn2222222216L961O5Gf0NSQEp6ClRp8,0*1C",
            "!AIVDM,2,2,1,A,88888888880,2*25",
        ],
        "mmsi=351759000 type=5 imo=9134270 callsign=\"3FOF8\" name=\"EVER DIADEM\" \
         shiptype=70 length=295 beam=32 eta=05-15T14:00 draught=12.2 dest=\"NEW YORK\"",
    ),
    (
        &[
            "!AIVDM,2,1,3,B,55P5TL01VIaAL@7WKO@mBplU@<PDhh000000001S;AJ::4A80?4i@E53,0*3E",
            "!AIVDM,2,2,3,B,1@0000000000000,2*55",
        ],
        "mmsi=369190000 type=5 imo=6710932 callsign=\"WDA9674\" name=\"MT.MITCHELL\" \
         shiptype=99 length=180 beam=20 eta=01-02T08:00 draught=6.0 dest=\"SEATTLE\"",
    ),
    (
        &["!AIVDM,1,1,,B,C5N3SRgPEnJGEBT>NhWAwwo862PaLELTBJ:V00000000S0D:R220,0*0B"],
        "mmsi=367059850 type=19 lat=29.543695 lon=-88.810392 sog=8.7 cog=335.9 second=46 \
         name=\"CAPT.J.RIMES\" shiptype=70 length=26 beam=8",
    ),
    (
        &["!AIVDM,1,1,,A,H42O55i18tMET00000000000000,2*6D"],
        "mmsi=271041815 type=24 part=A name=\"PROGUY\"",
    ),
    (
        &["!AIVDM,1,1,,A,H42O55lti4hhhilD3nink000?050,0*40"],
        "mmsi=271041815 type=24 part=B shiptype=60 vendor=\"1D0\" callsign=\"TC6163\" \
         length=15 beam=5",
    ),
    (
        &["!AIVDM,1,1,,B,K815>P8=5EikdUet,0*6B"],
        "mmsi=538005120 type=27 status=under_way lat=24.681667 lon=-79.641667 sog=11 cog=223",
    ),
];

#[test]
fn public_vectors_decode() {
    for (s, want) in VECTORS {
        assert_eq!(describe(s), *want, "{s:?}");
    }
}

/// Builds message bits field by field (most significant bit first).
#[derive(Default)]
struct Msg(Vec<u8>);

impl Msg {
    fn u(mut self, v: i64, n: usize) -> Self {
        self.0.extend((0..n).rev().map(|i| ((v >> i) & 1) as u8));
        self
    }
    fn text(mut self, t: &str, chars: usize) -> Self {
        let mut t: Vec<u8> = t.bytes().collect();
        t.resize(chars, b'@');
        for c in t {
            self = self.u(i64::from(c & 0x3F), 6);
        }
        self
    }
    /// Degrees → 1/10000 minute.
    fn deg(self, d: f64, n: usize) -> Self {
        self.u((d * 600_000.0).round() as i64, n)
    }
}

/// Class B position: MMSI 211234560, 3.2 kn, 41.5°, heading 40, second 12.
fn type18() -> Vec<u8> {
    Msg::default()
        .u(18, 6)
        .u(0, 2)
        .u(211_234_560, 30)
        .u(0, 8)
        .u(32, 10)
        .u(1, 1)
        .deg(-0.5, 28)
        .deg(51.25, 27)
        .u(415, 12)
        .u(40, 9)
        .u(12, 6)
        .u(0, 29)
        .0
}

/// Aid to navigation with a name extension.
fn type21() -> Vec<u8> {
    Msg::default()
        .u(21, 6)
        .u(0, 2)
        .u(992_111_222, 30)
        .u(9, 5)
        .text("NORTH PIER LIGHT AND", 20)
        .u(0, 1)
        .deg(10.123455, 28)
        .deg(-33.5, 27)
        .u(3, 9)
        .u(2, 9)
        .u(1, 6)
        .u(1, 6)
        .u(1, 4)
        .u(30, 6)
        .u(1, 1)
        .u(0, 8)
        .u(0, 1)
        .u(1, 1)
        .u(0, 2)
        .text(" FOG\"HORN", 9)
        .u(0, 2)
        .0
}

#[test]
fn built_messages_decode() {
    for (bits, want) in [
        (
            type18(),
            "mmsi=211234560 type=18 lat=51.250000 lon=-0.500000 sog=3.2 cog=41.5 heading=40 \
             second=12",
        ),
        (
            type21(),
            "mmsi=992111222 type=21 aid_type=9 name=\"NORTH PIER LIGHT AND FOG\\\"HORN\" \
             lat=-33.500000 lon=10.123455 length=5 beam=2 off_position=1 virtual=1",
        ),
    ] {
        assert_eq!(bits.len() % 8, 0);
        let mut out = String::new();
        ais::describe(&pack(&bits), bits.len(), &mut out);
        assert_eq!(out, want);
    }
}

#[test]
fn other_types_and_short_messages_come_out_raw() {
    // Type 8 (binary broadcast) from MMSI 2053501.
    let mut bits = vec![0, 0, 1, 0, 0, 0, 0, 0];
    bits.extend((0..30).rev().map(|i| ((2_053_501u32 >> i) & 1) as u8));
    bits.extend([1, 0, 1, 0, 1, 0, 1, 0, 0, 0]);
    let mut out = String::new();
    ais::describe(&pack(&bits), bits.len(), &mut out);
    let hex: String = pack(&bits).iter().map(|b| format!("{b:02X}")).collect();
    assert_eq!(out, format!("mmsi=002053501 type=8 raw={hex}"));
    // A type 1 cut short.
    let bits = payload_bits(&["!AIVDM,1,1,,B,177KQJ5000G?tO`K>RA1wUbN0TKH,0*5C"]);
    out.clear();
    ais::describe(&pack(&bits[..80]), 80, &mut out);
    assert!(
        out.starts_with("mmsi=477553000 type=1 short=1 raw="),
        "{out}"
    );
}

#[test]
fn nmea_output_rebuilds_the_sentences() {
    for (s, _) in VECTORS {
        // Bit-exact payloads only (the decoder's frames are whole bytes).
        let bits = payload_bits(s);
        if !bits.len().is_multiple_of(8) {
            continue;
        }
        let (_, fill) = sentence(s[s.len() - 1]);
        let ch = s[0].split(',').nth(4).unwrap();
        let seq = s[0].split(',').nth(3).unwrap().parse().unwrap_or(0);
        let mut out = String::new();
        ais::nmea(&pack(&bits), bits.len(), ch, seq, &mut out);
        let want: String = s.iter().map(|l| format!("{l}\n")).collect();
        if s.len() == 1 {
            assert_eq!(out, want);
        } else {
            // Split differently (60 characters per sentence), same bits.
            let lines: Vec<&str> = out.lines().collect();
            assert_eq!(payload_bits(&lines), bits);
            assert!(out.ends_with(&format!(",{fill}*{}\n", &lines[1][lines[1].len() - 2..])));
        }
    }
}

// ---- Transmitter -----------------------------------------------------------------

fn fcs(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in data {
        for i in 0..8 {
            let fb = (crc & 1) as u8 ^ ((b >> i) & 1);
            crc >>= 1;
            if fb == 1 {
                crc ^= 0x8408;
            }
        }
    }
    !crc
}

/// The line bits (NRZI levels) of one burst: ramp-up, 24 training bits,
/// flag, the message (each byte least significant bit first, 1s stuffed),
/// FCS, flag.
fn burst(msg_bits: &[u8]) -> Vec<u8> {
    let mut bytes: Vec<u8> = msg_bits
        .chunks(8)
        .map(|c| (0..8).fold(0u8, |a, i| a | (c.get(i).copied().unwrap_or(0) << i)))
        .collect();
    let c = fcs(&bytes);
    bytes.extend([c as u8, (c >> 8) as u8]);
    let mut data: Vec<u8> = (0..24).map(|i| (i % 2) as u8).collect();
    let flag = [0, 1, 1, 1, 1, 1, 1, 0];
    data.extend(flag);
    let mut ones = 0;
    for b in bytes {
        for i in 0..8 {
            let bit = (b >> i) & 1;
            data.push(bit);
            ones = if bit == 1 { ones + 1 } else { 0 };
            if ones == 5 {
                data.push(0);
                ones = 0;
            }
        }
    }
    data.extend(flag);
    data.extend([0; 4]);
    let mut level = 0u8;
    data.iter()
        .map(|&b| {
            if b == 0 {
                level ^= 1;
            }
            level
        })
        .collect()
}

struct Rng(u64);
impl Rng {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        (-2.0 * self.uniform().ln()).sqrt() * (TAU * self.uniform()).cos()
    }
}

const RATE: f64 = 48_000.0;
const SPS: usize = 5;

/// GMSK (BT 0.4, h 0.5) complex baseband at 48 kS/s, unit amplitude:
/// `bursts` separated by silence, at `offset` Hz, with complex noise for an
/// Eb/N0 of `ebn0_db` (none if infinite).
fn gmsk(bursts: &[Vec<u8>], offset: f64, ebn0_db: f64, rng: &mut Rng) -> Vec<Complex32> {
    // Gaussian frequency pulse, 3 bits long.
    let bt = 0.4;
    let sigma = (2.0f64.ln()).sqrt() / (TAU * bt) * SPS as f64;
    let half = 3 * SPS / 2 + SPS;
    let g: Vec<f64> = (0..=2 * half)
        .map(|i| {
            let t = i as f64 - half as f64;
            (-t * t / (2.0 * sigma * sigma)).exp()
        })
        .collect();
    let gsum: f64 = g.iter().sum();
    let mut nrz = Vec::new();
    let gap = 4000;
    nrz.extend(std::iter::repeat_n(0.0, gap));
    let mut on = vec![false; gap];
    for b in bursts {
        for &l in b {
            for _ in 0..SPS {
                nrz.push(if l == 1 { 1.0 } else { -1.0 });
                on.push(true);
            }
        }
        nrz.extend(std::iter::repeat_n(0.0, gap));
        on.extend(std::iter::repeat_n(false, gap));
    }
    let sigma_n = if ebn0_db.is_finite() {
        // Eb = 1 · SPS samples; N0 = 2σ² per sample.
        (SPS as f64 / 10f64.powf(ebn0_db / 10.0) / 2.0).sqrt()
    } else {
        0.0
    };
    let mut phase = rng.uniform() * TAU;
    let mut out = Vec::with_capacity(nrz.len());
    for (i, &is_on) in on.iter().enumerate() {
        let f: f64 = (0..g.len())
            .filter_map(|k| {
                (i + k)
                    .checked_sub(half)
                    .and_then(|j| nrz.get(j))
                    .map(|&v| v * g[k])
            })
            .sum::<f64>()
            / gsum;
        phase += TAU * (f * 2400.0 + offset) / RATE;
        let a = if is_on || f != 0.0 { 1.0 } else { 0.0 };
        out.push(Complex32::new(
            (a * phase.cos() + sigma_n * rng.gauss()) as f32,
            (a * phase.sin() + sigma_n * rng.gauss()) as f32,
        ));
    }
    out
}

/// Channel filter, FM demodulator, AIS decoder.
fn receive(input: Vec<Complex32>, decoder: serde_json::Value, cap: Option<usize>) -> String {
    let mut fg = match cap {
        Some(c) => Flowgraph::new()
            .with_buffer_capacity(c)
            .with_buffer_kind(BufferKind::Plain),
        None => Flowgraph::new(),
    };
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(input, StreamMeta::with_rate(RATE)),
    );
    let chan = fg.add(
        "chan",
        block(
            "freq_xlating_fir",
            json!({"offset": 0.0, "cutoff": 7500.0, "transition": 4000.0}),
        ),
    );
    let fm = fg.add(
        "fm",
        block(
            "fm_demod",
            json!({"deviation": 2400.0, "deemphasis": "off"}),
        ),
    );
    let dec = fg.add("ais", block("ais_decoder", decoder));
    let (sink, out) = VectorSink::<u8>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, chan, 0).unwrap();
    fg.connect(chan, 0, fm, 0).unwrap();
    fg.connect(fm, 0, dec, 0).unwrap();
    fg.connect(dec, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    String::from_utf8(out.lock().unwrap().clone()).unwrap()
}

/// Bursts of all byte-aligned test vectors.
fn vector_bursts() -> (Vec<Vec<u8>>, Vec<&'static str>) {
    VECTORS
        .iter()
        .filter_map(|(s, want)| {
            let bits = payload_bits(s);
            bits.len().is_multiple_of(8).then(|| (burst(&bits), *want))
        })
        .unzip()
}

#[test]
fn bursts_decode_through_fm_demod() {
    let (bursts, want) = vector_bursts();
    let mut rng = Rng(0xA15);
    // 1.2 kHz off (7 ppm at 162 MHz), Eb/N0 20 dB.
    let x = gmsk(&bursts, 1200.0, 20.0, &mut rng);
    for cap in [None, Some(97)] {
        let text = receive(x.clone(), json!({"channel": "B"}), cap);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), want.len(), "{text}");
        for (l, w) in lines.iter().zip(&want) {
            let (t, rest) = l.split_once(" ch=B ").unwrap();
            assert!(t.starts_with("t="), "{l}");
            assert_eq!(rest, *w);
        }
    }
}

#[test]
fn nmea_output_and_offsets() {
    let s = "!AIVDM,1,1,,A,15RTgt0PAso;90TKcjM8h6g208CQ,0*4A";
    let b = burst(&payload_bits(&[s]));
    for offset in [-2000.0, 0.0, 2000.0] {
        let mut rng = Rng(offset as u64 + 99);
        let x = gmsk(&[b.clone(), b.clone()], offset, 25.0, &mut rng);
        let text = receive(x, json!({"channel": "A", "format": "nmea"}), None);
        assert_eq!(text, format!("{s}\n{s}\n"), "offset {offset}");
    }
}

#[test]
fn noise_alone_decodes_nothing() {
    let mut rng = Rng(5);
    let x: Vec<Complex32> = (0..RATE as usize * 4)
        .map(|_| Complex32::new(rng.gauss() as f32, rng.gauss() as f32))
        .collect();
    assert_eq!(receive(x, json!({}), None), "");
}

/// Messages decoded out of `n` at each Eb/N0. Run with `--ignored
/// --nocapture` to print the curve.
fn decode_rate(ebn0: f64, n: usize, offset: f64) -> usize {
    let s = "!AIVDM,1,1,,B,15M67FC000G?ufbE`FepT@3n00Sa,0*5C";
    let b = burst(&payload_bits(&[s]));
    let mut rng = Rng(1234 + ebn0 as u64);
    let x = gmsk(&vec![b; n], offset, ebn0, &mut rng);
    receive(x, json!({}), None).lines().count()
}

#[test]
fn decodes_most_bursts_at_14_db() {
    let n = 50;
    let got = decode_rate(14.0, n, 500.0);
    assert!(got >= 42, "{got}/{n}");
}

#[test]
#[ignore]
fn sensitivity_curve() {
    let n = 200;
    for ebn0 in [8.0, 10.0, 11.0, 12.0, 13.0, 14.0, 16.0, 20.0] {
        let got = decode_rate(ebn0, n, 0.0);
        let off = decode_rate(ebn0, n, 2000.0);
        println!("Eb/N0 {ebn0:4.1} dB: {got}/{n} (offset 0), {off}/{n} (offset 2 kHz)");
    }
}
