// SPDX-License-Identifier: AGPL-3.0-only
//! **The command vocabulary, from outside the process.**
//!
//! One table (`surface::OPS`) says what every command op is: whether it
//! answers with a Message or a Task, and whether it answers to the operator
//! role alone. These tests hold the running daemon to that table over its real
//! listener:
//!
//! * a read is the spec's immediate reply — a Message, no task in the list
//!   every client of the principal enumerates, no `task` event on the feed;
//! * `admin.set` accepts exactly what the config file accepts for its path;
//! * introspection follows its own switch, with no feed, and `admin.set`;
//! * a `user` holding every grant still reaches none of the operator controls
//!   — the escalation the table exists to close.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::runtime::surface::{Floor, OPS, Reply};
use serde_json::{Value, json};

use common::{SendMessage, error_of, rpc_as, rpc_result};

/// The bearer the `user` principal presents; the config names it by reference.
const USER_TOKEN: &str = "vocabulary-user-token";

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
fn spawn_mock_llm() -> MockLlm {
    let pb = common::unique_path("vocab-playbook", "json");
    std::fs::write(&pb, json!({"turns": [{"content": "unused"}]}).to_string()).unwrap();
    let addr_file = common::unique_path("vocab-mock-llm", "addr");
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
        let _ = std::fs::remove_file(&self.cfg);
    }
}

/// A loopback daemon on `port`. `a2a` is the body of the `a2a:` section below
/// `listen`; `extra` is appended at the top level.
fn config(llm: &str, a2a: &str, extra: &str) -> impl Fn(u16) -> String {
    let (llm, a2a, extra) = (llm.to_string(), a2a.to_string(), extra.to_string());
    move |port| {
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: vocab-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: {llm}\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n{a2a}\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n{extra}"
        )
    }
}

/// Spawn a daemon on a free port, retrying when another process takes the
/// port between the probe and the bind.
fn spawn(yaml_for: impl Fn(u16) -> String) -> Daemon {
    for _ in 0..5 {
        let cfg = common::unique_path("vocab", "yaml");
        std::fs::write(&cfg, yaml_for(common::free_port())).unwrap();
        let stderr_path = common::unique_path("vocab-daemon", "log");
        let errf = std::fs::File::create(&stderr_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg])
            .env("AGENTD_VOCAB_USER_TOKEN", USER_TOKEN)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errf))
            .spawn()
            .expect("spawn agentd");
        let mut d = Daemon {
            child,
            stderr_path,
            cfg,
            addr: String::new(),
        };
        if let Some(addr) = common::try_a2a_bound(&d.stderr_path, Duration::from_secs(15)) {
            d.addr = addr;
            return d;
        }
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

