// SPDX-License-Identifier: AGPL-3.0-only
//! The mock's **MCP Tasks** surface (SEP-2663), served on its 2026-07-28
//! edge: `mock.job` answers with a task when the call declares the
//! extension (inline when it does not, or `-32021` with `require: true`), and
//! `tasks/get`, `tasks/update` and `tasks/cancel` follow it.
//!
//! A task runs on the clock, not on its polls, so a client that polls less
//! often — or not at all while it is down — finds it where time put it.
//! `mock.job`'s `script` says what the task does: `done` completes `after_ms`
//! after its creation (default 600) with `{"done": true}`; `stuck` works for
//! ever; `ask` asks for a name (an `elicitation/create` form — whether or
//! not the call declared elicitation, so a test sees a client decline) and
//! greets it once answered; `fail` fails with a JSON-RPC error; `tool_error` completes with
//! `isError: true`; `vanish` is forgotten after its first poll; `expire`
//! works until its `ttl_ms` (default 60000) and is then purged. `poll_ms` is
//! the `pollIntervalMs` it asks for (default 200). `seed` is the status the
//! `CreateTaskResult` says (default `working`; it carries no result, as a
//! seed never does). `update_error` and `get_error` answer every
//! `tasks/update`, or every `tasks/get`, with that JSON-RPC error code.
//!
//! Each request leaves a line: `MOCK_TASK_CREATED <id> <script>`,
//! `MOCK_TASK_GET <id> <unix ms>`, `MOCK_TASK_UPDATE <id> <inputResponses>`,
//! `MOCK_TASK_CANCEL <id>`.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The extension's identifier.
pub const EXTENSION: &str = "io.modelcontextprotocol/tasks";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Script {
    Done,
    Stuck,
    Ask,
    Fail,
    ToolError,
    Vanish,
    Expire,
}

impl Script {
    fn parse(s: &str) -> Script {
        match s {
            "stuck" => Script::Stuck,
            "ask" => Script::Ask,
            "fail" => Script::Fail,
            "tool_error" => Script::ToolError,
            "vanish" => Script::Vanish,
            "expire" => Script::Expire,
            _ => Script::Done,
        }
    }
}

struct MockTask {
    script: Script,
    created: Instant,
    after: Duration,
    poll_ms: u64,
    ttl_ms: Option<u64>,
    /// `ask`'s answer, once given; `Some(None)` once declined.
    answer: Option<Option<String>>,
    cancelled: bool,
    polls: u32,
    update_error: Option<i64>,
    get_error: Option<i64>,
}

/// Every task the mock holds.
#[derive(Default)]
pub struct Tasks {
    next: AtomicU64,
    open: Mutex<BTreeMap<String, MockTask>>,
}

/// Does a request's `params` declare the extension?
pub fn declares(params: &Value) -> bool {
    params["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
        .get(EXTENSION)
        .is_some()
}

/// The error a server sends a request that did not declare the extension it
/// needs.
pub fn missing_capability() -> Value {
    json!({"code": -32021, "message": "Missing required client capability",
           "data": {"requiredCapabilities": {"extensions": {EXTENSION: {}}}}})
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn shape(id: &str, t: &MockTask, status: &str, extra: Value) -> Value {
    let mut v = json!({"taskId": id, "status": status, "createdAt": "2026-10-19T10:00:00Z",
                       "lastUpdatedAt": "2026-10-19T10:00:00Z", "ttlMs": t.ttl_ms,
                       "pollIntervalMs": t.poll_ms});
    if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
        o.extend(e);
    }
    v
}

/// `mock.job`. `Ok(Some(_))` is a `CreateTaskResult`; `Ok(None)` asks the
/// caller to answer inline, as a call that did not declare the extension is.
pub fn job(state: &Tasks, params: &Value) -> Result<Option<Value>, Value> {
    let args = &params["arguments"];
    if !declares(params) {
        return if args["require"] == true {
            Err(missing_capability())
        } else {
            Ok(None)
        };
    }
    let script = Script::parse(args["script"].as_str().unwrap_or("done"));
    let id = format!("mock-task-{}", state.next.fetch_add(1, Ordering::SeqCst));
    let t = MockTask {
        script,
        created: Instant::now(),
        after: Duration::from_millis(args["after_ms"].as_u64().unwrap_or(600)),
        poll_ms: args["poll_ms"].as_u64().unwrap_or(200),
        ttl_ms: match args.get("ttl_ms") {
            Some(Value::Null) => None,
            Some(v) => v.as_u64().or(Some(60_000)),
            None => Some(60_000),
        },
        answer: None,
        cancelled: false,
        polls: 0,
        update_error: args["update_error"].as_i64(),
        get_error: args["get_error"].as_i64(),
    };
    eprintln!(
        "MOCK_TASK_CREATED {id} {}",
        args["script"].as_str().unwrap_or("done")
    );
    let seed = args["seed"].as_str().unwrap_or("working");
    let mut created = shape(&id, &t, seed, json!({"statusMessage": "started"}));
    created["resultType"] = json!("task");
    state
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, t);
    Ok(Some(created))
}

