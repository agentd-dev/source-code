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

/// Per-node visit cap (termination layer 2): a node visited more than this many
/// times is a runaway cycle → `LoopDetected`, even under a large step budget.
pub const MAX_VISITS_PER_NODE: u32 = 100;

/// The graph run budget → the layer-1 termination guard (a total node-visit cap).
/// Layers 2 (per-node visit cap) and 3 (progress guard) are enforced by the driver.
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
    // Termination bookkeeping: per-node visit count (layer 2) and the blackboard hash
    // last seen ON ENTRY to each node (layer 3 — a revisit with an unchanged board).
    let mut visits: BTreeMap<String, u32> = BTreeMap::new();
    let mut entry_hash: BTreeMap<String, u64> = BTreeMap::new();
    loop {
        // Layer 1 — total step budget.
        if !budget.step() {
            let result = bb_result(&bb, None);
            return GraphOutcome::engine(GraphStatus::Exhausted, result, budget.steps());
        }
        // Layer 2 — per-node visit cap (a runaway cycle, even under a large budget).
        let v = visits.entry(node_id.clone()).or_insert(0);
        *v += 1;
        if *v > MAX_VISITS_PER_NODE {
            let result = bb_result(&bb, None);
            return GraphOutcome::engine(GraphStatus::LoopDetected, result, budget.steps());
        }
        // Layer 3 — progress guard: re-entering a node with an unchanged blackboard
        // means the cycle back to here made no progress → stalled.
        let h = bb_hash(&bb);
        if entry_hash.get(&node_id) == Some(&h) {
            let result = bb_result(&bb, None);
            return GraphOutcome::engine(GraphStatus::Stalled, result, budget.steps());
        }
        entry_hash.insert(node_id.clone(), h);

        let Some(node) = graph.nodes.get(&node_id) else {
            // A dangling edge slipped past validation → fail closed.
            return GraphOutcome::engine(GraphStatus::Crashed, Value::Null, budget.steps());
        };

        // Effectful nodes produce `(label, edges)` and fall through to edge-follow;
        // Halt returns; Branch + the unsupported kinds transition/return directly.
        let (label, edges) = match node {
            // Halt terminates immediately, projecting `result_from`.
            Node::Halt { status, result_from } => {
                let result = bb_result(&bb, result_from.as_deref());
                return GraphOutcome::halt(*status, result, budget.steps());
            }
            // Branch: the first deterministic case whose predicate holds wins (Tier 1,
            // free). If none match and a Tier-2 semantic spec is present, ONE model
            // judgement picks a labelled choice; an unrecognised answer (or no spec)
            // takes `default`. A branch writes nothing and emits no ok/error label — it
            // transitions directly.
            Node::Branch { cases, default, semantic } => {
                node_id = if let Some(c) = cases.iter().find(|c| c.when.eval(&bb)) {
                    c.goto.clone()
                } else if let Some(spec) = semantic {
                    let labels: Vec<String> = spec.choices.keys().cloned().collect();
                    match exec.judge(&spec.prompt, &bb, &spec.reads, &labels) {
                        // Route to the chosen label's node; an unknown label → default.
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
            // Wait/Subgraph land in later phases; fail closed until then.
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
    fn an_unsupported_node_kind_fails_closed() {
        // A Wait node is a P4 feature; until then the driver must fail closed on it.
        let g: Graph = serde_json::from_value(json!({
            "start": "w",
            "nodes": {
                "w": {"kind": "wait", "on_uri": "file:///x", "timeout_ms": 1000, "edges": {"updated": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let mut exec = MockExec::default();
        let mut budget = GraphBudget::new(10);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(1000);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(1000);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(1000);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(100_000);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(100);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(100);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(100);
        let out = drive(&g, &mut exec, &mut budget);
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
        let mut budget = GraphBudget::new(100);
        let out = drive(&g, &mut exec, &mut budget);
        assert_eq!(out.terminal, Some(TerminalStatus::Refused), "None → default");
    }
}
