// SPDX-License-Identifier: Apache-2.0
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

use crate::agentloop::action::{SelfHandler, ToolClass};
use crate::agentloop::stop::{Outcome, TerminalStatus};
use crate::intel::client::IntelClient;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::supervisor::budget::Budget;
use crate::wire::intel::{Message, Request, ToolDef, Usage};
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

    /// Rebuild the MCP side of the tool catalogue from the servers' CURRENT
    /// `tools/list` (the warm-session LIVE refresh, pivot Phase 7 follow-up):
    /// called at a turn boundary after an inbound
    /// `notifications/tools/list_changed`, so a long-lived continue-session
    /// tracks a server whose tool set changed instead of holding a stale
    /// catalogue for its whole life. Self-tools and `resource.read` are
    /// re-merged; the transcript is untouched.
    pub fn refresh_tools(&mut self, self_handler: &mut dyn SelfHandler) -> Result<(), LoopAbort> {
        let (mut tools, tool_to_server) = build_catalogue(self.servers)?;
        tools.extend(self_handler.tools());
        if !self.resources.owner.is_empty() || self_handler.serves_self_resources() {
            tools.push(resource_read_tool_def());
        }
        self.tools = tools;
        self.tool_to_server = tool_to_server;
        Ok(())
    }

    /// The current catalogue size (observability for the live refresh).
    pub fn tools_len(&self) -> usize {
        self.tools.len()
    }

    /// Classify a catalogue tool by its seam (pivot Phase 5.1 — name the class): a
    /// name routed to an MCP server is [`ToolClass::Mcp`] (dispatched back to that
    /// server); every other catalogue entry is agentd's own
    /// [`ToolClass::SelfControl`] surface (the self-tools + `resource.read`). The
    /// routing map IS the MCP-tool set — the two classes are assembled by different
    /// code paths ([`build_catalogue`] vs the [`SelfHandler`] merge) — so this is
    /// the authoritative, testable boundary between "tools from a registered server"
    /// and "agentd's own orchestration primitives". Callers pass a name from
    /// [`Session::tools`]; a name absent from the catalogue still classifies as
    /// `SelfControl` (it is, by definition, not a routed server tool), so classify
    /// only names drawn from the catalogue.
    pub fn tool_class(&self, name: &str) -> ToolClass {
        if self.tool_to_server.contains_key(name) {
            ToolClass::Mcp
        } else {
            ToolClass::SelfControl
        }
    }

    /// Append the next event as a new user turn — the delivery point for a warm
    /// continue-session (RFC 0008). The transcript (the model's memory of the
    /// session) carries forward, so the next turn continues the conversation.
    pub fn deliver(&mut self, content: &str) {
        self.messages.push(Message::user(content));
    }

    /// Adopt a new model for subsequent turns (RFC 0018 §5.3 model hot-swap). The
    /// transcript is UNTOUCHED — only the model dialed for the NEXT turn changes;
    /// a turn already in flight completes on the old model (finish-on-old). The
    /// `model` is what each request's `model` field carries.
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    /// The current model dialed for the next turn (RFC 0018 §5.3) — used to detect
    /// whether a swap actually changed the model (a repoint with no model change is
    /// always finish-on-old / invisible, §5.1).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The number of transcript messages so far — a cheap pre-turn marker for the
    /// `restart-turn` policy (RFC 0018 §5.3): snapshot this before a turn, then
    /// [`truncate_transcript`](Session::truncate_transcript) back to it to discard
    /// the swapped-turn's appended messages and re-run from the same pre-turn state.
    pub fn transcript_len(&self) -> usize {
        self.messages.len()
    }

    /// Truncate the transcript back to `len` (RFC 0018 §5.3 `restart-turn`): drop
    /// every message a discarded turn appended, restoring the exact pre-turn
    /// transcript so the turn can be re-run on the new model. A no-op if `len`
    /// already ≥ the current length (never grows the transcript).
    pub fn truncate_transcript(&mut self, len: usize) {
        if len < self.messages.len() {
            self.messages.truncate(len);
        }
    }

    /// Run one turn: the ReAct loop over the persistent transcript until a
    /// terminal status, bounded by `budget`. `cancel` is polled at each turn
    /// boundary. Every assistant/tool message (including the final answer) is
    /// appended to the transcript, so a subsequent turn continues the same
    /// conversation.
    ///
    /// Returns the turn's [`Outcome`] together with the turn's token [`Usage`]
    /// (the sum of every model call in this turn — `input_tokens`/`output_tokens`).
    /// The control layer rolls this DELTA up to the supervisor as
    /// [`crate::subagent::protocol::AgentMsg::Usage`] so hierarchical token
    /// accounting (`agentd_tokens_total`) is non-zero — but the loop itself never
    /// touches the control channel (the `up` handle stays in `control.rs`).
    pub fn run_turn(
        &mut self,
        intel: &IntelClient,
        self_handler: &mut dyn SelfHandler,
        log: &Logger,
        budget: &mut Budget,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<(Outcome, Usage), LoopAbort> {
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
                return Ok((
                    Outcome {
                        status: TerminalStatus::Cancelled,
                        partial: last_text.is_some(),
                        result: json!(last_text.unwrap_or_default()),
                        scheduled: self_handler.take_scheduled(),
                        subscriptions: self_handler.take_subscriptions(),
                    },
                    Usage {
                        input_tokens: tok_in,
                        output_tokens: tok_out,
                    },
                ));
            }
            if let Some(status) = budget.exceeded() {
                log.warn("loop.final", json!({"status": status.as_str(), "steps": budget.steps(), "tokens": budget.tokens()}));
                run_span.finish(&self.model, tok_in, tok_out, false);
                return Ok((
                    Outcome {
                        status,
                        partial: last_text.is_some(),
                        result: json!(last_text.unwrap_or_default()),
                        scheduled: self_handler.take_scheduled(),
                        subscriptions: self_handler.take_subscriptions(),
                    },
                    Usage {
                        input_tokens: tok_in,
                        output_tokens: tok_out,
                    },
                ));
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
            return Ok((
                Outcome {
                    status: TerminalStatus::Completed,
                    partial: false,
                    result: json!(text),
                    scheduled: self_handler.take_scheduled(),
                    subscriptions: self_handler.take_subscriptions(),
                },
                Usage {
                    input_tokens: tok_in,
                    output_tokens: tok_out,
                },
            ));
        }
    }
}

