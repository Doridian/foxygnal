//! `.grc` fixtures and small hand-written flowgraphs through the importer:
//! the graphs must resolve against the real block registry, and the ones
//! without hardware (or with the SDR swapped for `sim:`) must run.

use std::time::Duration;

use foxygnal_core::graph::BlockSpec;
use foxygnal_core::{Flowgraph, GraphSpec, NoLoader, resolve};
use foxygnal_grc::{Import, Options, import};
use serde_json::{Value, json};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn imp(text: &str) -> Import {
    import(text, &Options::default()).expect("imports")
}

fn block<'a>(g: &'a GraphSpec, id: &str) -> &'a BlockSpec {
    g.blocks
        .iter()
        .find(|b| b.id == id)
        .unwrap_or_else(|| panic!("no block `{id}` in {:#?}", g.blocks))
}

fn has_edge(g: &GraphSpec, from: &str, to: &str) -> bool {
    g.connections.iter().any(|c| c.from == from && c.to == to)
}

fn warned(i: &Import, needles: &[&str]) -> bool {
    i.warnings
        .iter()
        .any(|w| needles.iter().all(|n| w.contains(n)))
}

/// Resolve against the real registry: every kind exists, every param
/// evaluates, every var resolves.
fn resolves(g: &GraphSpec) -> foxygnal_core::Resolved {
    let reg = foxygnal_blocks::registry();
    let r = resolve(g, &reg, &NoLoader).unwrap_or_else(|e| panic!("{e}\n{}", g.to_toml().unwrap()));
    // Building also checks ports and connection types.
    Flowgraph::from_resolved(&r.graph, &reg)
        .unwrap_or_else(|e| panic!("{e}\n{}", g.to_toml().unwrap()));
    r
}

/// Run for a moment, then stop; the run must not fail.
fn run_briefly(g: &GraphSpec) {
    let reg = foxygnal_blocks::registry();
    let r = resolve(g, &reg, &NoLoader).unwrap();
    let running = Flowgraph::from_resolved(&r.graph, &reg)
        .unwrap()
        .start()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    running.stop_handle().stop();
    running.wait().unwrap();
}

fn var(r: &foxygnal_core::Resolved, name: &str) -> f64 {
    r.vars
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no var {name}"))
        .1
}

