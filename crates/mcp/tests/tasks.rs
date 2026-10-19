// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP Tasks extension (SEP-2663) from the client's side, against a
//! 2026-07-28 server that holds every request to the revision's rules and
//! to the extension's: a task only for a call that declared the extension,
//! `-32021` for a `tasks/*` request that did not, `Mcp-Name` equal to the
//! `taskId` on every `tasks/*` request, and `-32602` for a task it does not
//! know.
//!
//! - A bounded call declares the extension and follows its task to the end
//!   at the server's `pollIntervalMs`; an unbounded one, or one on a
//!   connection that does not declare, is answered inline.
//! - A seed that already ended is fetched for its outcome.
//! - A server that requires the extension refuses a call without it, and the
//!   error names the extension.
//! - A task on any method but `tools/call` is an invalid answer.
//! - The call's bound cancels a task still open, with `tasks/cancel`; so does
//!   giving a task up, and a follow its caller stops ends at once.
//! - An input request reaches the elicitation handler once, however many
//!   polls list it, and the answer goes back with `tasks/update`; a task
//!   handed back open has every answer sent, or its key released.
//! - A listen asks for the open tasks' ids and declares the extension, and a
//!   pushed `notifications/tasks` reaches the host's queue.

mod common;

use common::*;
use mcp::client::{FirstRequest, McpClient, McpError};
use mcp::inbound::{Answer, Handler, Inbound};
use mcp::tasks::{Followed, Tracker};
use mcp::wire::{CallToolResult, Called};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TASKS: &str = "io.modelcontextprotocol/tasks";
/// The interval the server asks to be polled at.
const POLL_MS: u64 = 150;

/// What a task does as it is polled.
#[derive(Clone, Copy, PartialEq)]
enum Script {
    /// Working for two polls, then completed.
    Done,
    /// Working for ever.
    Stuck,
    /// Asks for a name, then greets it.
    Ask,
    /// Failed with a JSON-RPC error on the first poll.
    Fail,
    /// Completed with a tool error.
    ToolError,
    /// Forgotten by the server after the first poll (`-32602`).
    Vanish,
    /// Finished at once: the seed says `completed`, and only `tasks/get`
    /// carries the result.
    Instant,
    /// Asks two questions at once, `a` and `b`.
    Two,
    /// [`Script::Two`], every `tasks/update` refused `-32603`.
    TwoFail,
    /// Every `tasks/get` refused for good (`-32001`).
    Refuse,
    /// Every `tasks/get` answered about another task.
    Other,
}

/// One task: its script, its polls, the name it was told, and whether the
/// next poll still shows the question it answered — an eventually consistent
/// server lists an answered request once more.
type Held = (Script, u32, Option<String>, bool);

#[derive(Default)]
struct Tasks {
    next: AtomicU64,
    open: Mutex<BTreeMap<String, Held>>,
    /// When each `tasks/get` arrived.
    polled_at: Mutex<Vec<Instant>>,
}

fn declares(req: &Req) -> bool {
    req.meta("clientCapabilities")
        .and_then(|c| c.pointer(&format!("/extensions/{}", TASKS.replace('/', "~1"))))
        .is_some()
}

fn task_json(id: &str, status: &str, extra: Value) -> Value {
    let mut t = json!({"taskId": id, "status": status, "createdAt": "2026-10-19T10:00:00Z",
                       "lastUpdatedAt": "2026-10-19T10:00:00Z", "ttlMs": 60000, "pollIntervalMs": POLL_MS});
    if let (Some(o), Value::Object(e)) = (t.as_object_mut(), extra) {
        o.extend(e);
    }
    t
}

fn elicit(message: &str) -> Value {
    json!({"method": "elicitation/create", "params": {"mode": "form", "message": message,
        "requestedSchema": {"type": "object", "properties": {"x": {"type": "number"}}}}})
}