fn text_result(text: &str, is_error: bool, structured: Option<Value>) -> Value {
    let mut r = json!({"content": [{"type": "text", "text": text}], "isError": is_error});
    if let Some(s) = structured {
        r["structuredContent"] = s;
    }
    r
}

/// Where task `id` is now, as `tasks/get` and `notifications/tasks` show it.
fn view(id: &str, t: &MockTask) -> Value {
    if t.cancelled {
        return shape(
            id,
            t,
            "cancelled",
            json!({"statusMessage": "cancelled by the client"}),
        );
    }
    match t.script {
        Script::Done if t.created.elapsed() >= t.after => shape(
            id,
            t,
            "completed",
            json!({"result": text_result("{\"done\": true}", false, Some(json!({"done": true})))}),
        ),
        Script::Ask => match &t.answer {
            Some(Some(name)) => shape(
                id,
                t,
                "completed",
                json!({"result": text_result(&format!("Hello, {name}!"), false, None)}),
            ),
            Some(None) => shape(
                id,
                t,
                "completed",
                json!({"result": text_result("no name was given", true, None)}),
            ),
            None => shape(
                id,
                t,
                "input_required",
                json!({"statusMessage": "waiting for a name", "inputRequests": {"name": {
                    "method": "elicitation/create",
                    "params": {"mode": "form", "message": "Your name?",
                               "requestedSchema": {"type": "object",
                                   "properties": {"name": {"type": "string"}},
                                   "required": ["name"]}}}}}),
            ),
        },
        Script::Fail => shape(
            id,
            t,
            "failed",
            json!({"statusMessage": "the job ran out of quota",
                   "error": {"code": -32603, "message": "the job ran out of quota"}}),
        ),
        Script::ToolError => shape(
            id,
            t,
            "completed",
            json!({"result": text_result("the job failed: no such city", true, None)}),
        ),
        _ => shape(id, t, "working", json!({"statusMessage": "working"})),
    }
}

/// The ids among `ids` the mock holds: what a listen that asked for them is
/// acknowledged with.
pub fn known(state: &Tasks, ids: &[Value]) -> Vec<Value> {
    let open = state.open.lock().unwrap_or_else(|e| e.into_inner());
    ids.iter()
        .filter(|i| i.as_str().is_some_and(|i| open.contains_key(i)))
        .cloned()
        .collect()
}

/// Task `id` as a `notifications/tasks` carries it, once it has ended;
/// `None` while it runs or once the mock no longer holds it.
pub fn ended(state: &Tasks, id: &str) -> Option<Value> {
    let open = state.open.lock().unwrap_or_else(|e| e.into_inner());
    let v = view(id, open.get(id)?);
    matches!(
        v["status"].as_str(),
        Some("completed" | "failed" | "cancelled")
    )
    .then_some(v)
}

/// One `tasks/*` request: its result, or the JSON-RPC error to answer with.
pub fn handle(state: &Tasks, method: &str, params: &Value) -> Result<Value, Value> {
    let id = params["taskId"].as_str().unwrap_or_default().to_string();
    let not_found =
        || json!({"code": -32602, "message": "Failed to retrieve task: Task not found"});
    let mut open = state.open.lock().unwrap_or_else(|e| e.into_inner());
    match method {
        "tasks/get" => {
            eprintln!("MOCK_TASK_GET {id} {}", unix_ms());
            let Some(t) = open.get_mut(&id) else {
                return Err(not_found());
            };
            t.polls += 1;
            if let Some(code) = t.get_error {
                return Err(json!({"code": code, "message": "the mock refuses tasks/get"}));
            }
            let elapsed = t.created.elapsed();
            if t.script == Script::Expire
                && t.ttl_ms.is_some_and(|ttl| elapsed.as_millis() as u64 > ttl)
            {
                open.remove(&id);
                return Err(
                    json!({"code": -32602, "message": "Failed to retrieve task: Task has expired"}),
                );
            }
            if t.script == Script::Vanish && t.polls > 1 {
                open.remove(&id);
                return Err(not_found());
            }
            let mut answer = view(&id, t);
            answer["resultType"] = json!("complete");
            Ok(answer)
        }
        "tasks/update" => {
            eprintln!("MOCK_TASK_UPDATE {id} {}", params["inputResponses"]);
            let Some(t) = open.get_mut(&id) else {
                return Err(not_found());
            };
            if let Some(code) = t.update_error {
                return Err(json!({"code": code, "message": "the mock refuses tasks/update"}));
            }
            let r = &params["inputResponses"]["name"];
            if t.answer.is_none() && !r.is_null() {
                t.answer = Some(r["content"]["name"].as_str().map(str::to_string));
            }
            Ok(json!({"resultType": "complete"}))
        }
        "tasks/cancel" => {
            eprintln!("MOCK_TASK_CANCEL {id}");
            let Some(t) = open.get_mut(&id) else {
                return Err(not_found());
            };
            t.cancelled = true;
            Ok(json!({"resultType": "complete"}))
        }
        _ => Err(json!({"code": -32601, "message": "Method not found"})),
    }
}
