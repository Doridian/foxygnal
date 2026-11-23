//! Export round trips: every example graph and every GRC import fixture is
//! exported, the YAML checked, imported again and compared with the
//! original, modulo the lossy mappings documented in docs/GRC_IMPORT.md.

use std::collections::HashMap;
use std::path::PathBuf;

use foxygnal_core::graph::BlockSpec;
use foxygnal_core::{DirLoader, GraphLoader, GraphSpec, NoLoader, Registry, resolve};
use foxygnal_grc::{Export, ExportOptions, Options, export, import};
use serde_json::{Value, json};
use yaml_rust2::{Yaml, YamlLoader};

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// With `FOXYGNAL_GRC_DUMP=<dir>`, exported files are written there, for
/// `scripts/grc-check.py` or `grcc`.
fn dump(name: &str, grc: &str) {
    if let Some(dir) = std::env::var_os("FOXYGNAL_GRC_DUMP") {
        std::fs::write(PathBuf::from(dir).join(format!("{name}.grc")), grc).unwrap();
    }
}

/// The file must be GRC 3.10 YAML: options, blocks with name / id /
/// parameters / states, connections as [src, port, dst, port], format 1.
fn check_yaml(text: &str) {
    let docs = YamlLoader::load_from_str(text).expect("valid YAML");
    let doc = &docs[0];
    let opts = &doc["options"]["parameters"];
    assert!(opts["id"].as_str().is_some(), "options.id");
    assert_eq!(opts["generate_options"].as_str(), Some("no_gui"));
    assert_eq!(doc["metadata"]["file_format"].as_i64(), Some(1));
    let mut names = Vec::new();
    for b in doc["blocks"].as_vec().expect("blocks") {
        let name = b["name"].as_str().expect("name");
        assert!(
            name.starts_with(|c: char| c.is_ascii_alphabetic())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "{name}"
        );
        assert!(!names.contains(&name), "duplicate {name}");
        names.push(name);
        assert!(b["id"].as_str().is_some(), "{name}: id");
        assert!(b["parameters"].as_hash().is_some(), "{name}: parameters");
        for (_, v) in b["parameters"].as_hash().unwrap() {
            assert!(
                matches!(v, Yaml::String(_)),
                "{name}: parameters are strings"
            );
        }
        let st = &b["states"];
        let c = st["coordinate"].as_vec().expect("coordinate");
        assert!(c.len() == 2 && c.iter().all(|v| v.as_i64().is_some()));
        assert_eq!(st["rotation"].as_i64(), Some(0));
        assert_eq!(st["state"].as_str(), Some("enabled"));
    }
    for c in doc["connections"].as_vec().expect("connections") {
        let c = c.as_vec().expect("a list");
        assert_eq!(c.len(), 4);
        let (src, sp, dst, dp) = (&c[0], &c[1], &c[2], &c[3]);
        assert!(names.contains(&src.as_str().unwrap()), "{src:?}");
        assert!(names.contains(&dst.as_str().unwrap()), "{dst:?}");
        assert!(sp.as_str().unwrap().parse::<usize>().is_ok());
        assert!(dp.as_str().unwrap().parse::<usize>().is_ok());
    }
}

/// Parameters that don't survive a round trip (docs/GRC_IMPORT.md, Export).
const LOSSY: &[(&str, &str)] = &[
    ("file_source", "sample_rate"),
    ("file_source", "center_freq"),
    ("udp_source", "sample_rate"),
    ("udp_source", "host"),
    ("udp_source", "interface"),
    ("tcp_source", "sample_rate"),
    ("tcp_source", "reconnect"),
    ("tcp_source", "retry"),
    ("tcp_sink", "reconnect"),
    ("tcp_sink", "retry"),
    ("tcp_sink", "backpressure"),
    ("tcp_sink", "queue"),
    ("tcp_sink", "max_clients"),
    ("throttle", "sample_rate"),
    ("browser_audio", "pace"),
    ("rational_resampler", "output_rate"),
    ("rational_resampler", "interpolation"),
    ("rational_resampler", "decimation"),
    ("rational_resampler", "transition"),
    ("fir_filter", "attenuation"),
    ("freq_xlating_fir", "attenuation"),
    ("hilbert", "attenuation"),
    ("agc", "attack"),
    ("agc", "decay"),
    ("squelch", "hysteresis"),
    ("squelch", "averaging"),
    ("squelch", "ramp"),
    ("dc_blocker", "cutoff"),
    ("am_demod", "dc_time"),
    ("valve", "when_closed"),
    ("binary_slicer", "threshold"),
    ("log_power", "floor"),
    ("log_power", "type"),
    ("log_power", "offset"),
    ("audio_source", "latency"),
    ("audio_sink", "latency"),
];

