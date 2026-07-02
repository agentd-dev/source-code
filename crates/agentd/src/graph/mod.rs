// SPDX-License-Identifier: Apache-2.0
//! Agent-authored cyclic run-graph (pivot Phase 7) — the serde model + validation.
//!
//! agentd already *is* an implicit single-node graph executor: the ReAct loop is a
//! hard-coded cycle, the reactive router is an event→action edge set, self-schedule
//! is a delayed self-loop, and self-subscribe is the agent adding an edge at
//! runtime. This module reifies that into an explicit serde [`Graph`] the model
//! self-authors (`graph.define`/`graph.run`/`graph.patch` self-tools) and — from
//! P1 on — a thin driver reuses `Session::run_turn`, `Budget`, `TerminalStatus`,
//! and the `Router`. It adds exactly two genuinely-new node kinds over today's
//! primitives: an explicit condition/[`Branch`](Node::Branch) and an explicit
//! wait-on-resource ([`Wait`](Node::Wait)).
//!
//! **This file is the frozen, topology-only wire type (P0).** The resume point, the
//! blackboard, and the budget live on the persisted RUN SLICE (a later phase), NOT
//! on [`Graph`] — so the authored graph stays pure, deterministic topology. Serde +
//! `serde_json::Value::pointer` only; no new deps (the minimalism moat).

use crate::agentloop::stop::TerminalStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

mod driver;
pub use driver::{
    drive, resume, Blackboard, DriveResult, GraphBudget, GraphExec, GraphOutcome, GraphState,
    GraphStatus, Suspension, WaitOutcome,
};

mod exec;
pub use exec::{drive_pinned, SessionExec, GRAPH_MAX_STEPS};

/// A node identifier within a graph (author-chosen, stable across a run).
pub type NodeId = String;
/// A well-known edge label a node emits: `ok`/`error` (Agent, Tool), `updated`/
/// `timeout` (Wait), `ok`/`error` (Subgraph). Branch uses per-case gotos, not
/// labels. An emitted label with no target is rejected at author time; at run time
/// an unhandled label falls through to the implicit `Halt(Crashed)` safety sink, so
/// a mis-authored graph fails CLOSED.
pub type EdgeLabel = String;

/// Structural caps (author-time validation) — a mis-authored graph is refused, not
/// run. Generous enough for real orchestration, tight enough to bound the driver.
pub const MAX_NODES: usize = 128;
/// Total out-edges across all nodes.
pub const MAX_EDGES: usize = 512;
/// Distinct blackboard keys a graph may write.
pub const MAX_KEYS: usize = 64;
/// Maximum `Subgraph` nesting depth.
pub const MAX_SUBGRAPH_DEPTH: u32 = 4;

/// The authored run-graph — PURE TOPOLOGY (pivot Phase 7). Serde-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    /// The entry node id.
    pub start: NodeId,
    /// The node set keyed by id (`BTreeMap` = deterministic iteration + wire order).
    pub nodes: BTreeMap<NodeId, Node>,
}

/// An ADDITIVE patch to a stored graph (pivot Phase 7 · P5 — the `graph.patch` self
/// tool): new nodes and new edges only. Never overwrites a node or retargets an edge,
/// so applying it cannot break the reachability/termination a live run relies on.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GraphPatch {
    /// New nodes to add, keyed by id (rejected if the id already exists).
    #[serde(default)]
    pub add_nodes: BTreeMap<NodeId, Node>,
    /// New out-edges to add to existing edge-bearing nodes.
    #[serde(default)]
    pub add_edges: Vec<PatchEdge>,
}

/// One additive out-edge: attach `label → to` to the existing node `from`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchEdge {
    pub from: NodeId,
    pub label: EdgeLabel,
    pub to: NodeId,
}

/// Optional per-`Agent`-node budget override (a slice of the run budget). Absent
/// fields inherit the graph/run budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NodeLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

