// SPDX-License-Identifier: Apache-2.0
//! Agent-authored cyclic workflows (pivot Phase 7) — the serde graph model + validation.
//!
//! agentd already *is* an implicit single-node graph executor: the ReAct loop is a
//! hard-coded cycle, the reactive router is an event→action edge set, self-schedule
//! is a delayed self-loop, and self-subscribe is the agent adding an edge at
//! runtime. This module reifies that into an explicit serde [`Graph`] the model
//! self-authors (`workflow.define`/`workflow.run`/`workflow.patch` self-tools) and — from
//! P1 on — a thin driver reuses `Session::run_turn`, `Budget`, `TerminalStatus`,
//! and the `Router`. Node kinds: [`Agent`](Node::Agent) (a full agentic turn),
//! [`Tool`](Node::Tool) (one MCP call, args resolved via [`resolve_refs`]),
//! [`Assign`](Node::Assign) (pure data shaping), [`Infer`](Node::Infer) (one
//! schema-checked structured intelligence call), [`Branch`](Node::Branch)
//! (deterministic predicates + an optional semantic judgement),
//! [`Foreach`](Node::Foreach) (deterministic fan-out over an array),
//! [`Wait`](Node::Wait) (suspend on a resource), [`Subgraph`](Node::Subgraph),
//! and [`Halt`](Node::Halt).
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
mod sha;
pub use driver::{
    Blackboard, CheckpointError, DriveResult, GatePayload, GraphBudget, GraphExec, GraphOutcome,
    GraphState, GraphStatus, JoinWait, MAX_VALUE_BYTES, Suspension, WaitOutcome, drive,
    drive_budgeted, drive_from, drive_seeded, resume,
};

/// The workflow-identity hash (RFC 0021 §8.2): SHA-256 of the canonical
/// (compact, key-sorted — `BTreeMap` serialization) graph JSON. A checkpoint
/// envelope binds the graph it was taken from; resume refuses a mismatch.
pub fn workflow_hash(graph: &Graph) -> String {
    sha::sha256_hex(serde_json::to_string(graph).unwrap_or_default().as_bytes())
}

/// The LIVE reactive-workflow snapshot (`agent://workflow`): the daemon
/// publishes each transition (driving / suspended / terminal) into a process-
/// global slot the served self-MCP reads — observability for a workflow that
/// lives across many child processes. Absent until a reactive workflow runs.
pub mod live {
    use serde_json::Value;
    use std::sync::{Mutex, OnceLock};

    fn slot() -> &'static Mutex<Option<Value>> {
        static SLOT: OnceLock<Mutex<Option<Value>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    /// Publish the current reactive-workflow state (replaces the prior snapshot).
    pub fn publish(v: Value) {
        if let Ok(mut s) = slot().lock() {
            *s = Some(v);
        }
    }

    /// The last-published snapshot, if any.
    pub fn snapshot() -> Option<Value> {
        slot().lock().ok().and_then(|s| s.clone())
    }
}

mod exec;
pub use exec::{
    ExecFactory, GRAPH_MAX_STEPS, SessionExec, drive_connected, drive_connected_from,
    drive_connected_once, drive_pinned,
};

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

/// The workflow dialect this build speaks (RFC 0021 §4). Dialect 1 is the
/// baseline ten-kind surface; dialect 2 adds `writes_mode` reducers, the
/// `parallel` and `human` kinds, and the `checkpoint` policy. A graph declaring
/// a HIGHER dialect than this is refused at define time (fail closed); a graph
/// merely *using* dialect-2 constructs without declaring is auto-upgraded (the
/// construct itself is the signal — the field exists for humans and tooling).
pub const DIALECT: u32 = 2;

/// The authored workflow graph — PURE TOPOLOGY (pivot Phase 7). Serde-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    /// The entry node id.
    pub start: NodeId,
    /// The node set keyed by id (`BTreeMap` = deterministic iteration + wire order).
    pub nodes: BTreeMap<NodeId, Node>,
    /// The declared workflow dialect (RFC 0021 §4). Default 1; serialized only
    /// when explicitly non-default, so dialect-1 graphs stay byte-identical.
    #[serde(
        default = "default_dialect",
        skip_serializing_if = "is_default_dialect"
    )]
    pub dialect: u32,
    /// Durable-state policy (RFC 0021 §8): checkpoint the run slice to an MCP
    /// checkpointer server after supersteps. Absent = no checkpointing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointPolicy>,
}

fn default_dialect() -> u32 {
    1
}
fn is_default_dialect(d: &u32) -> bool {
    *d == 1
}

/// The graph-level checkpoint policy (RFC 0021 §8.1): after every `every`
/// successful supersteps (and ALWAYS at a suspension and at the terminal step),
/// the driver serializes the run slice and calls `state.put` on the named MCP
/// `server`. `key` identifies the run's state lineage (`{run_id}` interpolates).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointPolicy {
    /// The declared `--mcp` server name implementing the checkpointer tool
    /// profile (`state.put`/`state.get`/`state.list`).
    pub server: String,
    /// The state key; `{run_id}` interpolates the run id. A stable
    /// operator-chosen key makes the run resumable across pod replacements.
    #[serde(default = "default_checkpoint_key")]
    pub key: String,
    /// Checkpoint after every N successful supersteps (>= 1).
    #[serde(default = "default_checkpoint_every")]
    pub every: u32,
    /// What a failed checkpoint write does to the run.
    #[serde(default)]
    pub on_error: CheckpointOnError,
}

fn default_checkpoint_key() -> String {
    "run/{run_id}".into()
}
fn default_checkpoint_every() -> u32 {
    1
}

/// Failure policy for a checkpoint write (RFC 0021 §8.1): `continue` (default —
/// durability degrades, the run does not; telemetry records the failure) or
/// `halt` (the run takes the standard failure path — for workflows where replay
/// is worse than stopping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointOnError {
    #[default]
    Continue,
    Halt,
}

/// How a node's result lands on its `writes` key (RFC 0021 §5) — the reducer.
/// Pure and synchronous; a type mismatch takes the node's `error` edge, never a
/// silent coercion. The reduce happens BEFORE the value clamp (the accumulated
/// value is what must fit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WritesMode {
    /// Replace the existing value (the dialect-1 behavior).
    #[default]
    Overwrite,
    /// Absent → `[v]`; array → push; anything else is a type error.
    Append,
    /// Absent → `v`; both objects → shallow merge (incoming wins per key);
    /// anything else is a type error.
    Merge,
    /// As `append`, but skip the incoming value if an existing element is
    /// deep-equal to it.
    Union,
}

fn is_overwrite(m: &WritesMode) -> bool {
    *m == WritesMode::Overwrite
}

/// An ADDITIVE patch to a stored graph (pivot Phase 7 · P5 — the `workflow.patch` self
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

/// Per-node retry cap (author-time validated) — bounded so a flaky node cannot
/// turn into an unbounded loop.
pub const MAX_RETRY: u32 = 5;
/// Per-node retry backoff ceiling (ms).
pub const MAX_RETRY_BACKOFF_MS: u64 = 60_000;
/// `Infer` re-ask cap: at most this many validation-feedback re-asks.
pub const MAX_INFER_RETRIES: u32 = 3;
/// `Foreach` item ceiling — the fan-out primitive is for work batches, not
/// unbounded datasets; an oversized array is an error edge, never a surprise
/// month-long walk.
pub const MAX_FOREACH_ITEMS: usize = 1024;
/// `Foreach` parallel-lane ceiling (each lane is a worker with its own
/// intelligence + MCP connections). `Parallel` branches ride the SAME lane
/// ceiling — one pool, so composition never multiplies concurrency (RFC 0021 §6).
pub const MAX_FOREACH_PARALLEL: u32 = 8;
/// `Parallel` branch ceiling (RFC 0021 §6) — named heterogeneous branches per
/// node; concurrency is separately capped by [`MAX_FOREACH_PARALLEL`].
pub const MAX_PARALLEL_BRANCHES: usize = 16;

