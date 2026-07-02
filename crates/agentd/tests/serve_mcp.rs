// SPDX-License-Identifier: Apache-2.0
//! Composability E2E: a peer drives agentd's served self-MCP over the HTTP
//! control plane (pivot Phase 3). RFC 0005. Runs only under
//! `cargo test --features serve-https`.
#![cfg(all(unix, feature = "serve-https"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// Grab a free loopback port (bind :0 then drop). agentd rebinds within ms.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A daemon that just **idles and serves MCP** over loopback HTTP: reactive mode
/// subscribed to a URI no MCP server owns, so nothing contends for the
/// process-wide supervise lock and a served async run starts immediately.
/// Returns the child and the `host:port` the peer dials.
fn start_idle_daemon(exe: &str, intel: &str) -> (Child, String) {
    let port = free_port();
    let bind = format!("http://127.0.0.1:{port}");
    let child = Command::new(exe)
        .args([
            "--mode",
            "reactive",
            "--subscribe",
            "file:///noop",
            "--instruction",
            "stand by",
            "--intelligence",
            intel,
            "--serve-mcp",
            &bind,
            "--log-level",
            "warn",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn idle daemon");
    (child, format!("127.0.0.1:{port}"))
}

/// Start the built-in mock LLM on loopback TCP (addr-file handshake).
fn start_mock_llm(exe: &str, addr_file: &Path, script: &str) -> (Child, String) {
    let child = Command::new(exe)
        .args(["--internal-mock-llm", addr_file.to_str().unwrap(), script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !addr_file.exists() {
        assert!(Instant::now() < deadline, "mock-llm never announced");
        std::thread::sleep(Duration::from_millis(20));
    }
    let addr = std::fs::read_to_string(addr_file).expect("read mock-llm addr-file");
    (child, format!("http://{}", addr.trim()))
}

/// A served-MCP peer over HTTP: each JSON-RPC call is one `POST /mcp`
/// (Connection: close), and `listen` opens a `subscriptions/listen` SSE stream.
struct Peer {
    addr: String,
}

impl Peer {
    /// Connect once the daemon's HTTP port accepts (poll up to ~3s).
    fn connect(addr: &str) -> Peer {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if TcpStream::connect(addr).is_ok() {
                return Peer {
                    addr: addr.to_string(),
                };
            }
            assert!(Instant::now() < deadline, "served HTTP port never accepted: {addr}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// One JSON-RPC call over a fresh HTTP POST; returns the parsed reply.
    fn rpc(&self, body: &str) -> serde_json::Value {
        let mut s = TcpStream::connect(&self.addr).expect("connect peer");
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).expect("write rpc");
        s.flush().ok();
        let mut reader = BufReader::new(s);
        // Skip the status line + headers.
        loop {
            let mut l = String::new();
            if reader.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                break;
            }
        }
        let mut payload = String::new();
        reader.read_to_string(&mut payload).expect("read rpc body");
        serde_json::from_str(&payload).unwrap_or_else(|e| panic!("reply not json ({e}): {payload:?}"))
    }

    /// Open a modern `subscriptions/listen` SSE stream for `uris`; returns a
    /// channel of the pushed notification JSON values. The reader thread runs
    /// until the connection closes.
    fn listen(&self, uris: &[&str]) -> Receiver<serde_json::Value> {
        let subs: Vec<String> = uris.iter().map(|u| (*u).to_string()).collect();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "subscriptions/listen",
            "params": { "notifications": { "resourceSubscriptions": subs } }
        })
        .to_string();
        let mut s = TcpStream::connect(&self.addr).expect("connect listen");
        s.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).expect("write listen");
        s.flush().ok();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(s);
            // Skip headers.
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                    break;
                }
            }
            // Read SSE `data:` lines.
            loop {
                let mut l = String::new();
                match reader.read_line(&mut l) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Some(data) = l.strip_prefix("data:")
                            && let Ok(v) = serde_json::from_str::<serde_json::Value>(data.trim())
                            && tx.send(v).is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        rx
    }
}