/// A graph node. Internally tagged by `kind` so the author writes
/// `{"kind":"agent", …}`. Control-flow nodes (Agent/Tool/Wait/Subgraph) carry their
/// out-edges as a `label → target` map; `Branch` carries per-case gotos; `Halt`
/// terminates and has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    /// Run a subagent turn on `instruction` (optionally reading blackboard keys into
    /// its context and honouring an `output_contract`); write the distilled result
    /// to `writes`. Emits `ok`/`error`.
    Agent {
        instruction: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_contract: Option<String>,
        /// Blackboard keys to fold into the agent's context (RFC 6901 not applied —
        /// whole values).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reads: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limits: Option<NodeLimits>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Call one MCP `tool` on `server` with `args` (a later phase substitutes
    /// blackboard values); write the tool result to `writes`. Emits `ok`/`error`.
    Tool {
        server: String,
        tool: String,
        #[serde(default)]
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Branch on the blackboard: the first deterministic `case` whose predicate holds
    /// wins (Tier 1, free). If NONE match and an opt-in [`SemanticSpec`] is present, a
    /// single model judgement (Tier 2) picks a labelled choice; otherwise `default`.
    Branch {
        cases: Vec<Case>,
        default: NodeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        semantic: Option<SemanticSpec>,
    },
    /// Suspend until `on_uri` updates (or `timeout_ms` elapses); write the read
    /// content to `writes`. Emits `updated`/`timeout`. Costs nothing while idle.
    Wait {
        on_uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Run a nested `graph` (synchronously, or `async` as a detached subtree); write
    /// its result to `writes`. Emits `ok`/`error`. Caps are inherited by the
    /// supervisor at run time.
    Subgraph {
        graph: Box<Graph>,
        #[serde(default, rename = "async")]
        async_: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Terminate the graph with an author-chosen [`TerminalStatus`], projecting the
    /// blackboard key `result_from` as the graph result. A graph MUST have at least
    /// one `Halt` reachable from `start` (validated) — no-exit is rejected.
    Halt {
        status: TerminalStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_from: Option<String>,
    },
}

/// One `Branch` case: a deterministic predicate and where to go when it holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Case {
    /// The predicate over the blackboard that selects this case.
    pub when: Pred,
    /// The node to go to when `when` holds (first matching case wins).
    pub goto: NodeId,
}

/// A Tier-2 semantic branch (pivot Phase 7 §c, opt-in). When a `Branch`'s
/// deterministic cases all miss, the driver folds the `reads` blackboard values into
/// `prompt` and runs ONE intelligence `complete()` call with NO tools; the model's
/// free-text answer is matched (prompt-only, NOT constrained decode) against the
/// `choices` labels and routes to that label's node. An answer matching no label
/// falls through to the `Branch` default. Tokens are charged to the graph budget.
/// This is where the graph consults *intelligence*, not just structured data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticSpec {
    /// The natural-language question posed to the model.
    pub prompt: String,
    /// Blackboard keys folded into the prompt as context (whole values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<String>,
    /// The allowed answer labels → their target nodes. The model is asked to answer
    /// with exactly one label; an unrecognised answer takes the `Branch` default.
    pub choices: BTreeMap<String, NodeId>,
}

/// A Tier-1 deterministic predicate over the blackboard (pivot Phase 7 §c). Total
/// and cheap: it reads `blackboard[key]` then applies an RFC 6901 JSON `pointer`
/// (empty = the whole value), so a missing key/pointer is simply `false`. Internally
/// tagged by `op`. `All`/`Any`/`Not` compose. No new deps (`serde_json::Value::pointer`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Pred {
    /// The value at `key`+`pointer` deep-equals `value`.
    Eq {
        key: String,
        #[serde(default)]
        pointer: String,
        value: Value,
    },
    /// Does NOT deep-equal `value` (also true when the path is absent).
    Ne {
        key: String,
        #[serde(default)]
        pointer: String,
        value: Value,
    },
    /// Numerically less-than `value`.
    Lt {
        key: String,
        #[serde(default)]
        pointer: String,
        value: f64,
    },
    /// Numerically greater-than `value`.
    Gt {
        key: String,
        #[serde(default)]
        pointer: String,
        value: f64,
    },
    /// The path resolves to a present, non-null value.
    Exists {
        key: String,
        #[serde(default)]
        pointer: String,
    },
    /// A string containing the substring, or an array containing the element.
    Contains {
        key: String,
        #[serde(default)]
        pointer: String,
        value: Value,
    },
    /// Every sub-predicate holds.
    All { preds: Vec<Pred> },
    /// Any sub-predicate holds.
    Any { preds: Vec<Pred> },
    /// The sub-predicate does not hold.
    Not { pred: Box<Pred> },
}

