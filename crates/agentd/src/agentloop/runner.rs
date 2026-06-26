//! The ReAct agentic loop. RFC 0007.
//!
//! A turn: assemble the request (system + instruction + transcript + the
//! scoped tool catalogue) → call intelligence → if the model requested tools,
//! run them via MCP and feed the results back as observations; otherwise the
//! text is the final answer. Stopping is a disjunction of cheap checks, each
//! with a distinct [`TerminalStatus`] (RFC 0007 §3.4); the loop enforces the
//! step/token/deadline budget. `stalled`/`loop_detected` detectors and context
//! compaction are deferred (v2); the `Stalled`/`LoopDetected` statuses are
//! defined but not yet produced.
//!
//! The root agent runs as a subagent process behind the control channel
//! (spawned by `main::run_once` via `supervise_once`); the loop body here is
//! identical whether driven by the root or a nested child.

use crate::agentloop::action::SelfHandler;
use crate::agentloop::stop::{Outcome, TerminalStatus};
use crate::intel::client::IntelClient;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::supervisor::budget::Budget;
use crate::wire::intel::{Message, Request, ToolDef};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

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
    /// subagent's control thread on `ControlMsg::Cancel`). `None` for a run with
    /// no external canceller.
    pub cancel: Option<Arc<AtomicBool>>,
}

/// The durable state of an agent session: the scoped tool catalogue, the
/// resource-awareness map, and the **conversation transcript** — everything that
/// persists *across turns*. A once-mode / per-event run is a session of exactly
/// one turn ([`run_loop`]); a **warm** continue-session runs many turns over the
/// same transcript (RFC 0008 §spawn-vs-continue), each new event appended via
/// [`Session::deliver`] before another [`Session::run_turn`].
pub struct Session<'a> {
    servers: &'a [McpClient],
    tools: Vec<ToolDef>,
    tool_to_server: HashMap<String, usize>,
    resources: ResourceCatalogue,
    model: String,
    messages: Vec<Message>,
}

