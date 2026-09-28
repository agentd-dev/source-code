// SPDX-License-Identifier: AGPL-3.0-only
//! **What a2a-rs 0.10 changed on the wire, and what it must not have.**
//!
//! agentd answers the specification's methods through a2a-rs, so a version bump
//! of that library is a change to agentd's protocol surface even when not a
//! line of agentd's own code moves. These tests pin the two places 0.10 reaches
//! agentd, against the real listener:
//!
//! * **`SubscribeToTask` on a settled task is refused** with the spec's
//!   -32004, as plain JSON. 0.6 opened a stream that carried nothing and closed
//!   — indistinguishable, for a client, from an agent that has yet to speak. A
//!   `Last-Event-ID` still replays: reconnecting to a stream the caller already
//!   held is exactly when the missed events matter, and the task having
//!   finished in the meantime is the usual reason they were missed.
//! * **A send's own task is watchable.** 0.10 reads the task a send names
//!   before it attaches the send's subscription, and the name is the id the
//!   listener pre-minted for a task that does not exist yet — so the read finds
//!   nothing. Taken as a refusal, that fails the attach: a blocking send stops
//!   waiting and answers `WORKING`, and a streaming send is refused as "task
//!   not found" for the task it was about to create. `ports::StreamAuthz::
//!   record_read` is the guard, and these are the tests that fail without it.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use common::{SendMessage, a2a_post, a2a_post_within, rpc_body};

/// The answer the mock model gives, so a replay can be checked for it.
const ANSWER: &str = "the-settled-answer";

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

/// A mock model that answers every turn with [`ANSWER`].
fn spawn_mock_llm() -> MockLlm {
    let pb = common::unique_path("sdk-bump-playbook", "json");
    std::fs::write(&pb, json!({"turns": [{"content": ANSWER}]}).to_string()).unwrap();
    let addr_file = common::unique_path("sdk-bump-mock-llm", "addr");
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
    cfg: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
        let _ = std::fs::remove_file(&self.cfg);
    }
}

/// A daemon on a loopback port with no principals — the loopback caller is
/// the implicit operator, so every request is the owner's.
fn boot(llm: &MockLlm) -> (Daemon, String) {
    let port = common::free_port();
    let cfg = common::unique_path("a2a-sdk-bump", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: a2a-sdk-bump\n  instruction: You are a test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: {}\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n",
            llm.uri, port
        ),
    )
    .unwrap();
    let stderr_path = common::unique_path("a2a-sdk-bump-daemon", "log");
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
        .spawn()
        .expect("spawn agentd");
    let addr = common::wait_a2a_bound(&stderr_path);
    (
        Daemon {
            child,
            stderr_path,
            cfg,
        },
        addr,
    )
}

/// The task a settled send returned, checked to have settled.
fn completed_task(sent: &Value) -> (String, Value) {
    let task = sent["task"].clone();
    assert_eq!(
        task["status"]["state"], "TASK_STATE_COMPLETED",
        "a blocking send waits for its task to settle: {sent}"
    );
    let id = task["id"].as_str().expect("a task id").to_string();
    (id, task)
}

#[test]
fn subscribe_to_a_completed_task() {
    let llm = spawn_mock_llm();
    let (daemon, addr) = boot(&llm);
    let (id, _) = completed_task(&SendMessage::text("hello").result(&addr));

    // Without Last-Event-ID: the spec's "unsupported operation", as JSON — not
    // a stream that opens and closes with nothing in it.
    let refused = a2a_post(
        &addr,
        &rpc_body(2, "SubscribeToTask", json!({"id": id})),
        &[],
    );
    assert!(
        refused
            .header("content-type")
            .is_some_and(|t| t.contains("application/json")),
        "a settled task's subscribe is answered, not streamed: {refused:?}"
    );
    let v = refused.json();
    assert_eq!(
        v["error"]["code"],
        -32004,
        "subscribing to a settled task is refused: {v}\n{}",
        daemon.stderr()
    );
    assert_eq!(v["id"], 2, "the refusal answers the request: {v}");

    // With Last-Event-ID: 0 the stored events replay — the reconnect case.
    let replay = a2a_post_within(
        &addr,
        &rpc_body(3, "SubscribeToTask", json!({"id": id})),
        &[("Last-Event-ID", "0")],
        Duration::from_secs(10),
    );
    assert!(
        replay
            .header("content-type")
            .is_some_and(|t| t.contains("text/event-stream")),
        "a resumed subscribe is a stream: {replay:?}"
    );
    assert!(
        replay.body.contains("data:") && replay.body.contains(&id),
        "the stored events replay for the task: {replay:?}"
    );
}

/// The blocking half of the guard: the send holds the connection until the
/// task settles, which it can only do if its subscription attached.
#[test]
fn a_blocking_send_waits_for_the_task_it_creates() {
    let llm = spawn_mock_llm();
    let (_daemon, addr) = boot(&llm);
    let (_, task) = completed_task(&SendMessage::text("hello").result(&addr));
    assert!(
        task["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains(ANSWER)),
        "the settled task carries the answer: {task}"
    );
}

/// The streaming half: the send is a stream of its own task's transitions,
/// ending at the settled state — never "task not found" for the task it made.
#[test]
fn a_streaming_send_streams_the_task_it_creates() {
    let llm = spawn_mock_llm();
    let (daemon, addr) = boot(&llm);
    let reply = SendMessage::text("hello").streaming().post_raw(&addr);
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.contains("text/event-stream")),
        "a streaming send is a stream: {reply:?}\n{}",
        daemon.stderr()
    );
    let frames: Vec<Value> = reply
        .body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect();
    assert!(
        frames.iter().all(|f| f.get("error").is_none()),
        "no frame is an error: {frames:#?}"
    );
    let id = frames
        .iter()
        .find_map(|f| f["result"]["task"]["id"].as_str())
        .unwrap_or_else(|| panic!("the stream opens with the task: {frames:#?}"));
    // Settled either in a transition frame or — when the turn finished before
    // the send returned — in the opening task itself.
    assert!(
        frames.iter().any(|f| {
            let r = &f["result"];
            (r["statusUpdate"]["taskId"] == id
                && r["statusUpdate"]["status"]["state"] == "TASK_STATE_COMPLETED")
                || (r["task"]["id"] == id && r["task"]["status"]["state"] == "TASK_STATE_COMPLETED")
        }),
        "the stream carries the task to its settled state: {frames:#?}"
    );
}