impl Pred {
    /// Evaluate against the blackboard. Total — a missing key/pointer is `false`
    /// (never panics), so an incomplete blackboard just fails the predicate.
    pub fn eval(&self, blackboard: &BTreeMap<String, Value>) -> bool {
        match self {
            Pred::Eq { key, pointer, value } => at(blackboard, key, pointer) == Some(value),
            Pred::Ne { key, pointer, value } => at(blackboard, key, pointer) != Some(value),
            Pred::Lt { key, pointer, value } => at(blackboard, key, pointer)
                .and_then(Value::as_f64)
                .is_some_and(|x| x < *value),
            Pred::Gt { key, pointer, value } => at(blackboard, key, pointer)
                .and_then(Value::as_f64)
                .is_some_and(|x| x > *value),
            Pred::Exists { key, pointer } => {
                at(blackboard, key, pointer).is_some_and(|v| !v.is_null())
            }
            Pred::Contains { key, pointer, value } => match at(blackboard, key, pointer) {
                Some(Value::String(s)) => value.as_str().is_some_and(|needle| s.contains(needle)),
                Some(Value::Array(a)) => a.contains(value),
                _ => false,
            },
            Pred::All { preds } => preds.iter().all(|p| p.eval(blackboard)),
            Pred::Any { preds } => preds.iter().any(|p| p.eval(blackboard)),
            Pred::Not { pred } => !pred.eval(blackboard),
        }
    }
}

/// Resolve `blackboard[key]` then apply the JSON `pointer` (empty = the whole
/// value). `None` when the key is absent or the pointer does not resolve.
fn at<'a>(blackboard: &'a BTreeMap<String, Value>, key: &str, pointer: &str) -> Option<&'a Value> {
    blackboard.get(key)?.pointer(pointer)
}

impl Node {
    /// Every node id this node can transfer control to — for reachability + dangling
    /// validation, regardless of node kind. A `Halt` has none (it terminates).
    pub fn targets(&self) -> Vec<&NodeId> {
        match self {
            Node::Agent { edges, .. }
            | Node::Tool { edges, .. }
            | Node::Wait { edges, .. }
            | Node::Subgraph { edges, .. } => edges.values().collect(),
            Node::Branch { cases, default, semantic } => {
                let mut t: Vec<&NodeId> = cases.iter().map(|c| &c.goto).collect();
                t.push(default);
                // A Tier-2 semantic branch can also route to any of its choice nodes.
                if let Some(s) = semantic {
                    t.extend(s.choices.values());
                }
                t
            }
            Node::Halt { .. } => Vec::new(),
        }
    }

    fn is_halt(&self) -> bool {
        matches!(self, Node::Halt { .. })
    }

    /// Add an out-edge to an edge-bearing node (pivot Phase 7 · P5, additive patch):
    /// errs if the label already exists (no retarget) or the node carries no edge map
    /// (`Branch` routes via cases; `Halt` terminates).
    fn add_edge(&mut self, label: EdgeLabel, target: NodeId) -> Result<(), String> {
        let edges = match self {
            Node::Agent { edges, .. }
            | Node::Tool { edges, .. }
            | Node::Wait { edges, .. }
            | Node::Subgraph { edges, .. } => edges,
            Node::Branch { .. } => return Err("cannot add an edge to a Branch (use cases)".into()),
            Node::Halt { .. } => return Err("cannot add an edge to a Halt (it terminates)".into()),
        };
        if edges.contains_key(&label) {
            return Err(format!(
                "edge label {label:?} already exists (additive-only: no retarget)"
            ));
        }
        edges.insert(label, target);
        Ok(())
    }

