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

use super::{drive, resume, Blackboard, DriveResult, Graph, GraphExec, GraphOutcome, GraphStatus, WaitOutcome};
use crate::agentloop::action::SelfHandler;
use crate::agentloop::runner::{run_loop, LoopInput};
use crate::agentloop::stop::TerminalStatus;
use crate::config::McpServerSpec;
use crate::intel::client::IntelClient;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::wire::intel::{Message, Request, ToolDef};
use serde_json::Value;
use std::time::{Duration, Instant};

/// The graph run's total node-visit cap (layer-1 backstop) — generous, since a graph
/// is many node visits; the graph's own budget/validation/termination bound the walk.
pub const GRAPH_MAX_STEPS: u32 = 10_000;

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
    let mut exec = SessionExec::new(&intel, &servers, log, model, max_steps, max_tokens, node_timeout);

    // Drive to a terminal outcome, resolving each `Wait` in-process (block-until-update
    // -or-timeout). The graph's step budget accumulates across resumes, so even a Wait
    // loop is bounded.
    let mut result = drive(graph, &mut exec, GRAPH_MAX_STEPS);
    loop {
        match result {
            DriveResult::Done(outcome) => return Ok(outcome),
            DriveResult::Suspended(s) => {
                log.info(
                    "workflow.wait",
                    serde_json::json!({"on_uri": s.on_uri, "timeout_ms": s.timeout_ms}),
                );
                let outcome = wait_for_uri(&servers, &s.on_uri, Duration::from_millis(s.timeout_ms), log);
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
            deadline: Instant::now() + self.node_timeout,
            cancel: None,
        };
        let mut handler = LeafHandler;
        match run_loop(self.intel, self.servers, &input, &mut handler, self.log) {
            Ok((outcome, _usage)) => (outcome.result, outcome.status != TerminalStatus::Completed),
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
        let answer = self.intel.complete(&req).ok()?.text?;
        let lower = answer.to_lowercase();
        choices
            .iter()
            .find(|c| lower == c.to_lowercase())
            .or_else(|| choices.iter().find(|c| lower.contains(&c.to_lowercase())))
            .cloned()
    }

    fn run_subgraph(&mut self, graph: &Graph, _async_: bool, _blackboard: &Blackboard) -> (Value, bool) {
        // Sync: drive the nested graph inline with this same executor. (An `async`
        // subgraph spawned as a detached, capped subtree is the daemon-side follow-up;
        // a nested Wait cannot suspend inline, so it is reported as an error.)
        let max_steps = self.max_steps;
        match drive(graph, self, max_steps) {
            DriveResult::Done(o) => (o.result, o.status != GraphStatus::Completed),
            DriveResult::Suspended(_) => (
                Value::String("subgraph suspended on a Wait (unsupported inline)".into()),
                true,
            ),
        }
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
