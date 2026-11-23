//! Wired settings (`block.@param`) and constant inputs through the server:
//! catalog, graph round trips, runs, live changes and warnings.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use foxygnal_core::GraphSpec;
use foxygnal_server::session::Session;
use foxygnal_server::{Config, Security, Ui, router};
use tower::ServiceExt;

fn config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        ui: Ui::None,
        project_dir: PathBuf::from("."),
        security: Security {
            allow_remote: false,
            token: None,
        },
        allow_tx: false,
    }
}

fn session() -> Arc<Session> {
    Session::new(Arc::new(foxygnal_blocks::registry()), PathBuf::from("."))
}

/// A tone through a gain whose Gain is wired to a slow LFO, and an adder
/// with a constant on `b`.
const WIRED: &str = r#"
[vars]
dc = 0.25

[[block]]
id = "tone"
kind = "signal_source"
params = { type = "f32" }

[[block]]
id = "add"
kind = "add"
params = { type = "f32" }
const_inputs = { b = "$dc" }

[[block]]
id = "lfo"
kind = "signal_source"
params = { type = "f32", frequency = 1.0, amplitude = 0.2, offset = 0.5, sample_rate = 100 }

[[block]]
id = "pace"
kind = "throttle"
params = { type = "f32" }

[[block]]
id = "gain"
kind = "gain"
params = { type = "f32" }
ui = { wired = ["gain"] }

[[block]]
id = "throttle"
kind = "throttle"
params = { type = "f32" }

[[block]]
id = "sink"
kind = "null_sink"
params = { type = "f32" }

[[connect]]
from = "tone.out"
to = "add.a"

[[connect]]
from = "add.out"
to = "gain.in"

[[connect]]
from = "lfo.out"
to = "pace.in"

[[connect]]
from = "pace.out"
to = "gain.@gain"

[[connect]]
from = "gain.out"
to = "throttle.in"

[[connect]]
from = "throttle.out"
to = "sink.in"
"#;

async fn call(session: Arc<Session>, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = router(&config(), session).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, body.to_vec())
}

fn req(method: &str, uri: &str, ty: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7355")
        .header(header::CONTENT_TYPE, ty)
        .body(body.into())
        .unwrap()
}

fn status(s: &Session) -> serde_json::Value {
    let mut v = serde_json::to_value(s.snapshot(1)).unwrap();
    v["status"].take()
}

#[tokio::test]
async fn the_catalog_says_what_can_be_wired_and_hides_internal_blocks() {
    let (code, body) = call(
        session(),
        req("GET", "/api/blocks", "application/json", Body::empty()),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let all: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    let find = |k: &str| all.iter().find(|b| b["kind"] == k).unwrap();
    let gain = &find("gain")["params"];
    let g = gain
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "gain");
    assert_eq!(g.unwrap()["live"], true, "live (and a float) = wireable");
    assert_eq!(find("const_source")["hidden"], true);
    assert!(find("gain").get("hidden").is_none());
    assert_eq!(find("peak_frequency")["outputs"][0]["type"], "f32");
}

