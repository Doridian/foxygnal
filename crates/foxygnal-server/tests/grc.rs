//! `.grc` files in the project directory: listed, imported on open (with
//! warnings and a `.fox.toml` name to save as), never written.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use foxygnal_server::session::Session;
use foxygnal_server::{Config, Security, Ui, router};
use serde_json::Value;
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:7355";

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../foxygnal-grc/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path).unwrap()
}

async fn call(dir: &Path, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        ui: Ui::None,
        project_dir: dir.to_path_buf(),
        security: Security {
            allow_remote: false,
            token: None,
        },
        allow_tx: false,
    };
    let session = Session::new(Arc::new(foxygnal_blocks::registry()), dir.to_path_buf());
    let resp = router(&cfg, session).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, body.to_vec())
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri)
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn grc_files_import_on_open() {
    let dir = std::env::temp_dir().join(format!("foxygnal-grc-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let grc = fixture("fm_receiver.grc");
    std::fs::write(dir.join("fm rx.grc"), &grc).unwrap();
    std::fs::write(dir.join("notes.txt"), "x").unwrap();

    // Listed next to .fox.toml files.
    let (s, body) = call(&dir, get("/api/files")).await;
    assert_eq!(s, StatusCode::OK);
    let files: Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["fm rx.grc"]);

    // Opening converts it.
    let (s, body) = call(&dir, get("/api/import/fm%20rx.grc")).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let imp: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(imp["name"], "fm rx.fox.toml");
    assert_eq!(imp["title"], "FM receiver");
    assert!(
        imp["graph"]["block"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["kind"] == "fm_demod")
    );
    assert!(
        imp["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("qtgui_freq_sink_x_0"))
    );

    // Uploads convert the same way.
    let (s, body) = call(
        &dir,
        Request::post("/api/import")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from(grc.clone()))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let up: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(up["graph"], imp["graph"]);

    // Not a flowgraph: a readable 400.
    let (s, body) = call(
        &dir,
        Request::post("/api/import")
            .header(header::HOST, HOST)
            .body(Body::from("hello: world\n"))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("GNU Radio"));

    // GNU Radio 3.7 XML, told apart by its content; broken XML is a 400.
    for (text, status) in [
        (fixture("tone_scope_37.grc"), StatusCode::OK),
        ("<flow_graph><block>".to_owned(), StatusCode::BAD_REQUEST),
    ] {
        let (s, body) = call(
            &dir,
            Request::post("/api/import")
                .header(header::HOST, HOST)
                .body(Body::from(text))
                .unwrap(),
        )
        .await;
        assert_eq!(s, status, "{}", String::from_utf8_lossy(&body));
    }

    // Paths stay inside the project dir, and a .grc is never written.
    let (s, _) = call(&dir, get("/api/import/..%2Fx.grc")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&dir, get("/api/import/missing.grc")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(
        &dir,
        Request::put("/api/files/fm%20rx.grc")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(imp["graph"].to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(std::fs::read_to_string(dir.join("fm rx.grc")).unwrap(), grc);

    // Saving under the suggested name writes the .fox.toml next to it.
    let (s, _) = call(
        &dir,
        Request::put("/api/files/fm%20rx.fox.toml")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(imp["graph"].to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(dir.join("fm rx.fox.toml").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

fn post_json(uri: &str, body: String) -> Request<Body> {
    Request::post(uri)
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn graphs_export_to_grc() {
    let dir = std::env::temp_dir().join(format!("foxygnal-grc-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ex = format!("{}/../../examples", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(format!("{ex}/hier-tones.fox.toml")).unwrap();
    let spec = foxygnal_core::GraphSpec::from_toml(&text).unwrap();

    // Sub-graphs come from the project directory or the bundled library.
    let body = serde_json::to_string(&spec).unwrap();
    let uri = "/api/export?name=hier%20tones.fox.toml";
    let (s, out) = call(&dir, post_json(uri, body)).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&out));
    let exp: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(exp["name"], "hier tones.grc");
    let grc = exp["grc"].as_str().unwrap();
    assert!(grc.contains("file_format: 1"));
    assert!(grc.contains("id: analog_sig_source_x"));
    let warnings = exp["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("flattened"))
    );
    // Nothing is written.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    // A body that isn't a graph is a 4xx.
    let (s, b) = call(&dir, post_json("/api/export", "flowgraph".into())).await;
    assert!(s.is_client_error(), "{s} {}", String::from_utf8_lossy(&b));
    let _ = std::fs::remove_dir_all(&dir);
}
