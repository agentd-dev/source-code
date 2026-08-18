// SPDX-License-Identifier: AGPL-3.0-only
//! **MCP elicitation, end to end** — and what happens to a gate whose asker dies.
//!
//! agentd advertises the `elicitation` client capability on every MCP
//! connection a turn worker opens (`subagent/control.rs`), which is a promise:
//! a server may stop mid-tool-call and ask the operator a question. The promise
//! is only kept if the whole chain works — `elicitation/create` → the child's
//! `ElicitationBridge` → a `ToolRequest` up to the supervisor → `ask_human` →
//! an `input-required` A2A task an operator can answer → the answer back down
//! the same wire, shaped to the server's `requestedSchema`.
//!
//! One link was broken: `ask_human` answered with a bare string, and the bridge
//! reads `reply` out of the tool result to build the spec's `accept` content.
//! It found nothing, so *every* elicitation came back `cancel` — the capability
//! was advertised and could not be honoured. The first test asserts from the
//! **server's** side, which is the only side that can tell: the JSON-RPC
//! response to `elicitation/create` carries `"action": "accept"` and the
//! operator's answer as `content`.
//!
//! The second test covers the other half of the same machinery: a gate whose
//! asking child is gone. The `ToolResult` slot died with the process, so an
//! answer can never reach the model — and the gate used to sit in
//! `input-required` for the whole 24 h ask timeout with an operator staring at
//! an answerable question whose answer went nowhere. It must end explicitly.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

// ---- A2A client ------------------------------------------------------------

fn post_raw(addr: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.set_read_timeout(Some(Duration::from_secs(130))).ok();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        if l.trim().is_empty() {
            break;
        }
    }
    let mut b = String::new();
    reader.read_to_string(&mut b).unwrap();
    b
}

fn rpc(addr: &str, id: i64, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let resp = post_raw(addr, &body);
    let v: Value =
        serde_json::from_str(&resp).unwrap_or_else(|_| panic!("non-JSON A2A response: {resp:?}"));
    assert!(v.get("error").is_none(), "A2A rpc error for {method}: {v}");
    v["result"].clone()
}

/// Poll `GetTask` until `pred` holds (returns the task).
fn wait_task<F: Fn(&Value) -> bool>(addr: &str, id: &str, secs: u64, what: &str, pred: F) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let t = rpc(addr, 900, "GetTask", json!({"id": id}));
        if pred(&t) {
            return t;
        }
        assert!(Instant::now() < deadline, "timeout: {what}; last: {t}");
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// Poll `ListTasks` until some task matches (returns its id). A gate the agent
/// opened for itself has no id the test could have known in advance.
fn wait_for_task_where<F: Fn(&Value) -> bool>(
    addr: &str,
    secs: u64,
    what: &str,
    pred: F,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let l = rpc(addr, 901, "ListTasks", json!({}));
        if let Some(t) = l["tasks"]
            .as_array()
            .and_then(|ts| ts.iter().find(|t| pred(t)))
        {
            return t["id"].as_str().expect("task id").to_string();
        }
        assert!(Instant::now() < deadline, "timeout: {what}; last: {l}");
        std::thread::sleep(Duration::from_millis(80));
    }
}

// ---- the mock model --------------------------------------------------------

struct MockLlm {
    child: Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}
fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("elicit-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("elicit-mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

// ---- the daemon ------------------------------------------------------------

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
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

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-elicit", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

/// Spawn the daemon on a probed free port and return the authority it ACTUALLY
/// bound. The probe→bind gap is a real race under parallel CI, so a daemon
/// whose bind lost is retried on a fresh port rather than leaving the test
/// talking to a stranger's listener.
fn spawn_bound(cfg_for: impl Fn(u16) -> String, inbox: Option<&str>) -> (Daemon, String, String) {
    for _ in 0..5 {
        let cfg = write_config(&cfg_for(free_port()));
        let stderr_path = common::unique_path("elicit-daemon", "log");
        let errf = std::fs::File::create(&stderr_path).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
        cmd.args(["--config", &cfg])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errf));
        if let Some(path) = inbox {
            cmd.env("AGENTD_TEST_INBOX_FILE", path);
        }
        let child = cmd.spawn().expect("spawn agentd daemon");
        let daemon = Daemon { child, stderr_path };
        if let Some(addr) = common::try_a2a_bound(&daemon.stderr_path, Duration::from_secs(20)) {
            return (daemon, addr, cfg);
        }
        std::fs::remove_file(&cfg).ok();
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

// ---- the mock MCP server that elicits ---------------------------------------

/// What the mock MCP server observed. The elicitation RESULT is the point of
/// the whole file: only the server can see whether we answered `accept` or
/// `cancel`.
#[derive(Default)]
struct McpSeen {
    /// Every `initialize` params we were sent — the supervisor opens its own
    /// connection (no elicitation) and each turn worker opens another (with).
    inits: Vec<Value>,
    /// The JSON-RPC response the client POSTed for `elicitation/create`.
    elicit_result: Option<Value>,
}
/// The mutex the server threads share, plus the condvar the `tools/call`
/// handler parks on while the operator is being asked.
type Shared = Arc<(Mutex<McpSeen>, Condvar)>;

fn read_http(r: &mut BufReader<TcpStream>) -> Option<(String, Vec<u8>)> {
    let mut start = String::new();
    if r.read_line(&mut start).ok()? == 0 {
        return None;
    }
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.trim().is_empty() {
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

fn respond_json(w: &mut TcpStream, body: &Value) {
    let b = serde_json::to_vec(body).unwrap();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    );
    let _ = w.write_all(head.as_bytes());
    let _ = w.write_all(&b);
    let _ = w.flush();
}

/// A minimal Streamable-HTTP MCP server whose one tool stops to ask the human.
///
/// `tools/call` answers with an SSE stream — the transport's own path for a
/// server that has something to say before it has a result — writes an
/// `elicitation/create` REQUEST onto it, waits for the client to POST the
/// response back on a separate connection, and only then writes the tool
/// result. That is the real shape; a server that could not interleave a
/// request with its response could not elicit at all.
fn spawn_mock_mcp(seen: Shared) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(120))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some((start, body)) = read_http(&mut r) else {
                    return;
                };
                // We keep no session, so the standalone server→client GET
                // stream (and the DELETE that closes it) have nothing to serve.
                if !start.starts_with("POST") {
                    let _ = w
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                // A frame with an id and no method is the client ANSWERING our
                // elicitation. Record it and wake the parked `tools/call`.
                if method.is_empty() && msg.get("id").is_some() {
                    let (lock, cv) = &*seen;
                    lock.lock().unwrap().elicit_result = Some(msg);
                    cv.notify_all();
                    let _ = w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                if msg.get("id").is_none() {
                    // A notification (`notifications/initialized`).
                    let _ = w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let id = msg["id"].clone();
                match method {
                    "initialize" => {
                        seen.0.lock().unwrap().inits.push(msg["params"].clone());
                        respond_json(
                            &mut w,
                            &json!({"jsonrpc": "2.0", "id": id, "result": {
                                // Echo the revision the client asked for, the
                                // way a real server does when it can speak it.
                                "protocolVersion": msg["params"]["protocolVersion"],
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "ops-mock", "version": "0"}
                            }}),
                        );
                    }
                    "tools/list" => respond_json(
                        &mut w,
                        &json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{
                            "name": "deploy",
                            "description": "Deploy the service (asks which environment).",
                            "inputSchema": {"type": "object", "properties": {}}
                        }]}}),
                    ),
                    "tools/call" => serve_eliciting_call(&mut w, id, &seen),
                    other => respond_json(
                        &mut w,
                        &json!({"jsonrpc": "2.0", "id": id,
                                "error": {"code": -32601, "message": format!("no {other}")}}),
                    ),
                }
            });
        }
    });
    endpoint
}