/// Poll `subagent.status` for `handle` until the run is `done`, or the deadline.
fn poll_until_done(peer: &Peer, handle: &str, deadline: Instant) -> serde_json::Value {
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{{"name":"subagent.status","arguments":{{"handle":"{handle}"}}}}}}"#
    );
    loop {
        let v = peer.rpc(&line);
        let body = v["result"]["structuredContent"].clone();
        if body["done"] == true {
            return body;
        }
        assert!(Instant::now() < deadline, "status never reached done: {v}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_peer_initializes_lists_and_calls_status() {
    let exe = env!("CARGO_BIN_EXE_agentd");

    // A loop-mode daemon serves the self-MCP over loopback HTTP while it runs
    // (intel is unreachable, but the daemon stays up and keeps serving).
    let port = free_port();
    let bind = format!("http://127.0.0.1:{port}");
    let addr = format!("127.0.0.1:{port}");
    let mut child = Command::new(exe)
        .args([
            "--mode",
            "loop",
            "--interval",
            "10s",
            "--instruction",
            "x",
            "--intelligence",
            "http://127.0.0.1:9",
            "--serve-mcp",
            &bind,
            "--log-level",
            "warn",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agentd loop --serve-mcp");

    let peer = Peer::connect(&addr);

    // initialize
    let init = peer.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}"#,
    );
    assert_eq!(
        init["result"]["serverInfo"]["name"], "agentd",
        "init: {init}"
    );
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(
        init["result"]["capabilities"]["resources"].is_object(),
        "resources capability: {init}"
    );

    // tools/list advertises `status`
    let list = peer.rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    assert_eq!(list["result"]["tools"][0]["name"], "status", "list: {list}");

    // tools/call status returns this daemon's live state
    let status = peer.rpc(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status"}}"#,
    );
    assert_eq!(status["result"]["isError"], false, "status: {status}");
    assert_eq!(status["result"]["structuredContent"]["mode"], "loop");
    assert!(status["result"]["structuredContent"]["uptime_ms"].is_number());

    // an unknown tool is a JSON-RPC error, not a panic
    let bad = peer.rpc(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ghost"}}"#,
    );
    assert!(bad["error"].is_object(), "bad tool should error: {bad}");

    // subagent.spawn delegates a real run. The spawned agent fails on the
    // unreachable intel → a tool-domain error (isError:true), not a JSON-RPC
    // error — proving delegation reaches supervise_once + the result mapping.
    let spawn = peer.rpc(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{"instruction":"do a thing"}}}"#,
    );
    assert_eq!(
        spawn["result"]["isError"], true,
        "spawn (unreachable intel) is a tool error: {spawn}"
    );
    assert!(
        spawn["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("intel"),
        "spawn error should mention intel: {spawn}"
    );

    // a malformed subagent.spawn (no instruction) is a JSON-RPC error
    let bad_spawn = peer.rpc(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{}}}"#,
    );
    assert!(
        bad_spawn["error"].is_object(),
        "missing instruction → JSON-RPC error: {bad_spawn}"
    );

    // resources/list advertises the agentd:// surface
    let res_list = peer.rpc(r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#,
    );
    assert_eq!(
        res_list["result"]["resources"][0]["uri"], "agent://status",
        "resources/list: {res_list}"
    );

    // resources/read agentd://status returns a contents body with the live state
    let res_read = peer.rpc(r#"{"jsonrpc":"2.0","id":8,"method":"resources/read","params":{"uri":"agentd://status"}}"#,
    );
    let entry = &res_read["result"]["contents"][0];
    assert_eq!(
        entry["uri"], "agentd://status",
        "resources/read: {res_read}"
    );
    assert_eq!(entry["mimeType"], "application/json");
    let body: serde_json::Value =
        serde_json::from_str(entry["text"].as_str().expect("text")).expect("json body");
    assert_eq!(
        body["mode"], "loop",
        "served status body reflects the daemon mode: {body}"
    );

    // an unknown agentd:// uri is a JSON-RPC error
    let bad_read = peer.rpc(r#"{"jsonrpc":"2.0","id":9,"method":"resources/read","params":{"uri":"agentd://ghost"}}"#,
    );
    assert!(
        bad_read["error"].is_object(),
        "unknown resource → JSON-RPC error: {bad_read}"
    );

    sigterm(child.id());
    let _ = child.wait();
}