impl<'a> Session<'a> {
    /// Assemble a session: the tool catalogue (MCP tools + self-tools +
    /// `resource.read` when resources exist; resources: list = awareness,
    /// read = on-demand attention, RFC 0007 §resources), the resource awareness
    /// note, and the opening transcript (system prompt + seed + the instruction
    /// as the first user turn).
    pub fn prepare(
        servers: &'a [McpClient],
        input: &LoopInput,
        self_handler: &mut dyn SelfHandler,
    ) -> Result<Session<'a>, LoopAbort> {
        let (mut tools, tool_to_server) = build_catalogue(servers)?;
        tools.extend(self_handler.tools());
        let resources = collect_resources(servers);
        // Offer `resource.read` when there are MCP resources OR the handler
        // serves agentd:// self-resources (e.g. async-child completions).
        if !resources.owner.is_empty() || self_handler.serves_self_resources() {
            tools.push(resource_read_tool_def());
        }
        let mut messages = vec![Message::system(system_prompt(
            input.output_contract.as_deref(),
        ))];
        if let Some(note) = resources.catalogue_note() {
            messages.push(Message::system(note));
        }
        for (role, content) in &input.seed {
            messages.push(seed_message(role, content));
        }
        messages.push(Message::user(&input.instruction));
        Ok(Session {
            servers,
            tools,
            tool_to_server,
            resources,
            model: input.model.clone(),
            messages,
        })
    }

    /// Append the next event as a new user turn — the delivery point for a warm
    /// continue-session (RFC 0008). The transcript (the model's memory of the
    /// session) carries forward, so the next turn continues the conversation.
    pub fn deliver(&mut self, content: &str) {
        self.messages.push(Message::user(content));
    }

    /// Run one turn: the ReAct loop over the persistent transcript until a
    /// terminal status, bounded by `budget`. `cancel` is polled at each turn
    /// boundary. Every assistant/tool message (including the final answer) is
    /// appended to the transcript, so a subsequent turn continues the same
    /// conversation.
    pub fn run_turn(
        &mut self,
        intel: &IntelClient,
        self_handler: &mut dyn SelfHandler,
        log: &Logger,
        budget: &mut Budget,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<Outcome, LoopAbort> {
        let mut last_text: Option<String> = None;
        // otel run trace: the `invoke_agent` span plus a `chat` child per model
        // call and an `execute_tool` child per tool call. No-op without
        // `--features otel`, so the wiring carries no `cfg`. One trace per turn.
        let run_start = crate::obs::otel::now_unix_nanos();
        let mut run_span = crate::obs::otel::run_begin(log.ctx().trace_id.as_deref(), run_start);
        let (mut tok_in, mut tok_out) = (0u64, 0u64);

        log.info(
            "loop.start",
            json!({"tools": self.tools.len(), "servers": self.servers.len(), "resources": self.resources.owner.len(), "max_steps": budget.max_steps()}),
        );

        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                log.warn(
                    "loop.final",
                    json!({"status": "cancelled", "steps": budget.steps()}),
                );
                run_span.finish(&self.model, tok_in, tok_out, false);
                return Ok(Outcome {
                    status: TerminalStatus::Cancelled,
                    partial: last_text.is_some(),
                    result: json!(last_text.unwrap_or_default()),
                    scheduled: self_handler.take_scheduled(),
                    subscriptions: self_handler.take_subscriptions(),
                });
            }
            if let Some(status) = budget.exceeded() {
                log.warn("loop.final", json!({"status": status.as_str(), "steps": budget.steps(), "tokens": budget.tokens()}));
                run_span.finish(&self.model, tok_in, tok_out, false);
                return Ok(Outcome {
                    status,
                    partial: last_text.is_some(),
                    result: json!(last_text.unwrap_or_default()),
                    scheduled: self_handler.take_scheduled(),
                    subscriptions: self_handler.take_subscriptions(),
                });
            }

            // Per-turn audit anchor (RFC 0010 §2.9 `loop.step`): the running
            // budget snapshot at the head of each ReAct turn, distinct from the
            // LLM-call event below.
            log.debug(
                "loop.step",
                json!({"step": budget.steps(), "tokens": budget.tokens(), "messages": self.messages.len()}),
            );

            let req = Request {
                model: self.model.clone(),
                messages: self.messages.clone(),
                tools: self.tools.clone(),
                max_tokens: PER_CALL_MAX_TOKENS,
                temperature: Some(0.0),
            };

            log.debug(
                "intel.call",
                json!({"step": budget.steps(), "messages": self.messages.len()}),
            );
            let chat_start = crate::obs::otel::now_unix_nanos();
            let resp = intel
                .complete(&req)
                .map_err(|e| LoopAbort::Intel(e.to_string()))?;
            budget.record_usage(resp.usage);
            budget.record_step();
            tok_in += resp.usage.input_tokens;
            tok_out += resp.usage.output_tokens;
            run_span.record_chat(
                &self.model,
                resp.usage.input_tokens,
                resp.usage.output_tokens,
                true,
                chat_start,
            );
            log.debug(
                "intel.result",
                json!({"tool_calls": resp.tool_calls.len(), "tokens_in": resp.usage.input_tokens, "tokens_out": resp.usage.output_tokens}),
            );

            if resp.wants_tools() {
                if let Some(t) = resp.text.as_deref().filter(|t| !t.is_empty()) {
                    last_text = Some(t.to_string());
                }
                let tool_calls = resp.tool_calls.clone();
                self.messages.push(Message::Assistant {
                    text: resp.text,
                    tool_calls: tool_calls.clone(),
                });

                for tc in &tool_calls {
                    let mut call = json!({"tool": tc.name, "id": tc.id});
                    // Content capture is opt-in (RFC 0010 §2.9): default logs only
                    // the tool name + length; `--log-content` adds the (truncated)
                    // arguments/result body for debugging.
                    if log.content_capture() {
                        call["args"] = json!(truncate_for_log(&tc.arguments.to_string()));
                    }
                    log.info("tool.call", call);
                    let tool_start = crate::obs::otel::now_unix_nanos();
                    let (content, is_error) = if tc.name == "resource.read" {
                        // An `agentd://` URI reads agentd's own state (e.g. an
                        // async child's completion) via the self-handler; any
                        // other URI is an MCP-server resource.
                        let uri = tc
                            .arguments
                            .get("uri")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim();
                        if crate::agentd_uri::is_agentd(uri) {
                            self_handler.read_resource(uri).unwrap_or_else(|| {
                                (format!("unknown agentd resource: {uri}"), true)
                            })
                        } else {
                            read_resource_tool(self.servers, &self.resources.owner, &tc.arguments)
                        }
                    } else {
                        match self_handler.handle(&tc.name, &tc.arguments) {
                            Some(r) => r, // a self-tool (e.g. subagent.spawn)
                            None => dispatch_tool(
                                self.servers,
                                &self.tool_to_server,
                                &tc.name,
                                &tc.arguments,
                            ),
                        }
                    };
                    run_span.record_tool(&tc.name, !is_error, tool_start);
                    let mut result =
                        json!({"tool": tc.name, "is_error": is_error, "bytes": content.len()});
                    if log.content_capture() {
                        result["content"] = json!(truncate_for_log(&content));
                    }
                    log.info("tool.result", result);
                    self.messages
                        .push(Message::tool_result(&tc.id, content, is_error));
                }
                continue;
            }

            // No tool calls → the model's text is the final answer for this turn.
            // Record it in the transcript so a warm session's next turn sees its
            // own prior reply (invisible to once-mode, which discards the session).
            let text = resp.text.clone().or(last_text).unwrap_or_default();
            self.messages.push(Message::Assistant {
                text: Some(text.clone()),
                tool_calls: Vec::new(),
            });
            log.info(
                "loop.final",
                json!({"status": "completed", "steps": budget.steps(), "tokens": budget.tokens()}),
            );
            run_span.finish(&self.model, tok_in, tok_out, true);
            return Ok(Outcome {
                status: TerminalStatus::Completed,
                partial: false,
                result: json!(text),
                scheduled: self_handler.take_scheduled(),
                subscriptions: self_handler.take_subscriptions(),
            });
        }
    }
}

