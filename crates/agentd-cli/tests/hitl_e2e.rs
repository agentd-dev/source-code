// SPDX-License-Identifier: AGPL-3.0-only
//! **Human-in-the-loop** end to end (RFC 0032 §16): the `ask_human` tool and
//! the workflow `human` node gate through the interface as `input-required`
//! A2A tasks; a `SendMessage` carrying the `taskId` resolves the suspended
//! asker with the reply text. With no interface, the configured fallback
//! applies — `fail` errors the ask immediately (the model carries on), `auto`
//! has an LLM judge answer on the operator's behalf (marked as auto). A
//! cancelled gate unblocks its asker with an error.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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
        json!({"message": {"messageId": format!("m-{id}"), "parts": [{"data": {"agentd": data}}]}}),
    )
}

/// Poll GetTask until `pred` holds (returns the task).
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
    let pb = common::unique_path("hitl-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("hitl-mock-llm", "addr");
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
fn spawn_daemon(config: &str) -> Daemon {
    let stderr_path = common::unique_path("hitl-daemon", "log");
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

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-hitl", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

/// Spawn the daemon on a probed free port and return the authority IT actually
/// bound. The probe→bind gap is a real race under parallel CI (another process
/// can take the port), so a daemon whose bind lost is retried on a fresh port
/// rather than leaving the test talking to a stranger's listener.
fn spawn_bound(cfg_for: impl Fn(u16) -> String) -> (Daemon, String, String) {
    spawn_bound_with(cfg_for, spawn_daemon)
}

fn spawn_bound_with(
    cfg_for: impl Fn(u16) -> String,
    spawn: impl Fn(&str) -> Daemon,
) -> (Daemon, String, String) {
    for _ in 0..5 {
        let cfg = write_config(&cfg_for(free_port()));
        let daemon = spawn(&cfg);
        if let Some(addr) = common::try_a2a_bound(&daemon.stderr_path, Duration::from_secs(15)) {
            return (daemon, addr, cfg);
        }
        std::fs::remove_file(&cfg).ok();
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

fn base_config(llm: &str, port: u16, interface: bool, extra: &str) -> String {
    let iface = if interface {
        "interface:\n  enabled: true\n  debug: true\n"
    } else {
        ""
    };
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: hitl-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         {iface}lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n{extra}"
    )
}

#[test]
fn a_turn_ask_gates_as_input_required_and_the_reply_resumes_the_turn() {
    // Turn 1: the model asks the human; turn 2 (after the tool result): final.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Which color should the rollout badge be?"}}]},
            {"content": "Done — the badge is set."}
        ]
    }));
    let (daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, true, ""));

    // Send the prompt WITHOUT blocking; the task must reach input-required
    // with the QUESTION as its status message.
    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "Set up the badge"}]},
               "configuration": {"blocking": false}}),
    );
    let task_id = sent["task"]["id"].as_str().unwrap().to_string();
    let gated = wait_task(&addr, &task_id, 10, "gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    assert!(
        gated["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Which color"),
        "{gated}"
    );

    // The answer (carrying the taskId) resolves the gate; the turn finishes.
    let answered = rpc(
        &addr,
        2,
        "SendMessage",
        json!({"message": {"messageId": "m2", "taskId": task_id, "parts": [{"text": "blue"}]},
               "configuration": {"blocking": false}}),
    );
    assert_eq!(
        answered["task"]["status"]["state"], "TASK_STATE_WORKING",
        "back to working after the answer: {answered}"
    );
    let done = wait_task(&addr, &task_id, 15, "turn completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    let text = done["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.contains("badge is set"), "{done}");
    // The audit trail marks the human answer.
    let logs = daemon.stderr();
    assert!(logs.contains("human.ask"), "asked: {logs}");
    assert!(logs.contains("human.answered"), "answered: {logs}");
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_workflow_human_node_gates_the_run_task_and_the_reply_is_the_step_output() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let extra = "workflows:\n  - name: approve\n    steps:\n      s: {kind: manual}\n      gate: {kind: human, question: \"Approve the deploy?\", depends_on: [s]}\n      f: {kind: finish, depends_on: [gate], output: \"shipped\"}\n";
    let (_daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, true, extra));

    // Start the workflow; ITS task (linking the run) becomes the gate.
    let started = command(&addr, 1, "workflow.run", json!({"name": "approve"}));
    let task_id = started["task"]["id"].as_str().unwrap().to_string();
    let gated = wait_task(&addr, &task_id, 10, "run gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    assert!(
        gated["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Approve the deploy?"),
        "{gated}"
    );

    // Answer → the human step completes with the reply as output; the run
    // finishes and drives the task terminal.
    rpc(
        &addr,
        2,
        "SendMessage",
        json!({"message": {"messageId": "m2", "taskId": task_id, "parts": [{"text": "yes, ship it"}]},
               "configuration": {"blocking": false}}),
    );
    let done = wait_task(&addr, &task_id, 10, "run completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert!(
        done["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("shipped"),
        "{done}"
    );
    // Per-step detail (debug read): the gate step is Done with the reply.
    let ws = command(&addr, 3, "workflow.status", json!({}));
    let run_id = ws["task"]["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .and_then(|v| v["runs"][0]["run"].as_str().map(str::to_string))
        .expect("run id");
    let run = command(&addr, 4, "run.get", json!({"run": run_id}));
    assert_eq!(run["run"]["steps"]["gate"]["status"], "done", "{run}");
    assert_eq!(
        run["run"]["steps"]["gate"]["output"], "yes, ship it",
        "{run}"
    );
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn fallback_fail_errors_the_ask_immediately_and_the_model_carries_on() {
    // No interface block at all (fallback default = fail): the ask errors,
    // the model still gets its next turn and completes.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Anyone there?"}}]},
            {"content": "Proceeding without a human."}
        ]
    }));
    let (daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, false, ""));
    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "Try asking"}]}}),
    );
    assert_eq!(sent["task"]["status"]["state"], "TASK_STATE_COMPLETED");
    assert!(
        sent["task"]["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Proceeding without a human"),
        "{sent}"
    );
    // No gate ever existed.
    let tasks = rpc(&addr, 2, "ListTasks", json!({}));
    assert!(
        tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["state"] != "TASK_STATE_INPUT_REQUIRED"),
        "{tasks}"
    );
    assert!(daemon.stderr().contains("ask_human"), "the error is logged");
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn fallback_auto_lets_the_judge_answer_on_the_operators_behalf() {
    // The judge dial hits the same mock: route it by its system prompt.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Blue or green?"}}]},
            {"content": "Went ahead with the chosen color."}
        ],
        "match": [
            {"when_contains": "answering ON BEHALF OF the unavailable human operator", "content": "blue"}
        ]
    }));
    let extra = "";
    let (daemon, addr, cfg) = spawn_bound(|port| {
        base_config(&llm.uri, port, false, extra).replace(
            "  preflight: never\n",
            "  preflight: never\n  ask_human_fallback: auto\n",
        )
    });
    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "Pick a color and proceed"}]}}),
    );
    assert_eq!(
        sent["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );
    assert!(
        sent["task"]["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Went ahead"),
        "{sent}"
    );
    let logs = daemon.stderr();
    assert!(logs.contains("human.judge.start"), "{logs}");
    assert!(
        logs.contains("\"via\":\"auto\"") || logs.contains("\"outcome\":\"auto\""),
        "the auto answer is marked: {logs}"
    );
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn fallback_wait_parks_the_ask_until_its_timeout() {
    // No interface + `wait`: the ask parks, times out (1s here), errors, and
    // the model carries on.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Waiting?", "timeout": "1s"}}]},
            {"content": "Timed out; proceeding."}
        ]
    }));
    let (daemon, addr, cfg) = spawn_bound(|port| {
        base_config(&llm.uri, port, false, "").replace(
            "  preflight: never\n",
            "  preflight: never\n  ask_human_fallback: wait\n",
        )
    });
    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "Ask and wait"}]}}),
    );
    assert_eq!(
        sent["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );
    assert!(
        sent["task"]["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Timed out; proceeding"),
        "{sent}"
    );
    let logs = daemon.stderr();
    assert!(logs.contains("human.ask.parked"), "{logs}");
    assert!(logs.contains("no answer within the timeout"), "{logs}");
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn auto_fires_as_the_safety_net_when_an_interface_gate_times_out_unanswered() {
    // Interface ON + `auto`: the gate renders for humans, nobody answers
    // within the (1s) timeout, the judge answers on the operator's behalf.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Green or blue?", "timeout": "1s"}}]},
            {"content": "Color applied."}
        ],
        "match": [
            {"when_contains": "answering ON BEHALF OF the unavailable human operator", "content": "green"}
        ]
    }));
    let (daemon, addr, cfg) = spawn_bound(|port| {
        base_config(&llm.uri, port, true, "").replace(
            "  preflight: never\n",
            "  preflight: never\n  ask_human_fallback: auto\n",
        )
    });
    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "parts": [{"text": "Choose"}]},
               "configuration": {"blocking": false}}),
    );
    let task_id = sent["task"]["id"].as_str().unwrap().to_string();
    // The gate appears first (a human COULD answer)…
    wait_task(&addr, &task_id, 10, "gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    // …then the judge takes over and the turn completes.
    let done = wait_task(&addr, &task_id, 20, "auto answer + completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert!(
        done["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Color applied"),
        "{done}"
    );
    let logs = daemon.stderr();
    assert!(logs.contains("human.judge.start"), "{logs}");
    assert!(logs.contains("auto"), "marked auto: {logs}");
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn cancelling_a_gate_unblocks_the_asker_with_an_error() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let extra = "workflows:\n  - name: gated\n    steps:\n      s: {kind: manual}\n      gate: {kind: human, question: \"Proceed?\", depends_on: [s]}\n      f: {kind: finish, depends_on: [gate], output: \"done\"}\n";
    let (_daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, true, extra));
    let started = command(&addr, 1, "workflow.run", json!({"name": "gated"}));
    let task_id = started["task"]["id"].as_str().unwrap().to_string();
    wait_task(&addr, &task_id, 10, "gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    let cancelled = rpc(&addr, 2, "CancelTask", json!({"id": task_id}));
    assert_eq!(
        cancelled["status"]["state"], "TASK_STATE_CANCELED",
        "{cancelled}"
    );
    // The run resolved (the gate step failed / the run was cancelled) — it is
    // terminal, not stuck.
    let ws = command(&addr, 3, "workflow.status", json!({}));
    let status = ws["task"]["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .and_then(|v| v["runs"][0]["status"].as_str().map(str::to_string))
        .expect("run status");
    assert!(
        status == "cancelled" || status == "failed",
        "the gated run is terminal: {status}"
    );
    std::fs::remove_file(&cfg).ok();
}
