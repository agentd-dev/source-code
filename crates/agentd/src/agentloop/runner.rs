//! The ReAct agentic loop. RFC 0007.
//!
//! A turn: assemble the request (system + instruction + transcript + the
//! scoped tool catalogue) → call intelligence → if the model requested tools,
//! run them via MCP and feed the results back as observations; otherwise the
//! text is the final answer. Stopping is a disjunction of cheap checks, each
//! with a distinct [`TerminalStatus`] (RFC 0007 §3.4); v1 enforces the
//! step/token/deadline budget. `stalled`/`loop_detected` detectors and context
//! compaction land in later milestones.
//!
//! M1 runs the **root** agent in-process. M2 moves this into a subagent
//! process behind the control channel; the loop body is unchanged.

use crate::agentloop::stop::{Outcome, TerminalStatus};
use crate::config::Config;
use crate::intel::client::IntelClient;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::supervisor::budget::Budget;
use crate::wire::intel::{Message, Request, ToolDef};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-response token cap (distinct from the cumulative run budget).
const PER_CALL_MAX_TOKENS: u32 = 4096;

const SYSTEM_PROMPT: &str = "You are agentd, an autonomous agent. Accomplish the user's \
instruction by calling the available tools and reasoning over their results. Call a tool when you \
need information or need to act. When the task is complete, reply with your final answer and do \
NOT call a tool. If the task cannot be done, say so plainly. Be concise and factual.";

/// A fatal infrastructure failure that aborts the run (mapped to exit 4 / 6 by
/// the caller, RFC 0011). Tool-domain errors are *not* aborts — they are fed
/// back to the model as observations.
#[derive(Debug)]
pub enum LoopAbort {
    /// The intelligence endpoint is unreachable / erroring (exit 4).
    Intel(String),
    /// A required MCP server failed (exit 6).
    Mcp(String),
}

impl fmt::Display for LoopAbort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoopAbort::Intel(m) => write!(f, "intelligence: {m}"),
            LoopAbort::Mcp(m) => write!(f, "mcp: {m}"),
        }
    }
}

/// The explicit inputs the loop needs, independent of where they came from
/// (CLI `Config` for once-mode, or a `SpawnPayload` for a subagent). This is
/// the seam that lets the same loop body run in-process or in a child.
pub struct LoopInput {
    pub instruction: String,
    pub output_contract: Option<String>,
    /// Narrowed context seed as (role, content) pairs (role ∈
    /// system|user|assistant|tool).
    pub seed: Vec<(String, String)>,
    pub model: String,
    pub max_steps: u32,
    pub max_tokens: u64,
    pub deadline: Instant,
    /// A cooperative cancel flag checked at each turn boundary (set by a
    /// subagent's control thread on `ControlMsg::Cancel`). `None` for
    /// in-process once-mode.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl LoopInput {
    /// Build the once-mode input from CLI config.
    pub fn from_config(cfg: &Config) -> LoopInput {
        let deadline = cfg
            .deadline
            .map(|d| Instant::now() + d)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(10 * 365 * 24 * 3600));
        LoopInput {
            instruction: cfg.instruction.clone().unwrap_or_default(),
            output_contract: None,
            seed: Vec::new(),
            model: cfg.model.clone().unwrap_or_default(),
            max_steps: cfg.max_steps,
            max_tokens: cfg.max_tokens,
            deadline,
            cancel: None,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    }
}

/// Run the agent loop to a terminal status (in-process, once-mode entry).
pub fn run_root(
    intel: &IntelClient,
    servers: &[McpClient],
    cfg: &Config,
    log: &Logger,
) -> Result<Outcome, LoopAbort> {
    run_loop(intel, servers, &LoopInput::from_config(cfg), log)
}