/// A command's document: a read's Message data, or a task's JSON artifact.
fn doc_of(result: &Value) -> Value {
    if let Some(doc) = result["message"]["parts"][0].get("data") {
        return doc.clone();
    }
    result["task"]["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or(Value::Null)
}

/// Collect the observation feed in the background, from its first event.
fn watch_feed(addr: &str) -> Arc<Mutex<Vec<Value>>> {
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    let addr = addr.to_string();
    std::thread::spawn(move || {
        let mut reader = common::subscribe_feed(&addr, 0, Duration::from_secs(60));
        common::read_frames(&mut reader, |v| {
            sink.lock().unwrap().push(v["result"].clone());
            true
        });
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| f.get("hello").is_some())
    {
        assert!(Instant::now() < deadline, "the feed never said hello");
        std::thread::sleep(Duration::from_millis(20));
    }
    frames
}

fn feed_events(frames: &Arc<Mutex<Vec<Value>>>, kind: &str) -> Vec<Value> {
    frames
        .lock()
        .unwrap()
        .iter()
        .filter(|f| f["event"]["kind"] == kind)
        .map(|f| f["event"]["data"].clone())
        .collect()
}

fn task_count(addr: &str) -> usize {
    rpc_result(addr, 90, "ListTasks", json!({}))["tasks"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0)
}

#[test]
fn read_commands_create_no_task_and_push_no_task_event() {
    let llm = spawn_mock_llm();
    let d = spawn(config(
        &llm.uri,
        "  events:\n    enabled: true\n  introspection:\n    enabled: true\n",
        "workflows:\n  - name: greet\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], output: \"done\"}\n",
    ));
    let feed = watch_feed(&d.addr);

    // Every read the table declares — answered or refused (an unknown object
    // is still a read), none leaves a task behind.
    let reads: Vec<&str> = OPS
        .iter()
        .filter(|s| s.reply == Reply::Message)
        .map(|s| s.name)
        .collect();
    assert!(reads.contains(&"status") && reads.contains(&"debug.events"));
    for op in &reads {
        let v = SendMessage::command(op, json!({})).post(&d.addr);
        if v.get("error").is_some() {
            continue;
        }
        let r = &v["result"];
        assert!(r.get("task").is_none(), "{op} created a task: {v}");
        assert_eq!(r["message"]["role"], "ROLE_AGENT", "{op}: {v}");
        assert_eq!(
            r["message"]["parts"][0]["mediaType"], "application/json",
            "{op}: {v}"
        );
    }
    // `status` and `config` answer an operator whatever else exists.
    for op in ["status", "config"] {
        let v = SendMessage::command(op, json!({})).result(&d.addr);
        assert!(doc_of(&v).is_object(), "{op}: {v}");
    }
    // Streamed, a read is one Message frame, not a task stream.
    let raw = SendMessage::command("status", json!({}))
        .streaming()
        .post_raw(&d.addr)
        .body;
    let frames: Vec<Value> = raw
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect();
    assert_eq!(frames.len(), 1, "one frame: {raw}");
    assert!(
        frames[0]["result"]["message"].is_object() && frames[0]["result"].get("task").is_none(),
        "{raw}"
    );

    assert_eq!(task_count(&d.addr), 0, "reads are taskless");
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        feed_events(&feed, "task").is_empty(),
        "a read pushed a task event: {:?}",
        feed_events(&feed, "task")
    );

    // The control: work DOES make a task, and the feed carries it — so the
    // silence above is the reads', not a deaf observer's.
    let run = SendMessage::command("workflow.run", json!({"workflow": "greet"})).result(&d.addr);
    assert!(run["task"]["id"].is_string(), "{run}");
    assert_eq!(task_count(&d.addr), 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while feed_events(&feed, "task").is_empty() {
        assert!(
            Instant::now() < deadline,
            "workflow.run pushed no task event"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn admin_set_parses_like_the_config_file() {
    let llm = spawn_mock_llm();
    let d = spawn(config(&llm.uri, "  events:\n    enabled: true\n", ""));
    let feed = watch_feed(&d.addr);

    // Whether the config file accepts `value` at `path`.
    let file_accepts = |path: &str, value: &Value| {
        let (section, key) = path.rsplit_once('.').unwrap();
        let nested = match section {
            "agent" => format!("agent:\n  name: v\n  instruction: x\n  {key}: {value}\n"),
            "a2a.introspection" => format!(
                "agent:\n  name: v\n  instruction: x\n\
                 a2a:\n  listen: http://127.0.0.1:8443\n  introspection:\n    {key}: {value}\n"
            ),
            other => panic!("no fixture for {other}"),
        };
        let file = common::unique_path("vocab-validate", "yaml");
        std::fs::write(
            &file,
            format!(
                "config_version: \"1\"\n{nested}intelligence:\n  endpoints: {}\n  model: mock\n",
                llm.uri
            ),
        )
        .unwrap();
        let ok = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--validate-config", "-c", &file])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run --validate-config")
            .success();
        std::fs::remove_file(&file).ok();
        ok
    };

    let cases: &[(&str, Value)] = &[
        ("agent.approval", json!("ask")),
        ("agent.approval", json!("await")),
        ("agent.approval", json!("human")),
        ("agent.approval", json!("auto")),
        ("agent.approval", json!("accept_all")),
        ("agent.approval", json!("yes")),
        ("agent.approval", json!("Accept")),
        ("agent.approval", json!("sometimes")),
        ("agent.approval", json!(1)),
        ("a2a.introspection.enabled", json!(true)),
        ("a2a.introspection.enabled", json!(false)),
        ("a2a.introspection.enabled", json!("true")),
        ("a2a.introspection.enabled", json!(1)),
    ];
    let mut accepted = 0;
    for (path, value) in cases {
        let v =
            SendMessage::command("admin.set", json!({"path": path, "value": value})).post(&d.addr);
        let set_ok = v.get("error").is_none();
        assert_eq!(
            set_ok,
            file_accepts(path, value),
            "admin.set and the config file disagree on {path} = {value}: {v}"
        );
        if set_ok {
            accepted += 1;
            assert_eq!(
                v["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
                "{v}"
            );
            assert_eq!(doc_of(&v["result"])["path"], *path, "{v}");
        } else {
            let (code, msg) = error_of(&v);
            assert_eq!(code, -32602, "{v}");
            assert!(
                msg.contains("agent.approval") && msg.contains("a2a.introspection.enabled"),
                "the refusal lists what is settable: {msg}"
            );
        }
    }
    assert!(accepted >= 6, "the accepted spellings were exercised");

    // An alias is stored as what it means.
    let v = SendMessage::command(
        "admin.set",
        json!({"path": "agent.approval", "value": "await"}),
    )
    .result(&d.addr);
    assert_eq!(doc_of(&v)["value"], "ask", "{v}");

    // Every accepted set is announced — on the feed and in the log.
    let deadline = Instant::now() + Duration::from_secs(5);
    while feed_events(&feed, "config").len() < accepted + 1 {
        assert!(
            Instant::now() < deadline,
            "config events: {:?}",
            feed_events(&feed, "config")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let ev = feed_events(&feed, "config").pop().unwrap();
    assert_eq!(
        ev,
        json!({"paths": ["agent.approval"], "source": "admin.set"})
    );
    assert!(
        d.stderr().contains("\"event\":\"admin.set\""),
        "{}",
        d.stderr()
    );
}

#[test]
fn introspection_works_without_the_feed_and_follows_admin_set() {
    let llm = spawn_mock_llm();
    // No feed, no introspection.
    let d = spawn(config(&llm.uri, "", ""));

    let (code, _) = common::rpc_error(&d.addr, 1, common::feed_method(), json!({}));
    assert_eq!(code, -32004, "there is no feed");

    let refused = SendMessage::command("debug.events", json!({})).post(&d.addr);
    assert_eq!(refused["error"]["code"], -32004, "{refused}");
    assert_eq!(
        refused["error"]["data"][0]["reason"], "INTROSPECTION_DISABLED",
        "{refused}"
    );

    let set = |on: bool| {
        SendMessage::command(
            "admin.set",
            json!({"path": "a2a.introspection.enabled", "value": on}),
        )
        .result(&d.addr)
    };
    set(true);
    let ev = SendMessage::command("debug.events", json!({"limit": 5})).result(&d.addr);
    assert!(doc_of(&ev)["events"].is_array(), "{ev}");
    let conv = SendMessage::command("conversation.get", json!({"id": "nope"})).post(&d.addr);
    assert_eq!(
        conv["error"]["code"], -32001,
        "the read runs, and finds nothing: {conv}"
    );

    set(false);
    let again = SendMessage::command("debug.events", json!({})).post(&d.addr);
    assert_eq!(again["error"]["code"], -32004, "{again}");
}

#[test]
fn a_user_with_every_grant_cannot_reach_operator_controls() {
    let llm = spawn_mock_llm();
    let d = spawn(config(
        &llm.uri,
        "  events:\n    enabled: true\n  introspection:\n    enabled: true\n\
         \x20 principals:\n\
         \x20   - id: greedy\n\
         \x20     match: { bearer_ref: \"{{secret:AGENTD_VOCAB_USER_TOKEN}}\" }\n\
         \x20     role: user\n\
         \x20     grants: [\"*\", \"a*\", \"admin.*\", \"config*\", \"debug.*\", \"_instance.*\"]\n",
        "workflows:\n  - name: waiter\n    steps:\n      s: {kind: manual}\n      w: {kind: wait, on: signal, signal: go, depends_on: [s]}\n      f: {kind: finish, depends_on: [w], output: \"released\"}\n",
    ));

    let floor: Vec<&str> = OPS
        .iter()
        .filter(|s| s.floor == Floor::Operator)
        .map(|s| s.name)
        .filter(|n| !n.ends_with('.'))
        .chain(["_instance.result", "_instance.emit"])
        .collect();
    assert!(floor.contains(&"admin.set") && floor.contains(&"config"));
    for op in &floor {
        let args = match *op {
            "admin.set" => json!({"path": "agent.approval", "value": "accept"}),
            _ => json!({}),
        };
        let v = SendMessage::command(op, args)
            .bearer(USER_TOKEN)
            .post(&d.addr);
        let (code, _) = error_of(&v);
        assert!(
            code == -32003 || code == agentd::a2a::errors::PERMISSION_DENIED,
            "a user holding every grant reached {op}: {v}"
        );
    }

    // Nothing moved: the instance is not draining or paused.
    let st = SendMessage::command("status", json!({}))
        .bearer(USER_TOKEN)
        .result(&d.addr);
    let st = doc_of(&st);
    assert_eq!(st["draining"], false, "{st}");
    assert_ne!(st["paused"], true, "{st}");
    assert!(
        !d.stderr().contains("\"event\":\"admin.set\""),
        "an operator control ran"
    );

    // The extended card offers this caller none of them either.
    let card = rpc_as(&d.addr, USER_TOKEN, 5, "GetExtendedAgentCard", json!({}));
    let skills: Vec<&str> = card["result"]["skills"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["id"].as_str()).collect())
        .unwrap_or_default();
    for op in &floor {
        assert!(!skills.contains(op), "{op} offered to a user: {skills:?}");
    }

    // The grants still grant what they may: `workflow.signal` is no user
    // default, and `*` reaches it.
    let sig = SendMessage::command("workflow.signal", json!({"name": "go"}))
        .bearer(USER_TOKEN)
        .post(&d.addr);
    assert!(sig.get("error").is_none(), "{sig}");
}
