// SPDX-License-Identifier: AGPL-3.0-only
//! **A durable task id must not be re-minted by the next life, and the `config`
//! command must not echo a credential.** Two properties of the A2A surface that
//! only a real daemon can show, one of them across a restart.
//!
//! *Task ids* must not come from `task-<seq>` over a process-local counter that
//! starts at 0 — the tasks of the previous life come back from the store, so the
//! id the listener pre-mints for a new message ("task-1") can already name a
//! RESTORED task, and `SendMessage` then reads the message as a continuation of
//! it: the caller is handed someone else's task and history, and that task's
//! state is advanced by an unrelated message. The store here is therefore a mock
//! MCP server that outlives both lives — with an in-memory store there is
//! nothing to collide with and the test would pass against the very failure it
//! exists to catch.
//!
//! *The `config` command* must not answer with the raw merged settings document.
//! A credential supplied by env or flag sits INLINE in that document (only a
//! FILE is held to `{{secret:…}}` references), so such a reply would put live
//! credentials on the wire — which the secret discipline forbids on every
//! surface. The assertion is the blunt one: the token text appears NOWHERE in
//! the response bytes.
#![cfg(all(
    unix,
    feature = "a2a",
    any(feature = "internal-mocks", debug_assertions)
))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The credentials this suite plants. Distinctive enough that a substring
/// search over the whole response is meaningful.
const ENV_TOKEN: &str = "sk-env-token-must-never-be-echoed";
const HEADER_TOKEN: &str = "sk-header-token-must-never-be-echoed";
/// The header the credential rides in. Deliberately NOT `Authorization`:
/// `is_secret_shaped_key` recognises that name, so config validation refuses an
/// inline value for it in ANY layer and the daemon exits 2 before it listens.
/// `Proxy-Authorization` is exactly as credential-bearing and the name check
/// does not know it — which is the case redaction has to cover, and the reason
/// the view redacts every header value by construction instead of consulting a
/// list of names.
const HEADER_NAME: &str = "Proxy-Authorization";

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

