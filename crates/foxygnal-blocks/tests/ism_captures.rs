//! `ism_decoder` against rtl_433's public test captures, when they are
//! around: set `RTL433_TESTS` to a checkout of
//! <https://github.com/merbanan/rtl_433_tests> (a sparse checkout of the
//! device folders is enough) and run
//! `cargo test --release -p foxygnal-blocks --test ism_captures -- --nocapture`.
//! The captures aren't vendored (that repository carries no licence).
//!
//! Each `.cu8` file comes with rtl_433's output (`.json`, one object per
//! line). For every file where rtl_433 found a model we also decode, this
//! checks that we find the same (model, id) pairs, and prints a summary per
//! model.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{Complex32, Flowgraph, StreamMeta};
use serde_json::{Value, json};

/// rtl_433 model names our device table writes.
const MODELS: &[&str] = &[
    "Acurite-3n1",
    "Acurite-5n1",
    "Acurite-606TX",
    "Acurite-609TXC",
    "Acurite-986",
    "Acurite-Atlas",
    "Acurite-Rain899",
    "Acurite-Tower",
    "Ambientweather-F007TH",
    "AmbientWeather-WH31B",
    "AmbientWeather-WH31E",
    "LaCrosse-TX",
    "LaCrosse-TX5U",
    "LaCrosse-TX141B",
    "LaCrosse-TX141Bv2",
    "LaCrosse-TX141Bv3",
    "LaCrosse-TX141THBv2",
    "LaCrosse-TX141W",
    "LaCrosse-TX29IT",
    "LaCrosse-TX35DTHIT",
    "Fineoffset-WH24",
    "Fineoffset-WH65B",
    "Fineoffset-WHx080",
    "Fineoffset-WS69",
    "Oregon-BHTR968",
    "Oregon-PCR800",
    "Oregon-PCR800a",
    "Oregon-THGR122N",
    "Oregon-THGR228N",
    "Oregon-THGR810",
    "Oregon-THGR968",
    "Oregon-THN132N",
    "Oregon-THN802",
    "Oregon-THR228N",
    "Oregon-UV800",
    "Oregon-WGR800",
    "Oregon-WGR968",
    "Nexus-T",
    "Nexus-TH",
    "Prologue-TH",
    "Rubicson-Temperature",
    "Schrader",
    "Schrader-EG53MA4",
    "Toyota",
    "Ford",
    "Citroen",
    "Honeywell-Security",
    "Interlogix-Security",
    "ERT-SCM",
    "SCMplus",
    "IDM",
];

fn is_ours(model: &str) -> bool {
    MODELS.contains(&model) || model.starts_with("Generic-Remote")
}

/// rtl_433's generic remotes (`Generic-Remote`, `Generic-Remote-TE275`, …)
/// split the 24 bits differently from our EV1527 / PT2262: compare those by
/// kind only.
fn key(model: &str, id: String) -> (String, String) {
    if model.starts_with("Generic-Remote") || model == "EV1527" || model == "PT2262" {
        ("remote (EV1527/PT2262)".into(), String::new())
    } else {
        (model.to_owned(), id)
    }
}

/// Sample rate from a capture's name (`…_250k.cu8`, `…_1000k.cu8`), else
/// rtl_433's default of 250 kS/s.
fn rate_of(path: &Path) -> f64 {
    let stem = path.file_stem().unwrap().to_string_lossy();
    let last = stem.rsplit('_').next().unwrap_or("");
    let num = |s: &str| s.parse::<f64>().ok();
    if let Some(k) = last.strip_suffix('k').and_then(num) {
        return k * 1e3;
    }
    if let Some(m) = last.strip_suffix('M').and_then(num).filter(|&m| m < 10.0) {
        return m * 1e6;
    }
    250e3
}

