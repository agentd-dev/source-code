// SPDX-License-Identifier: Apache-2.0
//! The workflow driver (pivot Phase 7) — the thin walk that turns an authored
//! [`Graph`](super::Graph) into work.
//!
//! The driver is deliberately transport-free + Session-free: it walks nodes, threads
//! a blackboard, follows labelled edges (a missing label fails CLOSED to the implicit
//! `Halt(Crashed)` safety sink), and enforces the run budget + cycle-termination
//! guards. The effectful node kinds — `Agent` (run a turn), `Tool` (call an MCP tool),
//! and the Tier-2 `Branch` judgement — are dispatched through the [`GraphExec`] seam,
//! implemented over a real `Session` + intelligence client in production and a scripted
//! mock in tests. So the control-flow logic is proven independently of the execution
//! wiring (a later phase), and the same driver serves both the model-authored
//! `workflow.run` path and the operator `--workflow <file>` path.
//!
//! Handles `Agent`/`Tool`/`Branch`/`Halt` inline and `Wait` by SUSPENDING — [`drive`]
//! returns [`DriveResult::Suspended`] with a serializable [`GraphState`] the daemon
//! persists, watches, and hands back to [`resume`]. `Subgraph` (P5), a dangling edge,
//! or an unhandled label fail CLOSED (`Crashed`) rather than panicking.

use super::{Graph, Node, NodeId};
use crate::agentloop::stop::TerminalStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The blackboard — the graph's shared, keyed state (`BTreeMap` = deterministic).
pub type Blackboard = BTreeMap<String, Value>;

/// The graph-level run outcome. `status` is the ENGINE-level result (distinct from
/// the per-turn `TerminalStatus`, RFC 0007 — pivot decision "add distinct graph
/// statuses"); `terminal` carries the author's `Halt` status when the graph halted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphOutcome {
    pub status: GraphStatus,
    /// The author-chosen `Halt` status when the graph reached a `Halt`; `None` for an
    /// engine-forced termination (budget/loop/stall/crash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalStatus>,
    /// The projected result (the `Halt.result_from` blackboard value, or null).
    pub result: Value,
    /// Total node visits taken.
    pub steps: u32,
}

/// The engine-level graph status. Distinct from the per-turn/tool `TerminalStatus`
/// so the two RFC-0007 variants (`Stalled`/`LoopDetected`) are never overloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphStatus {
    /// Reached a `Halt` the author marked `Completed`.
    Completed,
    /// Reached a `Halt` with any other author status (Refused/Failed/…).
    Halted,
    /// The graph step budget was exhausted (termination layer 1).
    Exhausted,
    /// A node's visit cap tripped — a runaway cycle (layer 2, from P2).
    LoopDetected,
    /// A full cycle made no blackboard progress (layer 3, from P2).
    Stalled,
    /// The driver hit an unsupported node / a dangling edge / a missing label — fail
    /// closed (the implicit `Halt(Crashed)` safety sink).
    Crashed,
}

/// Per-node visit cap (termination layer 2): a node visited more than this many
/// times is a runaway cycle → `LoopDetected`, even under a large step budget.
pub const MAX_VISITS_PER_NODE: u32 = 100;

/// The graph run budget → the layer-1 termination guard (a total node-visit cap).
/// Serde-serializable so it rides the persisted [`GraphState`] across a long Wait.
/// Layers 2 (per-node visit cap) and 3 (progress guard) are enforced by the driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphBudget {
    max_steps: u32,
    steps: u32,
}

impl GraphBudget {
    pub fn new(max_steps: u32) -> GraphBudget {
        GraphBudget { max_steps, steps: 0 }
    }

    /// Charge one node visit; `false` when the budget is spent (do not proceed).
    fn step(&mut self) -> bool {
        if self.steps >= self.max_steps {
            return false;
        }
        self.steps += 1;
        true
    }

    pub fn steps(&self) -> u32 {
        self.steps
    }
}

/// The persisted, resumable run slice (pivot Phase 7 · P4) — everything a suspended
/// graph needs to continue: where it is, its blackboard, the cycle-termination
/// bookkeeping, and its budget. Kept OFF the frozen `Graph` (which stays pure
/// topology) and serde-serializable, so a long `Wait` survives across the process
/// boundary and even a restart — the durable-state decision: the daemon writes this
/// slice to an `agentd://graph/<id>` state file on suspend and reads it on resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphState {
    /// The node to (re-)enter.
    pub at: NodeId,
    /// The shared, keyed graph state.
    pub blackboard: Blackboard,
    /// Per-node visit counts (layer 2), preserved across suspends.
    visits: BTreeMap<NodeId, u32>,
    /// Blackboard hash last seen on entry to each node (layer 3).
    entry_hash: BTreeMap<NodeId, u64>,
    budget: GraphBudget,
}