/// An in-node retry policy for the effectful kinds (`Agent`/`Tool`/`Infer`): on an
/// error result, re-run the SAME node up to `max` more times (each attempt charges
/// the step budget), sleeping `backoff_ms` between attempts, before following the
/// `error` edge. Distinct from an authored self-edge retry loop: retries happen
/// within ONE node visit, so the loop/stall guards (which assume a revisit means
/// progress is expected) are not tripped by an intentionally-identical retry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Retry {
    /// Additional attempts after the first failure (1..=MAX_RETRY).
    pub max: u32,
    /// Sleep between attempts, in ms (0..=MAX_RETRY_BACKOFF_MS).
    #[serde(default)]
    pub backoff_ms: u64,
}

/// How a [`Node::Foreach`] treats a failing item: stop at the first failure
/// (the default — the error edge fires with the partial results), or keep
/// going and record a per-item error marker in the results array (the `ok`
/// edge fires; the author branches on the results content).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    #[default]
    FailFast,
    Continue,
}

/// The primitive type an [`Node::Infer`] schema field must satisfy. Deliberately a
/// tiny, closed set (not JSON-Schema): enough to make a model's structured answer
/// checkable + retryable, cheap enough to validate in-process with no deps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Number,
    Boolean,
    Array,
    Object,
    Any,
}

impl FieldType {
    fn matches(self, v: &Value) -> bool {
        match self {
            FieldType::String => v.is_string(),
            FieldType::Number => v.is_number(),
            FieldType::Boolean => v.is_boolean(),
            FieldType::Array => v.is_array(),
            FieldType::Object => v.is_object(),
            FieldType::Any => true,
        }
    }
}

/// Check an [`Node::Infer`] result against its schema: it must be a JSON object
/// carrying EVERY schema field with the declared type. Extra fields are allowed
/// (the schema is a floor, not a ceiling). Returns a message naming every miss —
/// the driver feeds it back to the model on a re-ask.
pub fn check_schema(schema: &BTreeMap<String, FieldType>, v: &Value) -> Result<(), String> {
    let Some(obj) = v.as_object() else {
        return Err("expected a JSON object".into());
    };
    let mut misses = Vec::new();
    for (field, ty) in schema {
        match obj.get(field) {
            None => misses.push(format!("missing field {field:?}")),
            Some(got) if !ty.matches(got) => {
                misses.push(format!("field {field:?} must be a {ty:?}"))
            }
            Some(_) => {}
        }
    }
    if misses.is_empty() {
        Ok(())
    } else {
        Err(misses.join("; "))
    }
}

/// Resolve `{"$from": key[, "pointer": ptr][, "default": v]}` references inside a
/// JSON template against the blackboard — the data-flow primitive `Tool.args` and
/// `Assign.value` use. Walks arrays/objects recursively; a ref object is replaced
/// by the blackboard value (or its `default` when the path is absent). A missing
/// path with NO default, or a ref object with unknown extra keys (a typo shield),
/// is an error — the node takes its `error` edge rather than calling a tool with a
/// silently-wrong shape.
pub fn resolve_refs(
    template: &Value,
    blackboard: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    match template {
        Value::Object(map) => {
            if let Some(from) = map.get("$from") {
                let Some(key) = from.as_str() else {
                    return Err("$from must be a string blackboard key".into());
                };
                for k in map.keys() {
                    if k != "$from" && k != "pointer" && k != "default" {
                        return Err(format!("unknown key {k:?} in a $from reference"));
                    }
                }
                let raw_pointer = map.get("pointer").and_then(Value::as_str).unwrap_or("");
                // Computed segments: `{bbkey}` inside the pointer expands to the
                // stringified SCALAR at blackboard[bbkey] — so a loop-carried
                // index addresses `/items/{index}` dynamically.
                let pointer = expand_pointer(raw_pointer, blackboard)?;
                match blackboard.get(key).and_then(|v| v.pointer(&pointer)) {
                    Some(v) => Ok(v.clone()),
                    None => match map.get("default") {
                        Some(d) => Ok(d.clone()),
                        None => Err(format!(
                            "blackboard has no value at {key:?}{pointer} (add \"default\" to make it optional)"
                        )),
                    },
                }
            } else {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (k, v) in map {
                    out.insert(k.clone(), resolve_refs(v, blackboard)?);
                }
                Ok(Value::Object(out))
            }
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                out.push(resolve_refs(v, blackboard)?);
            }
            Ok(Value::Array(out))
        }
        v => Ok(v.clone()),
    }
}