/// One HTTP POST of a JSON-RPC body; returns the response body once the
/// connection closes. The read timeout is generous because a blocking
/// `SendMessage` waits for its task to settle.
fn post_raw(addr: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.set_read_timeout(Some(Duration::from_secs(60))).ok();
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

/// A JSON-RPC call over A2A, returning the RAW response text — the redaction
/// assertion is about the bytes on the wire, not about a parsed field.
fn rpc_raw(addr: &str, id: i64, method: &str, params: Value) -> String {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    post_raw(addr, &body)
}

/// A JSON-RPC call over A2A; returns the `result` (panics on a transport/RPC
/// error, surfacing the body for diagnosis).
fn rpc(addr: &str, id: i64, method: &str, params: Value) -> Value {
    let resp = rpc_raw(addr, id, method, params);
    let v: Value =
        serde_json::from_str(&resp).unwrap_or_else(|_| panic!("non-JSON A2A response: {resp:?}"));
    assert!(v.get("error").is_none(), "A2A rpc error for {method}: {v}");
    v["result"].clone()
}

/// Block until the listener accepts. The daemon comes along so that a failure
/// reports WHY: a config the loader refuses exits 2 long before it binds, and
/// "never became connectable" on its own says nothing about that.
fn wait_ready(d: &Daemon, addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a2a listener never became connectable\nstderr:\n{}",
            d.stderr()
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
    let pb = common::unique_path("a2a-restart-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("a2a-restart-mock-llm", "addr");
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
    /// Drain and wait for the process to be GONE — the next life binds the same
    /// port and reads the same store, so overlapping them would test nothing.
    fn shutdown(mut self) {
        sigterm(self.child.id());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not exit on SIGTERM\nstderr:\n{}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
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

fn spawn_daemon(config: &str, env: &[(&str, &str)]) -> Daemon {
    let stderr_path = common::unique_path("a2a-restart-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn agentd a2a daemon");
    Daemon { child, stderr_path }
}

fn write_config(tag: &str, yaml: &str) -> String {
    let path = common::unique_path(tag, "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

/// The text of the task's `.result` artifact (the model's answer, or a
/// command's JSON result).
fn result_artifact(task: &Value) -> String {
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
fn a_new_message_after_a_restart_gets_its_own_task_not_a_restored_one() {
    // One answer per life, selected by the question rather than by turn index
    // (the playbook's turn cursor counts tool results, not requests).
    let llm = spawn_mock_llm(&json!({"match": [
        {"when_contains": "second-life-question", "content": "answer-from-the-second-life"},
        {"when_contains": "first-life-question", "content": "answer-from-the-first-life"}
    ]}));
    // The store outlives both lives; that is what makes a restored task exist.
    let store = common::spawn_mock_mcp("mock://noop", false);
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(
        "a2a-restart-ids",
        &format!(
            "config_version: \"1\"\n\
             agent:\n  name: a2a-restart\n  instruction: You are a test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: {}\n  model: mock\n\
             mcp:\n  servers:\n    - name: store\n      endpoint: {}\n\
             store:\n  kind: mcp\n  mcp:\n    server: store\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: warn\n",
            llm.uri,
            store.uri()
        ),
    );

    // Life 1: one natural-language message, one durable task.
    let life1 = spawn_daemon(&cfg, &[]);
    wait_ready(&life1, &addr);
    let params =
        json!({"message": {"messageId": "m1", "parts": [{"text": "first-life-question"}]}});
    let first = rpc(&addr, 1, "SendMessage", params)["task"].clone();
    let first_id = first["id"].as_str().unwrap_or_default().to_string();
    assert!(!first_id.is_empty(), "life 1 task: {first}");
    assert!(
        result_artifact(&first).contains("answer-from-the-first-life"),
        "life 1 answer: {first}"
    );
    life1.shutdown();

    // Life 2: the same store, so the task above comes back.
    let life2 = spawn_daemon(&cfg, &[]);
    wait_ready(&life2, &addr);
    let restored = rpc(&addr, 2, "GetTask", json!({"id": first_id.clone()}));
    assert_eq!(
        restored["id"], first_id,
        "the task really was restored — without that this test proves nothing: {restored}"
    );

    // The failure this guards against: the listener pre-mints the id for this
    // message, and a counter-minted one collides with the restored task above,
    // so the message silently continues THAT task instead of starting its own.
    let params =
        json!({"message": {"messageId": "m2", "parts": [{"text": "second-life-question"}]}});
    let second = rpc(&addr, 3, "SendMessage", params)["task"].clone();
    let second_id = second["id"].as_str().unwrap_or_default().to_string();
    assert_ne!(
        second_id, first_id,
        "a new message must start a NEW task, not join the restored one: {second}"
    );
    assert!(
        result_artifact(&second).contains("answer-from-the-second-life"),
        "life 2 answer: {second}"
    );

    // And the restored task is untouched: its history is still its own.
    let after = rpc(&addr, 4, "GetTask", json!({"id": first_id.clone()}));
    assert!(
        result_artifact(&after).contains("answer-from-the-first-life"),
        "the restored task kept its own result: {after}"
    );
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn the_config_command_never_echoes_a_credential() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    // The intelligence endpoint is deliberately dead: `config` is answered from
    // durable state, and `preflight: never` means nothing dials the model — the
    // header below only has to be CONFIGURED, never sent.
    let cfg = write_config(
        "a2a-restart-config",
        &format!(
            "config_version: \"1\"\n\
             agent:\n  name: a2a-redact\n  instruction: You are a test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: https://127.0.0.1:9\n  model: mock\n  \
             headers:\n    {HEADER_NAME}: \"Bearer {HEADER_TOKEN}\"\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: warn\n"
        ),
    );
    // The token comes from the ENVIRONMENT, which is exactly the layer a file
    // may not use, and the one that lands inline in the merged doc.
    let daemon = spawn_daemon(&cfg, &[("AGENTD_INTELLIGENCE_TOKEN", ENV_TOKEN)]);
    wait_ready(&daemon, &addr);

    let params =
        json!({"message": {"messageId": "m1", "parts": [{"data": {"agentd": {"op": "config"}}}]}});
    let raw = rpc_raw(&addr, 1, "SendMessage", params);
    assert!(
        !raw.contains(ENV_TOKEN),
        "the env-supplied intelligence token was echoed over A2A: {raw}"
    );
    assert!(
        !raw.contains(HEADER_TOKEN),
        "the configured Authorization header was echoed over A2A: {raw}"
    );

    // The view is still the effective configuration, minus the credentials.
    let v: Value = serde_json::from_str(&raw).unwrap_or_else(|_| panic!("non-JSON: {raw:?}"));
    let doc: Value = serde_json::from_str(&result_artifact(&v["result"]["task"]))
        .unwrap_or_else(|e| panic!("the config result is not JSON ({e}): {v}"));
    assert_eq!(doc["config"]["intelligence"]["token"], "***", "{doc}");
    assert_eq!(
        doc["config"]["intelligence"]["headers"][HEADER_NAME], "***",
        "{doc}"
    );
    assert_eq!(doc["config"]["intelligence"]["model"], "mock", "{doc}");
    assert_eq!(doc["config"]["agent"]["name"], "a2a-redact", "{doc}");

    // Nothing was leaked into the daemon's own telemetry either.
    assert!(
        !daemon.stderr().contains(ENV_TOKEN),
        "the token reached the log: {}",
        daemon.stderr()
    );

    std::fs::remove_file(&cfg).ok();
}
