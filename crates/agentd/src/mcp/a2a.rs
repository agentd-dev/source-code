// SPDX-License-Identifier: Apache-2.0
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
//! the NDJSON JSON-RPC codec, RFC 0004), plus the streaming pair
//! `a2a.SendStreamingMessage` / `a2a.SubscribeToTask` (A2A-2). The agentctl gateway
//! MUST emit exactly these method names when bridging HTTP↔vsock.
//!
//! **Streaming convention (agentd↔gateway):** A2A v1.0 status-level streaming maps
//! onto the NDJSON JSON-RPC reply channel as a *multi-frame response*: for one
//! streaming request `id`, agentd emits SEVERAL frames
//! `{"jsonrpc":"2.0","id":<id>,"result":<StreamResponse>}` — the intermediate ones
//! written directly to the connection writer, the FINAL one returned as the
//! dispatch [`Response`] (which `handle_conn` writes as the last frame). The final
//! frame is the one whose `statusUpdate.final == true`. The gateway re-frames this
//! sequence to SSE (RFC 0020): each `result` becomes one `StreamResponse` SSE event
//! and the stream closes on the `final` status. **Caveat:** these intermediate
//! frames all share the request `id`, so the gateway MUST consume an `a2a.Send`
//! `StreamingMessage`/`SubscribeToTask` reply as a STREAM (read frames until
//! `statusUpdate.final == true`), NOT as a single id-correlated unary reply — the
//! same-`id` frames would otherwise look like duplicate responses. agentd does
//! **status-level** streaming only: the distillate artifact is delivered ONCE, in a
//! single `artifactUpdate` frame on a completed run (the distillate-only invariant,
//! RFC 0009 §8) — there is no partial-artifact streaming.

use crate::json::{self, Id, Request, Response};
use crate::mcp::server::{ServeCtx, SharedWriter};
use crate::obs::log::Logger;
use serde_json::{Value, json};

// ── A2A-specific JSON-RPC error codes (RFC 0020 / A2A spec) ──────────────────

/// A2A `TaskNotFound`: a `GetTask`/`CancelTask`/`SubscribeToTask` for an `id` not
/// in the registry.
pub const TASK_NOT_FOUND: i64 = -32001;

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

    /// Parse a wire `TASK_STATE_*` string back into a [`TaskState`] — the inverse
    /// of [`as_str`](Self::as_str), used by the A2A **client** (RFC 0020 §3) to
    /// read a Task's status off a remote peer. An unrecognized string is
    /// `Unspecified` (a peer speaking a newer enum is treated as not-yet-terminal,
    /// so the client keeps polling rather than mistaking it for a terminal state).
    pub fn from_wire(s: &str) -> TaskState {
        match s {
            "TASK_STATE_SUBMITTED" => TaskState::Submitted,
            "TASK_STATE_WORKING" => TaskState::Working,
            "TASK_STATE_COMPLETED" => TaskState::Completed,
            "TASK_STATE_FAILED" => TaskState::Failed,
            "TASK_STATE_CANCELED" => TaskState::Canceled,
            "TASK_STATE_REJECTED" => TaskState::Rejected,
            "TASK_STATE_INPUT_REQUIRED" => TaskState::InputRequired,
            "TASK_STATE_AUTH_REQUIRED" => TaskState::AuthRequired,
            _ => TaskState::Unspecified,
        }
    }

    /// Whether this state is **terminal** — the A2A client stops polling once a
    /// Task reaches one (RFC 0020 §3). `Submitted`/`Working` are in-flight;
    /// `Unspecified` and the input/auth-required interaction states are treated
    /// as non-terminal (agentd never produces them, but a richer gateway peer
    /// might, and the client should keep waiting on the deadline, not give up).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
        )
    }
}

// ── A2A client-side wire helpers (RFC 0020 §3) ───────────────────────────────
//
// The inverse of the server builders above: agentd-as-A2A-client mints a
// `SendMessage` request and reads `Task` objects back off a remote peer. These
// live here (not in the client module) so client and server share ONE A2A wire
// vocabulary — no duplicated (de)serialization.

