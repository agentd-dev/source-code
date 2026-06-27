//! A2A v1.0 unary method surface, served over the existing self-MCP listener.
//! RFC 0020. [feature: a2a]
//!
//! A2A (Agent2Agent, Linux-Foundation) is the standardized agent↔agent wire;
//! agentd is already task-shaped (a served run *is* a Task), so this module is a
//! thin **binding**, not a new engine: it re-frames A2A JSON-RPC `a2a.<Method>`
//! calls onto the same served-spawn machinery [`crate::mcp::server`] already runs
//! for `subagent.spawn` (the async path) and reads them back from the same
//! `sessions` registry. The heavy A2A network machinery — HTTP, SSE, OAuth,
//! webhooks, durable `ListTasks` history — lives in the on-node gateway (RFC 0020
//! §2/§7); agentd carries none of it.
//!
//! **Trust:** the A2A surface is the *external agent* surface and is served only
//! to a [`PeerOrigin::Management`](crate::mcp::server::PeerOrigin) peer (the
//! trusted vsock/unix management transport, RFC 0015 §3.3) — the gateway is the
//! PEP that already authenticated the client. A `Stdio` peer (a spawned subagent)
//! gets `-32601`.
//!
//! **Wire convention:** methods are `a2a.SendMessage` / `a2a.GetTask` /
//! `a2a.CancelTask` / `a2a.ListTasks` (the `a2a.<Method>` dotted convention over
//! the NDJSON JSON-RPC codec, RFC 0004). The agentctl gateway MUST emit exactly
//! these method names when bridging HTTP↔vsock.

use crate::json::{self, Request, Response};
use crate::mcp::server::ServeCtx;
use crate::obs::log::Logger;
use serde_json::{Value, json};

// ── A2A-specific JSON-RPC error codes (RFC 0020 / A2A spec) ──────────────────

/// A2A `TaskNotFound`: a `GetTask`/`CancelTask` for an `id` not in the registry.
pub const TASK_NOT_FOUND: i64 = -32001;
/// This build does not stream: `SendStreamingMessage`/`SubscribeToTask` (A2A-2).
pub const STREAMING_NOT_SUPPORTED: i64 = -32004;

// ── A2A object schemas (proto-derived; RFC 0020 §5) ──────────────────────────

/// A2A `TaskState` — the exact enum strings from `a2a.proto`. agentd's
/// [`TerminalStatus`](crate::agentloop::stop::TerminalStatus) maps onto these
/// (RFC 0020 §5); a still-running served run is `WORKING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
    Rejected,
    InputRequired,
    AuthRequired,
    Unspecified,
}

impl TaskState {
    /// The wire string (verbatim from the proto enum).
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Submitted => "TASK_STATE_SUBMITTED",
            TaskState::Working => "TASK_STATE_WORKING",
            TaskState::Completed => "TASK_STATE_COMPLETED",
            TaskState::Failed => "TASK_STATE_FAILED",
            TaskState::Canceled => "TASK_STATE_CANCELED",
            TaskState::Rejected => "TASK_STATE_REJECTED",
            TaskState::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            TaskState::AuthRequired => "TASK_STATE_AUTH_REQUIRED",
            TaskState::Unspecified => "TASK_STATE_UNSPECIFIED",
        }
    }
}

/// Map an agentd `TerminalStatus` string (RFC 0007 §3.4, the `as_str` vocabulary)
/// to the A2A `TaskState` (RFC 0020 §5). This is the single mapping authority:
/// `completed→COMPLETED`, `refused→REJECTED`, `cancelled→CANCELED`, every budget/
/// stall/crash terminal `→FAILED`. An unrecognized status is `FAILED` (a terminal
/// run with no clean conclusion is a failure, never silently "working").
pub fn terminal_to_state(status: &str) -> TaskState {
    match status {
        "completed" => TaskState::Completed,
        "refused" => TaskState::Rejected,
        "cancelled" => TaskState::Canceled,
        "exhausted_steps" | "exhausted_tokens" | "deadline" | "stalled" | "loop_detected"
        | "crashed" => TaskState::Failed,
        _ => TaskState::Failed,
    }
}

