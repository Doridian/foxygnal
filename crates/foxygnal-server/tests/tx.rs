//! The arm-TX safety flow and device discovery. All TX here goes to the
//! simulator (`sim:`), which discards the samples.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use foxygnal_core::GraphSpec;
use foxygnal_server::session::{Command, Session};
use foxygnal_server::{Config, Security, Ui, router};
use serde_json::{Value, json};
use tower::ServiceExt;

const TX_GRAPH: &str = r#"
[vars]
rate = "1M"
[[block]]
id = "tone"
kind = "signal_source"
params = { sample_rate = "$rate" }
[[block]]
id = "tx"
kind = "sdr_sink"
params = { device = "sim:", frequency = "433.92M", gain = 10 }
[[connect]]
from = "tone.out"
to = "tx.in"
"#;

fn session(allow_tx: bool) -> Arc<Session> {
    let s = Session::with_tx(
        Arc::new(foxygnal_blocks::registry()),
        PathBuf::from("."),
        allow_tx,
    );
    s.set_graph(1, GraphSpec::from_toml(TX_GRAPH).unwrap(), None);
    s
}

fn hello(s: &Session) -> Value {
    serde_json::to_value(s.snapshot(1)).unwrap()
}

fn cmd(v: Value) -> Command {
    serde_json::from_value(v).unwrap()
}

async fn wait_for_state(s: &Session, state: &str) -> Value {
    for _ in 0..200 {
        let status = hello(s)["status"].clone();
        if status["state"] == state {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never reached {state}: {}", hello(s)["status"]);
}

#[tokio::test]
async fn meta_lists_tx_blocks_with_resolved_settings() {
    let s = session(false);
    let h = hello(&s);
    assert_eq!(h["allow_tx"], false);
    assert_eq!(
        h["meta"]["tx"],
        json!([{
            "block": "tx", "kind": "sdr_sink", "device": "sim:",
            "frequency": 433.92e6, "sample_rate": 1e6, "gain": 10.0,
        }])
    );
}

#[tokio::test]
async fn refuses_without_allow_tx_even_when_armed() {
    let s = session(false);
    s.handle(1, cmd(json!({"op": "run", "arm_tx": true})));
    let st = wait_for_state(&s, "error").await;
    assert_eq!(st["errors"][0]["block"], "tx");
    let msg = st["errors"][0]["message"].as_str().unwrap();
    assert!(msg.contains("--allow-tx"), "{msg}");
    assert!(st.get("tx_armed").is_none());
}

#[tokio::test]
async fn refuses_without_arming() {
    let s = session(true);
    assert_eq!(hello(&s)["allow_tx"], true);
    // A plain `run` (what older clients send) doesn't arm.
    s.handle(1, cmd(json!({"op": "run"})));
    let st = wait_for_state(&s, "error").await;
    assert_eq!(st["errors"][0]["block"], "tx");
    let msg = st["errors"][0]["message"].as_str().unwrap();
    assert!(msg.contains("arm TX"), "{msg}");
}

#[tokio::test]
async fn armed_run_transmits_and_disarms_when_stopped() {
    let s = session(true);
    assert!(!foxygnal_sdr::tx_armed());
    s.handle(1, cmd(json!({"op": "run", "arm_tx": true})));
    let st = wait_for_state(&s, "running").await;
    assert_eq!(st["tx_armed"], true);
    assert!(foxygnal_sdr::tx_armed());
    // Still running a moment later: the sink accepted the arm.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let st = hello(&s)["status"].clone();
    assert_eq!(st["state"], "running", "{st}");

    s.stop_and_wait().await;
    let st = hello(&s)["status"].clone();
    assert_eq!(st["state"], "stopped", "{st}");
    assert!(st.get("tx_armed").is_none());
    assert!(!foxygnal_sdr::tx_armed());

    // Arming lasted for that run only.
    s.run();
    let st = wait_for_state(&s, "error").await;
    assert!(
        st["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("arm TX")
    );
}

#[tokio::test]
async fn graphs_without_tx_ignore_arming() {
    let s = session(true);
    s.set_graph(
        1,
        GraphSpec::from_toml(
            r#"
[[block]]
id = "rx"
kind = "sdr_source"
params = { device = "sim:", sample_rate = 1e6 }
[[block]]
id = "sink"
kind = "null_sink"
[[connect]]
from = "rx.out"
to = "sink.in"
"#,
        )
        .unwrap(),
        None,
    );
    s.handle(1, cmd(json!({"op": "run", "arm_tx": true})));
    let st = wait_for_state(&s, "running").await;
    assert!(st.get("tx_armed").is_none(), "{st}");
    s.stop_and_wait().await;
}

#[tokio::test]
async fn devices_endpoint_lists_the_simulator() {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        ui: Ui::None,
        project_dir: PathBuf::from("."),
        security: Security {
            allow_remote: false,
            token: None,
        },
        allow_tx: false,
    };
    let resp = router(&cfg, session(false))
        .oneshot(
            Request::get("/api/devices")
                .header(header::HOST, "127.0.0.1:7355")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let all: Value = serde_json::from_slice(&body).unwrap();
    let sim = all
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["driver"] == "sim")
        .expect("sim driver listed");
    assert_eq!(sim["devices"][0]["args"], "sim:");
    assert_eq!(sim["devices"][0]["tx"], true);
    assert!(sim["error"].is_null());
}
