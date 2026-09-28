// SPDX-License-Identifier: AGPL-3.0-only
//! **The task methods over the wire**: `ListTasks` and `CancelTask` as the A2A
//! 1.0 specification defines them, through the real listener and the protocol
//! layer in front of it.
//!
//! Each of these once answered with something shaped right and meaning less:
//! a listing that ignored every filter and paging field it was sent, and a
//! cancel that "succeeded" on a task that had already finished. The
//! assertions are the fields a spec client relies on.
#![cfg(feature = "a2a")]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;

use common::{SendMessage, error_of, rpc, rpc_result};

struct Daemon {
    child: Child,
    stderr_path: String,
    cfg_path: String,
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
        let _ = std::fs::remove_file(&self.cfg_path);
    }
}

/// A daemon on a free loopback port, answering every turn with the in-process
/// mock model. Loopback with no credential configured makes the test the
/// operator, which sees every task. The probe→bind gap is a real race under
/// parallel CI, so a lost bind is retried.
fn boot() -> (Daemon, String) {
    for _ in 0..5 {
        let cfg_path = common::unique_path("tasks-e2e", "yaml");
        std::fs::write(
            &cfg_path,
            format!(
                "config_version: \"1\"\n\
                 agent:\n  name: tasks-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
                 intelligence:\n  endpoints: \"mock:final\"\n  model: mock\n\
                 store:\n  kind: memory\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n\
                 lifecycle:\n  run_until: drained\n\
                 observability:\n  log_level: info\n",
                port = common::free_port()
            ),
        )
        .unwrap();
        let stderr_path = common::unique_path("tasks-e2e", "log");
        let errf = std::fs::File::create(&stderr_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errf))
            .spawn()
            .expect("spawn agentd");
        let daemon = Daemon {
            child,
            stderr_path,
            cfg_path,
        };
        if let Some(addr) = common::try_a2a_bound(&daemon.stderr_path, Duration::from_secs(15)) {
            return (daemon, addr);
        }
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

/// Send `text` in conversation `ctx` and wait for the task to settle.
fn settled_task(addr: &str, ctx: &str, text: &str) -> String {
    let sent = SendMessage::text(text).context(ctx).result(addr);
    let task = &sent["task"];
    assert_eq!(
        task["status"]["state"], "TASK_STATE_COMPLETED",
        "a blocking send answers when the turn is done: {sent}"
    );
    task["id"].as_str().expect("a task id").to_string()
}

/// The ids a listing returned, in order. Proto3 JSON omits an empty repeated
/// field, so an empty page has no `tasks` member at all.
fn ids(result: &Value) -> Vec<String> {
    assert!(result.is_object(), "a ListTasks result: {result}");
    result
        .get("tasks")
        .map(|t| {
            t.as_array()
                .unwrap_or_else(|| panic!("a task list: {result}"))
        })
        .into_iter()
        .flatten()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect()
}

/// The token for the next page. Proto3 JSON omits a string at its default, so
/// on the wire "no more pages" is an absent `nextPageToken`.
fn next_token(result: &Value) -> String {
    result
        .get("nextPageToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[test]
fn list_tasks() {
    let (_daemon, addr) = boot();
    let first = settled_task(&addr, "ctx-a", "one");
    let second = settled_task(&addr, "ctx-b", "two");
    let third = settled_task(&addr, "ctx-a", "three");
    let newest_first = [third.clone(), second.clone(), first.clone()];

    // The whole listing: newest status first, and the count of it.
    let all = rpc_result(&addr, 10, "ListTasks", json!({}));
    assert_eq!(ids(&all), newest_first, "{all}");
    assert_eq!(all["totalSize"], 3, "{all}");
    assert_eq!(next_token(&all), "", "one page holds everything: {all}");

    // A page at a time walks the same order, each task once, to the end.
    let mut seen = Vec::new();
    let mut token = String::new();
    for n in 0.. {
        assert!(n < 5, "the walk must end: {seen:?}");
        let page = rpc_result(
            &addr,
            11 + n,
            "ListTasks",
            json!({"pageSize": 1, "pageToken": token}),
        );
        assert_eq!(page["pageSize"], 1, "{page}");
        assert_eq!(
            page["totalSize"], 3,
            "the total is the listing's, not the page's"
        );
        seen.extend(ids(&page));
        token = next_token(&page);
        if token.is_empty() {
            break;
        }
    }
    assert_eq!(seen, newest_first);

    // The filters are applied, not forwarded and dropped.
    let a = rpc_result(&addr, 20, "ListTasks", json!({"contextId": "ctx-a"}));
    assert_eq!(ids(&a), [third.clone(), first.clone()], "{a}");
    assert_eq!(a["totalSize"], 2);
    let working = rpc_result(
        &addr,
        21,
        "ListTasks",
        json!({"status": "TASK_STATE_WORKING"}),
    );
    assert!(
        ids(&working).is_empty(),
        "every task here is done: {working}"
    );
    let done = rpc_result(
        &addr,
        22,
        "ListTasks",
        json!({"contextId": "ctx-b", "status": "TASK_STATE_COMPLETED"}),
    );
    assert_eq!(ids(&done), std::slice::from_ref(&second));
    let later = rpc_result(
        &addr,
        23,
        "ListTasks",
        json!({"statusTimestampAfter": "2999-01-01T00:00:00Z"}),
    );
    assert!(ids(&later).is_empty(), "{later}");

    // Artifacts only when asked for.
    assert!(
        all["tasks"][0].get("artifacts").is_none(),
        "a listing is an index, not a download: {all}"
    );
    let full = rpc_result(
        &addr,
        24,
        "ListTasks",
        json!({"contextId": "ctx-b", "includeArtifacts": true}),
    );
    assert!(
        full["tasks"][0]["artifacts"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "{full}"
    );

    // What cannot be honoured is refused, never quietly ignored.
    for (n, bad) in [
        json!({"pageSize": 0}),
        json!({"pageSize": 101}),
        json!({"historyLength": -1}),
        json!({"pageToken": "not-a-token-this-server-issued!"}),
    ]
    .into_iter()
    .enumerate()
    {
        let (code, msg) = error_of(&rpc(&addr, 30 + n as i64, "ListTasks", bad.clone()));
        assert_eq!(code, -32602, "{bad}: {msg}");
    }
}

#[test]
fn cancel_terminal_is_not_cancelable() {
    let (_daemon, addr) = boot();
    let id = settled_task(&addr, "ctx-c", "finish first");
    let before = rpc_result(&addr, 2, "GetTask", json!({"id": id}));

    let (code, msg) = error_of(&rpc(&addr, 3, "CancelTask", json!({"id": id})));
    assert_eq!(code, -32002, "a finished task is not cancelable: {msg}");
    assert!(
        msg.contains("TASK_STATE_COMPLETED"),
        "the refusal names the state the task is in: {msg}"
    );

    // …and it is left exactly as it was: still COMPLETED, its result intact.
    let after = rpc_result(&addr, 4, "GetTask", json!({"id": id}));
    assert_eq!(after["status"]["state"], "TASK_STATE_COMPLETED", "{after}");
    assert_eq!(after["artifacts"], before["artifacts"]);

    // A task that does not exist is still "not found", not "not cancelable".
    let (code, _) = error_of(&rpc(
        &addr,
        5,
        "CancelTask",
        json!({"id": "task-does-not-exist"}),
    ));
    assert_eq!(code, -32001);
}
