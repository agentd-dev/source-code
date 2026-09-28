// SPDX-License-Identifier: AGPL-3.0-only
//! **The listener's protocol surface, against a real daemon.**
//!
//! What a spec client can send, and what it gets back, before and after
//! a2a-rs has had its turn:
//!
//! * every JSON-RPC request states the protocol version it speaks
//!   (`A2A-Version`), and one that does not is refused `-32009` without
//!   anything being done for it;
//! * the body is `application/json`, or the request is a bare 415;
//! * the method table is the specification's plus the declared extension
//!   methods, matched exactly — every spelling agentd once also answered is
//!   `-32601` for an operator and a user alike, and changes nothing;
//! * a stream is either exactly one frame (a read's Message) or a2a-rs's
//!   task stream, and a refusal is never a frame;
//! * a subscription to a task the caller cannot see is refused as JSON
//!   before any stream is opened;
//! * a refusal the RUNTIME made reaches the caller in the runtime's words,
//!   even when a2a-rs was the layer that answered — a draining agent's above
//!   all, which is the same object whichever path the send took;
//! * a task id is the server's: a message names a task only to continue it,
//!   and a new task's id is generated, never the caller's;
//! * a message is the spec's `SendMessageRequest`, spoken as a user, with
//!   parts of the media types the card accepts — and a field it does not know
//!   is ignored, not refused.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

use common::{HttpReply, SendMessage, a2a_post, a2a_post_omitting, rpc_body};

const OPERATOR: &str = "protocol-e2e-operator-token-0123456789";
const USER_A: &str = "protocol-e2e-user-a-token-0123456789";
const AGENT_B: &str = "protocol-e2e-agent-b-token-0123456789";

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

