//! Recording a wire of the running graph to SigMF over the protocol, and the
//! recordings endpoints.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use foxygnal_core::GraphSpec;
use foxygnal_server::session::{Command, Session};
use foxygnal_server::{Config, Security, Ui, router};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower::ServiceExt;

fn project(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("foxygnal-rec-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A looping SigMF source (a ramp with a retune and an annotation in every
/// pass) paced to 1 MS/s.
fn graph(dir: &Path) -> GraphSpec {
    let n = 1000u32;
    let data: Vec<u8> = (0..n)
        .flat_map(|i| [(i as f32).to_le_bytes(), 0f32.to_le_bytes()].concat())
        .collect();
    std::fs::write(dir.join("ramp.sigmf-data"), data).unwrap();
    std::fs::write(
        dir.join("ramp.sigmf-meta"),
        json!({
            "global": {"core:datatype": "cf32_le", "core:sample_rate": 1e6, "core:version": "1.2.0"},
            "captures": [{"core:sample_start": 0, "core:frequency": 100e6},
                         {"core:sample_start": 400, "core:frequency": 100.2e6}],
            "annotations": [{"core:sample_start": 600, "core:label": "packet",
                             "foxygnal:packet": "hi"}]
        })
        .to_string(),
    )
    .unwrap();
    GraphSpec::from_toml(
        r#"
[[block]]
id = "src"
kind = "sigmf_source"
params = { type = "c32", path = "ramp.sigmf-meta", repeat = true }
[[block]]
id = "throttle"
kind = "throttle"
params = { type = "c32" }
[[block]]
id = "sink"
kind = "null_sink"
params = { type = "c32" }
[[connect]]
from = "src.out"
to = "throttle.in"
[[connect]]
from = "throttle.out"
to = "sink.in"
"#,
    )
    .unwrap()
}

fn cmd(v: Value) -> Command {
    serde_json::from_value(v).unwrap()
}

/// The next `record` event matching `pred`.
async fn record_event(
    events: &mut broadcast::Receiver<Arc<str>>,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("a record event in time")
            .unwrap();
        let v: Value = serde_json::from_str(&ev).unwrap();
        if v["op"] == "record" && pred(&v) {
            return v;
        }
    }
}

const EDGE: &str = r#"{"from": "throttle.out", "to": "sink.in"}"#;

fn edge() -> Value {
    serde_json::from_str(EDGE).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn records_a_wire_losslessly_with_its_tags() {
    let dir = project("wire");
    let s = Session::new(Arc::new(foxygnal_blocks::registry()), dir.clone());
    s.set_graph(1, graph(&dir), Some("ramp test.fox.toml".into()));
    let (mut events, _) = s.subscribe(1);

    // Nothing to record before running.
    s.handle(1, cmd(json!({"op": "record_start", "edge": edge()})));
    let v = record_event(&mut events, |_| true).await;
    assert_eq!(v["done"], true);
    assert!(
        v["error"].as_str().unwrap().contains("Run the graph"),
        "{v}"
    );

    s.run();
    s.handle(1, cmd(json!({"op": "record_start", "edge": edge()})));
    let v = record_event(&mut events, |v| v["samples"].as_u64() > Some(100_000)).await;
    assert_eq!(v["done"], false, "{v}");
    // One packet per 1000-sample pass, counted while recording.
    assert!(v["annotations"].as_u64() > Some(50), "{v}");
    let name = v["name"].as_str().unwrap().to_owned();
    assert!(name.starts_with("ramp_test-throttle_out-"), "{name}");
    assert_eq!(v["path"], format!("recordings/{name}.sigmf-meta"));
    // A recording in progress is in the snapshot for new tabs.
    let hello = serde_json::to_value(s.snapshot(2)).unwrap();
    assert_eq!(hello["recordings"][0]["name"], name.as_str());
    // The same wire twice is refused.
    s.handle(1, cmd(json!({"op": "record_start", "edge": edge()})));
    let v = record_event(&mut events, |v| v["done"] == true).await;
    assert!(v["error"].as_str().unwrap().contains("already"), "{v}");

    s.handle(1, cmd(json!({"op": "record_stop", "edge": edge()})));
    let done = record_event(&mut events, |v| {
        v["done"] == true && v["name"] == name.as_str()
    })
    .await;
    assert!(done["error"].is_null(), "{done}");
    let samples = done["samples"].as_u64().unwrap();

    let base = dir.join("recordings").join(&name);
    let data = std::fs::read(base.with_extension("sigmf-data")).unwrap();
    assert_eq!(data.len() as u64, samples * 8);
    assert_eq!(done["bytes"], data.len() as u64);
    // Lossless: the ramp continues sample by sample from wherever it started.
    let re: Vec<u32> = data
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(c[..4].try_into().unwrap()) as u32)
        .collect();
    for (i, w) in re.windows(2).enumerate() {
        assert_eq!(w[1], (w[0] + 1) % 1000, "sample {i}");
    }
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(base.with_extension("sigmf-meta")).unwrap())
            .unwrap();
    assert_eq!(meta["global"]["core:datatype"], "cf32_le");
    assert_eq!(meta["global"]["core:sample_rate"], 1e6);
    assert_eq!(meta["global"]["core:recorder"], "foxygnal");
    assert_eq!(meta["captures"][0]["core:frequency"], 100e6);
    // Retunes and annotations at the right samples: the ramp says which.
    let caps = meta["captures"].as_array().unwrap();
    assert!(caps.len() > 2, "{caps:?}");
    for c in &caps[1..] {
        let at = c["core:sample_start"].as_u64().unwrap() as usize;
        let f = c["core:frequency"].as_f64().unwrap();
        let want = if re[at] == 0 { 100e6 } else { 100.2e6 };
        assert!(re[at] == 0 || re[at] == 400, "capture at ramp {}", re[at]);
        assert_eq!(f, want);
    }
    let ann = meta["annotations"].as_array().unwrap();
    assert!(!ann.is_empty());
    assert_eq!(done["annotations"], ann.len() as u64);
    for a in ann {
        let at = a["core:sample_start"].as_u64().unwrap() as usize;
        assert_eq!(re[at], 600, "{a}");
        assert_eq!(a["foxygnal:packet"], "hi");
    }

    // A recording also ends with the run.
    s.handle(
        1,
        cmd(json!({"op": "record_start", "edge": edge(), "name": "second"})),
    );
    record_event(&mut events, |v| v["name"] == "second" && v["done"] == false).await;
    s.stop();
    let v = record_event(&mut events, |v| v["name"] == "second" && v["done"] == true).await;
    assert!(v["error"].is_null(), "{v}");
    tokio::time::timeout(Duration::from_secs(5), s.stop_and_wait())
        .await
        .expect("graph stops");
    assert!(dir.join("recordings/second.sigmf-meta").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn recording_names_cannot_escape_the_project() {
    let dir = project("names");
    let s = Session::new(Arc::new(foxygnal_blocks::registry()), dir.clone());
    s.set_graph(1, graph(&dir), None);
    let (mut events, _) = s.subscribe(1);
    s.run();
    for bad in ["../evil", "/tmp/evil", "a/b", "..", ".hidden"] {
        s.handle(
            1,
            cmd(json!({"op": "record_start", "edge": edge(), "name": bad})),
        );
        let v = record_event(&mut events, |v| v["done"] == true).await;
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("not a valid recording name"),
            "{bad}: {v}"
        );
    }
    assert!(!dir.join("evil.sigmf-meta").exists());
    assert!(!dir.parent().unwrap().join("evil.sigmf-meta").exists());
    tokio::time::timeout(Duration::from_secs(5), s.stop_and_wait())
        .await
        .expect("graph stops");
}

fn config(dir: &Path) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        ui: Ui::None,
        project_dir: dir.to_owned(),
        security: Security {
            allow_remote: false,
            token: None,
        },
        allow_tx: false,
    }
}

