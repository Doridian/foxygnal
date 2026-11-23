//! Graph variables, expressions and hierarchical blocks (core `resolve`),
//! exercised with the standard blocks.

use std::collections::HashMap;

use foxygnal_core::resolve::{self, GraphLoader, diff_params};
use foxygnal_core::{Error, Flowgraph, GraphSpec, NoLoader};
use serde_json::json;

/// Graph files in memory.
struct Files(HashMap<&'static str, &'static str>);

impl GraphLoader for Files {
    fn load(&self, name: &str) -> Result<GraphSpec, Error> {
        let text = self
            .0
            .get(name)
            .ok_or_else(|| Error::Other(format!("no file `{name}`")))?;
        GraphSpec::from_toml(text)
    }
}

fn files(list: &[(&'static str, &'static str)]) -> Files {
    Files(list.iter().copied().collect())
}

fn spec(toml: &str) -> GraphSpec {
    GraphSpec::from_toml(toml).unwrap()
}

fn resolve(toml: &str, loader: &dyn GraphLoader) -> Result<resolve::Resolved, Error> {
    resolve::resolve(&spec(toml), &foxygnal_blocks::registry(), loader)
}

fn param<'a>(r: &'a resolve::Resolved, block: &str, name: &str) -> &'a serde_json::Value {
    &r.graph
        .blocks
        .iter()
        .find(|b| b.id == block)
        .unwrap_or_else(|| panic!("no block {block}"))
        .params[name]
}

#[test]
fn variables_and_expressions() {
    let r = resolve(
        r#"
        [vars]
        cutoff = "$samp_rate / 10"
        samp_rate = "2.4M"
        count = 1000

        [[block]]
        id = "tone"
        kind = "signal_source"
        params = { sample_rate = "$samp_rate", frequency = "-($cutoff - 40k) * 2", amplitude = 0.5 }

        [[block]]
        id = "head"
        kind = "head"
        params = { type = "c32", count = "$count * 2" }
        "#,
        &NoLoader,
    )
    .unwrap();
    assert_eq!(param(&r, "tone", "sample_rate"), &json!(2.4e6));
    assert_eq!(param(&r, "tone", "frequency"), &json!(-400e3));
    assert_eq!(param(&r, "tone", "amplitude"), &json!(0.5));
    assert_eq!(param(&r, "head", "count"), &json!(2000));
    assert_eq!(param(&r, "head", "type"), &json!("c32"));
    assert_eq!(
        r.vars,
        [
            ("cutoff".to_owned(), 2.4e5),
            ("samp_rate".to_owned(), 2.4e6),
            ("count".to_owned(), 1000.0)
        ]
    );
    // The resolved graph builds.
    Flowgraph::from_resolved(&r.graph, &foxygnal_blocks::registry()).unwrap();
}

#[test]
fn variable_errors_name_block_and_param() {
    let err = resolve(
        r#"
        [vars]
        samp_rate = 48000
        [[block]]
        id = "lp"
        kind = "gain"
        params = { gain = "$sr / 2" }
        "#,
        &NoLoader,
    )
    .err()
    .unwrap();
    assert_eq!(
        err.to_string(),
        "block `lp`, parameter `gain`: unknown variable `$sr`"
    );
    assert_eq!(err.block(), Some("lp"));

    let err = resolve(
        r#"
        [[block]]
        id = "h"
        kind = "head"
        params = { count = "10 / 4" }
        "#,
        &NoLoader,
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("not a whole number"), "{err}");

    // In vars: the variable with the bad reference is named.
    let err = resolve("[vars]\na = \"$b * 2\"\nb = \"$c + 1\"\n", &NoLoader)
        .err()
        .unwrap();
    assert_eq!(
        err.to_string(),
        "graph error: variable `b`: unknown variable `$c`"
    );
    let err = resolve("[vars]\na = \"1 +\"\n", &NoLoader).err().unwrap();
    assert!(
        err.to_string().starts_with("graph error: variable `a`:"),
        "{err}"
    );
    let err = resolve("[vars]\na = true\n", &NoLoader).err().unwrap();
    assert!(err.to_string().contains("must be a number"), "{err}");
}

#[test]
fn variable_cycles_are_reported() {
    let err = resolve(
        "[vars]\na = \"$b + 1\"\nb = \"$c * 2\"\nc = \"$a\"\nd = 1\n",
        &NoLoader,
    )
    .err()
    .unwrap();
    assert_eq!(
        err.to_string(),
        "graph error: variable `a` depends on itself: `a` → `b` → `c` → `a`"
    );
    let err = resolve("[vars]\nx = \"$x\"\n", &NoLoader).err().unwrap();
    assert!(err.to_string().contains("`x` → `x`"), "{err}");
}

