//! The MCP server over its HTTP transport (`POST /api/mcp`): the protocol,
//! graph editing and its checks, running, measuring, watches and files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use foxygnal_core::raster::{LAST, Row};
use foxygnal_server::session::Session;
use foxygnal_server::{Config, Security, Ui, router};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Rig {
    app: axum::Router,
    session: Arc<Session>,
    dir: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.session.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(name: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("foxygnal-mcp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
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
    let session = Session::new(Arc::new(foxygnal_blocks::registry()), dir.clone());
    Rig {
        app: router(&cfg, session.clone()),
        session,
        dir,
    }
}

impl Rig {
    async fn post(&self, body: String) -> (StatusCode, Option<Value>) {
        let req = Request::post("/api/mcp")
            .header(header::HOST, "127.0.0.1:7355")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 24)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).ok())
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        let (status, v) = self
            .post(
                json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}).to_string(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let v = v.unwrap();
        assert_eq!(v["id"], 7);
        v
    }

    /// Call a tool; returns (is_error, structured content or text, raw result).
    async fn tool(&self, name: &str, args: Value) -> (bool, Value, Value) {
        let v = self
            .rpc("tools/call", json!({"name": name, "arguments": args}))
            .await;
        let r = v["result"].clone();
        assert!(r.is_object(), "{name}: {v}");
        let err = r["isError"].as_bool().unwrap();
        let body = match &r["structuredContent"] {
            Value::Null => r["content"][0]["text"].clone(),
            s => s.clone(),
        };
        (err, body, r)
    }

    async fn ok(&self, name: &str, args: Value) -> Value {
        let (err, body, _) = self.tool(name, args).await;
        assert!(!err, "{name} failed: {body}");
        body
    }

    async fn fails(&self, name: &str, args: Value) -> String {
        let (err, body, _) = self.tool(name, args).await;
        assert!(err, "{name} should have failed: {body}");
        body.as_str().unwrap().to_owned()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn protocol_basics() {
    let r = rig("protocol");
    let v = r
        .rpc(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
        )
        .await;
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(v["result"]["serverInfo"]["name"], "foxygnal");
    assert!(v["result"]["capabilities"]["tools"].is_object());
    assert!(
        v["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("edit_graph")
    );
    // An unknown version gets our newest.
    let v = r
        .rpc("initialize", json!({"protocolVersion": "1999-01-01"}))
        .await;
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25");

    // Notifications get no answer.
    let (status, body) = r
        .post(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.into())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.is_none());

    let v = r.rpc("nonsense", json!({})).await;
    assert_eq!(v["error"]["code"], -32601);
    let (_, v) = r.post("{not json".into()).await;
    assert_eq!(v.unwrap()["error"]["code"], -32700);
    let v = r.rpc("tools/call", json!({"name": "no_such_tool"})).await;
    assert_eq!(v["error"]["code"], -32602);

    // A batch answers each request.
    let (_, v) = r
        .post(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/x"},{"jsonrpc":"2.0","id":2,"method":"ping"}]"#.into())
        .await;
    assert_eq!(v.unwrap().as_array().unwrap().len(), 2);

    let tools = r.rpc("tools/list", json!({})).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "list_blocks",
        "edit_graph",
        "run",
        "measure",
        "watch",
        "record",
    ] {
        assert!(names.contains(&want), "{names:?}");
    }
    for t in tools["result"]["tools"].as_array().unwrap() {
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
    }

    let res = r.rpc("resources/list", json!({})).await;
    let uris: Vec<&str> = res["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"foxygnal://docs/architecture"), "{uris:?}");
    assert!(uris.contains(&"foxygnal://examples/sstv"), "{uris:?}");
    let doc = r
        .rpc(
            "resources/read",
            json!({"uri": "foxygnal://docs/architecture"}),
        )
        .await;
    assert!(
        doc["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("# Architecture")
    );
    let missing = r
        .rpc("resources/read", json!({"uri": "foxygnal://nope"}))
        .await;
    assert!(missing["error"].is_object());

    let p = r
        .rpc(
            "prompts/get",
            json!({"name": "build_receiver", "arguments": {"signal": "NOAA weather radio"}}),
        )
        .await;
    let text = p["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("NOAA weather radio"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalog() {
    let r = rig("catalog");
    let found = r.ok("list_blocks", json!({"query": "fm demod"})).await;
    assert!(
        found
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["kind"] == "fm_demod"),
        "{found}"
    );
    let filters = r.ok("list_blocks", json!({"category": "filters"})).await;
    assert!(
        filters
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["category"] == "Filters")
    );
    let d = r
        .ok("describe_block", json!({"kind": "freq_xlating_fir"}))
        .await;
    let offset = d["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "offset")
        .unwrap();
    assert_eq!(offset["live"], true);
    assert_eq!(offset["wireable"], true);
    let e = r.fails("describe_block", json!({"kind": "fm"})).await;
    assert!(e.contains("did you mean") && e.contains("fm_demod"), "{e}");
    let ex = r.ok("list_examples", json!({})).await;
    assert!(ex.as_array().unwrap().iter().any(|e| e["name"] == "aprs"));
}

/// A tone into a throttle into a null sink, by edits.
async fn tone_graph(r: &Rig) -> Value {
    r.ok(
        "edit_graph",
        json!({"operations": [
            {"op": "set_variable", "name": "rate", "value": 48000},
            {"op": "add_block", "id": "tone", "kind": "signal_source",
             "params": {"type": "c32", "frequency": 3000.0, "amplitude": 0.5, "sample_rate": "$rate"}},
            {"op": "add_block", "id": "slow", "kind": "throttle", "params": {"type": "c32"}},
            {"op": "add_block", "id": "sink", "kind": "null_sink", "params": {"type": "c32"}},
            {"op": "connect", "from": "tone", "to": "slow"},
            {"op": "connect", "from": "slow.out", "to": "sink.in"},
        ]}),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn editing_is_checked_and_attributed() {
    let r = rig("edit");
    let mut events = r.session.events();
    let rep = tone_graph(&r).await;
    assert_eq!(rep["builds"], true, "{rep}");
    assert!(rep.get("unconnected_inputs").is_none(), "{rep}");
    assert_eq!(rep["wires"][0]["sample_rate"], 48000.0, "{rep}");
    // The browser hears about it, as the assistant's doing.
    let mut by = None;
    while let Ok(ev) = events.try_recv() {
        let ev: Value = serde_json::from_str(&ev).unwrap();
        if ev["op"] == "graph" {
            by = Some(ev["by"].clone());
        }
    }
    assert_eq!(by, Some(json!("assistant")));

    // Blocks keep their positions apart.
    let g = r.ok("get_graph", json!({})).await;
    let toml = g["toml"].as_str().unwrap();
    assert!(
        toml.contains("x = 380") && toml.contains("x = 710"),
        "{toml}"
    );

    // A wrong setting name lists the real ones; nothing changes.
    let e = r
        .fails(
            "edit_graph",
            json!({"operations": [
                {"op": "remove_block", "id": "sink"},
                {"op": "set_params", "id": "tone", "params": {"freq": 1.0}},
            ]}),
        )
        .await;
    assert!(e.contains("operation 2") && e.contains("frequency"), "{e}");
    assert!(e.contains("Nothing was changed"), "{e}");
    assert!(r.session.graph().blocks.iter().any(|b| b.id == "sink"));

    // An input takes one wire.
    let e = r
        .fails(
            "edit_graph",
            json!({"operations": [{"op": "connect", "from": "tone.out", "to": "sink.in"}]}),
        )
        .await;
    assert!(e.contains("already fed by `slow.out`"), "{e}");

    // A type mismatch builds with an error naming both ends.
    let rep = r
        .ok(
            "edit_graph",
            json!({"operations": [
                {"op": "add_block", "id": "real", "kind": "null_sink", "params": {"type": "f32"}},
                {"op": "connect", "from": "tone.out", "to": "real.in"},
            ]}),
        )
        .await;
    assert_eq!(rep["builds"], false, "{rep}");
    let msg = rep["error"]["message"].as_str().unwrap();
    assert!(msg.contains("tone.out") && msg.contains("real.in"), "{msg}");

    // Unconnected inputs are listed.
    let rep = r
        .ok(
            "edit_graph",
            json!({"operations": [
                {"op": "remove_block", "id": "real"},
                {"op": "add_block", "id": "mul", "kind": "multiply", "params": {"type": "c32"}},
            ]}),
        )
        .await;
    let un = rep["unconnected_inputs"].as_array().unwrap();
    assert!(
        un.contains(&json!("mul.a")) && un.contains(&json!("mul.b")),
        "{rep}"
    );
    // A constant fills one.
    let rep = r
        .ok(
            "edit_graph",
            json!({"operations": [
                {"op": "set_const_input", "id": "mul", "input": "b", "value": [0.5, 0]},
                {"op": "connect", "from": "tone.out", "to": "mul.a"},
                {"op": "add_block", "id": "s2", "kind": "null_sink", "params": {"type": "c32"}},
                {"op": "connect", "from": "mul.out", "to": "s2.in"},
            ]}),
        )
        .await;
    assert!(rep.get("unconnected_inputs").is_none(), "{rep}");

    // check_graph doesn't touch the open graph.
    let before = r.session.graph().blocks.len();
    let rep = r
        .ok(
            "check_graph",
            json!({"toml": "[[block]]\nid = \"x\"\nkind = \"nope\"\n"}),
        )
        .await;
    assert_eq!(rep["builds"], false);
    assert_eq!(r.session.graph().blocks.len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn run_and_measure() {
    let r = rig("measure");
    tone_graph(&r).await;
    let e = r
        .fails("measure", json!({"wire": "tone.out", "kind": "level"}))
        .await;
    assert!(e.contains("isn't running"), "{e}");
    let st = r.ok("run", json!({})).await;
    assert_eq!(st["state"], "running", "{st}");

    let s = r
        .ok(
            "measure",
            json!({"wire": "tone.out->slow.in", "kind": "spectrum", "seconds": 0.5}),
        )
        .await;
    let sig = &s["signals"][0];
    let f = sig["peak_offset_hz"].as_f64().unwrap();
    assert!((f - 3000.0).abs() < 2.0, "{s}");
    let c = sig["center_offset_hz"].as_f64().unwrap();
    assert!((c - 3000.0).abs() < 5.0, "{s}");
    let level = sig["level_dbfs"].as_f64().unwrap();
    assert!((level + 6.0).abs() < 0.5, "a 0.5 tone is −6 dBFS: {s}");
    assert_eq!(
        s["signals"].as_array().unwrap().len(),
        1,
        "a pure tone: {s}"
    );

    let l = r
        .ok(
            "measure",
            json!({"wire": "slow.out", "kind": "level", "seconds": 0.3}),
        )
        .await;
    assert!((l["rms_dbfs"].as_f64().unwrap() + 6.0).abs() < 0.2, "{l}");

    // A live setting changes the running graph.
    r.ok(
        "edit_graph",
        json!({"operations": [{"op": "set_params", "id": "tone", "params": {"frequency": -5000.0}}]}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let s = r
        .ok(
            "measure",
            json!({"wire": "tone.out", "kind": "spectrum", "seconds": 0.5}),
        )
        .await;
    let f = s["signals"][0]["peak_offset_hz"].as_f64().unwrap();
    assert!((f + 5000.0).abs() < 2.0, "{s}");

    let st = r.ok("status", json!({})).await;
    assert_eq!(st["state"], "running");
    assert!(
        st["wires"].as_array().unwrap().iter().any(|w| {
            let rate = w["items_per_second"].as_f64().unwrap();
            (rate - 48_000.0).abs() < 5_000.0
        }),
        "{st}"
    );
    r.ok("stop", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn transmitting_is_refused() {
    let r = rig("tx");
    r.ok(
        "edit_graph",
        json!({"operations": [
            {"op": "add_block", "id": "tone", "kind": "signal_source", "params": {"type": "c32", "sample_rate": 1e6}},
            {"op": "add_block", "id": "tx", "kind": "sdr_sink", "params": {"device": "sim:", "frequency": 433.92e6}},
            {"op": "connect", "from": "tone", "to": "tx"},
        ]}),
    )
    .await;
    let g = r.ok("get_graph", json!({})).await;
    assert!(g["tx_note"].is_string(), "{g}");
    let st = r.ok("run", json!({})).await;
    assert_ne!(st["state"], "running", "{st}");
    assert!(!st["errors"].as_array().unwrap().is_empty(), "{st}");
    assert!(r.session.monitor().is_none());
}

/// A file of raster records: one 16×8 RGB picture.
fn picture_file(dir: &Path) -> PathBuf {
    let mut bytes = Vec::new();
    for row in 0..8u16 {
        let r = Row {
            picture: 1,
            row,
            height: 8,
            width: 16,
            channels: 3,
            flags: if row == 7 { LAST } else { 0 },
        };
        bytes.extend(r.header());
        bytes.extend((0..16).flat_map(|x| [x as u8 * 16, row as u8 * 32, 128]));
    }
    let path = dir.join("picture.raw");
    std::fs::write(&path, bytes).unwrap();
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_and_pictures() {
    let r = rig("watch");
    let lines = r.dir.join("lines.txt");
    std::fs::write(&lines, "t=1 hello\nt=2 world\n").unwrap();
    let pic = picture_file(&r.dir);
    let rep = r
        .ok(
            "edit_graph",
            json!({"operations": [
                {"op": "add_block", "id": "text", "kind": "file_source",
                 "params": {"type": "u8", "path": lines.to_str().unwrap(), "repeat": true, "sample_rate": 2000}},
                {"op": "add_block", "id": "t_slow", "kind": "throttle", "params": {"type": "u8"}},
                {"op": "add_block", "id": "log", "kind": "message_log"},
                {"op": "connect", "from": "text", "to": "t_slow"},
                {"op": "connect", "from": "t_slow", "to": "log"},
                {"op": "add_block", "id": "pic", "kind": "file_source",
                 "params": {"type": "u8", "path": pic.to_str().unwrap(), "repeat": true, "sample_rate": 4000}},
                {"op": "add_block", "id": "p_slow", "kind": "throttle", "params": {"type": "u8"}},
                {"op": "add_block", "id": "sink", "kind": "null_sink", "params": {"type": "u8"}},
                {"op": "connect", "from": "pic", "to": "p_slow"},
                {"op": "connect", "from": "p_slow", "to": "sink"},
            ]}),
        )
        .await;
    assert_eq!(rep["builds"], true, "{rep}");
    // Watches can start before the run.
    let w = r
        .ok("watch", json!({"wire": "t_slow.out", "kind": "messages"}))
        .await;
    let wid = w["watch_id"].as_u64().unwrap();
    let img = r
        .ok(
            "watch",
            json!({"wire": "p_slow.out->sink.in", "kind": "image"}),
        )
        .await;
    let iid = img["watch_id"].as_u64().unwrap();
    r.ok("run", json!({})).await;

    let m = r
        .ok(
            "measure",
            json!({"wire": "t_slow.out", "kind": "messages", "seconds": 0.5}),
        )
        .await;
    let got: Vec<&str> = m["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(
        got.contains(&"t=1 hello") && got.contains(&"t=2 world"),
        "{m}"
    );

    tokio::time::sleep(Duration::from_millis(600)).await;
    let first = r.ok("read_watch", json!({"watch_id": wid})).await;
    assert!(!first["lines"].as_array().unwrap().is_empty(), "{first}");
    // Reading takes the lines: the next read only has new ones.
    let second = r
        .ok("read_watch", json!({"watch_id": wid, "max_lines": 1000}))
        .await;
    assert!(
        second["lines"].as_array().unwrap().len() < first["lines"].as_array().unwrap().len() + 40,
        "{second}"
    );

    let (err, body, raw) = r.tool("read_watch", json!({"watch_id": iid})).await;
    assert!(!err, "{body}");
    assert_eq!(body["pictures"][0]["width"], 16, "{body}");
    assert_eq!(body["pictures"][0]["finished"], true, "{body}");
    let image = &raw["content"][1];
    assert_eq!(image["type"], "image");
    assert_eq!(image["mimeType"], "image/png");
    assert!(
        image["data"].as_str().unwrap().starts_with("iVBORw0KGgo"),
        "a PNG signature"
    );

    r.ok("stop_watch", json!({"watch_id": wid})).await;
    let e = r.fails("read_watch", json!({"watch_id": wid})).await;
    assert!(e.contains("no watch"), "{e}");
    r.ok("stop", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn files_stay_in_the_project() {
    let r = rig("files");
    tone_graph(&r).await;
    r.ok("save_file", json!({"name": "tone.fox.toml"})).await;
    assert!(r.dir.join("tone.fox.toml").is_file());
    assert_eq!(r.session.name().as_deref(), Some("tone.fox.toml"));
    let e = r.fails("save_file", json!({"name": "tone.fox.toml"})).await;
    assert!(e.contains("overwrite"), "{e}");
    for bad in ["../evil.fox.toml", "/tmp/x.fox.toml", "x.txt"] {
        r.fails("save_file", json!({"name": bad})).await;
        r.fails("open_file", json!({"name": bad})).await;
    }
    let files = r.ok("list_files", json!({})).await;
    assert_eq!(files, json!(["tone.fox.toml"]));
    r.ok("set_graph", json!({"toml": ""})).await;
    assert!(r.session.graph().blocks.is_empty());
    let rep = r.ok("open_file", json!({"name": "tone.fox.toml"})).await;
    assert_eq!(rep["builds"], true, "{rep}");
    assert_eq!(r.session.graph().blocks.len(), 3);

    // GNU Radio round trip.
    let exp = r.ok("export_grc", json!({})).await;
    let grc = exp["grc"].as_str().unwrap();
    assert!(grc.contains("analog_sig_source_x"), "{grc}");
    let imp = r.ok("import_grc", json!({"grc": grc})).await;
    assert!(
        imp["toml"].as_str().unwrap().contains("signal_source"),
        "{imp}"
    );
}
