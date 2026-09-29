// SPDX-License-Identifier: AGPL-3.0-only
//! agentd **A2A transport** end to end: the daemon binds the real HTTPS
//! listener (plaintext loopback here, so the wiring is exercised through the
//! actual binary without cert plumbing — mTLS is covered by net's tls_server
//! tests and the resolver unit tests). A JSON-RPC peer drives the surface: a
//! `status` command DataPart completes deterministically; a natural language
//! message runs a turn worker (mock LLM) and the answer lands as the task
//! artifact; `GetTask`/`ListTasks` read it back; `workflow.run` starts a run
//! and the task tracks it to completion.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{SendMessage, get_card, rpc_result as rpc};

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
    let pb = common::unique_path("a2a-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("a2a-mock-llm", "addr");
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

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-a2a", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    /// The daemon's captured stderr (JSON-lines telemetry) so far.
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
    let stderr_path = common::unique_path("a2a-daemon", "log");
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

/// Spawn the daemon on `cfg_for(port)` and return it with the A2A authority it
/// confirmed binding and the config path to clean up. Ready means answering:
/// `proc.ready` is logged after the workflows load, and the listener binds
/// before that, so a `workflow.run` arriving in between would be answered
/// `no such workflow` — see [`common::spawn_bound`].
fn spawn_bound(cfg_for: impl Fn(u16) -> String) -> (Daemon, String, String) {
    let cfg = common::unique_path("a2a-e2e", "yaml");
    let (daemon, addr) = common::spawn_bound(|port| {
        std::fs::write(&cfg, cfg_for(port)).unwrap();
        let daemon = spawn_daemon(&cfg);
        let log = daemon.stderr_path.clone();
        (daemon, log)
    });
    (daemon, addr, cfg)
}

/// A daemon that serves A2A over plaintext loopback (⇒ operator), no preflight,
/// backed by the mock LLM and an in-memory store.
fn a2a_config(llm: &str, port: u16, extra: &str) -> String {
    format!(
        "\
         agent:\n  name: a2a-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n{extra}"
    )
}

fn text_part_answer(task: &Value) -> String {
    task["artifacts"]
        .as_array()
        .and_then(|a| {
            a.iter().find(|x| {
                x["artifactId"]
                    .as_str()
                    .is_some_and(|s| s.ends_with(".result"))
            })
        })
        .and_then(|x| x["parts"][0]["text"].as_str())
        .unwrap_or("")
        .to_string()
}

#[test]
fn a_status_command_over_a2a_is_a_message_without_a_task_or_a_model_turn() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (_daemon, addr, cfg) = spawn_bound(|port| a2a_config(&llm.uri, port, ""));

    // A `status` command DataPart is answered deterministically, as the
    // spec's immediate reply: an agent Message carrying the document, and no
    // task to track.
    let result = SendMessage::command("status", json!({})).result(&addr);
    assert!(
        result.get("task").is_none(),
        "a read creates no task: {result}"
    );
    let message = &result["message"];
    assert_eq!(message["role"], "ROLE_AGENT", "{result}");
    assert_eq!(
        message["extensions"][0],
        agentd::runtime::surface::COMMAND_EXTENSION
    );
    let doc = &message["parts"][0]["data"];
    assert!(
        doc["conversations"].is_array() && doc["runs"].is_array(),
        "status document: {doc}"
    );

    // The agent card is discoverable without a principal.
    let card = get_card(&addr);
    assert_eq!(card["name"], "a2a-e2e", "the configured agent.name");
    assert_eq!(card["capabilities"]["streaming"], true);

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_natural_language_message_runs_a_turn_and_the_answer_is_the_task_artifact() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "Hello over A2A!"}]}));
    let (_daemon, addr, cfg) = spawn_bound(|port| a2a_config(&llm.uri, port, ""));

    // A natural-language message → a conversation turn → a completed task whose
    // artifact carries the model's answer (blocking send waits for it).
    let result = SendMessage::text("Say hello").result(&addr);
    let task = &result["task"];
    let task_id = task["id"].as_str().unwrap().to_string();
    assert_eq!(
        task["status"]["state"], "TASK_STATE_COMPLETED",
        "nl task: {task}"
    );
    assert!(
        text_part_answer(task).contains("Hello over A2A"),
        "answer artifact: {task}"
    );

    // GetTask reads the same terminal task back.
    let got = rpc(&addr, 2, "GetTask", json!({"id": task_id}));
    assert_eq!(got["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(got["id"], task_id);

    // ListTasks (operator sees it).
    let listed = rpc(&addr, 3, "ListTasks", json!({}));
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == task_id),
        "the task is listed: {listed}"
    );

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_workflow_run_command_starts_a_run_and_the_task_tracks_it_to_completion() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    // A one-shot workflow: a manual start into a finish. `workflow.run` kicks it.
    let extra = "workflows:\n  - name: greet\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], output: \"done\"}\n";
    let (_daemon, addr, cfg) = spawn_bound(|port| a2a_config(&llm.uri, port, extra));

    // A `workflow.run` command DataPart starts the run; its task begins working.
    let result = SendMessage::command("workflow.run", json!({"workflow": "greet"})).result(&addr);
    let task_id = result["task"]["id"].as_str().unwrap().to_string();

    // Poll GetTask until the run completes and the task tracks it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut state = String::new();
    while Instant::now() < deadline {
        let got = rpc(&addr, 2, "GetTask", json!({"id": task_id}));
        state = got["status"]["state"].as_str().unwrap_or("").to_string();
        if state == "TASK_STATE_COMPLETED" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        state, "TASK_STATE_COMPLETED",
        "the workflow.run task completed"
    );

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn capabilities_describes_the_a2a_surface_without_side_effects() {
    // No reachable intelligence, cert files that don't exist — `--capabilities`
    // is pure config introspection and must not connect, read secrets, or block.
    let port = free_port();
    let cfg = write_config(&a2a_config("https://127.0.0.1:9", port, ""));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg, "--capabilities"])
        .stdin(Stdio::null())
        .output()
        .expect("run --capabilities");
    assert!(
        out.status.success(),
        "exit {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("capabilities json");
    assert_eq!(v["version"], agentd::VERSION);
    assert!(
        v.get("runtime").is_none() && v.get("surfaces").is_none(),
        "{v}"
    );
    assert_eq!(v["a2a"]["url"], format!("http://127.0.0.1:{port}/"));
    let methods = v["a2a"]["methods"].as_array().unwrap();
    assert!(methods.iter().any(|m| m == "SendMessage") && methods.iter().any(|m| m == "GetTask"));
    assert!(
        v["a2a"]["auth"]["implicit_operator"].as_bool().unwrap(),
        "no principals on a loopback bind ⇒ the implicit operator"
    );
    assert!(
        v["internal_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "workflow.run")
    );
    assert_eq!(v["store"], "memory");

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a2a_calls_are_audited_when_the_audit_log_sink_is_on() {
    // The audit stream: every A2A call is recorded as an `audit` event
    // carrying the principal, action (method:op), and outcome.
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (daemon, addr, cfg) =
        spawn_bound(|port| a2a_config(&llm.uri, port, "  audit:\n    sink: [log]\n"));

    // A deterministic `status` command drives one A2A call → one audit event.
    let _ = SendMessage::command("status", json!({})).result(&addr);

    // The audit line lands on the daemon's JSON-lines stderr.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut audit = None;
    while Instant::now() < deadline && audit.is_none() {
        for line in daemon.stderr().lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && v["event"] == "audit"
            {
                audit = Some(v);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let a = audit.expect("an audit event was emitted for the A2A call");
    let action = a["action"].as_str().unwrap_or("");
    assert!(
        action.starts_with("a2a.SendMessage"),
        "action names the method: {a}"
    );
    assert!(
        action.contains("status"),
        "action names the command op: {a}"
    );
    assert_eq!(
        a["principal"], "operator",
        "plaintext loopback ⇒ operator: {a}"
    );
    assert_eq!(a["role"], "operator");
    assert_eq!(a["outcome"], "ok");

    std::fs::remove_file(&cfg).ok();
}