fn decode(path: &Path, rate: f64) -> Vec<String> {
    let raw = std::fs::read(path).unwrap();
    let sig: Vec<Complex32> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            Complex32::new(
                (f32::from(c[0]) - 127.4) / 128.0,
                (f32::from(c[1]) - 127.4) / 128.0,
            )
        })
        .collect();
    // Some captures start right at the signal: give the noise floor a lead
    // of noise as strong as the capture's quietest part.
    let mut mags: Vec<f32> = sig.iter().step_by(7).map(|c| c.norm()).collect();
    mags.sort_by(f32::total_cmp);
    let level = mags.get(mags.len() / 4).copied().unwrap_or(0.01).max(1e-3);
    let mut seed = 0x1234_5678_u32;
    let mut noise = |n: usize| -> Vec<Complex32> {
        (0..n)
            .map(|_| {
                let mut u = || {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
                };
                Complex32::new(u(), u()) * (level * 2.0)
            })
            .collect()
    };
    let mut padded = noise((rate * 0.01) as usize);
    padded.extend(sig);
    padded.extend(noise((rate * 0.03) as usize));
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(padded, StreamMeta::with_rate(rate)),
    );
    let Ok(block) =
        foxygnal_blocks::registry().build("ism_decoder", json!({}).as_object().unwrap())
    else {
        panic!("ism_decoder");
    };
    let dec = fg.add("ism", block);
    fg.connect(src, 0, dec, 0).unwrap();
    let (sink, out) = VectorSink::<u8>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(dec, 0, sink, 0).unwrap();
    if fg.start().and_then(|r| r.wait()).is_err() {
        return vec![];
    }
    let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
    text.lines().map(str::to_owned).collect()
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(' ').find_map(|kv| {
        kv.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v)
    })
}

fn id_of(v: &Value) -> String {
    match v.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "cu8") {
            out.push(p);
        }
    }
}

#[test]
fn rtl_433_test_captures() {
    let Some(root) = std::env::var_os("RTL433_TESTS") else {
        eprintln!("RTL433_TESTS not set: skipped");
        return;
    };
    let mut files = Vec::new();
    walk(&Path::new(&root).join("tests"), &mut files);
    files.sort();
    // model → (files expected, files found, files where we decoded it
    // although rtl_433 didn't)
    let mut stats: BTreeMap<String, (u32, u32, u32)> = BTreeMap::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f.with_extension("json")) else {
            continue;
        };
        let expect: BTreeSet<(String, String)> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| {
                let m = v.get("model")?.as_str()?;
                is_ours(m).then(|| key(m, id_of(&v)))
            })
            .collect();
        let rate = rate_of(f);
        let lines = decode(f, rate);
        let got: BTreeSet<(String, String)> = lines
            .iter()
            .filter_map(|l| {
                Some(key(
                    field(l, "model")?,
                    field(l, "id").unwrap_or("").to_owned(),
                ))
            })
            .collect();
        for (m, id) in &expect {
            let s = stats.entry(m.clone()).or_default();
            s.0 += 1;
            if got.contains(&(m.clone(), id.clone())) {
                s.1 += 1;
            } else {
                eprintln!("MISSED {m} id={id} in {} ({rate})", f.display());
            }
        }
        for (m, id) in &got {
            if !expect.contains(&(m.clone(), id.clone())) {
                stats.entry(m.clone()).or_default().2 += 1;
                eprintln!("EXTRA {m} id={id} in {}", f.display());
            }
        }
    }
    let (mut e, mut g) = (0, 0);
    eprintln!(
        "{:<28} {:>8} {:>6} {:>6}",
        "model", "expected", "found", "extra"
    );
    for (m, (exp, found, extra)) in &stats {
        eprintln!("{m:<28} {exp:>8} {found:>6} {extra:>6}");
        e += exp;
        g += found;
    }
    eprintln!("total: {g} of {e}");
    assert!(e == 0 || f64::from(g) >= 0.8 * f64::from(e), "{g} of {e}");
}