/// A mock model that answers every turn with one line — slowly, when the
/// conversation asks for it, so a task can be held open.
fn spawn_mock_llm() -> MockLlm {
    let pb = common::unique_path("protocol-playbook", "json");
    std::fs::write(
        &pb,
        json!({
            "turns": [{"content": "an answer"}],
            "match": [{"when_contains": "slowly", "content": "a slow answer", "delay_ms": 4000}],
        })
        .to_string(),
    )
    .unwrap();
    let addr_file = common::unique_path("protocol-mock-llm", "addr");
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
    addr: String,
    _llm: MockLlm,
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

/// A daemon with three principals: an operator, a user and an agent, each
/// named by its bearer — so the two non-operators are two owners.
fn boot() -> Daemon {
    boot_with("")
}

/// [`boot`] with `extra` appended to the config.
fn boot_with(extra: &str) -> Daemon {
    let llm = spawn_mock_llm();
    let cfg = common::unique_path("a2a-protocol", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: a2a-protocol\n  instruction: You are a test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: {}\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{}\n\
             \x20 principals:\n\
             \x20   - id: ops\n\
             \x20     match: {{ bearer_ref: \"{{{{secret:PROTOCOL_E2E_OPS}}}}\" }}\n\
             \x20     role: operator\n\
             \x20   - id: user-a\n\
             \x20     match: {{ bearer_ref: \"{{{{secret:PROTOCOL_E2E_A}}}}\" }}\n\
             \x20     role: user\n\
             \x20   - id: agent-b\n\
             \x20     match: {{ bearer_ref: \"{{{{secret:PROTOCOL_E2E_B}}}}\" }}\n\
             \x20     role: agent\n\
             lifecycle:\n  run_until: drained\n  drain_timeout: 60s\n\
             observability:\n  log_level: info\n{}",
            llm.uri,
            common::free_port(),
            extra
        ),
    )
    .unwrap();
    let stderr_path = common::unique_path("a2a-protocol-daemon", "log");
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("PROTOCOL_E2E_OPS", OPERATOR)
        .env("PROTOCOL_E2E_A", USER_A)
        .env("PROTOCOL_E2E_B", AGENT_B)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
        .spawn()
        .expect("spawn agentd");
    let addr = common::wait_a2a_bound(&stderr_path);
    Daemon {
        child,
        stderr_path,
        cfg,
        addr,
        _llm: llm,
    }
}

fn auth(bearer: &str) -> String {
    format!("Bearer {bearer}")
}

/// `method` with `params` as `bearer`, the whole reply.
fn call(d: &Daemon, bearer: &str, id: i64, method: &str, params: Value) -> HttpReply {
    a2a_post(
        &d.addr,
        &rpc_body(id, method, params),
        &[("Authorization", &auth(bearer))],
    )
}

/// How many tasks `bearer` can list — the state a refused call must not move.
fn task_count(d: &Daemon, bearer: &str) -> usize {
    let v = call(d, bearer, 900, "ListTasks", json!({"pageSize": 100})).json();
    assert!(v.get("error").is_none(), "ListTasks: {v}");
    v["result"]["tasks"].as_array().map_or(0, Vec::len)
}

/// Every `data:` payload of an SSE body, as JSON.
fn frames(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect()
}

fn is_json(r: &HttpReply) -> bool {
    r.header("content-type")
        .is_some_and(|t| t.starts_with("application/json"))
}

/// No part of an error says it came from a2a-rs.
fn no_sdk_domain(error: &Value) {
    let data = error["data"].as_array().cloned().unwrap_or_default();
    assert!(
        data.iter().all(|d| d["domain"] != "a2a-rs"),
        "a runtime refusal carries a2a-rs's ErrorInfo: {error}"
    );
}

/// A client that does not state the version — every a2a-rs client up to 0.10
/// among them — is refused -32009 at HTTP 200, told which version is served,
/// and nothing is done for it. A 1.0 client with a patch number is fine.
#[test]
fn version_gate() {
    let d = boot();
    let before = task_count(&d, USER_A);
    let send = SendMessage::text("hello").return_immediately();
    let auth_a = auth(USER_A);
    for version in [None, Some("0.3"), Some("1.1"), Some("")] {
        let mut extra = vec![("Authorization", auth_a.as_str())];
        if let Some(v) = version {
            extra.push(("A2A-Version", v));
        }
        let reply = a2a_post_omitting(&d.addr, &send.body(41), &["A2A-Version"], &extra);
        assert_eq!(reply.status, 200, "{version:?}: {reply:?}");
        let v = reply.json();
        assert_eq!(v["id"], 41, "{version:?}: {v}");
        assert_eq!(v["error"]["code"], -32009, "{version:?}: {v}");
        let info = &v["error"]["data"][0];
        assert_eq!(info["reason"], "VERSION_NOT_SUPPORTED", "{v}");
        assert_eq!(info["domain"], "a2a-protocol.org", "{v}");
        assert_eq!(info["metadata"]["supportedVersions"], "1.0", "{v}");
    }
    assert_eq!(task_count(&d, USER_A), before, "a refused send made a task");

    let reply = a2a_post_omitting(
        &d.addr,
        &rpc_body(42, "ListTasks", json!({})),
        &["A2A-Version"],
        &[("Authorization", &auth_a), ("A2A-Version", "1.0.2")],
    );
    let v = reply.json();
    assert!(
        v.get("error").is_none(),
        "a patch is the same protocol: {v}"
    );
}

/// Every name agentd answered before 1.0 besides the table — the card read as
/// a method, the `a2a.` prefix, pairing, the 0.3 names — is -32601 for the
/// operator and a user alike, and changes nothing. The pre-1.0 card path is
/// gone too.
#[test]
fn vocabulary_deny_list() {
    let d = boot();
    let before = task_count(&d, OPERATOR);
    let send = SendMessage::text("hello").return_immediately().params();
    let legacy = [
        ("GetAgentCard", json!({})),
        ("agent/card", json!({})),
        ("agent/getAuthenticatedExtendedCard", json!({})),
        ("a2a.GetAgentCard", json!({})),
        ("a2a.SendMessage", send.clone()),
        ("a2a.ListTasks", json!({})),
        ("a2a.GetExtendedAgentCard", json!({})),
        ("Pair", json!({"code": "123456"})),
        ("interface.pair", json!({"code": "123456"})),
        ("message/send", send.clone()),
        ("message/stream", send.clone()),
        ("tasks/get", json!({"id": "task-x"})),
        ("SetTaskPushNotificationConfig", json!({"taskId": "task-x"})),
    ];
    for bearer in [OPERATOR, USER_A] {
        for (i, (method, params)) in legacy.iter().enumerate() {
            let reply = call(&d, bearer, 500 + i as i64, method, params.clone());
            assert_eq!(reply.status, 200, "{method}: {reply:?}");
            assert!(is_json(&reply), "{method}: {reply:?}");
            let v = reply.json();
            assert_eq!(v["error"]["code"], -32601, "{method} as {bearer}: {v}");
            assert_eq!(v["id"], 500 + i as i64, "{method}: {v}");
        }
    }
    assert_eq!(
        task_count(&d, OPERATOR),
        before,
        "a refused name did something"
    );

    assert_eq!(
        common::http_get(&d.addr, "/.well-known/agent.json").status,
        404
    );
    assert!(common::get_card(&d.addr)["name"].is_string());
}

/// The binding is JSON over `application/json`. Anything else is a bare 415.
#[test]
fn content_type() {
    let d = boot();
    let body = rpc_body(7, "ListTasks", json!({}));
    let auth_a = auth(USER_A);
    for ct in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let mut extra = vec![("Authorization", auth_a.as_str())];
        if let Some(ct) = ct {
            extra.push(("Content-Type", ct));
        }
        let reply = a2a_post_omitting(&d.addr, &body, &["Content-Type"], &extra);
        assert_eq!(reply.status, 415, "{ct:?}: {reply:?}");
        assert!(reply.body.is_empty(), "{ct:?}: {reply:?}");
    }
    let reply = a2a_post_omitting(
        &d.addr,
        &body,
        &["Content-Type"],
        &[
            ("Authorization", &auth_a),
            ("Content-Type", "application/json; charset=utf-8"),
        ],
    );
    assert_eq!(reply.status, 200, "{reply:?}");
    assert!(reply.json().get("error").is_none(), "{reply:?}");
}

/// A read op streamed is exactly one frame — the Message, under the
/// request's own id, with no SSE `id:` — and then the stream ends. Work is
/// a2a-rs's task stream: the task first, then its transitions to a settled
/// state. A refusal before the first frame is JSON, never a frame.
#[test]
fn stream_shapes() {
    let d = boot();

    let status = SendMessage::command("status", json!({}))
        .streaming()
        .bearer(OPERATOR);
    let headers = status.headers();
    let extra: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let reply = a2a_post(&d.addr, &status.body(31), &extra);
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.starts_with("text/event-stream")),
        "{reply:?}"
    );
    let got = frames(&reply.body);
    assert_eq!(got.len(), 1, "exactly one frame: {}", reply.body);
    assert_eq!(got[0]["id"], 31, "the request's own id: {}", got[0]);
    assert_eq!(
        got[0]["result"]["message"]["role"], "ROLE_AGENT",
        "{}",
        got[0]
    );
    assert!(got[0]["result"].get("task").is_none(), "{}", got[0]);
    assert!(
        !reply.body.lines().any(|l| l.starts_with("id:")),
        "a single reply has nothing to resume from: {}",
        reply.body
    );

    let reply = SendMessage::text("hello")
        .streaming()
        .bearer(USER_A)
        .post_raw(&d.addr);
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.starts_with("text/event-stream")),
        "{reply:?}\n{}",
        d.stderr()
    );
    let got = frames(&reply.body);
    assert!(
        got.first()
            .is_some_and(|f| f["result"]["task"]["id"].is_string()),
        "the stream opens with the task: {got:#?}"
    );
    assert!(
        got.iter().all(|f| f["id"] == 1 && f.get("error").is_none()),
        "{got:#?}"
    );
    let settled = |f: &Value| {
        let state = f["result"]["statusUpdate"]["status"]["state"]
            .as_str()
            .or_else(|| f["result"]["task"]["status"]["state"].as_str())
            .unwrap_or("");
        [
            "TASK_STATE_COMPLETED",
            "TASK_STATE_FAILED",
            "TASK_STATE_INPUT_REQUIRED",
        ]
        .contains(&state)
    };
    assert!(
        got.last().is_some_and(settled),
        "the stream ends settled: {got:#?}"
    );

    // A message the runtime refuses, streamed: a JSON answer, not a frame.
    let reply = a2a_post(
        &d.addr,
        &rpc_body(
            33,
            "SendStreamingMessage",
            json!({"message": {"role": "ROLE_USER", "messageId": "m-empty", "parts": [{"text": " "}]},
                   "configuration": {"returnImmediately": false}}),
        ),
        &[("Authorization", &auth(USER_A))],
    );
    assert!(is_json(&reply), "a refusal is JSON: {reply:?}");
    assert_eq!(reply.json()["error"]["code"], -32602, "{reply:?}");
}

