// SPDX-License-Identifier: Apache-2.0
//! The real, Session-backed [`GraphExec`] (pivot Phase 7 · P6) — the adapter that
//! runs a graph's effectful nodes against LIVE intelligence + MCP servers, as opposed
//! to the scripted mock the driver's unit tests use. This is where the workflow stops
//! being a pure state machine and starts doing real work:
//!   * an `Agent` node runs a full ReAct turn ([`run_loop`]) on its instruction, with
//!     the requested blackboard `reads` folded into its context;
//!   * a `Tool` node calls one MCP tool on the named server directly;
//!   * a Tier-2 `Branch` judgement is a single tool-less `complete()`;
//!   * a `Subgraph` runs inline (sync) with the same executor.
//!
//! It touches NO process-supervision code — it composes the existing loop + MCP client
//! + intelligence client, so a workflow inherits their transport/auth/resilience.

use super::{
    drive, drive_budgeted, resume, Blackboard, DriveResult, FieldType, Graph, GraphExec,
    GraphOutcome, GraphStatus, WaitOutcome,
};
use crate::agentloop::action::SelfHandler;
use crate::agentloop::runner::{run_loop, LoopInput};
use crate::agentloop::stop::TerminalStatus;
use crate::config::McpServerSpec;
use crate::intel::client::IntelClient;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::wire::intel::{Message, Request, ToolDef};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The graph run's total node-visit cap (layer-1 backstop) — generous, since a graph
/// is many node visits; the graph's own budget/validation/termination bound the walk.
pub const GRAPH_MAX_STEPS: u32 = 10_000;

/// The completion cap for one `Infer` ask — structured extraction answers are small
/// by construction; a runaway answer is truncated, fails the parse, and re-asks.
const INFER_MAX_TOKENS: u32 = 2048;

/// Parse a model's structured answer as JSON, tolerating the common wrappers: a
/// leading/trailing code fence and prose around the object (first `{` … last `}`).
/// Strictly parsed first, so a clean answer costs nothing extra.
fn parse_json_answer(answer: &str) -> Result<Value, String> {
    let t = answer.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}'))
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

/// Drive a pinned/stored graph SYNCHRONOUSLY to a terminal [`GraphOutcome`] against
/// freshly-connected intelligence + MCP servers (pivot Phase 7 · P6). The single
/// execution path shared by BOTH the operator `--mode graph` entry and the
/// agent-authored `workflow.run` self-tool, so they behave identically. `Err` is a setup
/// failure (unreachable intel / a failed MCP handshake), surfaced by the caller as a
/// usage/tool error. A `Wait` node is handled IN-PROCESS here — subscribe to its uri,
/// block until the resource updates or the timeout elapses (bounded by the graph
/// budget across all waits), then read + resume — so a synchronous graph can pause on
/// a dependency without a daemon. The connected servers live for the whole drive.
#[allow(clippy::too_many_arguments)]
pub fn drive_pinned(
    graph: &Graph,
    intel_uri: &str,
    intel_token: Option<String>,
    model: &str,
    server_specs: &[McpServerSpec],
    max_steps: u32,
    max_tokens: u64,
    node_timeout: Duration,
    deadline: Option<Instant>,
    log: &Logger,
) -> Result<GraphOutcome, String> {
    let intel = IntelClient::from_parts(intel_uri, intel_token)
        .map_err(|e| format!("intelligence: {e}"))?;
    let mut servers: Vec<McpClient> = Vec::new();
    for spec in server_specs {
        let client = crate::mcp::from_spec(spec, Duration::from_secs(60))
            .and_then(|mut c| c.initialize().map(|()| c))
            .map_err(|e| format!("mcp server '{}': {e}", spec.name))?;
        servers.push(client);
    }
    Ok(drive_connected(
        graph,
        &intel,
        &servers,
        model,
        max_steps,
        max_tokens,
        node_timeout,
        deadline,
        log,
    ))
}