/// Map a served-run status string from the registry to an A2A `TaskState`. The
/// synthetic `"running"` (a still-live run) is `WORKING`; every terminal status
/// goes through [`terminal_to_state`] (RFC 0020 §5).
fn state_from_status(status: &str) -> TaskState {
    if status == "running" {
        TaskState::Working
    } else {
        terminal_to_state(status)
    }
}

/// Build an A2A `TaskStatus` object: `{ state, timestamp }`. The timestamp is
/// ISO-8601 (RFC 3339), minted now (agentd is stateless — it does not retain the
/// per-transition time; the gateway owns durable history).
fn task_status(state: TaskState) -> Value {
    json!({
        "state": state.as_str(),
        "timestamp": crate::obs::log::rfc3339_millis(std::time::SystemTime::now()),
    })
}

/// Build an A2A `Task` object (RFC 0020 §5 schema). `artifact` is the final
/// distillate (terminal-completed only — the distillate-only invariant, RFC 0009
/// §8: no partial-artifact leakage), `None` otherwise.
fn task_object(id: &str, context_id: &str, state: TaskState, artifact: Option<&Value>) -> Value {
    let mut task = json!({
        "id": id,
        "contextId": context_id,
        "status": task_status(state),
    });
    if let Some(value) = artifact {
        // A2A Artifact = { artifactId?, parts:[Part], name?, metadata? }; the
        // distillate is one text Part. The value is rendered to a string Part.
        let part_text = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        task["artifacts"] = json!([{
            "artifactId": format!("{id}.distillate"),
            "parts": [{ "text": part_text }],
        }]);
    }
    task
}

/// The `contextId` minted for a served run: the A2A conversation grouping. agentd
/// has no durable context store, so it is derived deterministically from the run
/// handle (`ctx-<handle>`) — stable for the life of the task, distinct per task.
fn context_id_for(handle: &str) -> String {
    format!("ctx-{handle}")
}