impl GraphState {
    /// A fresh run slice entering `start` with a total step cap.
    pub fn new(start: NodeId, max_steps: u32) -> GraphState {
        GraphState {
            at: start,
            blackboard: BTreeMap::new(),
            visits: BTreeMap::new(),
            entry_hash: BTreeMap::new(),
            budget: GraphBudget::new(max_steps),
        }
    }

    pub fn steps(&self) -> u32 {
        self.budget.steps()
    }
}

/// What the daemon must arm when a graph suspends on a `Wait`, plus the persisted
/// state to resume it with. The daemon installs an ephemeral exact route on `on_uri`
/// + a `timeout_ms` timer; whichever fires first, it calls [`resume`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suspension {
    pub on_uri: String,
    pub timeout_ms: u64,
    pub state: GraphState,
}

/// How a `Wait` resolved — supplied by the daemon on [`resume`].
#[derive(Debug, Clone)]
pub enum WaitOutcome {
    /// The watched resource updated; carries its freshly-read content (written to the
    /// Wait node's `writes` key) and takes the `updated` edge.
    Updated(Value),
    /// The timeout elapsed first; takes the `timeout` edge.
    TimedOut,
}

/// The result of driving (or resuming) a graph: it terminated, or it suspended on a
/// `Wait` and must be resumed once the daemon's watch fires.
#[derive(Debug, Clone)]
pub enum DriveResult {
    /// The graph reached a terminal outcome.
    Done(GraphOutcome),
    /// The graph suspended on a `Wait`; resume with [`resume`] once it resolves.
    Suspended(Suspension),
}

/// A deterministic hash of the blackboard (the `BTreeMap` serializes in a stable key
/// order) for the progress guard — two identical blackboards hash equal.
fn bb_hash(bb: &Blackboard) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(bb).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// The execution surface a graph driver needs: run an `Agent` turn and call a `Tool`.
/// Implemented over a real `Session` + intelligence client in production, mocked in
/// tests. Each method returns `(result_value, is_error)` — `is_error` selects the
/// node's `error` edge, success selects `ok`.
pub trait GraphExec {
    /// Run an `Agent` node: an agentic turn on `instruction` (honouring an optional
    /// `output_contract`), with the named `reads` folded from the `blackboard` into
    /// its context. Returns the distilled result + whether the turn errored.
    fn run_agent(
        &mut self,
        instruction: &str,
        output_contract: Option<&str>,
        blackboard: &Blackboard,
        reads: &[String],
    ) -> (Value, bool);

    /// Run a `Tool` node: call `tool` on MCP `server` with `args`. Returns the tool
    /// result + whether it was an error result.
    fn call_tool(&mut self, server: &str, tool: &str, args: &Value) -> (Value, bool);

    /// Judge a Tier-2 semantic branch: fold the `reads` blackboard values into
    /// `prompt` and run ONE intelligence `complete()` with no tools, asking for one
    /// of `choices`. Return the chosen label (the caller validates it ∈ `choices`) or
    /// `None` to take the branch default. Default impl: `None` (a build/exec with no
    /// intelligence always takes the default — a semantic branch degrades safely).
    fn judge(
        &mut self,
        _prompt: &str,
        _blackboard: &Blackboard,
        _reads: &[String],
        _choices: &[String],
    ) -> Option<String> {
        None
    }

    /// Run a `Subgraph` node: execute the nested `graph` — synchronously inline, or
    /// `async` as a detached subtree — and return its result + whether it errored
    /// (selecting the node's `ok`/`error` edge). The production impl runs it via the
    /// supervisor's spawn machinery so the nested run inherits the depth/breadth/token
    /// caps (it crosses a process boundary). Default impl: `(Null, true)` — a build
    /// without that wiring degrades SAFELY to the Subgraph's `error` edge rather than
    /// silently skipping it.
    fn run_subgraph(&mut self, _graph: &Graph, _async_: bool, _blackboard: &Blackboard) -> (Value, bool) {
        (Value::Null, true)
    }
}

/// The label an effectful node emits given whether it errored.
fn edge_for(is_error: bool) -> &'static str {
    if is_error {
        "error"
    } else {
        "ok"
    }
}

/// Start a fresh graph run: enter `start` with a total step cap of `max_steps`.
/// Returns [`DriveResult::Done`] on termination, or [`DriveResult::Suspended`] when a
/// `Wait` is reached (the daemon then arms the watch and calls [`resume`]). P1–P4
/// execute `Agent`/`Tool`/`Branch`/`Wait`/`Halt`; `Subgraph` (P5), a dangling edge, or
/// an unhandled emitted label fails CLOSED (`Crashed`) — the implicit `Halt(Crashed)`
/// safety sink, so a mis-authored graph never runs away or panics.
pub fn drive(graph: &Graph, exec: &mut dyn GraphExec, max_steps: u32) -> DriveResult {
    let mut state = GraphState::new(graph.start.clone(), max_steps);
    drive_state(graph, &mut state, exec)
}

