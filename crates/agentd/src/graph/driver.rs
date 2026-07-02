// SPDX-License-Identifier: Apache-2.0
//! The run-graph driver (pivot Phase 7 · P1) — the thin walk that turns an authored
//! [`Graph`](super::Graph) into work.
//!
//! The driver is deliberately transport-free + Session-free: it walks nodes, threads
//! a blackboard, follows labelled edges (a missing label fails CLOSED to the implicit
//! `Halt(Crashed)` safety sink), and enforces the run budget. The two effectful node
//! kinds — `Agent` (run a turn) and `Tool` (call an MCP tool) — are dispatched through
//! the [`GraphExec`] seam, implemented over a real `Session` + intelligence client in
//! production and a scripted mock in tests. So the control-flow logic (P1) is proven
//! independently of the execution wiring (a later phase), and the same driver serves
//! both the model-authored `graph.run` path and the operator `--graph <file>` path.
//!
//! P1 handles `Agent`/`Tool`/`Halt`; `Branch`/`Wait`/`Subgraph` are later phases and,
//! until then, fail CLOSED (a `Crashed` outcome) rather than panicking.

use super::{Graph, Node};
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

/// The graph run budget → the layer-1 termination guard. A total node-visit cap; the
/// per-node visit cap (layer 2) and progress guard (layer 3) arrive with cycles (P2).
#[derive(Debug, Clone)]
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
}

/// The label an effectful node emits given whether it errored.
fn edge_for(is_error: bool) -> &'static str {
    if is_error {
        "error"
    } else {
        "ok"
    }
}

/// Drive `graph` to a terminal [`GraphOutcome`], threading a fresh blackboard through
/// the node walk. P1 executes `Agent`/`Tool`/`Halt`; any other node kind, a dangling
/// edge, or an unhandled emitted label fails CLOSED (`Crashed`) — the implicit
/// `Halt(Crashed)` safety sink, so a mis-authored graph never runs away or panics.
pub fn drive(graph: &Graph, exec: &mut dyn GraphExec, budget: &mut GraphBudget) -> GraphOutcome {
    let mut bb: Blackboard = BTreeMap::new();
    let mut node_id = graph.start.clone();
    loop {
        if !budget.step() {
            let result = bb_result(&bb, None);
            return GraphOutcome::engine(GraphStatus::Exhausted, result, budget.steps());
        }
        let Some(node) = graph.nodes.get(&node_id) else {
            // A dangling edge slipped past validation → fail closed.
            return GraphOutcome::engine(GraphStatus::Crashed, Value::Null, budget.steps());
        };

        // Halt terminates immediately, projecting `result_from` from the blackboard.
        if let Node::Halt { status, result_from } = node {
            let result = bb_result(&bb, result_from.as_deref());
            return GraphOutcome::halt(*status, result, budget.steps());
        }

        // Effectful nodes: run, write, then follow the emitted label.
        let (label, edges) = match node {
            Node::Agent {
                instruction,
                output_contract,
                reads,
                writes,
                edges,
                ..
            } => {
                let (val, is_err) =
                    exec.run_agent(instruction, output_contract.as_deref(), &bb, reads);
                write(&mut bb, writes, val);
                (edge_for(is_err), edges)
            }
            Node::Tool {
                server,
                tool,
                args,
                writes,
                edges,
            } => {
                let (val, is_err) = exec.call_tool(server, tool, args);
                write(&mut bb, writes, val);
                (edge_for(is_err), edges)
            }
            // Branch/Wait/Subgraph land in later phases; fail closed until then.
            _ => return GraphOutcome::engine(GraphStatus::Crashed, Value::Null, budget.steps()),
        };

        match edges.get(label) {
            Some(next) => node_id = next.clone(),
            // Unhandled label → the implicit Halt(Crashed) safety sink.
            None => {
                let result = bb_result(&bb, None);
                return GraphOutcome::engine(GraphStatus::Crashed, result, budget.steps());
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
        let mut budget = GraphBudget::new(100);
        let out = drive(&g, &mut exec, &mut budget);

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
        let mut budget = GraphBudget::new(100);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(1);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(10);
        let out = drive(&g, &mut exec, &mut budget);
        assert_eq!(out.status, GraphStatus::Crashed);
    }

    #[test]
    fn an_unsupported_node_kind_fails_closed_in_p1() {
        // A Branch node is a P2 feature; the P1 driver must fail closed on it.
        let g: Graph = serde_json::from_value(json!({
            "start": "b",
            "nodes": {
                "b": {"kind": "branch", "cases": [], "default": "h"},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let mut exec = MockExec::default();
        let mut budget = GraphBudget::new(10);
        let out = drive(&g, &mut exec, &mut budget);
        assert_eq!(out.status, GraphStatus::Crashed);
    }
}
