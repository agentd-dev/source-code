// SPDX-License-Identifier: AGPL-3.0-only
//! **Finished tasks are kept as long as the operator said, and found by the
//! name the caller asks with.**
//!
//! `store.retention.tasks` bounds the terminal A2A tasks a long-lived listener
//! keeps. Without it, every task any caller ever finished stays in memory and
//! in the store for the daemon's whole life. The bound has to hold everywhere a
//! task lives — the listing, the store a restart reads back, the display
//! clients told with `task.removed` — and it must never take a task somebody is
//! still waiting on: a WORKING task is an answer in progress, an
//! INPUT_REQUIRED one a question still open.
//!
//! The listing's `contextId` filter reads a conversation's name the way the
//! caller holds it: two users may both call theirs `chat1`, and the operator,
//! who addresses conversations by the runtime's own keys, finds either one by
//! its key and both by the name they share.
#![cfg(all(unix, feature = "a2a"))]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

mod common;

use common::{SendMessage, rpc, rpc_as};

const TOKEN_A: &str = "retention-token-for-user-a";
const TOKEN_B: &str = "retention-token-for-user-b";
const TOKEN_OP: &str = "retention-token-for-the-operator";

struct Daemon {
    child: Child,
    stderr_path: String,
    cfg_path: String,
    addr: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
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

/// Boot a daemon from the config `cfg_for` renders for a probed port, once it
/// has bound that port — read off its own `a2a.listen` line, since a probed
/// port can be taken before the daemon binds it, and a lost bind is retried.
fn boot(cfg_for: impl Fn(u16) -> String) -> Daemon {
    let mut last = String::new();
    for _ in 0..5 {
        let cfg_path = common::unique_path("retention-e2e", "yaml");
        std::fs::write(&cfg_path, cfg_for(common::free_port())).unwrap();
        let stderr_path = common::unique_path("retention-e2e", "log");
        let errf = std::fs::File::create(&stderr_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg_path])
            .env("AGENTD_RETENTION_TOKEN_A", TOKEN_A)
            .env("AGENTD_RETENTION_TOKEN_B", TOKEN_B)
            .env("AGENTD_RETENTION_TOKEN_OP", TOKEN_OP)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errf))
            .spawn()
            .expect("spawn agentd");
        let mut daemon = Daemon {
            child,
            stderr_path,
            cfg_path,
            addr: String::new(),
        };
        if let Some(addr) = common::try_a2a_bound(&daemon.stderr_path, Duration::from_secs(20)) {
            daemon.addr = addr;
            return daemon;
        }
        last = daemon.stderr();
    }
    panic!("the daemon never bound an A2A listener (5 attempts); last stderr:\n{last}")
}