    /// The blackboard key this node writes, if any (for the key-count cap).
    fn writes_key(&self) -> Option<&str> {
        match self {
            Node::Agent { writes, .. }
            | Node::Tool { writes, .. }
            | Node::Wait { writes, .. }
            | Node::Subgraph { writes, .. } => writes.as_deref(),
            _ => None,
        }
    }
}

impl Graph {
    /// Author-time structural validation (pivot Phase 7 §d, layer 4 — fail closed):
    /// `start` exists; no edge dangles; at least one `Halt` is reachable from `start`
    /// (no-exit rejected); every `Wait` has a non-empty uri + a non-zero timeout;
    /// caps on nodes/edges/keys/nesting hold; nested subgraphs validate recursively.
    /// Returns EVERY error found (so the author fixes them in one pass), or `Ok`.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        self.validate_into(0, &mut errs);
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    fn validate_into(&self, depth: u32, errs: &mut Vec<String>) {
        if self.nodes.is_empty() {
            errs.push("graph has no nodes".into());
            return;
        }
        if depth > MAX_SUBGRAPH_DEPTH {
            errs.push(format!(
                "subgraph nesting exceeds the max depth of {MAX_SUBGRAPH_DEPTH}"
            ));
            return; // don't descend further into a runaway nesting
        }
        if self.nodes.len() > MAX_NODES {
            errs.push(format!(
                "graph has {} nodes (max {MAX_NODES})",
                self.nodes.len()
            ));
        }
        if !self.nodes.contains_key(&self.start) {
            errs.push(format!("start node {:?} is not in the graph", self.start));
        }

        let mut keys = BTreeSet::new();
        let mut edge_count = 0usize;
        for (id, node) in &self.nodes {
            for t in node.targets() {
                edge_count += 1;
                if !self.nodes.contains_key(t) {
                    errs.push(format!("node {id:?} has an edge to unknown node {t:?}"));
                }
            }
            if let Some(k) = node.writes_key() {
                keys.insert(k.to_string());
            }
            match node {
                Node::Wait { timeout_ms, on_uri, .. } => {
                    if *timeout_ms == 0 {
                        errs.push(format!("Wait node {id:?} has timeout_ms=0 (must be > 0)"));
                    }
                    if on_uri.trim().is_empty() {
                        errs.push(format!("Wait node {id:?} has an empty on_uri"));
                    }
                }
                Node::Subgraph { graph, .. } => graph.validate_into(depth + 1, errs),
                _ => {}
            }
        }
        if edge_count > MAX_EDGES {
            errs.push(format!("graph has {edge_count} edges (max {MAX_EDGES})"));
        }
        if keys.len() > MAX_KEYS {
            errs.push(format!(
                "graph writes {} blackboard keys (max {MAX_KEYS})",
                keys.len()
            ));
        }
        // Fail-closed: a graph that cannot reach a Halt from start is rejected
        // (back-edges/cycles are fine — only "no exit at all" is the error).
        if self.nodes.contains_key(&self.start) && !self.reaches_halt() {
            errs.push(
                "no Halt node is reachable from start (the graph can never terminate)".into(),
            );
        }
    }