#[test]
fn fm_receiver() {
    let i = imp(&fixture("fm_receiver.grc"));
    let g = &i.graph;
    assert_eq!(i.title.as_deref(), Some("FM receiver"));

    // Variables, with their references kept.
    assert_eq!(g.vars["samp_rate"], json!(2400000));
    assert_eq!(g.vars["quad_rate"], json!("$samp_rate / $decim"));
    assert_eq!(g.vars["freq"], json!(100300000)); // a qtgui_range
    assert!(!g.vars.contains_key("channel_taps"));

    let sdr = block(g, "osmosdr_source_0");
    assert_eq!(sdr.kind, "sdr_source");
    assert_eq!(sdr.params["device"], json!("rtlsdr:"));
    assert_eq!(sdr.params["frequency"], json!("$freq - $offset"));
    assert_eq!(sdr.params["sample_rate"], json!("$samp_rate"));
    assert_eq!(sdr.params["gain"], json!(30.0));
    assert!(warned(&i, &["osmosdr_source_0", "rtlsdr:"]));

    let xl = block(g, "freq_xlating_fir_filter_xxx_0");
    assert_eq!(xl.kind, "freq_xlating_fir");
    assert_eq!(xl.params["offset"], json!("$offset"));
    assert_eq!(xl.params["decimation"], json!("$decim"));
    // Read out of firdes.low_pass(…) through the `channel_taps` variable.
    assert_eq!(xl.params["cutoff"], json!(100000.0));
    assert_eq!(xl.params["transition"], json!(40000.0));
    assert_eq!(xl.params["window"], json!("hamming"));

    let demod = block(g, "analog_quadrature_demod_cf_0");
    assert_eq!(demod.kind, "fm_demod");
    // gain = quad_rate/(2*math.pi*fm_dev) → deviation = fm_dev.
    assert_eq!(demod.params["deviation"], json!("$fm_dev"));
    assert_eq!(demod.params["deemphasis"], json!("off"));

    let rs = block(g, "rational_resampler_xxx_0");
    assert_eq!(rs.params["type"], json!("f32"));
    assert_eq!(rs.params["output_rate"], json!(0.0));
    assert_eq!(rs.params["interpolation"], json!("$audio_rate / 1000"));

    assert_eq!(
        block(g, "blocks_multiply_const_vxx_0").params["gain"],
        json!("$volume")
    );
    let audio = block(g, "audio_sink_0");
    assert_eq!(audio.kind, "browser_audio");
    assert_eq!(audio.params["pace"], json!(false)); // the SDR paces

    // The instrument became a null sink, with a note.
    assert_eq!(block(g, "qtgui_freq_sink_x_0").kind, "null_sink");
    assert!(warned(&i, &["qtgui_freq_sink_x_0", "Spectrum"]));

    // Positions from GRC coordinates.
    assert_eq!(sdr.ui["x"], json!(60));
    assert_eq!(sdr.ui["y"], json!(245));

    assert!(has_edge(
        g,
        "osmosdr_source_0.out",
        "freq_xlating_fir_filter_xxx_0.in"
    ));
    assert!(has_edge(
        g,
        "osmosdr_source_0.out",
        "qtgui_freq_sink_x_0.in"
    ));
    assert!(has_edge(
        g,
        "blocks_multiply_const_vxx_0.out",
        "audio_sink_0.in"
    ));
    assert_eq!(g.connections.len(), 6);

    let r = resolves(g);
    assert_eq!(var(&r, "decim"), 10.0);
    assert_eq!(var(&r, "quad_rate"), 240000.0);

    // Swap the RTL-SDR for the simulator and run it.
    let mut sim = g.clone();
    for b in &mut sim.blocks {
        if b.kind == "sdr_source" {
            b.params.insert("device".into(), json!("sim:"));
        }
    }
    run_briefly(&sim);

    // The sound card, when asked.
    let dev = import(&fixture("fm_receiver.grc"), &Options { audio_device: true }).unwrap();
    assert_eq!(block(&dev.graph, "audio_sink_0").kind, "audio_sink");
}

#[test]
fn tone_scope() {
    let i = imp(&fixture("tone_scope.grc"));
    let g = &i.graph;
    let src = block(g, "analog_sig_source_x_0");
    assert_eq!(src.kind, "signal_source");
    assert_eq!(src.params["type"], json!("c32"));
    assert_eq!(src.params["waveform"], json!("sine"));
    assert_eq!(src.params["frequency"], json!("$tone"));
    assert_eq!(src.params["sample_rate"], json!("$samp_rate"));
    let th = block(g, "blocks_throttle2_0");
    assert_eq!(th.kind, "throttle");
    assert_eq!(th.params["sample_rate"], json!("$samp_rate"));
    assert_eq!(block(g, "qtgui_time_sink_x_0").kind, "null_sink");
    assert!(warned(&i, &["qtgui_time_sink_x_0", "Scope"]));
    assert_eq!(i.warnings.len(), 1, "{:#?}", i.warnings);
    resolves(g);
    run_briefly(g);
}

