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

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{SendMessage, rpc_result as rpc};

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
    /// Drain and wait for the process to be GONE — the next life reads the
    /// same store, so overlapping them would test nothing.
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

/// Spawn a daemon on `cfg_for(port)` and return it with the A2A authority it
/// confirmed binding and the config path to clean up — see
/// [`common::spawn_bound`]. Each life gets its own: the store is what carries
/// a task across a restart, not the port.
fn spawn_bound(
    tag: &str,
    cfg_for: impl Fn(u16) -> String,
    env: &[(&str, &str)],
) -> (Daemon, String, String) {
    let cfg = common::unique_path(tag, "yaml");
    let (daemon, addr) = common::spawn_bound(|port| {
        std::fs::write(&cfg, cfg_for(port)).unwrap();
        let daemon = spawn_daemon(&cfg, env);
        let log = daemon.stderr_path.clone();
        (daemon, log)
    });
    (daemon, addr, cfg)
}

/// The text of the task's `.result` artifact (the model's answer).
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
    let cfg_for = |port: u16| {
        format!(
            "\
             agent:\n  name: a2a-restart\n  instruction: You are a test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: {}\n  model: mock\n\
             mcp:\n  servers:\n    - name: store\n      endpoint: {}\n\
             store:\n  kind: mcp\n  mcp:\n    server: store\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n",
            llm.uri,
            store.uri()
        )
    };

    // Life 1: one natural-language message, one durable task.
    let (life1, addr, cfg) = spawn_bound("a2a-restart-ids", cfg_for, &[]);
    let first = SendMessage::text("first-life-question").result(&addr)["task"].clone();
    let first_id = first["id"].as_str().unwrap_or_default().to_string();
    assert!(!first_id.is_empty(), "life 1 task: {first}");
    assert!(
        result_artifact(&first).contains("answer-from-the-first-life"),
        "life 1 answer: {first}"
    );
    life1.shutdown();
    std::fs::remove_file(&cfg).ok();

    // Life 2: the same store, so the task above comes back.
    let (_life2, addr, cfg) = spawn_bound("a2a-restart-ids", cfg_for, &[]);
    let restored = rpc(&addr, 2, "GetTask", json!({"id": first_id.clone()}));
    assert_eq!(
        restored["id"], first_id,
        "the task really was restored — without that this test proves nothing: {restored}"
    );

    // The failure this guards against: the listener pre-mints the id for this
    // message, and a counter-minted one collides with the restored task above,
    // so the message silently continues THAT task instead of starting its own.
    let second = SendMessage::text("second-life-question").result(&addr)["task"].clone();
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
    // The intelligence endpoint is deliberately dead: `config` is answered from
    // durable state, and `preflight: never` means nothing dials the model — the
    // header below only has to be CONFIGURED, never sent. The token comes from
    // the ENVIRONMENT, which is exactly the layer a file may not use, and the
    // one that lands inline in the merged doc.
    let (daemon, addr, cfg) = spawn_bound(
        "a2a-restart-config",
        |port| {
            format!(
                "\
                 agent:\n  name: a2a-redact\n  instruction: You are a test agent.\n  preflight: never\n\
                 intelligence:\n  endpoints: https://127.0.0.1:9\n  model: mock\n  \
                 headers:\n    {HEADER_NAME}: \"Bearer {HEADER_TOKEN}\"\n\
                 store:\n  kind: memory\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n\
                 lifecycle:\n  run_until: drained\n\
                 observability:\n  log_level: info\n"
            )
        },
        &[("AGENTD_INTELLIGENCE_TOKEN", ENV_TOKEN)],
    );

    // The redaction assertion is about the bytes on the wire, not a parsed field.
    let raw = SendMessage::command("config", json!({}))
        .post_raw(&addr)
        .body;
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
    // A read: the document is the Message's data part.
    let doc = v["result"]["message"]["parts"][0]["data"].clone();
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