fn ask_request() -> Value {
    json!({"name": {"method": "elicitation/create", "params": {"mode": "form", "message": "Your name?",
        "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}}})
}

/// The server. `requires` answers a call without the extension `-32021`.
fn server(requires: bool) -> (String, Seen, Arc<Tasks>) {
    let state = Arc::new(Tasks::default());
    let s = Arc::clone(&state);
    let (ep, seen) = spawn(move |req, w| {
        let id = req.id();
        let supported = [MODERN.to_string()];
        if let Some((status, body)) = stateless_refusal(req, &supported) {
            return respond(w, status, &[], &body);
        }
        let params = &req.body["params"];
        let task_method = req.method().starts_with("tasks/");
        if task_method {
            if req.count("mcp-name") != 1 || req.header("mcp-name") != params["taskId"].as_str() {
                return respond(
                    w,
                    400,
                    &[],
                    &error(&id, -32020, "Mcp-Name does not match the taskId"),
                );
            }
            if !declares(req) {
                return respond(w, 200, &[], &missing_capability(&id));
            }
        }
        match req.method() {
            "server/discover" => {
                let opts = StatelessOpts {
                    caps: json!({"tools": {}, "prompts": {}, "extensions": {TASKS: {}}}),
                    ..StatelessOpts::default()
                };
                respond(w, 200, &[], &discover_answer(&id, &opts))
            }
            "tools/list" => respond(
                w,
                200,
                &[],
                &result(
                    &id,
                    json!({"tools": [{"name": "job", "inputSchema": {"type": "object"}}]}),
                ),
            ),
            "tools/call" => {
                let script = match params["name"].as_str().unwrap_or_default() {
                    "stuck" => Script::Stuck,
                    "ask" => Script::Ask,
                    "fail" => Script::Fail,
                    "tool_error" => Script::ToolError,
                    "vanish" => Script::Vanish,
                    "instant" => Script::Instant,
                    "two" => Script::Two,
                    "two_fail" => Script::TwoFail,
                    "refuse" => Script::Refuse,
                    "other" => Script::Other,
                    _ => Script::Done,
                };
                if !declares(req) {
                    if requires {
                        return respond(w, 200, &[], &missing_capability(&id));
                    }
                    let inline = json!({"resultType": "complete", "content": [{"type": "text", "text": "inline"}], "isError": false});
                    return respond(w, 200, &[], &result(&id, inline));
                }
                let n = s.next.fetch_add(1, Ordering::SeqCst);
                let tid = format!("task-{n}");
                s.open
                    .lock()
                    .unwrap()
                    .insert(tid.clone(), (script, 0, None, false));
                let seed = match script {
                    Script::Instant => "completed",
                    _ => "working",
                };
                let mut created = task_json(&tid, seed, json!({}));
                created["resultType"] = json!("task");
                respond(w, 200, &[], &result(&id, created))
            }
            "tasks/get" => {
                s.polled_at.lock().unwrap().push(Instant::now());
                let tid = params["taskId"].as_str().unwrap_or_default().to_string();
                let mut open = s.open.lock().unwrap();
                let Some((script, polls, name, stale)) = open.get_mut(&tid) else {
                    return respond(
                        w,
                        200,
                        &[],
                        &error(&id, -32602, "Failed to retrieve task: Task not found"),
                    );
                };
                *polls += 1;
                let was_stale = std::mem::take(stale);
                let task = match (*script, *polls, name.clone()) {
                    (Script::Ask, _, Some(_)) if was_stale => task_json(
                        &tid,
                        "input_required",
                        json!({"inputRequests": ask_request()}),
                    ),
                    (Script::Done, p, _) if p >= 2 => task_json(
                        &tid,
                        "completed",
                        json!({"result":
                        {"content": [{"type": "text", "text": "done"}], "isError": false}}),
                    ),
                    (Script::Ask, _, Some(n)) => task_json(
                        &tid,
                        "completed",
                        json!({"result":
                        {"content": [{"type": "text", "text": format!("Hello, {n}!")}], "isError": false}}),
                    ),
                    (Script::Ask, _, None) => task_json(
                        &tid,
                        "input_required",
                        json!({"inputRequests": ask_request()}),
                    ),
                    (Script::Fail, _, _) => task_json(
                        &tid,
                        "failed",
                        json!({"statusMessage": "rate limited",
                        "error": {"code": -32603, "message": "API rate limit exceeded"}}),
                    ),
                    (Script::ToolError, _, _) => task_json(
                        &tid,
                        "completed",
                        json!({"result":
                        {"content": [{"type": "text", "text": "no such city"}], "isError": true}}),
                    ),
                    (Script::Instant, _, _) => task_json(
                        &tid,
                        "completed",
                        json!({"result":
                        {"content": [{"type": "text", "text": "at once"}], "isError": false}}),
                    ),
                    (Script::Two | Script::TwoFail, _, _) => task_json(
                        &tid,
                        "input_required",
                        json!({"inputRequests": {"a": elicit("First?"), "b": elicit("Second?")}}),
                    ),
                    (Script::Other, _, _) => task_json("someone-else", "working", json!({})),
                    (Script::Refuse, _, _) => {
                        return respond(w, 200, &[], &error(&id, -32001, "not for you"));
                    }
                    (Script::Vanish, p, _) if p >= 2 => {
                        open.remove(&tid);
                        return respond(
                            w,
                            200,
                            &[],
                            &error(&id, -32602, "Failed to retrieve task: Task has expired"),
                        );
                    }
                    _ => task_json(&tid, "working", json!({})),
                };
                let mut answer = task;
                answer["resultType"] = json!("complete");
                respond(w, 200, &[], &result(&id, answer))
            }
            "tasks/update" => {
                let tid = params["taskId"].as_str().unwrap_or_default();
                if s.open
                    .lock()
                    .unwrap()
                    .get(tid)
                    .is_some_and(|h| h.0 == Script::TwoFail)
                {
                    return respond(w, 200, &[], &error(&id, -32603, "busy"));
                }
                if let Some((_, _, name, stale)) = s.open.lock().unwrap().get_mut(tid) {
                    *name = params["inputResponses"]["name"]["content"]["name"]
                        .as_str()
                        .map(str::to_string);
                    *stale = true;
                }
                respond(w, 200, &[], &result(&id, json!({"resultType": "complete"})))
            }
            "tasks/cancel" => respond(w, 200, &[], &result(&id, json!({"resultType": "complete"}))),
            // A task where none may be: an invalid answer for the client.
            "prompts/get" => {
                let mut t = task_json("task-prompt", "working", json!({}));
                t["resultType"] = json!("task");
                respond(w, 200, &[], &result(&id, t))
            }
            "subscriptions/listen" => listen(req, w),
            m if m.starts_with("notifications/") => respond_raw(w, 202, &[], ""),
            _ => respond(w, 404, &[], &error(&id, -32601, "Method not found")),
        }
    });
    (ep, seen, state)
}

fn missing_capability(id: &Value) -> Value {
    let mut e = error(id, -32021, "Missing required client capability");
    e["error"]["data"] = json!({"requiredCapabilities": {"extensions": {TASKS: {}}}});
    e
}

/// A listen: acknowledged with the task ids it asked for, then one push for
/// the first of them, then held a moment.
fn listen(req: &Req, w: &mut std::net::TcpStream) {
    let id = req.id();
    let asked = req.body["params"]["notifications"].clone();
    let meta = json!({"io.modelcontextprotocol/subscriptionId": id});
    let mut frames = vec![
        json!({"jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
        "params": {"_meta": meta, "notifications": asked}}),
    ];
    if let Some(first) = asked["taskIds"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
    {
        let mut pushed = task_json(
            first,
            "completed",
            json!({"result": {"content": [{"type": "text", "text": "pushed"}]}}),
        );
        pushed["_meta"] = meta.clone();
        frames.push(json!({"jsonrpc": "2.0", "method": "notifications/tasks", "params": pushed}));
    }
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    for f in frames {
        out.push_str(&format!("data: {f}\n\n"));
    }
    let _ = w.write_all(out.as_bytes());
    let _ = w.flush();
    std::thread::sleep(Duration::from_millis(300));
}

/// A call with a bound, the kind that declares the extension.
fn within(c: &McpClient, tool: &str) -> Result<CallToolResult, McpError> {
    c.call_tool_with_meta_within(tool, Some(json!({})), json!({}), Duration::from_secs(10))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The task a declaring call to `tool` became.
fn task_of(c: &McpClient, tool: &str) -> mcp::wire::Task {
    match c.call_tool_called(
        tool,
        Some(json!({})),
        None,
        Some(Duration::from_secs(5)),
        true,
    ) {
        Ok(Called::Task(t)) => t,
        other => panic!("expected a task, got {other:?}"),
    }
}

fn connect(ep: &str, tasks: bool, handler: Option<Arc<dyn Handler>>) -> McpClient {
    let mut c = McpClient::connect("t", ep, vec![], Duration::from_secs(5))
        .unwrap()
        .start_with(FirstRequest::Discover)
        .with_tasks(tasks);
    if let Some(h) = handler {
        c = c.with_elicitation(h);
    }
    c.initialize().expect("connect");
    c
}

#[test]
fn a_declaring_call_follows_its_task_at_the_servers_interval() {
    let (ep, seen, state) = server(false);
    let c = connect(&ep, true, None);
    assert!(c.declares_tasks());
    let r = within(&c, "job").expect("the task's result");
    assert_eq!(r.text(), "done");
    let call = &requests(&seen, "tools/call")[0];
    assert!(declares(call), "tools/call declares the extension");
    // The connection's own capabilities are not declared on the rest.
    assert!(!declares(&requests(&seen, "server/discover")[0]));
    let gets = requests(&seen, "tasks/get");
    assert_eq!(gets.len(), 2, "polled until it completed");
    for g in &gets {
        assert_eq!(g.header("mcp-name"), Some("task-0"));
        assert!(declares(g));
    }
    let at = state.polled_at.lock().unwrap().clone();
    assert!(
        at[1].duration_since(at[0]) >= Duration::from_millis(POLL_MS),
        "polled faster than the server's pollIntervalMs"
    );
}

#[test]
fn a_connection_that_does_not_declare_gets_its_answer_inline() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, false, None);
    assert!(!c.declares_tasks());
    let r = within(&c, "job").unwrap();
    assert_eq!(r.text(), "inline");
    assert!(!declares(&requests(&seen, "tools/call")[0]));
    assert_eq!(count(&seen, "tasks/get"), 0);
}

/// Nothing could end the follow of a call with no bound, so it does not
/// declare the extension, on a connection that otherwise does.
#[test]
fn an_unbounded_call_never_declares() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, true, None);
    assert!(c.declares_tasks());
    let r = c.call_tool("job", Some(json!({}))).unwrap();
    assert_eq!(r.text(), "inline");
    let r = c
        .call_tool_with_meta("job", Some(json!({})), json!({}))
        .unwrap();
    assert_eq!(r.text(), "inline");
    assert!(requests(&seen, "tools/call").iter().all(|r| !declares(r)));
    assert_eq!(count(&seen, "tasks/get"), 0);
}

/// A `CreateTaskResult` is the base task only: a seed that already says
/// `completed` is fetched for its result, never refused for lacking one.
#[test]
fn a_seed_that_already_ended_is_fetched_for_its_outcome() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, true, None);
    let r = within(&c, "instant").expect("the result tasks/get carries");
    assert_eq!(r.text(), "at once");
    assert_eq!(count(&seen, "tasks/get"), 1, "fetched once");
    assert_eq!(count(&seen, "tasks/cancel"), 0);
}

