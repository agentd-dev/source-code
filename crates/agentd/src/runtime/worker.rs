// SPDX-License-Identifier: AGPL-3.0-only
//! The **turn worker** (RFC 0026 §2, §3.2): a child process (`Role::Turn`) that
//! runs ONE turn over the context slice the supervisor handed it — a root /
//! conversation turn, a bounded `agent` step, or a structured `think` — and
//! reports the transcript delta, the usage and the outcome (`TurnDone`).
//!
//! It calls the model and MCP tools itself; **internal** tools (memory, plan,
//! subagents, sleep, finish, …) are **round-tripped** to the supervisor
//! (`ToolRequest` → `ToolResult`) so state changes are made by the state
//! owner. Before each model call it may ask for budget admission
//! (`BudgetRequest` → `BudgetGrant`).
//!
//! Compared with the 1.x ReAct loop this turn keeps **structured** tool
//! results (`structuredContent` first, text-JSON second, text last),
//! validates a schema'd final answer and re-asks on a miss, keeps a
//! serializable transcript, estimates tokens for admission, and detects
//! call loops. The loop body is [`run_turn`], generic over a [`Bridge`] so it
//! is unit-testable in-process; [`run_turn_child`] is the process entry.

use crate::context::{Msg, tokens};
use crate::intel::client::IntelClient;
use crate::jsonschema;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::subagent::control::{Up, send_up};
use crate::subagent::protocol::{AgentMsg, SpawnPayload, TurnKind, TurnResult, TurnSpec};
use crate::subagent::replies::{Replies, Reply};
use crate::wire::intel::{Message, Request, ToolCall, Usage};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Default per-response completion cap.
pub const DEFAULT_MAX_TOKENS_PER_CALL: u32 = 4096;
/// Re-asks when a schema'd answer misses.
pub const SCHEMA_REASKS: u32 = 2;
/// The same tool call (name + args) this many times in one turn is a loop.
pub const LOOP_REPEATS: usize = 4;
/// Longest a single MCP tool call may take inside a turn.
pub const TOOL_CALL_CAP: Duration = Duration::from_secs(600);

/// A budget grant as the worker sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetReply {
    pub ok: bool,
    pub wait_ms: u64,
    pub model: Option<String>,
    pub reason: Option<String>,
}

/// The supervisor link: internal-tool and budget round-trips + cancellation.
pub trait Bridge {
    /// Execute an internal tool via the supervisor. `None` = no answer
    /// (cancelled / channel gone / deadline).
    fn tool_request(
        &mut self,
        name: &str,
        args: &Value,
        deadline: Instant,
    ) -> Option<(Value, bool)>;
    /// Ask for budget admission. `None` = no answer.
    fn budget_request(&mut self, estimate: u64, deadline: Instant) -> Option<BudgetReply>;
    fn cancelled(&self) -> bool;
    /// A progress event (liveness).
    fn progress(&mut self, _event: &str, _fields: Value) {}
}

/// The process bridge: frames over the control channel.
pub struct ChildBridge<'a> {
    pub up: &'a Up,
    pub replies: &'a Arc<Replies>,
    pub cancel: &'a Arc<AtomicBool>,
}

impl Bridge for ChildBridge<'_> {
    fn tool_request(
        &mut self,
        name: &str,
        args: &Value,
        deadline: Instant,
    ) -> Option<(Value, bool)> {
        let id = self.replies.next_id();
        send_up(
            self.up,
            &AgentMsg::ToolRequest {
                id,
                name: name.to_string(),
                args: args.clone(),
            },
        );
        match self.replies.wait(id, deadline, self.cancel)? {
            Reply::Tool { result, is_error } => Some((result, is_error)),
            Reply::Budget { .. } => None,
        }
    }
    fn budget_request(&mut self, estimate: u64, deadline: Instant) -> Option<BudgetReply> {
        let id = self.replies.next_id();
        send_up(self.up, &AgentMsg::BudgetRequest { id, estimate });
        match self.replies.wait(id, deadline, self.cancel)? {
            Reply::Budget {
                ok,
                wait_ms,
                model,
                reason,
            } => Some(BudgetReply {
                ok,
                wait_ms,
                model,
                reason,
            }),
            Reply::Tool { .. } => None,
        }
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    fn progress(&mut self, event: &str, fields: Value) {
        send_up(
            self.up,
            &AgentMsg::Event {
                event: event.to_string(),
                fields,
            },
        );
    }
}

/// How MCP tools are reached from the turn.
pub trait McpCaller {
    /// Call `tool` on `server` with `args` and extra `_meta`; `(content, is_error)`
    /// where content is the structured result when the server gave one.
    fn call(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        meta: Value,
        timeout: Duration,
    ) -> Result<(Value, bool), String>;
}

/// The connected MCP clients as an [`McpCaller`].
pub struct McpClients<'a>(pub &'a [McpClient]);

