// SPDX-License-Identifier: Apache-2.0
//! The **notify-then-read must not run on the reactor thread** (RFC 0027 §5
//! `wait on: resource`).
//!
//! A `resources/updated` notification resolves suspended `wait resource` steps
//! by reading the resource back. That read is a network round trip bounded only
//! by the MCP server's patience — so doing it inline on the single-writer loop
//! hands one slow (or hostile) server the whole daemon: for the length of that
//! read no timer fires, no checkpoint is written, the inbox drain does not
//! progress and SIGTERM is not observed. Subscriptions ARE agentd's reactivity
//! story, so this is the hot path, not an edge.
//!
//! The regression this suite exists for: `on_resource_updated` called
//! `read_resource` inline. The test points a subscription at a mock MCP server
//! that sits on `resources/read` for [`READ_DELAY`], and asserts that a durable
//! `sleep` timer armed in another run fires (and its run completes) DURING that
//! window — both by wall clock and, timing-free, by log order: the timer-driven
//! run must finish BEFORE the delayed read lands. With the inline read the
//! reactor is parked in `recv_timeout`'s callee and both assertions fail.

mod common;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the mock sits on `resources/read`. Long enough that a stalled
/// reactor is unmistakable, short enough to keep the suite under ~15 s.
const READ_DELAY: Duration = Duration::from_secs(10);
/// The `sleep` the other run waits out — comfortably inside [`READ_DELAY`].
const TICK_SLEEP: &str = "1s";
/// The budget the timer-driven run has to finish in. Generous next to its 1 s
/// sleep (a loaded CI box may spend a second connecting to the mock), and still
/// half of [`READ_DELAY`], so "slow" cannot be confused with "stalled".
const REACTIVE_BUDGET: Duration = Duration::from_secs(5);
/// A wedged daemon must fail the test, not hang CI.
const HARD_TIMEOUT: Duration = Duration::from_secs(60);
/// The resource the workflow subscribes to.
const WATCHED: &str = "mock://watched";

// ---- a mock MCP server that is slow on `resources/read` ---------------------

/// Launch a minimal **Streamable HTTP** MCP server on loopback that serves one
/// resource, pushes exactly one `resources/updated` after a subscribe (on the
/// long-lived `GET` SSE stream), and takes `read_delay` to answer every
/// `resources/read`. Returns the endpoint agentd should dial.
///
/// It answers `initialize` with a **legacy** revision on purpose: the client
/// picks `resources/subscribe` (not the modern `subscriptions/listen`) from the
/// negotiated version, and that is the path this mock implements.
fn spawn_slow_read_mcp(read_delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock mcp");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    // A subscribe (on a POST) arms the one-shot push the open GET stream sends.
    let pending: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || serve_conn(conn, pending, read_delay));
        }
    });
    endpoint
}

/// One HTTP request per connection (the client sends `Connection: close`): a
/// `GET` is the notification stream, a `POST` is one JSON-RPC frame.
fn serve_conn(conn: TcpStream, pending: Arc<AtomicBool>, read_delay: Duration) {
    conn.set_read_timeout(Some(Duration::from_secs(120))).ok();
    let mut w = match conn.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut r = BufReader::new(conn);
    let Some((start, body)) = read_http(&mut r) else {
        return;
    };
    if start.starts_with("GET ") {
        serve_notifications(&mut w, &pending);
        return;
    }
    if start.starts_with("DELETE ") {
        let _ = w.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(id) = msg.get("id").cloned() else {
        // A notification POST (`notifications/initialized`) — nothing to answer.
        let _ =
            w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let (result, session) = match method {
        "initialize" => (
            json!({
                // Legacy on purpose — see `spawn_slow_read_mcp`.
                "protocolVersion": "2025-11-25",
                "capabilities": {"resources": {"subscribe": true, "listChanged": true}, "tools": {}},
                "serverInfo": {"name": "slow-read-mock", "version": "0"}
            }),
            true,
        ),
        "ping" => (json!({}), false),
        "tools/list" => (json!({"tools": []}), false),
        "prompts/list" => (json!({"prompts": []}), false),
        "resources/templates/list" => (json!({"resourceTemplates": []}), false),
        "resources/list" => (
            json!({"resources": [{"uri": WATCHED, "name": "watched"}]}),
            false,
        ),
        "resources/subscribe" => {
            pending.store(true, Ordering::SeqCst);
            (json!({}), false)
        }
        "resources/unsubscribe" => (json!({}), false),
        // The whole point: the read is slow. A reactor that waits for it inline
        // is doing nothing else for `read_delay`.
        "resources/read" => {
            std::thread::sleep(read_delay);
            (
                json!({"contents": [{"uri": WATCHED, "mimeType": "text/plain", "text": "the watched resource changed"}]}),
                false,
            )
        }
        other => {
            respond_json(
                &mut w,
                &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("no {other}")}}),
                false,
            );
            return;
        }
    };
    respond_json(
        &mut w,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
        session,
    );
}

/// Hold the `GET` SSE stream open and deliver the one-shot `resources/updated`
/// a subscribe armed. No keep-alives: the client polls its stop flag between
/// events, and a comment stream would keep its reader busy.
fn serve_notifications(w: &mut TcpStream, pending: &AtomicBool) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    if w.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = w.flush();
    loop {
        if pending.swap(false, Ordering::SeqCst) {
            let note = json!({"jsonrpc": "2.0", "method": "notifications/resources/updated",
                              "params": {"uri": WATCHED}});
            if w.write_all(format!("data: {note}\n\n").as_bytes()).is_err() {
                return;
            }
            let _ = w.flush();
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Read one HTTP request off `r`: `(request line, body)`.
fn read_http(r: &mut BufReader<TcpStream>) -> Option<(String, Vec<u8>)> {
    let mut start = String::new();
    if r.read_line(&mut start).ok()? == 0 {
        return None;
    }
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end().to_string();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut body).ok()?;
    }
    Some((start, body))
}