#[test]
fn a_server_that_requires_the_extension_is_refused_by_name() {
    let (ep, _, _) = server(true);
    let c = connect(&ep, false, None);
    match c.call_tool("job", Some(json!({}))) {
        Err(McpError::Capability(m)) => {
            assert!(m.contains(TASKS) && m.contains("-32021"), "{m}")
        }
        other => panic!("expected the extension named, got {other:?}"),
    }
}

#[test]
fn a_task_on_any_method_but_tools_call_is_an_invalid_answer() {
    let (ep, _, _) = server(false);
    let c = connect(&ep, true, None);
    let got = c.get_prompt("p", None, None);
    assert!(got.is_err(), "a prompt answered with a task: {got:?}");
}

#[test]
fn the_calls_bound_cancels_a_task_still_open() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, true, None);
    let started = Instant::now();
    let e = c
        .call_tool_with_meta_within(
            "stuck",
            Some(json!({})),
            json!({}),
            Duration::from_millis(700),
        )
        .expect_err("still working at the bound");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(e.to_string().contains("tasks/cancel sent"), "{e}");
    let cancels = requests(&seen, "tasks/cancel");
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].header("mcp-name"), Some("task-0"));
    assert!(declares(&cancels[0]));
    assert_eq!(
        count(&seen, "notifications/cancelled"),
        0,
        "never notifications/cancelled"
    );
}

