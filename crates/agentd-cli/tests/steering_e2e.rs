// SPDX-License-Identifier: AGPL-3.0-only
//! **Steering over A2A** end to end (RFC 0029 §5/§7, now dispatched): a client
//! fires `workflow.signal` to resume a waiting run, pauses/resumes one run and
//! the whole instance (`a2a.pause`/`a2a.resume`), and reads a conversation's
//! plan — the control verbs a display client uses beyond cancel/drain.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
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
        // Non-blocking: command tasks complete inline anyway, and workflow.run
        // must NOT be polled to terminal — the tests steer runs mid-flight.
        json!({"message": {"messageId": format!("m-{id}"), "parts": [{"data": {"agentd": data}}]},
               "configuration": {"blocking": false}}),
    )
}

/// Parse a command task's JSON artifact.
fn artifact_json(v: &Value) -> Value {
    v["task"]["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or(Value::Null)
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

/// Poll `workflow.status` for one run until `pred` holds; returns the view.
fn wait_run<F: Fn(&Value) -> bool>(addr: &str, run: &str, secs: u64, what: &str, pred: F) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let ws = command(addr, 901, "workflow.status", json!({"run": run}));
        let view = artifact_json(&ws)["runs"][0].clone();
        if pred(&view) {
            return view;
        }
        assert!(Instant::now() < deadline, "timeout: {what}; last: {view}");
        std::thread::sleep(Duration::from_millis(80));
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
    let pb = common::unique_path("steer-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("steer-mock-llm", "addr");
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
    let stderr_path = common::unique_path("steer-daemon", "log");
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
    let path = common::unique_path("agentd-steer", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

fn steer_config(llm: &str, port: u16, extra: &str) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: steer-e2e\n  instruction: Test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         interface:\n  enabled: true\n  debug: true\n\
         lifecycle:\n  run_until: drained\n{extra}"
    )
}

#[test]
fn a_signal_resumes_a_waiting_run() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    // A run that WAITS for a named signal, then finishes with its payload.
    let extra = "workflows:\n  - name: waiter\n    steps:\n      s: {kind: manual}\n      w: {kind: wait, on: signal, signal: go, depends_on: [s]}\n      f: {kind: finish, depends_on: [w], output: \"released\"}\n";
    let cfg = write_config(&steer_config(&llm.uri, port, extra));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let started = command(&addr, 1, "workflow.run", json!({"name": "waiter"}));
    let task_id = started["task"]["id"].as_str().unwrap().to_string();
    // Find the run id + confirm it parks on the wait.
    let ws = command(&addr, 2, "workflow.status", json!({}));
    let run_id = artifact_json(&ws)["runs"][0]["run"]
        .as_str()
        .unwrap()
        .to_string();
    wait_run(&addr, &run_id, 10, "suspended on the signal", |v| {
        v["status"] == "suspended" || v["status"] == "running"
    });

    // The steering verb: `workflow.signal` fires the named signal.
    let sig = command(
        &addr,
        3,
        "workflow.signal",
        json!({"name": "go", "payload": {"by": "e2e"}}),
    );
    assert_eq!(artifact_json(&sig)["delivered"], 1, "{sig}");

    wait_run(&addr, &run_id, 10, "run completed after the signal", |v| {
        v["status"] == "completed"
    });
    // The tracking task went terminal too.
    let t = rpc(&addr, 4, "GetTask", json!({"id": task_id}));
    assert_eq!(t["status"]["state"], "TASK_STATE_COMPLETED", "{t}");
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_single_run_pauses_and_resumes() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    // A run with a 1s sleep between steps — enough of a window to pause it.
    let extra = "workflows:\n  - name: slow\n    steps:\n      s: {kind: manual}\n      z: {kind: sleep, duration: 1s, depends_on: [s]}\n      f: {kind: finish, depends_on: [z], output: \"done\"}\n";
    let cfg = write_config(&steer_config(&llm.uri, port, extra));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    command(&addr, 1, "workflow.run", json!({"name": "slow"}));
    let ws = command(&addr, 2, "workflow.status", json!({}));
    let run_id = artifact_json(&ws)["runs"][0]["run"]
        .as_str()
        .unwrap()
        .to_string();

    // Pause the run mid-flight; it must NOT complete while paused.
    let paused = rpc(&addr, 3, "a2a.pause", json!({"run": run_id}));
    assert_eq!(paused["paused"], run_id, "{paused}");
    std::thread::sleep(Duration::from_millis(1600)); // past the sleep deadline
    let view = artifact_json(&command(
        &addr,
        4,
        "workflow.status",
        json!({"run": run_id}),
    ))["runs"][0]
        .clone();
    assert_ne!(
        view["status"], "completed",
        "paused runs don't advance: {view}"
    );

    // Resume → completes.
    let resumed = rpc(&addr, 5, "a2a.resume", json!({"run": run_id}));
    assert_eq!(resumed["resumed"], run_id, "{resumed}");
    wait_run(&addr, &run_id, 10, "completion after resume", |v| {
        v["status"] == "completed"
    });
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_global_pause_holds_new_work_and_resume_releases_it() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "Answered after the hold."}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&steer_config(&llm.uri, port, ""));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    // Pause the instance; intake continues but nothing dispatches.
    let paused = rpc(&addr, 1, "a2a.pause", json!({}));
    assert_eq!(paused["state"], "paused");
    let st = command(&addr, 2, "status", json!({}));
    assert_eq!(artifact_json(&st)["paused"], true, "{st}");

    let sent = rpc(
        &addr,
        3,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "Hello during the pause"}]},
               "configuration": {"blocking": false}}),
    );
    let task_id = sent["task"]["id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(900));
    let held = rpc(&addr, 4, "GetTask", json!({"id": task_id}));
    assert_eq!(
        held["status"]["state"], "TASK_STATE_WORKING",
        "the turn is queued, not run, while paused: {held}"
    );

    // Resume → the queued turn dispatches and completes.
    rpc(&addr, 5, "a2a.resume", json!({}));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let t = rpc(&addr, 6, "GetTask", json!({"id": task_id}));
        if t["status"]["state"] == "TASK_STATE_COMPLETED" {
            assert!(
                t["artifacts"][0]["parts"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("after the hold"),
                "{t}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "resume released the turn: {t}");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn subagent_send_injects_into_a_warm_subagent_and_plan_get_reads_the_plan() {
    // The root spawns a WARM subagent, then the e2e steers it over A2A.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {"instruction": "stand by for instructions", "mode": "warm"}}]},
            {"content": "Warm helper started."}
        ],
        "match": [
            {"when_contains": "You are agentd, an autonomous agent.", "content": "standing by"}
        ]
    }));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&steer_config(&llm.uri, port, ""));
    let _daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "Start a warm helper"}]}}),
    );
    assert_eq!(
        sent["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );
    // Find the warm handle.
    let st = command(&addr, 2, "status", json!({}));
    let subs = artifact_json(&st)["subagents"].clone();
    let handle = subs[0]["handle"]
        .as_str()
        .expect("warm subagent")
        .to_string();

    // Steer it: inject a message over A2A.
    let injected = command(
        &addr,
        3,
        "subagent.send",
        json!({"handle": handle, "message": "focus on the staging cluster"}),
    );
    assert_eq!(artifact_json(&injected)["ok"], true, "{injected}");

    // Unknown handle → clean error.
    let body = json!({"jsonrpc": "2.0", "id": 4, "method": "SendMessage", "params": {"message": {"messageId": "m4", "parts": [{"data": {"agentd": {"op": "subagent.send", "handle": "nope", "message": "x"}}}]}}}).to_string();
    let resp: Value = serde_json::from_str(&post_raw(&addr, &body)).unwrap();
    assert_eq!(resp["error"]["code"], -32602, "{resp}");

    // plan.get on the root conversation (operator).
    let plan = command(&addr, 5, "plan.get", json!({}));
    assert!(
        artifact_json(&plan).get("plan").is_some(),
        "plan.get answers (plan may be null): {plan}"
    );
    std::fs::remove_file(&cfg).ok();
}