#[test]
fn async_spawn_returns_a_handle_and_tracks_the_run() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    // intel unreachable → the served async run fails fast; we observe the
    // lifecycle (handle → running → failed) via the registry.
    let (mut child, addr) = start_idle_daemon(exe, "http://127.0.0.1:9");

    let peer = Peer::connect(&addr);
    peer.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );

    // async spawn → a handle immediately, status running (NON-blocking).
    let spawn = peer.rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{"instruction":"do a thing","async":true}}}"#,
    );
    assert_eq!(spawn["result"]["isError"], false, "async spawn ok: {spawn}");
    let sc = &spawn["result"]["structuredContent"];
    assert_eq!(sc["status"], "running", "starts running: {spawn}");
    let handle = sc["handle"].as_str().expect("handle").to_string();

    // poll the registry until the run terminates → failed (intel unreachable)
    let body = poll_until_done(&peer, &handle,
        Instant::now() + Duration::from_secs(20),
    );
    assert_eq!(
        body["status"], "failed",
        "intel-unreachable async run → failed: {body}"
    );

    // the same run is readable as an agentd:// resource
    let read = peer.rpc(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{{"uri":"agentd://subagent/{handle}"}}}}"#
        ),
    );
    let rbody: serde_json::Value = serde_json::from_str(
        read["result"]["contents"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    assert_eq!(rbody["handle"], handle.as_str());
    assert_eq!(
        rbody["status"], "failed",
        "resource read matches status: {rbody}"
    );

    sigterm(child.id());
    let _ = child.wait();
}

/// Block until a `notifications/resources/updated` for `uri` arrives on the SSE
/// channel (or the deadline). Skips other notifications.
fn recv_resource_updated(rx: &Receiver<serde_json::Value>, uri: &str, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(v) => {
                if v["method"] == "notifications/resources/updated" && v["params"]["uri"] == uri {
                    return true;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return false, // stream closed
        }
    }
    false
}

#[test]
fn a_peer_is_pushed_a_notification_when_a_subscribed_run_completes() {
    // The reactive loop closed: a peer opens a subscriptions/listen SSE stream for
    // agentd://subagent/<handle> and is PUSHED notifications/resources/updated when
    // that run terminates — no polling. (We cancel a hanging run to terminate it.)
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let llm_sock = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(exe, &llm_sock, "hang");
    let (mut child, addr) = start_idle_daemon(exe, &intel);

    let peer = Peer::connect(&addr);
    let init = peer.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    assert_eq!(
        init["result"]["capabilities"]["resources"]["subscribe"], true,
        "subscribe advertised: {init}"
    );

    let spawn = peer.rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{"instruction":"slow","async":true}}}"#);
    let handle = spawn["result"]["structuredContent"]["handle"]
        .as_str()
        .expect("handle")
        .to_string();
    let uri = format!("agent://subagent/{handle}");

    // Open the SSE notification stream for the run's resource, then cancel it.
    let rx = peer.listen(&[&uri]);
    peer.rpc(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"subagent.cancel","arguments":{{"handle":"{handle}"}}}}}}"#
    ));

    // The run drains (~5-7s) → its resource changed → we are pushed an update.
    assert!(
        recv_resource_updated(&rx, &uri, Instant::now() + Duration::from_secs(20)),
        "expected a pushed notifications/resources/updated for {uri}"
    );

    let _ = llm.kill();
    let _ = llm.wait();
    sigterm(child.id());
    let _ = child.wait();
}