/// A loopback daemon on `store` (a [`file_store`] or [`MEMORY_STORE`]), with
/// `retention` as its `store.retention.tasks` block (empty keeps everything).
/// No principals, so the test is the operator. `waiter` parks on a signal, so
/// its task stays WORKING; `approve` asks a human, so its task waits
/// INPUT_REQUIRED.
fn retention_config(store: &str, retention: &str, port: u16) -> String {
    format!(
        "\
         agent:\n  name: retention-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: \"mock:final\"\n  model: mock\n\
         store:\n{store}{retention}\
         a2a:\n  listen: http://127.0.0.1:{port}\n  events:\n    enabled: true\n\
         workflows:\n\
         \x20 - name: waiter\n    steps:\n      s: {{kind: manual}}\n      w: {{kind: wait, on: signal, signal: go, depends_on: [s]}}\n      f: {{kind: finish, depends_on: [w]}}\n\
         \x20 - name: approve\n    steps:\n      s: {{kind: manual}}\n      gate: {{kind: human, question: \"Approve?\", depends_on: [s]}}\n      f: {{kind: finish, depends_on: [gate]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

/// The `store` block body of a file store under `dir`: what a restart reads
/// back, so what an eviction must reach.
fn file_store(dir: &str) -> String {
    format!("  kind: file\n  file:\n    path: {dir}/state\n")
}

/// The `store` block body of a store that writes nothing to disk.
const MEMORY_STORE: &str = "  kind: memory\n";

/// The task `id` as `GetTask` answers it: `Some(state)`, or `None` for the
/// spec's "task not found".
fn state_of(addr: &str, id: &str) -> Option<String> {
    let v = rpc(addr, 900, "GetTask", json!({"id": id}));
    if let Some(e) = v.get("error") {
        assert_eq!(e["code"], -32001, "GetTask {id}: {v}");
        return None;
    }
    Some(v["result"]["status"]["state"].as_str().unwrap().to_string())
}

/// `id` is still there and still unfinished. A restored run's task is
/// checked for that, not for one state: whether the store holds its WORKING
/// or the SUBMITTED before it is the run's business, not retention's.
fn assert_live(addr: &str, id: &str) {
    let state = state_of(addr, id).unwrap_or_else(|| panic!("task {id} was evicted"));
    assert!(
        ["TASK_STATE_SUBMITTED", "TASK_STATE_WORKING"].contains(&state.as_str()),
        "task {id} is {state}"
    );
}

/// Poll `GetTask` until `id` is in `want` (`None` = gone).
fn wait_state(addr: &str, id: &str, want: Option<&str>, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let got = state_of(addr, id);
        if got.as_deref() == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "task {id} never reached {want:?}; it is {got:?}"
        );
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// Start `workflow`; its task's id. Answered at once: neither run settles.
fn start(addr: &str, workflow: &str) -> String {
    let r = SendMessage::command("workflow.run", json!({"workflow": workflow}))
        .return_immediately()
        .result(addr);
    r["task"]["id"].as_str().expect("a run's task").to_string()
}

/// Send `text` and wait for its task to finish; the task's id.
fn finished_task(addr: &str, text: &str) -> String {
    let r = SendMessage::text(text).result(addr);
    assert_eq!(r["task"]["status"]["state"], "TASK_STATE_COMPLETED", "{r}");
    r["task"]["id"].as_str().unwrap().to_string()
}

/// Which of `ids` the feed has announced as `task.removed`, replayed from its
/// start. Stops reading once all are seen, or after `secs`.
fn removed_on_feed(addr: &str, ids: &[&str], secs: u64) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut seen = Vec::new();
    let mut reader = common::subscribe_feed(addr, 0, Duration::from_secs(2));
    common::read_frames(&mut reader, |v| {
        let ev = &v["result"]["event"];
        if ev["kind"] == "task.removed"
            && let Some(id) = ev["data"]["id"].as_str()
            && ids.contains(&id)
            && !seen.iter().any(|s| s == id)
        {
            seen.push(id.to_string());
        }
        seen.len() < ids.len() && Instant::now() < deadline
    });
    seen.sort();
    seen
}

#[test]
fn terminal_tasks_are_evicted_and_announced() {
    let dir = common::unique_path("retention-e2e-store", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let keep_one = "  retention:\n    tasks:\n      keep_last: 1\n";

    // ── keep_last: the bound holds at every finish ──────────────────────────
    let daemon = boot(|port| retention_config(&file_store(&dir), keep_one, port));
    let addr = daemon.addr.clone();
    let working = start(&addr, "waiter");
    wait_state(&addr, &working, Some("TASK_STATE_WORKING"), 10);
    let asking = start(&addr, "approve");
    wait_state(&addr, &asking, Some("TASK_STATE_INPUT_REQUIRED"), 10);

    let t1 = finished_task(&addr, "one");
    let t2 = finished_task(&addr, "two");
    let t3 = finished_task(&addr, "three");
    // Dropped as the newer ones finished — not on some later sweep — and the
    // newest is still there to be read.
    assert_eq!(state_of(&addr, &t1), None, "t1 is past keep_last");
    assert_eq!(state_of(&addr, &t2), None, "t2 is past keep_last");
    assert_eq!(
        state_of(&addr, &t3).as_deref(),
        Some("TASK_STATE_COMPLETED")
    );
    // Older than all three, and never candidates.
    assert_eq!(
        state_of(&addr, &working).as_deref(),
        Some("TASK_STATE_WORKING")
    );
    assert_eq!(
        state_of(&addr, &asking).as_deref(),
        Some("TASK_STATE_INPUT_REQUIRED")
    );
    let listed = rpc(&addr, 901, "ListTasks", json!({}));
    assert_eq!(listed["result"]["totalSize"], 3, "{listed}");
    // Every display client is told, so none keeps showing a task that is gone.
    let mut want = vec![t1.clone(), t2.clone()];
    want.sort();
    assert_eq!(
        removed_on_feed(&addr, &[&t1, &t2], 15),
        want,
        "task.removed for each evicted task"
    );
    drop(daemon);

    // ── a restart that keeps everything still has none of them ─────────────
    let daemon = boot(|port| retention_config(&file_store(&dir), "", port));
    let addr = daemon.addr.clone();
    assert_eq!(state_of(&addr, &t1), None, "the store forgot t1 too");
    assert_eq!(state_of(&addr, &t2), None, "the store forgot t2 too");
    assert_eq!(
        state_of(&addr, &t3).as_deref(),
        Some("TASK_STATE_COMPLETED")
    );
    assert_live(&addr, &working);
    assert_eq!(
        state_of(&addr, &asking).as_deref(),
        Some("TASK_STATE_INPUT_REQUIRED")
    );
    drop(daemon);

    // ── ttl: a finished task ages out with nothing else happening ──────────
    let daemon = boot(|port| {
        retention_config(
            &file_store(&dir),
            "  retention:\n    tasks:\n      ttl: 1s\n",
            port,
        )
    });
    let addr = daemon.addr.clone();
    wait_state(&addr, &t3, None, 15);
    // A task finished in this life, then nothing: no transition follows it,
    // so only the tick can notice its ttl has passed.
    let t4 = finished_task(&addr, "four");
    wait_state(&addr, &t4, None, 15);
    assert_eq!(
        removed_on_feed(&addr, &[&t4], 15),
        std::slice::from_ref(&t4),
        "task.removed for a task the ttl took"
    );
    // Both are far older than the ttl, and both are still somebody's.
    assert_live(&addr, &working);
    assert_eq!(
        state_of(&addr, &asking).as_deref(),
        Some("TASK_STATE_INPUT_REQUIRED")
    );
    assert!(
        daemon.stderr().contains("\"a2a.task.evicted\""),
        "the eviction is logged: {}",
        daemon.stderr()
    );
    drop(daemon);
    std::fs::remove_dir_all(&dir).ok();
}

/// A caller blocked on its own send is answered with its task, however many
/// others finish around it. a2a-rs answers a blocking send with a `GetTask`
/// it makes AFTER the terminal event, a second trip through the reactor; with
/// `keep_last: 0` every finished task is over the bound the moment it
/// finishes, and concurrent callers finish between one another's event and
/// read. Evicting in that gap answered a caller whose task COMPLETED with
/// "task not found". Once read, each is still dropped as the bound says.
///
/// The race is between the reactor's eviction and a2a-rs's read, and no
/// store takes part in it, so the daemon keeps its tasks in memory. On a file
/// store every checkpoint is two fsyncs on the single-writer loop, and on a
/// disk other builds were writing to, the 40 turns ran past a2a-rs's 25 s
/// send-wait. A send whose wait runs out is answered with its task still
/// unsettled. That is a slow host, not this defect, so an unsettled answer is
/// held only to being the caller's own task. The race needs settled answers
/// to happen at all, though, so a run where none settled proves nothing and
/// fails as such.
#[test]
fn a_blocking_send_is_answered_with_its_task_under_any_bound() {
    let daemon = boot(|port| {
        retention_config(
            MEMORY_STORE,
            "  retention:\n    tasks:\n      keep_last: 0\n",
            port,
        )
    });
    let addr = daemon.addr.clone();
    let answers: Vec<(String, serde_json::Value)> = std::thread::scope(|s| {
        let sends: Vec<_> = (0..40)
            .map(|i| {
                let addr = &addr;
                s.spawn(move || {
                    let text = format!("hello {i}");
                    let v = SendMessage::text(&text).post(addr);
                    (text, v)
                })
            })
            .collect();
        sends.into_iter().map(|h| h.join().unwrap()).collect()
    });
    // The defect: an answer that is not the caller's own task. An evicted
    // task comes back as -32001, which has no task at all.
    let lost: Vec<&serde_json::Value> = answers
        .iter()
        .filter(|(text, v)| v["result"]["task"]["history"][0]["parts"][0]["text"] != *text)
        .map(|(_, v)| v)
        .collect();
    assert!(
        lost.is_empty(),
        "{} of {} blocking sends were not handed their task: {lost:?}",
        lost.len(),
        answers.len()
    );
    let state = |v: &serde_json::Value| v["result"]["task"]["status"]["state"].clone();
    let settled = answers
        .iter()
        .filter(|(_, v)| state(v) == "TASK_STATE_COMPLETED")
        .count();
    let odd: Vec<&serde_json::Value> = answers
        .iter()
        .map(|(_, v)| v)
        .filter(|v| {
            ![
                "TASK_STATE_COMPLETED",
                "TASK_STATE_SUBMITTED",
                "TASK_STATE_WORKING",
            ]
            .iter()
            .any(|s| state(v) == *s)
        })
        .collect();
    assert!(
        odd.is_empty(),
        "a send that neither finished nor is still running: {odd:?}"
    );
    assert!(
        settled > 0,
        "none of the {} sends settled within a2a-rs's send-wait, so no answer \
         was read after its task finished and the race never ran",
        answers.len()
    );
    for (_, v) in &answers {
        wait_state(&addr, v["result"]["task"]["id"].as_str().unwrap(), None, 15);
    }
    drop(daemon);
}

/// Two users and the operator, over the in-process mock model.
fn principals_config(port: u16) -> String {
    format!(
        "\
         agent:\n  name: retention-ctx\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: \"mock:final\"\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         \x20 principals:\n\
         \x20   - id: user-a\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_RETENTION_TOKEN_A}}}}\" }}\n\
         \x20     role: user\n\
         \x20   - id: user-b\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_RETENTION_TOKEN_B}}}}\" }}\n\
         \x20     role: user\n\
         \x20   - id: op\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_RETENTION_TOKEN_OP}}}}\" }}\n\
         \x20     role: operator\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

/// The ids `bearer`'s `ListTasks` in conversation `ctx` returns, sorted.
fn listed_in(addr: &str, bearer: &str, ctx: &str) -> Vec<String> {
    let v = rpc_as(addr, bearer, 1, "ListTasks", json!({"contextId": ctx}));
    assert!(v.get("error").is_none(), "ListTasks {ctx}: {v}");
    let mut ids: Vec<String> = v["result"]["tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("a task list: {v}"))
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

/// The runtime's key for the conversation `bearer` calls `ctx`, as its own
/// `status` lists it.
fn conversation_key(addr: &str, bearer: &str, ctx: &str) -> String {
    let v = SendMessage::command("status", json!({}))
        .bearer(bearer)
        .return_immediately()
        .post(addr);
    v["result"]["message"]["parts"][0]["data"]["conversations"]
        .as_array()
        .unwrap_or_else(|| panic!("status lists conversations: {v}"))
        .iter()
        .find(|c| c["contextId"] == ctx)
        .and_then(|c| c["id"].as_str())
        .unwrap_or_else(|| panic!("no conversation {ctx:?}: {v}"))
        .to_string()
}

#[test]
fn operator_context_filter() {
    let daemon = boot(principals_config);
    let addr = daemon.addr.clone();
    let sent_as = |bearer: &str| {
        let r = SendMessage::text("hello")
            .bearer(bearer)
            .context("chat1")
            .result(&addr);
        assert_eq!(r["task"]["contextId"], "chat1", "{r}");
        r["task"]["id"].as_str().unwrap().to_string()
    };
    let ta = sent_as(TOKEN_A);
    let tb = sent_as(TOKEN_B);
    let a_key = conversation_key(&addr, TOKEN_A, "chat1");
    let b_key = conversation_key(&addr, TOKEN_B, "chat1");
    assert_ne!(a_key, b_key, "two conversations, both called chat1");

    // The operator: both by the name they share, each by its own key.
    let mut both = vec![ta.clone(), tb.clone()];
    both.sort();
    assert_eq!(listed_in(&addr, TOKEN_OP, "chat1"), both);
    assert_eq!(
        listed_in(&addr, TOKEN_OP, &a_key),
        std::slice::from_ref(&ta)
    );
    assert_eq!(
        listed_in(&addr, TOKEN_OP, &b_key),
        std::slice::from_ref(&tb)
    );

    // An owner: its own by its own name, and nothing by a key the runtime
    // chose — not even the key of its own conversation.
    assert_eq!(
        listed_in(&addr, TOKEN_A, "chat1"),
        std::slice::from_ref(&ta)
    );
    assert!(listed_in(&addr, TOKEN_A, &a_key).is_empty());
    assert!(listed_in(&addr, TOKEN_A, &b_key).is_empty());
    drop(daemon);
}