/// The `tools/call` half: elicit, wait, then answer with what the human said.
fn serve_eliciting_call(w: &mut TcpStream, id: Value, seen: &Shared) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    let _ = w.write_all(head.as_bytes());
    let elicit = json!({"jsonrpc": "2.0", "id": "elicit-1", "method": "elicitation/create",
    "params": {
        "message": "Which environment should I deploy to?",
        "requestedSchema": {"type": "object",
                            "properties": {"env": {"type": "string"}},
                            "required": ["env"]}
    }});
    let _ = w.write_all(format!("data: {elicit}\n\n").as_bytes());
    let _ = w.flush();

    // Park until the client answers (or long enough that the test has failed
    // for its own reasons). The stream stays open the whole time — that is what
    // a real elicitation looks like from the wire.
    let (lock, cv) = &**seen;
    let answered = {
        let guard = lock.lock().unwrap();
        let (guard, _) = cv
            .wait_timeout_while(guard, Duration::from_secs(60), |s| {
                s.elicit_result.is_none()
            })
            .unwrap();
        guard.elicit_result.clone()
    };
    // Echo the answer into the tool result so a wrong `content` cannot pass:
    // the model's transcript then carries what the operator actually typed.
    let env = answered
        .as_ref()
        .and_then(|a| a["result"]["content"]["env"].as_str())
        .unwrap_or("<nothing>")
        .to_string();
    let result = json!({"jsonrpc": "2.0", "id": id, "result": {
        "content": [{"type": "text", "text": format!("deployed to {env}")}],
        "isError": false
    }});
    let _ = w.write_all(format!("data: {result}\n\n").as_bytes());
    let _ = w.flush();
}

// ---- tests ------------------------------------------------------------------