/// The agentic loop over explicit inputs — one session, one turn. Used by
/// once-mode (`run_root`) and a per-event subagent run (`subagent::control`).
/// `self_handler` supplies agentd's in-process self-tools (e.g. `subagent.spawn`);
/// the loop tries it before MCP. A warm continue-session instead drives
/// [`Session`] directly across many turns.
pub fn run_loop(
    intel: &IntelClient,
    servers: &[McpClient],
    input: &LoopInput,
    self_handler: &mut dyn SelfHandler,
    log: &Logger,
) -> Result<Outcome, LoopAbort> {
    let mut session = Session::prepare(servers, input, self_handler)?;
    let mut budget = Budget::new(input.max_steps, input.max_tokens, input.deadline);
    session.run_turn(intel, self_handler, log, &mut budget, input.cancel.as_ref())
}

/// Max characters of tool content recorded under `--log-content`. Bounds a log
/// line so a large tool body can't bloat the telemetry stream; the full body
/// still flows to the model as the observation.
const CONTENT_LOG_CAP: usize = 4096;

/// Truncate a body for content-capture logging, appending a byte-count marker
/// when clipped. Char-based so a multi-byte boundary is never split.
fn truncate_for_log(s: &str) -> String {
    if s.chars().count() <= CONTENT_LOG_CAP {
        return s.to_string();
    }
    let mut t: String = s.chars().take(CONTENT_LOG_CAP).collect();
    t.push_str(&format!(
        "…(+{} more bytes)",
        s.len().saturating_sub(t.len())
    ));
    t
}

/// Build the model's tool catalogue from every connected server, plus a
/// name→server-index routing map. On a name collision the first server wins
/// (logged at call time as "unknown" only if truly absent). RFC 0004.
fn build_catalogue(
    servers: &[McpClient],
) -> Result<(Vec<ToolDef>, HashMap<String, usize>), LoopAbort> {
    let mut tools = Vec::new();
    let mut routing = HashMap::new();
    for (i, server) in servers.iter().enumerate() {
        let listed = server
            .list_tools()
            .map_err(|e| LoopAbort::Mcp(e.to_string()))?;
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
/// adapt; a wedged server is caught by the budget (RFC 0004 §isError).
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
        "assistant" => Message::Assistant {
            text: Some(content.to_string()),
            tool_calls: Vec::new(),
        },
        _ => Message::user(content),
    }
}

/// Cap on the injected resource catalogue (URIs only; bodies are pulled on
/// demand). A server exposing thousands is truncated with a note.
const RESOURCE_CAP: usize = 50;

/// The compact resource awareness catalogue + a uri→owning-server map for
/// `resource.read`. RFC 0007 §resources.
struct ResourceCatalogue {
    owner: HashMap<String, usize>,
    entries: Vec<(String, String)>, // (uri, label)
    truncated: bool,
}