/// a2a-rs 0.10 would open a stream on a task it could not find. The
/// listener reads the task first, as the caller: another principal's task
/// and an id nobody holds both get "not found" as JSON, and no stream opens.
#[test]
fn subscribe_to_an_invisible_task() {
    let d = boot();
    let sent = SendMessage::text("hello")
        .return_immediately()
        .bearer(USER_A)
        .result(&d.addr);
    let task = sent["task"]["id"].as_str().expect("a task id").to_string();
    for id in [task.as_str(), "task-nobody-holds"] {
        let reply = call(&d, AGENT_B, 61, "SubscribeToTask", json!({"id": id}));
        assert!(is_json(&reply), "{id}: no stream is opened: {reply:?}");
        assert!(frames(&reply.body).is_empty(), "{id}: {reply:?}");
        let v = reply.json();
        assert_eq!(v["error"]["code"], -32001, "{id}: {v}");
        assert_eq!(v["id"], 61, "{id}: {v}");
    }
}

/// A refusal the runtime made reaches the caller exactly as the runtime made
/// it — code, message and data — even though a2a-rs answered the call. Left
/// to a2a-rs, the message came back prefixed and the runtime's reasons were
/// replaced by an `ErrorInfo` naming a2a-rs.
#[test]
fn runtime_refusals_survive_the_sdk() {
    let d = boot();
    // A message with nothing a turn could be made of: the runtime refuses the
    // turn, on the natural-language path a2a-rs serves.
    let empty = json!({"message": {"role": "ROLE_USER", "messageId": "m-blank",
        "parts": [{"text": " "}]}, "configuration": {"returnImmediately": true}});
    for method in ["SendMessage", "SendStreamingMessage"] {
        let reply = call(&d, USER_A, 71, method, empty.clone());
        assert!(is_json(&reply), "{method}: {reply:?}");
        let v = reply.json();
        assert_eq!(
            v["error"],
            json!({"code": -32602, "message": "message has no text or command part"}),
            "{method}: {v}"
        );
        assert_eq!(v["id"], 71);
    }

    // A read of a task the caller cannot see: the runtime's own words.
    let v = call(
        &d,
        USER_A,
        72,
        "GetTask",
        json!({"id": "task-nobody-holds"}),
    )
    .json();
    assert_eq!(
        v["error"],
        json!({"code": -32001, "message": "task not found"}),
        "{v}"
    );

    // A command no table row names travels a2a-rs's path too (only a read's
    // Message is answered by the listener), and its reason survives.
    let v = SendMessage::command("no.such.op", json!({}))
        .bearer(OPERATOR)
        .post(&d.addr);
    let error = &v["error"];
    assert_eq!(error["code"], -32602, "{v}");
    assert_eq!(error["message"], "unknown command \"no.such.op\"", "{v}");
    assert_eq!(error["data"][0]["reason"], "UNKNOWN_OP", "{v}");
    assert_eq!(error["data"][0]["domain"], "agentd.dev", "{v}");
    no_sdk_domain(error);
}

