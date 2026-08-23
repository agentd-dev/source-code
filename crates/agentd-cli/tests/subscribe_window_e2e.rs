// SPDX-License-Identifier: AGPL-3.0-only
//! The `subscribe` start node's **sample window** (`window: {samples: N}`):
//! a stream of resource updates — the hardware-driver shape — delivers not just
//! the latest reading but the last N, as `steps.<id>.output.window`, so a
//! workflow can act on a trend without the MCP server pre-aggregating.
//!
//! End to end: a mock MCP server publishes a counter resource and pushes an
//! update whenever the test tells it to; the daemon's subscription fires a run
//! per update; each run's finish output carries the ring as of its firing. The
//! ring must GROW to N and then SLIDE (oldest sample out), and survive across
//! firings — it rides the durable start-state, not the run.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const WATCHED: &str = "mock://val";
const HARD_TIMEOUT: Duration = Duration::from_secs(60);

/// The mock's knobs, shared with the test body: `val` is what `resources/read`
/// answers (`{"v": <val>}`), bumping `pushes` makes the notification stream
/// send one `resources/updated`.
struct Knobs {
    val: AtomicU64,
    pushes: AtomicU64,
}

fn spawn_counter_mcp(knobs: Arc<Knobs>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock mcp");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let knobs = Arc::clone(&knobs);
            std::thread::spawn(move || serve_conn(conn, knobs));
        }
    });
    endpoint
}

fn serve_conn(conn: TcpStream, knobs: Arc<Knobs>) {
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
        serve_notifications(&mut w, &knobs);
        return;
    }
    if start.starts_with("DELETE ") {
        let _ = w.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(id) = msg.get("id").cloned() else {
        let _ =
            w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let (result, session) = match method {
        "initialize" => (
            json!({
                // Legacy revision on purpose: the client then arms the
                // subscription via `resources/subscribe`, which this mock speaks.
                "protocolVersion": "2025-11-25",
                "capabilities": {"resources": {"subscribe": true, "listChanged": true}, "tools": {}},
                "serverInfo": {"name": "counter-mock", "version": "0"}
            }),
            true,
        ),
        "ping" => (json!({}), false),
        "tools/list" => (json!({"tools": []}), false),
        "prompts/list" => (json!({"prompts": []}), false),
        "resources/templates/list" => (json!({"resourceTemplates": []}), false),
        "resources/list" => (
            json!({"resources": [{"uri": WATCHED, "name": "val"}]}),
            false,
        ),
        "resources/subscribe" | "resources/unsubscribe" => (json!({}), false),
        "resources/read" => {
            let v = knobs.val.load(Ordering::SeqCst);
            (
                json!({"contents": [{"uri": WATCHED, "mimeType": "application/json",
                                     "text": format!("{{\"v\":{v}}}")}]}),
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

fn serve_notifications(w: &mut TcpStream, knobs: &Knobs) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    if w.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = w.flush();
    loop {
        while knobs.pushes.load(Ordering::SeqCst) > 0 {
            knobs.pushes.fetch_sub(1, Ordering::SeqCst);
            let note = json!({"jsonrpc": "2.0", "method": "notifications/resources/updated",
                              "params": {"uri": WATCHED}});
            if w.write_all(format!("data: {note}\n\n").as_bytes()).is_err() {
                return;
            }
            let _ = w.flush();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

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
        "Mcp-Session-Id: counter-mock\r\n"
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

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

#[test]
fn a_subscribe_window_delivers_the_last_n_samples_growing_then_sliding() {
    let knobs = Arc::new(Knobs {
        val: AtomicU64::new(0),
        pushes: AtomicU64::new(0),
    });
    let endpoint = spawn_counter_mcp(Arc::clone(&knobs));

    let steps = r#"{
        "s": {"kind": "subscribe", "server": "mock", "uri": "mock://val", "window": {"samples": 3}},
        "f": {"kind": "finish", "depends_on": ["s"], "status": "completed",
              "output": {"win": "{{steps.s.output.window}}", "cur": "{{steps.s.output.content}}"}}
    }"#;
    let cfg_path = common::unique_path("agentd-sub-window", "yaml");
    std::fs::write(
        &cfg_path,
        format!(
            "config_version: \"1\"\nagent:\n  name: sub-window\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}\nworkflows:\n  - name: watch\n    steps: {steps}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n  log_content: true\n"
        ),
    )
    .expect("write config");

    let err_path = common::unique_path("agentd-sub-window", "err");
    let err = std::fs::File::create(&err_path).expect("create stderr file");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn agentd");

    let deadline = Instant::now() + HARD_TIMEOUT;
    let wait_for = |pred: &dyn Fn(&str) -> bool, what: &str| {
        loop {
            let log = std::fs::read_to_string(&err_path).unwrap_or_default();
            if pred(&log) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    };

    // The subscription must be armed before the first push, or the mock sends
    // a notification into a stream nobody reads yet.
    wait_for(
        &|log| !events(log, "start.subscribe.armed").is_empty(),
        "the subscription to arm",
    );

    // Four serialized updates: set the value, push, wait for that run to finish.
    for n in 1u64..=4 {
        knobs.val.store(n, Ordering::SeqCst);
        knobs.pushes.fetch_add(1, Ordering::SeqCst);
        wait_for(
            &|log| events(log, "run.done").len() >= n as usize,
            "the fired run to complete",
        );
    }

    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let _ = child.wait();
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    let _ = std::fs::remove_file(&cfg_path);

    let done = events(&stderr, "run.done");
    assert_eq!(done.len(), 4, "one run per update:\n{stderr}");
    let wins: Vec<Value> = done.iter().map(|e| e["output"]["win"].clone()).collect();
    // Grows to N…
    assert_eq!(
        wins[0],
        json!([{"v": 1}]),
        "first firing: one sample\n{stderr}"
    );
    assert_eq!(
        wins[1],
        json!([{"v": 1}, {"v": 2}]),
        "second firing: two\n{stderr}"
    );
    assert_eq!(
        wins[2],
        json!([{"v": 1}, {"v": 2}, {"v": 3}]),
        "third firing: the full window\n{stderr}"
    );
    // …then SLIDES: the oldest sample leaves, order is oldest→newest.
    assert_eq!(
        wins[3],
        json!([{"v": 2}, {"v": 3}, {"v": 4}]),
        "fourth firing: the window slid\n{stderr}"
    );
    // The plain latest-value payload is untouched by windowing.
    assert_eq!(done[3]["output"]["cur"], json!({"v": 4}), "{stderr}");
}
