// SPDX-License-Identifier: AGPL-3.0-only
//! The **interface surface** end to end (RFC 0032): a v2 daemon with
//! `interface.enabled` (+ `debug`) serves the display-client contract over its
//! real A2A listener — `interface.info` discovery, the global
//! `SubscribeToEvents` SSE feed (cross-client transcript sync + cursor
//! resume), the taskless debug reads (`conversation.get` with message bodies,
//! `run.get` with per-step detail, `debug.events` log-ring tail), the
//! browser-origin CORS path, the disabled-by-default gate, and the
//! `agentd tui` passthrough (client spawn + tied lifetimes) with a stub
//! client binary.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn post_raw(addr: &str, body: &str) -> String {
    post_raw_auth(addr, body, None)
}

fn post_raw_auth(addr: &str, body: &str, bearer: Option<&str>) -> String {
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.set_read_timeout(Some(Duration::from_secs(130))).ok();
    let auth = bearer
        .map(|b| format!("Authorization: Bearer {b}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

/// An rpc that EXPECTS an error; returns (code, message).
fn rpc_err(addr: &str, id: i64, method: &str, params: Value) -> (i64, String) {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let resp = post_raw(addr, &body);
    let v: Value = serde_json::from_str(&resp).expect("json");
    let e = v
        .get("error")
        .unwrap_or_else(|| panic!("expected error: {v}"));
    (
        e["code"].as_i64().unwrap_or(0),
        e["message"].as_str().unwrap_or("").to_string(),
    )
}

/// A command DataPart send.
fn command(addr: &str, id: i64, op: &str, extra: Value) -> Value {
    let mut data = json!({"op": op});
    if let (Value::Object(d), Value::Object(x)) = (&mut data, extra) {
        for (k, v) in x {
            d.insert(k, v);
        }
    }
    rpc(
        addr,
        id,
        "SendMessage",
        json!({"message": {"messageId": format!("m-{id}"), "parts": [{"data": {"agentd": data}}]}}),
    )
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
    let pb = common::unique_path("iface-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("iface-mock-llm", "addr");
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

struct Daemon {
    child: Child,
    stderr_path: String,
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
fn spawn_daemon(config: &str) -> Daemon {
    let stderr_path = common::unique_path("iface-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd daemon");
    Daemon { child, stderr_path }
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-iface", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

/// A loopback daemon (⇒ operator) with the interface on; `debug` + `extra`
/// shape each test.
fn iface_config(llm: &str, port: u16, debug: bool, extra: &str) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: iface-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         interface:\n  enabled: true\n  debug: {debug}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n{extra}"
    )
}

/// Open a `SubscribeToEvents` SSE stream; frames (each a JSON-RPC response's
/// `result`) are appended to the shared vec until the connection closes or the
/// socket read times out.
fn subscribe_events(addr: &str, from_seq: u64, sink: Arc<Mutex<Vec<Value>>>) {
    let body = json!({"jsonrpc": "2.0", "id": 77, "method": "SubscribeToEvents", "params": {"fromSeq": from_seq}})
        .to_string();
    let mut s = TcpStream::connect(addr).expect("connect sse");
    s.set_read_timeout(Some(Duration::from_secs(20))).ok();
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
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(v) = serde_json::from_str::<Value>(data.trim())
            && let Some(result) = v.get("result")
        {
            sink.lock().unwrap().push(result.clone());
        }
    }
}

fn wait_for<F: Fn(&[Value]) -> bool>(sink: &Arc<Mutex<Vec<Value>>>, secs: u64, pred: F) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        {
            let got = sink.lock().unwrap();
            if pred(&got) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "condition never held; frames: {:#?}",
                *got
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn interface_info_and_the_debug_reads_work_over_a2a() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "Hello from the mock."}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let extra = "workflows:\n  - name: greet\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], output: \"done\"}\n";
    let cfg = write_config(&iface_config(&llm.uri, port, true, extra));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    // Discovery: enabled + debug + the op list a client keys its panes off.
    let info = command(&addr, 1, "interface.info", json!({}));
    assert_eq!(info["interface"]["enabled"], true, "{info}");
    assert_eq!(info["interface"]["debug"], true);
    assert_eq!(info["interface"]["feed"]["method"], "SubscribeToEvents");
    let ops: Vec<&str> = info["interface"]["ops"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(ops.contains(&"conversation.get") && ops.contains(&"debug.events"));
    // Taskless: an interface read creates NO durable task.
    let tasks_before = rpc(&addr, 2, "ListTasks", json!({}))["tasks"]
        .as_array()
        .unwrap()
        .len();
    let _ = command(&addr, 3, "interface.info", json!({}));
    let tasks_after = rpc(&addr, 4, "ListTasks", json!({}))["tasks"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(tasks_before, tasks_after, "interface reads are taskless");

    // The agent card advertises the surface (public discovery).
    let card = rpc(&addr, 5, "GetAgentCard", json!({}));
    assert_eq!(
        card["capabilities"]["extensions"][0]["uri"],
        "urn:agentd:interface"
    );

    // A conversation turn, then read its transcript (debug).
    let sent = rpc(
        &addr,
        6,
        "SendMessage",
        json!({"message": {"messageId": "m-nl", "parts": [{"text": "Say hello"}]}}),
    );
    let ctx = sent["task"]["contextId"].as_str().unwrap().to_string();
    assert_eq!(sent["task"]["status"]["state"], "TASK_STATE_COMPLETED");
    let conv = command(&addr, 7, "conversation.get", json!({"id": ctx}));
    let msgs = conv["conversation"]["messages"].as_array().unwrap();
    assert!(
        msgs.iter()
            .any(|m| m["role"] == "user" && m["text"].as_str().unwrap_or("").contains("Say hello")),
        "user message in transcript: {msgs:#?}"
    );
    assert!(
        msgs.iter().any(|m| m["role"] == "assistant"),
        "assistant reply in transcript"
    );

    // A run with per-step detail (debug).
    let run_task = command(&addr, 8, "workflow.run", json!({"name": "greet"}));
    let run_task_id = run_task["task"]["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut run_id = String::new();
    while Instant::now() < deadline {
        let got = rpc(&addr, 9, "GetTask", json!({"id": run_task_id}));
        if got["status"]["state"] == "TASK_STATE_COMPLETED" {
            let ws = command(&addr, 10, "workflow.status", json!({}));
            run_id = ws["task"]["artifacts"][0]["parts"][0]["text"]
                .as_str()
                .and_then(|t| serde_json::from_str::<Value>(t).ok())
                .and_then(|v| v["runs"][0]["run"].as_str().map(str::to_string))
                .unwrap_or_default();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!run_id.is_empty(), "workflow.status yielded the run id");
    let run = command(&addr, 11, "run.get", json!({"run": run_id}));
    assert_eq!(run["run"]["status"], "completed", "{run}");
    let steps = run["run"]["steps"].as_object().unwrap();
    assert_eq!(steps["f"]["status"], "done", "per-step detail: {steps:?}");
    assert!(steps["f"]["finished"].is_u64());

    // The live log ring (debug) has lines, cursored.
    let ev = command(&addr, 12, "debug.events", json!({"limit": 50}));
    let events = ev["events"].as_array().unwrap();
    assert!(!events.is_empty(), "the event ring is live");
    assert!(events[0]["seq"].is_u64() && events[0]["event"].is_string());
    let newest = ev["newest_seq"].as_u64().unwrap();
    let again = command(&addr, 13, "debug.events", json!({"after": newest}));
    assert!(
        again["events"].as_array().unwrap().len() <= events.len(),
        "the cursor advances"
    );

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn subscribe_to_events_streams_cross_client_activity_and_resumes() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "The reply."}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&iface_config(&llm.uri, port, false, ""));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    // Client A: attach to the feed.
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    let addr2 = addr.clone();
    std::thread::spawn(move || subscribe_events(&addr2, 0, sink));
    wait_for(&frames, 5, |f| f.iter().any(|v| v.get("hello").is_some()));
    {
        let f = frames.lock().unwrap();
        let hello = f.iter().find(|v| v.get("hello").is_some()).unwrap();
        assert_eq!(hello["hello"]["debug"], false);
        assert_eq!(hello["hello"]["resync"], false);
    }

    // Client B: send a prompt on a separate connection (blocking).
    let sent = rpc(
        &addr,
        20,
        "SendMessage",
        json!({"message": {"messageId": "m-x", "parts": [{"text": "Ping across clients"}]}}),
    );
    assert_eq!(sent["task"]["status"]["state"], "TASK_STATE_COMPLETED");

    // Client A observes B's prompt (the `message` event) AND the task reaching
    // terminal with the artifact — the cross-client transcript, no polling.
    wait_for(&frames, 10, |f| {
        f.iter().any(|v| {
            v["event"]["kind"] == "message"
                && v["event"]["data"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("Ping across clients"))
        })
    });
    wait_for(&frames, 10, |f| {
        f.iter().any(|v| {
            v["event"]["kind"] == "task"
                && v["event"]["data"]["task"]["status"]["state"] == "TASK_STATE_COMPLETED"
                && v["event"]["data"]["task"]["artifacts"][0]["parts"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("The reply."))
        })
    });

    // Resume: a second subscriber from the observed cursor sees NOTHING old.
    let max_seq = {
        let f = frames.lock().unwrap();
        f.iter()
            .filter_map(|v| v["event"]["seq"].as_u64())
            .max()
            .unwrap()
    };
    let resumed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink2 = Arc::clone(&resumed);
    let addr3 = addr.clone();
    std::thread::spawn(move || subscribe_events(&addr3, max_seq, sink2));
    wait_for(&resumed, 5, |f| f.iter().any(|v| v.get("hello").is_some()));
    std::thread::sleep(Duration::from_millis(400));
    {
        let f = resumed.lock().unwrap();
        assert!(
            f.iter()
                .filter_map(|v| v["event"]["seq"].as_u64())
                .all(|s| s > max_seq),
            "no replayed event at or before the cursor: {f:#?}"
        );
    }

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn the_interface_is_gated_off_by_default() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    // NO interface block: the surface must refuse, the core must be untouched.
    let cfg = write_config(&format!(
        "config_version: \"2\"\n\
         agent:\n  name: iface-off\n  instruction: Test.\n  preflight: never\n\
         intelligence:\n  endpoints: {}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         lifecycle:\n  run_until: drained\n",
        llm.uri
    ));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    // The command ops refuse…
    let (code, msg) = rpc_err(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"data": {"agentd": {"op": "interface.info"}}}]}}),
    );
    assert_eq!(code, -32004);
    assert!(msg.contains("interface.enabled"), "{msg}");
    // …debug reads refuse…
    let (code, _) = rpc_err(
        &addr,
        2,
        "SendMessage",
        json!({"message": {"messageId": "m2", "parts": [{"data": {"agentd": {"op": "debug.events"}}}]}}),
    );
    assert_eq!(code, -32004);
    // …the stream refuses (as its SSE terminal frame)…
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let body = json!({"jsonrpc": "2.0", "id": 3, "method": "SubscribeToEvents", "params": {}})
            .to_string();
        let resp_or_stream = post_raw(&addr, &body);
        // Either a plain error body or an SSE stream whose only frame is the error.
        assert!(
            resp_or_stream.contains("-32004") && resp_or_stream.contains("interface.enabled"),
            "{resp_or_stream}"
        );
        drop(frames);
    }
    // …and the core surface still answers (status command untouched).
    let st = command(&addr, 4, "status", json!({}));
    assert_eq!(st["task"]["status"]["state"], "TASK_STATE_COMPLETED");
    // The card carries no interface extension.
    let card = rpc(&addr, 5, "GetAgentCard", json!({}));
    assert!(card["capabilities"].get("extensions").is_none());

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_configured_web_origin_gets_cors_and_others_stay_rejected() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let extra = "  origins: [\"https://ui.example\"]\n";
    let cfg = write_config(&iface_config(&llm.uri, port, false, "").replace(
        "interface:\n  enabled: true\n  debug: false\n",
        &format!("interface:\n  enabled: true\n  debug: false\n{extra}"),
    ));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let send = |req: String| -> (u16, Vec<(String, String)>) {
        let mut s = TcpStream::connect(&addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut reader = BufReader::new(s);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let code = status
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut headers = Vec::new();
        loop {
            let mut l = String::new();
            if reader.read_line(&mut l).unwrap_or(0) == 0 {
                break;
            }
            if l.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        (code, headers)
    };

    // Preflight from the configured origin → 204 + grant.
    let (code, headers) = send(
        "OPTIONS / HTTP/1.1\r\nHost: x\r\nOrigin: https://ui.example\r\nAccess-Control-Request-Method: POST\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    );
    assert_eq!(code, 204);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "access-control-allow-origin" && v == "https://ui.example"),
        "{headers:?}"
    );
    // A POST from it → 200 + echo.
    let body =
        json!({"jsonrpc": "2.0", "id": 1, "method": "GetAgentCard", "params": {}}).to_string();
    let (code, headers) = send(format!(
        "POST / HTTP/1.1\r\nHost: x\r\nOrigin: https://ui.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    assert_eq!(code, 200);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "access-control-allow-origin" && v == "https://ui.example")
    );
    // Any other cross-site origin: still the rebind 403.
    let (code, _) = send(format!(
        "POST / HTTP/1.1\r\nHost: x\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    assert_eq!(code, 403);

    std::fs::remove_file(&cfg).ok();
}

/// A JSON-RPC call with a bearer; returns the whole response value.
fn rpc_raw_auth(addr: &str, id: i64, method: &str, params: Value, bearer: Option<&str>) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let resp = post_raw_auth(addr, &body, bearer);
    serde_json::from_str(&resp).unwrap_or_else(|_| panic!("non-JSON response: {resp:?}"))
}

#[test]
fn pairing_exchanges_the_rotating_code_for_a_session_token() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    // A bearer-PROTECTED listener with pairing on: uncredentialed callers are
    // anonymous (may call exactly Pair + the card), the server bearer is the
    // operator, and a paired session becomes a first-class credential.
    let cfg = write_config(&format!(
        "config_version: \"2\"\n\
         agent:\n  name: pair-e2e\n  instruction: Test.\n  preflight: never\n\
         intelligence:\n  endpoints: {}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n  bearer: \"{{{{secret:PAIRB}}}}\"\n\
         interface:\n  enabled: true\n  pairing:\n    enabled: true\n\
         lifecycle:\n  run_until: drained\n",
        llm.uri
    ));
    let stderr_path = common::unique_path("pair-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("PAIRB", "server-secret-bearer")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn daemon");
    let _daemon = Daemon { child, stderr_path };
    wait_ready(&addr);

    // 1. Anonymous: the card is public, work is refused.
    let card = rpc_raw_auth(&addr, 1, "GetAgentCard", json!({}), None);
    assert!(card["error"].is_null(), "{card}");
    let denied = rpc_raw_auth(
        &addr,
        2,
        "SendMessage",
        json!({"message": {"messageId": "m", "parts": [{"data": {"agentd": {"op": "status"}}}]}}),
        None,
    );
    assert_eq!(denied["error"]["code"], -32003, "{denied}");

    // 2. The operator (server bearer) reads the current code…
    let code_resp = rpc_raw_auth(
        &addr,
        3,
        "SendMessage",
        json!({"message": {"messageId": "m2", "parts": [{"data": {"agentd": {"op": "pairing.code"}}}]}}),
        Some("server-secret-bearer"),
    );
    let code = code_resp["result"]["pairing"]["code"]
        .as_str()
        .expect("code")
        .to_string();
    assert_eq!(code.len(), 6);
    assert!(
        code_resp["result"]["pairing"]["expires_in_ms"]
            .as_u64()
            .unwrap()
            <= 60_000
    );

    // 3. …a wrong code fails, the right one (ANONYMOUS) mints a session…
    let wrong = rpc_raw_auth(&addr, 4, "Pair", json!({"code": "000001"}), None);
    assert_eq!(wrong["error"]["code"], -32003);
    let paired = rpc_raw_auth(&addr, 5, "Pair", json!({"code": code}), None);
    let token = paired["result"]["token"]
        .as_str()
        .expect("token")
        .to_string();
    assert!(token.starts_with("pat-"));
    assert_eq!(paired["result"]["role"], "operator");

    // 4. …and the session token IS a working operator credential.
    let st = rpc_raw_auth(
        &addr,
        6,
        "SendMessage",
        json!({"message": {"messageId": "m3", "parts": [{"data": {"agentd": {"op": "status"}}}]}}),
        Some(&token),
    );
    assert!(st["error"].is_null(), "{st}");
    assert_eq!(
        st["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    // interface.info advertises pairing.
    let info = rpc_raw_auth(
        &addr,
        7,
        "SendMessage",
        json!({"message": {"messageId": "m4", "parts": [{"data": {"agentd": {"op": "interface.info"}}}]}}),
        Some(&token),
    );
    assert_eq!(info["result"]["interface"]["pairing"]["enabled"], true);

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn config_set_toggles_debug_live_and_reshapes_the_display() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    // Debug starts OFF.
    let cfg = write_config(&iface_config(&llm.uri, port, false, ""));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    // Debug reads refuse; info says so; the default display is served.
    let (code, _) = rpc_err(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"data": {"agentd": {"op": "debug.events"}}}]}}),
    );
    assert_eq!(code, -32004);
    let info = command(&addr, 2, "interface.info", json!({}));
    assert_eq!(info["interface"]["debug"], false);
    assert!(
        info["interface"]["display"]["bottom"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "conn"),
        "{info}"
    );

    // `config.set interface.debug true` flips it at runtime…
    let set = command(
        &addr,
        3,
        "config.set",
        json!({"path": "interface.debug", "value": true}),
    );
    assert_eq!(set["set"]["value"], true, "{set}");
    let info = command(&addr, 4, "interface.info", json!({}));
    assert_eq!(info["interface"]["debug"], true);
    // …and the debug reads work — including the log ring, installed on toggle.
    let ev = command(&addr, 5, "debug.events", json!({"limit": 10}));
    assert!(ev["events"].is_array(), "{ev}");

    // The display is runtime-shapeable; unknown paths name the whitelist.
    let set = command(
        &addr,
        6,
        "config.set",
        json!({"path": "interface.display.bottom", "value": ["conn", "model", "tokens"]}),
    );
    assert_eq!(set["set"]["value"], json!(["conn", "model", "tokens"]));
    let info = command(&addr, 7, "interface.info", json!({}));
    assert_eq!(
        info["interface"]["display"]["bottom"],
        json!(["conn", "model", "tokens"])
    );
    assert!(info["interface"]["model"].is_string());
    let (code, msg) = rpc_err(
        &addr,
        8,
        "SendMessage",
        json!({"message": {"messageId": "m8", "parts": [{"data": {"agentd": {"op": "config.set", "path": "intelligence.model", "value": "x"}}}]}}),
    );
    assert_eq!(code, -32602);
    assert!(msg.contains("not runtime-settable"), "{msg}");

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_live_subagent_is_observable_and_drillable() {
    // The root delegates to a sync subagent (mock tool_calls); the interface
    // then shows it: `subagent` feed/section data + the `subagent.get` detail.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {"instruction": "count to three", "mode": "sync"}}]},
            {"content": "delegated and done"}
        ],
        "match": [
            {"when_contains": "You are agentd, an autonomous agent.", "content": "three"}
        ]
    }));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&iface_config(&llm.uri, port, true, ""));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    // Missing handle → non-disclosing not-found.
    let (code, _) = rpc_err(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m0", "parts": [{"data": {"agentd": {"op": "subagent.get", "handle": "nope"}}}]}}),
    );
    assert_eq!(code, -32001);

    // Drive the delegating turn (blocking → returns when the tree settles).
    let sent = rpc(
        &addr,
        2,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "count for me"}]}}),
    );
    assert_eq!(
        sent["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );

    // The status section lists the subagent; drill into it.
    let st = command(&addr, 3, "status", json!({}));
    let subs = st["task"]["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .map(|v| v["subagents"].clone())
        .unwrap_or_default();
    let handle = subs[0]["handle"]
        .as_str()
        .expect("a subagent exists")
        .to_string();
    let got = command(&addr, 4, "subagent.get", json!({"handle": handle}));
    let sub = &got["subagent"];
    assert_eq!(sub["status"], "completed", "{got}");
    assert_eq!(sub["mode"], "sync");
    assert!(
        sub["instruction"]
            .as_str()
            .unwrap()
            .contains("count to three"),
        "{got}"
    );
    assert!(
        sub["result"].is_object() || sub["result"].is_string(),
        "{got}"
    );

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn live_activity_reports_phase_tool_and_tokens_on_the_feed() {
    // A turn that calls a tool then answers: the feed must carry `activity`
    // events naming the phase and the TOOL, with tokens accruing — the data
    // behind the clients' working row (RFC 0032 §17).
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "memory.set", "arguments": {"key": "k", "value": 1}}]},
            {"content": "Stored it."}
        ]
    }));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&iface_config(&llm.uri, port, false, ""));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    // Attach FIRST so the activity events stream as they happen.
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    let addr2 = addr.clone();
    std::thread::spawn(move || subscribe_events(&addr2, 0, sink));
    wait_for(&frames, 5, |f| f.iter().any(|v| v.get("hello").is_some()));

    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "Remember k=1"}]}}),
    );
    assert_eq!(
        sent["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );

    let activity = |f: &[Value]| -> Vec<Value> {
        f.iter()
            .filter(|v| v["event"]["kind"] == "activity")
            .map(|v| v["event"]["data"].clone())
            .collect()
    };
    // The tool phase names the tool the model actually called.
    wait_for(&frames, 10, |f| {
        activity(f)
            .iter()
            .any(|a| a["phase"] == "tool" && a["tool"] == "memory.set")
    });
    // Thinking is reported too — and the unit answering the A2A message binds
    // to its task (a workflow step turn reports alongside it, unbound).
    wait_for(&frames, 10, |f| {
        activity(f).iter().any(|a| a["phase"] == "thinking")
    });
    wait_for(&frames, 10, |f| {
        activity(f)
            .iter()
            .any(|a| a["task"].as_str().is_some_and(|t| t.starts_with("task-")))
    });
    // …and the unit's record disappears when the turn ends.
    wait_for(&frames, 10, |f| {
        f.iter().any(|v| v["event"]["kind"] == "activity.removed")
    });

    let got = frames.lock().unwrap();
    let acts = activity(&got);
    assert!(
        acts.iter()
            .all(|a| a["started_ms"].as_u64().is_some_and(|t| t > 0)),
        "clients tick elapsed from started_ms: {acts:#?}"
    );
    let bound = acts
        .iter()
        .find(|a| a["task"].as_str().is_some_and(|t| t.starts_with("task-")))
        .expect("the A2A turn's activity binds to its task");
    assert!(
        bound["ctx"].as_str().is_some_and(|c| c.starts_with("a2a-")),
        "…and to its conversation: {bound}"
    );
    // Tokens accrue on the record (the mock reports usage per round).
    assert!(
        acts.iter().any(|a| a["tokens_in"].as_u64().unwrap_or(0)
            + a["tokens_out"].as_u64().unwrap_or(0)
            > 0),
        "activity carries the turn's spend: {acts:#?}"
    );
    // Deliberately COARSE: a handful of events, not a stream (the replay ring
    // must stay meaningful — this is the property token streaming would break).
    assert!(
        acts.len() <= 24,
        "activity is change-triggered, not a token stream: {} events",
        acts.len()
    );
    drop(got);
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn the_tui_passthrough_spawns_the_client_and_ties_lifetimes() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    // The stub "TUI": record the handed endpoint, then exit — which must drain
    // the daemon (client-exit ⇒ SIGTERM ⇒ graceful exit 0).
    let out = common::unique_path("iface-stub-out", "txt");
    let stub = common::unique_path("iface-stub", "sh");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\nprintf '%s %s' \"$AGENTD_ENDPOINT\" \"$1 $2\" > {out}\nexit 0\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let cfg = write_config(&format!(
        "config_version: \"2\"\n\
         agent:\n  name: tui-pass\n  instruction: Test.\n  preflight: never\n\
         intelligence:\n  endpoints: {}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         lifecycle:\n  run_until: drained\n  drain_timeout: 2s\n",
        llm.uri
    ));
    let daemon_log = common::unique_path("iface-pass-daemon", "log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["tui", "--config", &cfg])
        .env("AGENTD_TUI_BIN", &stub)
        .env("AGENTD_INTERFACE_LOG", &daemon_log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agentd tui");

    // The whole assembly winds down by itself: stub exits ⇒ daemon drains ⇒ 0.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Ok(Some(st)) = child.try_wait() {
            break st;
        }
        assert!(
            Instant::now() < deadline,
            "agentd tui did not exit after the client did; log: {}",
            std::fs::read_to_string(&daemon_log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "clean drain exit: {status:?}");

    // The stub got the derived loopback endpoint + the --endpoint arg.
    let recorded = std::fs::read_to_string(&out).expect("stub ran");
    assert!(
        recorded.contains(&format!("http://127.0.0.1:{port}")),
        "endpoint handed to the client: {recorded}"
    );
    assert!(recorded.contains("--endpoint"), "{recorded}");
    // The daemon's telemetry went to the log file, with the interface forced on.
    let dlog = std::fs::read_to_string(&daemon_log).unwrap_or_default();
    assert!(dlog.contains("a2a.listen"), "daemon logged to the file");
    assert!(
        dlog.contains("\"interface\":true"),
        "the subcommand forced interface.enabled"
    );

    let _ = std::fs::remove_file(&stub);
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&daemon_log);
    std::fs::remove_file(&cfg).ok();
}