/// Resume a suspended graph once its `Wait` resolved: apply the [`WaitOutcome`] (write
/// the updated value + take the `updated` edge, or take the `timeout` edge), then keep
/// driving. Resuming a state whose `at` is not a `Wait`, or a `Wait` missing the
/// resolved edge, fails CLOSED (`Crashed`) — the daemon's contract is to resume with
/// the same `(graph, state)` it suspended.
pub fn resume(
    graph: &Graph,
    mut state: GraphState,
    exec: &mut dyn GraphExec,
    outcome: WaitOutcome,
) -> DriveResult {
    let Some(Node::Wait { writes, edges, .. }) = graph.nodes.get(&state.at) else {
        return DriveResult::Done(GraphOutcome::engine(
            GraphStatus::Crashed,
            Value::Null,
            state.steps(),
        ));
    };
    let label = match &outcome {
        WaitOutcome::Updated(v) => {
            write(&mut state.blackboard, writes, v.clone());
            "updated"
        }
        WaitOutcome::TimedOut => "timeout",
    };
    match edges.get(label) {
        Some(next) => state.at = next.clone(),
        None => {
            let result = bb_result(&state.blackboard, None);
            return DriveResult::Done(GraphOutcome::engine(
                GraphStatus::Crashed,
                result,
                state.steps(),
            ));
        }
    }
    // Crossing a Wait is a real-world checkpoint (an external event arrived, time
    // passed), so the per-node visit + progress bookkeeping resets: a compute loop is
    // bounded PER EVENT, not across the whole reactive lifetime. A tight runaway loop
    // BETWEEN waits is still caught; a long-lived event loop (a back-edge into a Wait)
    // runs indefinitely — bounded only by the total step budget (the operator's cap).
    state.visits.clear();
    state.entry_hash.clear();
    drive_state(graph, &mut state, exec)
}

/// The core walk from `state.at`, mutating the run slice in place until it terminates
/// (`Done`) or hits a `Wait` (`Suspended`).
fn drive_state(graph: &Graph, state: &mut GraphState, exec: &mut dyn GraphExec) -> DriveResult {
    loop {
        let Some(node) = graph.nodes.get(&state.at) else {
            // A dangling edge slipped past validation → fail closed.
            return DriveResult::Done(GraphOutcome::engine(
                GraphStatus::Crashed,
                Value::Null,
                state.steps(),
            ));
        };

        // Layer 1 — total step budget (every node visit is charged).
        if !state.budget.step() {
            let result = bb_result(&state.blackboard, None);
            return DriveResult::Done(GraphOutcome::engine(
                GraphStatus::Exhausted,
                result,
                state.steps(),
            ));
        }

        // Layers 2 + 3 apply to COMPUTE nodes only. A `Wait` suspends (it does not
        // spin), so a back-edge into a Wait is a long-lived reactive loop that must
        // NOT trip loop-detection/stall — it costs nothing idle. The step budget is
        // the backstop even for a Wait loop.
        if !matches!(node, Node::Wait { .. }) {
            let v = state.visits.entry(state.at.clone()).or_insert(0);
            *v += 1;
            if *v > MAX_VISITS_PER_NODE {
                let result = bb_result(&state.blackboard, None);
                return DriveResult::Done(GraphOutcome::engine(
                    GraphStatus::LoopDetected,
                    result,
                    state.steps(),
                ));
            }
            let h = bb_hash(&state.blackboard);
            if state.entry_hash.get(&state.at) == Some(&h) {
                let result = bb_result(&state.blackboard, None);
                return DriveResult::Done(GraphOutcome::engine(
                    GraphStatus::Stalled,
                    result,
                    state.steps(),
                ));
            }
            state.entry_hash.insert(state.at.clone(), h);
        }

        // Effectful nodes produce `(label, edges)` and fall through to edge-follow;
        // Halt/Wait return; Branch transitions directly; Subgraph fails closed (P5).
        let (label, edges) = match node {
            Node::Halt { status, result_from } => {
                let result = bb_result(&state.blackboard, result_from.as_deref());
                return DriveResult::Done(GraphOutcome::halt(*status, result, state.steps()));
            }
            // A Wait SUSPENDS: hand the daemon the watch (uri + timeout) and the state
            // to resume with. The current node stays `state.at` so `resume` knows which
            // Wait resolved.
            Node::Wait { on_uri, timeout_ms, .. } => {
                return DriveResult::Suspended(Suspension {
                    on_uri: on_uri.clone(),
                    timeout_ms: *timeout_ms,
                    state: state.clone(),
                });
            }
            // Branch: the first deterministic case whose predicate holds wins (Tier 1,
            // free); else a Tier-2 semantic judgement; else `default`. It writes nothing
            // and emits no ok/error label — it transitions directly.
            Node::Branch { cases, default, semantic } => {
                state.at = if let Some(c) = cases.iter().find(|c| c.when.eval(&state.blackboard)) {
                    c.goto.clone()
                } else if let Some(spec) = semantic {
                    let labels: Vec<String> = spec.choices.keys().cloned().collect();
                    match exec.judge(&spec.prompt, &state.blackboard, &spec.reads, &labels) {
                        Some(label) => spec.choices.get(&label).unwrap_or(default).clone(),
                        None => default.clone(),
                    }
                } else {
                    default.clone()
                };
                continue;
            }
            Node::Agent {
                instruction,
                output_contract,
                reads,
                writes,
                edges,
                ..
            } => {
                let (val, is_err) = exec.run_agent(
                    instruction,
                    output_contract.as_deref(),
                    &state.blackboard,
                    reads,
                );
                write(&mut state.blackboard, writes, val);
                (edge_for(is_err), edges)
            }
            Node::Tool { server, tool, args, writes, edges } => {
                let (val, is_err) = exec.call_tool(server, tool, args);
                write(&mut state.blackboard, writes, val);
                (edge_for(is_err), edges)
            }
            // Subgraph: run the nested graph through the exec seam (the real impl
            // spawns a capped subtree), write its result, follow ok/error.
            Node::Subgraph { graph: sub, async_, writes, edges } => {
                let (val, is_err) = exec.run_subgraph(sub, *async_, &state.blackboard);
                write(&mut state.blackboard, writes, val);
                (edge_for(is_err), edges)
            }
        };

        match edges.get(label) {
            Some(next) => state.at = next.clone(),
            // Unhandled label → the implicit Halt(Crashed) safety sink.
            None => {
                let result = bb_result(&state.blackboard, None);
                return DriveResult::Done(GraphOutcome::engine(
                    GraphStatus::Crashed,
                    result,
                    state.steps(),
                ));
            }
        }
    }
}

