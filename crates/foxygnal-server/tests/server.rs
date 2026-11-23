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
