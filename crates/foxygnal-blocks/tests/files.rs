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