fn respond_json(w: &mut TcpStream, body: &Value, session: bool) {
    let b = serde_json::to_vec(body).unwrap_or_default();
    let session_hdr = if session {
        "Mcp-Session-Id: slow-read-mock\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session_hdr}Content-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    );
    let _ = w.write_all(head.as_bytes());
    let _ = w.write_all(&b);
    let _ = w.flush();
}

// ---- harness ----------------------------------------------------------------

/// Every telemetry line, in emission order.
fn lines(stderr: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// The index of the first line matching `pred`, among all telemetry lines.
fn index_of(stderr: &str, pred: impl Fn(&Value) -> bool) -> Option<usize> {
    lines(stderr).iter().position(pred)
}

/// The `ticker` run reached its terminal `completed`.
fn ticker_done(v: &Value) -> bool {
    v["event"] == "run.done" && v["output"]["who"] == "ticker"
}

/// The `wait resource` step was resolved by the (delayed) read.
fn watch_step_done(v: &Value) -> bool {
    v["event"] == "step.done" && v["step"] == "changed"
}

#[test]
fn a_slow_resources_read_after_a_notification_does_not_stall_the_reactor() {
    let endpoint = spawn_slow_read_mcp(READ_DELAY);
    // Two runs, started together from the test inbox:
    //   `watch`  subscribes and suspends on the resource — the notification
    //            arrives within ~100 ms and triggers the notify-then-read;
    //   `ticker` arms a durable sleep timer and needs the reactor to fire it.
    let steps_watch = r#"{
        "start": {"kind": "manual"},
        "changed": {"kind": "wait", "depends_on": ["start"], "on": "resource", "server": "mock", "uri": "mock://watched", "timeout": "120s"},
        "done": {"kind": "finish", "depends_on": ["changed"], "status": "completed", "output": {"who": "watch", "content": "{{steps.changed.output.content}}"}}
    }"#;
    let steps_ticker = format!(
        r#"{{
        "start": {{"kind": "manual"}},
        "hold": {{"kind": "sleep", "depends_on": ["start"], "duration": "{TICK_SLEEP}"}},
        "done": {{"kind": "finish", "depends_on": ["hold"], "status": "completed", "output": {{"who": "ticker"}}}}
    }}"#
    );
    let cfg_path = common::unique_path("agentd-notify-block", "yaml");
    std::fs::write(
        &cfg_path,
        format!(
            "config_version: \"2\"\nagent:\n  name: notify-block\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}\nworkflows:\n  - name: watch\n    steps: {steps_watch}\n  - name: ticker\n    steps: {steps_ticker}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n"
        ),
    )
    .expect("write config");
    let inbox = common::unique_path("inbox-notify-block", "json");
    std::fs::write(
        &inbox,
        json!([
            {"kind": "workflow_run", "payload": {"workflow": "watch", "node": "start", "inputs": {}}},
            {"kind": "workflow_run", "payload": {"workflow": "ticker", "node": "start", "inputs": {}}}
        ])
        .to_string(),
    )
    .expect("write inbox");

    let err_path = common::unique_path("agentd-notify-block", "err");
    let err = std::fs::File::create(&err_path).expect("create stderr file");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .env("AGENTD_TEST_INBOX_FILE", &inbox)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn agentd");
    let started = Instant::now();

    // 1. The timer-driven run must complete while the read is still in flight.
    let deadline = started + HARD_TIMEOUT;
    let ticker_at = loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if lines(&log).iter().any(ticker_done) {
            break started.elapsed();
        }
        assert!(
            Instant::now() < deadline,
            "the `ticker` run never completed in {HARD_TIMEOUT:?}; stderr:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    // 2. Let the daemon finish (it idle-exits once both runs are terminal).
    let code = loop {
        match child.try_wait().expect("wait for agentd") {
            Some(status) => break status.code(),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let log = std::fs::read_to_string(&err_path).unwrap_or_default();
                panic!("agentd never exited in {HARD_TIMEOUT:?}; stderr:\n{log}");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&inbox);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");

    // The wall-clock claim: the reactor kept its own time while a foreign server
    // sat on a read for `READ_DELAY`.
    assert!(
        ticker_at < REACTIVE_BUDGET,
        "the reactor stalled: the 1 s sleep of `ticker` only completed after {ticker_at:?} \
         while an MCP `resources/read` was in flight for {READ_DELAY:?} \
         (budget {REACTIVE_BUDGET:?}); stderr:\n{stderr}"
    );
    // The timing-free claim: the timer fired BEFORE the read came back. On a box
    // slow enough to blur the budget above, this still separates "the read is
    // off the loop" from "the loop is waiting for the read".
    let ticker_line = index_of(&stderr, ticker_done).expect("a ticker run.done line");
    let read_line = index_of(&stderr, watch_step_done).expect("a `changed` step.done line");
    assert!(
        ticker_line < read_line,
        "the timer-driven run finished only after the notify-then-read returned \
         — the read is running on the single-writer loop; stderr:\n{stderr}"
    );
    // And the offloaded read is still APPLIED: the wait resolved with content.
    let watch = lines(&stderr)
        .into_iter()
        .find(|v| v["event"] == "run.done" && v["output"]["who"] == "watch")
        .expect("a watch run.done line");
    assert_eq!(watch["status"], "completed", "{watch}");
    assert_eq!(
        watch["output"]["content"], "the watched resource changed",
        "the read that landed off-thread resolved the wait with the resource body: {watch}"
    );
}
