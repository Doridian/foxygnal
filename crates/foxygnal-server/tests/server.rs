use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use foxygnal_core::GraphSpec;
use foxygnal_server::session::{RunState, Session};
use foxygnal_server::{Config, Security, Ui, router};
use tower::ServiceExt;

fn config(token: Option<&str>) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        ui: Ui::None,
        project_dir: PathBuf::from("."),
        security: Security {
            allow_remote: token.is_some(),
            token: token.map(str::to_owned),
        },
        allow_tx: false,
    }
}

fn session() -> Arc<Session> {
    Session::new(Arc::new(foxygnal_blocks::registry()), PathBuf::from("."))
}

async fn status_of(cfg: &Config, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap) {
    let resp = router(cfg, session()).oneshot(req).await.unwrap();
    (resp.status(), resp.headers().clone())
}

fn get(uri: &str, host: &str) -> axum::http::request::Builder {
    Request::get(uri).header(header::HOST, host)
}

#[tokio::test]
async fn host_and_origin_checks() {
    let cfg = config(None);
    let ok = |b: axum::http::request::Builder| b.body(Body::empty()).unwrap();

    let (s, _) = status_of(&cfg, ok(get("/api/health", "127.0.0.1:7355"))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = status_of(&cfg, ok(get("/api/health", "localhost:5173"))).await;
    assert_eq!(s, StatusCode::OK);

    // DNS rebinding: a foreign name pointing at loopback.
    let (s, _) = status_of(&cfg, ok(get("/api/health", "evil.example:7355"))).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);

    // Cross-site request from another page in the same browser.
    let req = get("/api/health", "127.0.0.1:7355").header(header::ORIGIN, "https://evil.example");
    let (s, _) = status_of(&cfg, ok(req)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Same origin is fine.
    let req = get("/api/health", "127.0.0.1:7355").header(header::ORIGIN, "http://127.0.0.1:7355");
    let (s, _) = status_of(&cfg, ok(req)).await;
    assert_eq!(s, StatusCode::OK);

    // Another local port is a different origin, too.
    let req = get("/api/health", "127.0.0.1:7355").header(header::ORIGIN, "http://127.0.0.1:8080");
    let (s, _) = status_of(&cfg, ok(req)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn token_flow_for_remote_access() {
    let cfg = config(Some("s3cret"));
    let ok = |b: axum::http::request::Builder| b.body(Body::empty()).unwrap();

    // Any host is allowed with --allow-remote, but not without a token.
    let (s, _) = status_of(&cfg, ok(get("/api/health", "sdr.lan:7355"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = status_of(&cfg, ok(get("/?token=wrong", "sdr.lan:7355"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // The URL token becomes a cookie and is stripped from the address bar.
    let (s, h) = status_of(&cfg, ok(get("/?x=1&token=s3cret", "sdr.lan:7355"))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(h[header::LOCATION], "/?x=1");
    let cookie = h[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.starts_with("foxygnal_token=s3cret;"), "{cookie}");
    assert!(cookie.contains("HttpOnly"));

    let req =
        get("/api/health", "sdr.lan:7355").header(header::COOKIE, "a=b; foxygnal_token=s3cret");
    let (s, _) = status_of(&cfg, ok(req)).await;
    assert_eq!(s, StatusCode::OK);
    let req = get("/api/health", "sdr.lan:7355").header(header::AUTHORIZATION, "Bearer s3cret");
    let (s, _) = status_of(&cfg, ok(req)).await;
    assert_eq!(s, StatusCode::OK);
}

fn graph(toml: &str) -> GraphSpec {
    GraphSpec::from_toml(toml).unwrap()
}

const RUNNING_TONE: &str = r#"
[[block]]
id = "tone"
kind = "signal_source"
params = { type = "f32" }
[[block]]
id = "gain"
kind = "gain"
params = { type = "f32" }
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
to = "gain.in"
[[connect]]
from = "gain.out"
to = "throttle.in"
[[connect]]
from = "throttle.out"
to = "sink.in"
"#;

fn state(s: &Session) -> serde_json::Value {
    match serde_json::to_value(s.snapshot(1)).unwrap() {
        serde_json::Value::Object(mut m) => m.remove("status").unwrap(),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn build_errors_are_pinned_and_cleared_by_edits() {
    let s = session();
    s.set_graph(
        1,
        graph("[[block]]\nid = \"sink\"\nkind = \"null_sink\"\n"),
        None,
    );
    s.run();
    let st = state(&s);
    assert_eq!(st["state"], "error");
    assert_eq!(st["errors"][0]["block"], "sink");
    assert_eq!(st["errors"][0]["message"], "input `in` is not connected");

    s.set_graph(1, GraphSpec::default(), None);
    let st = state(&s);
    assert_eq!(st["state"], "stopped");
    assert_eq!(st["errors"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn live_edits_apply_and_other_edits_mark_the_run_stale() {
    let s = session();
    let mut g = graph(RUNNING_TONE);
    s.set_graph(1, g.clone(), None);
    s.run();
    assert_eq!(
        serde_json::to_value(RunState::Running).unwrap(),
        state(&s)["state"]
    );

    // Gain is live: applied in place.
    g.blocks[1].params.insert("gain".into(), 0.5.into());
    s.set_graph(1, g.clone(), None);
    assert_eq!(state(&s)["stale"], false);

    // Moving a block on the canvas changes nothing in the engine.
    g.blocks[1].ui.insert("x".into(), 100.into());
    s.set_graph(1, g.clone(), None);
    assert_eq!(state(&s)["stale"], false);

    // The waveform is not live: needs a restart.
    g.blocks[0]
        .params
        .insert("waveform".into(), "square".into());
    s.set_graph(1, g.clone(), None);
    assert_eq!(state(&s)["stale"], true);
    assert_eq!(state(&s)["state"], "running");

    tokio::time::timeout(Duration::from_secs(5), s.stop_and_wait())
        .await
        .expect("graph stops");
    let st = state(&s);
    assert_eq!(st["state"], "stopped");
    assert_eq!(st["stale"], false);
}

/// Probes attach to wires of the running graph and stream binary frames to
/// the tab that asked; a probe on a wire that isn't running says why; stats
/// arrive for everyone.
#[tokio::test(flavor = "multi_thread")]
async fn probes_stream_frames_and_stats_flow() {
    use foxygnal_server::probe::Outgoing;
    use foxygnal_server::session::Command;

    let s = session();
    s.set_graph(1, graph(RUNNING_TONE), None);
    let (mut events, _) = s.subscribe(7);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    s.hub().add_client(7, tx);
    let cmd = |json: serde_json::Value| serde_json::from_value::<Command>(json).unwrap();
    s.handle(
        7,
        cmd(serde_json::json!({
            "op": "probe_set", "id": 3,
            "edge": {"from": "gain.out", "to": "throttle.in"},
            "probe": {"kind": "scope", "span": 480, "points": 240},
        })),
    );
    s.handle(
        7,
        cmd(serde_json::json!({
            "op": "probe_set", "id": 4,
            "edge": {"from": "nope.out", "to": "sink.in"},
            "probe": {"kind": "meter"},
        })),
    );
    s.run();

    let mut scope_frames = 0;
    let mut missing_error = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while scope_frames < 3 || missing_error.is_none() {
        let msg = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("probe output in time")
            .unwrap();
        match msg {
            Outgoing::Binary(b) => {
                let id = u32::from_le_bytes(b[0..4].try_into().unwrap());
                assert_eq!(id, 3);
                assert_eq!(b[8], 1, "scope frame");
                let span = u32::from_le_bytes(b[12..16].try_into().unwrap());
                assert_eq!(span, 480);
                scope_frames += 1;
            }
            Outgoing::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                assert_eq!(v["op"], "probe");
                if v["id"] == 4 {
                    missing_error = v["error"].as_str().map(str::to_owned);
                }
            }
        }
    }
    assert!(
        missing_error
            .unwrap()
            .contains("isn't in the running graph")
    );

    // Stats are broadcast with per-wire rates and per-block CPU.
    let stats = loop {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("stats in time")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&ev).unwrap();
        if v["op"] == "stats" {
            break v;
        }
    };
    let edge = stats["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["from"] == "throttle.out")
        .expect("throttle edge");
    let rate = edge["rate"].as_f64().unwrap();
    assert!(
        (20_000.0..80_000.0).contains(&rate),
        "about 48 kS/s, got {rate}"
    );
    assert!(
        stats["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["block"] == "tone")
    );

    s.hub().remove_client(7);
    tokio::time::timeout(Duration::from_secs(5), s.stop_and_wait())
        .await
        .expect("graph stops");
}

/// Every graph change comes with the stream metadata of each wire.
#[tokio::test]
async fn graph_changes_carry_stream_metadata() {
    let s = session();
    let (mut events, _) = s.subscribe(9);
    s.set_graph(1, graph(RUNNING_TONE), None);
    let meta = loop {
        let v: serde_json::Value = serde_json::from_str(&events.recv().await.unwrap()).unwrap();
        if v["op"] == "meta" {
            break v;
        }
    };
    let edges = meta["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 3);
    assert!(edges.iter().all(|e| e["sample_rate"] == 48000.0), "{meta}");
    let hello = serde_json::to_value(s.snapshot(9)).unwrap();
    assert_eq!(hello["meta"]["edges"].as_array().unwrap().len(), 3);
}

/// The catalog flags blocks with a frequency-response preview, and the
/// preview endpoint designs the filter at the given input rate.
#[tokio::test]
async fn filter_response_preview() {
    let cfg = config(None);
    let call = |req: Request<Body>| async {
        let resp = router(&cfg, session()).oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body)
    };
    let post = |kind: &str, body: serde_json::Value| {
        Request::post(format!("/api/blocks/{kind}/response"))
            .header(header::HOST, "127.0.0.1:7355")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (s, body) = call(
        get("/api/blocks", "127.0.0.1:7355")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let cat: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let find = |kind: &str| {
        cat.as_array()
            .unwrap()
            .iter()
            .find(|d| d["kind"] == kind)
            .unwrap()
            .clone()
    };
    assert_eq!(find("fir_filter")["response"], true);
    assert!(find("add").get("response").is_none());

    let (s, body) = call(post(
        "fir_filter",
        serde_json::json!({"params": {"cutoff": 3000.0}, "sample_rate": 48000.0}),
    ))
    .await;
    assert_eq!(s, StatusCode::OK);
    let r: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        r["freqs"].as_array().unwrap().len(),
        r["db"].as_array().unwrap().len()
    );
    assert!(r["taps"].as_u64().unwrap() > 1);

    let (s, body) = call(post(
        "fir_filter",
        serde_json::json!({"params": {"cutoff": 30000.0}, "sample_rate": 48000.0}),
    ))
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("24 kHz"));

    let (s, _) = call(post("add", serde_json::json!({"sample_rate": 48000.0}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(post("fir_filter", serde_json::json!({"sample_rate": 0.0}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// A browser audio block paces the graph at the stream rate, and its audio
/// probe delivers every sample, in order, with no gaps.
#[tokio::test(flavor = "multi_thread")]
async fn browser_audio_streams_every_sample() {
    use foxygnal_server::probe::Outgoing;
    use foxygnal_server::session::Command;

    let s = session();
    s.set_graph(
        1,
        graph(
            r#"
[[block]]
id = "tone"
kind = "signal_source"
params = { type = "f32", sample_rate = 48000.0, frequency = 1000.0 }
[[block]]
id = "speaker"
kind = "browser_audio"
[[connect]]
from = "tone.out"
to = "speaker.in"
"#,
        ),
        None,
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    s.hub().add_client(7, tx);
    let cmd = |json: serde_json::Value| serde_json::from_value::<Command>(json).unwrap();
    s.handle(
        7,
        cmd(serde_json::json!({
            "op": "probe_set", "id": 9, "fps": 60,
            "edge": {"from": "tone.out", "to": "speaker.in"},
            "probe": {"kind": "audio"},
        })),
    );
    s.run();
    let t0 = tokio::time::Instant::now();
    let mut samples: Vec<f32> = Vec::new();
    let mut frames = 0;
    // Samples received during the second half-second onwards: the first
    // frames may carry the buffer's backlog in one burst.
    let mut steady = 0usize;
    while t0.elapsed() < Duration::from_millis(1500) {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await else {
            panic!("audio frames in time");
        };
        if let Outgoing::Binary(b) = msg {
            assert_eq!(b[8], 5, "audio frame");
            let n = u32::from_le_bytes(b[12..16].try_into().unwrap()) as usize;
            // The first read can race the generator filling the buffer at
            // start-up; after that, every sample must arrive.
            if b[16] & 1 != 0 {
                assert!(frames == 0, "gap after the first frame");
            }
            frames += 1;
            assert_eq!(b.len(), 20 + 4 * n);
            if t0.elapsed() >= Duration::from_millis(500) {
                steady += n;
            }
            samples.extend(
                b[20..]
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes(c.try_into().unwrap())),
            );
        }
    }
    s.stop();
    // Paced at 48 kS/s, not as fast as the CPU allows.
    assert!(
        (40_000..56_000).contains(&steady),
        "{steady} samples in 1 s"
    );
    // A continuous 1 kHz sine: x[n+1] + x[n-1] = 2 cos(w) x[n].
    let k = 2.0 * (2.0 * std::f32::consts::PI * 1000.0 / 48000.0).cos();
    for w in samples.windows(3) {
        assert!((w[2] + w[0] - k * w[1]).abs() < 1e-3, "discontinuity");
    }
}