/// The agentic loop over explicit inputs. Used by once-mode (`run_root`) and
/// by a subagent process (`subagent::control`).
pub fn run_loop(
    intel: &IntelClient,
    servers: &[McpClient],
    input: &LoopInput,
    log: &Logger,
) -> Result<Outcome, LoopAbort> {
    let (tools, tool_to_server) = build_catalogue(servers)?;

    let mut budget = Budget::new(input.max_steps, input.max_tokens, input.deadline);

    let mut messages = vec![Message::system(system_prompt(input.output_contract.as_deref()))];
    for (role, content) in &input.seed {
        messages.push(seed_message(role, content));
    }
    messages.push(Message::user(&input.instruction));
    let mut last_text: Option<String> = None;
    let model = input.model.clone();

    log.info(
        "loop.start",
        json!({"tools": tools.len(), "servers": servers.len(), "max_steps": input.max_steps}),
    );

    loop {
        if input.cancelled() {
            log.warn("loop.final", json!({"status": "cancelled", "steps": budget.steps()}));
            return Ok(Outcome {
                status: TerminalStatus::Cancelled,
                partial: last_text.is_some(),
                result: json!(last_text.unwrap_or_default()),
            });
        }
        if let Some(status) = budget.exceeded() {
            log.warn("loop.final", json!({"status": status.as_str(), "steps": budget.steps(), "tokens": budget.tokens()}));
            return Ok(Outcome {
                status,
                partial: last_text.is_some(),
                result: json!(last_text.unwrap_or_default()),
            });
        }

        let req = Request {
            model: model.clone(),
            messages: messages.clone(),
            tools: tools.clone(),
            max_tokens: PER_CALL_MAX_TOKENS,
            temperature: Some(0.0),
        };

        log.debug("intel.call", json!({"step": budget.steps(), "messages": messages.len()}));
        let resp = intel.complete(&req).map_err(|e| LoopAbort::Intel(e.to_string()))?;
        budget.record_usage(resp.usage);
        budget.record_step();
        log.debug(
            "intel.result",
            json!({"tool_calls": resp.tool_calls.len(), "tokens_in": resp.usage.input_tokens, "tokens_out": resp.usage.output_tokens}),
        );

        if resp.wants_tools() {
            if let Some(t) = resp.text.as_deref().filter(|t| !t.is_empty()) {
                last_text = Some(t.to_string());
            }
            let tool_calls = resp.tool_calls.clone();
            messages.push(Message::Assistant { text: resp.text, tool_calls: tool_calls.clone() });

            for tc in &tool_calls {
                log.info("tool.call", json!({"tool": tc.name, "id": tc.id}));
                let (content, is_error) = dispatch_tool(servers, &tool_to_server, &tc.name, &tc.arguments);
                log.info("tool.result", json!({"tool": tc.name, "is_error": is_error, "bytes": content.len()}));
                messages.push(Message::tool_result(&tc.id, content, is_error));
            }
            continue;
        }

        // No tool calls → the model's text is the final answer.
        let text = resp.text.clone().or(last_text).unwrap_or_default();
        log.info("loop.final", json!({"status": "completed", "steps": budget.steps(), "tokens": budget.tokens()}));
        return Ok(Outcome { status: TerminalStatus::Completed, partial: false, result: json!(text) });
    }
}

/// Build the model's tool catalogue from every connected server, plus a
/// name→server-index routing map. On a name collision the first server wins
/// (logged at call time as "unknown" only if truly absent). RFC 0004.
fn build_catalogue(servers: &[McpClient]) -> Result<(Vec<ToolDef>, HashMap<String, usize>), LoopAbort> {
    let mut tools = Vec::new();
    let mut routing = HashMap::new();
    for (i, server) in servers.iter().enumerate() {
        let listed = server.list_tools().map_err(|e| LoopAbort::Mcp(e.to_string()))?;
        for t in listed {
            routing.entry(t.name.clone()).or_insert(i);
            tools.push(ToolDef {
                name: t.name,
                description: t.description.unwrap_or_default(),
                input_schema: t.input_schema,
            });
        }
    }
    Ok((tools, routing))
}

/// Route one tool call to its owning server. A transport error is returned as
/// an error *observation* (is_error = true), not an abort — the model can
/// adapt; a wedged server is caught by the budget. (M2/M3 refine the
/// abort-vs-observe policy per RFC 0004 §isError.)
fn dispatch_tool(
    servers: &[McpClient],
    routing: &HashMap<String, usize>,
    name: &str,
    arguments: &Value,
) -> (String, bool) {
    match routing.get(name) {
        Some(&i) => match servers[i].call_tool(name, Some(arguments.clone())) {
            Ok(res) => (res.text(), res.is_error()),
            Err(e) => (format!("tool transport error: {e}"), true),
        },
        None => (format!("error: no such tool '{name}'"), true),
    }
}

/// The system prompt, optionally appended with the delegation output contract
/// (RFC 0009 §spawn-payload).
fn system_prompt(contract: Option<&str>) -> String {
    match contract {
        Some(c) if !c.is_empty() => format!("{SYSTEM_PROMPT}\n\nOutput contract:\n{c}"),
        _ => SYSTEM_PROMPT.to_string(),
    }
}

/// Map a seed (role, content) pair to a loop message. A `tool` seed has no
/// tool-call id to replay against, so it degrades to a user note.
fn seed_message(role: &str, content: &str) -> Message {
    match role {
        "system" => Message::system(content),
        "assistant" => Message::Assistant { text: Some(content.to_string()), tool_calls: Vec::new() },
        _ => Message::user(content),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_appends_contract() {
        let p = system_prompt(Some("Return JSON."));
        assert!(p.contains("Output contract:"));
        assert!(p.contains("Return JSON."));
        assert_eq!(system_prompt(None), SYSTEM_PROMPT);
    }

    #[test]
    fn dispatch_unknown_tool_is_error_observation() {
        let routing = HashMap::new();
        let (content, is_error) = dispatch_tool(&[], &routing, "ghost", &Value::Null);
        assert!(is_error);
        assert!(content.contains("ghost"));
    }

    #[test]
    fn loop_abort_display() {
        assert!(LoopAbort::Intel("down".into()).to_string().contains("down"));
    }
}
