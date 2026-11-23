//! Graph variables and hierarchical blocks through the session: live updates
//! via a variable, metadata/stats/probes/errors mapped back to the graph as
//! drawn, and graph files offered as blocks.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use foxygnal_core::GraphSpec;
use foxygnal_server::probe::Outgoing;
use foxygnal_server::session::{Command, Session};
use foxygnal_server::{Config, Security, Ui, router};
use serde_json::{Value, json};
use tower::ServiceExt;

fn graph(toml: &str) -> GraphSpec {
    GraphSpec::from_toml(toml).unwrap()
}

/// A fresh project directory with the given files.
fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("foxygnal-hier-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (n, text) in files {
        std::fs::write(dir.join(n), text).unwrap();
    }
    dir
}

fn session(dir: PathBuf) -> Arc<Session> {
    Session::new(Arc::new(foxygnal_blocks::registry()), dir)
}

fn status(s: &Session) -> Value {
    serde_json::to_value(s.snapshot(1)).unwrap()["status"].clone()
}

fn meta(s: &Session) -> Value {
    serde_json::to_value(s.snapshot(1)).unwrap()["meta"].clone()
}

const WITH_VARS: &str = r#"
[vars]
fs = 48000
g = 0.5
f = 1000

[[block]]
id = "tone"
kind = "signal_source"
params = { type = "f32", sample_rate = "$fs", frequency = "$f" }
[[block]]
id = "gain"
kind = "gain"
params = { type = "f32", gain = "$g * 2" }
[[block]]
id = "throttle"
kind = "throttle"
params = { type = "f32", sample_rate = "$fs" }
[[block]]
id = "sink"
kind = "null_sink"
params = { type = "f32" }
[[connect]]
from = "tone.out"
to = "gain.in"
[[connect]]
from = "gain.out"
to = "throttle.in"
[[connect]]
from = "throttle.out"
to = "sink.in"
"#;

#[tokio::test]
async fn variables_drive_live_updates() {
    let s = session(project("vars", &[]));
    let mut g = graph(WITH_VARS);
    s.set_graph(1, g.clone(), None);
    assert!(meta(&s)["error"].is_null(), "{}", meta(&s));
    assert_eq!(meta(&s)["edges"][0]["sample_rate"], 48000.0);
    s.run();
    assert_eq!(status(&s)["state"], "running", "{}", status(&s));

    // `g` and `f` only reach live params: applied in place.
    g.vars.insert("g".into(), json!(0.25));
    g.vars.insert("f".into(), json!("2k"));
    s.set_graph(1, g.clone(), None);
    assert_eq!(status(&s)["stale"], false);

    // A broken expression doesn't touch the running graph, but it is stale
    // and the error shows up in the metadata.
    g.vars.insert("g".into(), json!("$nope"));
    s.set_graph(1, g.clone(), None);
    assert_eq!(status(&s)["stale"], true);
    assert_eq!(
        meta(&s)["error"]["message"],
        "graph error: variable `g`: unknown variable `$nope`"
    );

    tokio::time::timeout(Duration::from_secs(5), s.stop_and_wait())
        .await
        .expect("graph stops");

    // `fs` reaches the (non-live) sample rates: stale.
    let mut g = graph(WITH_VARS);
    s.set_graph(1, g.clone(), None);
    s.run();
    g.vars.insert("fs".into(), json!(96000));
    s.set_graph(1, g.clone(), None);
    assert_eq!(status(&s)["stale"], true);
    assert_eq!(meta(&s)["edges"][0]["sample_rate"], 96000.0);
    tokio::time::timeout(Duration::from_secs(5), s.stop_and_wait())
        .await
        .expect("graph stops");
}

/// Outputs a tone through a gain; `freq` and `level` are its parameters.
const TONE: &str = r#"
[vars]
rate = 48000
freq = 1000
level = 1

[[export.output]]
name = "out"
from = "amp.out"

[[block]]
id = "src"
kind = "signal_source"
params = { type = "c32", frequency = "$freq", sample_rate = "$rate" }
[[block]]
id = "amp"
kind = "gain"
params = { type = "c32", gain = "$level" }
[[connect]]
from = "src.out"
to = "amp.in"
"#;

const PARENT: &str = r#"
[vars]
fs = 32000

