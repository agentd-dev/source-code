// SPDX-License-Identifier: AGPL-3.0-only
//! **A child's log, and a message to it, over the operator's surface.** The
//! real binary spawns an instance child — a daemon of its own, writing its
//! own log in the directory its parent composed it in — and the operator
//! reads that log through `subagent.log`: the newest lines and a cursor, a
//! read from the cursor that sees only what came since, a line count that
//! holds. A handle that does not exist, and a caller who is not the
//! operator, are refused as other operator ops refuse. Then the operator
//! messages the child with `subagent.send`, and the child's answer becomes
//! its `reply`, which `subagent.status` reads.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use common::SendMessage;
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const OPERATOR: &str = "subagent-log-operator-token";
const USER: &str = "subagent-log-user-token";

struct Daemon {
    child: Child,
    stderr: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config(port: u16) -> String {
    format!(
        "\
         agent: {{ name: sl-parent, instruction: You are the parent., preflight: never }}\n\
         intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
         store: {{ kind: memory }}\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         \x20 principals:\n\
         \x20   - id: ops\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_SL_OPERATOR}}}}\" }}\n\
         \x20     role: operator\n\
         \x20   - id: ada\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_SL_USER}}}}\" }}\n\
         \x20     role: user\n\
         \x20     grants: [\"*\"]\n\
         lifecycle: {{ run_until: drained }}\n\
         observability: {{ log_level: info }}\n\
         subagents:\n\
         \x20 templates:\n\
         \x20   room:\n\
         \x20     instruction: |\n\
         \x20       You are the room. Answer whoever writes to you.\n\
         \x20       :::config{{id=cfg1 dialect=agentd/config}}\n\
         \x20       agent: {{ preflight: never }}\n\
         \x20       :::\n\
         workflows:\n\
         \x20 - name: open\n\
         \x20   steps:\n\
         \x20     s: {{ kind: once }}\n\
         \x20     spawn: {{ kind: subagent, template: room, mode: detached, depends_on: [s] }}\n\
         \x20     f: {{ kind: finish, depends_on: [spawn], status: completed }}\n"
    )
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A read op's document, as `bearer`.
fn read(addr: &str, bearer: &str, op: &str, args: Value) -> Value {
    let r = SendMessage::command(op, args).bearer(bearer).result(addr);
    assert!(r.get("task").is_none(), "{op} created a task: {r}");
    r["message"]["parts"][0]["data"].clone()
}

fn wait_for<T>(what: &str, d: &Daemon, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            Instant::now() < end,
            "timed out waiting for {what}; parent stderr:\n{}",
            std::fs::read_to_string(&d.stderr).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn the_operator_reads_an_instance_childs_log_and_messages_it() {
    let (daemon, addr) = common::spawn_bound(|port| {
        let cfg = common::unique_path("subagent-log", "yaml");
        std::fs::write(&cfg, config(port)).unwrap();
        let stderr = common::unique_path("subagent-log", "log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg])
            .env("AGENTD_SL_OPERATOR", OPERATOR)
            .env("AGENTD_SL_USER", USER)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let path = stderr.clone();
        (Daemon { child, stderr }, path)
    });
    let parent = || std::fs::read_to_string(&daemon.stderr).unwrap_or_default();

    // The child the `once` workflow spawned, and the log it writes.
    let spawned = wait_for("the instance child", &daemon, || {
        events(&parent(), "instance.spawn").into_iter().next()
    });
    let handle = spawned["handle"].as_str().expect("a handle").to_string();
    let log_path = spawned["log"].as_str().expect("its log").to_string();

    // Its tail, once the child is up: the child's OWN lines, as written.
    let first = wait_for("the child's log", &daemon, || {
        let doc = read(&addr, OPERATOR, "subagent.log", json!({"handle": handle}));
        let lines = doc["lines"].as_array().cloned().unwrap_or_default();
        lines
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l.as_str()?).ok())
            .any(|v| v["event"] == "proc.ready")
            .then_some(doc)
    });
    assert_eq!(first["handle"], json!(handle), "{first}");
    assert_eq!(first["source"], "file", "{first}");
    // The document is the op's published result.
    let schema = agentd::runtime::surface::result_schema_of("subagent.log").unwrap();
    agentd::jsonschema::validate(&schema, &first)
        .unwrap_or_else(|e| panic!("subagent.log's reply breaks its schema: {e:?}\n{first}"));
    let lines = first["lines"].as_array().unwrap();
    for l in lines {
        let v: Value = serde_json::from_str(l.as_str().unwrap())
            .unwrap_or_else(|e| panic!("a log line is agentd's JSON line ({e}): {l}"));
        assert!(v["event"].is_string(), "{v}");
    }
    let on_disk = std::fs::read_to_string(&log_path).unwrap();
    for l in lines {
        assert!(
            on_disk.contains(l.as_str().unwrap()),
            "the line is the file's, as written: {l}"
        );
    }
    let cursor = first["cursor"].as_u64().expect("a cursor");
    assert!(cursor > 0 && cursor <= on_disk.len() as u64, "{first}");

    // From the cursor: only what came since — never a line read already.
    let next = read(
        &addr,
        OPERATOR,
        "subagent.log",
        json!({"handle": handle, "after": cursor}),
    );
    assert!(next["cursor"].as_u64().unwrap() >= cursor, "{next}");
    for l in next["lines"].as_array().unwrap() {
        assert!(!lines.contains(l), "a line read twice: {l}");
    }
    // The line count holds.
    let one = read(
        &addr,
        OPERATOR,
        "subagent.log",
        json!({"handle": handle, "lines": 1}),
    );
    assert_eq!(one["lines"].as_array().unwrap().len(), 1, "{one}");

    // A handle that does not exist is not found; anyone but the operator is
    // refused, whatever their grants say.
    let unknown = SendMessage::command("subagent.log", json!({"handle": "inst-nope"}))
        .bearer(OPERATOR)
        .post(&addr);
    assert_eq!(unknown["error"]["code"], -32001, "{unknown}");
    let user = SendMessage::command("subagent.log", json!({"handle": handle}))
        .bearer(USER)
        .post(&addr);
    assert_eq!(user["error"]["code"], -31403, "{user}");
    assert_eq!(
        user["error"]["data"][0]["reason"], "PERMISSION_DENIED",
        "{user}"
    );
    assert!(user.get("result").is_none(), "{user}");

    // A message to the child reaches it over its socket, and its answer is
    // its `reply` — not its `result`, which is the child's own report.
    let sent = SendMessage::command(
        "subagent.send",
        json!({"handle": handle, "message": "hello room"}),
    )
    .bearer(OPERATOR)
    .result(&addr);
    assert_eq!(
        sent["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );
    let replied = wait_for("the child's reply", &daemon, || {
        let st = read(
            &addr,
            OPERATOR,
            "subagent.status",
            json!({"handle": handle}),
        );
        assert!(st.get("result").is_none() || st["result"].is_null(), "{st}");
        st["reply"]
            .as_str()
            .filter(|r| !r.is_empty())
            .map(str::to_string)
    });
    assert!(!replied.is_empty());
    let turned = events(&parent(), "subagent.turn");
    assert!(
        turned
            .iter()
            .any(|e| e["handle"] == json!(handle) && e["tier"] == "instance"),
        "the reply is said as the child's turn:\n{}",
        parent()
    );
    // And the child's own log says it took the message.
    let after = read(
        &addr,
        OPERATOR,
        "subagent.log",
        json!({"handle": handle, "after": cursor}),
    );
    assert!(
        !after["lines"].as_array().unwrap().is_empty(),
        "the child logged the turn the message started: {after}"
    );
}