/// Poll `subagent.status` until the warm session has run at least `target` turns.
fn poll_warm_turns(peer: &Peer, handle: &str, target: u64, deadline: Instant) -> u64 {
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{{"name":"subagent.status","arguments":{{"handle":"{handle}"}}}}}}"#
    );
    loop {
        let v = peer.rpc(&line);
        let turns = v["result"]["structuredContent"]["turns"]
            .as_u64()
            .unwrap_or(0);
        if turns >= target {
            return turns;
        }
        assert!(
            Instant::now() < deadline,
            "warm session never reached {target} turns: {v}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_warm_session_runs_a_turn_per_send() {
    // Bidirectional composability: subagent.spawn warm=true keeps the agent alive;
    // each subagent.send runs another turn over the SAME conversation.
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let llm_sock = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(exe, &llm_sock, "final"); // each turn completes at once
    let (mut child, addr) = start_idle_daemon(exe, &intel);

    let peer = Peer::connect(&addr);
    peer.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );

    // warm spawn → a live session (turn 1 runs from the instruction).
    let spawn = peer.rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{"instruction":"hello","warm":true}}}"#,
    );
    assert_eq!(
        spawn["result"]["structuredContent"]["warm"], true,
        "warm session: {spawn}"
    );
    let handle = spawn["result"]["structuredContent"]["handle"]
        .as_str()
        .expect("handle")
        .to_string();

    let t1 = poll_warm_turns(&peer, &handle,
        1,
        Instant::now() + Duration::from_secs(15),
    );
    assert!(t1 >= 1, "the warm session runs turn 1 from the instruction");

    // send another message → a SECOND turn over the same live session.
    let sent = peer.rpc(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"subagent.send","arguments":{{"handle":"{handle}","message":"and again"}}}}}}"#
        ),
    );
    assert_eq!(
        sent["result"]["structuredContent"]["delivered"], true,
        "send delivered: {sent}"
    );
    // The peer is told which turn index to wait for (turn 1 already drained by the
    // status polls above, so this send produces turn 2).
    assert_eq!(
        sent["result"]["structuredContent"]["awaiting_turn"], 2,
        "send reports the awaited turn: {sent}"
    );

    let t2 = poll_warm_turns(&peer, &handle,
        2,
        Instant::now() + Duration::from_secs(15),
    );
    assert!(
        t2 >= 2,
        "the SAME session ran a second turn from the injected message"
    );

    // Once the turn completes the session reads idle (not busy) — the peer's signal
    // that last_result is fresh and it is safe to send again.
    let status = peer.rpc(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{"name":"subagent.status","arguments":{{"handle":"{handle}"}}}}}}"#
        ),
    );
    assert_eq!(
        status["result"]["structuredContent"]["busy"], false,
        "idle after the turn drains: {status}"
    );

    // end it.
    let cancel = peer.rpc(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"subagent.cancel","arguments":{{"handle":"{handle}"}}}}}}"#
        ),
    );
    assert_eq!(
        cancel["result"]["structuredContent"]["cancelled"], true,
        "warm session cancelled: {cancel}"
    );

    let _ = llm.kill();
    let _ = llm.wait();
    sigterm(child.id());
    let _ = child.wait();
}

