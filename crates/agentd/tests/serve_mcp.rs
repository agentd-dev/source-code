//! Composability E2E: a peer drives agentd's served self-MCP. RFC 0005. Runs
//! only under `cargo test --features serve-mcp`.
#![cfg(all(unix, feature = "serve-mcp"))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// A daemon that just **idles and serves MCP**: reactive mode subscribed to a URI
/// no MCP server owns, so there are no reactions / read-after-subscribe runs —
/// nothing contends for the process-wide supervise lock, so a served async run
/// starts immediately.
fn start_idle_daemon(exe: &str, intel: &str, sock: &Path) -> Child {
    Command::new(exe)
        .args([
            "--mode", "reactive", "--subscribe", "file:///noop", "--instruction", "stand by",
            "--intelligence", intel, "--serve-mcp",
        ])
        .arg(format!("unix:{}", sock.display()))
        .args(["--log-level", "warn"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn idle daemon")
}

/// Start the built-in mock LLM on a unix socket; wait until it binds.
fn start_mock_llm(exe: &str, sock: &Path, script: &str) -> Child {
    let child = Command::new(exe)
        .args(["--internal-mock-llm", sock.to_str().unwrap(), script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !sock.exists() {
        assert!(Instant::now() < deadline, "mock-llm never bound");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

/// Poll `subagent.status` for `handle` until the run is `done`, or the deadline.
fn poll_until_done(reader: &mut BufReader<UnixStream>, write: &mut UnixStream, handle: &str, deadline: Instant) -> serde_json::Value {
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{{"name":"subagent.status","arguments":{{"handle":"{handle}"}}}}}}"#
    );
    loop {
        let v = rpc(reader, write, &line);
        let body = v["result"]["structuredContent"].clone();
        if body["done"] == true {
            return body;
        }
        assert!(Instant::now() < deadline, "status never reached done: {v}");
        std::thread::sleep(Duration::from_millis(50));
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
    assert!(init["result"]["capabilities"]["resources"].is_object(), "resources capability: {init}");

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

    // resources/list advertises the agentd:// surface
    let res_list = rpc(&mut reader, &mut write, r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#);
    assert_eq!(res_list["result"]["resources"][0]["uri"], "agentd://status", "resources/list: {res_list}");

    // resources/read agentd://status returns a contents body with the live state
    let res_read = rpc(
        &mut reader,
        &mut write,
        r#"{"jsonrpc":"2.0","id":8,"method":"resources/read","params":{"uri":"agentd://status"}}"#,
    );
    let entry = &res_read["result"]["contents"][0];
    assert_eq!(entry["uri"], "agentd://status", "resources/read: {res_read}");
    assert_eq!(entry["mimeType"], "application/json");
    let body: serde_json::Value = serde_json::from_str(entry["text"].as_str().expect("text")).expect("json body");
    assert_eq!(body["mode"], "loop", "served status body reflects the daemon mode: {body}");

    // an unknown agentd:// uri is a JSON-RPC error
    let bad_read = rpc(
        &mut reader,
        &mut write,
        r#"{"jsonrpc":"2.0","id":9,"method":"resources/read","params":{"uri":"agentd://ghost"}}"#,
    );
    assert!(bad_read["error"].is_object(), "unknown resource → JSON-RPC error: {bad_read}");

    sigterm(child.id());
    let _ = child.wait();
}

#[test]
fn async_spawn_returns_a_handle_and_tracks_the_run() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("agentd.sock");
    // intel unreachable → the served async run fails fast; we observe the
    // lifecycle (handle → running → failed) via the registry.
    let mut child = start_idle_daemon(exe, "unix:/nonexistent.sock", &sock);

    let stream = connect(&sock);
    let mut write = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    rpc(&mut reader, &mut write, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    // async spawn → a handle immediately, status running (NON-blocking).
    let spawn = rpc(
        &mut reader,
        &mut write,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{"instruction":"do a thing","async":true}}}"#,
    );
    assert_eq!(spawn["result"]["isError"], false, "async spawn ok: {spawn}");
    let sc = &spawn["result"]["structuredContent"];
    assert_eq!(sc["status"], "running", "starts running: {spawn}");
    let handle = sc["handle"].as_str().expect("handle").to_string();

    // poll the registry until the run terminates → failed (intel unreachable)
    let body = poll_until_done(&mut reader, &mut write, &handle, Instant::now() + Duration::from_secs(20));
    assert_eq!(body["status"], "failed", "intel-unreachable async run → failed: {body}");

    // the same run is readable as an agentd:// resource
    let read = rpc(
        &mut reader,
        &mut write,
        &format!(r#"{{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{{"uri":"agentd://subagent/{handle}"}}}}"#),
    );
    let rbody: serde_json::Value =
        serde_json::from_str(read["result"]["contents"][0]["text"].as_str().expect("text")).expect("json");
    assert_eq!(rbody["handle"], handle.as_str());
    assert_eq!(rbody["status"], "failed", "resource read matches status: {rbody}");

    sigterm(child.id());
    let _ = child.wait();
}

#[test]
fn cancel_drains_a_live_async_run() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let llm_sock = dir.path().join("llm.sock");
    // `hang`: the run's child blocks ~30s in the model call, so reaching a
    // terminal state quickly proves the cancel/drain torn it down (not natural
    // completion).
    let mut llm = start_mock_llm(exe, &llm_sock, "hang");
    let sock = dir.path().join("agentd.sock");
    let mut child = start_idle_daemon(exe, &format!("unix:{}", llm_sock.display()), &sock);

    let stream = connect(&sock);
    let mut write = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    rpc(&mut reader, &mut write, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let spawn = rpc(
        &mut reader,
        &mut write,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{"instruction":"do a slow thing","async":true}}}"#,
    );
    let handle = spawn["result"]["structuredContent"]["handle"].as_str().expect("handle").to_string();

    // Let the run reach its (hanging) model call, then cancel it.
    std::thread::sleep(Duration::from_millis(400));
    let cancel = rpc(
        &mut reader,
        &mut write,
        &format!(r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"subagent.cancel","arguments":{{"handle":"{handle}"}}}}}}"#),
    );
    assert_eq!(cancel["result"]["structuredContent"]["cancelled"], true, "cancel accepted: {cancel}");

    // It reaches a terminal "cancelled" state well before the 30s hang → the
    // reactor's per-run cancel token drained the live subtree.
    let body = poll_until_done(&mut reader, &mut write, &handle, Instant::now() + Duration::from_secs(20));
    assert_eq!(body["status"], "cancelled", "a cancelled live run is reported cancelled: {body}");

    let _ = llm.kill();
    let _ = llm.wait();
    sigterm(child.id());
    let _ = child.wait();
}
