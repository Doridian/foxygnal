//! `foxygnal mcp` over stdio: requests in, one JSON reply per line out, and
//! nothing else on stdout, even from a block that prints there.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

#[test]
fn stdio_carries_only_the_protocol() {
    let dir = std::env::temp_dir().join(format!("foxygnal-mcp-stdio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("lines.txt"), "printed on stdout\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_foxygnal"))
        .args(["mcp", "--project-dir"])
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut send = |v: Value| {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    };
    send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
    );
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    // A Message Log that prints every line to stdout.
    let toml = format!(
        "[[block]]\nid = \"src\"\nkind = \"file_source\"\nparams = {{ type = \"u8\", path = {:?}, repeat = true, sample_rate = 500 }}\n\n\
         [[block]]\nid = \"slow\"\nkind = \"throttle\"\nparams = {{ type = \"u8\" }}\n\n\
         [[block]]\nid = \"log\"\nkind = \"message_log\"\nparams = {{ stdout = true }}\n\n\
         [[connect]]\nfrom = \"src.out\"\nto = \"slow.in\"\n\n[[connect]]\nfrom = \"slow.out\"\nto = \"log.in\"\n",
        dir.join("lines.txt").to_str().unwrap()
    );
    send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "set_graph", "arguments": {"toml": toml}}}),
    );
    send(
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "run", "arguments": {}}}),
    );
    std::thread::sleep(Duration::from_millis(800));
    send(
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "stop", "arguments": {}}}),
    );
    std::thread::sleep(Duration::from_millis(300));
    drop(stdin);

    let out = BufReader::new(child.stdout.take().unwrap());
    let mut ids = Vec::new();
    for line in out.lines() {
        let line = line.unwrap();
        let v: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout has a line that isn't JSON-RPC ({e}): {line}"));
        assert_eq!(v["jsonrpc"], "2.0");
        ids.push(v["id"].as_u64().unwrap());
        if v["id"] == 3 {
            assert_eq!(v["result"]["structuredContent"]["state"], "running", "{v}");
        }
    }
    assert!(child.wait().unwrap().success());
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3, 4]);
    // The printed lines went to stderr instead.
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    assert!(err.contains("printed on stdout"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
