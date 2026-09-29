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
//!   — the escalation the table exists to close;
//! * `workflow.run` asks the grants AND the default start's `roles:`, and
//!   refuses before any task exists — and the extended card agrees;
//! * `subagent.get` answers the subagent's owner and the operator only;
//! * a reload that puts back what `admin.set` changed says so.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::runtime::surface::{Floor, Handler, INSTANCE_OPS, OPS, RUNTIME_SETTABLE, Reply};
use serde_json::{Value, json};

use common::{SendMessage, error_of, rpc_as, rpc_result};

/// The bearer the `user` principal presents; the config names it by reference.
const USER_TOKEN: &str = "vocabulary-user-token";
/// A second `user`'s bearer, for the rules that scope one user's grants.
const SCOPED_TOKEN: &str = "vocabulary-scoped-token";
/// The operator's bearer. Declaring any principal rule turns the loopback
/// operator default off, so a test that declares users names its operator.
const OPS_TOKEN: &str = "vocabulary-ops-token";

/// The `principals:` rules (under `a2a:`) for an operator `ops` and a plain
/// `user` named `plain`, plus `more` rules.
fn principals(more: &str) -> String {
    format!(
        "\x20 principals:\n\
         \x20   - id: ops\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_VOCAB_OPS_TOKEN}}}}\" }}\n\
         \x20     role: operator\n\
         \x20   - id: plain\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_VOCAB_USER_TOKEN}}}}\" }}\n\
         \x20     role: user\n{more}"
    )
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
fn spawn_mock_llm() -> MockLlm {
    spawn_mock_llm_with(&json!({"turns": [{"content": "unused"}]}))
}
fn spawn_mock_llm_with(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("vocab-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
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
            "\
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
            .env("AGENTD_VOCAB_SCOPED_TOKEN", SCOPED_TOKEN)
            .env("AGENTD_VOCAB_OPS_TOKEN", OPS_TOKEN)
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

/// A command's document: a read's Message data, or a task's JSON DataPart artifact.
fn doc_of(result: &Value) -> Value {
    if let Some(doc) = result["message"]["parts"][0].get("data") {
        return doc.clone();
    }
    result["task"]["artifacts"][0]["parts"][0]["data"].clone()
}

/// Who owns the task a `task` feed event carries: its task-annotations
/// `principal`.
fn owner_of(event_data: &Value) -> &Value {
    &event_data["task"]["metadata"][agentd::runtime::surface::TASK_ANNOTATIONS_EXTENSION]["principal"]
}

/// Collect the observation feed in the background, from its first event.
fn watch_feed(addr: &str) -> Arc<Mutex<Vec<Value>>> {
    watch_feed_as(addr, None)
}

/// [`watch_feed`], presenting `bearer`.
fn watch_feed_as(addr: &str, bearer: Option<&str>) -> Arc<Mutex<Vec<Value>>> {
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    let addr = addr.to_string();
    let auth = bearer.map(|b| format!("Bearer {b}"));
    std::thread::spawn(move || {
        let body = common::rpc_body(77, common::feed_method(), json!({"fromSeq": 0}));
        let activate = common::feed_extensions();
        let mut headers: Vec<(&str, &str)> = vec![("A2A-Extensions", &activate)];
        if let Some(a) = auth.as_deref() {
            headers.push(("Authorization", a));
        }
        let mut reader = common::a2a_open(&addr, &body, &headers, Duration::from_secs(60));
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

/// How many tasks `bearer`'s principal can list (an empty list is omitted
/// from the reply, as proto3 JSON omits every empty repeated field).
fn task_count_as(addr: &str, bearer: &str) -> usize {
    let v = rpc_as(addr, bearer, 91, "ListTasks", json!({}));
    assert!(v["result"].is_object(), "ListTasks failed: {v}");
    v["result"]["tasks"].as_array().map(Vec::len).unwrap_or(0)
}

/// The workflows `bearer`'s principal is offered on the extended card (a
/// workflow's skill id is `workflow:<name>`).
fn skills_as(addr: &str, bearer: &str) -> Vec<String> {
    rpc_as(addr, bearer, 5, "GetExtendedAgentCard", json!({}))["result"]["skills"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s["id"].as_str()?.strip_prefix("workflow:"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until `pred` holds over the feed frames collected so far.
fn feed_until(frames: &Arc<Mutex<Vec<Value>>>, what: &str, pred: impl Fn(&[Value]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pred(&frames.lock().unwrap()) {
        assert!(
            Instant::now() < deadline,
            "the feed never carried {what}: {:?}",
            frames.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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
                "{nested}intelligence:\n  endpoints: {}\n  model: mock\n",
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
        ("agent.approval", json!("auto")),
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
    assert!(accepted >= 4, "the accepted spellings were exercised");

    // Every accepted set is announced — on the feed and in the log.
    let deadline = Instant::now() + Duration::from_secs(5);
    while feed_events(&feed, "config").len() < accepted {
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
        json!({"paths": ["a2a.introspection.enabled"], "source": "admin.set"})
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

    // With the feed off, the events extension is not declared: the method is refused as
    // one this instance does not offer, whatever the request activates.
    let v = common::rpc_activating(
        &d.addr,
        1,
        common::feed_method(),
        json!({}),
        &[agentd::runtime::surface::EVENTS_EXTENSION],
    );
    assert_eq!(v["error"]["code"], -32601, "there is no feed: {v}");
    assert_eq!(
        v["error"]["data"][0]["reason"], "EXTENSION_NOT_DECLARED",
        "{v}"
    );

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

    // Every operator-floor op something serves; `ask_human`, held in reserve
    // and served to nobody, is unknown to every caller (below).
    let floor: Vec<&str> = OPS
        .iter()
        .filter(|s| s.floor == Floor::Operator && s.handler != Handler::Reserved)
        .map(|s| s.name)
        .filter(|n| !n.ends_with('.'))
        .chain(INSTANCE_OPS.iter().map(|m| m.name))
        .collect();
    assert!(floor.contains(&"admin.set") && floor.contains(&"config"));
    for op in &floor {
        // Arguments each op's schema accepts: the envelope is checked before
        // the floor, so what is refused here is the caller, not the call.
        let args = match *op {
            "admin.set" => json!({"path": "agent.approval", "value": "accept"}),
            "admin.cancel" => json!({"run": "waiter-1"}),
            "auth.device.approve" => json!({"user_code": "WDJB-MJHT", "as": "greedy"}),
            "auth.device.deny" | "auth.sessions.revoke" => json!({"all": true}),
            "_instance.result" => json!({"handle": "c1"}),
            "_instance.emit" => json!({"handle": "c1", "stream": "s", "event": {}}),
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

    let held = SendMessage::command("ask_human", json!({}))
        .bearer(USER_TOKEN)
        .post(&d.addr);
    assert_eq!(held["error"]["data"][1]["reason"], "UNKNOWN_OP", "{held}");

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

#[test]
fn workflow_run_asks_the_grants_and_the_default_starts_roles_before_any_task_exists() {
    let llm = spawn_mock_llm();
    let d = spawn(config(
        &llm.uri,
        &format!(
            "  events:\n    enabled: true\n  introspection:\n    enabled: true\n{}",
            principals(
                "\x20   - id: scoped\n\
                 \x20     match: { bearer_ref: \"{{secret:AGENTD_VOCAB_SCOPED_TOKEN}}\" }\n\
                 \x20     role: user\n\
                 \x20     grants: [\"workflow.run:deploy-*\"]\n"
            )
        ),
        "workflows:\n\
         \x20 - name: deploy-web\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], output: \"deployed\"}\n\
         \x20 - name: wipe\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], output: \"wiped\"}\n\
         \x20 - name: ops-only\n    steps:\n      s: {kind: a2a, command: ops-only.go, roles: [operator]}\n      f: {kind: finish, depends_on: [s], output: \"ran\"}\n",
    ));
    let feed = watch_feed_as(&d.addr, Some(OPS_TOKEN));
    let denied = |v: &Value, what: &str| {
        let (code, _) = error_of(v);
        assert_eq!(code, agentd::a2a::errors::PERMISSION_DENIED, "{what}: {v}");
        assert_eq!(
            v["error"]["data"][0]["reason"], "PERMISSION_DENIED",
            "{what}: {v}"
        );
    };
    let run = |bearer: &str, wf: &str| {
        SendMessage::command("workflow.run", json!({"workflow": wf}))
            .bearer(bearer)
            .post(&d.addr)
    };

    // The default start admits operators only: a user may not start it by
    // name, and nothing is created on the way to saying so.
    denied(&run(USER_TOKEN, "ops-only"), "the default start's roles");
    assert_eq!(
        task_count_as(&d.addr, USER_TOKEN),
        0,
        "no task for a refusal"
    );
    // A scoped grant replaces the role default: `wipe` is outside it.
    denied(&run(SCOPED_TOKEN, "wipe"), "the scoped grant");
    denied(&run(SCOPED_TOKEN, "ops-only"), "the scope and the roles");
    assert_eq!(task_count_as(&d.addr, SCOPED_TOKEN), 0);
    // A name that does not exist reads, to anyone who may not run
    // everything, exactly like one they may not run — the card hides the
    // second, and a different refusal would name it anyway.
    let ghost = run(USER_TOKEN, "ghost");
    denied(&ghost, "a missing workflow");
    let gated = run(USER_TOKEN, "ops-only");
    assert_eq!(
        error_of(&ghost).1.replace("ghost", "X"),
        error_of(&gated).1.replace("ops-only", "X"),
        "one refusal for missing and forbidden"
    );
    // The operator hears which it is.
    let (code, msg) = error_of(&run(OPS_TOKEN, "ghost"));
    assert_eq!(code, -32602, "{msg}");

    // What the caller may run does run.
    let ok = SendMessage::command("workflow.run", json!({"workflow": "deploy-web"}))
        .bearer(SCOPED_TOKEN)
        .result(&d.addr);
    assert!(ok["task"]["id"].is_string(), "{ok}");
    assert_eq!(task_count_as(&d.addr, SCOPED_TOKEN), 1);
    let ok = SendMessage::command("workflow.run", json!({"workflow": "wipe"}))
        .bearer(USER_TOKEN)
        .result(&d.addr);
    assert!(ok["task"]["id"].is_string(), "{ok}");

    // The extended card lists exactly what each caller may run.
    let plain = skills_as(&d.addr, USER_TOKEN);
    assert!(
        plain.contains(&"deploy-web".into()) && plain.contains(&"wipe".into()),
        "{plain:?}"
    );
    assert!(!plain.contains(&"ops-only".into()), "{plain:?}");
    let scoped = skills_as(&d.addr, SCOPED_TOKEN);
    assert!(scoped.contains(&"deploy-web".into()), "{scoped:?}");
    assert!(
        !scoped.contains(&"wipe".into()) && !scoped.contains(&"ops-only".into()),
        "{scoped:?}"
    );
    assert!(
        skills_as(&d.addr, OPS_TOKEN).contains(&"ops-only".into()),
        "the operator's card"
    );

    // The feed carried the task of each run that ran, and nothing for a
    // refusal — each of which the audit mirror did record. (A command has no
    // feed kind of its own: its task, which holds the command message, is
    // what a display client sees.)
    for who in ["user:scoped", "user:plain"] {
        feed_until(&feed, &format!("{who}'s task event"), |f| {
            f.iter()
                .any(|v| v["event"]["kind"] == "task" && *owner_of(&v["event"]["data"]) == who)
        });
    }
    feed_until(&feed, "the refusals' audit events", |f| {
        f.iter()
            .filter(|v| v["event"]["kind"] == "audit")
            .filter(|v| v["event"]["data"]["action"] == "a2a.SendMessage:workflow.run")
            .filter(|v| v["event"]["data"]["outcome"] == "error")
            .count()
            >= 5
    });
    let tasks: std::collections::BTreeSet<(String, String)> = feed_events(&feed, "task")
        .into_iter()
        .map(|t| {
            (
                owner_of(&t).as_str().unwrap_or_default().to_string(),
                t["task"]["id"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let owners: Vec<&str> = tasks.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        owners,
        ["user:plain", "user:scoped"],
        "one task per run that ran, none for a refusal: {tasks:?}"
    );
}

#[test]
fn reads_are_audited_but_not_mirrored_onto_the_feed() {
    let llm = spawn_mock_llm();
    let d = spawn(config(
        &llm.uri,
        &format!(
            "  events:\n    enabled: true\n  introspection:\n    enabled: true\n{}",
            principals("")
        ),
        "workflows:\n  - name: greet\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], output: \"done\"}\n",
    ));
    let feed = watch_feed_as(&d.addr, Some(OPS_TOKEN));

    // A display client's polling: reads by op and by method, a few times.
    for _ in 0..3 {
        SendMessage::command("status", json!({}))
            .bearer(OPS_TOKEN)
            .result(&d.addr);
        SendMessage::command("debug.events", json!({"limit": 1}))
            .bearer(OPS_TOKEN)
            .result(&d.addr);
        assert!(rpc_as(&d.addr, OPS_TOKEN, 7, "ListTasks", json!({}))["result"].is_object());
        skills_as(&d.addr, OPS_TOKEN);
    }
    // A refusal the runtime makes, and a mutation — the latter preceded by
    // the mint every send is (which is plumbing, not a change).
    let refused = SendMessage::command("workflow.run", json!({"workflow": "ghost"}))
        .bearer(USER_TOKEN)
        .post(&d.addr);
    assert!(refused.get("error").is_some(), "{refused}");
    let run = SendMessage::command("workflow.run", json!({"workflow": "greet"}))
        .bearer(OPS_TOKEN)
        .result(&d.addr);
    let task = run["task"]["id"].as_str().expect("a task").to_string();
    let got = rpc_as(&d.addr, OPS_TOKEN, 8, "GetTask", json!({"id": task}));
    assert!(got["result"].is_object(), "{got}");
    // A conversational send is minted an id first, which is not a change.
    SendMessage::text("hello").bearer(OPS_TOKEN).result(&d.addr);
    feed_until(&feed, "the send's audit event", |f| {
        f.iter().any(|v| {
            v["event"]["kind"] == "audit" && v["event"]["data"]["action"] == "a2a.SendMessage"
        })
    });

    feed_until(&feed, "the run's and the refusal's audit events", |f| {
        f.iter()
            .filter(|v| {
                v["event"]["kind"] == "audit"
                    && v["event"]["data"]["action"] == "a2a.SendMessage:workflow.run"
            })
            .count()
            >= 2
    });
    let actions: Vec<String> = feed_events(&feed, "audit")
        .iter()
        .map(|a| {
            format!(
                "{} {}",
                a["action"].as_str().unwrap_or(""),
                a["outcome"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert!(
        actions.contains(&"a2a.SendMessage:workflow.run error".to_string()),
        "the refusal is mirrored: {actions:?}"
    );
    assert!(
        actions.contains(&"a2a.SendMessage:workflow.run ok".to_string()),
        "the mutation is mirrored: {actions:?}"
    );
    for read in [
        "a2a.SendMessage:status ok",
        "a2a.SendMessage:debug.events ok",
        "a2a.ListTasks ok",
        "a2a.GetTask ok",
        "a2a.GetExtendedAgentCard ok",
    ] {
        assert!(
            !actions.iter().any(|a| a == read),
            "a successful read was mirrored ({read}): {actions:?}"
        );
    }
}

#[test]
fn subagent_get_answers_the_owner_and_the_operator_only() {
    // The operator's conversation delegates to a sync subagent (mock
    // tool_calls); the subagent is the operator's.
    let llm = spawn_mock_llm_with(&json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {"instruction": "count to three", "mode": "sync"}}]},
            {"content": "delegated and done"}
        ],
        "match": [
            {"when_contains": "You are agentd, an autonomous agent.", "content": "three"}
        ]
    }));
    let d = spawn(config(
        &llm.uri,
        &format!("  introspection:\n    enabled: true\n{}", principals("")),
        "",
    ));
    let sent = SendMessage::text("count for me")
        .bearer(OPS_TOKEN)
        .result(&d.addr);
    assert_eq!(
        sent["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );
    let st = doc_of(
        &SendMessage::command("status", json!({}))
            .bearer(OPS_TOKEN)
            .result(&d.addr),
    );
    let handle = st["subagents"][0]["handle"]
        .as_str()
        .expect("a subagent exists")
        .to_string();

    let theirs = SendMessage::command("subagent.get", json!({"handle": handle}))
        .bearer(USER_TOKEN)
        .post(&d.addr);
    assert_eq!(
        theirs["error"]["code"], -32001,
        "another principal's subagent: {theirs}"
    );
    let nope = SendMessage::command("subagent.get", json!({"handle": "nope"}))
        .bearer(USER_TOKEN)
        .post(&d.addr);
    assert_eq!(
        theirs["error"]["message"], nope["error"]["message"],
        "not told it exists"
    );
    let mine = SendMessage::command("subagent.get", json!({"handle": handle}))
        .bearer(OPS_TOKEN)
        .result(&d.addr);
    assert!(
        doc_of(&mine)["subagent"]["instruction"]
            .as_str()
            .is_some_and(|i| i.contains("count to three")),
        "{mine}"
    );
}

#[test]
fn a_reload_announces_what_it_put_back_after_admin_set() {
    let llm = spawn_mock_llm();
    let d = spawn(config(&llm.uri, "  events:\n    enabled: true\n", ""));
    let feed = watch_feed(&d.addr);

    // Every runtime-settable path, set away from what the file says.
    for path in RUNTIME_SETTABLE {
        let value = match *path {
            "agent.approval" => json!("accept"),
            "a2a.introspection.enabled" => json!(true),
            other => panic!("no fixture for {other}: give it a value the file does not"),
        };
        SendMessage::command("admin.set", json!({"path": path, "value": value})).result(&d.addr);
    }
    // The file did not change; the reload puts every one of them back.
    unsafe { libc::kill(d.child.id() as i32, libc::SIGHUP) };
    feed_until(&feed, "the reload's config event", |f| {
        f.iter()
            .any(|v| v["event"]["kind"] == "config" && v["event"]["data"]["source"] == "reload")
    });
    let ev = feed_events(&feed, "config")
        .into_iter()
        .find(|e| e["source"] == "reload")
        .unwrap();
    for path in RUNTIME_SETTABLE {
        assert!(
            ev["paths"].as_array().unwrap().contains(&json!(path)),
            "the reload put {path} back and did not say so: {ev}"
        );
    }
    assert!(
        !d.stderr().contains("\"changed\":[\"nothing\"]"),
        "the reload recorded no change: {}",
        d.stderr()
    );
}