/// Expand `{bbkey}` placeholders inside an RFC 6901 pointer with the
/// stringified SCALAR (string/number/bool) at `blackboard[bbkey]` — the
/// computed-index primitive (`/items/{index}`). A placeholder whose key is
/// missing or non-scalar is an error (the node takes its `error` edge). RFC
/// 6901 escaping is applied to expanded STRING values (`~` → `~0`, `/` → `~1`)
/// so a key containing a slash cannot smuggle in extra path segments.
fn expand_pointer(pointer: &str, blackboard: &BTreeMap<String, Value>) -> Result<String, String> {
    if !pointer.contains('{') {
        return Ok(pointer.to_string());
    }
    let mut out = String::with_capacity(pointer.len());
    let mut rest = pointer;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(format!("unclosed '{{' in pointer {pointer:?}"));
        };
        let key = &after[..close];
        let seg = match blackboard.get(key) {
            Some(Value::String(s)) => s.replace('~', "~0").replace('/', "~1"),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(_) => {
                return Err(format!(
                    "pointer placeholder {{{key}}} is not a scalar blackboard value"
                ));
            }
            None => {
                return Err(format!(
                    "pointer placeholder {{{key}}} has no blackboard value"
                ));
            }
        };
        out.push_str(&seg);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
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
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limits: Option<NodeLimits>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry: Option<Retry>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Call one MCP `tool` on `server` with `args`. `args` may embed
    /// `{"$from": key[, "pointer", "default"]}` references, resolved against the
    /// blackboard just before the call ([`resolve_refs`]) — the explicit data flow
    /// from earlier nodes into a tool. Write the tool result to `writes`. Emits
    /// `ok`/`error` (an unresolvable reference is an error).
    Tool {
        server: String,
        tool: String,
        #[serde(default)]
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry: Option<Retry>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Pure data shaping — NO model call, no tool call: resolve the `value`
    /// template (with `{"$from": …}` references) against the blackboard and write
    /// the result to `writes`. Project, rename, combine, or constant-seed values so
    /// downstream `Tool.args`/`Branch` predicates get exactly the shape they need.
    /// Emits `ok`/`error` (error only on an unresolvable reference).
    Assign {
        #[serde(default)]
        value: Value,
        /// COMPUTED alternative to `value` (feature `cel`): a CEL expression
        /// over the blackboard — filter/map/aggregate/assemble without a model
        /// call or a tool round-trip. Exactly one of `value`/`expr`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expr: Option<String>,
        writes: String,
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// One structured intelligence call — the model answers `prompt` (with `reads`
    /// folded in) as a JSON object satisfying `schema` (field → [`FieldType`]).
    /// The driver validates the answer and re-asks with the validation errors up
    /// to `retries` times ([`MAX_INFER_RETRIES`] cap) before taking `error`. This
    /// is how a workflow turns free-form intelligence into CHECKED structured data
    /// the deterministic Tier-1 branches can route on. Writes the parsed object to
    /// `writes`. Emits `ok`/`error`.
    Infer {
        prompt: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reads: Vec<String>,
        schema: BTreeMap<String, FieldType>,
        /// A CEL VALUE constraint over the answer (feature `cel`): the answer's
        /// fields are top-level identifiers (e.g. `score >= 0.0 && score <= 1.0`).
        /// A type-correct answer failing the check is re-asked with the
        /// constraint named, like a schema miss.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
        /// Validation-feedback re-asks (default 1, capped at [`MAX_INFER_RETRIES`]).
        #[serde(default = "default_infer_retries")]
        retries: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry: Option<Retry>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Fan OUT over an array — the deterministic map primitive: resolve `items`
    /// (a `{"$from": …}` reference or a literal array), then run the `body`
    /// subgraph once per element. Each iteration gets a SCOPED blackboard: a
    /// clone of the parent board with `item` (the element) and `index` (its
    /// position) seeded — body writes do NOT flow back; only each body's halt
    /// result does, collected POSITIONALLY into `writes` (a failed item's slot
    /// carries `{"index", "error"}`). `parallel` > 1 runs items on that many
    /// worker lanes, each with its own intelligence/MCP connections. A body of
    /// pure tool/assign/branch nodes costs ZERO model tokens per item — this is
    /// how a big array is processed without exhausting the LLM. Emits
    /// `ok`/`error` per [`OnError`].
    Foreach {
        items: Value,
        body: Box<Graph>,
        #[serde(default = "default_parallel")]
        parallel: u32,
        #[serde(default)]
        on_error: OnError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Run NAMED heterogeneous branch bodies CONCURRENTLY in-process (RFC 0021
    /// §6) — the "review it three ways at once" primitive. Each branch is a full
    /// sub-graph run on a scoped board (a clone of the parent board with
    /// `branch` = its name seeded — like `Foreach`'s lanes, body writes do NOT
    /// flow back); results are collected into ONE OBJECT keyed by branch name
    /// (a failed branch's slot carries `{"branch","error"}`). Branches share
    /// the run's step budget and token pool; concurrency rides the same lane
    /// ceiling as `Foreach` ([`MAX_FOREACH_PARALLEL`]), so composition never
    /// multiplies lanes. Emits `ok`/`error` per [`OnError`].
    Parallel {
        branches: BTreeMap<String, Graph>,
        #[serde(default)]
        on_error: OnError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Fan IN: await previously-spawned ASYNC subgraphs. `handles` resolves
    /// against the blackboard (a handle string, the `{"handle": …}` object an
    /// async [`Subgraph`](Node::Subgraph) wrote, or an array of either); each is
    /// awaited up to the node's shared `timeout_ms`, results collected
    /// POSITIONALLY into `writes` (a failed child's slot carries
    /// `{"handle","error"}`). Emits `ok` (all completed), `error` (some child
    /// failed), or `timeout` (some child still running when the clock ran out —
    /// the collected-so-far results are written; the stragglers keep running and
    /// may be joined again). The spawn/join pair is how one workflow runs
    /// subgraphs in PARALLEL as supervised child processes.
    Join {
        handles: Value,
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
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
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// A HUMAN GATE (RFC 0021 §7): publish `payload` for a human (or any A2A
    /// peer) to inspect, signal `input-required` on the served A2A task, and
    /// SUSPEND until a reply arrives — via an A2A `SendMessage` addressed to
    /// the waiting task, or an update on `reply_uri` (any MCP resource; the
    /// standard notify-then-read). First signal wins. The reply value lands on
    /// `writes`; the node emits `replied`/`timeout`. Without a serving build
    /// this degrades to a plain wait on `reply_uri` — never a hard requirement
    /// on `--serve-mcp`. The gate deliberately does NOT encode approve/reject:
    /// the reply is data, and routing on it is a `Branch`.
    Human {
        /// The gate payload (may embed `{"$from": …}` references) — what the
        /// human is being asked to look at.
        #[serde(default)]
        payload: Value,
        /// An MCP resource whose update carries the reply (optional when the
        /// run is served over A2A — `SendMessage` is the other resume path).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_uri: Option<String>,
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        edges: BTreeMap<EdgeLabel, NodeId>,
    },
    /// Run a nested `graph`: synchronously inline (the default — its halt result
    /// lands in `writes`), or `async: true` as a SPAWNED CHILD WORKFLOW — a
    /// supervised subagent process drives it (depth/breadth/rate caps apply) and
    /// `writes` receives `{"handle": …}` immediately; collect later with a
    /// [`Join`](Node::Join) node. Either way the nested graph starts with an
    /// EMPTY blackboard (data flows OUT via its halt result, not in). Emits
    /// `ok`/`error`.
    Subgraph {
        graph: Box<Graph>,
        #[serde(default, rename = "async")]
        async_: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writes: Option<String>,
        #[serde(default, skip_serializing_if = "is_overwrite")]
        writes_mode: WritesMode,
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

fn default_infer_retries() -> u32 {
    1
}

fn default_parallel() -> u32 {
    1
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
    /// Numerically less-than `value` (a literal number, or a `{"$from": …}`
    /// reference to compare against another blackboard value).
    Lt {
        key: String,
        #[serde(default)]
        pointer: String,
        value: Value,
    },
    /// Numerically greater-than `value` (a literal number, or a `{"$from": …}`
    /// reference to compare against another blackboard value).
    Gt {
        key: String,
        #[serde(default)]
        pointer: String,
        value: Value,
    },
    /// Numerically less-than-or-equal `value` (literal or `$from` reference).
    Lte {
        key: String,
        #[serde(default)]
        pointer: String,
        value: Value,
    },
    /// Numerically greater-than-or-equal `value` (literal or `$from` reference).
    Gte {
        key: String,
        #[serde(default)]
        pointer: String,
        value: Value,
    },
    /// The value deep-equals ONE of `values` (set membership).
    In {
        key: String,
        #[serde(default)]
        pointer: String,
        values: Vec<Value>,
    },
    /// A string starting with `value`.
    StartsWith {
        key: String,
        #[serde(default)]
        pointer: String,
        value: String,
    },
    /// A string ending with `value`.
    EndsWith {
        key: String,
        #[serde(default)]
        pointer: String,
        value: String,
    },
    /// The length of a string (chars), array, or object is within `[min, max]`
    /// (either bound optional; both absent is simply "it has a length").
    Len {
        key: String,
        #[serde(default)]
        pointer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u64>,
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
    /// A CEL expression over the blackboard (feature `cel`): every blackboard
    /// key is a top-level identifier — arithmetic, string functions, and
    /// collection macros (`exists`/`filter`/`map`) the structural ops can't
    /// express. Must return a BOOL; a non-bool result, an eval error, or an
    /// unresolvable reference is `false` (fail-closed). Compile-checked at
    /// define time; a build without the feature REJECTS it there.
    Cel { expr: String },
    /// Every sub-predicate holds.
    All { preds: Vec<Pred> },
    /// Any sub-predicate holds.
    Any { preds: Vec<Pred> },
    /// The sub-predicate does not hold.
    Not { pred: Box<Pred> },
}

/// Resolve a predicate's comparison VALUE: a `{"$from": key[, "pointer": p]}`
/// object reads the blackboard (CROSS-KEY comparison — branch on one value
/// against another); anything else is the literal itself. `None` when a
/// reference does not resolve — the enclosing predicate is then `false`
/// (fail-closed, even for `ne`: an unknown right-hand side compares to nothing).
fn pred_value<'a>(value: &'a Value, blackboard: &'a BTreeMap<String, Value>) -> Option<&'a Value> {
    match value {
        Value::Object(m) if m.contains_key("$from") => {
            let key = m.get("$from")?.as_str()?;
            let pointer = m.get("pointer").and_then(Value::as_str).unwrap_or("");
            let pointer = expand_pointer(pointer, blackboard).ok()?;
            blackboard.get(key)?.pointer(&pointer)
        }
        v => Some(v),
    }
}

impl Pred {
    /// Evaluate against the blackboard. Total — a missing key/pointer is `false`
    /// (never panics), so an incomplete blackboard just fails the predicate.
    pub fn eval(&self, blackboard: &BTreeMap<String, Value>) -> bool {
        match self {
            Pred::Eq {
                key,
                pointer,
                value,
            } => match (at(blackboard, key, pointer), pred_value(value, blackboard)) {
                (Some(l), Some(r)) => l == r,
                _ => false,
            },
            Pred::Ne {
                key,
                pointer,
                value,
            } => match pred_value(value, blackboard) {
                // An absent LEFT side is "not equal" (as before); an unresolvable
                // RIGHT-side reference is fail-closed false.
                Some(r) => at(blackboard, key, pointer) != Some(r),
                None => false,
            },
            Pred::Lt {
                key,
                pointer,
                value,
            } => num_cmp(blackboard, key, pointer, value).is_some_and(|(l, r)| l < r),
            Pred::Gt {
                key,
                pointer,
                value,
            } => num_cmp(blackboard, key, pointer, value).is_some_and(|(l, r)| l > r),
            Pred::Lte {
                key,
                pointer,
                value,
            } => num_cmp(blackboard, key, pointer, value).is_some_and(|(l, r)| l <= r),
            Pred::Gte {
                key,
                pointer,
                value,
            } => num_cmp(blackboard, key, pointer, value).is_some_and(|(l, r)| l >= r),
            Pred::In {
                key,
                pointer,
                values,
            } => at(blackboard, key, pointer).is_some_and(|l| {
                values
                    .iter()
                    .any(|v| pred_value(v, blackboard).is_some_and(|r| r == l))
            }),
            Pred::StartsWith {
                key,
                pointer,
                value,
            } => at(blackboard, key, pointer)
                .and_then(Value::as_str)
                .is_some_and(|s| s.starts_with(value.as_str())),
            Pred::EndsWith {
                key,
                pointer,
                value,
            } => at(blackboard, key, pointer)
                .and_then(Value::as_str)
                .is_some_and(|s| s.ends_with(value.as_str())),
            Pred::Len {
                key,
                pointer,
                min,
                max,
            } => {
                let len = match at(blackboard, key, pointer) {
                    Some(Value::String(s)) => Some(s.chars().count() as u64),
                    Some(Value::Array(a)) => Some(a.len() as u64),
                    Some(Value::Object(o)) => Some(o.len() as u64),
                    _ => None,
                };
                len.is_some_and(|n| min.is_none_or(|lo| n >= lo) && max.is_none_or(|hi| n <= hi))
            }
            Pred::Exists { key, pointer } => {
                at(blackboard, key, pointer).is_some_and(|v| !v.is_null())
            }
            Pred::Contains {
                key,
                pointer,
                value,
            } => {
                let Some(needle) = pred_value(value, blackboard) else {
                    return false;
                };
                match at(blackboard, key, pointer) {
                    Some(Value::String(s)) => needle.as_str().is_some_and(|n| s.contains(n)),
                    Some(Value::Array(a)) => a.contains(needle),
                    _ => false,
                }
            }
            Pred::Cel { expr } => {
                crate::cel::eval_bool(expr, &crate::cel::vars_of(blackboard)).unwrap_or(false)
            }
            Pred::All { preds } => preds.iter().all(|p| p.eval(blackboard)),
            Pred::Any { preds } => preds.iter().any(|p| p.eval(blackboard)),
            Pred::Not { pred } => !pred.eval(blackboard),
        }
    }
}

impl Pred {
    /// Author-time structural sanity (recursive): a predicate that can NEVER hold
    /// (empty `In` set, inverted `Len` bounds) is almost certainly a mistake —
    /// surface it at define time instead of silently routing to `default` forever.
    fn check(&self) -> Option<String> {
        match self {
            Pred::In { values, .. } if values.is_empty() => {
                Some("`in` with an empty values set can never hold".into())
            }
            Pred::Len {
                min: Some(lo),
                max: Some(hi),
                ..
            } if lo > hi => Some(format!("`len` bounds are inverted (min {lo} > max {hi})")),
            Pred::Cel { expr } => crate::cel::compile_check(expr)
                .err()
                .map(|e| format!("`cel` predicate: {e}")),
            Pred::All { preds } | Pred::Any { preds } => preds.iter().find_map(|p| p.check()),
            Pred::Not { pred } => pred.check(),
            _ => None,
        }
    }
}

/// Resolve `blackboard[key]` then apply the JSON `pointer` (empty = the whole
/// value). `None` when the key is absent or the pointer does not resolve.
fn at<'a>(blackboard: &'a BTreeMap<String, Value>, key: &str, pointer: &str) -> Option<&'a Value> {
    blackboard.get(key)?.pointer(pointer)
}

/// Resolve both sides of a numeric comparison: the left path and the right
/// literal-or-reference, as f64s. `None` (→ predicate false) when either side
/// is absent or non-numeric.
fn num_cmp(
    blackboard: &BTreeMap<String, Value>,
    key: &str,
    pointer: &str,
    value: &Value,
) -> Option<(f64, f64)> {
    let l = at(blackboard, key, pointer).and_then(Value::as_f64)?;
    let r = pred_value(value, blackboard).and_then(Value::as_f64)?;
    Some((l, r))
}

impl Node {
    /// Every node id this node can transfer control to — for reachability + dangling
    /// validation, regardless of node kind. A `Halt` has none (it terminates).
    pub fn targets(&self) -> Vec<&NodeId> {
        match self {
            Node::Agent { edges, .. }
            | Node::Tool { edges, .. }
            | Node::Assign { edges, .. }
            | Node::Infer { edges, .. }
            | Node::Foreach { edges, .. }
            | Node::Parallel { edges, .. }
            | Node::Join { edges, .. }
            | Node::Wait { edges, .. }
            | Node::Human { edges, .. }
            | Node::Subgraph { edges, .. } => edges.values().collect(),
            Node::Branch {
                cases,
                default,
                semantic,
            } => {
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
            | Node::Assign { edges, .. }
            | Node::Infer { edges, .. }
            | Node::Foreach { edges, .. }
            | Node::Parallel { edges, .. }
            | Node::Join { edges, .. }
            | Node::Wait { edges, .. }
            | Node::Human { edges, .. }
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
            | Node::Infer { writes, .. }
            | Node::Foreach { writes, .. }
            | Node::Parallel { writes, .. }
            | Node::Join { writes, .. }
            | Node::Wait { writes, .. }
            | Node::Human { writes, .. }
            | Node::Subgraph { writes, .. } => writes.as_deref(),
            Node::Assign { writes, .. } => Some(writes),
            _ => None,
        }
    }

    /// This node's write reducer (RFC 0021 §5); `Overwrite` for non-writing kinds.
    pub(crate) fn writes_mode(&self) -> WritesMode {
        match self {
            Node::Agent { writes_mode, .. }
            | Node::Tool { writes_mode, .. }
            | Node::Assign { writes_mode, .. }
            | Node::Infer { writes_mode, .. }
            | Node::Foreach { writes_mode, .. }
            | Node::Parallel { writes_mode, .. }
            | Node::Join { writes_mode, .. }
            | Node::Wait { writes_mode, .. }
            | Node::Human { writes_mode, .. }
            | Node::Subgraph { writes_mode, .. } => *writes_mode,
            Node::Branch { .. } | Node::Halt { .. } => WritesMode::Overwrite,
        }
    }

    /// Does this node use a dialect-2 construct (RFC 0021 §4 auto-upgrade signal)?
    fn uses_dialect2(&self) -> bool {
        matches!(self, Node::Parallel { .. } | Node::Human { .. })
            || self.writes_mode() != WritesMode::Overwrite
    }

    /// This node's in-node retry policy, if any (the effectful kinds only).
    pub(crate) fn retry(&self) -> Option<&Retry> {
        match self {
            Node::Agent { retry, .. } | Node::Tool { retry, .. } | Node::Infer { retry, .. } => {
                retry.as_ref()
            }
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
        if errs.is_empty() { Ok(()) } else { Err(errs) }
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
            if let Some(r) = node.retry() {
                if r.max == 0 || r.max > MAX_RETRY {
                    errs.push(format!(
                        "node {id:?} retry.max must be 1..={MAX_RETRY} (got {})",
                        r.max
                    ));
                }
                if r.backoff_ms > MAX_RETRY_BACKOFF_MS {
                    errs.push(format!(
                        "node {id:?} retry.backoff_ms must be <= {MAX_RETRY_BACKOFF_MS} (got {})",
                        r.backoff_ms
                    ));
                }
            }
            match node {
                Node::Wait {
                    timeout_ms, on_uri, ..
                } => {
                    if *timeout_ms == 0 {
                        errs.push(format!("Wait node {id:?} has timeout_ms=0 (must be > 0)"));
                    }
                    if on_uri.trim().is_empty() {
                        errs.push(format!("Wait node {id:?} has an empty on_uri"));
                    }
                }
                Node::Assign {
                    writes,
                    value,
                    expr,
                    ..
                } => {
                    if writes.trim().is_empty() {
                        errs.push(format!("Assign node {id:?} has an empty writes key"));
                    }
                    match expr {
                        Some(e) => {
                            if !value.is_null() {
                                errs.push(format!(
                                    "Assign node {id:?} has both value and expr (choose one)"
                                ));
                            }
                            if let Err(m) = crate::cel::compile_check(e) {
                                errs.push(format!("Assign node {id:?} expr: {m}"));
                            }
                        }
                        None if value.is_null() => {
                            errs.push(format!(
                                "Assign node {id:?} needs a value or (cel builds) an expr"
                            ));
                        }
                        None => {}
                    }
                }
                Node::Infer {
                    schema,
                    retries,
                    check,
                    ..
                } => {
                    if schema.is_empty() {
                        errs.push(format!("Infer node {id:?} has an empty schema"));
                    }
                    if *retries > MAX_INFER_RETRIES {
                        errs.push(format!(
                            "Infer node {id:?} retries must be <= {MAX_INFER_RETRIES} (got {retries})"
                        ));
                    }
                    if let Some(c) = check
                        && let Err(m) = crate::cel::compile_check(c)
                    {
                        errs.push(format!("Infer node {id:?} check: {m}"));
                    }
                }
                Node::Branch { cases, .. } => {
                    for (i, c) in cases.iter().enumerate() {
                        if let Some(msg) = c.when.check() {
                            errs.push(format!("Branch node {id:?} case {i}: {msg}"));
                        }
                    }
                }
                Node::Join { timeout_ms, .. } if *timeout_ms == 0 => {
                    errs.push(format!("Join node {id:?} has timeout_ms=0 (must be > 0)"));
                }
                Node::Foreach { body, parallel, .. } => {
                    if *parallel == 0 || *parallel > MAX_FOREACH_PARALLEL {
                        errs.push(format!(
                            "Foreach node {id:?} parallel must be 1..={MAX_FOREACH_PARALLEL} (got {parallel})"
                        ));
                    }
                    // The body is a nested workflow — it counts against the same
                    // nesting depth Subgraph does.
                    body.validate_into(depth + 1, errs);
                }
                Node::Parallel { branches, .. } => {
                    if branches.is_empty() {
                        errs.push(format!("Parallel node {id:?} has no branches"));
                    }
                    if branches.len() > MAX_PARALLEL_BRANCHES {
                        errs.push(format!(
                            "Parallel node {id:?} has {} branches (max {MAX_PARALLEL_BRANCHES})",
                            branches.len()
                        ));
                    }
                    for (name, body) in branches {
                        if name.trim().is_empty() {
                            errs.push(format!("Parallel node {id:?} has an empty branch name"));
                        }
                        body.validate_into(depth + 1, errs);
                    }
                }
                Node::Human {
                    timeout_ms,
                    reply_uri,
                    ..
                } => {
                    if *timeout_ms == 0 {
                        errs.push(format!("Human node {id:?} has timeout_ms=0 (must be > 0)"));
                    }
                    if let Some(u) = reply_uri
                        && u.trim().is_empty()
                    {
                        errs.push(format!(
                            "Human node {id:?} has an empty reply_uri (omit it to rely on A2A)"
                        ));
                    }
                }
                Node::Subgraph { graph, .. } => graph.validate_into(depth + 1, errs),
                _ => {}
            }
        }
        // RFC 0021 §4: a declared dialect above this build's is refused; §8.1:
        // the checkpoint policy must be sane (the server-name existence check
        // happens at run wiring, where the configured set is known).
        if depth == 0 {
            if self.dialect > DIALECT {
                errs.push(format!(
                    "workflow dialect {} is not supported by this build (max {DIALECT})",
                    self.dialect
                ));
            }
            if let Some(cp) = &self.checkpoint {
                if cp.server.trim().is_empty() {
                    errs.push("checkpoint.server must name a configured MCP server".into());
                }
                if cp.key.trim().is_empty() {
                    errs.push("checkpoint.key must be non-empty".into());
                }
                if cp.every == 0 {
                    errs.push("checkpoint.every must be >= 1".into());
                }
            }
        } else if self.checkpoint.is_some() {
            errs.push("checkpoint policy is root-only (found on a nested graph)".into());
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

    /// The graph's EFFECTIVE dialect (RFC 0021 §4): the declared field, upgraded
    /// to 2 when any node (at any nesting depth) uses a dialect-2 construct —
    /// the construct itself is the signal.
    pub fn effective_dialect(&self) -> u32 {
        fn any_d2(g: &Graph) -> bool {
            g.checkpoint.is_some()
                || g.nodes.values().any(|n| {
                    n.uses_dialect2()
                        || match n {
                            Node::Foreach { body, .. } => any_d2(body),
                            Node::Subgraph { graph, .. } => any_d2(graph),
                            Node::Parallel { branches, .. } => branches.values().any(any_d2),
                            _ => false,
                        }
                })
        }
        if any_d2(self) {
            self.dialect.max(2)
        } else {
            self.dialect
        }
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

/// STRICT node-field validation over the RAW graph JSON (RFC 0021 §4.1, fail
/// closed). Serde's internally-tagged enums cannot `deny_unknown_fields`, so an
/// unknown field on a known kind would deserialize SILENTLY — and a dialect-2
/// field like `writes_mode` on an older build would silently overwrite where
/// the author wrote append. This walker compares each node object's keys
/// against the kind's allowlist (recursing into `foreach.body`,
/// `subgraph.graph`, and `parallel.branches`) so a typo'd or future field is a
/// DEFINE-TIME error, never a semantic change. Predicate/args/value/payload
/// interiors are data, not dialect — they are not walked.
pub fn strict_check(graph_json: &Value) -> Vec<String> {
    /// Per-kind allowed field sets (must track the [`Node`] enum exactly —
    /// `tests::strict_allowlists_track_the_node_enum` round-trips every kind).
    fn allowed(kind: &str) -> Option<&'static [&'static str]> {
        Some(match kind {
            "agent" => &[
                "kind",
                "instruction",
                "output_contract",
                "reads",
                "writes",
                "writes_mode",
                "limits",
                "retry",
                "edges",
            ],
            "tool" => &[
                "kind",
                "server",
                "tool",
                "args",
                "writes",
                "writes_mode",
                "retry",
                "edges",
            ],
            "assign" => &["kind", "value", "expr", "writes", "writes_mode", "edges"],
            "infer" => &[
                "kind",
                "prompt",
                "reads",
                "schema",
                "check",
                "writes",
                "writes_mode",
                "retries",
                "retry",
                "edges",
            ],
            "foreach" => &[
                "kind",
                "items",
                "body",
                "parallel",
                "on_error",
                "writes",
                "writes_mode",
                "edges",
            ],
            "parallel" => &[
                "kind",
                "branches",
                "on_error",
                "writes",
                "writes_mode",
                "edges",
            ],
            "join" => &[
                "kind",
                "handles",
                "timeout_ms",
                "writes",
                "writes_mode",
                "edges",
            ],
            "branch" => &["kind", "cases", "default", "semantic"],
            "wait" => &[
                "kind",
                "on_uri",
                "writes",
                "writes_mode",
                "timeout_ms",
                "edges",
            ],
            "human" => &[
                "kind",
                "payload",
                "reply_uri",
                "timeout_ms",
                "writes",
                "writes_mode",
                "edges",
            ],
            "subgraph" => &["kind", "graph", "async", "writes", "writes_mode", "edges"],
            "halt" => &["kind", "status", "result_from"],
            _ => return None, // unknown kind — serde will refuse it with its own error
        })
    }

    fn check_node(path: &str, node: &Value, errs: &mut Vec<String>) {
        let Some(obj) = node.as_object() else {
            return; // serde will refuse a non-object node
        };
        let Some(kind) = obj.get("kind").and_then(Value::as_str) else {
            return; // serde will refuse a missing/typed kind
        };
        let Some(allow) = allowed(kind) else {
            return;
        };
        for key in obj.keys() {
            if !allow.contains(&key.as_str()) {
                errs.push(format!(
                    "node {path:?} ({kind}): unknown field {key:?} (allowed: {})",
                    allow.join(", ")
                ));
            }
        }
        // Recurse into nested graph bodies.
        match kind {
            "foreach" => {
                if let Some(body) = obj.get("body") {
                    check_graph(&format!("{path}.body"), body, errs);
                }
            }
            "subgraph" => {
                if let Some(g) = obj.get("graph") {
                    check_graph(&format!("{path}.graph"), g, errs);
                }
            }
            "parallel" => {
                if let Some(branches) = obj.get("branches").and_then(Value::as_object) {
                    for (name, g) in branches {
                        check_graph(&format!("{path}.branches.{name}"), g, errs);
                    }
                }
            }
            _ => {}
        }
    }

    fn check_graph(path: &str, graph: &Value, errs: &mut Vec<String>) {
        let Some(obj) = graph.as_object() else {
            return;
        };
        const GRAPH_KEYS: &[&str] = &["start", "nodes", "dialect", "checkpoint"];
        for key in obj.keys() {
            if !GRAPH_KEYS.contains(&key.as_str()) {
                errs.push(format!(
                    "graph {path:?}: unknown field {key:?} (allowed: {})",
                    GRAPH_KEYS.join(", ")
                ));
            }
        }
        if let Some(cp) = obj.get("checkpoint").and_then(Value::as_object) {
            const CP_KEYS: &[&str] = &["server", "key", "every", "on_error"];
            for key in cp.keys() {
                if !CP_KEYS.contains(&key.as_str()) {
                    errs.push(format!(
                        "graph {path:?} checkpoint: unknown field {key:?} (allowed: {})",
                        CP_KEYS.join(", ")
                    ));
                }
            }
        }
        if let Some(nodes) = obj.get("nodes").and_then(Value::as_object) {
            for (id, node) in nodes {
                check_node(
                    &if path.is_empty() {
                        id.clone()
                    } else {
                        format!("{path}.{id}")
                    },
                    node,
                    errs,
                );
            }
        }
    }

    let mut errs = Vec::new();
    check_graph("", graph_json, &mut errs);
    errs
}

/// THE one front door for a graph entering agentd from raw JSON (RFC 0021 §4):
/// strict-field check (fail closed on unknown fields) → deserialize →
/// structural [`Graph::validate`]. Every entry point (`--workflow` file,
/// `workflow.define`, `workflow.patch` nodes) routes here so no path can admit
/// a graph the others would refuse.
pub fn parse_graph(graph_json: &Value) -> Result<Graph, Vec<String>> {
    let strict = strict_check(graph_json);
    if !strict.is_empty() {
        return Err(strict);
    }
    let graph: Graph =
        serde_json::from_value(graph_json.clone()).map_err(|e| vec![format!("parse: {e}")])?;
    graph.validate()?;
    Ok(graph)
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
        assert!(
            work_targets.contains(&&"fetch".to_string()),
            "back-edge present"
        );
        assert!(
            work_targets.contains(&&"done".to_string()),
            "error-edge present"
        );
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
            Node::Halt {
                status: TerminalStatus::Completed,
                result_from: None,
            },
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
                    writes_mode: WritesMode::Overwrite,
                    limits: None,
                    retry: None,
                    edges,
                },
            );
        }
        let g = Graph {
            start: "h".into(),
            nodes,
            dialect: 1,
            checkpoint: None,
        };
        let errs = g.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("max")), "{errs:?}");
    }

    // ── RFC 0021 §4: dialect hygiene ──────────────────────────────────────────

    #[test]
    fn strict_check_rejects_unknown_fields_on_every_kind() {
        // The §5 hazard verbatim: `writes_mode` misspelled would silently
        // overwrite where the author wrote append — strict_check catches it.
        let errs = strict_check(&json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "assign", "value": 1, "writes": "x",
                      "write_mode": "append", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }));
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("write_mode"), "{errs:?}");

        // Recursion: a typo inside a foreach body / subgraph / parallel branch
        // is found with its path.
        let errs = strict_check(&json!({
            "start": "f",
            "nodes": {
                "f": {"kind": "foreach", "items": [1], "body": {
                    "start": "b", "nodes": {
                        "b": {"kind": "tool", "server": "s", "tool": "t",
                              "arguments": {}, "edges": {"ok": "h"}},
                        "h": {"kind": "halt", "status": "completed"}
                    }}, "edges": {"ok": "h2"}},
                "h2": {"kind": "halt", "status": "completed"}
            }
        }));
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].contains("f.body.b") && errs[0].contains("arguments"),
            "{errs:?}"
        );

        // Root + checkpoint keys are strict too.
        let errs = strict_check(&json!({
            "start": "h", "node": {}, "checkpoint": {"server": "s", "ttl": 5},
            "nodes": {"h": {"kind": "halt", "status": "completed"}}
        }));
        assert_eq!(errs.len(), 2, "{errs:?}");

        // A clean dialect-2 graph passes.
        let errs = strict_check(&json!({
            "start": "p", "dialect": 2,
            "checkpoint": {"server": "state", "key": "k", "every": 2, "on_error": "halt"},
            "nodes": {
                "p": {"kind": "parallel", "branches": {"a": {"start": "h", "nodes": {
                        "h": {"kind": "halt", "status": "completed"}}}},
                      "writes": "r", "writes_mode": "merge", "edges": {"ok": "g"}},
                "g": {"kind": "human", "payload": {}, "reply_uri": "u://r",
                      "timeout_ms": 1, "writes": "v", "edges": {"replied": "h", "timeout": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }));
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn parse_graph_is_the_one_front_door() {
        // strict error → refused before deserialization.
        let errs = parse_graph(&json!({
            "start": "h",
            "nodes": {"h": {"kind": "halt", "status": "completed", "results_from": "x"}}
        }))
        .unwrap_err();
        assert!(errs[0].contains("results_from"));
        // unknown KIND → serde's fail-closed error surfaces as parse.
        let errs = parse_graph(&json!({
            "start": "h", "nodes": {"h": {"kind": "hlat", "status": "completed"}}
        }))
        .unwrap_err();
        assert!(errs[0].starts_with("parse:"), "{errs:?}");
        // structural validation still runs (no reachable halt, dangling edge…).
        let errs = parse_graph(&json!({
            "start": "a",
            "nodes": {"a": {"kind": "assign", "value": 1, "writes": "x", "edges": {"ok": "a"}}}
        }))
        .unwrap_err();
        assert!(errs.iter().any(|e| e.contains("Halt")), "{errs:?}");
    }

    #[test]
    fn dialect_gating_and_auto_upgrade() {
        // A future dialect is refused (fail closed).
        let errs = parse_graph(&json!({
            "start": "h", "dialect": 3,
            "nodes": {"h": {"kind": "halt", "status": "completed"}}
        }))
        .unwrap_err();
        assert!(errs[0].contains("dialect 3"), "{errs:?}");

        // Using a dialect-2 construct without declaring auto-upgrades.
        let g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "assign", "value": 1, "writes": "x",
                      "writes_mode": "append", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        assert_eq!(g.dialect, 1, "declared");
        assert_eq!(g.effective_dialect(), 2, "upgraded by the construct");

        // A plain dialect-1 graph stays 1 — and round-trips byte-identically
        // (no dialect/checkpoint/writes_mode keys appear on the wire).
        let g = sample();
        assert_eq!(g.effective_dialect(), 1);
        let wire = serde_json::to_value(&g).unwrap();
        assert!(wire.get("dialect").is_none());
        assert!(wire.get("checkpoint").is_none());
        assert!(
            !serde_json::to_string(&wire)
                .unwrap()
                .contains("writes_mode"),
            "default reducers are invisible on the wire"
        );
    }

    #[test]
    fn checkpoint_policy_is_validated_and_root_only() {
        let errs = parse_graph(&json!({
            "start": "h",
            "checkpoint": {"server": " ", "key": "", "every": 0},
            "nodes": {"h": {"kind": "halt", "status": "completed"}}
        }))
        .unwrap_err();
        assert_eq!(errs.len(), 3, "{errs:?}");

        // A nested checkpoint (inside a subgraph body) is refused.
        let errs = parse_graph(&json!({
            "start": "s",
            "nodes": {
                "s": {"kind": "subgraph", "graph": {
                        "start": "h", "checkpoint": {"server": "x"},
                        "nodes": {"h": {"kind": "halt", "status": "completed"}}},
                      "edges": {"ok": "h2"}},
                "h2": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap_err();
        assert!(errs.iter().any(|e| e.contains("root-only")), "{errs:?}");
    }

    #[test]
    fn parallel_validation_caps_branches_and_recurses() {
        // Empty branch set refused.
        let errs = parse_graph(&json!({
            "start": "p",
            "nodes": {
                "p": {"kind": "parallel", "branches": {}, "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap_err();
        assert!(errs.iter().any(|e| e.contains("no branches")), "{errs:?}");
        // A broken body inside a branch is found.
        let errs = parse_graph(&json!({
            "start": "p",
            "nodes": {
                "p": {"kind": "parallel",
                      "branches": {"a": {"start": "missing", "nodes": {
                          "x": {"kind": "halt", "status": "completed"}}}},
                      "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap_err();
        assert!(errs.iter().any(|e| e.contains("start node")), "{errs:?}");
        // Human validation: timeout_ms=0 refused.
        let errs = parse_graph(&json!({
            "start": "g",
            "nodes": {
                "g": {"kind": "human", "payload": {}, "timeout_ms": 0,
                      "edges": {"replied": "h", "timeout": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap_err();
        assert!(errs.iter().any(|e| e.contains("timeout_ms=0")), "{errs:?}");
    }

    #[test]
    fn workflow_hash_is_stable_and_content_sensitive() {
        let g = sample();
        let h1 = workflow_hash(&g);
        let h2 = workflow_hash(&g.clone());
        assert_eq!(h1, h2, "deterministic");
        assert_eq!(h1.len(), 64);
        let mut g2 = g.clone();
        g2.start = g2.start.clone(); // no-op → same hash
        assert_eq!(workflow_hash(&g2), h1);
        g2.nodes.remove(&g2.nodes.keys().next().unwrap().clone());
        assert_ne!(workflow_hash(&g2), h1, "content-sensitive");
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
            value: json!(5.0),
        };
        assert!(gt.eval(&bb));
        // Composition + missing paths are false, never panics.
        let both = Pred::All {
            preds: vec![eq.clone(), gt],
        };
        assert!(both.eval(&bb));
        let missing = Pred::Exists {
            key: "absent".into(),
            pointer: "/x".into(),
        };
        assert!(!missing.eval(&bb));
        assert!(
            Pred::Not {
                pred: Box::new(missing)
            }
            .eval(&bb)
        );
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
        assert_eq!(
            g.nodes["a"].targets().len(),
            2,
            "a now has ok + error edges"
        );
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
        assert_eq!(
            g.nodes["a"].targets(),
            vec!["h"],
            "rejected patch left it intact"
        );
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
    fn richer_preds_evaluate_totally() {
        let mut bb = BTreeMap::new();
        bb.insert(
            "item".to_string(),
            json!({"n": 5, "tag": "urgent-ops", "list": [1, 2, 3], "status": "open"}),
        );
        let p = |v: serde_json::Value| -> Pred { serde_json::from_value(v).unwrap() };
        assert!(p(json!({"op":"gte","key":"item","pointer":"/n","value":5.0})).eval(&bb));
        assert!(p(json!({"op":"lte","key":"item","pointer":"/n","value":5.0})).eval(&bb));
        assert!(!p(json!({"op":"gte","key":"item","pointer":"/n","value":6.0})).eval(&bb));
        assert!(
            p(json!({"op":"in","key":"item","pointer":"/status","values":["open","held"]}))
                .eval(&bb)
        );
        assert!(
            !p(json!({"op":"in","key":"item","pointer":"/status","values":["closed"]})).eval(&bb)
        );
        assert!(
            p(json!({"op":"starts_with","key":"item","pointer":"/tag","value":"urgent"})).eval(&bb)
        );
        assert!(p(json!({"op":"ends_with","key":"item","pointer":"/tag","value":"ops"})).eval(&bb));
        assert!(p(json!({"op":"len","key":"item","pointer":"/list","min":1,"max":3})).eval(&bb));
        assert!(!p(json!({"op":"len","key":"item","pointer":"/list","min":4})).eval(&bb));
        // Total over missing paths / non-strings — false, never a panic.
        assert!(!p(json!({"op":"starts_with","key":"item","pointer":"/n","value":"5"})).eval(&bb));
        assert!(!p(json!({"op":"len","key":"ghost","min":0})).eval(&bb));
    }

    #[test]
    fn an_impossible_predicate_is_rejected_at_author_time() {
        // An empty `in` set and inverted `len` bounds can never hold — a Branch
        // carrying one is refused at validation, not silently routed to default.
        let g: Graph = serde_json::from_value(json!({
            "start": "b",
            "nodes": {
                "b": {"kind": "branch", "cases": [
                    {"when": {"op": "in", "key": "k", "values": []}, "goto": "h"},
                    {"when": {"op": "not", "pred": {"op": "len", "key": "k", "min": 9, "max": 3}}, "goto": "h"}
                ], "default": "h"},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("empty values set")),
            "{errs:?}"
        );
        assert!(errs.iter().any(|e| e.contains("inverted")), "{errs:?}");
    }

    #[test]
    fn resolve_refs_substitutes_nested_defaults_and_rejects_typos() {
        let mut bb = BTreeMap::new();
        bb.insert("item".to_string(), json!({"id": 7, "meta": {"tag": "x"}}));
        // Nested substitution + pointer + default + literals pass through.
        let tpl = json!({
            "a": {"$from": "item", "pointer": "/id"},
            "b": [{"$from": "item", "pointer": "/meta/tag"}, "lit"],
            "c": {"$from": "ghost", "default": null},
            "d": 4
        });
        assert_eq!(
            resolve_refs(&tpl, &bb).unwrap(),
            json!({"a": 7, "b": ["x", "lit"], "c": null, "d": 4})
        );
        // A missing path with no default errs, naming the path.
        let e = resolve_refs(&json!({"$from": "ghost"}), &bb).unwrap_err();
        assert!(e.contains("ghost"), "{e}");
        // An unknown extra key in a ref object is a typo shield.
        let e = resolve_refs(&json!({"$from": "item", "pointr": "/id"}), &bb).unwrap_err();
        assert!(e.contains("pointr"), "{e}");
        // $from must be a string.
        assert!(resolve_refs(&json!({"$from": 3}), &bb).is_err());
    }

    #[test]
    fn check_schema_enforces_fields_and_types() {
        let schema: BTreeMap<String, FieldType> = serde_json::from_value(json!({
            "verdict": "string", "score": "number", "extra_ok": "any"
        }))
        .unwrap();
        assert!(
            check_schema(
                &schema,
                &json!({"verdict": "ok", "score": 1, "extra_ok": [1], "unlisted": true})
            )
            .is_ok()
        );
        let e = check_schema(&schema, &json!({"verdict": 5, "extra_ok": 1})).unwrap_err();
        assert!(
            e.contains("verdict") && e.contains("score"),
            "every miss named: {e}"
        );
        assert!(check_schema(&schema, &json!("not an object")).is_err());
    }

    #[test]
    fn retry_and_infer_caps_are_validated() {
        let g: Graph = serde_json::from_value(json!({
            "start": "t",
            "nodes": {
                "t": {"kind": "tool", "server": "s", "tool": "x", "retry": {"max": 99, "backoff_ms": 999999}, "edges": {"ok": "i"}},
                "i": {"kind": "infer", "prompt": "p", "schema": {}, "retries": 9, "edges": {"ok": "a"}},
                "a": {"kind": "assign", "value": 1, "writes": " ", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("retry.max")), "{errs:?}");
        assert!(
            errs.iter().any(|e| e.contains("retry.backoff_ms")),
            "{errs:?}"
        );
        assert!(errs.iter().any(|e| e.contains("empty schema")), "{errs:?}");
        assert!(
            errs.iter().any(|e| e.contains("retries must be")),
            "{errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("empty writes key")),
            "{errs:?}"
        );
    }

    #[test]
    fn assign_and_infer_nodes_round_trip_through_serde() {
        let g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "assign", "value": {"x": {"$from": "k"}}, "writes": "out", "edges": {"ok": "i"}},
                "i": {"kind": "infer", "prompt": "p", "reads": ["out"], "schema": {"v": "string"}, "writes": "c", "edges": {"ok": "h", "error": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let wire = serde_json::to_value(&g).unwrap();
        let back: Graph = serde_json::from_value(wire).unwrap();
        assert_eq!(g, back);
        assert!(matches!(g.nodes["a"], Node::Assign { .. }));
        assert!(matches!(g.nodes["i"], Node::Infer { .. }));
    }

    #[test]
    fn computed_pointer_segments_expand_from_the_blackboard() {
        let mut bb = BTreeMap::new();
        bb.insert("scan".to_string(), json!({"items": [10, 20, 30]}));
        bb.insert("index".to_string(), json!(2));
        bb.insert("field".to_string(), json!("a/b"));
        bb.insert("obj".to_string(), json!({"a/b": "slashed"}));
        // {index} expands to the scalar → /items/2.
        let v = resolve_refs(&json!({"$from": "scan", "pointer": "/items/{index}"}), &bb).unwrap();
        assert_eq!(v, json!(30));
        // A string segment is RFC-6901-escaped (a slash can't add path levels).
        let v = resolve_refs(&json!({"$from": "obj", "pointer": "/{field}"}), &bb).unwrap();
        assert_eq!(v, json!("slashed"));
        // Missing placeholder key / non-scalar / unclosed brace → errors.
        assert!(resolve_refs(&json!({"$from": "scan", "pointer": "/items/{ghost}"}), &bb).is_err());
        assert!(resolve_refs(&json!({"$from": "scan", "pointer": "/items/{scan}"}), &bb).is_err());
        assert!(resolve_refs(&json!({"$from": "scan", "pointer": "/items/{index"}), &bb).is_err());
    }

    #[test]
    fn predicates_compare_across_blackboard_keys() {
        let mut bb = BTreeMap::new();
        bb.insert("a".to_string(), json!({"n": 7, "tag": "x"}));
        bb.insert("b".to_string(), json!({"n": 7, "limit": 5}));
        let p = |v: serde_json::Value| -> Pred { serde_json::from_value(v).unwrap() };
        // eq across two keys.
        assert!(
            p(json!({"op":"eq","key":"a","pointer":"/n","value":{"$from":"b","pointer":"/n"}}))
                .eval(&bb)
        );
        // numeric compare against a referenced value.
        assert!(
            p(json!({"op":"gt","key":"a","pointer":"/n","value":{"$from":"b","pointer":"/limit"}}))
                .eval(&bb)
        );
        // in with a referenced element.
        assert!(p(json!({"op":"in","key":"a","pointer":"/tag","values":[{"$from":"a","pointer":"/tag"}]})).eval(&bb));
        // An unresolvable REFERENCE is fail-closed false — even for ne.
        assert!(
            !p(json!({"op":"ne","key":"a","pointer":"/n","value":{"$from":"ghost"}})).eval(&bb)
        );
        assert!(
            !p(json!({"op":"eq","key":"a","pointer":"/n","value":{"$from":"ghost"}})).eval(&bb)
        );
        // Literals still work exactly as before.
        assert!(p(json!({"op":"gte","key":"a","pointer":"/n","value":7})).eval(&bb));
    }

    #[test]
    fn foreach_round_trips_and_validates_caps() {
        let g: Graph = serde_json::from_value(json!({
            "start": "fan",
            "nodes": {
                "fan": {
                    "kind": "foreach",
                    "items": {"$from": "scan", "pointer": "/items"},
                    "body": {
                        "start": "work",
                        "nodes": {
                            "work": {"kind": "assign", "value": {"$from": "item"}, "writes": "out", "edges": {"ok": "h"}},
                            "h": {"kind": "halt", "status": "completed", "result_from": "out"}
                        }
                    },
                    "parallel": 4,
                    "on_error": "continue",
                    "writes": "results",
                    "edges": {"ok": "done", "error": "fail"}
                },
                "done": {"kind": "halt", "status": "completed", "result_from": "results"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap();
        assert!(g.validate().is_ok());
        let wire = serde_json::to_value(&g).unwrap();
        let back: Graph = serde_json::from_value(wire).unwrap();
        assert_eq!(g, back);
        // parallel out of range is rejected; a bad BODY is caught recursively.
        let bad: Graph = serde_json::from_value(json!({
            "start": "fan",
            "nodes": {
                "fan": {
                    "kind": "foreach", "items": [],
                    "body": {"start": "x", "nodes": {"x": {"kind": "assign", "value": 1, "writes": "o", "edges": {"ok": "ghost"}}}},
                    "parallel": 99,
                    "edges": {"ok": "done"}
                },
                "done": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = bad.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("parallel must be")),
            "{errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("unknown node \"ghost\"")),
            "{errs:?}"
        );
    }

    #[cfg(feature = "cel")]
    #[test]
    fn cel_predicates_and_assign_exprs_evaluate_and_validate() {
        let mut bb = BTreeMap::new();
        bb.insert(
            "scan".to_string(),
            json!({"items": [{"ok": true}, {"ok": false}]}),
        );
        bb.insert("limit".to_string(), json!(1));
        let p: Pred = serde_json::from_value(
            json!({"op": "cel", "expr": "scan.items.filter(i, i.ok).size() >= limit"}),
        )
        .unwrap();
        assert!(p.eval(&bb));
        // Non-bool / undeclared references are fail-closed false.
        let p: Pred =
            serde_json::from_value(json!({"op": "cel", "expr": "scan.items.size()"})).unwrap();
        assert!(!p.eval(&bb));
        let p: Pred = serde_json::from_value(json!({"op": "cel", "expr": "ghost > 1"})).unwrap();
        assert!(!p.eval(&bb));
        // A parse error is caught at DEFINE time via validation.
        let g: Graph = serde_json::from_value(json!({
            "start": "b",
            "nodes": {
                "b": {"kind": "branch", "cases": [{"when": {"op": "cel", "expr": "a >=< 1"}, "goto": "h"}], "default": "h"},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("cel") && e.contains("parse")),
            "{errs:?}"
        );
        // Assign: value XOR expr, expr compile-checked.
        let g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "assign", "value": 1, "expr": "1 + 1", "writes": "x", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("both value and expr")),
            "{errs:?}"
        );
    }

    #[cfg(all(feature = "workflow", not(feature = "cel")))]
    #[test]
    fn without_the_cel_feature_cel_surfaces_are_rejected_at_define_time() {
        // A cel predicate parses on the wire but validation names the feature.
        let g: Graph = serde_json::from_value(json!({
            "start": "b",
            "nodes": {
                "b": {"kind": "branch", "cases": [{"when": {"op": "cel", "expr": "a > 1"}, "goto": "h"}], "default": "h"},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("'cel' build feature")),
            "{errs:?}"
        );
        // And an assign expr likewise.
        let g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "assign", "expr": "1 + 1", "writes": "x", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let errs = g.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("'cel' build feature")),
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