/// The agentic loop over explicit inputs — one session, one turn. Used by
/// once-mode (`run_root`) and a per-event subagent run (`subagent::control`).
/// `self_handler` supplies agentd's in-process self-tools (e.g. `subagent.spawn`);
/// the loop tries it before MCP. A warm continue-session instead drives
/// [`Session`] directly across many turns.
///
/// Returns the run's [`Outcome`] together with the run's total token [`Usage`].
/// A one-shot run is exactly one turn, so the run total IS that turn's usage;
/// the control layer emits it once per run as a single
/// [`crate::subagent::protocol::AgentMsg::Usage`] (no double-count).
pub fn run_loop(
    intel: &IntelClient,
    servers: &[McpClient],
    input: &LoopInput,
    self_handler: &mut dyn SelfHandler,
    log: &Logger,
) -> Result<(Outcome, Usage), LoopAbort> {
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
    fn catalogue_partitions_into_mcp_and_self_control_classes() {
        use crate::agentloop::action::SELF_CONTROL_TOOLS;
        // A catalogue: two MCP-server tools (routed) + agentd's full self/control
        // surface + resource.read. Every entry must classify into exactly one class
        // (pivot Phase 5.1): the MCP side is precisely the routed set; the rest is
        // agentd's own control surface — and no self/control tool is a local-exec
        // primitive (principle 2).
        let mcp = ["db.query", "http.get"];
        let mut tool_to_server = HashMap::new();
        let mut tools: Vec<ToolDef> = Vec::new();
        for n in mcp {
            tool_to_server.insert(n.to_string(), 0usize);
            tools.push(ToolDef {
                name: n.into(),
                description: String::new(),
                input_schema: json!({}),
            });
        }
        // The full self/control surface a root handler with peers advertises, plus
        // the runner-added resource.read — i.e. the whole named class.
        for n in SELF_CONTROL_TOOLS {
            tools.push(ToolDef {
                name: (*n).into(),
                description: String::new(),
                input_schema: json!({}),
            });
        }
        let sess = Session {
            servers: &[],
            tools,
            tool_to_server,
            resources: ResourceCatalogue {
                owner: HashMap::new(),
                entries: vec![],
                truncated: false,
            },
            model: "m".into(),
            messages: vec![],
        };
        // Routed names → Mcp; every self/control name → SelfControl.
        for n in mcp {
            assert_eq!(sess.tool_class(n), ToolClass::Mcp, "{n} is an MCP tool");
        }
        for n in SELF_CONTROL_TOOLS {
            assert_eq!(
                sess.tool_class(n),
                ToolClass::SelfControl,
                "{n} is self/control"
            );
        }
        // The two classes EXACTLY cover the catalogue (no unclassified tool).
        let (mut n_mcp, mut n_self) = (0usize, 0usize);
        for t in &sess.tools {
            match sess.tool_class(&t.name) {
                ToolClass::Mcp => n_mcp += 1,
                ToolClass::SelfControl => n_self += 1,
            }
        }
        assert_eq!(n_mcp, mcp.len(), "every MCP tool classified");
        assert_eq!(n_self, SELF_CONTROL_TOOLS.len(), "every self tool classified");
        // Principle 2: the self/control class holds NO local-exec primitive.
        for bad in [
            "exec", "shell", "bash", "sh", "command", "system", "eval", "run",
        ] {
            assert!(
                !SELF_CONTROL_TOOLS.contains(&bad),
                "no local-exec self-tool: {bad}"
            );
        }
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

    // ---- the run_turn / run_loop token-usage producer (metrics-honesty) ----
    //
    // `run_turn`/`run_loop` now return the turn's/run's `Usage` so `control.rs`
    // can roll it up to the supervisor as `AgentMsg::Usage` — the missing PRODUCER
    // half of the producer→consumer→`agentd_tokens_total` chain. These drive the
    // *real* loop against the built-in mock LLM (over a unix socket) and assert the
    // returned `Usage` carries the model's reported tokens. The consumer→counter
    // half is covered by the `obs::metrics` `record_tokens` tests and (end to end)
    // by the reactive `/metrics` scrape in `reactive_e2e`.
    #[test]
    fn refresh_tools_picks_up_a_changed_handler_catalogue() {
        // A handler whose advertised tool set CHANGES between turns: refresh
        // rebuilds the catalogue in place (the live warm-session refresh) and
        // leaves the transcript untouched.
        struct GrowingHandler {
            grown: bool,
        }
        impl SelfHandler for GrowingHandler {
            fn tools(&self) -> Vec<ToolDef> {
                let mut t = vec![ToolDef {
                    name: "alpha".into(),
                    description: String::new(),
                    input_schema: Value::Null,
                }];
                if self.grown {
                    t.push(ToolDef {
                        name: "beta".into(),
                        description: String::new(),
                        input_schema: Value::Null,
                    });
                }
                t
            }
            fn handle(&mut self, _name: &str, _args: &Value) -> Option<(String, bool)> {
                None
            }
        }
        let input = LoopInput {
            instruction: "x".into(),
            output_contract: None,
            seed: Vec::new(),
            model: "m".into(),
            max_steps: 5,
            max_tokens: 1000,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
            cancel: None,
        };
        let mut handler = GrowingHandler { grown: false };
        let mut session = Session::prepare(&[], &input, &mut handler).unwrap();
        let before = session.tools_len();
        let transcript = session.transcript_len();
        handler.grown = true;
        session.refresh_tools(&mut handler).unwrap();
        assert_eq!(session.tools_len(), before + 1, "the new tool is live");
        assert_eq!(session.transcript_len(), transcript, "transcript untouched");
        // And the class boundary still holds: a self-tool is SelfControl.
        assert_eq!(session.tool_class("beta"), ToolClass::SelfControl);
    }

    #[cfg(unix)]
    mod usage_producer {
        use super::*;
        use crate::intel::client::IntelClient;
        use crate::obs::log::{Comp, Level, LogCtx, Logger};
        use std::time::{Duration, Instant};

        /// A SelfHandler that advertises no self-tools and handles nothing — the
        /// loop falls through to MCP (here: no servers), so a `final` script's
        /// answer ends the turn at once.
        struct NoopHandler;
        impl SelfHandler for NoopHandler {
            fn tools(&self) -> Vec<ToolDef> {
                Vec::new()
            }
            fn handle(&mut self, _name: &str, _args: &Value) -> Option<(String, bool)> {
                None
            }
        }

        fn test_log() -> Logger {
            Logger::new(
                LogCtx {
                    run_id: "r".into(),
                    agent_id: "0".into(),
                    agent_path: "0".into(),
                    comp: Comp::Agent,
                    pid: 0,
                    trace_id: None,
                },
                Level::Error, // keep the test quiet
            )
        }

        /// Spawn the built-in mock LLM on `socket` with `script`, blocking until it
        /// binds (so the first `complete()` connects, not races).
        /// Run the in-process mock LLM, announcing through `addr_file`; returns
        /// the `http://<addr>` intelligence URL once announced.
        fn start_mock_llm(addr_file: &std::path::Path, script: &'static str) -> String {
            let s = addr_file.to_str().unwrap().to_string();
            std::thread::spawn(move || {
                crate::intel::mock::run(&s, script);
            });
            let deadline = Instant::now() + Duration::from_secs(3);
            while !addr_file.exists() {
                assert!(Instant::now() < deadline, "mock-llm never announced");
                std::thread::sleep(Duration::from_millis(10));
            }
            let addr = std::fs::read_to_string(addr_file).expect("read mock-llm addr-file");
            format!("http://{}", addr.trim())
        }

        fn input(instruction: &str) -> LoopInput {
            LoopInput {
                instruction: instruction.into(),
                output_contract: None,
                seed: Vec::new(),
                model: "mock".into(),
                max_steps: 8,
                max_tokens: 100_000,
                deadline: Instant::now() + Duration::from_secs(10),
                cancel: None,
            }
        }

        #[test]
        fn run_turn_returns_the_turns_token_usage() {
            // The `final` script answers in one model call reporting
            // usage{prompt_tokens: 11, completion_tokens: 5} (intel::mock). The turn
            // surfaces exactly that split, NON-zero — the value control.rs emits up.
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("llm.addr");
            let url = start_mock_llm(&sock, "final");

            let intel = IntelClient::from_parts(&url, None).unwrap();
            let inp = input("do the thing");
            let mut handler = NoopHandler;
            let mut session = Session::prepare(&[], &inp, &mut handler).unwrap();
            let mut budget = Budget::new(inp.max_steps, inp.max_tokens, inp.deadline);

            let (outcome, usage) = session
                .run_turn(&intel, &mut handler, &test_log(), &mut budget, None)
                .expect("turn runs against the mock LLM");

            assert_eq!(outcome.status, TerminalStatus::Completed);
            // The producer half: the turn's reported tokens, non-zero, so the
            // AgentMsg::Usage control.rs sends carries real tokens (not silent 0).
            assert_eq!(
                usage.input_tokens, 11,
                "input tokens surfaced from the model"
            );
            assert_eq!(
                usage.output_tokens, 5,
                "output tokens surfaced from the model"
            );
            assert!(usage.total() > 0, "the rolled-up Usage is non-zero");
        }

        #[test]
        fn run_loop_returns_the_runs_total_token_usage() {
            // The one-shot path: run_loop is a single turn, so its returned Usage IS
            // that turn's usage — one Usage per run (no double-count). The `read`
            // script makes a tool call then answers: two model calls, so the run
            // total SUMS both turns' tokens (each reports 11 in; 7 then 5 out).
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("llm.addr");
            let url = start_mock_llm(&sock, "read");

            let intel = IntelClient::from_parts(&url, None).unwrap();
            let inp = input("read the resource");
            let mut handler = NoopHandler;

            let (outcome, usage) =
                run_loop(&intel, &[], &inp, &mut handler, &test_log()).expect("one-shot run");

            assert_eq!(outcome.status, TerminalStatus::Completed);
            // Two model calls in the run (tool call then final answer) — the run
            // total accumulates both, proving run_loop sums across its turns' calls.
            assert_eq!(usage.input_tokens, 22, "summed input over both model calls");
            assert_eq!(
                usage.output_tokens, 12,
                "summed output over both model calls"
            );
        }
    }
}