#[test]
fn vars_survive_a_toml_round_trip_in_order() {
    let text = r#"
[vars]
samp_rate = 48000.0
cutoff = "$samp_rate / 4"

[export]
help = "A tone."

[[export.input]]
name = "in"
to = ["a.in", "b.in"]

[[export.output]]
name = "out"
from = "a.out"

[[block]]
id = "a"
kind = "gain"
params = { gain = "$cutoff" }
"#;
    let g = spec(text);
    let out = g.to_toml().unwrap();
    assert!(
        out.find("[vars]").unwrap() < out.find("[[block]]").unwrap(),
        "{out}"
    );
    assert!(out.contains("cutoff = \"$samp_rate / 4\""), "{out}");
    let back = GraphSpec::from_toml(&out).unwrap();
    assert_eq!(
        back.vars.keys().collect::<Vec<_>>(),
        ["samp_rate", "cutoff"]
    );
    assert_eq!(back.export, g.export);
    assert_eq!(back.blocks[0].params["gain"], json!("$cutoff"));
}

/// A tone source exposing `out`, with `freq` and `rate` as parameters and a
/// derived variable.
const TONE: &str = r#"
[vars]
rate = 48000
freq = 1000
half = "$rate / 2"

[[export.output]]
name = "out"
from = "gain.out"

[[block]]
id = "src"
kind = "signal_source"
params = { type = "c32", frequency = "$freq", sample_rate = "$rate" }

[[block]]
id = "gain"
kind = "gain"
params = { type = "c32", gain = 0.5 }

[[connect]]
from = "src.out"
to = "gain.in"
"#;

/// Passes `in` through a gain whose value is a parameter.
const AMP: &str = r#"
[vars]
g = 2

[[export.input]]
name = "in"
to = "amp.in"

[[export.output]]
name = "out"
from = "amp.out"

[[block]]
id = "amp"
kind = "gain"
params = { type = "c32", gain = "$g" }
"#;

const PARENT: &str = r#"
[vars]
fs = 96000

[[block]]
id = "tone"
kind = "graph:tone.fox.toml"
params = { rate = "$fs", freq = "$fs / 8" }

[[block]]
id = "amp"
kind = "graph:amp.fox.toml"
params = { g = 3 }

[[block]]
id = "sink"
kind = "null_sink"
params = { type = "c32" }

[[connect]]
from = "tone.out"
to = "amp.in"

[[connect]]
from = "amp.0"
to = "sink.in"
"#;

#[test]
fn hierarchical_blocks_flatten() {
    let loader = files(&[("tone.fox.toml", TONE), ("amp.fox.toml", AMP)]);
    let r = resolve(PARENT, &loader).unwrap();
    let ids: Vec<&str> = r.graph.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, ["tone/src", "tone/gain", "amp/amp", "sink"]);
    // Parent params override the child's vars, evaluated in the parent.
    assert_eq!(param(&r, "tone/src", "sample_rate"), &json!(96000.0));
    assert_eq!(param(&r, "tone/src", "frequency"), &json!(12000.0));
    assert_eq!(param(&r, "amp/amp", "gain"), &json!(3.0));
    let conns: Vec<(&str, &str)> = r
        .graph
        .connections
        .iter()
        .map(|c| (c.from.as_str(), c.to.as_str()))
        .collect();
    assert_eq!(
        conns,
        [
            ("tone/src.out", "tone/gain.in"),
            ("tone/gain.out", "amp/amp.in"),
            ("amp/amp.out", "sink.in"),
        ]
    );
    // Each connection knows where it came from; ports by export name.
    let origins: Vec<Option<(&str, &str)>> = r
        .origins
        .iter()
        .map(|o| o.as_ref().map(|c| (c.from.as_str(), c.to.as_str())))
        .collect();
    assert_eq!(
        origins,
        [
            None,
            Some(("tone.out", "amp.in")),
            Some(("amp.out", "sink.in"))
        ]
    );

    // Metadata flows through: the rate set inside `tone` reaches the sink.
    let mut fg = Flowgraph::from_resolved(&r.graph, &foxygnal_blocks::registry()).unwrap();
    let meta = fg.configure().unwrap();
    let into_sink = meta.iter().find(|m| m.to == "sink.in").unwrap();
    assert_eq!(into_sink.from, "amp/amp.out");
    assert_eq!(into_sink.meta.sample_rate, Some(96000.0));

    // And it runs.
    let reg = foxygnal_blocks::registry();
    let fg = Flowgraph::from_spec_with(&spec(PARENT), &reg, &loader).unwrap();
    let running = fg.start().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    running.stop();
    running.wait().unwrap();
}