#[test]
fn concurrent_async_runs_do_not_serialize() {
    // Two async runs in flight at once. The second is cancelled and must drain
    // PROMPTLY *while the first is still supervising its (hanging) run*. Before
    // the single-reaper refactor, run 2's reactor would be blocked on the
    // process-wide SUPERVISE_LOCK held by run 1's ~12s hang, so it could not even
    // observe the cancel until run 1 finished — by which point run 1 would be done
    // too. Run 2 finishing cancelled while run 1 is still running proves the two
    // supervisors run concurrently.
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let llm_sock = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(exe, &llm_sock, "hang");
    let (mut child, addr) = start_idle_daemon(exe, &intel);

    let peer = Peer::connect(&addr);
    peer.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );

    let spawn_async = |id: u32| -> String {
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"subagent.spawn","arguments":{{"instruction":"slow","async":true}}}}}}"#
        );
        let v = peer.rpc(&line);
        v["result"]["structuredContent"]["handle"]
            .as_str()
            .expect("handle")
            .to_string()
    };

    let h1 = spawn_async(2);
    let h2 = spawn_async(3);
    assert_ne!(h1, h2);

    // Let both runs reach their hanging model call, then cancel only run 2.
    std::thread::sleep(Duration::from_millis(400));
    peer.rpc(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"subagent.cancel","arguments":{{"handle":"{h2}"}}}}}}"#
        ),
    );

    // Run 2 reaches a terminal "cancelled" promptly (drain ladder ~5-7s) —
    // well inside the 12s hang of run 1.
    let body2 = poll_until_done(&peer, &h2,
        Instant::now() + Duration::from_secs(15),
    );
    assert_eq!(body2["status"], "cancelled", "run 2 drained: {body2}");

    // ...and run 1 is STILL running at that moment (not blocked, not finished) —
    // the two supervisors ran concurrently.
    let status1 = peer.rpc(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{"name":"subagent.status","arguments":{{"handle":"{h1}"}}}}}}"#
        ),
    );
    assert_eq!(
        status1["result"]["structuredContent"]["done"], false,
        "run 1 must still be running while run 2 was cancelled (concurrent supervision): {status1}"
    );

    let _ = llm.kill();
    let _ = llm.wait();
    sigterm(child.id());
    let _ = child.wait();
}

#[test]
fn cancel_drains_a_live_async_run() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let llm_sock = dir.path().join("llm.addr");
    // `hang`: the run's child blocks ~30s in the model call, so reaching a
    // terminal state quickly proves the cancel/drain torn it down (not natural
    // completion).
    let (mut llm, intel) = start_mock_llm(exe, &llm_sock, "hang");
    let (mut child, addr) = start_idle_daemon(exe, &intel);

    let peer = Peer::connect(&addr);
    peer.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );

    let spawn = peer.rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"subagent.spawn","arguments":{"instruction":"do a slow thing","async":true}}}"#,
    );
    let handle = spawn["result"]["structuredContent"]["handle"]
        .as_str()
        .expect("handle")
        .to_string();

    // Let the run reach its (hanging) model call, then cancel it.
    std::thread::sleep(Duration::from_millis(400));
    let cancel = peer.rpc(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"subagent.cancel","arguments":{{"handle":"{handle}"}}}}}}"#
        ),
    );
    assert_eq!(
        cancel["result"]["structuredContent"]["cancelled"], true,
        "cancel accepted: {cancel}"
    );

    // It reaches a terminal "cancelled" state well before the 30s hang → the
    // reactor's per-run cancel token drained the live subtree.
    let body = poll_until_done(&peer, &handle,
        Instant::now() + Duration::from_secs(20),
    );
    assert_eq!(
        body["status"], "cancelled",
        "a cancelled live run is reported cancelled: {body}"
    );

    let _ = llm.kill();
    let _ = llm.wait();
    sigterm(child.id());
    let _ = child.wait();
}