/// A person who takes their time, and when they were asked.
struct Person(AtomicU64, Mutex<Vec<(Instant, Instant)>>);

impl Handler for Person {
    fn handle(&self, req: Inbound) -> Option<Answer> {
        let Inbound::Elicit { message, .. } = req else {
            return None;
        };
        assert_eq!(message, "Your name?");
        self.0.fetch_add(1, Ordering::SeqCst);
        let asked = Instant::now();
        std::thread::sleep(Duration::from_millis(400));
        self.1.lock().unwrap().push((asked, Instant::now()));
        Some(Answer::Accept(json!({"name": "Luca"})))
    }
}

#[test]
fn an_input_request_is_asked_once_and_answered_with_tasks_update() {
    let (ep, seen, state) = server(false);
    let person = Arc::new(Person(AtomicU64::new(0), Mutex::default()));
    let c = connect(&ep, true, Some(Arc::clone(&person) as Arc<dyn Handler>));
    let r = within(&c, "ask").expect("greeted");
    assert_eq!(r.text(), "Hello, Luca!");
    assert_eq!(person.0.load(Ordering::SeqCst), 1, "asked once");
    // No poll goes out while the person decides.
    let (asked, answered) = person.1.lock().unwrap()[0];
    let polls = state.polled_at.lock().unwrap().clone();
    assert!(
        polls.iter().all(|p| *p < asked || *p > answered),
        "polled while the person was asked"
    );
    let updates = requests(&seen, "tasks/update");
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0].body["params"]["inputResponses"],
        json!({"name": {"action": "accept", "content": {"name": "Luca"}}})
    );
    assert_eq!(updates[0].header("mcp-name"), Some("task-0"));
}