#[test]
fn nested_hierarchy_and_fan_out_exports() {
    // `pair` uses `amp` twice; its input feeds both.
    const PAIR: &str = r#"
[vars]
g = 1
[[export.input]]
name = "in"
to = ["a.in", "b.in"]
[[export.output]]
name = "first"
from = "a.out"
[[export.output]]
name = "second"
from = "b.out"
[[block]]
id = "a"
kind = "graph:amp.fox.toml"
params = { g = "$g" }
[[block]]
id = "b"
kind = "graph:amp.fox.toml"
params = { g = "$g * 10" }
"#;
    let loader = files(&[
        ("tone.fox.toml", TONE),
        ("amp.fox.toml", AMP),
        ("pair.fox.toml", PAIR),
    ]);
    let r = resolve(
        r#"
        [[block]]
        id = "t"
        kind = "graph:tone.fox.toml"
        [[block]]
        id = "p"
        kind = "graph:pair.fox.toml"
        params = { g = 4 }
        [[block]]
        id = "s1"
        kind = "null_sink"
        [[block]]
        id = "s2"
        kind = "null_sink"
        [[connect]]
        from = "t.out"
        to = "p.in"
        [[connect]]
        from = "p.first"
        to = "s1.in"
        [[connect]]
        from = "p.second"
        to = "s2.in"
        "#,
        &loader,
    )
    .unwrap();
    assert_eq!(param(&r, "p/a/amp", "gain"), &json!(4.0));
    assert_eq!(param(&r, "p/b/amp", "gain"), &json!(40.0));
    let into_p: Vec<&str> = r
        .graph
        .connections
        .iter()
        .filter(|c| c.from == "t/gain.out")
        .map(|c| c.to.as_str())
        .collect();
    assert_eq!(into_p, ["p/a/amp.in", "p/b/amp.in"]);
    assert!(
        r.graph
            .connections
            .iter()
            .any(|c| c.from == "p/b/amp.out" && c.to == "s2.in")
    );
    Flowgraph::from_resolved(&r.graph, &foxygnal_blocks::registry())
        .unwrap()
        .configure()
        .unwrap();
}

#[test]
fn hierarchy_errors() {
    // A graph that contains itself, directly or not.
    let loop_a = r#"
[[export.output]]
name = "out"
from = "b.out"
[[block]]
id = "b"
kind = "graph:b.fox.toml"
"#;
    let loop_b = r#"
[[export.output]]
name = "out"
from = "a.out"
[[block]]
id = "a"
kind = "graph:a.fox.toml"
"#;
    let loader = files(&[("a.fox.toml", loop_a), ("b.fox.toml", loop_b)]);
    let err = resolve(
        "[[block]]\nid = \"x\"\nkind = \"graph:a.fox.toml\"\n",
        &loader,
    )
    .err()
    .unwrap();
    assert_eq!(err.block(), Some("x/b/a"), "{err}");
    assert!(
        err.to_string()
            .contains("`a.fox.toml` contains itself: a.fox.toml → b.fox.toml → a.fox.toml"),
        "{err}"
    );

    // Inner errors name the inner block under the hierarchical block's id.
    let bad = r#"
[vars]
g = 1
[[export.output]]
name = "out"
from = "amp.out"
[[block]]
id = "amp"
kind = "gain"
params = { gain = "$nope" }
"#;
    let loader = files(&[("bad.fox.toml", bad), ("amp.fox.toml", AMP)]);
    let err = resolve(
        "[[block]]\nid = \"h\"\nkind = \"graph:bad.fox.toml\"\n",
        &loader,
    )
    .err()
    .unwrap();
    assert_eq!(
        err.to_string(),
        "block `h/amp`, parameter `gain`: unknown variable `$nope`"
    );

    // An override is evaluated in the parent.
    let err = resolve(
        "[[block]]\nid = \"h\"\nkind = \"graph:amp.fox.toml\"\nparams = { g = \"$missing\" }\n",
        &loader,
    )
    .err()
    .unwrap();
    assert_eq!(
        err.to_string(),
        "block `h`, parameter `g`: unknown variable `$missing`"
    );

    // Missing file, unknown port.
    let err = resolve(
        "[[block]]\nid = \"h\"\nkind = \"graph:nope.fox.toml\"\n",
        &loader,
    )
    .err()
    .unwrap();
    assert_eq!(err.block(), Some("h"));
    let err = resolve(
        r#"
        [[block]]
        id = "h"
        kind = "graph:amp.fox.toml"
        [[block]]
        id = "s"
        kind = "null_sink"
        [[connect]]
        from = "h.bogus"
        to = "s.in"
        "#,
        &loader,
    )
    .err()
    .unwrap();
    assert!(
        matches!(&err, Error::Edge { from, .. } if from == "h.bogus"),
        "{err}"
    );
    assert!(err.to_string().contains("has no output `bogus`"), "{err}");

    // Sub-graphs need a loader; `/` is reserved for flattened ids.
    let err = resolve(
        "[[block]]\nid = \"h\"\nkind = \"graph:amp.fox.toml\"\n",
        &NoLoader,
    )
    .err()
    .unwrap();
    assert_eq!(err.block(), Some("h"));
    let err = resolve("[[block]]\nid = \"a/b\"\nkind = \"gain\"\n", &NoLoader)
        .err()
        .unwrap();
    assert!(err.to_string().contains("must not contain `/`"), "{err}");

    // Exports must point at real blocks.
    let loader = files(&[(
        "e.fox.toml",
        "[[export.output]]\nname = \"out\"\nfrom = \"ghost.out\"\n",
    )]);
    let err = resolve(
        "[[block]]\nid = \"h\"\nkind = \"graph:e.fox.toml\"\n",
        &loader,
    )
    .err()
    .unwrap();
    assert!(
        err.to_string().contains("there is no block `ghost`"),
        "{err}"
    );
}