/// RFC 0015 chunk 3: the operator surface over the unix MANAGEMENT transport.
/// A unix peer is `PeerOrigin::Management`, so it sees + can call the operator
/// tools (drain / lame-duck / cancel) and read `agentd://inventory`.
#[test]
fn management_peer_drives_the_operator_surface() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    // An idle reactive daemon that just serves the socket (intel unreachable; it
    // never reacts, so nothing contends with the management calls).
    let (mut child, addr) = start_idle_daemon(exe, "http://127.0.0.1:9");

    let peer = Peer::connect(&addr);
    peer.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );

    // tools/list to a management peer includes the operator tools.
    let list = peer.rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for t in ["drain", "lame-duck", "pause", "resume", "cancel"] {
        assert!(names.contains(&t), "management sees {t}: {names:?}");
    }

    // agentd://inventory is readable + carries the lifecycle flags.
    let inv = peer.rpc(r#"{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"agentd://inventory"}}"#,
    );
    let body: serde_json::Value =
        serde_json::from_str(inv["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["paused"], false, "not paused at startup: {body}");
    assert_eq!(body["draining"], false);
    assert_eq!(body["ready"], true);
    assert!(body["totals"]["total_spawned"].is_number());

    // pause flips the instance-wide pause flag in the projection (no in-flight
    // subagents on this idle daemon → affected:0) and is reversible. NOT a drain
    // and NOT a lame-duck: readiness is unchanged by pause (RFC 0015 §4.3).
    let pause = peer.rpc(r#"{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"pause"}}"#,
    );
    assert_eq!(pause["result"]["isError"], false, "pause: {pause}");
    assert_eq!(pause["result"]["structuredContent"]["paused"], true);
    assert_eq!(pause["result"]["structuredContent"]["affected"], 0);
    let inv_p: serde_json::Value = serde_json::from_str(
        peer.rpc(r#"{"jsonrpc":"2.0","id":32,"method":"resources/read","params":{"uri":"agentd://inventory"}}"#,
        )["result"]["contents"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        inv_p["paused"], true,
        "pause reflected in inventory: {inv_p}"
    );
    assert_eq!(inv_p["ready"], true, "pause is not lame-duck → still ready");
    assert_eq!(inv_p["draining"], false, "pause is not drain");
    // resume clears it.
    let resume = peer.rpc(r#"{"jsonrpc":"2.0","id":33,"method":"tools/call","params":{"name":"resume"}}"#,
    );
    assert_eq!(resume["result"]["structuredContent"]["paused"], false);
    let inv_r: serde_json::Value = serde_json::from_str(
        peer.rpc(r#"{"jsonrpc":"2.0","id":34,"method":"resources/read","params":{"uri":"agentd://inventory"}}"#,
        )["result"]["contents"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(inv_r["paused"], false, "resume cleared the flag: {inv_r}");

    // lame-duck flips readiness in the projection (no exit, no drain).
    let ld = peer.rpc(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"lame-duck"}}"#,
    );
    assert_eq!(ld["result"]["isError"], false, "lame-duck: {ld}");
    assert_eq!(ld["result"]["structuredContent"]["ready"], false);
    let inv2 = peer.rpc(r#"{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"agentd://inventory"}}"#,
    );
    let body2: serde_json::Value =
        serde_json::from_str(inv2["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        body2["ready"], false,
        "lame-duck reflected in inventory: {body2}"
    );

    // cancel of an unknown handle is an isError result, not a protocol error.
    let bad = peer.rpc(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"cancel","arguments":{"handle":"0.9.9"}}}"#,
    );
    assert!(
        bad["error"].is_null(),
        "cancel is a result, not an error: {bad}"
    );
    assert_eq!(bad["result"]["isError"], true);

    // drain returns a snapshot immediately and latches draining; the daemon then
    // winds down and exits clean. (Tested last so the daemon can exit.)
    let drain = peer.rpc(r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"drain"}}"#,
    );
    assert_eq!(drain["result"]["isError"], false, "drain: {drain}");
    assert_eq!(drain["result"]["structuredContent"]["draining"], true);
    assert!(drain["result"]["structuredContent"]["drain_timeout_ms"].is_number());

    // The drain drove a graceful shutdown — the daemon exits 0 on its own (no
    // SIGTERM needed), proving `drain` reuses the SIGTERM choreography.
    let code = wait_for_exit(&mut child, Duration::from_secs(15));
    assert_eq!(code, Some(0), "a tool-driven drain exits clean 0");
}
// NOTE: the `Stdio`-origin containment (a stdio peer can't see/call the operator
// tools, §3.4) is covered exhaustively by the server unit tests, which dispatch
// with `PeerOrigin::Stdio` directly. There is no `--serve-mcp stdio` CLI form to
// drive it end-to-end (the served transports are unix/vsock only), so it isn't
// re-tested here.