impl ResourceCatalogue {
    /// The system note listing readable resources (never their bodies).
    fn catalogue_note(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let mut s = String::from(
            "Available MCP resources — read the current content of any with the resource.read tool:\n",
        );
        for (uri, label) in &self.entries {
            if label.is_empty() {
                s.push_str(&format!("- {uri}\n"));
            } else {
                s.push_str(&format!("- {uri} — {label}\n"));
            }
        }
        if self.truncated {
            s.push_str(&format!(
                "(… more than {RESOURCE_CAP} resources; list truncated)\n"
            ));
        }
        Some(s)
    }
}

/// List resources from every server (first owner wins for a duplicate URI),
/// capped. `resources/list` is capability-gated in the client (empty if unsupported).
fn collect_resources(servers: &[McpClient]) -> ResourceCatalogue {
    let mut owner = HashMap::new();
    let mut entries = Vec::new();
    let mut truncated = false;
    'outer: for (i, s) in servers.iter().enumerate() {
        let Ok(list) = s.list_resources() else {
            continue;
        };
        for r in list {
            if entries.len() >= RESOURCE_CAP {
                truncated = true;
                break 'outer;
            }
            if !owner.contains_key(&r.uri) {
                let label = r.title.or(r.name).or(r.description).unwrap_or_default();
                owner.insert(r.uri.clone(), i);
                entries.push((r.uri, label));
            }
        }
    }
    ResourceCatalogue {
        owner,
        entries,
        truncated,
    }
}

fn resource_read_tool_def() -> ToolDef {
    ToolDef {
        name: "resource.read".into(),
        description: "Read the current content of an available MCP resource by its uri (see the \
            resource catalogue). Use this to pull a resource's body when you need it."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {"uri": {"type": "string", "description": "the resource uri to read"}},
            "required": ["uri"]
        }),
    }
}

/// Handle a `resource.read` call against the connected servers: read from the
/// owning server (or try each), returning the text as the observation.
fn read_resource_tool(
    servers: &[McpClient],
    owner: &HashMap<String, usize>,
    args: &Value,
) -> (String, bool) {
    let uri = args.get("uri").and_then(Value::as_str).unwrap_or("").trim();
    if uri.is_empty() {
        return ("error: resource.read requires a 'uri'".into(), true);
    }
    let candidates: Vec<usize> = match owner.get(uri) {
        Some(i) => vec![*i],
        None => (0..servers.len()).collect(), // a templated/unlisted uri — try all
    };
    for i in candidates {
        if let Ok(r) = servers[i].read_resource(uri) {
            return (r.text(), false);
        }
    }
    (format!("resource.read: no server could read '{uri}'"), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_catalogue_note_lists_uris() {
        let c = ResourceCatalogue {
            owner: HashMap::new(),
            entries: vec![
                ("file:///a.json".into(), "inbox".into()),
                ("db://orders".into(), String::new()),
            ],
            truncated: false,
        };
        let note = c.catalogue_note().unwrap();
        assert!(note.contains("resource.read"));
        assert!(note.contains("file:///a.json — inbox"));
        assert!(note.contains("- db://orders\n"));
    }

    #[test]
    fn empty_catalogue_is_no_note() {
        let c = ResourceCatalogue {
            owner: HashMap::new(),
            entries: vec![],
            truncated: false,
        };
        assert!(c.catalogue_note().is_none());
    }

    #[test]
    fn resource_read_rejects_missing_uri() {
        let (msg, err) = read_resource_tool(&[], &HashMap::new(), &json!({}));
        assert!(err);
        assert!(msg.contains("uri"));
    }

    #[test]
    fn resource_read_no_server_is_an_error_observation() {
        let (msg, err) = read_resource_tool(&[], &HashMap::new(), &json!({"uri": "file:///x"}));
        assert!(err);
        assert!(msg.contains("file:///x"));
    }

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

    #[test]
    fn truncate_for_log_caps_and_marks() {
        let short = "{\"a\":1}";
        assert_eq!(truncate_for_log(short), short); // under the cap: verbatim
        let big = "x".repeat(CONTENT_LOG_CAP + 500);
        let out = truncate_for_log(&big);
        assert!(out.len() < big.len());
        assert!(
            out.contains("more bytes"),
            "truncation is marked: {}",
            &out[out.len() - 32..]
        );
        // multi-byte safety: never panics on a char boundary
        let multi = "é".repeat(CONTENT_LOG_CAP + 10);
        let _ = truncate_for_log(&multi);
    }
}