#[test]
fn without_a_person_an_elicitation_is_declined() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, true, None);
    let _ = c.call_tool_with_meta_within("ask", Some(json!({})), json!({}), Duration::from_secs(1));
    let updates = requests(&seen, "tasks/update");
    assert_eq!(updates.len(), 1, "declined once, not on every poll");
    assert_eq!(
        updates[0].body["params"]["inputResponses"],
        json!({"name": {"action": "decline"}})
    );
}

#[test]
fn how_a_task_ends_is_how_the_call_ends() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, true, None);
    let e = within(&c, "fail").expect_err("failed");
    assert!(e.to_string().contains("API rate limit exceeded"), "{e}");
    let r = within(&c, "tool_error").expect("a tool error is a result");
    assert!(r.is_error());
    assert_eq!(r.text(), "no such city");
    let e = within(&c, "vanish").expect_err("forgotten");
    assert!(e.to_string().contains("unknown or expired"), "{e}");
    // A task the server does not know is not cancelled.
    assert_eq!(count(&seen, "tasks/cancel"), 0);
}

/// A task given up while the server may still run it — here a `tasks/get`
/// refused for good — is cancelled.
#[test]
fn a_task_given_up_is_cancelled() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, true, None);
    let e = within(&c, "refuse").expect_err("refused");
    assert!(e.to_string().contains("not for you"), "{e}");
    let cancels = requests(&seen, "tasks/cancel");
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].header("mcp-name"), Some("task-0"));
}

/// A `tasks/get` answered about another task is no answer about this one:
/// a failed poll, tried again after a pause, never taken as this task's.
#[test]
fn an_answer_about_another_task_is_a_failed_poll() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, true, None);
    let e = c
        .call_tool_with_meta_within(
            "other",
            Some(json!({})),
            json!({}),
            Duration::from_millis(2500),
        )
        .expect_err("never answered about this task");
    assert!(e.to_string().contains("tasks/cancel sent"), "{e}");
    let polls = count(&seen, "tasks/get");
    assert!(polls <= 4, "{polls} polls in 2.5 s: not backed off");
}