/// RFC 0020 §3 loopback: ONE agentd delegating to ANOTHER over A2A. A "server"
/// agentd serves its A2A surface over a unix socket (with the mock LLM as its
/// intelligence, so a served Task COMPLETES → a distillate). A "client" agentd
/// runs a one-shot whose mock LLM calls the `a2a.delegate` self-tool against a
/// declared peer pointing at the server. This exercises the whole A2A client path
/// end to end — connect → SendMessage → poll GetTask → distillate — and proves
/// the "agentd-as-A2A-client ↔ agentd-as-A2A-server" composition. Then we read
/// the SERVER's task registry to confirm a COMPLETED task actually landed there.
#[cfg(feature = "a2a")]
#[test]
fn one_agentd_delegates_to_another_over_a2a() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");

    // The server agentd: a mock LLM it can reach, and its A2A surface served over
    // loopback HTTP. A loop-mode daemon stays up and keeps serving while idle.
    let srv_llm = dir.path().join("srv-llm.addr");
    let (mut srv_llm_proc, srv_intel) = start_mock_llm(exe, &srv_llm, "final");
    let srv_port = free_port();
    let srv_bind = format!("http://127.0.0.1:{srv_port}");
    let srv_addr = format!("127.0.0.1:{srv_port}");
    let mut server = Command::new(exe)
        .args([
            "--mode",
            "loop",
            "--interval",
            "60s",
            "--instruction",
            "serve a2a",
            "--intelligence",
        ])
        .arg(&srv_intel)
        .arg("--serve-mcp")
        .arg(&srv_bind)
        .args(["--log-level", "warn"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server agentd");
    // Ensure the server's HTTP port accepts before the client dials.
    let peer = Peer::connect(&srv_addr);

    // The client agentd: a one-shot run whose mock LLM script calls a2a.delegate
    // against a peer that points at the server's A2A HTTP surface. Its own
    // intelligence is the `a2a-delegate` mock; the peer is the server.
    let cli_llm = dir.path().join("cli-llm.addr");
    let (mut cli_llm_proc, cli_intel) = start_mock_llm(exe, &cli_llm, "a2a-delegate");
    let client = Command::new(exe)
        .args(["--mode", "once", "--instruction", "delegate the work"])
        .arg("--intelligence")
        .arg(&cli_intel)
        .arg("--a2a-peer")
        .arg(format!("peer={srv_bind}"))
        .args(["--log-level", "warn"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("run client agentd");

    assert!(
        client.status.success(),
        "client run exited non-zero: {:?}",
        client.status.code()
    );
    let out = String::from_utf8_lossy(&client.stdout);
    assert!(
        out.contains("delegated over a2a"),
        "client should answer after a successful delegation; stdout: {out}"
    );

    // Confirm the delegation actually reached the SERVER: its A2A task registry
    // now holds a COMPLETED task (the served run the client's SendMessage started).
    let list = peer.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"a2a.ListTasks"}"#);
    let tasks = list["result"]["tasks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !tasks.is_empty(),
        "the server should have a task from the delegation: {list}"
    );
    let completed = tasks.iter().any(|t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
            && t["artifacts"][0]["parts"][0]["text"]
                .as_str()
                .is_some_and(|s| s.contains("mock-llm done"))
    });
    assert!(
        completed,
        "the server's delegated task completed with the distillate: {tasks:?}"
    );

    sigterm(server.id());
    let _ = server.wait();
    let _ = srv_llm_proc.kill();
    let _ = srv_llm_proc.wait();
    let _ = cli_llm_proc.kill();
    let _ = cli_llm_proc.wait();
}

/// Wait up to `timeout` for `child` to exit; return its code (None on timeout).
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => return None,
        }
    }
}
