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
    config_with(port, "mock:final")
}

/// The deployment, its model the mock at `intel`.
fn config_with(port: u16, intel: &str) -> String {
    format!(
        "\
         agent: {{ name: sl-parent, instruction: You are the parent., preflight: never }}\n\
         intelligence: {{ endpoints: \"{intel}\", model: mock }}\n\
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

/// The parent on the model `intel`, and its A2A address.
fn start(intel: &str) -> (Daemon, String) {
    common::spawn_bound(|port| {
        let cfg = common::unique_path("subagent-log", "yaml");
        std::fs::write(&cfg, config_with(port, intel)).unwrap();
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
    })
}

/// `f`, given `secs` to answer: a call the daemon never answers fails here
/// rather than hanging the test.
fn within<T: Send + 'static>(secs: u64, what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .unwrap_or_else(|_| panic!("{what} got no answer in {secs}s: the daemon hangs"))
}

/// How many of the process `pid`'s threads have a name beginning `prefix`.
fn threads_named(pid: u32, prefix: &str) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|t| {
                    std::fs::read_to_string(t.path().join("comm"))
                        .is_ok_and(|c| c.starts_with(prefix))
                })
                .count()
        })
        .unwrap_or(0)
}

/// The child owns its directory, so what is at its log's path is its to
/// choose; the parent reads only the file it opened for it. A symlink to a
/// file of the parent's, a FIFO (whose open would wait, on the loop) and
/// another file are refused, said; the log put back reads again. Then the
/// child is messaged: a burst of sends is carried by one follower thread,
/// not one each, and the reply kept is the newest send's — named by the
/// task that send was answered with — though an older send's turn ends
/// after it.
#[test]
fn the_parent_reads_only_the_log_it_opened_and_follows_sends_in_order() {
    let play = common::unique_path("subagent-log-play", "json");
    std::fs::write(
        &play,
        json!({"match": [
            {"when_contains": "SLOW-A", "delay_ms": 4000, "content": "answer A"},
            {"when_contains": "FAST-B", "content": "answer B"}],
         "turns": [{"content": "ok"}]})
        .to_string(),
    )
    .unwrap();
    let (daemon, addr) = start(&format!("mock:file:{play}"));
    let parent = || std::fs::read_to_string(&daemon.stderr).unwrap_or_default();
    let spawned = wait_for("the instance child", &daemon, || {
        events(&parent(), "instance.spawn").into_iter().next()
    });
    let handle = spawned["handle"].as_str().unwrap().to_string();
    let log = std::path::PathBuf::from(spawned["log"].as_str().unwrap());
    let kept = log.with_extension("kept");
    wait_for("the child's log", &daemon, || {
        let doc = read(&addr, OPERATOR, "subagent.log", json!({"handle": handle}));
        (!doc["lines"].as_array()?.is_empty()).then_some(())
    });
    let log_op = |addr: &str, handle: &str| {
        let (addr, handle) = (addr.to_string(), handle.to_string());
        within(10, "subagent.log", move || {
            SendMessage::command("subagent.log", json!({"handle": handle}))
                .bearer(OPERATOR)
                .post(&addr)
        })
    };
    let refused = |r: &Value, what: &str| {
        let msg = r["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("the child's log is not the file agentd opened for it"),
            "{what} is read as the child's log: {r}"
        );
        assert!(!r.to_string().contains("only the parent"), "{what}: {r}");
    };

    // A symlink to a file only the parent may read: not followed.
    std::fs::rename(&log, &kept).unwrap();
    let parents = common::unique_path("subagent-log-parents", "json");
    std::fs::write(&parents, "{\"event\":\"only the parent\"}\n").unwrap();
    std::os::unix::fs::symlink(&parents, &log).unwrap();
    refused(&log_op(&addr, &handle), "a symlink");
    // A FIFO: refused at once, the daemon still answering.
    std::fs::remove_file(&log).unwrap();
    let c = std::ffi::CString::new(log.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    refused(&log_op(&addr, &handle), "a FIFO");
    // Another regular file under the log's name.
    std::fs::remove_file(&log).unwrap();
    std::fs::copy(&parents, &log).unwrap();
    refused(&log_op(&addr, &handle), "another file");
    // The log it opened, put back, reads.
    std::fs::remove_file(&log).unwrap();
    std::fs::rename(&kept, &log).unwrap();
    let back = log_op(&addr, &handle);
    assert!(back.get("error").is_none(), "{back}");

    // A burst of sends: one follower carries them all.
    let send = |text: &str| {
        let r = SendMessage::command("subagent.send", json!({"handle": handle, "message": text}))
            .bearer(OPERATOR)
            .result(&addr);
        r["task"]["id"].as_str().expect("a task").to_string()
    };
    let pid = daemon.child.id();
    let mut most = 0;
    for n in 0..10 {
        send(&format!("burst {n}"));
        most = most.max(threads_named(pid, "inst."));
    }
    for _ in 0..5 {
        most = most.max(threads_named(pid, "inst."));
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(most <= 1, "{most} threads carried one child's sends");

    // A slow turn, then a fast one: the fast one's reply is kept, and stays.
    wait_for("the burst's answers", &daemon, || {
        (threads_named(pid, "inst.") == 0).then_some(())
    });
    let a = send("SLOW-A");
    let b = send("FAST-B");
    assert_ne!(a, b);
    let status = || {
        read(
            &addr,
            OPERATOR,
            "subagent.status",
            json!({"handle": handle}),
        )
    };
    let says_b = |st: &Value| st["reply"].as_str().is_some_and(|r| r.contains("answer B"));
    wait_for("the fast send's reply", &daemon, || {
        says_b(&status()).then_some(())
    });
    // Past the slow turn's end: its answer did not take the newer one's place.
    std::thread::sleep(Duration::from_secs(6));
    let st = status();
    assert!(says_b(&st), "an older send's reply took its place: {st}");
    assert_eq!(st["reply_to"], json!(b), "the reply names its send: {st}");
}