/// Concatenate the TEXT parts of an A2A `Message` into the run instruction
/// (RFC 0020 §5 — text parts become the instruction). Non-text parts (data / raw
/// / url) are ignored for the instruction; the gateway handles richer parts. Parts
/// are joined with newlines, in order.
fn instruction_from_message(message: &Value) -> String {
    let mut out = String::new();
    if let Some(parts) = message.get("parts").and_then(Value::as_array) {
        for p in parts {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
    }
    out
}

/// Project a served-run snapshot `(status, result)` to an A2A `Task` value, with
/// the distillate attached only on a terminal-completed run.
fn snapshot_to_task(handle: &str, state: TaskState, result: Option<&Value>) -> Value {
    let context_id = context_id_for(handle);
    let artifact = if state == TaskState::Completed {
        result
    } else {
        None
    };
    task_object(handle, &context_id, state, artifact)
}

/// Route one `a2a.*` JSON-RPC request to its handler. Called from
/// [`crate::mcp::server`]'s `dispatch` only for a `Management`-origin peer (the
/// gating is enforced at the call site — RFC 0020 §integration); a `Stdio` peer
/// never reaches here and falls through to `-32601`.
pub fn dispatch_a2a(method: &str, req: Request, ctx: &ServeCtx, log: &Logger) -> Response {
    match method {
        "a2a.SendMessage" => send_message(req, ctx, log),
        "a2a.GetTask" => get_task(req, ctx),
        "a2a.CancelTask" => cancel_task(req, ctx),
        "a2a.ListTasks" => list_tasks(req, ctx),
        // Streaming is added by A2A-2; this build refuses it explicitly so a
        // client distinguishes "not in this build" from "unknown method".
        "a2a.SendStreamingMessage" | "a2a.SubscribeToTask" => Response::err(
            req.id,
            STREAMING_NOT_SUPPORTED,
            "streaming not supported in this build",
        ),
        // push-notification-config + GetExtendedAgentCard are gateway-owned
        // (RFC 0020 §7): agentd does not serve them → method-not-found.
        _ => Response::err(
            req.id,
            json::METHOD_NOT_FOUND,
            format!("unsupported a2a method: {method}"),
        ),
    }
}

/// `a2a.SendMessage` (params: `SendMessageRequest`) → a **Task**. The concatenated
/// text parts become the run instruction; the run is spawned via the SAME
/// served-spawn async machinery `subagent.spawn{async}` uses, returning
/// immediately with a `TASK_STATE_WORKING` Task whose `id` is the served handle
/// (`served.N`). `configuration.returnImmediately` defaults to true; `false` blocks
/// on the sync served-spawn and returns a terminal Task.
fn send_message(req: Request, ctx: &ServeCtx, log: &Logger) -> Response {
    let id = req.id.clone();
    let params = req.params.clone().unwrap_or(json!({}));
    let message = params.get("message").cloned().unwrap_or(json!({}));
    let instruction = instruction_from_message(&message);
    if instruction.trim().is_empty() {
        return Response::err(
            id,
            json::INVALID_PARAMS,
            "a2a.SendMessage requires a message with at least one non-empty text part",
        );
    }
    // returnImmediately defaults to true (async Task); false → block on the sync
    // served-spawn and return the terminal Task.
    let return_immediately = params
        .get("configuration")
        .and_then(|c| c.get("returnImmediately"))
        .and_then(Value::as_bool)
        .unwrap_or(true);

    if return_immediately {
        match ctx.a2a_spawn_async(&instruction, log) {
            // A just-dispatched async run is WORKING (it is already executing on
            // its background thread); SUBMITTED is the pre-dispatch state.
            Ok(handle) => Response::ok(id, snapshot_to_task(&handle, TaskState::Working, None)),
            Err(msg) => Response::err(id, json::INTERNAL_ERROR, msg),
        }
    } else {
        // Blocking: map the sync served-spawn's terminal outcome to a Task.
        let (handle, status, result) = ctx.a2a_spawn_sync(&instruction, log);
        let state = terminal_to_state(&status);
        Response::ok(id, snapshot_to_task(&handle, state, result.as_ref()))
    }
}

/// `a2a.GetTask` (params: `{id, historyLength?}`) → a **Task**. Reads the served
/// run by `id` from the registry and projects it: a still-running run is
/// `TASK_STATE_WORKING`; a terminal run maps via [`terminal_to_state`], and a
/// terminal *completed* run carries the distillate as the single artifact (the
/// distillate-only invariant, RFC 0009 §8). An unknown `id` → `-32001`.
fn get_task(req: Request, ctx: &ServeCtx) -> Response {
    let id = req.id.clone();
    let task_id = task_id_param(&req);
    match ctx.a2a_task_snapshot(&task_id) {
        Some((status, result)) => {
            let state = state_from_status(&status);
            Response::ok(id, snapshot_to_task(&task_id, state, result.as_ref()))
        }
        None => Response::err(id, TASK_NOT_FOUND, "task not found"),
    }
}

/// `a2a.CancelTask` (params: `{id}`) → a **Task** in `TASK_STATE_CANCELED`. Wraps
/// the existing served cancel (the `subagent.cancel` path): requests cancellation
/// of the still-running run by handle (it then drains its subtree via the kill
/// ladder). An unknown `id` → `-32001`. An already-terminal run is returned in its
/// real terminal state (A2A cancel of a finished task is a read).
fn cancel_task(req: Request, ctx: &ServeCtx) -> Response {
    let id = req.id.clone();
    let task_id = task_id_param(&req);
    match ctx.a2a_cancel(&task_id) {
        // Cancel requested (the run was live): report CANCELED.
        Some(true) => Response::ok(id, snapshot_to_task(&task_id, TaskState::Canceled, None)),
        // Already-terminal → return its real state (cancel-of-finished is a read).
        Some(false) => match ctx.a2a_task_snapshot(&task_id) {
            Some((status, result)) => {
                let state = state_from_status(&status);
                Response::ok(id, snapshot_to_task(&task_id, state, result.as_ref()))
            }
            None => Response::err(id, TASK_NOT_FOUND, "task not found"),
        },
        None => Response::err(id, TASK_NOT_FOUND, "task not found"),
    }
}

/// `a2a.ListTasks` (params: `{pageSize?, pageToken?, filter?}`) →
/// `ListTasksResponse {tasks:[Task]}`. Lists the *live* served-run registry — the
/// ephemeral instance-local view (durable cross-pod `ListTasks` history is
/// gateway-held, RFC 0020 §5/§7). Pagination params are accepted but this
/// ephemeral registry is small + bounded (`MAX_SESSIONS`), so the whole set is
/// returned and `nextPageToken` is omitted.
fn list_tasks(req: Request, ctx: &ServeCtx) -> Response {
    let id = req.id.clone();
    let tasks: Vec<Value> = ctx
        .a2a_list()
        .into_iter()
        .map(|(handle, status, result)| {
            let state = state_from_status(&status);
            snapshot_to_task(&handle, state, result.as_ref())
        })
        .collect();
    Response::ok(id, json!({ "tasks": tasks }))
}

/// Extract the `id` param (the task handle) from a `GetTask`/`CancelTask` request.
fn task_id_param(req: &Request) -> String {
    req.params
        .as_ref()
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentloop::stop::TerminalStatus;

    #[test]
    fn instruction_concats_text_parts_in_order() {
        let msg = json!({
            "messageId": "m1",
            "role": "ROLE_USER",
            "parts": [
                { "text": "first" },
                { "data": { "k": "v" } },   // non-text part is ignored
                { "text": "second" }
            ]
        });
        assert_eq!(instruction_from_message(&msg), "first\nsecond");
    }

    #[test]
    fn terminal_status_maps_every_arm() {
        // The full RFC 0007 §3.4 vocabulary → A2A TaskState (RFC 0020 §5).
        assert_eq!(
            terminal_to_state(TerminalStatus::Completed.as_str()),
            TaskState::Completed
        );
        assert_eq!(
            terminal_to_state(TerminalStatus::Refused.as_str()),
            TaskState::Rejected
        );
        assert_eq!(
            terminal_to_state(TerminalStatus::Cancelled.as_str()),
            TaskState::Canceled
        );
        for s in [
            TerminalStatus::ExhaustedSteps,
            TerminalStatus::ExhaustedTokens,
            TerminalStatus::Deadline,
            TerminalStatus::Stalled,
            TerminalStatus::LoopDetected,
            TerminalStatus::Crashed,
        ] {
            assert_eq!(
                terminal_to_state(s.as_str()),
                TaskState::Failed,
                "{} → FAILED",
                s.as_str()
            );
        }
        // An unrecognized status is FAILED, never silently working.
        assert_eq!(terminal_to_state("nonsense"), TaskState::Failed);
    }

    #[test]
    fn task_state_wire_strings_are_the_proto_enum() {
        assert_eq!(TaskState::Submitted.as_str(), "TASK_STATE_SUBMITTED");
        assert_eq!(TaskState::Working.as_str(), "TASK_STATE_WORKING");
        assert_eq!(TaskState::Completed.as_str(), "TASK_STATE_COMPLETED");
        assert_eq!(TaskState::Failed.as_str(), "TASK_STATE_FAILED");
        assert_eq!(TaskState::Canceled.as_str(), "TASK_STATE_CANCELED");
        assert_eq!(TaskState::Rejected.as_str(), "TASK_STATE_REJECTED");
    }

    #[test]
    fn task_object_carries_distillate_only_when_completed() {
        let done = snapshot_to_task("served.0", TaskState::Completed, Some(&json!("the answer")));
        assert_eq!(done["id"], "served.0");
        assert_eq!(done["contextId"], "ctx-served.0");
        assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(done["artifacts"][0]["parts"][0]["text"], "the answer");
        assert_eq!(done["artifacts"][0]["artifactId"], "served.0.distillate");

        // A failed/working task carries NO artifact (distillate-only invariant).
        let failed = snapshot_to_task("served.1", TaskState::Failed, Some(&json!("leak?")));
        assert!(
            failed.get("artifacts").is_none(),
            "no partial-artifact leakage on a failed task"
        );
        let working = snapshot_to_task("served.2", TaskState::Working, None);
        assert!(working.get("artifacts").is_none());
        assert_eq!(working["status"]["state"], "TASK_STATE_WORKING");
    }
}