/// Kinds that come back as another block.
const KIND_CHANGES: &[(&str, &str)] = &[
    ("decimator", "rational_resampler"),
    ("interpolator", "rational_resampler"),
    // Complex to Float comes back as one `c32_to_f32` per used output.
    ("c32_to_f32_pair", "c32_to_f32"),
];

/// Kinds whose GNU Radio form has more blocks, so their wires come back
/// through extra nodes.
const RESHAPED: &[&str] = &["log_power", "binary_slicer", "costas_loop", "cu8_to_c32"];

fn defaults(reg: &Registry, kind: &str) -> HashMap<String, Value> {
    reg.get(kind)
        .map(|d| {
            d.params
                .iter()
                .map(|p| (p.name.to_owned(), p.default.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => (x - y).abs() <= 1e-6 * x.abs().max(y.abs()).max(1e-9),
        _ => a == b,
    }
}

/// Export, check, import again; compare the resolved graphs.
fn round_trip(spec: &GraphSpec, loader: &dyn GraphLoader, name: &str) -> (Export, GraphSpec) {
    let reg = foxygnal_blocks::registry();
    let opts = ExportOptions {
        loader: Some(loader),
        name: Some(name.into()),
    };
    let exp = export(spec, &opts).unwrap_or_else(|e| panic!("{name}: {e}"));
    check_yaml(&exp.grc);
    dump(name, &exp.grc);
    let back = import(&exp.grc, &Options::default())
        .unwrap_or_else(|e| panic!("{name}: re-import: {e}\n{}", exp.grc));

    let orig = resolve(spec, &reg, loader)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .graph;
    let again = resolve(&back.graph, &reg, &NoLoader)
        .unwrap_or_else(|e| {
            panic!(
                "{name}: re-imported graph: {e}\n{}",
                back.graph.to_toml().unwrap()
            )
        })
        .graph;
    let ids: HashMap<&str, &str> = exp
        .ids
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let find = |id: &str| -> Option<&BlockSpec> {
        let n = ids.get(id).copied().unwrap_or(id);
        again.blocks.iter().find(|b| b.id == n)
    };
    let ctx = || {
        format!(
            "{name}\n{}\n{}",
            exp.warnings.join("\n"),
            back.graph.to_toml().unwrap()
        )
    };
    let mut exact = HashMap::new();
    for b in &orig.blocks {
        let Some(r) = find(&b.id) else {
            panic!(
                "{name}: `{}` ({}) is missing after the round trip\n{}",
                b.id,
                b.kind,
                ctx()
            );
        };
        if r.kind != b.kind {
            assert!(
                KIND_CHANGES.contains(&(b.kind.as_str(), r.kind.as_str())),
                "{name}: `{}` came back as {} instead of {}\n{}",
                b.id,
                r.kind,
                b.kind,
                ctx()
            );
            continue;
        }
        exact.insert(b.id.as_str(), !RESHAPED.contains(&b.kind.as_str()));
        let d = defaults(&reg, &b.kind);
        let get = |p: &BlockSpec, k: &str| p.params.get(k).or_else(|| d.get(k)).cloned();
        let mut keys: Vec<&String> = b.params.keys().chain(r.params.keys()).collect();
        keys.sort();
        keys.dedup();
        for k in keys {
            if LOSSY.contains(&(b.kind.as_str(), k.as_str())) {
                continue;
            }
            let (x, y) = (get(b, k), get(r, k));
            match (&x, &y) {
                (Some(x), Some(y)) if same(x, y) => {}
                // Not a parameter of the block (ignored by it either way).
                (None, _) | (_, None) if d.is_empty() || !d.contains_key(k) => {}
                _ => panic!(
                    "{name}: `{}` ({}) parameter `{k}`: {x:?} became {y:?}\n{}",
                    b.id,
                    b.kind,
                    ctx()
                ),
            }
        }
    }
    // Wires between blocks that came back as they were.
    for c in &orig.connections {
        let (f, t) = c.endpoints().unwrap();
        if !(exact.get(f.block) == Some(&true) && exact.get(t.block) == Some(&true)) {
            continue;
        }
        // GNU Radio can't drive a parameter from a stream: a wired setting
        // keeps its starting value (and the export warns).
        if t.port.starts_with('@') {
            assert!(
                exp.warnings.iter().any(|w| w.contains(&c.to)),
                "{name}: no warning for the wired setting {}",
                c.to
            );
            continue;
        }
        let (fb, tb) = (find(f.block).unwrap(), find(t.block).unwrap());
        let found = again.connections.iter().any(|x| {
            let (xf, xt) = x.endpoints().unwrap();
            xf.block == fb.id && xt.block == tb.id && xf.port == f.port && xt.port == t.port
        });
        // A stereo audio sink or WAV sink comes back through converters.
        let via = matches!(tb.kind.as_str(), "browser_audio" | "wav_sink")
            && tb.params.get("type") == Some(&json!("c32"));
        assert!(
            found || via,
            "{name}: connection {} → {} is missing\n{}",
            c.from,
            c.to,
            ctx()
        );
    }
    (exp, back.graph)
}

#[test]
fn examples_round_trip() {
    let dir = examples();
    let mut n = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let Some(stem) = file.strip_suffix(".fox.toml") else {
            continue;
        };
        let spec = GraphSpec::from_toml(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let (exp, _) = round_trip(&spec, &DirLoader(dir.clone()), stem);
        // Never dropped silently: every block is accounted for.
        assert_eq!(
            exp.ids.len(),
            resolve(&spec, &foxygnal_blocks::registry(), &DirLoader(dir.clone()))
                .unwrap()
                .graph
                .blocks
                .len()
        );
        n += 1;
    }
    assert!(n >= 8, "found {n} examples");
}

#[test]
fn import_fixtures_round_trip() {
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let imp = import(
            &std::fs::read_to_string(&path).unwrap(),
            &Options::default(),
        )
        .unwrap();
        round_trip(&imp.graph, &NoLoader, &name);
    }
}

fn blk(id: &str, kind: &str, params: Value) -> BlockSpec {
    BlockSpec {
        id: id.into(),
        kind: kind.into(),
        params: params.as_object().cloned().unwrap_or_default(),
        const_inputs: Default::default(),
        ui: Default::default(),
    }
}

fn graph(vars: Value, blocks: Vec<BlockSpec>, conns: &[(&str, &str)]) -> GraphSpec {
    let mut g = GraphSpec {
        vars: vars.as_object().cloned().unwrap_or_default(),
        ..Default::default()
    };
    for (k, b) in blocks.into_iter().enumerate() {
        let mut b = b;
        b.ui.insert("x".into(), json!(50 + 300 * (k % 5)));
        b.ui.insert("y".into(), json!(60 + 180 * (k / 5)));
        g.blocks.push(b);
    }
    for (f, t) in conns {
        g.connections.push(foxygnal_core::graph::ConnectionSpec {
            from: (*f).into(),
            to: (*t).into(),
        });
    }
    g
}

fn warned(e: &Export, needles: &[&str]) -> bool {
    e.warnings
        .iter()
        .any(|w| needles.iter().all(|n| w.contains(n)))
}

/// Most of the catalog in one graph, with parameters off their defaults.
#[test]
fn catalog_round_trip() {
    let g = graph(
        json!({ "samp_rate": "2.4M", "decim": 10, "dev": "75k" }),
        vec![
            blk(
                "src",
                "sdr_source",
                json!({ "device": "rtlsdr:index=1,bias_tee=1", "frequency": "433.92M", "sample_rate": "$samp_rate", "gain": 20.0, "ppm": 3.0, "agc": true }),
            ),
            blk("dc", "dc_blocker", json!({ "type": "c32", "cutoff": 50.0 })),
            blk(
                "xl",
                "freq_xlating_fir",
                json!({ "offset": "-100k", "cutoff": "80k", "transition": "20k", "decimation": "$decim", "window": "blackman" }),
            ),
            blk(
                "lp",
                "fir_filter",
                json!({ "type": "c32", "mode": "band_pass", "cutoff": 1000.0, "cutoff_high": 30000.0, "transition": 2000.0, "window": "kaiser", "attenuation": 70.0, "gain": 2.0 }),
            ),
            blk(
                "agc",
                "agc",
                json!({ "type": "c32", "target": 0.5, "max_gain": 1000.0 }),
            ),
            blk(
                "sq",
                "squelch",
                json!({ "type": "c32", "threshold": -50.0 }),
            ),
            blk(
                "fm",
                "fm_demod",
                json!({ "deviation": "$dev / 15", "deemphasis": "75us" }),
            ),
            blk(
                "rs",
                "rational_resampler",
                json!({ "type": "f32", "output_rate": "48k" }),
            ),
            blk("vol", "gain", json!({ "type": "f32", "gain": 0.7 })),
            blk(
                "bias",
                "add_const",
                json!({ "type": "f32", "constant": 0.1 }),
            ),
            blk(
                "wav",
                "wav_sink",
                json!({ "type": "f32", "path": "out.wav", "format": "pcm16" }),
            ),
            blk("mag", "c32_to_f32", json!({ "part": "magnitude_squared" })),
            blk("hd", "head", json!({ "type": "f32", "count": 1000 })),
            blk("sk", "skip", json!({ "type": "f32", "count": 10 })),
            blk("dl", "delay", json!({ "type": "f32", "delay": 5 })),
            blk("vl", "valve", json!({ "type": "f32", "open": false })),
            blk(
                "fs",
                "file_sink",
                json!({ "type": "f32", "path": "/tmp/x.f32" }),
            ),
            blk(
                "costas",
                "costas_loop",
                json!({ "order": "4", "loop_bw": 0.05 }),
            ),
            blk(
                "cr",
                "clock_recovery",
                json!({ "type": "c32", "sps": 4.0, "loop_bw": 0.02, "ted": "mueller_muller" }),
            ),
            blk("re", "c32_to_f32", json!({ "part": "real" })),
            blk("sl", "binary_slicer", json!({})),
            blk(
                "cac",
                "correlate_access_code",
                json!({ "access_code": "0x1ACFFC1D", "threshold": 3 }),
            ),
            blk("bits", "null_sink", json!({ "type": "u8" })),
            blk("hz", "null_sink", json!({ "type": "f32" })),
            blk(
                "tone",
                "signal_source",
                json!({ "type": "f32", "waveform": "square", "frequency": 440.0, "amplitude": 0.5, "offset": 0.1, "sample_rate": 8000.0 }),
            ),
            blk(
                "thr",
                "throttle",
                json!({ "type": "f32", "sample_rate": 8000.0 }),
            ),
            blk(
                "sel",
                "selector",
                json!({ "type": "f32", "inputs": 3, "input": 1 }),
            ),
            blk(
                "noise",
                "noise_source",
                json!({ "type": "f32", "amplitude": 0.3, "seed": 7 }),
            ),
            blk("conj", "f32_to_c32", json!({})),
            blk("cj", "conjugate", json!({})),
            blk("pair", "c32_to_f32_pair", json!({})),
            blk("ab", "abs", json!({})),
            blk("mix", "multiply", json!({ "type": "f32" })),
            blk(
                "hil",
                "hilbert",
                json!({ "taps": 33, "window": "blackman" }),
            ),
            blk(
                "udp",
                "udp_sink",
                json!({ "type": "c32", "host": "10.0.0.2", "port": 5000, "header": "seq64", "payload": 1000 }),
            ),
            blk("i16", "f32_to_i16", json!({ "scale": 2.0 })),
            blk(
                "tcp",
                "tcp_sink",
                json!({ "type": "i16", "mode": "client", "host": "10.0.0.3", "port": 7000 }),
            ),
            blk(
                "rrc",
                "rrc_filter",
                json!({ "type": "c32", "mode": "filter", "sps": 4.0, "rolloff": 0.25, "span": 8 }),
            ),
            blk("rrcn", "null_sink", json!({ "type": "c32" })),
        ],
        &[
            ("src.out", "dc.in"),
            ("dc.out", "xl.in"),
            ("xl.out", "lp.in"),
            ("lp.out", "agc.in"),
            ("agc.out", "sq.in"),
            ("sq.out", "fm.in"),
            ("fm.out", "rs.in"),
            ("rs.out", "vol.in"),
            ("vol.out", "bias.in"),
            ("bias.out", "wav.in"),
            ("xl.out", "mag.in"),
            ("mag.out", "hd.in"),
            ("hd.out", "sk.in"),
            ("sk.out", "dl.in"),
            ("dl.out", "vl.in"),
            ("vl.out", "fs.in"),
            ("xl.out", "costas.in"),
            ("costas.out", "cr.in"),
            ("costas.freq", "hz.in"),
            ("cr.out", "re.in"),
            ("re.out", "sl.in"),
            ("sl.out", "cac.in"),
            ("cac.out", "bits.in"),
            ("tone.out", "thr.in"),
            ("thr.out", "sel.in0"),
            ("noise.out", "sel.in1"),
            ("noise.out", "mix.a"),
            ("tone.out", "mix.b"),
            ("mix.out", "sel.in2"),
            ("sel.out", "conj.in"),
            ("conj.out", "cj.in"),
            ("cj.out", "pair.in"),
            ("pair.re", "ab.in"),
            ("ab.out", "hil.in"),
            ("hil.out", "udp.in"),
            ("pair.im", "i16.in"),
            ("i16.out", "tcp.in"),
            ("xl.out", "rrc.in"),
            ("rrc.out", "rrcn.in"),
        ],
    );
    let (exp, back) = round_trip(&g, &NoLoader, "catalog");
    let grc = &exp.grc;
    // Spot checks of the GNU Radio side.
    assert!(grc.contains("args: \"'rtl=1,bias=1'\""), "{grc}");
    assert!(grc.contains("firdes.low_pass(1, samp_rate, 80000, 20000, window.WIN_BLACKMAN, 6.76)"));
    assert!(
        grc.contains("gain: samp_rate / int(decim) / (2 * math.pi * dev / 15)"),
        "{grc}"
    );
    assert!(grc.contains("access_code: \"'00011010110011111111110000011101'\""));
    assert!(grc.contains("id: import_"));
    assert!(grc.contains("imports: import math"), "{grc}");
    // 2.4 MS/s ÷ 10 → 48 kS/s.
    assert!(grc.contains("decim: \"5\""));
    assert!(warned(&exp, &["`dc`", "length of 20160"]));
    assert!(warned(&exp, &["`agc`", "per-sample"]));
    // The costas frequency output is scaled to Hz in GNU Radio.
    assert!(
        grc.contains("const: samp_rate / int(decim) / (2 * math.pi)"),
        "{grc}"
    );
    let tone = back.blocks.iter().find(|b| b.id == "tone").unwrap();
    assert_eq!(tone.params["amplitude"], json!(0.5));
    assert!((tone.params["offset"].as_f64().unwrap() - 0.1).abs() < 1e-12);
}

#[test]
fn placeholders_keep_the_file_loadable_and_come_back() {
    let g = graph(
        json!({ "rate": "2M" }),
        vec![
            blk(
                "sdr",
                "sdr_source",
                json!({ "device": "soapy:driver=plutosdr,uri=ip:192.168.2.1", "sample_rate": "$rate", "frequency": "1090M" }),
            ),
            blk("mag", "c32_to_f32", json!({ "part": "magnitude" })),
            blk("adsb", "adsb_decoder", json!({ "fix_errors": false })),
            blk("log", "message_log", json!({ "stdout": true })),
            blk(
                "sp",
                "my_plugin_block",
                json!({ "gain": "$rate / 4", "name": "x" }),
            ),
            blk(
                "bq",
                "biquad",
                json!({ "type": "f32", "shape": "notch", "freq": 1000.0, "q": 5.0, "stages": 2 }),
            ),
            blk("out", "null_sink", json!({ "type": "f32" })),
            blk(
                "rec",
                "sigmf_sink",
                json!({ "type": "c32", "path": "capture.sigmf-meta" }),
            ),
        ],
        &[
            ("sdr.out", "mag.in"),
            ("mag.out", "adsb.in"),
            ("adsb.out", "log.in"),
            ("mag.out", "sp.in"),
            ("sp.out", "bq.in"),
            ("bq.out", "out.in"),
            ("sdr.out", "rec.in"),
        ],
    );
    let exp = export(&g, &ExportOptions::default()).unwrap();
    check_yaml(&exp.grc);
    dump("placeholders", &exp.grc);
    let grc = &exp.grc;
    assert!(grc.contains("id: soapy_plutosdr_source"));
    assert!(grc.contains("dev_args: \"'uri=ip:192.168.2.1'\""), "{grc}");
    assert!(grc.contains("id: epy_block"));
    assert!(grc.contains("in_sig=[numpy.float32]"));
    assert!(grc.contains("out_sig=[numpy.uint8]"));
    assert!(grc.contains("file: \"'capture.sigmf-data'\""));
    assert!(grc.contains("id: iir_filter_xxx"));
    // The plugin's input type comes from what feeds it; its output from
    // its `type`-less default (complex) is unknown, so float from the biquad.
    assert!(warned(&exp, &["`adsb` (adsb_decoder)", "placeholder"]));
    assert!(warned(&exp, &["`sp` (my_plugin_block)", "placeholder"]));
    assert!(warned(&exp, &["`rec` (sigmf_sink)", "gr-sigmf"]));
    assert!(warned(&exp, &["`bq` (biquad)", "IIR"]));
    // The importer brings the placeholders back as they were.
    let back = import(grc, &Options::default()).unwrap();
    let get = |id: &str| back.graph.blocks.iter().find(|b| b.id == id).unwrap();
    assert_eq!(get("adsb").kind, "adsb_decoder");
    assert_eq!(get("adsb").params["fix_errors"], json!(false));
    assert_eq!(get("log").params["stdout"], json!(true));
    assert_eq!(get("sp").kind, "my_plugin_block");
    assert_eq!(get("sp").params["gain"], json!("$rate / 4"));
    assert_eq!(get("sp").params["name"], json!("x"));
    assert_eq!(
        get("sdr").params["device"],
        json!("soapy:driver=plutosdr,uri=ip:192.168.2.1")
    );
    let has = |f: &str, t: &str| {
        back.graph
            .connections
            .iter()
            .any(|c| c.from == f && c.to == t)
    };
    assert!(has("mag.out", "adsb.in"));
    assert!(has("adsb.out", "log.in"));
    assert!(has("mag.out", "sp.in"));
}

#[test]
fn hierarchical_blocks_are_flattened() {
    let spec = GraphSpec::from_toml(
        &std::fs::read_to_string(examples().join("hier-tones.fox.toml")).unwrap(),
    )
    .unwrap();
    let exp = export(
        &spec,
        &ExportOptions {
            loader: Some(&DirLoader(examples())),
            name: Some("hier-tones".into()),
        },
    )
    .unwrap();
    assert!(warned(&exp, &["`low`", "flattened"]));
    // The block's parameters become variables, expressions kept.
    assert!(exp.grc.contains("name: low_freq"));
    assert!(exp.grc.contains("value: \"-base * 3\""), "{}", exp.grc);
    assert!(exp.grc.contains("freq: high_freq"));
    // Without a loader the sub-graph is a placeholder, not dropped.
    let exp = export(&spec, &ExportOptions::default()).unwrap();
    assert!(warned(&exp, &["`low`", "can't be opened"]));
    assert!(exp.grc.contains("graph:tone-and-noise.fox.toml"));
}