#[tokio::test]
async fn a_wired_graph_round_trips_through_the_api() {
    let s = session();
    // TOML → JSON → TOML keeps `@` ports, `const_inputs` and the UI flag.
    let (code, json) = call(
        s.clone(),
        req("POST", "/api/toml/parse", "text/plain", WIRED),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{}", String::from_utf8_lossy(&json));
    let g: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let add = g["block"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "add");
    assert_eq!(add.unwrap()["const_inputs"]["b"], "$dc");
    assert!(
        g["connect"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["to"] == "gain.@gain")
    );
    let (code, _) = call(
        s.clone(),
        req("PUT", "/api/graph", "application/json", json.clone()),
    )
    .await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let (code, toml) = call(
        s.clone(),
        req("GET", "/api/graph.toml", "text/plain", Body::empty()),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let back = GraphSpec::from_toml(std::str::from_utf8(&toml).unwrap()).unwrap();
    let orig = GraphSpec::from_toml(WIRED).unwrap();
    assert_eq!(
        serde_json::to_value(&back).unwrap(),
        serde_json::to_value(&orig).unwrap()
    );

    // The metadata names the wire into the setting as drawn; the hidden
    // constant source and its wire stay out of sight.
    let mut v = serde_json::to_value(s.snapshot(1)).unwrap();
    let meta = v["meta"].take();
    assert!(meta.get("error").is_none(), "{meta}");
    let edges = meta["edges"].as_array().unwrap();
    assert!(edges.iter().any(|e| e["to"] == "gain.@gain"), "{meta}");
    assert!(
        edges
            .iter()
            .all(|e| !e["from"].as_str().unwrap().contains('/')),
        "{meta}"
    );
    assert!(
        edges
            .iter()
            .any(|e| e["from"] == "add.out" && e["sample_rate"] == 48000.0)
    );

    // It runs; changing the constant applies live (not stale).
    s.run();
    assert_eq!(status(&s)["state"], "running", "{}", status(&s));
    let mut g = orig.clone();
    g.vars.insert("dc".into(), 0.75.into());
    s.set_graph(1, g.clone(), None);
    assert_eq!(status(&s)["stale"], false);
    // Turning the constant into a wire is a change of topology: stale.
    g.blocks[1].const_inputs.clear();
    s.set_graph(1, g, None);
    assert_eq!(status(&s)["stale"], true);
    tokio::time::timeout(Duration::from_secs(5), s.stop_and_wait())
        .await
        .expect("stops");
    assert_eq!(status(&s)["state"], "stopped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_wired_value_is_a_warning_on_the_block_not_an_error() {
    let s = session();
    let (mut events, _) = s.subscribe(9);
    // The cutoff must be above 0; the LFO sends −5.
    let g = GraphSpec::from_toml(
        r#"
        [[block]]
        id = "src"
        kind = "signal_source"
        params = { type = "c32", sample_rate = 48000 }
        [[block]]
        id = "x"
        kind = "freq_xlating_fir"
        params = { cutoff = 5000, transition = 2000 }
        [[block]]
        id = "throttle"
        kind = "throttle"
        params = { type = "c32" }
        [[block]]
        id = "sink"
        kind = "null_sink"
        params = { type = "c32" }
        [[block]]
        id = "ctl"
        kind = "signal_source"
        params = { type = "f32", waveform = "constant", amplitude = 0.0, offset = -5.0, sample_rate = 10 }
        [[block]]
        id = "pace"
        kind = "throttle"
        params = { type = "f32" }
        [[connect]]
        from = "src.out"
        to = "x.in"
        [[connect]]
        from = "x.out"
        to = "throttle.in"
        [[connect]]
        from = "throttle.out"
        to = "sink.in"
        [[connect]]
        from = "ctl.out"
        to = "pace.in"
        [[connect]]
        from = "pace.out"
        to = "x.@cutoff"
        "#,
    )
    .unwrap();
    s.set_graph(1, g, None);
    s.run();
    let warned = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v: serde_json::Value = serde_json::from_str(&events.recv().await.unwrap()).unwrap();
            if v["op"] == "status" && v.get("warnings").is_some() {
                break v;
            }
        }
    })
    .await
    .expect("a warning arrives");
    let w = &warned["warnings"][0];
    assert_eq!(w["block"], "x");
    let msg = w["message"].as_str().unwrap();
    assert!(
        msg.contains("setting `cutoff`") && msg.contains("-5"),
        "{msg}"
    );
    assert_eq!(warned["state"], "running");
    // Still running a while later, still one warning.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let st = status(&s);
    assert_eq!(st["state"], "running");
    assert_eq!(st["warnings"].as_array().unwrap().len(), 1);
    tokio::time::timeout(Duration::from_secs(5), s.stop_and_wait())
        .await
        .expect("stops");
}