/// Drive `graph` against ALREADY-CONNECTED intelligence + MCP clients, resolving
/// each `Wait` in-process (block-until-update-or-timeout) — the shared engine tail
/// behind [`drive_pinned`] and the payload-workflow child path (a supervised
/// subagent handed a workflow drives it here with the connections its runner
/// already made). `max_tokens` is the WHOLE-WORKFLOW intelligence pool.
#[allow(clippy::too_many_arguments)]
pub fn drive_connected(
    graph: &Graph,
    intel: &IntelClient,
    servers: &[McpClient],
    model: &str,
    max_steps: u32,
    max_tokens: u64,
    node_timeout: Duration,
    deadline: Option<Instant>,
    log: &Logger,
) -> GraphOutcome {
    let mut exec = SessionExec::new(intel, servers, log, model, max_steps, max_tokens, node_timeout)
        .with_deadline(deadline);
    let mut result = drive_budgeted(graph, &mut exec, GRAPH_MAX_STEPS, max_tokens);
    loop {
        match result {
            DriveResult::Done(outcome) => return outcome,
            DriveResult::Suspended(s) => {
                log.info(
                    "workflow.wait",
                    serde_json::json!({"on_uri": s.on_uri, "timeout_ms": s.timeout_ms}),
                );
                let outcome =
                    wait_for_uri(servers, &s.on_uri, Duration::from_millis(s.timeout_ms), log);
                result = resume(graph, s.state, &mut exec, outcome);
            }
        }
    }
}

/// Block until `uri` updates on some subscribing server (returning its freshly-read
/// content as [`WaitOutcome::Updated`]) or `timeout` elapses ([`WaitOutcome::TimedOut`]).
/// A uri no server can watch fails OPEN to the timeout edge rather than hanging.
fn wait_for_uri(servers: &[McpClient], uri: &str, timeout: Duration, log: &Logger) -> WaitOutcome {
    use crate::wire::mcp::method;
    let Some(server) = servers
        .iter()
        .find(|s| s.capabilities().supports_subscribe() && s.subscribe(uri).is_ok())
    else {
        log.warn("workflow.wait.unwatchable", serde_json::json!({"uri": uri}));
        return WaitOutcome::TimedOut;
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        for n in server.drain_notifications() {
            let hit = n.method == method::NOTIFY_RESOURCES_UPDATED
                && n.params
                    .as_ref()
                    .and_then(|p| p.get("uri"))
                    .and_then(Value::as_str)
                    == Some(uri);
            if hit {
                let _ = server.unsubscribe(uri);
                // notify-then-read: the current read is authoritative, not the note.
                let content = server
                    .read_resource_within(uri, Duration::from_secs(5))
                    .map(|r| r.text())
                    .unwrap_or_default();
                let val = serde_json::from_str::<Value>(&content).unwrap_or(Value::String(content));
                return WaitOutcome::Updated(val);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = server.unsubscribe(uri);
    WaitOutcome::TimedOut
}

/// A self-handler that offers no self-tools: a graph `Agent` node is a LEAF worker —
/// it runs its instruction against the MCP tools, it does not self-orchestrate (the
/// graph is the orchestration). Nesting, if ever wanted, is a `Subgraph` node.
struct LeafHandler;

impl SelfHandler for LeafHandler {
    fn tools(&self) -> Vec<ToolDef> {
        Vec::new()
    }
    fn handle(&mut self, _name: &str, _args: &Value) -> Option<(String, bool)> {
        None
    }
}

/// The production [`GraphExec`]: the live intelligence client + MCP server set + the
/// per-node limits a graph run executes against.
pub struct SessionExec<'a> {
    intel: &'a IntelClient,
    servers: &'a [McpClient],
    log: &'a Logger,
    model: String,
    max_steps: u32,
    max_tokens: u64,
    node_timeout: Duration,
    /// Whole-workflow wall-clock deadline (drives [`GraphExec::deadline_exceeded`]
    /// and shortens each node's own deadline to what remains).
    deadline: Option<Instant>,
    /// Intelligence tokens consumed since the driver last drained them.
    pending_tokens: u64,
}

impl<'a> SessionExec<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        intel: &'a IntelClient,
        servers: &'a [McpClient],
        log: &'a Logger,
        model: &str,
        max_steps: u32,
        max_tokens: u64,
        node_timeout: Duration,
    ) -> SessionExec<'a> {
        SessionExec {
            intel,
            servers,
            log,
            model: model.to_string(),
            max_steps,
            max_tokens,
            node_timeout,
            deadline: None,
            pending_tokens: 0,
        }
    }

    /// Set the whole-workflow wall-clock deadline.
    pub fn with_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.deadline = deadline;
        self
    }

    /// The per-node deadline: the node's own timeout, shortened to whatever
    /// remains of the whole-workflow deadline.
    fn node_deadline(&self) -> Instant {
        let own = Instant::now() + self.node_timeout;
        match self.deadline {
            Some(d) if d < own => d,
            _ => own,
        }
    }
}