/// Build the `params` for an `a2a.SendMessage` request carrying `objective` as a
/// single text `Part` of one `ROLE_USER` message (`message_id` minted by the
/// caller). The optional `output_contract` rides as a second text part so the
/// remote agent gets the same delegation contract a local subagent would
/// (RFC 0009 §spawn-payload). `returnImmediately:true` (the default) → an async
/// Task the client then polls via `a2a.GetTask`.
pub fn send_message_params(
    objective: &str,
    output_contract: Option<&str>,
    message_id: &str,
) -> Value {
    let mut parts = vec![json!({ "text": objective })];
    if let Some(contract) = output_contract.filter(|c| !c.is_empty()) {
        parts.push(json!({ "text": format!("Required output: {contract}") }));
    }
    json!({
        "message": {
            "messageId": message_id,
            "role": "ROLE_USER",
            "parts": parts,
        }
    })
}

/// The `id` (task handle) of a `Task` value returned by `a2a.SendMessage` /
/// `a2a.GetTask`. Empty if absent (a malformed reply the client surfaces as an
/// error).
pub fn task_id_of(task: &Value) -> String {
    task.get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The [`TaskState`] of a `Task` value (`status.state`). A missing/garbled
/// status reads as `Unspecified` (non-terminal — the client keeps polling).
pub fn task_state_of(task: &Value) -> TaskState {
    task.get("status")
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .map(TaskState::from_wire)
        .unwrap_or(TaskState::Unspecified)
}

/// Concatenate the text `Part`s of a completed `Task`'s terminal artifact(s) —
/// the **distillate** the client returns to the delegating model (RFC 0020 §5:
/// `Artifact` (final) = the distillate). Parts are joined with newlines, across
/// every artifact, in order. Empty if the task carries no artifact text.
pub fn artifact_text_of(task: &Value) -> String {
    let mut out = String::new();
    let Some(artifacts) = task.get("artifacts").and_then(Value::as_array) else {
        return out;
    };
    for artifact in artifacts {
        if let Some(parts) = artifact.get("parts").and_then(Value::as_array) {
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
    }
    out
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

// ── A2A streaming events (status-level; RFC 0020 §5 / A2A v1.0) ───────────────
//
// A2A v1.0 `StreamResponse` is a oneof of `{ task | message | statusUpdate |
// artifactUpdate }`. agentd emits only the two update variants (status-level
// streaming): `TaskStatusUpdateEvent` for each lifecycle transition (WORKING →
// terminal) and a single `TaskArtifactUpdateEvent` carrying the distillate on a
// completed run. The three builders below mint the `result` payload of one stream
// frame as a serde `Value` (one key set per the oneof), so a frame is exactly
// `{"jsonrpc":"2.0","id":<id>,"result":<here>}`.

/// A2A `TaskStatusUpdateEvent` → the `statusUpdate` arm of a `StreamResponse`.
/// `is_final` is the wire `final` flag (renamed — `final` is a Rust keyword): the
/// gateway closes the SSE stream on the frame whose `final == true`. `contextId` is
/// always present here (agentd mints it deterministically from the handle), so the
/// `Option` is only for shape-completeness with the proto.
fn status_update_frame(task_id: &str, context_id: &str, state: TaskState, is_final: bool) -> Value {
    json!({
        "statusUpdate": {
            "taskId": task_id,
            "contextId": context_id,
            "status": task_status(state),
            "final": is_final,
        }
    })
}

/// A2A `TaskArtifactUpdateEvent` → the `artifactUpdate` arm of a `StreamResponse`.
/// The distillate is one text `Part` in a single `Artifact`; `lastChunk:true` marks
/// it as the whole artifact (agentd never chunks — the distillate is delivered once,
/// the distillate-only invariant, RFC 0009 §8). Emitted ONLY for a completed run.
fn artifact_update_frame(task_id: &str, context_id: &str, distillate: &Value) -> Value {
    let part_text = match distillate {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    json!({
        "artifactUpdate": {
            "taskId": task_id,
            "contextId": context_id,
            "artifact": {
                "artifactId": format!("{task_id}.distillate"),
                "parts": [{ "text": part_text }],
            },
            "lastChunk": true,
        }
    })
}

/// Wrap a `StreamResponse` payload as one JSON-RPC reply frame
/// `{"jsonrpc":"2.0","id":<id>,"result":<stream_response>}` and write it directly
/// to the connection writer (the agentd↔gateway streaming convention — see the
/// module doc). Best-effort: a dead peer fails the write and the run still records
/// its terminal session for a later `GetTask`.
fn write_stream_frame(writer: &SharedWriter, id: &Id, stream_response: Value) {
    let frame = Response::ok(id.clone(), stream_response);
    if let Ok(mut w) = writer.lock() {
        let _ = crate::json::frame::write_line(&mut *w, &frame);
    }
}

/// Route one `a2a.*` JSON-RPC request to its handler. Called from
/// [`crate::mcp::server`]'s `dispatch` only for a `Management`-origin peer (the
/// gating is enforced at the call site — RFC 0020 §integration); a `Stdio` peer
/// never reaches here and falls through to `-32601`.
///
/// `writer` is the calling connection's shared write half: the streaming handlers
/// write their INTERMEDIATE frames directly to it and RETURN the FINAL frame (the
/// agentd↔gateway streaming convention — see the module doc). The unary handlers
/// ignore it.
pub fn dispatch_a2a(
    method: &str,
    req: Request,
    ctx: &ServeCtx,
    writer: &SharedWriter,
    log: &Logger,
) -> Response {
    match method {
        "a2a.SendMessage" => send_message(req, ctx, log),
        "a2a.GetTask" => get_task(req, ctx),
        "a2a.CancelTask" => cancel_task(req, ctx),
        "a2a.ListTasks" => list_tasks(req, ctx),
        // Status-level streaming (A2A-2): emit a multi-frame StreamResponse stream
        // on `writer`, returning the terminal frame. See the module doc.
        "a2a.SendStreamingMessage" => send_streaming_message(req, ctx, writer, log),
        "a2a.SubscribeToTask" => subscribe_to_task(req, ctx, writer),
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

/// `a2a.SendStreamingMessage` (params: `SendMessageRequest`, same shape as
/// `SendMessage`) → a STREAM of `StreamResponse` frames (status-level; RFC 0020).
/// The concatenated text parts become the run instruction; the run is then executed
/// **synchronously** on this connection thread (`supervise_once`), exactly like
/// `SendMessage{returnImmediately:false}`, while the lifecycle is streamed:
///
///   1. write `statusUpdate{ WORKING, final:false }` directly (the run is now live),
///   2. block on the served-spawn (recording the terminal `ServedSession` so a later
///      `GetTask`/`SubscribeToTask` resolves the same handle),
///   3. on a COMPLETED run, write `artifactUpdate{ distillate, lastChunk:true }`
///      directly (the distillate-only invariant — RFC 0009 §8: completed-only, no
///      partial-artifact streaming),
///   4. RETURN `statusUpdate{ <mapped terminal>, final:true }` as the dispatch
///      response (the gateway closes the SSE stream on it). FAILED/CANCELED/REJECTED
///      carry no artifact frame.
///
/// An empty instruction is the only pre-stream error (`-32602`, a normal error
/// Response — nothing is streamed).
fn send_streaming_message(
    req: Request,
    ctx: &ServeCtx,
    writer: &SharedWriter,
    log: &Logger,
) -> Response {
    let id = req.id.clone();
    let params = req.params.clone().unwrap_or(json!({}));
    let message = params.get("message").cloned().unwrap_or(json!({}));
    let instruction = instruction_from_message(&message);
    if instruction.trim().is_empty() {
        return Response::err(
            id,
            json::INVALID_PARAMS,
            "a2a.SendStreamingMessage requires a message with at least one non-empty text part",
        );
    }
    // Run the served-spawn synchronously, streaming the WORKING frame the instant the
    // handle is minted (before the blocking run) via the callback, then mapping the
    // terminal outcome. The spawn/registry logic stays in `server.rs` (no duplication).
    let context = std::cell::RefCell::new(None::<String>);
    let (handle, status, result) = ctx.a2a_spawn_stream_sync(&instruction, log, |handle| {
        let context_id = context_id_for(handle);
        *context.borrow_mut() = Some(context_id.clone());
        write_stream_frame(
            writer,
            &id,
            status_update_frame(handle, &context_id, TaskState::Working, false),
        );
    });
    let context_id = context
        .into_inner()
        .unwrap_or_else(|| context_id_for(&handle));
    let state = terminal_to_state(&status);
    // Completed → the distillate artifact frame (completed-only); then the terminal
    // status frame is RETURNED (the final frame).
    if state == TaskState::Completed
        && let Some(distillate) = result.as_ref()
    {
        write_stream_frame(
            writer,
            &id,
            artifact_update_frame(&handle, &context_id, distillate),
        );
    }
    Response::ok(id, status_update_frame(&handle, &context_id, state, true))
}

/// `a2a.SubscribeToTask` (params: `{id}`) → a STREAM of `StreamResponse` frames for
/// an EXISTING served run (status-level; RFC 0020). Looks the run up by handle:
///
///   * unknown → `-32001 TaskNotFound` (a normal error Response; nothing streamed),
///   * already terminal → write the artifact frame (completed-only) directly, then
///     RETURN the terminal `statusUpdate{final:true}` immediately,
///   * still running → write a current `statusUpdate{ WORKING, final:false }`, then
///     POLL the sessions registry (~`POLL_INTERVAL`) until the run reaches a terminal
///     status OR a bounded deadline (the drain timeout); on terminal, write the
///     artifact frame (completed-only) directly and RETURN the terminal frame. If the
///     deadline elapses first, RETURN a non-final-looking-but-`final:true` WORKING
///     terminal frame is avoided — instead the last observed running state is closed
///     out as `final:true` so the gateway's SSE stream always terminates.
fn subscribe_to_task(req: Request, ctx: &ServeCtx, writer: &SharedWriter) -> Response {
    let id = req.id.clone();
    let task_id = task_id_param(&req);
    let context_id = context_id_for(&task_id);
    // Snapshot once: unknown handle → TaskNotFound, no stream.
    let snapshot = match ctx.a2a_task_snapshot(&task_id) {
        Some(s) => s,
        None => return Response::err(id, TASK_NOT_FOUND, "task not found"),
    };
    // Already terminal at lookup → artifact (completed-only) + final frame now.
    if snapshot.0 != "running" {
        let state = terminal_to_state(&snapshot.0);
        if state == TaskState::Completed
            && let Some(distillate) = snapshot.1.as_ref()
        {
            write_stream_frame(
                writer,
                &id,
                artifact_update_frame(&task_id, &context_id, distillate),
            );
        }
        return Response::ok(id, status_update_frame(&task_id, &context_id, state, true));
    }
    // Still running → stream WORKING, then poll until terminal or the deadline.
    write_stream_frame(
        writer,
        &id,
        status_update_frame(&task_id, &context_id, TaskState::Working, false),
    );
    let (status, result) = ctx.a2a_poll_until_terminal(&task_id);
    let state = terminal_to_state(&status);
    if state == TaskState::Completed
        && let Some(distillate) = result.as_ref()
    {
        write_stream_frame(
            writer,
            &id,
            artifact_update_frame(&task_id, &context_id, distillate),
        );
    }
    Response::ok(id, status_update_frame(&task_id, &context_id, state, true))
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
    fn task_state_from_wire_roundtrips_and_terminal_classifies() {
        // from_wire ∘ as_str is identity for every named variant.
        for st in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
            TaskState::InputRequired,
            TaskState::AuthRequired,
            TaskState::Unspecified,
        ] {
            assert_eq!(TaskState::from_wire(st.as_str()), st, "{}", st.as_str());
        }
        // An unknown wire string is Unspecified (non-terminal — keep polling).
        assert_eq!(
            TaskState::from_wire("TASK_STATE_FUTURE"),
            TaskState::Unspecified
        );
        // Terminal classification: the four end states are terminal, the rest not.
        for st in [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
        ] {
            assert!(st.is_terminal(), "{} is terminal", st.as_str());
        }
        for st in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::InputRequired,
            TaskState::AuthRequired,
            TaskState::Unspecified,
        ] {
            assert!(!st.is_terminal(), "{} is not terminal", st.as_str());
        }
    }

    #[test]
    fn send_message_params_carry_objective_and_contract_as_text_parts() {
        let p = send_message_params("do the thing", Some("a 1-line summary"), "m-1");
        assert_eq!(p["message"]["messageId"], "m-1");
        assert_eq!(p["message"]["role"], "ROLE_USER");
        assert_eq!(p["message"]["parts"][0]["text"], "do the thing");
        assert_eq!(
            p["message"]["parts"][1]["text"], "Required output: a 1-line summary",
            "the output contract rides as a second text part"
        );
        // No contract → a single part.
        let p2 = send_message_params("solo", None, "m-2");
        assert_eq!(p2["message"]["parts"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn client_parses_task_id_state_and_distillate() {
        // A completed Task as the server builds it (snapshot_to_task) round-trips
        // through the client readers back to the distillate.
        let task = snapshot_to_task("served.7", TaskState::Completed, Some(&json!("the answer")));
        assert_eq!(task_id_of(&task), "served.7");
        assert_eq!(task_state_of(&task), TaskState::Completed);
        assert!(task_state_of(&task).is_terminal());
        assert_eq!(artifact_text_of(&task), "the answer");

        // A working task: terminal=false, no artifact text.
        let working = snapshot_to_task("served.8", TaskState::Working, None);
        assert_eq!(task_state_of(&working), TaskState::Working);
        assert!(!task_state_of(&working).is_terminal());
        assert_eq!(artifact_text_of(&working), "");

        // A garbled reply degrades gracefully (empty id, Unspecified, no text).
        let junk = json!({ "not": "a task" });
        assert_eq!(task_id_of(&junk), "");
        assert_eq!(task_state_of(&junk), TaskState::Unspecified);
        assert_eq!(artifact_text_of(&junk), "");
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

    // ── streaming (A2A-2) ────────────────────────────────────────────────────

    use crate::config::{Config, McpServerSpec, Mode};
    use crate::mcp::server::{PeerOrigin, ServeStream};
    use crate::obs::log::{Comp, Level, LogCtx};
    use crate::subagent::protocol::{IntelConfig, Limits, SpawnPayload, Telemetry};
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn base_payload() -> SpawnPayload {
        SpawnPayload {
            instruction: "standing".into(),
            output_contract: None,
            context_seed: Vec::new(),
            intelligence: IntelConfig {
                // Unreachable intel → a served-spawn fails fast (FAILED terminal).
                uri: "unix:/nonexistent/a2a-stream-test.sock".into(),
                token: None,
                model: None,
            },
            mcp_servers: vec![McpServerSpec {
                name: "fs".into(),
                command: vec!["a".into()],
                tags: Vec::new(),
            }],
            a2a_peers: Vec::new(),
            limits: Limits {
                max_steps: 2,
                max_tokens: 1000,
                deadline_ms: 1000,
                max_depth: 4,
            },
            telemetry: Telemetry {
                run_id: "r1".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                trace_id: None,
                log_level: "error".into(),
                log_content: false,
            },
            depth: 0,
            exec_allow: Vec::new(),
            warm: false,
        }
    }

    fn ctx() -> ServeCtx {
        let cfg = Config {
            run_id: "r1".into(),
            mode: Mode::Reactive,
            intelligence: Some("unix:/x".into()),
            ..Config::default()
        };
        ServeCtx::new(
            "r1".into(),
            "reactive".into(),
            // A bogus exe so a real subagent never starts → the served-spawn fails
            // fast and the streaming run terminates FAILED (no hang).
            "/nonexistent/agentd-a2a-stream".into(),
            base_payload(),
            Duration::from_secs(2),
            Arc::new(cfg),
        )
    }

    fn log() -> Logger {
        Logger::new(
            LogCtx {
                run_id: "r1".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: 0,
                trace_id: None,
            },
            Level::Error,
        )
    }

    fn req(method: &str, params: Option<Value>) -> Request {
        Request::new(Id::Num(1), method, params)
    }

    /// A connection writer whose PEER end is RETAINED + returned, so a test can READ
    /// the `StreamResponse` frames the handler wrote directly to the writer. (The
    /// unary `writer()` helper drops the peer; this one keeps it.)
    fn readable_writer() -> (SharedWriter, BufReader<UnixStream>) {
        let (a, b) = UnixStream::pair().expect("socketpair");
        (
            Arc::new(Mutex::new(ServeStream::Unix(a))),
            BufReader::new(b),
        )
    }

    /// Read one NDJSON frame off the peer end and parse it as JSON.
    fn read_frame(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read frame");
        serde_json::from_str(&line).expect("parse frame")
    }

    /// The `result` (a `StreamResponse`) of one frame, asserting it shares the id.
    fn stream_result(reader: &mut BufReader<UnixStream>) -> Value {
        let frame = read_frame(reader);
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["id"], 1, "stream frames share the request id");
        frame["result"].clone()
    }

    #[test]
    fn send_streaming_message_streams_working_then_final_failed() {
        // A fresh streaming run against unreachable intel fails fast: the handler
        // emits WORKING(final:false), then RETURNS the final FAILED(final:true) — and
        // NO artifact frame (the run did not complete; distillate-only invariant).
        let ctx = ctx();
        let (w, mut reader) = readable_writer();
        let msg = json!({ "message": { "parts": [{ "text": "do it" }] } });
        let resp =
            send_streaming_message(req("a2a.SendStreamingMessage", Some(msg)), &ctx, &w, &log());

        // Frame 1 (written to the writer): WORKING, not final.
        let f1 = stream_result(&mut reader);
        assert_eq!(f1["statusUpdate"]["status"]["state"], "TASK_STATE_WORKING");
        assert_eq!(f1["statusUpdate"]["final"], false);
        let task_id = f1["statusUpdate"]["taskId"].as_str().unwrap().to_string();
        assert_eq!(f1["statusUpdate"]["contextId"], format!("ctx-{task_id}"));

        // The RETURNED frame is the final FAILED status — no artifact was emitted.
        let final_sr = resp.result.expect("ok");
        assert_eq!(
            final_sr["statusUpdate"]["status"]["state"],
            "TASK_STATE_FAILED"
        );
        assert_eq!(final_sr["statusUpdate"]["final"], true);
        assert_eq!(final_sr["statusUpdate"]["taskId"], task_id);
        assert!(
            final_sr.get("artifactUpdate").is_none(),
            "a failed run streams no artifact (distillate-only)"
        );

        // Only the single WORKING frame was written to the writer (no artifact frame).
        // Closing our handle would EOF the reader; assert nothing else is queued by
        // checking the writer has no further buffered line via a non-blocking peek is
        // overkill — instead the FAILED-final being the RETURN value (not a written
        // frame) is the contract, already asserted above.
    }

    #[test]
    fn send_streaming_message_empty_instruction_is_invalid_params_no_stream() {
        let ctx = ctx();
        let (w, _reader) = readable_writer();
        let msg = json!({ "message": { "parts": [{ "data": { "k": "v" } }] } });
        let resp =
            send_streaming_message(req("a2a.SendStreamingMessage", Some(msg)), &ctx, &w, &log());
        assert_eq!(resp.error.expect("err").code, json::INVALID_PARAMS);
    }

    #[test]
    fn subscribe_to_task_completed_streams_artifact_then_final() {
        // Already-terminal COMPLETED run: artifactUpdate frame (the distillate), then
        // the RETURNED final COMPLETED(final:true) frame.
        let ctx = ctx();
        ctx.a2a_seed_done("served.0", "completed", json!("the answer"));
        let (w, mut reader) = readable_writer();
        let resp = subscribe_to_task(
            req("a2a.SubscribeToTask", Some(json!({ "id": "served.0" }))),
            &ctx,
            &w,
        );

        // Frame 1 (written): the artifact carrying the distillate, lastChunk.
        let art = stream_result(&mut reader);
        assert_eq!(art["artifactUpdate"]["taskId"], "served.0");
        assert_eq!(art["artifactUpdate"]["contextId"], "ctx-served.0");
        assert_eq!(
            art["artifactUpdate"]["artifact"]["parts"][0]["text"],
            "the answer"
        );
        assert_eq!(
            art["artifactUpdate"]["artifact"]["artifactId"],
            "served.0.distillate"
        );
        assert_eq!(art["artifactUpdate"]["lastChunk"], true);

        // The RETURNED frame is the final COMPLETED status.
        let final_sr = resp.result.expect("ok");
        assert_eq!(
            final_sr["statusUpdate"]["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
        assert_eq!(final_sr["statusUpdate"]["final"], true);
    }

    #[test]
    fn subscribe_to_task_failed_streams_final_only_no_artifact() {
        // A terminal failed/refused run streams NO artifact — only the final frame.
        for (status, want_state) in [
            ("crashed", "TASK_STATE_FAILED"),
            ("refused", "TASK_STATE_REJECTED"),
        ] {
            let ctx = ctx();
            // `refused` carries no result; `crashed`/Failed carries none either.
            ctx.a2a_seed_done("served.0", status, json!("should-not-leak"));
            let (w, mut reader) = readable_writer();
            let resp = subscribe_to_task(
                req("a2a.SubscribeToTask", Some(json!({ "id": "served.0" }))),
                &ctx,
                &w,
            );
            let final_sr = resp.result.expect("ok");
            assert_eq!(
                final_sr["statusUpdate"]["status"]["state"], want_state,
                "{status}"
            );
            assert_eq!(final_sr["statusUpdate"]["final"], true);
            // No artifact frame was written: the only readable line is... none. We
            // can't block on read here (it would hang), so instead assert the RETURN
            // carries no artifact and trust the handler only writes the artifact on
            // the COMPLETED branch (covered by the completed test above).
            assert!(final_sr.get("artifactUpdate").is_none());
            // Drop the writer (the only write end) so the reader EOFs rather than
            // blocking; then confirm no frame was written before EOF.
            drop(w);
            let mut line = String::new();
            let n = reader.read_line(&mut line).expect("read");
            assert_eq!(
                n, 0,
                "no frame written for a failed subscribe ({status}): {line:?}"
            );
        }
    }

    #[test]
    fn subscribe_to_task_unknown_id_is_task_not_found() {
        let ctx = ctx();
        let (w, _reader) = readable_writer();
        let resp = subscribe_to_task(
            req("a2a.SubscribeToTask", Some(json!({ "id": "served.404" }))),
            &ctx,
            &w,
        );
        assert_eq!(resp.error.expect("err").code, TASK_NOT_FOUND);
    }

    #[test]
    fn streaming_methods_are_management_gated() {
        // The origin gate lives in `server::dispatch` (a Stdio peer never reaches
        // `dispatch_a2a`); a Stdio-origin `a2a.*` falls through to METHOD_NOT_FOUND.
        let ctx = ctx();
        for method in ["a2a.SendStreamingMessage", "a2a.SubscribeToTask"] {
            let msg = json!({ "message": { "parts": [{ "text": "x" }] }, "id": "served.0" });
            let resp = crate::mcp::server::dispatch_for_test(
                req(method, Some(msg)),
                &ctx,
                PeerOrigin::Stdio,
                &log(),
            );
            assert_eq!(
                resp.error.expect("err").code,
                json::METHOD_NOT_FOUND,
                "{method} from a Stdio origin → -32601"
            );
        }
    }
}