impl McpCaller for McpClients<'_> {
    fn call(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        meta: Value,
        timeout: Duration,
    ) -> Result<(Value, bool), String> {
        let c = self
            .0
            .iter()
            .find(|c| c.name() == server)
            .ok_or_else(|| format!("mcp server {server:?} is not connected"))?;
        let res = c
            .call_tool_with_meta_within(tool, Some(args), meta, timeout)
            .map_err(|e| e.to_string())?;
        Ok((tool_result_value(&res), res.is_error()))
    }
}

/// The value of an MCP tool result: `structuredContent`, else the text parsed
/// as JSON, else the text.
pub fn tool_result_value(res: &::mcp::wire::CallToolResult) -> Value {
    if let Some(sc) = &res.structured_content {
        return sc.clone();
    }
    let text = res.text();
    serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))
}

/// The knobs of one turn (from the payload).
#[derive(Debug, Clone)]
pub struct TurnLimits {
    pub max_rounds: u32,
    pub max_tokens: u64,
    pub deadline: Instant,
    pub model: String,
}

/// Run ONE turn. Never panics; every failure is a `TurnResult` status.
pub fn run_turn(
    spec: &TurnSpec,
    limits: &TurnLimits,
    intel: &IntelClient,
    mcp: &dyn McpCaller,
    bridge: &mut dyn Bridge,
    log: &Logger,
) -> TurnResult {
    let start = Instant::now();
    let mut result = TurnResult {
        status: "completed".into(),
        ..Default::default()
    };
    let mut messages: Vec<Message> = Vec::with_capacity(spec.messages.len() + 2);
    if !spec.system.trim().is_empty() {
        messages.push(Message::System(spec.system.clone()));
    }
    messages.extend(spec.messages.iter().map(Msg::to_wire));
    let tools = if spec.kind == TurnKind::Think {
        Vec::new()
    } else {
        spec.tools.clone()
    };
    let mut model = limits.model.clone();
    let max_rounds = if spec.max_rounds > 0 {
        spec.max_rounds
    } else if limits.max_rounds > 0 {
        limits.max_rounds
    } else {
        u32::MAX
    };
    let max_tokens_per_call = if spec.max_tokens_per_call > 0 {
        spec.max_tokens_per_call
    } else {
        DEFAULT_MAX_TOKENS_PER_CALL
    };
    let mut reasks = 0u32;
    let mut recent_calls: Vec<String> = Vec::new();
    let mut usage_total = Usage::default();
    let mut delta: Vec<Msg> = Vec::new();
    let mut last_text: Option<String> = None;
    // The OTEL `invoke_agent` span for this turn (plan §3.11) — a no-op handle
    // without the `otel` feature / an endpoint. `Option` so `finish` (which
    // consumes the span) can be `take`n from the return-macro without a move
    // across the loop.
    let mut run_span = Some(crate::obs::otel::run_begin(
        log.ctx().trace_id.as_deref(),
        crate::obs::otel::now_unix_nanos(),
    ));

    log.info("turn.start", json!({"turn": spec.turn_id, "kind": spec.kind, "messages": messages.len(), "tools": tools.len()}));

    macro_rules! finish_with {
        ($status:expr) => {{
            result.status = $status.to_string();
            result.messages = delta;
            result.usage = usage_total;
            result.text = last_text.clone();
            if let Some(rs) = run_span.take() {
                rs.finish(&model, usage_total.input_tokens, usage_total.output_tokens, $status == "completed");
            }
            log.info("turn.done", json!({"turn": spec.turn_id, "status": result.status, "rounds": result.rounds, "tool_calls": result.tool_calls, "tokens_in": usage_total.input_tokens, "tokens_out": usage_total.output_tokens}));
            return result;
        }};
    }

    loop {
        if bridge.cancelled() {
            finish_with!("cancelled");
        }
        if Instant::now() >= limits.deadline {
            finish_with!("deadline");
        }
        if result.rounds >= max_rounds {
            finish_with!("exhausted_steps");
        }
        if limits.max_tokens > 0 && usage_total.total() >= limits.max_tokens {
            finish_with!("exhausted_tokens");
        }
        // Budget admission (RFC 0026 §7).
        if spec.budget_admission {
            let estimate: u64 = messages
                .iter()
                .map(|m| tokens::estimate(&render_len(m)) + tokens::MESSAGE_OVERHEAD)
                .sum::<u64>()
                + tools
                    .iter()
                    .map(|t| tokens::estimate(&t.name) + tokens::estimate_value(&t.input_schema))
                    .sum::<u64>()
                + max_tokens_per_call as u64;
            loop {
                match bridge.budget_request(estimate, limits.deadline) {
                    None => finish_with!("cancelled"),
                    Some(BudgetReply {
                        ok: true, model: m, ..
                    }) => {
                        if let Some(m) = m
                            && m != model
                        {
                            log.info(
                                "turn.model_degraded",
                                json!({"turn": spec.turn_id, "from": model, "to": m}),
                            );
                            model = m;
                        }
                        break;
                    }
                    Some(BudgetReply {
                        ok: false,
                        wait_ms,
                        reason,
                        ..
                    }) => {
                        if let Some(r) = reason {
                            result.error = Some(format!("budget: {r}"));
                            finish_with!(if r.contains("refus") {
                                "refused"
                            } else {
                                "exhausted_tokens"
                            });
                        }
                        let remaining = limits.deadline.saturating_duration_since(Instant::now());
                        let wait = Duration::from_millis(wait_ms.max(50)).min(remaining);
                        if wait.is_zero() {
                            finish_with!("deadline");
                        }
                        log.info(
                            "turn.budget_wait",
                            json!({"turn": spec.turn_id, "wait_ms": wait.as_millis() as u64}),
                        );
                        std::thread::sleep(wait);
                        if bridge.cancelled() {
                            finish_with!("cancelled");
                        }
                    }
                }
            }
        }
        // The model call. Announce it first (RFC 0032 §17): the supervisor turns
        // this into the display clients' live activity — `thinking` from here
        // until the response lands.
        bridge.progress(
            "turn.think",
            json!({"turn": spec.turn_id, "round": result.rounds + 1}),
        );
        let req = Request {
            model: model.clone(),
            messages: messages.clone(),
            tools: tools.clone(),
            max_tokens: max_tokens_per_call,
            temperature: spec.temperature.or(Some(0.0)),
        };
        let chat_start = crate::obs::otel::now_unix_nanos();
        let resp = match intel.complete(&req) {
            Ok(r) => r,
            Err(e) => {
                if let Some(rs) = run_span.as_mut() {
                    rs.record_chat(&model, 0, 0, false, chat_start);
                }
                result.error = Some(format!("intel: {e}"));
                finish_with!("failed");
            }
        };
        result.rounds += 1;
        usage_total.input_tokens += resp.usage.input_tokens;
        usage_total.output_tokens += resp.usage.output_tokens;
        if let Some(rs) = run_span.as_mut() {
            rs.record_chat(
                &model,
                resp.usage.input_tokens,
                resp.usage.output_tokens,
                true,
                chat_start,
            );
        }
        // Carry the round's token usage upward too — the supervisor attributes
        // it to this turn's live activity (the instance counters are settled
        // separately from the terminal usage, so this never double-counts).
        bridge.progress("turn.round", json!({"turn": spec.turn_id, "round": result.rounds, "tool_calls": resp.tool_calls.len(), "tokens_in": resp.usage.input_tokens, "tokens_out": resp.usage.output_tokens}));
        log.debug("turn.round", json!({"turn": spec.turn_id, "round": result.rounds, "tokens_in": resp.usage.input_tokens, "tokens_out": resp.usage.output_tokens, "tool_calls": resp.tool_calls.len()}));

        if resp.wants_tools() {
            if let Some(t) = &resp.text
                && !t.is_empty()
            {
                last_text = Some(t.clone());
            }
            let assistant = Msg::assistant(resp.text.clone(), resp.tool_calls.clone());
            messages.push(assistant.to_wire());
            delta.push(assistant);
            let mut finish_seen = None;
            for (i, tc) in resp.tool_calls.iter().enumerate() {
                if bridge.cancelled() {
                    seal_unanswered(&mut delta, &resp.tool_calls[i..], "cancelled");
                    finish_with!("cancelled");
                }
                result.tool_calls += 1;
                // Loop detection: the same call repeated.
                let sig = format!("{}:{}", tc.name, tc.arguments);
                recent_calls.push(sig.clone());
                if recent_calls.iter().filter(|s| **s == sig).count() >= LOOP_REPEATS {
                    result.error = Some(format!(
                        "the model repeated {} with identical arguments {LOOP_REPEATS} times",
                        tc.name
                    ));
                    seal_unanswered(&mut delta, &resp.tool_calls[i..], "loop_detected");
                    finish_with!("loop_detected");
                }
                let call_start = Instant::now();
                let tool_span_start = crate::obs::otel::now_unix_nanos();
                // The one signal that says WHAT it is doing right now — for MCP
                // tools the supervisor never sees the call otherwise (the child
                // holds its own MCP connections).
                bridge.progress(
                    "turn.tool",
                    json!({"turn": spec.turn_id, "tool": tc.name, "i": i + 1, "of": resp.tool_calls.len()}),
                );
                let (content, is_error) = execute_call(spec, limits, tc, i, mcp, bridge, log);
                if let Some(rs) = run_span.as_mut() {
                    rs.record_tool(&tc.name, !is_error, tool_span_start);
                }
                log.info("tool.result", json!({"turn": spec.turn_id, "tool": tc.name, "is_error": is_error, "ms": call_start.elapsed().as_millis() as u64}));
                let msg = Msg::tool(tc.id.clone(), tc.name.clone(), content, is_error);
                messages.push(msg.to_wire());
                delta.push(msg);
                if tc.name == "finish" && !is_error {
                    finish_seen = Some(tc.arguments.clone());
                }
            }
            if let Some(f) = finish_seen {
                result.finish = Some(f);
                last_text = last_text.or_else(|| {
                    result
                        .finish
                        .as_ref()
                        .and_then(|f| f.get("output"))
                        .map(|o| match o {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                });
                finish_with!("completed");
            }
            continue;
        }

        // A final answer.
        let text = resp
            .text
            .clone()
            .or_else(|| last_text.clone())
            .unwrap_or_default();
        let assistant = Msg::assistant(Some(text.clone()), Vec::new());
        // Structured answers: parse + validate, re-ask on a miss.
        let wants_object = spec.kind == TurnKind::Think || spec.output_schema.is_some();
        if wants_object {
            match parse_json_answer(&text) {
                Ok(v) => {
                    let check = match &spec.output_schema {
                        Some(schema) => {
                            jsonschema::validate(schema, &v).map_err(|e| jsonschema::explain(&e))
                        }
                        None => Ok(()),
                    };
                    match check {
                        Ok(()) => {
                            messages.push(assistant.to_wire());
                            delta.push(assistant);
                            last_text = Some(text);
                            result.value = Some(v);
                            finish_with!("completed");
                        }
                        Err(e) => {
                            if reasks < SCHEMA_REASKS {
                                reasks += 1;
                                log.info("turn.reask", json!({"turn": spec.turn_id, "reason": e}));
                                messages.push(assistant.to_wire());
                                delta.push(assistant);
                                let ask = Msg::user(
                                    format!(
                                        "Your answer did not match the required schema: {e}. Reply again with ONLY one JSON object that matches the schema."
                                    ),
                                    None,
                                );
                                messages.push(ask.to_wire());
                                delta.push(ask);
                                continue;
                            }
                            result.error =
                                Some(format!("answer does not match the output schema: {e}"));
                            messages.push(assistant.to_wire());
                            delta.push(assistant);
                            last_text = Some(text);
                            finish_with!("failed");
                        }
                    }
                }
                Err(e) => {
                    if reasks < SCHEMA_REASKS {
                        reasks += 1;
                        log.info("turn.reask", json!({"turn": spec.turn_id, "reason": e}));
                        messages.push(assistant.to_wire());
                        delta.push(assistant);
                        let ask = Msg::user(format!("{e}. Reply with ONLY one JSON object."), None);
                        messages.push(ask.to_wire());
                        delta.push(ask);
                        continue;
                    }
                    result.error = Some(e);
                    delta.push(assistant);
                    last_text = Some(text);
                    finish_with!("failed");
                }
            }
        }
        delta.push(assistant);
        last_text = Some(text);
        let _ = start;
        finish_with!("completed");
    }
}

fn render_len(m: &Message) -> String {
    match m {
        Message::System(s) | Message::User(s) => s.clone(),
        Message::Assistant { text, tool_calls } => format!(
            "{}{}",
            text.as_deref().unwrap_or(""),
            tool_calls
                .iter()
                .map(|c| c.arguments.to_string())
                .collect::<String>()
        ),
        Message::ToolResult { content, .. } => content.clone(),
    }
}

/// Answer the tool calls the turn never got to run, so the transcript delta it
/// reports is self-consistent.
///
/// Every provider dialect requires one tool result per `tool_calls` id on the
/// preceding assistant message, and the tool loop has exits that fire BETWEEN
/// pushing that assistant message and pushing its results (cancellation between
/// calls, loop detection on the offending call). The delta is appended verbatim
/// to the DURABLE context, so an unanswered id is not one lost result: every
/// later turn and every restart replays the same malformed context and the
/// provider rejects it with a fatal 400 forever — reported as a retryable
/// `intel:` failure (exit `INTEL_UNAVAILABLE`), so an external scheduler keeps
/// retrying a request agentd itself malformed. The synthetic result is an error
/// result, the same shape [`execute_call`] uses when the supervisor never
/// answers, so the model sees the call did not happen rather than a made-up
/// success.
fn seal_unanswered(delta: &mut Vec<Msg>, unanswered: &[ToolCall], status: &str) {
    for tc in unanswered {
        delta.push(Msg::tool(
            tc.id.clone(),
            tc.name.clone(),
            Value::String(format!(
                "{}: not executed — the turn ended ({status}) before this call ran",
                tc.name
            )),
            true,
        ));
    }
}

/// Dispatch one tool call: internal (round-trip) → MCP (own call) → code.
fn execute_call(
    spec: &TurnSpec,
    limits: &TurnLimits,
    tc: &ToolCall,
    index: usize,
    mcp: &dyn McpCaller,
    bridge: &mut dyn Bridge,
    log: &Logger,
) -> (Value, bool) {
    let name = tc.name.as_str();
    log.info(
        "tool.call",
        json!({"turn": spec.turn_id, "tool": name, "id": tc.id}),
    );
    if spec.internal.iter().any(|n| n == name) {
        return match bridge.tool_request(name, &tc.arguments, limits.deadline) {
            Some(r) => r,
            None => (
                Value::String(format!(
                    "{name}: no answer from the supervisor (cancelled or deadline)"
                )),
                true,
            ),
        };
    }
    if let Some((server, tool)) = spec.mcp_routes.get(name) {
        let remaining = limits.deadline.saturating_duration_since(Instant::now());
        let timeout = remaining.min(TOOL_CALL_CAP).max(Duration::from_millis(100));
        let mut meta = spec.tool_meta.clone().unwrap_or_else(|| json!({}));
        if !spec.idempotency_prefix.is_empty() {
            meta["agent/idempotency_key"] = json!(format!(
                "{}#{}.{}",
                spec.idempotency_prefix, spec.turn_id, index
            ));
        }
        return match mcp.call(server, tool, tc.arguments.clone(), meta, timeout) {
            Ok(r) => r,
            Err(e) => (Value::String(format!("tool transport error: {e}")), true),
        };
    }
    if let Some(r) = crate::tools::call(name, &tc.arguments) {
        return match r {
            Ok(v) => (v, false),
            Err(e) => (Value::String(e), true),
        };
    }
    (Value::String(format!("error: no such tool '{name}'")), true)
}

/// Parse a model's JSON answer tolerantly (fences, prose around the object).
pub fn parse_json_answer(answer: &str) -> Result<Value, String> {
    let t = answer.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    let t2 = t
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(v) = serde_json::from_str::<Value>(t2) {
        return Ok(v);
    }
    if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}'))
        && b > a
        && let Ok(v) = serde_json::from_str::<Value>(&t[a..=b])
    {
        return Ok(v);
    }
    if let (Some(a), Some(b)) = (t.find('['), t.rfind(']'))
        && b > a
        && let Ok(v) = serde_json::from_str::<Value>(&t[a..=b])
    {
        return Ok(v);
    }
    Err(format!(
        "the answer was not valid JSON: {}",
        t.chars().take(120).collect::<String>()
    ))
}

/// The process entry for a `Role::Turn` child: run the turn, report
/// `Usage` + `TurnDone`, return the exit code.
pub fn run_turn_child(
    payload: &SpawnPayload,
    intel: &IntelClient,
    servers: &[McpClient],
    up: &Up,
    cancel: &Arc<AtomicBool>,
    replies: &Arc<Replies>,
    log: &Logger,
) -> i32 {
    let Some(spec) = payload.turn.as_deref() else {
        send_up(
            up,
            &AgentMsg::Failed {
                error: "role is turn but the payload carries no turn spec".into(),
            },
        );
        return crate::exit::USAGE;
    };
    let limits = TurnLimits {
        max_rounds: if spec.max_rounds > 0 {
            spec.max_rounds
        } else {
            payload.limits.max_steps
        },
        max_tokens: payload.limits.max_tokens,
        deadline: Instant::now() + Duration::from_millis(payload.limits.deadline_ms.max(1)),
        model: payload.intelligence.model.clone().unwrap_or_default(),
    };
    let mut bridge = ChildBridge {
        up,
        replies,
        cancel,
    };
    let result = run_turn(spec, &limits, intel, &McpClients(servers), &mut bridge, log);
    let code = match result.status.as_str() {
        "completed" => crate::exit::SUCCESS,
        "failed"
            if result
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("intel:")) =>
        {
            crate::exit::INTEL_UNAVAILABLE
        }
        "failed" | "cancelled" => crate::exit::GENERIC,
        "refused" => crate::exit::REFUSED,
        "exhausted_steps" | "exhausted_tokens" => crate::exit::BUDGET,
        "deadline" => crate::exit::DEADLINE,
        _ => crate::exit::PARTIAL,
    };
    send_up(up, &AgentMsg::Usage(result.usage));
    send_up(
        up,
        &AgentMsg::TurnDone {
            turn: Box::new(result),
        },
    );
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::log::{Comp, Level, LogCtx};
    use crate::wire::intel::ToolDef;
    use std::collections::BTreeMap;

    fn log() -> Logger {
        Logger::new(
            LogCtx {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Agent,
                pid: 0,
                trace_id: None,
            },
            Level::Warn,
        )
    }

    /// Start the in-process mock LLM with a playbook file; returns the client.
    fn mock_intel(playbook: &Value) -> IntelClient {
        let dir = std::env::temp_dir();
        let n = std::process::id() as u64 * 1000
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
                % 1000;
        let pb = dir.join(format!("agentd-worker-pb-{n}.json"));
        std::fs::write(&pb, playbook.to_string()).unwrap();
        let addr_file = dir.join(format!("agentd-worker-mock-{n}.addr"));
        let _ = std::fs::remove_file(&addr_file);
        let af = addr_file.to_string_lossy().to_string();
        let script = format!("file:{}", pb.to_string_lossy());
        std::thread::spawn(move || crate::intel::mock::run(&af, &script));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !addr_file.exists() {
            assert!(Instant::now() < deadline, "mock never announced");
            std::thread::sleep(Duration::from_millis(5));
        }
        let addr = std::fs::read_to_string(&addr_file).unwrap();
        IntelClient::from_parts(&format!("http://{}", addr.trim()), None).unwrap()
    }

    struct FakeBridge {
        calls: Vec<(String, Value)>,
        answers: BTreeMap<String, (Value, bool)>,
        budget: Vec<BudgetReply>,
        cancel: bool,
    }
    impl Bridge for FakeBridge {
        fn tool_request(&mut self, name: &str, args: &Value, _d: Instant) -> Option<(Value, bool)> {
            self.calls.push((name.to_string(), args.clone()));
            Some(
                self.answers
                    .get(name)
                    .cloned()
                    .unwrap_or((json!({"ok": true}), false)),
            )
        }
        fn budget_request(&mut self, _e: u64, _d: Instant) -> Option<BudgetReply> {
            if self.budget.is_empty() {
                Some(BudgetReply {
                    ok: true,
                    wait_ms: 0,
                    model: None,
                    reason: None,
                })
            } else {
                Some(self.budget.remove(0))
            }
        }
        fn cancelled(&self) -> bool {
            self.cancel
        }
    }
    struct FakeMcp;
    impl McpCaller for FakeMcp {
        fn call(
            &self,
            server: &str,
            tool: &str,
            args: Value,
            meta: Value,
            _t: Duration,
        ) -> Result<(Value, bool), String> {
            if server == "down" {
                return Err("connection refused".into());
            }
            Ok((
                json!({"server": server, "tool": tool, "args": args, "idem": meta["agent/idempotency_key"]}),
                tool == "boom",
            ))
        }
    }

    fn limits() -> TurnLimits {
        TurnLimits {
            max_rounds: 8,
            max_tokens: 0,
            deadline: Instant::now() + Duration::from_secs(20),
            model: "mock".into(),
        }
    }

    fn spec(kind: TurnKind) -> TurnSpec {
        TurnSpec {
            kind,
            system: "You are a test agent.".into(),
            messages: vec![Msg::user("do the thing", None)],
            tools: vec![
                ToolDef {
                    name: "memory.set".into(),
                    description: "".into(),
                    input_schema: json!({"type": "object"}),
                },
                ToolDef {
                    name: "fs.read".into(),
                    description: "".into(),
                    input_schema: json!({"type": "object"}),
                },
                ToolDef {
                    name: "finish".into(),
                    description: "".into(),
                    input_schema: json!({"type": "object"}),
                },
            ],
            internal: vec!["memory.set".into(), "finish".into()],
            mcp_routes: [
                (
                    "fs.read".to_string(),
                    ("fs".to_string(), "read".to_string()),
                ),
                ("boom".to_string(), ("fs".to_string(), "boom".to_string())),
            ]
            .into_iter()
            .collect(),
            idempotency_prefix: "ctx/1".into(),
            turn_id: "t1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_turn_round_trips_internal_tools_calls_mcp_itself_and_finishes() {
        // The playbook indexes turns by the number of tool results in the
        // transcript: one round with three calls, then the final answer.
        let intel = mock_intel(&json!({"turns": [
            {"tool_calls": [{"name": "memory.set", "arguments": {"key": "k", "value": 1}}, {"name": "fs.read", "arguments": {"path": "/x"}}, {"name": "boom", "arguments": {}}]},
            {"content": "unused"},
            {"content": "unused"},
            {"content": "all done", "usage": {"prompt_tokens": 100, "completion_tokens": 20}}
        ]}));
        let mut bridge = FakeBridge {
            calls: vec![],
            answers: BTreeMap::new(),
            budget: vec![],
            cancel: false,
        };
        let r = run_turn(
            &spec(TurnKind::Turn),
            &limits(),
            &intel,
            &FakeMcp,
            &mut bridge,
            &log(),
        );
        assert_eq!(r.status, "completed", "{:?}", r.error);
        assert_eq!(r.text.as_deref(), Some("all done"));
        assert_eq!(r.rounds, 2);
        assert_eq!(r.tool_calls, 3);
        assert_eq!(
            bridge.calls,
            vec![("memory.set".to_string(), json!({"key": "k", "value": 1}))],
            "only the internal tool round-tripped"
        );
        // Delta: assistant, tool, tool, tool, assistant.
        assert_eq!(r.messages.len(), 5);
        match &r.messages[2] {
            Msg::Tool {
                name,
                content,
                is_error,
                ..
            } => {
                assert_eq!(name, "fs.read");
                assert!(!is_error);
                assert_eq!(content["server"], json!("fs"));
                assert_eq!(
                    content["idem"],
                    json!("ctx/1#t1.1"),
                    "idempotency key stamped per call"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&r.messages[3], Msg::Tool { is_error: true, .. }));
        assert_eq!(r.usage.input_tokens, 11 + 100);
        assert_eq!(r.usage.output_tokens, 7 + 20);
    }

    #[test]
    fn finish_ends_the_turn_and_a_think_returns_an_object_with_reasks() {
        let intel = mock_intel(&json!({"turns": [
            {"tool_calls": [{"name": "finish", "arguments": {"status": "completed", "output": {"n": 3}}}]},
            {"content": "never reached"}
        ]}));
        let mut bridge = FakeBridge {
            calls: vec![],
            answers: BTreeMap::new(),
            budget: vec![],
            cancel: false,
        };
        let r = run_turn(
            &spec(TurnKind::Agent),
            &limits(),
            &intel,
            &FakeMcp,
            &mut bridge,
            &log(),
        );
        assert_eq!(r.status, "completed");
        assert_eq!(r.finish.as_ref().unwrap()["output"]["n"], json!(3));
        assert_eq!(r.rounds, 1);
        // A think: the first answer misses the schema, the re-ask fixes it.
        let intel = mock_intel(&json!({"turns": [
            {"content": "Sure: {\"intent\": \"maybe\"}"},
            {"content": "```json\n{\"intent\": \"task\", \"needs_plan\": true}\n```"}
        ]}));
        let mut s = spec(TurnKind::Think);
        s.output_schema = Some(
            json!({"type": "object", "properties": {"intent": {"enum": ["chat", "task"]}, "needs_plan": {"type": "boolean"}}, "required": ["intent"]}),
        );
        // The mock indexes turns by tool results; a think has none, so it always
        // answers turn 0 unless a match rule catches the re-ask.
        let intel2 = mock_intel(
            &json!({"turns": [{"content": "Sure: {\"intent\": \"maybe\"}"}], "match": [{"when_contains": "did not match the required schema", "content": {"intent": "task", "needs_plan": true}}]}),
        );
        drop(intel);
        let r = run_turn(&s, &limits(), &intel2, &FakeMcp, &mut bridge, &log());
        assert_eq!(r.status, "completed", "{:?}", r.error);
        assert_eq!(r.value.as_ref().unwrap()["intent"], json!("task"));
        assert_eq!(r.rounds, 2);
        // Never valid ⇒ failed after the re-asks.
        let intel3 = mock_intel(&json!({"turns": [{"content": "not json at all"}]}));
        let r = run_turn(&s, &limits(), &intel3, &FakeMcp, &mut bridge, &log());
        assert_eq!(r.status, "failed");
        assert_eq!(r.rounds, 1 + SCHEMA_REASKS);
    }

    #[test]
    fn loops_budget_waits_and_cancel_are_bounded() {
        // The model repeats the same call forever ⇒ loop_detected.
        let intel = mock_intel(
            &json!({"turns": [{"tool_calls": [{"name": "fs.read", "arguments": {"path": "/same"}}]}]}),
        );
        let mut bridge = FakeBridge {
            calls: vec![],
            answers: BTreeMap::new(),
            budget: vec![],
            cancel: false,
        };
        let r = run_turn(
            &spec(TurnKind::Turn),
            &limits(),
            &intel,
            &FakeMcp,
            &mut bridge,
            &log(),
        );
        assert_eq!(r.status, "loop_detected");
        assert_eq!(r.tool_calls, LOOP_REPEATS as u32);
        // Round cap.
        let mut l = limits();
        l.max_rounds = 2;
        let mut s = spec(TurnKind::Turn);
        s.mcp_routes
            .insert("fs.read".into(), ("down".into(), "read".into()));
        let intel = mock_intel(&json!({"turns": [
            {"tool_calls": [{"name": "fs.read", "arguments": {"path": "/a"}}]},
            {"tool_calls": [{"name": "fs.read", "arguments": {"path": "/b"}}]},
            {"tool_calls": [{"name": "fs.read", "arguments": {"path": "/c"}}]}
        ]}));
        let r = run_turn(&s, &l, &intel, &FakeMcp, &mut bridge, &log());
        assert_eq!(r.status, "exhausted_steps");
        assert!(
            matches!(&r.messages[1], Msg::Tool { is_error: true, content, .. } if content.as_str().unwrap().contains("transport error"))
        );
        // Budget: one wait then ok with a degraded model; then a refusal.
        let intel = mock_intel(&json!({"turns": [{"content": "ok"}]}));
        let mut s = spec(TurnKind::Turn);
        s.budget_admission = true;
        let mut b = FakeBridge {
            calls: vec![],
            answers: BTreeMap::new(),
            budget: vec![
                BudgetReply {
                    ok: false,
                    wait_ms: 10,
                    model: None,
                    reason: None,
                },
                BudgetReply {
                    ok: true,
                    wait_ms: 0,
                    model: Some("cheap".into()),
                    reason: None,
                },
            ],
            cancel: false,
        };
        let r = run_turn(&s, &limits(), &intel, &FakeMcp, &mut b, &log());
        assert_eq!(r.status, "completed");
        let mut b = FakeBridge {
            calls: vec![],
            answers: BTreeMap::new(),
            budget: vec![BudgetReply {
                ok: false,
                wait_ms: 0,
                model: None,
                reason: Some("budget refused: window exhausted".into()),
            }],
            cancel: false,
        };
        let r = run_turn(&s, &limits(), &intel, &FakeMcp, &mut b, &log());
        assert_eq!(r.status, "refused");
        // Cancel before the first call.
        let mut b = FakeBridge {
            calls: vec![],
            answers: BTreeMap::new(),
            budget: vec![],
            cancel: true,
        };
        let r = run_turn(
            &spec(TurnKind::Turn),
            &limits(),
            &intel,
            &FakeMcp,
            &mut b,
            &log(),
        );
        assert_eq!(r.status, "cancelled");
        assert_eq!(r.rounds, 0);
        // A dead intelligence is `failed` with an intel error.
        let dead = IntelClient::from_parts("http://127.0.0.1:9", None).unwrap();
        let r = run_turn(
            &spec(TurnKind::Turn),
            &limits(),
            &dead,
            &FakeMcp,
            &mut b,
            &log(),
        );
        assert_eq!(r.status, "cancelled", "cancel wins first");
        b.cancel = false;
        let r = run_turn(
            &spec(TurnKind::Turn),
            &limits(),
            &dead,
            &FakeMcp,
            &mut b,
            &log(),
        );
        assert_eq!(r.status, "failed");
        assert!(r.error.as_deref().unwrap().starts_with("intel:"));
    }

    #[test]
    fn an_exit_inside_the_tool_loop_answers_every_call_it_persisted() {
        // Cancellation landing BETWEEN two calls of one assistant message (the
        // other early exit, loop detection, is covered end to end in
        // `wedged_context_e2e`). The delta is appended verbatim to the DURABLE
        // context, so leaving an id unanswered would malform that context for
        // every later turn and every restart — not just lose one result.
        struct CancelAfterOneCall(usize);
        impl Bridge for CancelAfterOneCall {
            fn tool_request(&mut self, _n: &str, _a: &Value, _d: Instant) -> Option<(Value, bool)> {
                self.0 += 1;
                Some((json!({"ok": true}), false))
            }
            fn budget_request(&mut self, _e: u64, _d: Instant) -> Option<BudgetReply> {
                None
            }
            fn cancelled(&self) -> bool {
                self.0 > 0
            }
        }
        // Three DISTINCT calls so loop detection does not fire first.
        let intel = mock_intel(&json!({"turns": [{"tool_calls": [
            {"name": "memory.set", "arguments": {"key": "a"}},
            {"name": "memory.set", "arguments": {"key": "b"}},
            {"name": "memory.set", "arguments": {"key": "c"}}
        ]}]}));
        let mut bridge = CancelAfterOneCall(0);
        let r = run_turn(
            &spec(TurnKind::Turn),
            &limits(),
            &intel,
            &FakeMcp,
            &mut bridge,
            &log(),
        );
        assert_eq!(r.status, "cancelled");
        // assistant + the one executed result + two sealed ones.
        assert_eq!(r.messages.len(), 4, "{:?}", r.messages);
        let answered: Vec<&str> = r
            .messages
            .iter()
            .filter_map(|m| match m {
                Msg::Tool { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        match &r.messages[0] {
            Msg::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 3);
                for tc in tool_calls {
                    assert!(
                        answered.contains(&tc.id.as_str()),
                        "tool_call {} has no result: {:?}",
                        tc.id,
                        r.messages
                    );
                }
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&r.messages[3], Msg::Tool { is_error: true, content, .. }
                if content.as_str().unwrap_or_default().contains("not executed")),
            "the sealed result says the call did not happen: {:?}",
            r.messages[3]
        );
    }

    #[test]
    fn json_answers_are_parsed_tolerantly() {
        assert_eq!(parse_json_answer("{\"a\":1}").unwrap(), json!({"a": 1}));
        assert_eq!(
            parse_json_answer("```json\n{\"a\":1}\n```").unwrap(),
            json!({"a": 1})
        );
        assert_eq!(
            parse_json_answer("Here: {\"a\":1}. Done.").unwrap(),
            json!({"a": 1})
        );
        assert_eq!(parse_json_answer("[1,2]").unwrap(), json!([1, 2]));
        assert!(parse_json_answer("nope").is_err());
        let res = ::mcp::wire::CallToolResult {
            content: vec![json!({"type": "text", "text": "{\"x\": 2}"})],
            is_error: None,
            structured_content: None,
        };
        assert_eq!(tool_result_value(&res), json!({"x": 2}));
        let res = ::mcp::wire::CallToolResult {
            content: vec![json!({"type": "text", "text": "plain"})],
            is_error: None,
            structured_content: Some(json!({"s": 1})),
        };
        assert_eq!(tool_result_value(&res), json!({"s": 1}));
    }
}