    /// Apply an ADDITIVE patch (pivot Phase 7 · P5): new nodes + new edges ONLY — no
    /// node overwrite, no edge retarget — so a graph can grow at runtime (like
    /// `Router::add_route`) without a live run losing reachability or a termination
    /// guarantee. Applied to a CLONE and re-validated; the graph is swapped in only on
    /// success, so a rejected patch leaves it UNCHANGED. Returns every error at once.
    pub fn apply_patch(&mut self, patch: GraphPatch) -> Result<(), Vec<String>> {
        let mut next = self.clone();
        let mut errs = Vec::new();
        for (id, node) in patch.add_nodes {
            match next.nodes.entry(id) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(node);
                }
                std::collections::btree_map::Entry::Occupied(e) => {
                    errs.push(format!(
                        "node {:?} already exists (additive-only: no overwrite)",
                        e.key()
                    ));
                }
            }
        }
        for e in patch.add_edges {
            match next.nodes.get_mut(&e.from) {
                Some(node) => {
                    if let Err(msg) = node.add_edge(e.label, e.to) {
                        errs.push(format!("add_edge on node {:?}: {msg}", e.from));
                    }
                }
                None => errs.push(format!("add_edge from unknown node {:?}", e.from)),
            }
        }
        if !errs.is_empty() {
            return Err(errs);
        }
        next.validate()?; // the grown graph must still be structurally valid
        *self = next;
        Ok(())
    }

    /// BFS/DFS from `start`; true if any reachable node is a `Halt`.
    fn reaches_halt(&self) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![self.start.clone()];
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            if node.is_halt() {
                return true;
            }
            for t in node.targets() {
                stack.push(t.clone());
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A representative valid graph: Agent → Branch → (Tool | Halt) with a back-edge
    /// (Tool → Agent, a cycle) and a reachable Halt.
    fn sample() -> Graph {
        serde_json::from_value(json!({
            "start": "fetch",
            "nodes": {
                "fetch": {
                    "kind": "agent",
                    "instruction": "fetch the next work item",
                    "writes": "item",
                    "edges": {"ok": "route", "error": "done"}
                },
                "route": {
                    "kind": "branch",
                    "cases": [
                        {"when": {"op": "eq", "key": "item", "pointer": "/status", "value": "pending"}, "goto": "work"}
                    ],
                    "default": "done"
                },
                "work": {
                    "kind": "tool",
                    "server": "fs",
                    "tool": "process",
                    "args": {"id": 1},
                    "writes": "item",
                    "edges": {"ok": "fetch", "error": "done"}
                },
                "done": {"kind": "halt", "status": "completed", "result_from": "item"}
            }
        }))
        .expect("sample parses")
    }

    #[test]
    fn a_graph_round_trips_through_serde() {
        let g = sample();
        let wire = serde_json::to_value(&g).unwrap();
        let back: Graph = serde_json::from_value(wire).unwrap();
        assert_eq!(g, back, "topology round-trips byte-for-byte");
        // The cycle (work → fetch) is representable by construction.
        assert!(matches!(g.nodes["work"], Node::Tool { .. }));
        let work_targets = g.nodes["work"].targets();
        assert!(work_targets.contains(&&"fetch".to_string()), "back-edge present");
        assert!(work_targets.contains(&&"done".to_string()), "error-edge present");
    }

    #[test]
    fn a_valid_graph_passes_validation() {
        assert!(sample().validate().is_ok());
    }

    #[test]
    fn a_missing_start_is_rejected() {
        let mut g = sample();
        g.start = "ghost".into();
        let errs = g.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("start node")), "{errs:?}");
    }

    #[test]
    fn a_dangling_edge_is_rejected() {
        let mut g = sample();
        if let Node::Agent { edges, .. } = g.nodes.get_mut("fetch").unwrap() {
            edges.insert("ok".into(), "nowhere".into());
        }
        let errs = g.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("unknown node \"nowhere\"")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_graph_that_cannot_reach_a_halt_is_rejected() {
        // fetch → fetch (a bare self-loop) with no Halt reachable.
        let g: Graph = serde_json::from_value(json!({
            "start": "spin",
            "nodes": {
                "spin": {"kind": "agent", "instruction": "spin", "edges": {"ok": "spin"}}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("no Halt")), "{errs:?}");
    }

    #[test]
    fn a_zero_timeout_wait_is_rejected() {
        let g: Graph = serde_json::from_value(json!({
            "start": "w",
            "nodes": {
                "w": {"kind": "wait", "on_uri": "file:///x", "timeout_ms": 0, "edges": {"updated": "d"}},
                "d": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("timeout_ms=0")), "{errs:?}");
    }

    #[test]
    fn too_many_nodes_is_rejected() {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "h".to_string(),
            Node::Halt { status: TerminalStatus::Completed, result_from: None },
        );
        for i in 0..=MAX_NODES {
            let mut edges = BTreeMap::new();
            edges.insert("ok".to_string(), "h".to_string());
            nodes.insert(
                format!("n{i}"),
                Node::Agent {
                    instruction: "x".into(),
                    output_contract: None,
                    reads: vec![],
                    writes: None,
                    limits: None,
                    edges,
                },
            );
        }
        let g = Graph { start: "h".into(), nodes };
        let errs = g.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("max")), "{errs:?}");
    }

    #[test]
    fn pred_eval_is_total_over_the_blackboard() {
        let mut bb = BTreeMap::new();
        bb.insert("item".to_string(), json!({"status": "ready", "n": 7}));
        let eq = Pred::Eq {
            key: "item".into(),
            pointer: "/status".into(),
            value: json!("ready"),
        };
        assert!(eq.eval(&bb));
        let gt = Pred::Gt {
            key: "item".into(),
            pointer: "/n".into(),
            value: 5.0,
        };
        assert!(gt.eval(&bb));
        // Composition + missing paths are false, never panics.
        let both = Pred::All { preds: vec![eq.clone(), gt] };
        assert!(both.eval(&bb));
        let missing = Pred::Exists {
            key: "absent".into(),
            pointer: "/x".into(),
        };
        assert!(!missing.eval(&bb));
        assert!(Pred::Not { pred: Box::new(missing) }.eval(&bb));
    }

    #[test]
    fn an_additive_patch_grows_a_graph_and_stays_valid() {
        // Start: a → h. Patch: add a node `b` and an edge a→b (on a fresh label),
        // plus b → h. The grown graph must validate.
        let mut g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "x", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let patch: GraphPatch = serde_json::from_value(json!({
            "add_nodes": {"b": {"kind": "agent", "instruction": "y", "edges": {"ok": "h"}}},
            "add_edges": [{"from": "a", "label": "error", "to": "b"}]
        }))
        .unwrap();
        g.apply_patch(patch).expect("additive patch applies");
        assert!(g.nodes.contains_key("b"));
        assert_eq!(g.nodes["a"].targets().len(), 2, "a now has ok + error edges");
        assert!(g.validate().is_ok());
    }

    #[test]
    fn a_patch_that_overwrites_or_retargets_is_rejected() {
        let mut g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "x", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        // Overwrite an existing node.
        let overwrite: GraphPatch = serde_json::from_value(json!({
            "add_nodes": {"a": {"kind": "halt", "status": "completed"}}
        }))
        .unwrap();
        assert!(g.apply_patch(overwrite).is_err(), "no node overwrite");
        // Retarget an existing edge label.
        let retarget: GraphPatch = serde_json::from_value(json!({
            "add_edges": [{"from": "a", "label": "ok", "to": "a"}]
        }))
        .unwrap();
        assert!(g.apply_patch(retarget).is_err(), "no edge retarget");
        // The graph is UNCHANGED after a rejected patch.
        assert_eq!(g.nodes["a"].targets(), vec!["h"], "rejected patch left it intact");
    }

    #[test]
    fn a_patch_that_breaks_validation_is_rejected_and_reverts() {
        let mut g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "x", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        // An edge to a node that isn't added → dangling → validation rejects the patch.
        let bad: GraphPatch = serde_json::from_value(json!({
            "add_edges": [{"from": "a", "label": "error", "to": "ghost"}]
        }))
        .unwrap();
        assert!(g.apply_patch(bad).is_err());
        assert_eq!(g.nodes.len(), 2, "graph unchanged after a rejected patch");
    }

    #[test]
    fn a_semantic_branch_dangling_choice_is_rejected() {
        // A Tier-2 choice target counts as an out-edge — a dangling one is caught.
        let g: Graph = serde_json::from_value(json!({
            "start": "b",
            "nodes": {
                "b": {"kind": "branch", "cases": [], "default": "h", "semantic": {"prompt": "?", "choices": {"yes": "ghost"}}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("unknown node \"ghost\"")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_subgraph_validates_recursively() {
        // The OUTER graph is fine, but the INNER subgraph has a dangling edge.
        let g: Graph = serde_json::from_value(json!({
            "start": "sg",
            "nodes": {
                "sg": {
                    "kind": "subgraph",
                    "graph": {
                        "start": "a",
                        "nodes": {"a": {"kind": "agent", "instruction": "x", "edges": {"ok": "ghost"}}}
                    },
                    "edges": {"ok": "done"}
                },
                "done": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("unknown node \"ghost\"")),
            "inner subgraph validated: {errs:?}"
        );
    }
}
