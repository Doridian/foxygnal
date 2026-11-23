//! WAV and SigMF round trips through real files.

use std::path::{Path, PathBuf};

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{Complex32, Flowgraph, Instance, Params, Sample};
use serde_json::json;

fn params(v: serde_json::Value) -> Params {
    v.as_object().unwrap().clone()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("foxygnal-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn build(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry().build(kind, &params(p)).unwrap()
}

/// `src -> head(count) -> sink`, run to completion.
fn record(src: Instance, count: usize, ty: &str, sink: Instance) {
    let mut fg = Flowgraph::new().with_buffer_capacity(37);
    let s = fg.add("src", src);
    let h = fg.add("head", build("head", json!({"type": ty, "count": count})));
    let k = fg.add("sink", sink);
    fg.connect(s, 0, h, 0).unwrap();
    fg.connect(h, 0, k, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
}

/// Run `src` into a vector sink; also return the configured sample rate and
/// centre frequency of its output.
fn play<T: Sample>(src: Instance) -> (Vec<T>, Option<f64>, Option<f64>) {
    let mut fg = Flowgraph::new().with_buffer_capacity(37);
    let s = fg.add("src", src);
    let (sink, out) = VectorSink::<T>::instance();
    let k = fg.add("sink", sink);
    fg.connect(s, 0, k, 0).unwrap();
    let meta = fg.configure().unwrap()[0].meta;
    fg.start().unwrap().wait().unwrap();
    let v = out.lock().unwrap().clone();
    (v, meta.sample_rate, meta.center_freq)
}

fn tone(ty: &str, rate: f64) -> Instance {
    build(
        "signal_source",
        json!({"type": ty, "frequency": 1000.0, "amplitude": 0.5, "sample_rate": rate}),
    )
}

fn path_str(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

#[test]
fn wav_float_round_trip() {
    let path = scratch("tone.wav");
    record(
        tone("f32", 22_050.0),
        1000,
        "f32",
        build("wav_sink", json!({"type": "f32", "path": path_str(&path)})),
    );
    let (got, rate, _) = play::<f32>(build("wav_source", json!({"path": path_str(&path)})));
    assert_eq!(rate, Some(22_050.0));
    assert_eq!(got.len(), 1000);
    assert_eq!(got, first_n(tone("f32", 22_050.0), 1000));
}

/// The first `n` items of a source.
fn first_n(src: Instance, n: usize) -> Vec<f32> {
    let mut fg = Flowgraph::new();
    let s = fg.add("src", src);
    let h = fg.add("head", build("head", json!({"type": "f32", "count": n})));
    let (sink, out) = VectorSink::<f32>::instance();
    let k = fg.add("sink", sink);
    fg.connect(s, 0, h, 0).unwrap();
    fg.connect(h, 0, k, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    out.lock().unwrap().clone()
}

#[test]
fn wav_stereo_iq_pcm16_and_channels() {
    let path = scratch("iq.wav");
    record(
        tone("c32", 48_000.0),
        480,
        "c32",
        build(
            "wav_sink",
            json!({"type": "c32", "format": "pcm16", "path": path_str(&path)}),
        ),
    );
    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().bits_per_sample, 16);
    assert_eq!(reader.duration(), 480);
    drop(reader);

    let (iq, rate, _) = play::<Complex32>(build(
        "wav_source",
        json!({"type": "c32", "path": path_str(&path)}),
    ));
    assert_eq!(rate, Some(48_000.0));
    assert_eq!(iq.len(), 480);
    // A 0.5-amplitude complex tone, quantized to 16 bits.
    for v in &iq {
        assert!((v.norm() - 0.5).abs() < 1e-3, "{v}");
    }
    // The right channel alone is Q; as i16 it's the raw PCM.
    let (q, _, _) = play::<i16>(build(
        "wav_source",
        json!({"type": "i16", "channel": 1, "path": path_str(&path)}),
    ));
    assert_eq!(q.len(), 480);
    for (a, b) in q.iter().zip(&iq) {
        assert!((*a as f32 / 32768.0 - b.im).abs() < 1e-4);
    }
    // c32 from a mono file, or a channel that doesn't exist, fail clearly.
    let mono = scratch("mono.wav");
    record(
        tone("f32", 8000.0),
        10,
        "f32",
        build("wav_sink", json!({"type": "f32", "path": path_str(&mono)})),
    );
    let mut fg = Flowgraph::new();
    let s = fg.add(
        "src",
        build(
            "wav_source",
            json!({"type": "c32", "path": path_str(&mono)}),
        ),
    );
    let (k, _) = VectorSink::<Complex32>::instance();
    let k = fg.add("k", k);
    fg.connect(s, 0, k, 0).unwrap();
    let e = fg.configure().unwrap_err().to_string();
    assert!(e.contains("mono"), "{e}");
}

#[test]
fn wav_repeat_loops() {
    let path = scratch("short.wav");
    record(
        tone("f32", 8000.0),
        10,
        "f32",
        build("wav_sink", json!({"type": "f32", "path": path_str(&path)})),
    );
    let mut fg = Flowgraph::new().with_buffer_capacity(7);
    let s = fg.add(
        "src",
        build(
            "wav_source",
            json!({"path": path_str(&path), "repeat": true}),
        ),
    );
    let h = fg.add("head", build("head", json!({"type": "f32", "count": 35})));
    let (k, out) = VectorSink::<f32>::instance();
    let k = fg.add("k", k);
    fg.connect(s, 0, h, 0).unwrap();
    fg.connect(h, 0, k, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let out = out.lock().unwrap();
    assert_eq!(out.len(), 35);
    assert_eq!(out[..10], out[10..20]);
    assert_eq!(out[..5], out[30..35]);
}

#[test]
fn wav_sink_needs_a_rate() {
    let mut fg = Flowgraph::new();
    let s = fg.add("src", VectorSource::instance(vec![0.0f32; 4]));
    let k = fg.add(
        "wav",
        build(
            "wav_sink",
            json!({"type": "f32", "path": "/nonexistent/x.wav"}),
        ),
    );
    fg.connect(s, 0, k, 0).unwrap();
    let e = fg.configure().unwrap_err().to_string();
    assert!(e.contains("wav") && e.contains("sample rate"), "{e}");
}

#[test]
fn sigmf_round_trip() {
    let base = scratch("rec");
    // file_source carries a centre frequency, so record through one.
    let raw = scratch("rec.c32");
    record(
        tone("c32", 250_000.0),
        5000,
        "c32",
        build("file_sink", json!({"type": "c32", "path": path_str(&raw)})),
    );
    record(
        build(
            "file_source",
            json!({"type": "c32", "path": path_str(&raw), "sample_rate": 250_000.0,
                   "center_freq": 100.1e6}),
        ),
        5000,
        "c32",
        build(
            "sigmf_sink",
            json!({"type": "c32", "path": path_str(&base)}),
        ),
    );
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(base.with_extension("sigmf-meta")).unwrap())
            .unwrap();
    assert_eq!(meta["global"]["core:datatype"], "cf32_le");
    assert_eq!(meta["global"]["core:sample_rate"], 250_000.0);
    assert_eq!(meta["global"]["core:recorder"], "foxygnal");
    assert!(meta["global"]["core:version"].is_string());
    assert_eq!(meta["captures"][0]["core:sample_start"], 0);
    assert_eq!(meta["captures"][0]["core:frequency"], 100.1e6);
    assert!(
        meta["captures"][0]["core:datetime"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    // The data file is the raw samples, byte for byte.
    assert_eq!(
        std::fs::read(base.with_extension("sigmf-data")).unwrap(),
        std::fs::read(&raw).unwrap()
    );
    // Reading back, via either file name.
    for p in ["sigmf-meta", "sigmf-data"] {
        let (iq, rate, freq) = play::<Complex32>(build(
            "sigmf_source",
            json!({"type": "c32", "path": path_str(&base.with_extension(p))}),
        ));
        assert_eq!((rate, freq), (Some(250_000.0), Some(100.1e6)));
        assert_eq!(iq.len(), 5000);
        assert!((iq[0].re - 0.5).abs() < 1e-6);
    }
}

fn write_sigmf(base: &Path, datatype: &str, data: &[u8]) {
    std::fs::write(
        base.with_extension("sigmf-meta"),
        json!({
            "global": {"core:datatype": datatype, "core:sample_rate": 2.4e6,
                       "core:version": "1.0.0"},
            "captures": [{"core:sample_start": 0, "core:frequency": 433.92e6}],
            "annotations": []
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(base.with_extension("sigmf-data"), data).unwrap();
}

#[test]
fn sigmf_integer_types() {
    // ci16_le: (16384, -32768), (0, 32767)
    let base = scratch("ci16");
    let mut data = Vec::new();
    for v in [16384i16, -32768, 0, 32767] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    write_sigmf(&base, "ci16_le", &data);
    let path = path_str(&base.with_extension("sigmf-meta"));
    let (iq, rate, freq) =
        play::<Complex32>(build("sigmf_source", json!({"type": "c32", "path": path})));
    assert_eq!((rate, freq), (Some(2.4e6), Some(433.92e6)));
    assert_eq!(
        iq,
        vec![
            Complex32::new(0.5, -1.0),
            Complex32::new(0.0, 32767.0 / 32768.0)
        ]
    );
    // Native i16 output: interleaved, at twice the item rate.
    let (raw, rate, _) = play::<i16>(build("sigmf_source", json!({"type": "i16", "path": path})));
    assert_eq!(raw, vec![16384, -32768, 0, 32767]);
    assert_eq!(rate, Some(4.8e6));
    // f32 from a complex recording is refused with a hint.
    let mut fg = Flowgraph::new();
    let s = fg.add(
        "src",
        build("sigmf_source", json!({"type": "f32", "path": path})),
    );
    let (k, _) = VectorSink::<f32>::instance();
    let k = fg.add("k", k);
    fg.connect(s, 0, k, 0).unwrap();
    let e = fg.configure().unwrap_err().to_string();
    assert!(e.contains("choose c32"), "{e}");

    // cu8 through the RTL-SDR converter, with small buffers so pairs split
    // across wraps; the converter halves the rate back.
    let base = scratch("cu8");
    let bytes: Vec<u8> = (0..1001u32).map(|i| (i % 256) as u8).collect();
    write_sigmf(&base, "cu8", &bytes);
    let mut fg = Flowgraph::new().with_buffer_capacity(7);
    let s = fg.add(
        "src",
        build(
            "sigmf_source",
            json!({"type": "u8", "path": path_str(&base)}),
        ),
    );
    let c = fg.add("conv", build("cu8_to_c32", json!({})));
    let (k, out) = VectorSink::<Complex32>::instance();
    let k = fg.add("k", k);
    fg.connect(s, 0, c, 0).unwrap();
    fg.connect(c, 0, k, 0).unwrap();
    let meta = fg.configure().unwrap();
    assert_eq!(meta[1].meta.sample_rate, Some(2.4e6));
    fg.start().unwrap().wait().unwrap();
    let out = out.lock().unwrap();
    assert_eq!(out.len(), 500);
    for (i, v) in out.iter().enumerate() {
        let want = |b: u32| ((b % 256) as f32 - 127.5) / 127.5;
        let d = *v - Complex32::new(want(2 * i as u32), want(2 * i as u32 + 1));
        assert!(d.norm() < 1e-6, "{i}: {v}");
    }
}

/// Tags into a SigMF recording and back out: retunes become capture
/// segments, other tags annotations, all at the right sample.
#[test]
fn sigmf_tags_round_trip() {
    use foxygnal_core::{Tag, TagValue};
    let base = scratch("tagged");
    let n = 1000;
    let data: Vec<Complex32> = (0..n).map(|i| Complex32::new(i as f32, 0.0)).collect();
    let tags = vec![
        Tag::new(0, "hw", "Simulated FM band (sim:)"),
        Tag::new(0, "rx_freq", 100e6),
        Tag::new(300, "rx_freq", 101.5e6),
        Tag::new(500, "overflow", 42i64),
        Tag::new(700, "packet", "hello"),
        Tag::new(800, "rx_gain", f64::NAN),
    ];
    let mut fg = Flowgraph::new().with_buffer_capacity(37);
    let s = fg.add("src", VectorSource::with_tags(data, tags));
    let k = fg.add(
        "sink",
        build(
            "sigmf_sink",
            json!({"type": "c32", "path": path_str(&base)}),
        ),
    );
    fg.connect(s, 0, k, 0).unwrap();
    fg.start().unwrap().wait().unwrap();

    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(base.with_extension("sigmf-meta")).unwrap())
            .unwrap();
    assert!(!base.with_extension("sigmf-meta.tmp").exists());
    assert_eq!(meta["global"]["core:hw"], "Simulated FM band (sim:)");
    let caps = meta["captures"].as_array().unwrap();
    assert_eq!(caps.len(), 2, "{caps:?}");
    assert_eq!(caps[0]["core:sample_start"], 0);
    assert_eq!(caps[0]["core:frequency"], 100e6);
    assert_eq!(caps[1]["core:sample_start"], 300);
    assert_eq!(caps[1]["core:frequency"], 101.5e6);
    let ann = meta["annotations"].as_array().unwrap();
    assert_eq!(ann.len(), 3, "{ann:?}");
    assert_eq!(ann[0]["core:sample_start"], 500);
    assert_eq!(ann[0]["core:label"], "overflow");
    assert_eq!(ann[0]["foxygnal:overflow"], 42);
    assert_eq!(ann[1]["core:sample_start"], 700);
    assert_eq!(ann[1]["foxygnal:packet"], "hello");
    assert_eq!(ann[2]["core:label"], "rx_gain");
    assert!(ann[2]["foxygnal:rx_gain"].is_null(), "NaN has no JSON form");

    // Back out as tags at the same items, again on every loop.
    let mut fg = Flowgraph::new().with_buffer_capacity(37);
    let s = fg.add(
        "src",
        build(
            "sigmf_source",
            json!({"type": "c32", "path": path_str(&base), "repeat": true}),
        ),
    );
    let h = fg.add(
        "head",
        build("head", json!({"type": "c32", "count": 2 * n})),
    );
    let (sink, out, got) = VectorSink::<Complex32>::with_tags();
    let k = fg.add("sink", sink);
    fg.connect(s, 0, h, 0).unwrap();
    fg.connect(h, 0, k, 0).unwrap();
    let meta = fg.configure().unwrap()[0].meta;
    assert_eq!(meta.center_freq, Some(100e6));
    fg.start().unwrap().wait().unwrap();
    assert_eq!(out.lock().unwrap().len(), 2 * n);
    let got = got.lock().unwrap().clone();
    let key = |t: &Tag| (t.offset, t.key.to_string());
    let mut want = Vec::new();
    for pass in [0, n as u64] {
        want.extend([
            (pass, "hw".to_owned()),
            (pass, "rx_freq".to_owned()),
            (pass + 300, "rx_freq".to_owned()),
            (pass + 500, "overflow".to_owned()),
            (pass + 700, "packet".to_owned()),
            (pass + 800, "rx_gain".to_owned()),
        ]);
    }
    assert_eq!(got.iter().map(key).collect::<Vec<_>>(), want);
    assert_eq!(got[2].value, TagValue::Float(101.5e6));
    assert_eq!(got[3].value, TagValue::Int(42));
    assert_eq!(got[4].value, TagValue::Str("hello".into()));
    assert!(matches!(got[5].value, TagValue::Float(v) if v.is_nan()));
}

/// Interleaved I/Q (ci16): tags land on sample indices in the metadata and
/// on item indices again when played back as raw i16.
#[test]
fn sigmf_interleaved_tags_use_sample_indices() {
    use foxygnal_core::Tag;
    let base = scratch("tagged-ci16");
    let data: Vec<i16> = (0..400).collect();
    let mut fg = Flowgraph::new().with_buffer_capacity(37);
    let s = fg.add(
        "src",
        VectorSource::with_tags(data, vec![Tag::new(100, "burst", true)]),
    );
    let k = fg.add(
        "sink",
        build(
            "sigmf_sink",
            json!({"type": "i16", "complex": true, "path": path_str(&base)}),
        ),
    );
    fg.connect(s, 0, k, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(base.with_extension("sigmf-meta")).unwrap())
            .unwrap();
    assert_eq!(meta["global"]["core:datatype"], "ci16_le");
    assert_eq!(meta["annotations"][0]["core:sample_start"], 50);
    assert_eq!(meta["annotations"][0]["foxygnal:burst"], true);

    let tags_of = |ty: &str| {
        let mut fg = Flowgraph::new();
        let s = fg.add(
            "src",
            build("sigmf_source", json!({"type": ty, "path": path_str(&base)})),
        );
        let (k, got) = if ty == "i16" {
            let (sink, _, got) = VectorSink::<i16>::with_tags();
            (fg.add("sink", sink), got)
        } else {
            let (sink, _, got) = VectorSink::<Complex32>::with_tags();
            (fg.add("sink", sink), got)
        };
        fg.connect(s, 0, k, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        let got = got.lock().unwrap().clone();
        got.iter()
            .map(|t| (t.offset, t.key.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(tags_of("i16"), vec![(100, "burst".to_owned())]);
    assert_eq!(tags_of("c32"), vec![(50, "burst".to_owned())]);
}

/// The metadata is usable from the moment a recording starts, and later
/// annotations reach it at the latest when it finishes.
#[test]
fn sigmf_writer_meta_is_usable_before_finish() {
    use foxygnal_blocks::sigmf::{RecordingInfo, SigmfWriter, parse_meta, parse_tags};
    use foxygnal_core::{StreamType, TagValue};
    let base = scratch("crash");
    let info = RecordingInfo {
        sample_rate: Some(48_000.0),
        frequency: Some(7.1e6),
        description: Some("test".into()),
        ..RecordingInfo::default()
    };
    let mut w = SigmfWriter::create(&base, StreamType::F32, &info).unwrap();
    w.write(&[0u8; 400]).unwrap();
    w.tag(10, "squelch", &TagValue::Bool(true));
    let text = std::fs::read_to_string(base.with_extension("sigmf-meta")).unwrap();
    let m = parse_meta(&text).unwrap();
    assert_eq!((m.sample_rate, m.frequency), (Some(48_000.0), Some(7.1e6)));
    assert!(
        parse_tags(&text)
            .unwrap()
            .iter()
            .all(|t| &*t.key != "squelch")
    );
    w.gap(50, 7);
    w.finish().unwrap();
    assert_eq!((w.items(), w.bytes()), (100, 400));
    let text = std::fs::read_to_string(base.with_extension("sigmf-meta")).unwrap();
    let tags = parse_tags(&text).unwrap();
    let keys: Vec<_> = tags.iter().map(|t| (t.offset, &*t.key)).collect();
    assert_eq!(keys, vec![(0, "rx_freq"), (10, "squelch"), (50, "gap")]);
    assert_eq!(w.annotations(), 2);
}

/// Record `data` with `tags` through a SigMF sink (`p` without the path).
fn record_tagged<T: Sample>(
    base: &Path,
    data: Vec<T>,
    tags: Vec<foxygnal_core::Tag>,
    mut p: serde_json::Value,
) {
    let mut fg = Flowgraph::new().with_buffer_capacity(37);
    let s = fg.add("src", VectorSource::with_tags(data, tags));
    p["path"] = json!(path_str(base));
    let k = fg.add("sink", build("sigmf_sink", p));
    fg.connect(s, 0, k, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
}

/// Play a SigMF source through `head(count)`; returns the items, the tags
/// and the configured centre frequency.
fn play_from<T: Sample + Clone>(
    p: serde_json::Value,
    ty: &str,
    count: usize,
) -> (Vec<T>, Vec<foxygnal_core::Tag>, Option<f64>) {
    let mut fg = Flowgraph::new().with_buffer_capacity(37);
    let s = fg.add("src", build("sigmf_source", p));
    let h = fg.add("head", build("head", json!({"type": ty, "count": count})));
    let (sink, out, got) = VectorSink::<T>::with_tags();
    let k = fg.add("sink", sink);
    fg.connect(s, 0, h, 0).unwrap();
    fg.connect(h, 0, k, 0).unwrap();
    let freq = fg.configure().unwrap()[0].meta.center_freq;
    fg.start().unwrap().wait().unwrap();
    let out = out.lock().unwrap().clone();
    let got = got.lock().unwrap().clone();
    (out, got, freq)
}

/// `start` seeks into the recording: earlier tags are dropped except the
/// capture in effect, later ones move back, and a loop restarts there.
#[test]
fn sigmf_source_starts_at_a_sample() {
    use foxygnal_core::{Tag, TagValue};
    let base = scratch("seek");
    let n = 1000usize;
    let data: Vec<Complex32> = (0..n).map(|i| Complex32::new(i as f32, 0.0)).collect();
    record_tagged(
        &base,
        data,
        vec![
            Tag::new(0, "hw", "Simulated FM band (sim:)"),
            Tag::new(0, "rx_freq", 100e6),
            Tag::new(300, "rx_freq", 101.5e6),
            Tag::new(350, "packet", "early"),
            Tag::new(500, "overflow", 42i64),
            Tag::new(700, "packet", "hello"),
        ],
        json!({"type": "c32"}),
    );
    let key = |t: &Tag| (t.offset, t.key.to_string());
    let src = |start: u64, repeat: bool| json!({"type": "c32", "path": path_str(&base), "start": start, "repeat": repeat});

    // Looping from sample 400: 600 samples per pass.
    let (out, got, freq) = play_from::<Complex32>(src(400, true), "c32", 1200);
    assert_eq!(freq, Some(101.5e6), "the capture in effect at the start");
    assert_eq!(out.len(), 1200);
    assert_eq!(out[0].re, 400.0);
    assert_eq!(out[599].re, 999.0);
    assert_eq!(out[600].re, 400.0, "loops back to the start, not to 0");
    let mut want = Vec::new();
    for pass in [0u64, 600] {
        want.extend([
            (pass, "hw".to_owned()),
            (pass, "rx_freq".to_owned()),
            (pass + 100, "overflow".to_owned()),
            (pass + 300, "packet".to_owned()),
        ]);
    }
    assert_eq!(got.iter().map(key).collect::<Vec<_>>(), want);
    assert_eq!(got[1].value, TagValue::Float(101.5e6));
    assert_eq!(got[3].value, TagValue::Str("hello".into()));

    // Right on a capture boundary: its rx_freq once, not twice.
    let (out, got, _) = play_from::<Complex32>(src(300, false), "c32", n);
    assert_eq!(out.len(), 700);
    assert_eq!(
        got.iter().map(key).collect::<Vec<_>>(),
        vec![
            (0, "hw".to_owned()),
            (0, "rx_freq".to_owned()),
            (50, "packet".to_owned()),
            (200, "overflow".to_owned()),
            (400, "packet".to_owned()),
        ]
    );
    assert_eq!(got[1].value, TagValue::Float(101.5e6));

    // Before the first retune: the first capture's frequency.
    let (_, got, freq) = play_from::<Complex32>(src(10, false), "c32", n);
    assert_eq!(freq, Some(100e6));
    assert_eq!(got[1].value, TagValue::Float(100e6));
    assert_eq!(got[2].offset, 290);

    // Past the end is an error that says how long the recording is.
    let mut fg = Flowgraph::new();
    let s = fg.add("src", build("sigmf_source", src(1000, false)));
    let (k, _) = VectorSink::<Complex32>::instance();
    let k = fg.add("k", k);
    fg.connect(s, 0, k, 0).unwrap();
    let e = match fg.start() {
        Ok(run) => run.wait().expect_err("start past the end"),
        Err(e) => e,
    }
    .to_string();
    assert!(e.contains("1000 samples"), "{e}");
}

/// Interleaved ci16 played as raw i16: `start` counts samples (I/Q pairs),
/// so the file offset and the tag offsets are in items, twice that.
#[test]
fn sigmf_source_start_counts_samples_not_items() {
    use foxygnal_core::{Tag, TagValue};
    let base = scratch("seek-ci16");
    // 200 I/Q samples; retune at sample 30, a burst at sample 50.
    let data: Vec<i16> = (0..400).collect();
    record_tagged(
        &base,
        data,
        vec![Tag::new(60, "rx_freq", 7.1e6), Tag::new(100, "burst", true)],
        json!({"type": "i16", "complex": true}),
    );
    let key = |t: &Tag| (t.offset, t.key.to_string());
    let p = json!({"type": "i16", "path": path_str(&base), "start": 40, "repeat": true});
    // 160 samples = 320 items per pass.
    let (out, got, freq) = play_from::<i16>(p, "i16", 640);
    assert_eq!(freq, Some(7.1e6));
    assert_eq!(&out[..4], &[80, 81, 82, 83]);
    assert_eq!(out[319], 399);
    assert_eq!(&out[320..322], &[80, 81]);
    assert_eq!(
        got.iter().map(key).collect::<Vec<_>>(),
        vec![
            (0, "rx_freq".to_owned()),
            (20, "burst".to_owned()),
            (320, "rx_freq".to_owned()),
            (340, "burst".to_owned()),
        ]
    );
    assert_eq!(got[0].value, TagValue::Float(7.1e6));
    // As c32, the same start gives sample offsets.
    let p = json!({"type": "c32", "path": path_str(&base), "start": 40});
    let (out, got, _) = play_from::<Complex32>(p, "c32", 1000);
    assert_eq!(out.len(), 160);
    assert_eq!(out[0], Complex32::new(80.0 / 32768.0, 81.0 / 32768.0));
    assert_eq!(
        got.iter().map(key).collect::<Vec<_>>(),
        vec![(0, "rx_freq".to_owned()), (10, "burst".to_owned())]
    );
}