/// A follow whose caller stops waiting ends at once, the task handed back
/// open, rather than at its bound.
#[test]
fn a_follow_its_caller_stops_ends_at_once() {
    let (ep, _, _) = server(false);
    let c = connect(&ep, true, None);
    let task = task_of(&c, "stuck");
    let started = Instant::now();
    let stop_at = started + Duration::from_millis(300);
    let got = c.follow_task(
        Tracker::new(task, now_ms()),
        Some(started + Duration::from_secs(30)),
        None,
        &|| Instant::now() >= stop_at,
    );
    assert!(matches!(got, Ok(Followed::Open(_))), "{got:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "followed on for {:?} after the stop",
        started.elapsed()
    );
}

/// Answers the first question at once and is still deciding the second.
struct FirstOnly;

impl Handler for FirstOnly {
    fn handle(&self, req: Inbound) -> Option<Answer> {
        let Inbound::Elicit { message, .. } = req else {
            return None;
        };
        if message != "First?" {
            std::thread::sleep(Duration::from_secs(3));
        }
        Some(Answer::Accept(json!({"x": 1})))
    }
}

/// A task handed back open at its bound — for the supervisor to follow — has
/// every answer the person gave sent, or, when the server would not take it,
/// its key released, so the next follower deals with it: it cannot send
/// words it was never given.
#[test]
fn a_task_handed_back_open_has_its_answers_sent_or_released() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, true, Some(Arc::new(FirstOnly) as Arc<dyn Handler>));
    for (tool, took) in [("two", true), ("two_fail", false)] {
        let task = task_of(&c, tool);
        let until = Instant::now() + Duration::from_millis(800);
        let t = match c.follow_task(Tracker::new(task, now_ms()), Some(until), None, &|| false) {
            Ok(Followed::Open(t)) => t,
            other => panic!("{tool}: expected the task open at the bound, got {other:?}"),
        };
        assert!(t.unsent.is_empty(), "{tool}: {:?}", t.unsent);
        assert_eq!(t.answered.contains("a"), took, "{tool}: {:?}", t.answered);
        assert!(!t.answered.contains("b"), "{tool}: still being decided");
    }
    let updates = requests(&seen, "tasks/update");
    let of = |id: &str| -> Vec<Value> {
        updates
            .iter()
            .filter(|u| u.body["params"]["taskId"] == id)
            .map(|u| u.body["params"]["inputResponses"].clone())
            .collect()
    };
    let answer = json!({"a": {"action": "accept", "content": {"x": 1}}});
    assert_eq!(
        of("task-0"),
        std::slice::from_ref(&answer),
        "sent once, before the bound"
    );
    assert_eq!(
        of("task-1"),
        [answer.clone(), answer],
        "tried again before it was handed back"
    );
}

#[test]
fn a_listen_asks_for_the_tasks_and_their_pushes_reach_the_queue() {
    let (ep, seen, _) = server(false);
    let c = connect(&ep, true, None);
    let ids = ["task-a".to_string()].into_iter().collect();
    let acked = c
        .listen_tasks(ids, Duration::from_secs(5))
        .expect("listened");
    assert!(acked.contains("task-a"), "acknowledged: {acked:?}");
    let listens = requests(&seen, "subscriptions/listen");
    let asked = listens
        .iter()
        .find(|l| l.body["params"]["notifications"]["taskIds"].is_array())
        .expect("a listen that asked for the task");
    assert_eq!(
        asked.body["params"]["notifications"]["taskIds"],
        json!(["task-a"])
    );
    assert!(
        declares(asked),
        "a listen for task notifications declares the extension"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let got = c.drain_notifications();
        if let Some(n) = got.iter().find(|n| n.method == "notifications/tasks") {
            assert_eq!(n.params.as_ref().unwrap()["status"], "completed");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no notifications/tasks reached the queue"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_connection_reached_through_initialize_declares_nothing() {
    let (ep, seen) = legacy_server(LegacyOpts::default());
    let c = connect(&ep, true, None);
    assert!(!c.declares_tasks());
    c.call_tool("echo", Some(json!({}))).unwrap();
    assert_eq!(count(&seen, "tasks/get"), 0);
    assert!(matches!(
        c.get_task("t", None, Duration::from_secs(1)),
        Err(McpError::Capability(_))
    ));
}