#[test]
fn unknown_and_disabled_blocks() {
    let i = imp(&fixture("unknown_blocks.grc"));
    let g = &i.graph;

    // Unknown blocks are named, with their GNU Radio ids.
    for (name, id) in [
        ("epy_block_0", "epy_block"),
        ("blocks_message_strobe_0", "blocks_message_strobe"),
        ("blocks_message_debug_0", "blocks_message_debug"),
    ] {
        assert!(
            warned(&i, &[name, id, "no foxygnal equivalent"]),
            "{name}: {:#?}",
            i.warnings
        );
        assert!(g.blocks.iter().all(|b| b.id != name));
    }
    assert!(warned(&i, &["message connection", "strobe"]));

    // Disabled: gone, silently (GRC ignores them too).
    assert!(g.blocks.iter().all(|b| b.id != "blocks_file_sink_0"));
    assert!(!g.vars.contains_key("unused_gain"));
    assert!(i.warnings.iter().all(|w| !w.contains("blocks_file_sink_0")));

    // Bypassed filter wired through; the virtual sink/source pair too.
    assert!(g.blocks.iter().all(|b| b.id != "low_pass_filter_0"));
    assert!(has_edge(g, "blocks_add_xx_0.out", "blocks_throttle_0.in"));
    assert!(has_edge(
        g,
        "analog_sig_source_x_0.out",
        "blocks_add_xx_0.a"
    ));
    assert!(has_edge(
        g,
        "analog_noise_source_x_0.out",
        "blocks_add_xx_0.b"
    ));

    // `samp_rate // 10` is exact here, so it stays an expression.
    assert_eq!(
        block(g, "blocks_head_0").params["count"],
        json!("$duration")
    );
    assert_eq!(g.vars["duration"], json!("$samp_rate / 10"));
    // A string variable is inlined.
    let wav = block(g, "blocks_wavfile_sink_0");
    assert_eq!(wav.params["path"], json!("/tmp/unknown_blocks.wav"));
    assert_eq!(wav.params["format"], json!("pcm16"));

    resolves(g);

    // It runs to the end (head stops it) and writes the WAV file.
    let dir = std::env::temp_dir().join(format!("foxygnal-grc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("out.wav");
    let mut g = g.clone();
    for b in &mut g.blocks {
        if b.kind == "wav_sink" {
            b.params.insert("path".into(), json!(out.to_str().unwrap()));
        }
    }
    let reg = foxygnal_blocks::registry();
    let r = resolve(&g, &reg, &NoLoader).unwrap();
    Flowgraph::from_resolved(&r.graph, &reg)
        .unwrap()
        .start()
        .unwrap()
        .wait()
        .unwrap();
    let len = std::fs::metadata(&out).unwrap().len();
    assert!(len >= 4800 * 2, "{len} bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A minimal GRC file from `(name, id, params, state)` blocks and connections.
/// `(name, GNU Radio id, params, state)`.
type Blk<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], &'a str);

fn grc(blocks: &[Blk<'_>], conns: &[[&str; 4]]) -> String {
    let mut s = String::from(
        "options:\n  parameters:\n    id: t\n    title: t\n    generate_options: no_gui\n  states:\n    coordinate: [8, 8]\n    rotation: 0\n    state: enabled\nblocks:\n",
    );
    for (i, (name, id, params, state)) in blocks.iter().enumerate() {
        s.push_str(&format!(
            "- name: {name}\n  id: {id}\n  parameters:\n    comment: ''\n"
        ));
        for (k, v) in *params {
            s.push_str(&format!("    {k}: {}\n", yaml_quote(v)));
        }
        s.push_str(&format!(
            "  states:\n    coordinate: [{}, 100]\n    rotation: 0\n    state: {state}\n",
            i * 200
        ));
    }
    s.push_str("connections:\n");
    for c in conns {
        s.push_str(&format!("- [{}, '{}', {}, '{}']\n", c[0], c[1], c[2], c[3]));
    }
    s.push_str("metadata:\n  file_format: 1\n");
    s
}

fn yaml_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

const EN: &str = "enabled";

#[test]
fn converters_and_multi_input_math() {
    let text = grc(
        &[
            ("samp_rate", "variable", &[("value", "48000")], EN),
            (
                "src",
                "analog_sig_source_x",
                &[
                    ("type", "complex"),
                    ("samp_rate", "samp_rate"),
                    ("freq", "1000"),
                    ("amp", "1"),
                    ("waveform", "analog.GR_COS_WAVE"),
                ],
                EN,
            ),
            ("c2f", "blocks_complex_to_float", &[], EN),
            ("mag", "blocks_complex_to_mag_squared", &[], EN),
            (
                "add",
                "blocks_add_xx",
                &[("type", "float"), ("num_inputs", "3")],
                EN,
            ),
            ("f2c", "blocks_float_to_complex", &[], EN),
            (
                "th",
                "blocks_throttle",
                &[("type", "complex"), ("samples_per_second", "samp_rate")],
                EN,
            ),
            (
                "head",
                "blocks_head",
                &[("type", "complex"), ("num_items", "4800")],
                EN,
            ),
            (
                "sink",
                "blocks_null_sink",
                &[("type", "complex"), ("num_inputs", "1")],
                EN,
            ),
        ],
        &[
            ["src", "0", "c2f", "0"],
            ["src", "0", "mag", "0"],
            ["c2f", "0", "add", "0"],
            ["c2f", "1", "add", "1"],
            ["mag", "0", "add", "2"],
            ["add", "0", "f2c", "0"],
            ["c2f", "1", "f2c", "1"],
            ["f2c", "0", "th", "0"],
            ["th", "0", "head", "0"],
            ["head", "0", "sink", "0"],
        ],
    );
    let i = imp(&text);
    let g = &i.graph;
    assert!(i.warnings.is_empty(), "{:#?}", i.warnings);
    assert_eq!(block(g, "c2f").params["part"], json!("real"));
    assert_eq!(block(g, "c2f_im").params["part"], json!("imag"));
    assert_eq!(block(g, "mag").params["part"], json!("magnitude_squared"));
    // Three inputs: two adds in a chain.
    assert!(has_edge(g, "c2f.out", "add.a"));
    assert!(has_edge(g, "c2f_im.out", "add.b"));
    assert!(has_edge(g, "add.out", "add_1.a"));
    assert!(has_edge(g, "mag.out", "add_1.b"));
    assert!(has_edge(g, "add_1.out", "f2c.re"));
    assert!(has_edge(g, "c2f_im.out", "f2c.im"));
    assert_eq!(block(g, "f2c").kind, "f32_pair_to_c32");
    resolves(g);
    let reg = foxygnal_blocks::registry();
    let r = resolve(g, &reg, &NoLoader).unwrap();
    Flowgraph::from_resolved(&r.graph, &reg)
        .unwrap()
        .start()
        .unwrap()
        .wait()
        .unwrap();
}

#[test]
fn filters_demods_and_resamplers() {
    let text = grc(
        &[
            ("samp_rate", "variable", &[("value", "480e3")], EN),
            ("fsk_dev", "variable", &[("value", "5e3")], EN),
            (
                "src",
                "analog_noise_source_x",
                &[
                    ("type", "complex"),
                    ("amp", "0.1"),
                    ("noise_type", "analog.GR_GAUSSIAN"),
                    ("seed", "0"),
                ],
                EN,
            ),
            (
                "th",
                "blocks_throttle",
                &[("type", "complex"), ("samples_per_second", "samp_rate")],
                EN,
            ),
            (
                "lpf",
                "low_pass_filter",
                &[
                    ("type", "fir_filter_ccf"),
                    ("decim", "2"),
                    ("interp", "1"),
                    ("gain", "1"),
                    ("samp_rate", "samp_rate"),
                    ("cutoff_freq", "50e3"),
                    ("width", "10e3"),
                    ("win", "window.WIN_KAISER"),
                    ("beta", "5.653"),
                ],
                EN,
            ),
            (
                "demod",
                "analog_quadrature_demod_cf",
                &[("gain", "samp_rate/2/(2*math.pi*fsk_dev/8.0)")],
                EN,
            ),
            (
                "wfm",
                "analog_wfm_rcv",
                &[("quad_rate", "samp_rate/2"), ("audio_decimation", "5")],
                EN,
            ),
            (
                "nbfm",
                "analog_nbfm_rx",
                &[
                    ("audio_rate", "48000"),
                    ("quad_rate", "240000"),
                    ("tau", "75e-6"),
                    ("max_dev", "fsk_dev"),
                ],
                EN,
            ),
            (
                "hpf",
                "high_pass_filter",
                &[
                    ("type", "fir_filter_fff"),
                    ("decim", "1"),
                    ("interp", "1"),
                    ("gain", "1"),
                    ("samp_rate", "48000"),
                    ("cutoff_freq", "300"),
                    ("width", "100"),
                    ("win", "window.WIN_HANN"),
                    ("beta", "6.76"),
                ],
                EN,
            ),
            (
                "agc",
                "analog_agc_xx",
                &[
                    ("type", "float"),
                    ("rate", "1e-4"),
                    ("reference", "0.5"),
                    ("gain", "1.0"),
                    ("max_gain", "65536"),
                ],
                EN,
            ),
            (
                "sq",
                "analog_pwr_squelch_xx",
                &[
                    ("type", "float"),
                    ("threshold", "-50"),
                    ("alpha", "1e-4"),
                    ("ramp", "0"),
                    ("gate", "False"),
                ],
                EN,
            ),
            ("snk1", "blocks_null_sink", &[("type", "float")], EN),
            ("snk2", "blocks_null_sink", &[("type", "float")], EN),
            ("snk3", "blocks_null_sink", &[("type", "float")], EN),
        ],
        &[
            ["src", "0", "th", "0"],
            ["th", "0", "lpf", "0"],
            ["lpf", "0", "demod", "0"],
            ["lpf", "0", "wfm", "0"],
            ["lpf", "0", "nbfm", "0"],
            ["demod", "0", "snk1", "0"],
            ["wfm", "0", "snk2", "0"],
            ["nbfm", "0", "hpf", "0"],
            ["hpf", "0", "agc", "0"],
            ["agc", "0", "sq", "0"],
            ["sq", "0", "snk3", "0"],
        ],
    );
    let i = imp(&text);
    let g = &i.graph;
    // Low-pass with decimation: the filter keeps the name, a decimator follows.
    let lpf = block(g, "lpf");
    assert_eq!(lpf.kind, "fir_filter");
    assert_eq!(lpf.params["mode"], json!("low_pass"));
    assert_eq!(lpf.params["window"], json!("kaiser"));
    assert_eq!(lpf.params["attenuation"], json!(60.0));
    assert_eq!(block(g, "lpf_decim").kind, "decimator");
    assert!(has_edge(g, "lpf.out", "lpf_decim.in"));
    assert!(has_edge(g, "lpf_decim.out", "demod.in"));
    // gain = rate / (2π · dev / 8) → deviation = dev / 8.
    assert_eq!(
        block(g, "demod").params["deviation"],
        json!("$fsk_dev / 8.0")
    );
    // WFM: demod + decimator.
    assert_eq!(block(g, "wfm").params["deemphasis"], json!("75us"));
    assert_eq!(block(g, "wfm_audio").params["decimation"], json!(5));
    assert!(has_edge(g, "wfm_audio.out", "snk2.in"));
    // NBFM: demod + resampler to the audio rate.
    assert_eq!(block(g, "nbfm").params["deviation"], json!("$fsk_dev"));
    assert_eq!(block(g, "nbfm_audio").params["output_rate"], json!(48000.0));
    assert!(has_edge(g, "nbfm_audio.out", "hpf.in"));
    // Hann isn't available.
    assert_eq!(block(g, "hpf").params["window"], json!("hamming"));
    assert!(warned(&i, &["hpf", "WIN_HANN"]));
    assert_eq!(block(g, "agc").params["target"], json!(0.5));
    assert!(warned(&i, &["agc", "attack"]));
    assert_eq!(block(g, "sq").params["threshold"], json!(-50.0));
    resolves(g);
    run_briefly(g);
}

#[test]
fn bypass_rules_and_unsupported_streams() {
    let text = grc(
        &[
            (
                "a",
                "analog_sig_source_x",
                &[("type", "float"), ("samp_rate", "48000")],
                EN,
            ),
            (
                "b",
                "analog_sig_source_x",
                &[("type", "float"), ("samp_rate", "48000")],
                EN,
            ),
            (
                "add",
                "blocks_add_xx",
                &[("type", "float"), ("num_inputs", "2")],
                "bypassed",
            ),
            (
                "i",
                "analog_sig_source_x",
                &[("type", "int"), ("samp_rate", "48000")],
                EN,
            ),
            (
                "v",
                "blocks_throttle",
                &[
                    ("type", "float"),
                    ("vlen", "4"),
                    ("samples_per_second", "48000"),
                ],
                EN,
            ),
            ("snk", "blocks_null_sink", &[("type", "float")], EN),
        ],
        &[
            ["a", "0", "add", "0"],
            ["b", "0", "add", "1"],
            ["add", "0", "snk", "0"],
        ],
    );
    let i = imp(&text);
    assert!(
        warned(&i, &["`add`", "bypassed", "exactly one input"]),
        "{:#?}",
        i.warnings
    );
    assert!(warned(&i, &["`i`", "stream type `int`"]));
    assert!(warned(&i, &["`v`", "vlen 4"]));
    assert!(warned(&i, &["`snk`", "isn't connected"]));
    assert!(
        i.graph
            .blocks
            .iter()
            .all(|b| !["add", "i", "v"].contains(&b.id.as_str()))
    );
}

#[test]
fn variables_that_dont_translate() {
    let text = grc(
        &[
            ("a", "variable", &[("value", "2**10")], EN),
            ("b", "variable", &[("value", "int(a / 3)")], EN),
            ("c", "variable", &[("value", "a * 2")], EN),
            ("d", "variable", &[("value", "some_module.thing(3)")], EN),
            ("e", "variable", &[("value", "[1, 2, 3]")], EN),
            ("f", "variable", &[("value", "1 if True else 2")], EN),
            (
                "g",
                "variable_qtgui_chooser",
                &[("value", "1e3"), ("options", "[1e3, 2e3]")],
                EN,
            ),
            ("h", "variable_qtgui_entry", &[("value", "c - 1")], EN),
        ],
        &[],
    );
    let i = imp(&text);
    let v = &i.graph.vars;
    // `**` has no equivalent: the value is kept, with a warning.
    assert_eq!(v["a"], json!(1024));
    assert!(warned(&i, &["variable `a`", "kept as its value 1024"]));
    // int() that truncates: the value.
    assert_eq!(v["b"], json!(341));
    assert!(warned(&i, &["variable `b`", "doesn't translate exactly"]));
    // Plain references survive.
    assert_eq!(v["c"], json!("$a * 2"));
    assert_eq!(v["h"], json!("$c - 1"));
    assert_eq!(v["g"], json!(1000));
    for name in ["d", "e", "f"] {
        assert!(!v.contains_key(name));
        assert!(
            warned(&i, &[&format!("variable `{name}`")]),
            "{name}: {:#?}",
            i.warnings
        );
    }
    let r = resolve(&i.graph, &foxygnal_blocks::registry(), &NoLoader).unwrap();
    assert_eq!(var(&r, "h"), 2047.0);
}

#[test]
fn rejects_what_isnt_grc() {
    for bad in [
        "",
        "just text",
        "[[block]]\nid = 'x'",
        "- a\n- b",
        "blocks: [\n",
    ] {
        assert!(import(bad, &Options::default()).is_err(), "{bad:?}");
    }
    let e = import("foo: 1\n", &Options::default()).unwrap_err();
    assert!(e.0.contains("GNU Radio"), "{e}");
}

#[test]
fn toml_has_a_header_and_parses() {
    let i = imp(&fixture("unknown_blocks.grc"));
    let text = foxygnal_grc::to_toml(&i, "unknown_blocks.grc").unwrap();
    assert!(text.starts_with("# Imported from unknown_blocks.grc"));
    assert!(text.contains("# - `epy_block_0` (epy_block)"));
    let back = GraphSpec::from_toml(&text).unwrap();
    assert_eq!(back.blocks.len(), i.graph.blocks.len());
    let _: Value = serde_json::to_value(&back).unwrap();
}