/// Whether `id` is a UUIDv4 in its canonical lower-case form — the id a2a-rs
/// generates, and so the shape of every task id a send creates.
fn is_uuid_v4(id: &str) -> bool {
    id.len() == 36
        && id.as_bytes()[14] == b'4'
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
        })
}

/// The task a send refused must not exist, for its caller or anyone: the
/// operator lists every task.
fn assert_no_task_named(d: &Daemon, id: &str) {
    let v = call(d, OPERATOR, 901, "ListTasks", json!({"pageSize": 100})).json();
    let tasks = v["result"]["tasks"].as_array().cloned().unwrap_or_default();
    assert!(
        tasks.iter().all(|t| t["id"] != id),
        "a task was created under the caller's id {id}: {v}"
    );
}

/// A task id is the server's (A2A 1.0 §3.4.2).
///
/// - A send that names no task — or names it empty — gets a new task under an
///   id a2a-rs generated.
/// - A named task must exist and be the caller's: an unknown id and another
///   principal's task are "not found", and no task is created under the id.
/// - A settled task takes no further messages.
/// - A task still open continues under its own id.
/// - A command names no task; and a context that is not the task's own is
///   refused.
#[test]
fn task_id_rules() {
    let d = boot();

    let done = SendMessage::text("hello").bearer(USER_A).result(&d.addr);
    let id = done["task"]["id"].as_str().expect("a task id").to_string();
    assert!(is_uuid_v4(&id), "a generated id: {done}");
    assert_eq!(
        done["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{done}"
    );
    // Empty or blank alike: the listener reads a blank id as a2a-rs does,
    // as none, so the send is a new task rather than a lookup of the id
    // generated in its place.
    for (n, none) in ["", "  "].into_iter().enumerate() {
        let mut blank = SendMessage::text("hello").return_immediately().params();
        blank["message"]["taskId"] = json!(none);
        let fresh = call(&d, USER_A, 20 + n as i64, "SendMessage", blank).json();
        let fresh_id = fresh["result"]["task"]["id"].as_str().unwrap_or_default();
        assert!(is_uuid_v4(fresh_id) && fresh_id != id, "{none:?}: {fresh}");
    }
    // A command with a blank id names no task either.
    let status = SendMessage::command("status", json!({}))
        .task("  ")
        .bearer(OPERATOR)
        .post(&d.addr);
    assert!(status.get("error").is_none(), "{status}");

    let before_a = task_count(&d, USER_A);
    let before_b = task_count(&d, AGENT_B);
    for streaming in [false, true] {
        let with = |s: SendMessage| if streaming { s.streaming() } else { s };
        // An id nobody holds: never the id of a new task.
        let reply =
            with(SendMessage::text("hi").task("my-own-id").bearer(USER_A)).post_raw(&d.addr);
        assert!(is_json(&reply), "{reply:?}");
        let v = reply.json();
        assert_eq!(v["error"]["code"], -32001, "streaming {streaming}: {v}");
        assert_no_task_named(&d, "my-own-id");
        // Another principal's task: the same answer, so its existence is not
        // disclosed — and not a fresh task of the caller's own either.
        let v = with(SendMessage::text("hi").task(&id).bearer(AGENT_B)).post(&d.addr);
        assert_eq!(v["error"]["code"], -32001, "streaming {streaming}: {v}");
        // A settled task.
        let v = with(SendMessage::text("again").task(&id).bearer(USER_A)).post(&d.addr);
        assert_eq!(
            v["error"],
            json!({"code": -32004,
                   "message": format!("task {id} is TASK_STATE_COMPLETED; it accepts no further messages")}),
            "streaming {streaming}: {v}"
        );
    }
    assert_eq!(
        task_count(&d, USER_A),
        before_a,
        "a refused send made a task"
    );
    assert_eq!(
        task_count(&d, AGENT_B),
        before_b,
        "a refused send made a task"
    );
    let still = call(&d, USER_A, 12, "GetTask", json!({"id": id})).json();
    assert_eq!(
        still["result"]["status"]["state"], "TASK_STATE_COMPLETED",
        "a settled task stays settled: {still}"
    );

    // A command starts its own task, on either path it can take — refused
    // before anything reads the task it names, so the answer is the same for
    // a task that exists and one that does not.
    for (op, task) in [
        ("status", id.as_str()),
        ("workflow.run", id.as_str()),
        ("workflow.run", "my-own-id"),
    ] {
        for streaming in [false, true] {
            let mut s = SendMessage::command(op, json!({"workflow": "none"}))
                .task(task)
                .bearer(OPERATOR);
            if streaming {
                s = s.streaming();
            }
            let reply = s.post_raw(&d.addr);
            assert!(is_json(&reply), "{op}: {reply:?}");
            let v = reply.json();
            assert_eq!(v["error"]["code"], -32602, "{op} {task}: {v}");
            assert_eq!(
                v["error"]["data"][1]["reason"], "COMMAND_TASK_ID",
                "{op}: {v}"
            );
            assert_eq!(
                v["error"]["data"][0]["fieldViolations"][0]["field"], "message.taskId",
                "{op}: {v}"
            );
        }
    }

    // A task still open continues under its own id.
    let open = SendMessage::text("answer slowly")
        .return_immediately()
        .bearer(USER_A)
        .result(&d.addr);
    let open_id = open["task"]["id"].as_str().expect("a task id").to_string();
    let ctx = open["task"]["contextId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let more = SendMessage::text("and one more thing")
        .task(&open_id)
        .return_immediately()
        .bearer(USER_A)
        .result(&d.addr);
    assert_eq!(more["task"]["id"], open_id.as_str(), "{more}");
    assert_eq!(more["task"]["contextId"], ctx.as_str(), "{more}");
    // …but not from another conversation.
    let v = SendMessage::text("elsewhere")
        .task(&open_id)
        .context("not-the-tasks-context")
        .return_immediately()
        .bearer(USER_A)
        .post(&d.addr);
    assert_eq!(v["error"]["code"], -32602, "{v}");
}

/// What a message may carry. A file — bytes or a URL — is no media type the
/// card accepts, and refuses the whole message with `-32005`, naming what was
/// sent and what is accepted; nothing is created for it. The message is a
/// user's: any other role is `-32602`.
#[test]
fn errors_codes() {
    let d = boot();
    let before = task_count(&d, USER_A);
    let file = |part: Value| {
        json!({"message": {"role": "ROLE_USER", "messageId": "m-file",
            "parts": [{"text": "look at this"}, part]},
            "configuration": {"returnImmediately": true}})
    };
    for method in ["SendMessage", "SendStreamingMessage"] {
        let reply = call(
            &d,
            USER_A,
            21,
            method,
            file(json!({"url": "https://files.example/cat.png", "mediaType": "image/png"})),
        );
        assert!(is_json(&reply), "{method}: {reply:?}");
        let v = reply.json();
        assert_eq!(v["error"]["code"], -32005, "{method}: {v}");
        assert_eq!(
            v["error"]["message"],
            "media type image/png is not supported; accepted: text/plain, application/json",
            "{method}: {v}"
        );
        assert_eq!(
            v["error"]["data"][0]["reason"], "CONTENT_TYPE_NOT_SUPPORTED",
            "{v}"
        );
        no_sdk_domain(&v["error"]);
    }
    let v = call(&d, USER_A, 22, "SendMessage", file(json!({"raw": "aGk="}))).json();
    assert_eq!(v["error"]["code"], -32005, "{v}");
    assert_eq!(
        v["error"]["message"],
        "media type unspecified is not supported; accepted: text/plain, application/json",
        "{v}"
    );
    assert_eq!(
        task_count(&d, USER_A),
        before,
        "a refused message made a task"
    );

    // The spec's roles: a caller speaks as the user, never as the agent, and
    // says so.
    for role in [json!("ROLE_AGENT"), json!("ROLE_UNSPECIFIED"), Value::Null] {
        let mut params = SendMessage::text("hello").return_immediately().params();
        params["message"]["role"] = role.clone();
        let v = call(&d, USER_A, 23, "SendMessage", params).json();
        assert_eq!(v["error"]["code"], -32602, "role {role}: {v}");
    }
    assert_eq!(
        task_count(&d, USER_A),
        before,
        "a refused message made a task"
    );

    // A JSON DataPart is accepted: the card lists application/json.
    let mut params = SendMessage::text("with data").return_immediately().params();
    params["message"]["parts"] = json!([{"text": "the numbers:"}, {"data": {"k": [1, 2]}}]);
    let v = call(&d, USER_A, 24, "SendMessage", params).json();
    assert!(v["result"]["task"]["id"].is_string(), "{v}");
}

/// A field the spec's types do not know is ignored — on the request and on
/// the message — as ProtoJSON says a reader must; a newer client is not a
/// malformed one.
#[test]
fn unknown_fields_are_ignored() {
    let d = boot();
    let mut params = SendMessage::text("hello").return_immediately().params();
    params["futureField"] = json!({"x": 1});
    params["message"]["futureField"] = json!("from a later version");
    let v = call(&d, USER_A, 81, "SendMessage", params).json();
    assert!(v.get("error").is_none(), "{v}");
    assert!(v["result"]["task"]["id"].is_string(), "{v}");
}

/// While the agent drains, every send is refused with one object — code,
/// message and reason — whichever path answers it: a2a-rs's for a message and
/// for a command that does work, the listener's own for a read. A stream asked
/// for is answered as JSON: a refusal is never a frame.
#[test]
fn draining_is_identical_on_both_paths() {
    // A shutdown workflow that waits for a signal nobody sends holds the drain
    // open, so the listener is still answering while the agent refuses work.
    let d = boot_with(
        "workflows:\n\
         \x20 - name: linger\n    steps:\n\
         \x20     bye: {kind: event, on: lifecycle.shutdown}\n\
         \x20     hold: {kind: wait, on: signal, signal: never, depends_on: [bye]}\n\
         \x20     f: {kind: finish, depends_on: [hold]}\n\
         \x20 - name: greet\n    steps:\n\
         \x20     s: {kind: manual}\n\
         \x20     f: {kind: finish, depends_on: [s]}\n",
    );
    SendMessage::command("admin.drain", json!({"reason": "the test"}))
        .bearer(OPERATOR)
        .result(&d.addr);

    let expected = |v: &Value, what: &str| {
        let error = &v["error"];
        assert_eq!(error["code"], -32603, "{what}: {v}");
        assert_eq!(error["message"], "the agent is draining", "{what}: {v}");
        assert_eq!(error["data"][0]["reason"], "DRAINING", "{what}: {v}");
        assert_eq!(error["data"][0]["domain"], "agentd.dev", "{what}: {v}");
        no_sdk_domain(error);
        error.clone()
    };
    let words = SendMessage::text("hello").bearer(USER_A).post(&d.addr);
    let one = expected(&words, "a message");
    let run = SendMessage::command("workflow.run", json!({"workflow": "greet"}))
        .bearer(OPERATOR)
        .post(&d.addr);
    assert_eq!(expected(&run, "workflow.run"), one, "{run}");
    let read = SendMessage::command("status", json!({}))
        .bearer(OPERATOR)
        .post(&d.addr);
    assert_eq!(expected(&read, "a read"), one, "{read}");

    let streamed = SendMessage::text("hello")
        .streaming()
        .bearer(USER_A)
        .post_raw(&d.addr);
    assert!(is_json(&streamed), "a refusal is JSON: {streamed:?}");
    assert_eq!(expected(&streamed.json(), "a stream"), one);
}

/// The listener once rewrote sends for the protocol layer: a task id minted
/// ahead of the message, and the 0.3 `configuration.blocking` translated to
/// 1.0's field. Both are gone — a task id is a2a-rs's to generate, and a
/// field 1.0 does not have means nothing — and they stay gone.
#[test]
fn the_listener_rewrites_no_send() {
    let src = include_str!("../../agentd/src/a2a/serve/dispatch.rs");
    assert!(
        !src.contains("\"blocking\""),
        "serve/dispatch.rs reads configuration.blocking again"
    );
    assert!(
        !src.contains("NewTaskId"),
        "serve/dispatch.rs mints task ids again"
    );
}
