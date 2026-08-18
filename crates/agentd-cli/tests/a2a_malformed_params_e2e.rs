// SPDX-License-Identifier: Apache-2.0
//! **Malformed `SendMessage` params must not be able to kill the daemon.**
//!
//! `params` is remote input, and the listener rewrites it before handing it to
//! the protocol layer (a task id for a send that names none). That rewrite used
//! to index into the value with serde_json's `IndexMut`, which *panics* — not
//! errors — when what is underneath is an array, a string or a number. The
//! release profile is `panic = "abort"`, so a single `curl` with
//! `"params": []` was a remote kill switch for the whole daemon.
//!
//! Two things are asserted for each malformed shape: the caller gets a
//! JSON-RPC error, and — the half that matters — the daemon is still running and
//! still answering afterwards. In a debug build `panic = "unwind"` confines the
//! panic to the hyper task, so a test that only read the response would pass
//! against the bug it exists to catch.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// A free loopback port (bind :0, read the port, drop). A tiny TOCTOU window —
/// agentd rebinds within milliseconds.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One HTTP POST of a JSON-RPC body, returning the response body — or the
/// transport failure as `Err`, because "the listener hung up mid-request" is
/// exactly what the panic looked like from outside and must be reported as a
/// failed assertion rather than an unwrap in the harness.
fn post_raw(addr: &str, body: &str) -> Result<String, String> {
    let mut s = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    s.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    if reader.read_line(&mut status).map_err(|e| e.to_string())? == 0 {
        return Err("the listener closed the connection without a response".into());
    }
    loop {
        let mut l = String::new();
        if reader.read_line(&mut l).map_err(|e| e.to_string())? == 0 {
            break;
        }
        if l.trim().is_empty() {
            break;
        }
    }
    let mut b = String::new();
    reader.read_to_string(&mut b).map_err(|e| e.to_string())?;
    Ok(b)
}

/// A JSON-RPC call whose `params` are handed over verbatim — the point of this
/// suite is the shapes a typed client could never produce.
fn post_rpc(addr: &str, id: i64, method: &str, params: Value) -> Result<Value, String> {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let resp = post_raw(addr, &body)?;
    serde_json::from_str(&resp).map_err(|e| format!("non-JSON response ({e}): {resp:?}"))
}

fn wait_ready(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a2a listener never became connectable"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    /// Whether the process is still running. `panic = "abort"` in the release
    /// profile turns the listener's panic into a dead process, so this is the
    /// assertion the defect is really about.
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        // Graceful drain first; SIGKILL only if it lingers.
        sigterm(self.child.id());
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

fn spawn_daemon(config: &str) -> Daemon {
    let stderr_path = common::unique_path("a2a-malformed-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd a2a daemon");
    Daemon { child, stderr_path }
}

/// A daemon serving A2A over plaintext loopback (⇒ operator). The intelligence
/// endpoint is deliberately dead: no turn is ever run here, and `preflight:
/// never` means nothing dials it, which keeps the test to one process. The
/// interface is armed because the observation feed is the other surface a
/// caller-supplied cursor is handed to.
fn config(port: u16) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: a2a-malformed\n  instruction: You are a test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: https://127.0.0.1:9\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         interface:\n  enabled: true\n  debug: false\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

/// Open a `SubscribeToEvents` stream from `from_seq` and return the `hello`
/// frame, then hang up — the frame is the whole contract under test.
fn hello_frame(addr: &str, from_seq: u64) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 77, "method": "SubscribeToEvents",
                      "params": {"fromSeq": from_seq}})
    .to_string();
    let mut s = TcpStream::connect(addr).expect("connect sse");
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut reader = BufReader::new(s);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => panic!("the feed never sent a hello frame"),
            Ok(_) => {}
        }
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(v) = serde_json::from_str::<Value>(data.trim())
            && let Some(hello) = v["result"].get("hello")
        {
            return hello.clone();
        }
    }
}

#[test]
fn malformed_send_params_are_refused_and_the_daemon_keeps_serving() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg_path = common::unique_path("a2a-malformed", "yaml");
    std::fs::write(&cfg_path, config(port)).unwrap();
    let mut daemon = spawn_daemon(&cfg_path);
    wait_ready(&addr);

    // Every shape a `params` field can take that is not "an object with an
    // object `message`". The first three reached the `IndexMut` rewrite and
    // panicked the listener; `null` and the rest are here because the guard has
    // to hold for the whole space, not for the reported reproducer.
    let shapes = [
        json!([]),
        json!({"message": "hi"}),
        json!({"message": 3}),
        json!({"message": []}),
        json!("send"),
        json!(7),
        Value::Null,
    ];
    for (i, params) in shapes.iter().enumerate() {
        let id = 100 + i as i64;
        let v = post_rpc(&addr, id, "SendMessage", params.clone())
            .unwrap_or_else(|e| panic!("params {params} killed the request: {e}"));
        assert!(
            v.get("error").is_some(),
            "params {params} must be refused, not accepted: {v}"
        );
        assert!(
            v.get("result").is_none(),
            "params {params} must not produce a result: {v}"
        );
        // Still up after each one, so a failure names the shape that did it.
        assert!(
            daemon.alive(),
            "the daemon died on params {params}\nstderr:\n{}",
            daemon.stderr()
        );
    }

    // The half that actually distinguishes a contained error from a dead
    // listener: a well-formed request over a NEW connection is still answered.
    let status =
        json!({"message": {"messageId": "m1", "parts": [{"data": {"agentd": {"op": "status"}}}]}});
    let v = post_rpc(&addr, 1, "SendMessage", status)
        .unwrap_or_else(|e| panic!("the listener stopped answering after malformed input: {e}"));
    assert_eq!(
        v["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "a good request after the bad ones: {v}"
    );
    assert!(daemon.alive(), "the daemon survived the whole sequence");

    std::fs::remove_file(&cfg_path).ok();
}

#[test]
fn a_feed_cursor_ahead_of_the_feed_is_told_to_resync() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg_path = common::unique_path("a2a-malformed-feed", "yaml");
    std::fs::write(&cfg_path, config(port)).unwrap();
    let mut daemon = spawn_daemon(&cfg_path);
    wait_ready(&addr);

    // The control: a client starting from the beginning is caught up by replay,
    // not by re-bootstrapping.
    let fresh = hello_frame(&addr, 0);
    assert_eq!(fresh["resync"], false, "a zero cursor is honoured: {fresh}");

    // The regression: the feed is in memory, so every attached display client
    // reconnecting across a daemon restart presents a cursor from the *previous*
    // process — one far ahead of this feed's seq. Honouring it silently kills
    // the subscription (`since` only yields `seq > cursor`), so the client waits
    // out a whole restart's worth of events showing nothing. `resync` is how it
    // is told to re-bootstrap instead.
    let stale = hello_frame(&addr, 9_000_000);
    assert_eq!(
        stale["resync"], true,
        "a cursor past the end of the feed must resync: {stale}"
    );
    assert!(
        stale["seq"].as_u64().unwrap_or(u64::MAX) < 9_000_000,
        "the feed really is behind that cursor: {stale}"
    );
    assert!(
        daemon.alive(),
        "the daemon is still up: {}",
        daemon.stderr()
    );

    std::fs::remove_file(&cfg_path).ok();
}