/// Write a node's result to its `writes` key (a no-op when the node writes nothing).
fn write(bb: &mut Blackboard, writes: &Option<String>, val: Value) {
    if let Some(k) = writes {
        bb.insert(k.clone(), val);
    }
}

/// Project the graph result: the `result_from` blackboard value (or null if unset /
/// absent).
fn bb_result(bb: &Blackboard, result_from: Option<&str>) -> Value {
    result_from
        .and_then(|k| bb.get(k))
        .cloned()
        .unwrap_or(Value::Null)
}

impl GraphOutcome {
    fn halt(status: TerminalStatus, result: Value, steps: u32) -> GraphOutcome {
        let gs = if status == TerminalStatus::Completed {
            GraphStatus::Completed
        } else {
            GraphStatus::Halted
        };
        GraphOutcome {
            status: gs,
            terminal: Some(status),
            result,
            steps,
        }
    }

    fn engine(status: GraphStatus, result: Value, steps: u32) -> GraphOutcome {
        GraphOutcome {
            status,
            terminal: None,
            result,
            steps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A scripted execution surface: `run_agent` returns the value keyed by the
    /// instruction, `call_tool` the value keyed by `"server.tool"`; either can be
    /// marked an error. Records the call order so a test can assert the walk.
    #[derive(Default)]
    struct MockExec {
        agents: BTreeMap<String, (Value, bool)>,
        tools: BTreeMap<String, (Value, bool)>,
        calls: Vec<String>,
        last_blackboard: Blackboard,
        /// When an agent's instruction equals this, return an INCREMENTING counter
        /// `{"n": k}` (a progressing loop body) instead of a scripted constant.
        counting: Option<String>,
        counter: i64,
        /// The label `judge` returns for a Tier-2 semantic branch (`None` = default).
        judge_answer: Option<String>,
    }

    impl GraphExec for MockExec {
        fn run_agent(
            &mut self,
            instruction: &str,
            _output_contract: Option<&str>,
            blackboard: &Blackboard,
            _reads: &[String],
        ) -> (Value, bool) {
            self.calls.push(format!("agent:{instruction}"));
            self.last_blackboard = blackboard.clone();
            if self.counting.as_deref() == Some(instruction) {
                self.counter += 1;
                return (json!({ "n": self.counter }), false);
            }
            self.agents
                .get(instruction)
                .cloned()
                .unwrap_or((Value::Null, false))
        }

        fn call_tool(&mut self, server: &str, tool: &str, _args: &Value) -> (Value, bool) {
            let key = format!("{server}.{tool}");
            self.calls.push(format!("tool:{key}"));
            self.tools.get(&key).cloned().unwrap_or((Value::Null, false))
        }

        fn judge(
            &mut self,
            prompt: &str,
            _blackboard: &Blackboard,
            _reads: &[String],
            _choices: &[String],
        ) -> Option<String> {
            self.calls.push(format!("judge:{prompt}"));
            self.judge_answer.clone()
        }

        fn run_subgraph(&mut self, graph: &Graph, _async_: bool, _bb: &Blackboard) -> (Value, bool) {
            self.calls.push("subgraph".to_string());
            // Drive the nested graph inline (the real impl spawns a capped subtree);
            // its result + whether it completed select the parent's ok/error edge.
            match drive(graph, self, 1000) {
                DriveResult::Done(o) => (o.result, o.status != GraphStatus::Completed),
                DriveResult::Suspended(_) => (json!("subgraph suspended"), true),
            }
        }
    }

    /// Drive a graph that is expected to run to completion (no `Wait`) and return its
    /// terminal outcome — the common shape for the non-suspending tests.
    fn run(g: &Graph, exec: &mut dyn GraphExec, max_steps: u32) -> GraphOutcome {
        match drive(g, exec, max_steps) {
            DriveResult::Done(o) => o,
            DriveResult::Suspended(s) => panic!("unexpected suspend on {}", s.on_uri),
        }
    }

    /// extract (agent) → transform (tool) → load (agent) → halt(result_from = "out").
    /// Four effectful steps so a DOWNSTREAM agent (`load`) genuinely receives the
    /// blackboard written by both an earlier agent and an earlier tool.
    fn etl() -> Graph {
        serde_json::from_value(json!({
            "start": "extract",
            "nodes": {
                "extract": {
                    "kind": "agent",
                    "instruction": "extract",
                    "writes": "raw",
                    "edges": {"ok": "transform", "error": "fail"}
                },
                "transform": {
                    "kind": "tool",
                    "server": "fs",
                    "tool": "transform",
                    "writes": "mid",
                    "edges": {"ok": "load", "error": "fail"}
                },
                "load": {
                    "kind": "agent",
                    "instruction": "load",
                    "reads": ["raw", "mid"],
                    "writes": "out",
                    "edges": {"ok": "done", "error": "fail"}
                },
                "done": {"kind": "halt", "status": "completed", "result_from": "out"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn drives_a_linear_etl_threading_the_blackboard() {
        let g = etl();
        assert!(g.validate().is_ok());
        let mut exec = MockExec::default();
        exec.agents
            .insert("extract".into(), (json!({"rows": 3}), false));
        exec.tools
            .insert("fs.transform".into(), (json!({"clean": true}), false));
        exec.agents.insert("load".into(), (json!({"loaded": 3}), false));
        let out = run(&g, &mut exec, 100);

        assert_eq!(out.status, GraphStatus::Completed);
        assert_eq!(out.terminal, Some(TerminalStatus::Completed));
        // The result is projected from the blackboard key the Halt names.
        assert_eq!(out.result, json!({"loaded": 3}));
        assert_eq!(out.steps, 4, "extract → transform → load → done");
        // The walk order.
        assert_eq!(
            exec.calls,
            vec!["agent:extract", "tool:fs.transform", "agent:load"]
        );
        // Blackboard threading: the `load` agent (last run_agent) saw BOTH the
        // earlier agent's write (`raw`) and the earlier tool's write (`mid`).
        assert_eq!(exec.last_blackboard.get("raw"), Some(&json!({"rows": 3})));
        assert_eq!(exec.last_blackboard.get("mid"), Some(&json!({"clean": true})));
    }

    #[test]
    fn an_error_result_follows_the_error_edge() {
        let g = etl();
        let mut exec = MockExec::default();
        // The extract agent ERRORS → the `error` edge → the crashed Halt.
        exec.agents
            .insert("extract".into(), (json!("boom"), true));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.terminal, Some(TerminalStatus::Crashed));
        assert_eq!(out.status, GraphStatus::Halted);
        // The transform tool was never reached.
        assert_eq!(exec.calls, vec!["agent:extract"]);
    }

    #[test]
    fn the_step_budget_bounds_the_walk() {
        // A budget of 1 halts the 3-step ETL early as Exhausted (layer 1).
        let g = etl();
        let mut exec = MockExec::default();
        exec.agents.insert("extract".into(), (json!(1), false));
        exec.tools.insert("fs.load".into(), (json!(2), false));
        let out = run(&g, &mut exec, 1);
        assert_eq!(out.status, GraphStatus::Exhausted);
        assert_eq!(out.terminal, None);
    }

    #[test]
    fn a_missing_emitted_label_fails_closed() {
        // The agent succeeds but has no `ok` edge → the implicit Halt(Crashed) sink.
        let g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "x", "edges": {"error": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        // (This graph is valid — every EDGE has a target — but at run time the `ok`
        // label is unhandled, which must fail closed rather than wedge.)
        let mut exec = MockExec::default();
        let out = run(&g, &mut exec, 10);
        assert_eq!(out.status, GraphStatus::Crashed);
    }

    // ── P2: branches, cycles, termination ────────────────────────────────────

    /// tick (agent, writes "c") → gate (branch on c/n) → tick | done. A back-edge
    /// (gate → tick) makes it a real cycle.
    fn counter_loop(exit_at: i64) -> Graph {
        serde_json::from_value(json!({
            "start": "tick",
            "nodes": {
                "tick": {"kind": "agent", "instruction": "tick", "writes": "c", "edges": {"ok": "gate"}},
                "gate": {
                    "kind": "branch",
                    "cases": [
                        {"when": {"op": "gt", "key": "c", "pointer": "/n", "value": exit_at as f64}, "goto": "done"}
                    ],
                    "default": "tick"
                },
                "done": {"kind": "halt", "status": "completed", "result_from": "c"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_branch_routes_on_the_blackboard() {
        // The gate exits once the counter passes 2 (n=3): tick,gate,tick,gate,tick,gate,done.
        let g = counter_loop(2);
        assert!(g.validate().is_ok(), "a cyclic graph with a reachable halt is valid");
        let mut exec = MockExec {
            counting: Some("tick".into()),
            ..MockExec::default()
        };
        let out = run(&g, &mut exec, 1000);
        assert_eq!(out.status, GraphStatus::Completed);
        assert_eq!(out.result, json!({"n": 3}), "exited when the counter passed 2");
        // Three loop iterations: (tick,gate) × 3 then done.
        assert_eq!(out.steps, 7);
    }

    #[test]
    fn a_branch_falls_through_to_default() {
        // No case matches (default routes onward); with a constant body it stalls,
        // proving `default` was taken (the case predicate never fired).
        let g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "const", "writes": "k", "edges": {"ok": "b"}},
                "b": {
                    "kind": "branch",
                    "cases": [{"when": {"op": "eq", "key": "k", "value": "never"}, "goto": "done"}],
                    "default": "a"
                },
                "done": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let mut exec = MockExec::default();
        exec.agents.insert("const".into(), (json!("always"), false));
        let out = run(&g, &mut exec, 1000);
        // The default keeps looping with no progress → Stalled (layer 3).
        assert_eq!(out.status, GraphStatus::Stalled);
    }

    #[test]
    fn a_non_progressing_loop_is_stalled() {
        // A bare self-loop whose body writes the same value every time. The agent
        // node re-enters with an unchanged blackboard → Stalled.
        let g: Graph = serde_json::from_value(json!({
            "start": "spin",
            "nodes": {
                "spin": {"kind": "agent", "instruction": "const", "writes": "k", "edges": {"ok": "gate"}},
                "gate": {"kind": "branch", "cases": [], "default": "spin"}
            }
        }))
        .unwrap();
        // (No Halt reachable → validation rejects it; drive it directly to prove the
        // runtime guard independent of author-time validation.)
        let mut exec = MockExec::default();
        exec.agents.insert("const".into(), (json!(1), false));
        let out = run(&g, &mut exec, 1000);
        assert_eq!(out.status, GraphStatus::Stalled);
        assert!(out.steps < 10, "stalls fast, not at the budget: {}", out.steps);
    }

    #[test]
    fn a_progressing_but_unbounded_loop_hits_the_visit_cap() {
        // The body PROGRESSES each iteration (an incrementing counter, so never
        // Stalled) but the gate never exits (exit_at unreachably high) → the per-node
        // visit cap trips first: LoopDetected (layer 2), before the step budget.
        let g = counter_loop(1_000_000);
        let mut exec = MockExec {
            counting: Some("tick".into()),
            ..MockExec::default()
        };
        let out = run(&g, &mut exec, 100_000);
        assert_eq!(out.status, GraphStatus::LoopDetected);
        // Tripped at the per-node cap (~2 visits/iteration), well under the budget.
        assert!(
            out.steps < 100_000,
            "loop-detected before budget exhaustion: {}",
            out.steps
        );
    }

    // ── P3: the Tier-2 semantic branch ───────────────────────────────────────

    /// review (agent) → gate (branch: no Tier-1 case, a semantic {approve|reject}) →
    /// approved | rejected halts.
    fn review_graph() -> Graph {
        serde_json::from_value(json!({
            "start": "review",
            "nodes": {
                "review": {"kind": "agent", "instruction": "review", "writes": "doc", "edges": {"ok": "gate"}},
                "gate": {
                    "kind": "branch",
                    "cases": [],
                    "default": "rejected",
                    "semantic": {
                        "prompt": "Is the document acceptable?",
                        "reads": ["doc"],
                        "choices": {"approve": "approved", "reject": "rejected"}
                    }
                },
                "approved": {"kind": "halt", "status": "completed", "result_from": "doc"},
                "rejected": {"kind": "halt", "status": "refused"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_semantic_branch_routes_on_the_model_judgement() {
        let g = review_graph();
        assert!(g.validate().is_ok(), "semantic choice targets resolve");
        let mut exec = MockExec {
            judge_answer: Some("approve".into()),
            ..MockExec::default()
        };
        exec.agents.insert("review".into(), (json!({"ok": true}), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed, "approve → approved halt");
        assert_eq!(out.result, json!({"ok": true}));
        // The model judgement was consulted (one complete() call).
        assert!(exec.calls.iter().any(|c| c.starts_with("judge:")));
    }

    #[test]
    fn an_unrecognised_semantic_answer_takes_the_default() {
        let g = review_graph();
        let mut exec = MockExec {
            judge_answer: Some("maybe".into()), // not in {approve, reject}
            ..MockExec::default()
        };
        exec.agents.insert("review".into(), (json!(1), false));
        let out = run(&g, &mut exec, 100);
        // default = "rejected" (status refused).
        assert_eq!(out.terminal, Some(TerminalStatus::Refused));
    }

    #[test]
    fn tier1_cases_take_priority_over_the_semantic_branch() {
        // A deterministic case matches, so the model is NEVER consulted.
        let g: Graph = serde_json::from_value(json!({
            "start": "review",
            "nodes": {
                "review": {"kind": "agent", "instruction": "review", "writes": "doc", "edges": {"ok": "gate"}},
                "gate": {
                    "kind": "branch",
                    "cases": [{"when": {"op": "eq", "key": "doc", "pointer": "/flag", "value": true}, "goto": "approved"}],
                    "default": "rejected",
                    "semantic": {"prompt": "acceptable?", "choices": {"approve": "approved"}}
                },
                "approved": {"kind": "halt", "status": "completed"},
                "rejected": {"kind": "halt", "status": "refused"}
            }
        }))
        .unwrap();
        let mut exec = MockExec {
            judge_answer: Some("approve".into()),
            ..MockExec::default()
        };
        exec.agents.insert("review".into(), (json!({"flag": true}), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.terminal, Some(TerminalStatus::Completed), "Tier-1 case won");
        assert!(
            !exec.calls.iter().any(|c| c.starts_with("judge:")),
            "the model must NOT be consulted when a deterministic case matches"
        );
    }

    #[test]
    fn a_semantic_branch_degrades_to_default_without_intelligence() {
        // The default GraphExec::judge returns None, so a semantic branch safely
        // takes its default rather than wedging. Prove it with an exec that never
        // answers (judge_answer = None).
        let g = review_graph();
        let mut exec = MockExec::default(); // judge_answer: None
        exec.agents.insert("review".into(), (json!(0), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.terminal, Some(TerminalStatus::Refused), "None → default");
    }

    // ── P4: Wait — suspend / resume / durable state ──────────────────────────

    /// A bare Wait: suspend on `mcp://inbox`, `updated` → done(result_from evt),
    /// `timeout` → expired(deadline).
    fn wait_graph() -> Graph {
        serde_json::from_value(json!({
            "start": "w",
            "nodes": {
                "w": {"kind": "wait", "on_uri": "mcp://inbox", "writes": "evt", "timeout_ms": 5000, "edges": {"updated": "done", "timeout": "expired"}},
                "done": {"kind": "halt", "status": "completed", "result_from": "evt"},
                "expired": {"kind": "halt", "status": "deadline"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_wait_node_suspends_with_the_watch() {
        let g = wait_graph();
        assert!(g.validate().is_ok());
        let mut exec = MockExec::default();
        let DriveResult::Suspended(s) = drive(&g, &mut exec, 100) else {
            panic!("a Wait must suspend, not run to completion");
        };
        assert_eq!(s.on_uri, "mcp://inbox");
        assert_eq!(s.timeout_ms, 5000);
        assert_eq!(s.state.at, "w", "suspends AT the wait node");
    }

    #[test]
    fn resume_with_an_update_writes_the_value_and_takes_the_updated_edge() {
        let g = wait_graph();
        let mut exec = MockExec::default();
        let DriveResult::Suspended(s) = drive(&g, &mut exec, 100) else {
            panic!("suspend");
        };
        let DriveResult::Done(out) =
            resume(&g, s.state, &mut exec, WaitOutcome::Updated(json!({"msg": "hi"})))
        else {
            panic!("resume should complete the graph");
        };
        assert_eq!(out.status, GraphStatus::Completed);
        // The updated value was written to `evt` and projected as the result.
        assert_eq!(out.result, json!({"msg": "hi"}));
    }

    #[test]
    fn resume_on_timeout_takes_the_timeout_edge() {
        let g = wait_graph();
        let mut exec = MockExec::default();
        let DriveResult::Suspended(s) = drive(&g, &mut exec, 100) else {
            panic!("suspend");
        };
        let DriveResult::Done(out) = resume(&g, s.state, &mut exec, WaitOutcome::TimedOut) else {
            panic!("resume should complete");
        };
        assert_eq!(out.terminal, Some(TerminalStatus::Deadline), "timeout edge → expired");
    }

    #[test]
    fn a_suspended_graph_state_round_trips_through_serde() {
        // Durability (the state-file decision): the suspended slice survives a
        // serialize→deserialize (a process restart) and resumes correctly.
        let g = wait_graph();
        let mut exec = MockExec::default();
        let DriveResult::Suspended(s) = drive(&g, &mut exec, 100) else {
            panic!("suspend");
        };
        let json = serde_json::to_string(&s.state).expect("state serializes");
        let restored: GraphState = serde_json::from_str(&json).expect("state restores");
        assert_eq!(restored.at, "w");
        let DriveResult::Done(out) =
            resume(&g, restored, &mut exec, WaitOutcome::Updated(json!(42)))
        else {
            panic!("a restored state resumes");
        };
        assert_eq!(out.result, json!(42), "resumed from the persisted slice");
    }

    #[test]
    fn a_reactive_wait_loop_survives_far_past_the_visit_cap() {
        // tick (agent, writes n) → w (wait, updated → tick [back-edge], timeout → done).
        // A back-edge into a Wait is a long-lived reactive loop: resuming it far more
        // than MAX_VISITS_PER_NODE times must NOT trip LoopDetected/Stalled, because a
        // Wait is a checkpoint that resets per-event bookkeeping.
        let g: Graph = serde_json::from_value(json!({
            "start": "tick",
            "nodes": {
                "tick": {"kind": "agent", "instruction": "tick", "writes": "n", "edges": {"ok": "w"}},
                "w": {"kind": "wait", "on_uri": "mcp://inbox", "writes": "evt", "timeout_ms": 60000, "edges": {"updated": "tick", "timeout": "done"}},
                "done": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        assert!(g.validate().is_ok());
        let mut exec = MockExec {
            counting: Some("tick".into()),
            ..MockExec::default()
        };
        let mut result = drive(&g, &mut exec, 1_000_000);
        let events = MAX_VISITS_PER_NODE as usize + 50;
        for i in 0..events {
            let DriveResult::Suspended(s) = result else {
                panic!("iteration {i}: reactive loop must still be waiting, got {result:?}");
            };
            assert_eq!(s.on_uri, "mcp://inbox");
            result = resume(&g, s.state, &mut exec, WaitOutcome::Updated(json!(i)));
        }
        // Alive after 150 events — neither LoopDetected nor Stalled nor Exhausted.
        assert!(
            matches!(result, DriveResult::Suspended(_)),
            "a reactive Wait loop runs long past the visit cap: {result:?}"
        );
    }

    // ── P5: Subgraph ─────────────────────────────────────────────────────────

    #[test]
    fn a_sync_subgraph_runs_and_its_result_flows_into_the_parent() {
        // sg (subgraph: sub-work → halt) → done(result_from = sub_out).
        let g: Graph = serde_json::from_value(json!({
            "start": "sg",
            "nodes": {
                "sg": {
                    "kind": "subgraph",
                    "graph": {
                        "start": "s",
                        "nodes": {
                            "s": {"kind": "agent", "instruction": "sub-work", "writes": "r", "edges": {"ok": "sh"}},
                            "sh": {"kind": "halt", "status": "completed", "result_from": "r"}
                        }
                    },
                    "writes": "sub_out",
                    "edges": {"ok": "done", "error": "fail"}
                },
                "done": {"kind": "halt", "status": "completed", "result_from": "sub_out"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap();
        assert!(g.validate().is_ok(), "a graph with a nested subgraph validates");
        let mut exec = MockExec::default();
        exec.agents.insert("sub-work".into(), (json!({"did": "it"}), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed);
        // The nested graph's result flowed up into `sub_out` and was projected.
        assert_eq!(out.result, json!({"did": "it"}));
        assert!(exec.calls.iter().any(|c| c == "subgraph"));
    }

    #[test]
    fn a_subgraph_without_execution_support_takes_the_error_edge() {
        // An exec that uses the DEFAULT run_subgraph (no spawn wiring) → error edge.
        struct NoSubgraph;
        impl GraphExec for NoSubgraph {
            fn run_agent(&mut self, _: &str, _: Option<&str>, _: &Blackboard, _: &[String]) -> (Value, bool) {
                (Value::Null, false)
            }
            fn call_tool(&mut self, _: &str, _: &str, _: &Value) -> (Value, bool) {
                (Value::Null, false)
            }
        }
        let g: Graph = serde_json::from_value(json!({
            "start": "sg",
            "nodes": {
                "sg": {
                    "kind": "subgraph",
                    "graph": {"start": "x", "nodes": {"x": {"kind": "halt", "status": "completed"}}},
                    "edges": {"ok": "done", "error": "fail"}
                },
                "done": {"kind": "halt", "status": "completed"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap();
        let mut exec = NoSubgraph;
        let DriveResult::Done(out) = drive(&g, &mut exec, 100) else {
            panic!("no wait, should complete");
        };
        // Default run_subgraph → (Null, true) → the error edge → the crashed halt.
        assert_eq!(out.terminal, Some(TerminalStatus::Crashed));
    }
}