#[test]
fn an_mcp_elicitation_reaches_the_operator_and_the_server_sees_accept_with_the_content() {
    let seen: Shared = Arc::new((Mutex::new(McpSeen::default()), Condvar::new()));
    let mcp = spawn_mock_mcp(Arc::clone(&seen));
    // Turn 1: call the MCP tool (which elicits mid-call); turn 2: the answer.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "deploy", "arguments": {}}]},
            {"content": "Rollout complete."}
        ]
    }));
    let (daemon, addr, cfg) = spawn_bound(
        |port| {
            format!(
                "config_version: \"2\"\n\
                 agent:\n  name: elicit-e2e\n  instruction: You deploy things.\n  preflight: never\n\
                 intelligence:\n  endpoints: {llm}\n  model: mock\n\
                 mcp:\n  servers:\n    - name: ops\n      endpoint: {mcp}\n\
                 store:\n  kind: memory\n\
                 workflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n\
                 interface:\n  enabled: true\n  debug: true\n\
                 lifecycle:\n  run_until: drained\n\
                 observability:\n  log_level: info\n  log_content: true\n",
                llm = llm.uri,
                mcp = mcp
            )
        },
        None,
    );

    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "Deploy the service"}]},
               "configuration": {"blocking": false}}),
    );
    let task_id = sent["task"]["id"].as_str().unwrap().to_string();

    // The server's question — not the agent's — is what the operator is shown.
    let gated = wait_task(&addr, &task_id, 30, "the elicitation gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    assert!(
        gated["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Which environment"),
        "the server's elicitation message reaches the operator verbatim: {gated}"
    );

    rpc(
        &addr,
        2,
        "SendMessage",
        json!({"message": {"messageId": "m2", "taskId": task_id, "parts": [{"text": "staging"}]},
               "configuration": {"blocking": false}}),
    );

    // THE assertion: from the server's side, the elicitation was accepted and
    // carries the operator's answer bound to the property it asked for. A bare
    // string tool result made this a `cancel` every single time.
    let deadline = Instant::now() + Duration::from_secs(30);
    let answered = loop {
        if let Some(a) = seen.0.lock().unwrap().elicit_result.clone() {
            break a;
        }
        assert!(
            Instant::now() < deadline,
            "the server never got an answer to elicitation/create; daemon log:\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        answered["id"], "elicit-1",
        "answered OUR request: {answered}"
    );
    assert_eq!(
        answered["result"]["action"], "accept",
        "the operator answered, so the action is accept — not cancel: {answered}"
    );
    assert_eq!(
        answered["result"]["content"],
        json!({"env": "staging"}),
        "the answer is shaped to the server's requestedSchema: {answered}"
    );

    // …and the turn carries on with the tool result the server built from it.
    let done = wait_task(&addr, &task_id, 30, "turn completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert!(
        done["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Rollout complete"),
        "{done}"
    );
    // The capability we advertise is the one we just honoured.
    let inits = &seen.0.lock().unwrap().inits;
    assert!(
        inits
            .iter()
            .any(|p| p["capabilities"]["elicitation"].is_object()),
        "a connection declared the elicitation capability: {inits:?}"
    );
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_gate_whose_asking_child_died_ends_explicitly_instead_of_hanging() {
    // The turn is injected through the inbox seam, so no A2A message owns it:
    // `ask_human` therefore creates a STANDALONE gate task, whose state is
    // driven only by the gate itself. Nothing else can terminate it, so what
    // this test observes is the gate's own disposition and nothing else.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Ship it?"}}]},
            {"content": "unreachable — the turn dies at its deadline"}
        ]
    }));
    let inbox = common::unique_path("elicit-inbox", "json");
    std::fs::write(
        &inbox,
        json!([{"kind": "a2a_message", "payload": {"context_id": "c-orphan", "text": "ask me something"}}])
            .to_string(),
    )
    .unwrap();
    // A short run deadline is how the asking child is made to die with its gate
    // still open: the worker gives up waiting for the `ToolResult` at its
    // deadline and exits, while the supervisor's gate keeps the 24 h ask
    // timeout. That is exactly the orphan — a crash or a kill lands identically.
    let (daemon, addr, cfg) = spawn_bound(
        |port| {
            format!(
                "config_version: \"2\"\n\
                 agent:\n  name: orphan-e2e\n  instruction: You ask questions.\n  preflight: never\n\
                 intelligence:\n  endpoints: {llm}\n  model: mock\n\
                 store:\n  kind: memory\n\
                 workflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\n\
                 limits:\n  run:\n    deadline: 6s\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n\
                 interface:\n  enabled: true\n  debug: true\n\
                 lifecycle:\n  run_until: drained\n\
                 observability:\n  log_level: info\n  log_content: true\n",
                llm = llm.uri
            )
        },
        Some(&inbox),
    );

    let gate = wait_for_task_where(&addr, 30, "the standalone gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });

    // The child is gone within its 6s deadline. The gate must not still be
    // waiting: an answerable question whose answer can no longer be delivered
    // is worse than a failed one.
    let ended = wait_task(&addr, &gate, 45, "the orphaned gate ends", |t| {
        t["status"]["state"] != "TASK_STATE_INPUT_REQUIRED"
    });
    assert_eq!(
        ended["status"]["state"], "TASK_STATE_FAILED",
        "an orphaned gate fails explicitly: {ended}"
    );
    assert!(
        ended["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("asking turn ended"),
        "the failure says WHY, so an operator is not left guessing: {ended}"
    );
    let logs = daemon.stderr();
    assert!(
        logs.contains("human.ask.pruned"),
        "the prune is on the record, not silent: {logs}"
    );
    // …and the pass that ended the gate did not take the reactor with it. The
    // pending sweep mutates the table it is walking (ending one entry can prune
    // others), so "the daemon is still answering afterwards" is the assertion
    // that a stale-index regression would trip.
    let info = rpc(&addr, 50, "GetTask", json!({"id": gate}));
    assert_eq!(
        info["id"],
        gate.as_str(),
        "the daemon still answers: {info}"
    );
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_file(&inbox).ok();
}