[[block]]
id = "tone"
kind = "graph:tone.fox.toml"
params = { rate = "$fs", freq = 500, level = 0.5 }
[[block]]
id = "throttle"
kind = "throttle"
params = { type = "c32" }
[[block]]
id = "sink"
kind = "null_sink"
params = { type = "c32" }
[[connect]]
from = "tone.out"
to = "throttle.in"
[[connect]]
from = "throttle.out"
to = "sink.in"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn hierarchical_blocks_look_like_blocks() {
    let s = session(project("hier", &[("tone.fox.toml", TONE)]));
    let (mut events, _) = s.subscribe(7);
    let mut g = graph(PARENT);
    s.set_graph(1, g.clone(), None);

    // Metadata names the wires as drawn; the wire inside `tone` isn't one.
    let m = meta(&s);
    assert!(m["error"].is_null(), "{m}");
    let edges = m["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2, "{m}");
    assert_eq!(edges[0]["from"], "tone.out");
    assert_eq!(edges[0]["to"], "throttle.in");
    assert_eq!(edges[0]["sample_rate"], 32000.0);

    // A probe on the wire out of the hierarchical block.
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    s.hub().add_client(7, tx);
    let cmd = serde_json::from_value::<Command>(json!({
        "op": "probe_set", "id": 1,
        "edge": {"from": "tone.out", "to": "throttle.in"},
        "probe": {"kind": "meter"},
    }))
    .unwrap();
    s.handle(7, cmd);
    s.run();
    assert_eq!(status(&s)["state"], "running", "{}", status(&s));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("probe frames in time")
            .unwrap()
        {
            Outgoing::Binary(b) => {
                assert_eq!(b[8], 4, "meter frame");
                break;
            }
            Outgoing::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                assert!(v["error"].is_null(), "{v}");
            }
        }
    }

    // Stats: the hierarchical block's CPU and its output wire, as drawn.
    let stats = loop {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("stats in time")
            .unwrap();
        let v: Value = serde_json::from_str(&ev).unwrap();
        if v["op"] == "stats" {
            break v;
        }
    };
    let blocks: Vec<&str> = stats["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["block"].as_str().unwrap())
        .collect();
    assert_eq!(blocks, ["tone", "throttle", "sink"], "{stats}");
    let wires: Vec<(&str, &str)> = stats["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["from"].as_str().unwrap(), e["to"].as_str().unwrap()))
        .collect();
    assert!(wires.contains(&("tone.out", "throttle.in")), "{stats}");
    assert!(!wires.iter().any(|(f, _)| f.contains('/')), "{stats}");

    // A param of the hierarchical block that only reaches live params.
    g.blocks[0].params.insert("level".into(), json!(0.1));
    s.set_graph(1, g.clone(), None);
    assert_eq!(status(&s)["stale"], false);
    g.blocks[0].params.insert("rate".into(), json!(8000));
    s.set_graph(1, g.clone(), None);
    assert_eq!(status(&s)["stale"], true);

    s.hub().remove_client(7);
    tokio::time::timeout(Duration::from_secs(5), s.stop_and_wait())
        .await
        .expect("graph stops");
}

#[tokio::test]
async fn errors_inside_point_at_the_hierarchical_block() {
    let bad = TONE.replace("gain = \"$level\"", "gain = \"$level / 0\"");
    let s = session(project("errors", &[("tone.fox.toml", &bad)]));
    s.set_graph(1, graph(PARENT), None);
    let m = meta(&s);
    assert_eq!(m["error"]["block"], "tone", "{m}");
    assert_eq!(
        m["error"]["message"],
        "amp: parameter `gain`: division by zero"
    );

    // A type mismatch on a wire into an inner block names the wire as drawn.
    let s = session(project("edges", &[("tone.fox.toml", TONE)]));
    s.set_graph(
        1,
        graph(&PARENT.replace(
            "params = { type = \"c32\" }\n[[block]]\nid = \"sink\"",
            "params = { type = \"f32\" }\n[[block]]\nid = \"sink\"",
        )),
        None,
    );
    let m = meta(&s);
    assert_eq!(m["error"]["edge"]["from"], "tone.out", "{m}");
    assert_eq!(m["error"]["edge"]["to"], "throttle.in", "{m}");

    // Unconnected exported input: the run error names the block.
    let amp = r#"
[[export.input]]
name = "in"
to = "g.in"
[[export.output]]
name = "out"
from = "g.out"
[[block]]
id = "g"
kind = "gain"
"#;
    let s = session(project("unconnected", &[("amp.fox.toml", amp)]));
    s.set_graph(
        1,
        graph("[[block]]\nid = \"a\"\nkind = \"graph:amp.fox.toml\"\n[[block]]\nid = \"s\"\nkind = \"null_sink\"\n[[connect]]\nfrom = \"a.out\"\nto = \"s.in\"\n"),
        None,
    );
    s.run();
    let st = status(&s);
    assert_eq!(st["errors"][0]["block"], "a", "{st}");
    assert_eq!(st["errors"][0]["message"], "g: input `in` is not connected");
}

#[tokio::test]
async fn graph_files_with_exports_are_offered_as_blocks() {
    let dir = project(
        "palette",
        &[
            ("tone.fox.toml", TONE),
            ("plain.fox.toml", "[vars]\na = 1\n"),
            ("broken.fox.toml", "this is not toml"),
        ],
    );
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        ui: Ui::None,
        project_dir: dir.clone(),
        security: Security {
            allow_remote: false,
            token: None,
        },
        allow_tx: false,
    };
    let app = router(&cfg, session(dir));
    let resp = app
        .oneshot(
            Request::get("/api/blocks")
                .header(header::HOST, "127.0.0.1:7355")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let all: Vec<Value> = serde_json::from_slice(&body).unwrap();
    let graphs: Vec<&Value> = all.iter().filter(|d| d["category"] == "Graphs").collect();
    let kinds: Vec<&str> = graphs.iter().map(|d| d["kind"].as_str().unwrap()).collect();
    // The project's file, and the bundled library's.
    assert_eq!(
        kinds,
        ["graph:tone-and-noise.fox.toml", "graph:tone.fox.toml"]
    );
    let tone = graphs[1];
    assert_eq!(tone["outputs"], json!([{"name": "out", "type": "c32"}]));
    let params: Vec<&str> = tone["params"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(params, ["rate", "freq", "level"]);
    assert!(all.iter().any(|d| d["kind"] == "signal_source"));
}

#[tokio::test]
async fn bundled_hierarchical_example_builds_anywhere() {
    // The example's sub-graph comes from the bundled library.
    let s = session(project("example", &[]));
    let g = graph(include_str!("../../../examples/hier-tones.fox.toml"));
    s.set_graph(1, g, None);
    let m = meta(&s);
    assert!(m["error"].is_null(), "{m}");
    let into_mix = m["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["from"] == "low.out")
        .expect("low.out wire");
    assert_eq!(into_mix["sample_rate"], 48000.0);
}