#[test]
fn graph_files_describe_themselves_as_blocks() {
    let loader = files(&[("tone.fox.toml", TONE), ("amp.fox.toml", AMP)]);
    let reg = foxygnal_blocks::registry();
    let d = resolve::describe("tone.fox.toml", &reg, &loader)
        .unwrap()
        .unwrap();
    assert_eq!(d.kind, "graph:tone.fox.toml");
    assert_eq!(d.label, "tone");
    assert_eq!(d.category, "Graphs");
    assert!(d.inputs.is_empty());
    assert_eq!(d.outputs.len(), 1);
    assert_eq!(d.outputs[0].name, "out");
    assert_eq!(d.outputs[0].ty, foxygnal_core::StreamType::C32);
    // `half` is derived from `rate`, so it isn't a parameter.
    let ps: Vec<(&str, f64, bool)> = d
        .params
        .iter()
        .map(|p| (p.name.as_str(), p.default, p.live))
        .collect();
    // `freq` only drives a live param; `rate` also the (non-live) sample rate.
    assert_eq!(ps, [("rate", 48000.0, false), ("freq", 1000.0, true)]);
    let json = serde_json::to_value(&d).unwrap();
    assert_eq!(json["params"][1]["kind"], "float");
    assert_eq!(json["params"][1]["live"], true);
    assert_eq!(json["outputs"][0]["type"], "c32");

    let d = resolve::describe("amp.fox.toml", &reg, &loader)
        .unwrap()
        .unwrap();
    assert_eq!(d.inputs[0].ty, foxygnal_core::StreamType::C32);

    // No exports: not a block.
    let loader = files(&[("plain.fox.toml", "[vars]\na = 1\n")]);
    assert!(
        resolve::describe("plain.fox.toml", &reg, &loader)
            .unwrap()
            .is_none()
    );
}

#[test]
fn param_diff_sees_through_variables() {
    let reg = foxygnal_blocks::registry();
    let loader = files(&[("tone.fox.toml", TONE), ("amp.fox.toml", AMP)]);
    let a = resolve(PARENT, &loader).unwrap().graph;
    let b = resolve(&PARENT.replace("$fs / 8", "$fs / 4"), &loader)
        .unwrap()
        .graph;
    let ch = diff_params(&reg, &a, &b).unwrap();
    assert_eq!(ch.len(), 1);
    assert_eq!(ch[0].block, "tone/src");
    assert_eq!(ch[0].param, "frequency");
    assert_eq!(ch[0].value, json!(24000.0));
    assert!(ch[0].live);
    // Changing `fs` also changes the sample rate, which isn't live.
    let c = resolve(&PARENT.replace("fs = 96000", "fs = 48000"), &loader)
        .unwrap()
        .graph;
    let ch = diff_params(&reg, &a, &c).unwrap();
    assert!(ch.iter().any(|c| c.param == "sample_rate" && !c.live));
}