impl GraphExec for SessionExec<'_> {
    fn run_agent(
        &mut self,
        instruction: &str,
        output_contract: Option<&str>,
        blackboard: &Blackboard,
        reads: &[String],
    ) -> (Value, bool) {
        // Fold the requested blackboard reads into the turn's context seed (RFC 0009
        // §spawn-payload — narrowed context, not the whole board).
        let seed = reads
            .iter()
            .filter_map(|k| blackboard.get(k).map(|v| ("user".to_string(), format!("{k} = {v}"))))
            .collect();
        let input = LoopInput {
            instruction: instruction.to_string(),
            output_contract: output_contract.map(str::to_string),
            seed,
            model: self.model.clone(),
            max_steps: self.max_steps,
            max_tokens: self.max_tokens,
            deadline: self.node_deadline(),
            cancel: None,
        };
        let mut handler = LeafHandler;
        match run_loop(self.intel, self.servers, &input, &mut handler, self.log) {
            Ok((outcome, usage)) => {
                self.pending_tokens = self.pending_tokens.saturating_add(usage.total());
                (outcome.result, outcome.status != TerminalStatus::Completed)
            }
            Err(e) => (Value::String(e.to_string()), true),
        }
    }

    fn call_tool(&mut self, server: &str, tool: &str, args: &Value) -> (Value, bool) {
        // A graph Tool node names its (server, tool) explicitly, so route straight to
        // that server's client (not the loop's tool-name catalogue).
        let Some(client) = self.servers.iter().find(|s| s.name() == server) else {
            return (Value::String(format!("no such MCP server '{server}'")), true);
        };
        match client.call_tool(tool, Some(args.clone())) {
            Ok(res) => {
                let text = res.text();
                // Prefer structured JSON; fall back to the raw text as a string.
                let val = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
                (val, res.is_error())
            }
            Err(e) => (Value::String(format!("tool transport error: {e}")), true),
        }
    }

    fn judge(
        &mut self,
        prompt: &str,
        blackboard: &Blackboard,
        reads: &[String],
        choices: &[String],
    ) -> Option<String> {
        // One tool-less completion (Tier-2, prompt-only per the design): context from
        // the reads + the question + the allowed labels. The answer is matched to a
        // label (exact, else first-substring); no match → None → the branch default.
        let mut ctx = String::new();
        for k in reads {
            if let Some(v) = blackboard.get(k) {
                ctx.push_str(&format!("{k} = {v}\n"));
            }
        }
        let question = format!("{ctx}\n{prompt}\n\nAnswer with exactly one of: {}", choices.join(", "));
        let req = Request {
            model: self.model.clone(),
            messages: vec![
                Message::system(
                    "You are a routing judge. Reply with ONLY one of the allowed labels.",
                ),
                Message::user(&question),
            ],
            tools: Vec::new(),
            max_tokens: 64,
            temperature: None,
        };
        let resp = self.intel.complete(&req).ok()?;
        self.pending_tokens = self.pending_tokens.saturating_add(resp.usage.total());
        let answer = resp.text?;
        let lower = answer.to_lowercase();
        // Exact match first; else the LONGEST contained label — so overlapping
        // labels ("done" vs "not-done") resolve to the more specific one.
        choices
            .iter()
            .find(|c| lower == c.to_lowercase())
            .or_else(|| {
                choices
                    .iter()
                    .filter(|c| lower.contains(&c.to_lowercase()))
                    .max_by_key(|c| c.len())
            })
            .cloned()
    }

    fn infer(
        &mut self,
        prompt: &str,
        blackboard: &Blackboard,
        reads: &[String],
        schema: &BTreeMap<String, FieldType>,
        feedback: Option<&str>,
    ) -> Result<Value, String> {
        // One tool-less completion asked to emit ONLY a JSON object with the schema
        // fields. The driver validates + re-asks; this method just asks and parses.
        let mut ctx = String::new();
        for k in reads {
            if let Some(v) = blackboard.get(k) {
                ctx.push_str(&format!("{k} = {v}\n"));
            }
        }
        let fields: Vec<String> = schema
            .iter()
            .map(|(f, t)| format!("\"{f}\": {t:?}"))
            .collect();
        let mut question = format!(
            "{ctx}\n{prompt}\n\nReply with ONLY a JSON object carrying these fields: {{{}}}",
            fields.join(", ")
        );
        if let Some(fb) = feedback {
            question.push_str(&format!(
                "\n\nYour previous answer was invalid: {fb}. Reply again with ONLY the corrected JSON object."
            ));
        }
        let req = Request {
            model: self.model.clone(),
            messages: vec![
                Message::system(
                    "You are a structured extraction engine. Reply with ONLY one JSON object — no prose, no code fences.",
                ),
                Message::user(&question),
            ],
            tools: Vec::new(),
            max_tokens: INFER_MAX_TOKENS,
            temperature: None,
        };
        let resp = self.intel.complete(&req).map_err(|e| e.to_string())?;
        self.pending_tokens = self.pending_tokens.saturating_add(resp.usage.total());
        let answer = resp.text.ok_or("empty completion")?;
        parse_json_answer(&answer)
    }

    fn run_subgraph(&mut self, graph: &Graph, _async_: bool, _blackboard: &Blackboard) -> (Value, bool) {
        // Sync: drive the nested graph inline with this same executor, resolving a
        // nested `Wait` the same way the top level does (subscribe + block until
        // update-or-timeout) — a subgraph is a full workflow, waits included. (An
        // `async` subgraph spawned as a detached subtree is the daemon-side follow-up.)
        let max_steps = self.max_steps;
        let mut result = drive(graph, self, max_steps);
        loop {
            match result {
                DriveResult::Done(o) => {
                    return (o.result, o.status != GraphStatus::Completed);
                }
                DriveResult::Suspended(s) => {
                    self.log.info(
                        "workflow.wait",
                        serde_json::json!({"on_uri": s.on_uri, "timeout_ms": s.timeout_ms, "nested": true}),
                    );
                    let outcome = wait_for_uri(
                        self.servers,
                        &s.on_uri,
                        Duration::from_millis(s.timeout_ms),
                        self.log,
                    );
                    result = resume(graph, s.state, self, outcome);
                }
            }
        }
    }

    fn take_tokens(&mut self) -> u64 {
        std::mem::take(&mut self.pending_tokens)
    }

    fn deadline_exceeded(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::obs::log::{Comp, Level, LogCtx};
    use serde_json::json;

    fn log() -> Logger {
        Logger::new(
            LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Agent,
                pid: 0,
                trace_id: None,
            },
            Level::Error,
        )
    }

    /// Start the built-in mock LLM in-process, returning its `http://<addr>` url.
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
        format!("http://{}", std::fs::read_to_string(addr_file).unwrap().trim())
    }

    #[test]
    fn session_exec_drives_a_real_agent_node_against_the_mock_llm() {
        // A graph Agent node runs a REAL ReAct turn against the mock LLM (the `final`
        // script answers "mock-llm done" in one call), and the result flows to the Halt.
        let dir = tempfile::tempdir().unwrap();
        let url = start_mock_llm(&dir.path().join("llm.addr"), "final");
        let intel = IntelClient::from_parts(&url, None).unwrap();
        let lg = log();
        let mut exec = SessionExec::new(&intel, &[], &lg, "mock", 8, 100_000, Duration::from_secs(10));
        let g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "do it", "writes": "out", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed", "result_from": "out"}
            }
        }))
        .unwrap();
        let DriveResult::Done(out) = drive(&g, &mut exec, 100) else {
            panic!("no wait node — should complete");
        };
        assert_eq!(out.status, GraphStatus::Completed);
        assert!(
            out.result.as_str().unwrap_or_default().contains("mock-llm done"),
            "real agent result: {:?}",
            out.result
        );
    }

    #[test]
    fn session_exec_judges_a_semantic_branch_against_the_mock_llm() {
        // The Tier-2 judge runs one real completion; "done" is a substring of the mock
        // answer "mock-llm done", so it is the chosen label.
        let dir = tempfile::tempdir().unwrap();
        let url = start_mock_llm(&dir.path().join("llm.addr"), "final");
        let intel = IntelClient::from_parts(&url, None).unwrap();
        let lg = log();
        let mut exec = SessionExec::new(&intel, &[], &lg, "mock", 8, 100_000, Duration::from_secs(10));
        let bb = Blackboard::new();
        let choice = exec.judge(
            "Is the task done?",
            &bb,
            &[],
            &["done".to_string(), "pending".to_string()],
        );
        assert_eq!(choice.as_deref(), Some("done"), "judge picked the matching label");
    }

    #[test]
    fn session_exec_infers_structured_json_against_the_mock_llm() {
        // The `json` script answers a pure JSON object; infer parses it and the
        // driver-side schema check accepts it end to end through a real Infer node.
        let dir = tempfile::tempdir().unwrap();
        let url = start_mock_llm(&dir.path().join("llm.addr"), "json");
        let intel = IntelClient::from_parts(&url, None).unwrap();
        let lg = log();
        let mut exec =
            SessionExec::new(&intel, &[], &lg, "mock", 8, 100_000, Duration::from_secs(10));
        let g: Graph = serde_json::from_value(json!({
            "start": "i",
            "nodes": {
                "i": {"kind": "infer", "prompt": "verdict?", "schema": {"verdict": "string", "score": "number"}, "writes": "c", "edges": {"ok": "h", "error": "f"}},
                "h": {"kind": "halt", "status": "completed", "result_from": "c"},
                "f": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap();
        let DriveResult::Done(out) = drive(&g, &mut exec, 100) else {
            panic!("no wait — should complete");
        };
        assert_eq!(out.status, GraphStatus::Completed, "{:?}", out.result);
        assert_eq!(out.result, json!({"verdict": "approve", "score": 9}));
    }

    #[test]
    fn parse_json_answer_tolerates_fences_and_prose() {
        assert_eq!(
            parse_json_answer(r#"{"a": 1}"#).unwrap(),
            json!({"a": 1}),
            "strict"
        );
        assert_eq!(
            parse_json_answer("```json\n{\"a\": 1}\n```").unwrap(),
            json!({"a": 1}),
            "fenced"
        );
        assert_eq!(
            parse_json_answer("Sure! Here you go: {\"a\": 1} — anything else?").unwrap(),
            json!({"a": 1}),
            "prose-wrapped"
        );
        assert!(parse_json_answer("no json here").is_err());
    }

    #[test]
    fn judge_prefers_the_longest_matching_label() {
        // The mock answers "mock-llm done". Both "done" and "llm done" are
        // contained; the LONGEST match wins so overlapping labels resolve to the
        // more specific one.
        let dir = tempfile::tempdir().unwrap();
        let url = start_mock_llm(&dir.path().join("llm.addr"), "final");
        let intel = IntelClient::from_parts(&url, None).unwrap();
        let lg = log();
        let mut exec =
            SessionExec::new(&intel, &[], &lg, "mock", 8, 100_000, Duration::from_secs(10));
        let bb = Blackboard::new();
        let choice = exec.judge(
            "state?",
            &bb,
            &[],
            &["done".to_string(), "llm done".to_string()],
        );
        assert_eq!(choice.as_deref(), Some("llm done"), "longest contained label");
    }

    #[test]
    fn session_exec_reports_consumed_tokens_and_deadline() {
        // The mock stamps usage on every completion; take_tokens drains it, and a
        // past deadline flips deadline_exceeded.
        let dir = tempfile::tempdir().unwrap();
        let url = start_mock_llm(&dir.path().join("llm.addr"), "final");
        let intel = IntelClient::from_parts(&url, None).unwrap();
        let lg = log();
        let mut exec =
            SessionExec::new(&intel, &[], &lg, "mock", 8, 100_000, Duration::from_secs(10));
        assert!(!exec.deadline_exceeded(), "no deadline set");
        let _ = exec.judge("q?", &Blackboard::new(), &[], &["done".to_string()]);
        assert!(exec.take_tokens() > 0, "judge usage accumulated");
        assert_eq!(exec.take_tokens(), 0, "drained");
        let mut exec = exec.with_deadline(Some(Instant::now() - Duration::from_millis(1)));
        assert!(exec.deadline_exceeded(), "past deadline detected");
        let _ = &mut exec;
    }

    #[test]
    fn call_tool_on_an_unknown_server_is_an_error() {
        // Routing is by server NAME; an unknown server is an error result (the node's
        // error edge), never a panic. (No intel call — just the routing path.)
        let dir = tempfile::tempdir().unwrap();
        let url = start_mock_llm(&dir.path().join("llm.addr"), "final");
        let intel = IntelClient::from_parts(&url, None).unwrap();
        let lg = log();
        let mut exec = SessionExec::new(&intel, &[], &lg, "mock", 8, 100_000, Duration::from_secs(10));
        let (val, is_err) = exec.call_tool("ghost", "do", &json!({}));
        assert!(is_err);
        assert!(val.as_str().unwrap().contains("no such MCP server"));
    }
}
