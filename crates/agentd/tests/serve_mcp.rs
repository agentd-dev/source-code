//! Composability E2E: a peer drives agentd's served self-MCP. RFC 0005. Runs
//! only under `cargo test --features serve-mcp`.
#![cfg(all(unix, feature = "serve-mcp"))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// Connect once the daemon has bound the socket (poll up to ~3s).
fn connect(path: &std::path::Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(s) = UnixStream::connect(path) {
            return s;
        }
        if Instant::now() >= deadline {
            panic!("served MCP socket never became connectable: {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Send one JSON-RPC line and read the one-line response.
fn rpc(reader: &mut BufReader<UnixStream>, write: &mut UnixStream, line: &str) -> serde_json::Value {
    writeln!(write, "{line}").expect("write rpc");
    write.flush().ok();
    let mut buf = String::new();
    reader.read_line(&mut buf).expect("read rpc reply");
    serde_json::from_str(&buf).expect("reply is json")
}

#[test]
fn a_peer_initializes_lists_and_calls_status() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("agentd.sock");

    // A loop-mode daemon serves the self-MCP while it runs (intel is unreachable,
    // but the daemon stays up and keeps serving the socket).
    let mut child = Command::new(exe)
        .args([
            "--mode", "loop", "--interval", "10s", "--instruction", "x", "--intelligence",
            "unix:/nonexistent.sock", "--serve-mcp",
        ])
        .arg(format!("unix:{}", sock.display()))
        .args(["--log-level", "warn"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agentd loop --serve-mcp");

    let stream = connect(&sock);
    let mut write = stream.try_clone().expect("clone stream");
    let mut reader = BufReader::new(stream);

    // initialize
    let init = rpc(
        &mut reader,
        &mut write,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}"#,
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "agentd", "init: {init}");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    // tools/list advertises `status`
    let list = rpc(&mut reader, &mut write, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    assert_eq!(list["result"]["tools"][0]["name"], "status", "list: {list}");

    // tools/call status returns this daemon's live state
    let status = rpc(
        &mut reader,
        &mut write,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status"}}"#,
    );
    assert_eq!(status["result"]["isError"], false, "status: {status}");
    assert_eq!(status["result"]["structuredContent"]["mode"], "loop");
    assert!(status["result"]["structuredContent"]["uptime_ms"].is_number());

    // an unknown tool is a JSON-RPC error, not a panic
    let bad = rpc(
        &mut reader,
        &mut write,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ghost"}}"#,
    );
    assert!(bad["error"].is_object(), "bad tool should error: {bad}");

    // subagent.spawn delegates a real run. The spawned agent fails on the
    // unreachable intel → a tool-domain error (isError:true), not a JSON-RPC
    // error — proving delegation reaches supervise_once + the result mapping.
    let spawn = rpc(
        &mut reader,
        &mut write,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{"instruction":"do a thing"}}}"#,
    );
    assert_eq!(spawn["result"]["isError"], true, "spawn (unreachable intel) is a tool error: {spawn}");
    assert!(
        spawn["result"]["content"][0]["text"].as_str().unwrap_or("").contains("intel"),
        "spawn error should mention intel: {spawn}"
    );

    // a malformed subagent.spawn (no instruction) is a JSON-RPC error
    let bad_spawn = rpc(
        &mut reader,
        &mut write,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{}}}"#,
    );
    assert!(bad_spawn["error"].is_object(), "missing instruction → JSON-RPC error: {bad_spawn}");

    sigterm(child.id());
    let _ = child.wait();
}