async fn call(cfg: &Config, s: &Arc<Session>, method: &str, uri: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7355")
        .body(Body::empty())
        .unwrap();
    let resp = router(cfg, s.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn recordings_endpoints() {
    let dir = project("rest");
    let s = Session::new(Arc::new(foxygnal_blocks::registry()), dir.clone());
    let cfg = config(&dir);
    // No recordings directory yet: an empty list.
    let (st, body) = call(&cfg, &s, "GET", "/api/recordings").await;
    assert_eq!((st, body.as_str()), (StatusCode::OK, "[]"));

    let rec = dir.join("recordings");
    std::fs::create_dir_all(&rec).unwrap();
    graph(&rec); // writes ramp.sigmf-{meta,data}
    // A file outside recordings/ that traversal would reach.
    std::fs::write(dir.join("secret.sigmf-meta"), "{}").unwrap();

    let (st, body) = call(&cfg, &s, "GET", "/api/recordings").await;
    assert_eq!(st, StatusCode::OK);
    let list: Value = serde_json::from_str(&body).unwrap();
    let r = &list[0];
    assert_eq!(r["name"], "ramp");
    assert_eq!(r["path"], "recordings/ramp.sigmf-meta");
    assert_eq!(r["datatype"], "cf32_le");
    assert_eq!(r["sample_rate"], 1e6);
    assert_eq!(r["frequency"], 100e6);
    assert_eq!(r["samples"], 1000);
    assert_eq!(r["captures"], 2);
    assert_eq!(r["annotations"], 1);
    assert_eq!(r["recording"], false);

    let (st, body) = call(&cfg, &s, "GET", "/api/recordings/ramp").await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("foxygnal:packet"));
    for bad in [
        "/api/recordings/..%2Fsecret",
        "/api/recordings/..%2Fsecret.sigmf-meta",
        "/api/recordings/%2E%2E",
        "/api/recordings/%2Ftmp%2Fx",
        "/api/recordings/.hidden",
    ] {
        let (st, _) = call(&cfg, &s, "GET", bad).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "GET {bad}");
        let (st, _) = call(&cfg, &s, "DELETE", bad).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "DELETE {bad}");
    }
    assert!(dir.join("secret.sigmf-meta").exists());
    let (st, _) = call(&cfg, &s, "GET", "/api/recordings/nope").await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, _) = call(&cfg, &s, "DELETE", "/api/recordings/ramp").await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(!rec.join("ramp.sigmf-meta").exists());
    assert!(!rec.join("ramp.sigmf-data").exists());
    let (st, _) = call(&cfg, &s, "DELETE", "/api/recordings/ramp").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
