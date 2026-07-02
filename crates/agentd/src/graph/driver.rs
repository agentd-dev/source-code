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

use super::{FieldType, Graph, Node, NodeId, OnError, Retry};
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
    /// WHY the engine terminated the walk (set on every engine-forced status —
    /// which guard tripped, at which node); `None` for an author `Halt`. Precision
    /// for the operator: `Exhausted` alone doesn't say steps vs tokens vs deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The projected result (the `Halt.result_from` blackboard value, or null).
    pub result: Value,
    /// Total node visits taken.
    pub steps: u32,
    /// Total intelligence tokens the walk consumed (Agent turns, Infer asks,
    /// Tier-2 judgements) — the workflow's cost, for the operator/agent summary.
    #[serde(default)]
    pub tokens: u64,
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

/// The graph run budget → the layer-1 termination guard: a total node-visit cap
/// AND a total intelligence-token cap (a workflow of many Agent/Infer nodes must
/// not multiply the per-node token budget unboundedly — the whole walk shares one
/// pool). Serde-serializable so it rides the persisted [`GraphState`] across a
/// long Wait. Layers 2 (per-node visit cap) and 3 (progress guard) are enforced
/// by the driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphBudget {
    max_steps: u32,
    steps: u32,
    #[serde(default = "u64_max")]
    max_tokens: u64,
    #[serde(default)]
    tokens: u64,
}

fn u64_max() -> u64 {
    u64::MAX
}

impl GraphBudget {
    pub fn new(max_steps: u32, max_tokens: u64) -> GraphBudget {
        GraphBudget {
            max_steps,
            steps: 0,
            max_tokens,
            tokens: 0,
        }
    }

    /// Charge one node visit; `false` when the budget is spent (do not proceed).
    fn step(&mut self) -> bool {
        if self.steps >= self.max_steps {
            return false;
        }
        self.steps += 1;
        true
    }

    /// Charge intelligence tokens the exec just consumed; `false` once the pool is
    /// overdrawn (the walk stops — the tokens are already spent, so this charges
    /// first and refuses after).
    fn charge_tokens(&mut self, n: u64) -> bool {
        self.tokens = self.tokens.saturating_add(n);
        self.tokens <= self.max_tokens
    }

    pub fn steps(&self) -> u32 {
        self.steps
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
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
    /// A fresh run slice entering `start` with a total step + token cap.
    pub fn new(start: NodeId, max_steps: u32, max_tokens: u64) -> GraphState {
        GraphState {
            at: start,
            blackboard: BTreeMap::new(),
            visits: BTreeMap::new(),
            entry_hash: BTreeMap::new(),
            budget: GraphBudget::new(max_steps, max_tokens),
        }
    }

    pub fn steps(&self) -> u32 {
        self.budget.steps()
    }

    pub fn tokens(&self) -> u64 {
        self.budget.tokens()
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
    fn run_subgraph(
        &mut self,
        _graph: &Graph,
        _async_: bool,
        _blackboard: &Blackboard,
    ) -> (Value, bool) {
        (Value::Null, true)
    }

    /// Run an `Infer` node's single structured ask: fold the `reads` into `prompt`
    /// (with `feedback` from a failed validation appended on a re-ask) and return
    /// the model's answer PARSED as JSON. The driver owns schema validation + the
    /// re-ask loop. Default impl: `Err` — a build/exec with no intelligence degrades
    /// safely to the node's `error` edge.
    fn infer(
        &mut self,
        _prompt: &str,
        _blackboard: &Blackboard,
        _reads: &[String],
        _schema: &BTreeMap<String, FieldType>,
        _feedback: Option<&str>,
    ) -> Result<Value, String> {
        Err("this executor has no intelligence wired for infer".into())
    }

    /// Intelligence tokens consumed since the last call (drained: a second call
    /// returns 0 until more are spent). The driver charges these to the workflow's
    /// shared token pool after every effectful node, so N agent nodes cannot
    /// multiply the per-node budget N-fold. Default: 0 (a tokenless executor).
    fn take_tokens(&mut self) -> u64 {
        0
    }

    /// Whether the whole-workflow wall-clock deadline has passed. Checked by the
    /// driver on every node entry — a workflow stuck in slow (but individually
    /// succeeding) nodes still terminates. Default: never.
    fn deadline_exceeded(&self) -> bool {
        false
    }

    /// Run ONE `Foreach` body iteration on its scoped, pre-seeded blackboard,
    /// returning the body's projected result + whether it failed to complete.
    /// The production impl drives it inline (waits resolved); the default errs —
    /// an executor without body support degrades to the Foreach `error` edge.
    fn run_body(&mut self, _body: &Graph, _seed: Blackboard) -> (Value, bool) {
        (
            Value::String("this executor cannot run a foreach body".into()),
            true,
        )
    }

    /// Run a batch of `Foreach` iterations, up to `parallel` at a time. Returns
    /// one `(index, result, is_error)` per seed, ANY order (the driver restores
    /// positions). Default: sequential over [`GraphExec::run_body`] — a correct
    /// fallback for any executor; the production impl overrides with worker
    /// lanes that own their own connections.
    fn run_body_parallel(
        &mut self,
        body: &Graph,
        seeds: Vec<(usize, Blackboard)>,
        _parallel: u32,
    ) -> Vec<(usize, Value, bool)> {
        seeds
            .into_iter()
            .map(|(i, seed)| {
                let (v, e) = self.run_body(body, seed);
                (i, v, e)
            })
            .collect()
    }

    /// Spawn `graph` as a DETACHED CHILD WORKFLOW (a supervised subagent process
    /// drives it; depth/breadth/rate caps apply), returning its handle at once —
    /// the `Subgraph { async: true }` seam. Default: no spawn machinery wired —
    /// the node degrades to its `error` edge.
    fn spawn_subgraph(&mut self, _graph: &Graph) -> Result<String, String> {
        Err("this executor cannot spawn an async subgraph (no orchestrator wired)".into())
    }

    /// Await one previously-spawned child workflow up to `timeout_ms`: its
    /// terminal `(result, is_error)`, or [`JoinWait::Pending`] when it is still
    /// running at the deadline (the child keeps running; a later Join may catch
    /// it). Default: an error result (no spawn machinery, nothing to await).
    fn await_handle(&mut self, handle: &str, _timeout_ms: u64) -> JoinWait {
        JoinWait::Ready(
            Value::String(format!(
                "this executor cannot await '{handle}' (no orchestrator wired)"
            )),
            true,
        )
    }
}

/// One [`GraphExec::await_handle`] resolution.
pub enum JoinWait {
    /// The child reached a terminal result (`is_error` selects the edge/mark).
    Ready(Value, bool),
    /// Still running at the timeout — the Join takes its `timeout` edge.
    Pending,
}

/// Build one `Foreach` iteration's scoped blackboard: the parent board (cloned —
/// body writes never flow back) + the reserved `item` / `index` keys.
fn foreach_seed(parent: &Blackboard, index: usize, item: &Value) -> Blackboard {
    let mut seed = parent.clone();
    seed.insert("item".to_string(), item.clone());
    seed.insert("index".to_string(), Value::from(index as u64));
    seed
}

/// Extract the handle strings a `Join` awaits from its resolved `handles`
/// value: a bare string, the `{"handle": …}` object an async Subgraph wrote, or
/// an array of either. `None` for any other shape.
fn handle_list(resolved: &Value) -> Option<Vec<String>> {
    fn one(v: &Value) -> Option<String> {
        match v {
            Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            Value::Object(o) => o.get("handle").and_then(Value::as_str).map(str::to_string),
            _ => None,
        }
    }
    match resolved {
        Value::Array(items) => items.iter().map(one).collect(),
        v => one(v).map(|h| vec![h]),
    }
}

/// Per-blackboard-value size cap (serialized bytes). The blackboard is shared
/// COORDINATION state, not bulk transport — an unbounded value (a whole file, a
/// dumped dataset) multiplied by MAX_KEYS and a long walk is a real memory risk.
/// An oversized node result is replaced by a small marker and takes the `error`
/// edge (recoverable by authoring one), never silently truncated mid-JSON.
pub const MAX_VALUE_BYTES: usize = 1 << 20;

/// Clamp an effectful node's result to [`MAX_VALUE_BYTES`]: oversized → a marker
/// value + forced error (the caller's `error` edge). Cheap for small values (one
/// serialization only when the value is plausibly large).
fn clamp_value(val: Value, is_err: bool) -> (Value, bool) {
    // Fast path: primitives and short strings can't bust a 1 MiB cap.
    let approx_small = match &val {
        Value::String(s) => s.len() < MAX_VALUE_BYTES / 2,
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
        _ => false,
    };
    if approx_small {
        return (val, is_err);
    }
    let bytes = serde_json::to_string(&val)
        .map(|s| s.len())
        .unwrap_or(usize::MAX);
    if bytes <= MAX_VALUE_BYTES {
        return (val, is_err);
    }
    (
        serde_json::json!({
            "error": "workflow value too large for the blackboard",
            "bytes": bytes,
            "cap": MAX_VALUE_BYTES,
        }),
        true,
    )
}

/// The label an effectful node emits given whether it errored.
fn edge_for(is_error: bool) -> &'static str {
    if is_error { "error" } else { "ok" }
}

/// Run one effectful attempt, honouring an in-node [`Retry`] policy: on an error
/// result, re-run up to `retry.max` more times, sleeping `backoff_ms` between
/// attempts. Every retry charges the step budget; `None` means the budget ran out
/// mid-retry (the caller reports `Exhausted`). Retries stay within ONE node visit,
/// so the loop/stall guards are not tripped by an intentionally-identical retry.
fn with_retry(
    retry: Option<&Retry>,
    budget: &mut GraphBudget,
    mut attempt: impl FnMut() -> (Value, bool),
) -> Option<(Value, bool)> {
    let (mut val, mut is_err) = attempt();
    if let (true, Some(r)) = (is_err, retry) {
        for _ in 0..r.max {
            if !budget.step() {
                return None;
            }
            if r.backoff_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(r.backoff_ms));
            }
            (val, is_err) = attempt();
            if !is_err {
                break;
            }
        }
    }
    Some((val, is_err))
}

/// Start a fresh graph run: enter `start` with a total step cap of `max_steps`.
/// Returns [`DriveResult::Done`] on termination, or [`DriveResult::Suspended`] when a
/// `Wait` is reached (the daemon then arms the watch and calls [`resume`]). P1–P4
/// execute `Agent`/`Tool`/`Branch`/`Wait`/`Halt`; `Subgraph` (P5), a dangling edge, or
/// an unhandled emitted label fails CLOSED (`Crashed`) — the implicit `Halt(Crashed)`
/// safety sink, so a mis-authored graph never runs away or panics.
pub fn drive(graph: &Graph, exec: &mut dyn GraphExec, max_steps: u32) -> DriveResult {
    drive_budgeted(graph, exec, max_steps, u64::MAX)
}

/// [`drive`] with a whole-workflow intelligence-token pool: the walk terminates
/// `Exhausted` once its Agent/Infer/judge calls have consumed `max_tokens` total
/// (charged via [`GraphExec::take_tokens`]), independent of any per-node budget.
pub fn drive_budgeted(
    graph: &Graph,
    exec: &mut dyn GraphExec,
    max_steps: u32,
    max_tokens: u64,
) -> DriveResult {
    let mut state = GraphState::new(graph.start.clone(), max_steps, max_tokens);
    drive_state(graph, &mut state, exec)
}

/// [`drive`] starting from a PRE-SEEDED blackboard — the `Foreach` body entry
/// (each iteration's scoped board carries the parent values + `item`/`index`).
pub fn drive_seeded(
    graph: &Graph,
    exec: &mut dyn GraphExec,
    max_steps: u32,
    seed: Blackboard,
) -> DriveResult {
    let mut state = GraphState::new(graph.start.clone(), max_steps, u64::MAX);
    state.blackboard = seed;
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
            format!("resume at {:?}, which is not a Wait node", state.at),
            Value::Null,
            state.steps(),
            state.tokens(),
        ));
    };
    let label = match outcome {
        WaitOutcome::Updated(v) => {
            let (v, _oversized) = clamp_value(v, false);
            write(&mut state.blackboard, writes, v);
            "updated"
        }
        WaitOutcome::TimedOut => "timeout",
    };
    match edges.get(label) {
        Some(next) => state.at = next.clone(),
        None => {
            let reason = format!(
                "Wait node {:?} has no {label:?} edge for its outcome",
                state.at
            );
            let result = bb_result(&state.blackboard, None);
            return DriveResult::Done(GraphOutcome::engine(
                GraphStatus::Crashed,
                reason,
                result,
                state.steps(),
                state.tokens(),
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
        // Whole-workflow wall-clock deadline — checked on EVERY node entry, so a
        // walk of slow-but-succeeding nodes still terminates on time.
        if exec.deadline_exceeded() {
            let result = bb_result(&state.blackboard, None);
            return DriveResult::Done(GraphOutcome::engine(
                GraphStatus::Exhausted,
                "workflow deadline exceeded".into(),
                result,
                state.steps(),
                state.tokens(),
            ));
        }
        let Some(node) = graph.nodes.get(&state.at) else {
            // A dangling edge slipped past validation → fail closed.
            return DriveResult::Done(GraphOutcome::engine(
                GraphStatus::Crashed,
                format!("no such node {:?} (dangling edge)", state.at),
                Value::Null,
                state.steps(),
                state.tokens(),
            ));
        };

        // Layer 1 — total step budget (every node visit is charged).
        if !state.budget.step() {
            let result = bb_result(&state.blackboard, None);
            return DriveResult::Done(GraphOutcome::engine(
                GraphStatus::Exhausted,
                format!("step budget exhausted ({} visits)", state.steps()),
                result,
                state.steps(),
                state.tokens(),
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
                let reason = format!(
                    "node {:?} visited more than {MAX_VISITS_PER_NODE} times (runaway cycle)",
                    state.at
                );
                let result = bb_result(&state.blackboard, None);
                return DriveResult::Done(GraphOutcome::engine(
                    GraphStatus::LoopDetected,
                    reason,
                    result,
                    state.steps(),
                    state.tokens(),
                ));
            }
            let h = bb_hash(&state.blackboard);
            if state.entry_hash.get(&state.at) == Some(&h) {
                let reason = format!(
                    "re-entered node {:?} with an unchanged blackboard (no progress)",
                    state.at
                );
                let result = bb_result(&state.blackboard, None);
                return DriveResult::Done(GraphOutcome::engine(
                    GraphStatus::Stalled,
                    reason,
                    result,
                    state.steps(),
                    state.tokens(),
                ));
            }
            state.entry_hash.insert(state.at.clone(), h);
        }

        // Effectful nodes produce `(label, edges)` and fall through to edge-follow;
        // Halt/Wait return; Branch transitions directly; Subgraph fails closed (P5).
        let (label, edges) = match node {
            Node::Halt {
                status,
                result_from,
            } => {
                let result = bb_result(&state.blackboard, result_from.as_deref());
                return DriveResult::Done(GraphOutcome::halt(
                    *status,
                    result,
                    state.steps(),
                    state.tokens(),
                ));
            }
            // A Wait SUSPENDS: hand the daemon the watch (uri + timeout) and the state
            // to resume with. The current node stays `state.at` so `resume` knows which
            // Wait resolved.
            Node::Wait {
                on_uri, timeout_ms, ..
            } => {
                return DriveResult::Suspended(Suspension {
                    on_uri: on_uri.clone(),
                    timeout_ms: *timeout_ms,
                    state: state.clone(),
                });
            }
            // Branch: the first deterministic case whose predicate holds wins (Tier 1,
            // free); else a Tier-2 semantic judgement; else `default`. It writes nothing
            // and emits no ok/error label — it transitions directly.
            Node::Branch {
                cases,
                default,
                semantic,
            } => {
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
                // A Tier-2 judgement consumed tokens — charge the shared pool.
                if let Some(r) = charge(state, exec) {
                    return r;
                }
                continue;
            }
            Node::Agent {
                instruction,
                output_contract,
                reads,
                writes,
                retry,
                edges,
                ..
            } => {
                let GraphState {
                    blackboard, budget, ..
                } = state;
                let attempt = with_retry(retry.as_ref(), budget, || {
                    exec.run_agent(instruction, output_contract.as_deref(), blackboard, reads)
                });
                let Some((val, is_err)) = attempt else {
                    return exhausted(state, "step budget exhausted mid-retry");
                };
                let (val, is_err) = clamp_value(val, is_err);
                write(&mut state.blackboard, writes, val);
                (edge_for(is_err), edges)
            }
            Node::Tool {
                server,
                tool,
                args,
                writes,
                retry,
                edges,
            } => {
                // Resolve `$from` references against the CURRENT blackboard once;
                // retries re-call with the same resolved args (nothing can change
                // the board between in-node attempts). An unresolvable reference
                // is an error result — the tool is never called with a bad shape.
                let (val, is_err) = match super::resolve_refs(args, &state.blackboard) {
                    Err(e) => (Value::String(format!("argument error: {e}")), true),
                    Ok(resolved) => {
                        let GraphState { budget, .. } = state;
                        match with_retry(retry.as_ref(), budget, || {
                            exec.call_tool(server, tool, &resolved)
                        }) {
                            Some(r) => r,
                            None => return exhausted(state, "step budget exhausted mid-retry"),
                        }
                    }
                };
                let (val, is_err) = clamp_value(val, is_err);
                write(&mut state.blackboard, writes, val);
                (edge_for(is_err), edges)
            }
            // Pure data shaping: resolve the value template against the blackboard.
            // No model, no tool — deterministic and cheap.
            Node::Assign {
                value,
                expr,
                writes,
                edges,
            } => {
                let computed = match expr {
                    // Computed path (feature `cel`): the blackboard's keys are the
                    // expression's identifiers — filter/map/aggregate/assemble
                    // deterministically, zero model tokens.
                    Some(e) => crate::cel::eval_value(e, &crate::cel::vars_of(&state.blackboard)),
                    None => super::resolve_refs(value, &state.blackboard),
                };
                let (val, is_err) = match computed {
                    Ok(v) => clamp_value(v, false),
                    Err(e) => (Value::String(format!("assign error: {e}")), true),
                };
                state.blackboard.insert(writes.clone(), val);
                (edge_for(is_err), edges)
            }
            // One structured intelligence ask, schema-checked, with bounded
            // validation-feedback re-asks INSIDE the attempt (≤ MAX_INFER_RETRIES,
            // cheap) and the generic in-node retry policy around it.
            Node::Infer {
                prompt,
                reads,
                schema,
                check,
                writes,
                retries,
                retry,
                edges,
            } => {
                let GraphState {
                    blackboard, budget, ..
                } = state;
                let attempt = with_retry(retry.as_ref(), budget, || {
                    let mut feedback: Option<String> = None;
                    for _ in 0..=*retries {
                        match exec.infer(prompt, blackboard, reads, schema, feedback.as_deref()) {
                            Err(e) => return (Value::String(format!("infer error: {e}")), true),
                            Ok(v) => match super::check_schema(schema, &v)
                                .and_then(|()| infer_check(check.as_deref(), &v))
                            {
                                Ok(()) => return (v, false),
                                Err(e) => feedback = Some(e),
                            },
                        }
                    }
                    (
                        Value::String(format!(
                            "infer: the answer failed the schema after {} attempts: {}",
                            retries + 1,
                            feedback.unwrap_or_default()
                        )),
                        true,
                    )
                });
                let Some((val, is_err)) = attempt else {
                    return exhausted(state, "step budget exhausted mid-retry");
                };
                let (val, is_err) = clamp_value(val, is_err);
                write(&mut state.blackboard, writes, val);
                (edge_for(is_err), edges)
            }
            // Fan OUT over an array (the deterministic map primitive): resolve the
            // items, run the body per element on a scoped board, collect results
            // positionally. Every ITEM charges a budget step; the wall deadline is
            // honoured between items; body model usage lands on the shared token
            // pool. A tool/assign-only body costs zero model tokens per item.
            Node::Foreach {
                items,
                body,
                parallel,
                on_error,
                writes,
                edges,
            } => {
                let (val, is_err) = match super::resolve_refs(items, &state.blackboard) {
                    Err(e) => (Value::String(format!("foreach items: {e}")), true),
                    Ok(Value::Array(arr)) if arr.len() > super::MAX_FOREACH_ITEMS => (
                        Value::String(format!(
                            "foreach: {} items exceeds the cap of {}",
                            arr.len(),
                            super::MAX_FOREACH_ITEMS
                        )),
                        true,
                    ),
                    Ok(Value::Array(arr)) => {
                        match run_foreach(arr, body, *parallel, *on_error, state, exec) {
                            Ok(r) => r,
                            Err(done) => return *done,
                        }
                    }
                    Ok(other) => (
                        Value::String(format!(
                            "foreach: items must resolve to an array (got {})",
                            kind_of(&other)
                        )),
                        true,
                    ),
                };
                let (val, is_err) = clamp_value(val, is_err);
                write(&mut state.blackboard, writes, val);
                (edge_for(is_err), edges)
            }
            // Subgraph: inline (sync) through the exec seam — or `async: true`,
            // SPAWNED as a supervised child workflow whose handle is written for
            // a later Join (the fan-out half of the spawn/join pair).
            Node::Subgraph {
                graph: sub,
                async_,
                writes,
                edges,
            } => {
                let (val, is_err) = if *async_ {
                    match exec.spawn_subgraph(sub) {
                        Ok(handle) => (serde_json::json!({ "handle": handle }), false),
                        Err(e) => (Value::String(format!("async subgraph: {e}")), true),
                    }
                } else {
                    exec.run_subgraph(sub, false, &state.blackboard)
                };
                let (val, is_err) = clamp_value(val, is_err);
                write(&mut state.blackboard, writes, val);
                (edge_for(is_err), edges)
            }
            // Join: fan IN — await async-subgraph handles, collecting results
            // positionally; stragglers at the timeout take the `timeout` edge
            // (they keep running and may be joined again).
            Node::Join {
                handles,
                timeout_ms,
                writes,
                edges,
            } => {
                let started = std::time::Instant::now();
                let (val, label) = match super::resolve_refs(handles, &state.blackboard)
                    .map_err(|e| format!("join handles: {e}"))
                    .and_then(|resolved| {
                        handle_list(&resolved).ok_or_else(|| {
                            format!("join: handles must be a handle, {{\"handle\"}} object, or array of them (got {resolved})")
                        })
                    }) {
                    Err(e) => (Value::String(e), "error"),
                    Ok(list) => {
                        let mut results: Vec<Value> = Vec::with_capacity(list.len());
                        let mut label = "ok";
                        for handle in &list {
                            let elapsed = started.elapsed().as_millis() as u64;
                            let left = timeout_ms.saturating_sub(elapsed);
                            if left == 0 || exec.deadline_exceeded() {
                                label = "timeout";
                                break;
                            }
                            match exec.await_handle(handle, left) {
                                JoinWait::Ready(v, false) => results.push(v),
                                JoinWait::Ready(v, true) => {
                                    results.push(serde_json::json!({"handle": handle, "error": v}));
                                    if label == "ok" {
                                        label = "error";
                                    }
                                }
                                JoinWait::Pending => {
                                    label = "timeout";
                                    break;
                                }
                            }
                        }
                        (Value::Array(results), label)
                    }
                };
                let (val, _) = clamp_value(val, false);
                write(&mut state.blackboard, writes, val);
                match edges.get(label) {
                    Some(next) => {
                        state.at = next.clone();
                        continue;
                    }
                    None => {
                        let reason =
                            format!("node {:?} emitted unhandled label {label:?}", state.at);
                        let result = bb_result(&state.blackboard, None);
                        return DriveResult::Done(GraphOutcome::engine(
                            GraphStatus::Crashed,
                            reason,
                            result,
                            state.steps(),
                            state.tokens(),
                        ));
                    }
                }
            }
        };

        // Charge whatever intelligence tokens the node just consumed to the
        // workflow's shared pool (Agent turns, Infer asks, subgraph interiors).
        if let Some(r) = charge(state, exec) {
            return r;
        }

        match edges.get(label) {
            Some(next) => state.at = next.clone(),
            // Unhandled label → the implicit Halt(Crashed) safety sink.
            None => {
                let reason = format!("node {:?} emitted unhandled label {label:?}", state.at);
                let result = bb_result(&state.blackboard, None);
                return DriveResult::Done(GraphOutcome::engine(
                    GraphStatus::Crashed,
                    reason,
                    result,
                    state.steps(),
                    state.tokens(),
                ));
            }
        }
    }
}

/// Execute a `Foreach`'s items: sequentially (per-item budget/deadline checks)
/// or as a parallel batch (steps pre-charged; the exec owns the lanes). Returns
/// the positional results value + whether the node errs, or `Err(done)` when a
/// budget/deadline guard terminates the whole walk mid-iteration.
fn run_foreach(
    arr: Vec<Value>,
    body: &Graph,
    parallel: u32,
    on_error: OnError,
    state: &mut GraphState,
    exec: &mut dyn GraphExec,
) -> Result<(Value, bool), Box<DriveResult>> {
    let total = arr.len();
    let mut results: Vec<Value> = Vec::with_capacity(total);
    let mut any_failed = false;

    if parallel > 1 && total > 1 {
        // Parallel batch: pre-charge one step per item (refuse before running
        // anything the budget can't cover), then hand the lanes to the exec.
        for _ in 0..total {
            if !state.budget.step() {
                return Err(Box::new(exhausted(
                    state,
                    "step budget exhausted (foreach pre-charge)",
                )));
            }
        }
        let seeds: Vec<(usize, Blackboard)> = arr
            .iter()
            .enumerate()
            .map(|(i, item)| (i, foreach_seed(&state.blackboard, i, item)))
            .collect();
        let mut batch = exec.run_body_parallel(body, seeds, parallel);
        batch.sort_by_key(|(i, _, _)| *i);
        results = vec![Value::Null; total];
        for (i, v, e) in batch {
            if e {
                any_failed = true;
                results[i] = serde_json::json!({"index": i, "error": v});
            } else {
                results[i] = v;
            }
        }
        if let Some(done) = charge(state, exec) {
            return Err(Box::new(done));
        }
        if any_failed && on_error == OnError::FailFast {
            return Ok((Value::Array(results), true));
        }
    } else {
        for (i, item) in arr.iter().enumerate() {
            if exec.deadline_exceeded() {
                // Mark the unprocessed tail rather than silently shortening the
                // array — positional integrity for downstream consumers.
                for j in i..total {
                    results.push(
                        serde_json::json!({"index": j, "error": "workflow deadline exceeded"}),
                    );
                }
                return Ok((Value::Array(results), true));
            }
            if !state.budget.step() {
                return Err(Box::new(exhausted(
                    state,
                    "step budget exhausted mid-foreach",
                )));
            }
            let seed = foreach_seed(&state.blackboard, i, item);
            let (v, e) = exec.run_body(body, seed);
            if let Some(done) = charge(state, exec) {
                return Err(Box::new(done));
            }
            if e {
                any_failed = true;
                results.push(serde_json::json!({"index": i, "error": v}));
                if on_error == OnError::FailFast {
                    return Ok((Value::Array(results), true));
                }
            } else {
                results.push(v);
            }
        }
    }
    // `continue` reports ok with per-item markers in place; `fail_fast` only
    // reaches here failure-free.
    Ok((
        Value::Array(results),
        any_failed && on_error == OnError::FailFast,
    ))
}

/// The JSON kind name (for the foreach items type error).
fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Run an `Infer` node's optional CEL VALUE constraint over the (schema-valid)
/// answer object: its fields become the expression's identifiers. A violation —
/// or an eval error — reads like a schema miss, so the same re-ask loop carries
/// it back to the model with the constraint named.
fn infer_check(check: Option<&str>, answer: &Value) -> Result<(), String> {
    let Some(expr) = check else { return Ok(()) };
    let obj = answer
        .as_object()
        .expect("schema-checked answers are objects");
    let fields: BTreeMap<String, Value> = obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    match crate::cel::eval_bool(expr, &crate::cel::vars_of(&fields)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("the answer failed the check: {expr}")),
        Err(e) => Err(format!("the answer failed the check ({e}): {expr}")),
    }
}

/// The budget ran out (possibly mid-retry): report `Exhausted` with whatever the
/// blackboard holds.
fn exhausted(state: &GraphState, reason: &str) -> DriveResult {
    let result = bb_result(&state.blackboard, None);
    DriveResult::Done(GraphOutcome::engine(
        GraphStatus::Exhausted,
        reason.to_string(),
        result,
        state.steps(),
        state.tokens(),
    ))
}

/// Drain the exec's just-consumed tokens into the shared pool; `Some(done)` when
/// the pool is overdrawn (the walk must stop `Exhausted`).
fn charge(state: &mut GraphState, exec: &mut dyn GraphExec) -> Option<DriveResult> {
    let spent = exec.take_tokens();
    if state.budget.charge_tokens(spent) {
        return None;
    }
    let reason = format!(
        "token budget exhausted ({} tokens consumed)",
        state.budget.tokens()
    );
    let result = bb_result(&state.blackboard, None);
    Some(DriveResult::Done(GraphOutcome::engine(
        GraphStatus::Exhausted,
        reason,
        result,
        state.steps(),
        state.tokens(),
    )))
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
    fn halt(status: TerminalStatus, result: Value, steps: u32, tokens: u64) -> GraphOutcome {
        let gs = if status == TerminalStatus::Completed {
            GraphStatus::Completed
        } else {
            GraphStatus::Halted
        };
        GraphOutcome {
            status: gs,
            terminal: Some(status),
            reason: None,
            result,
            steps,
            tokens,
        }
    }

    fn engine(
        status: GraphStatus,
        reason: String,
        result: Value,
        steps: u32,
        tokens: u64,
    ) -> GraphOutcome {
        GraphOutcome {
            status,
            terminal: None,
            reason: Some(reason),
            result,
            steps,
            tokens,
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
        /// Per-tool scripted SEQUENCES (consumed front-first) — for retry tests
        /// where a tool must fail N times then succeed. Falls back to `tools`.
        tool_seq: BTreeMap<String, std::collections::VecDeque<(Value, bool)>>,
        calls: Vec<String>,
        last_blackboard: Blackboard,
        /// Every args value `call_tool` received, in order — proves substitution.
        tool_args: Vec<Value>,
        /// Scripted `infer` answers, consumed front-first (`Err` = transport-ish).
        infers: std::collections::VecDeque<Result<Value, String>>,
        /// The `feedback` each infer call received — proves the re-ask loop.
        infer_feedbacks: Vec<Option<String>>,
        /// When an agent's instruction equals this, return an INCREMENTING counter
        /// `{"n": k}` (a progressing loop body) instead of a scripted constant.
        counting: Option<String>,
        counter: i64,
        /// The label `judge` returns for a Tier-2 semantic branch (`None` = default).
        judge_answer: Option<String>,
        /// Async subgraphs spawned (handle = "m.<n>").
        spawned_subgraphs: Vec<Graph>,
        /// Scripted join results by handle; a handle in `join_pending` never
        /// resolves (drives the timeout edge).
        join_results: BTreeMap<String, (Value, bool)>,
        join_pending: std::collections::BTreeSet<String>,
        /// Every effectful call "costs" this many tokens (drained by take_tokens).
        tokens_per_call: u64,
        pending_tokens: u64,
        /// deadline_exceeded() turns true once this many calls were made.
        deadline_after_calls: Option<usize>,
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
            self.pending_tokens += self.tokens_per_call;
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

        fn call_tool(&mut self, server: &str, tool: &str, args: &Value) -> (Value, bool) {
            let key = format!("{server}.{tool}");
            self.calls.push(format!("tool:{key}"));
            self.pending_tokens += self.tokens_per_call;
            self.tool_args.push(args.clone());
            if let Some(seq) = self.tool_seq.get_mut(&key)
                && let Some(next) = seq.pop_front()
            {
                return next;
            }
            self.tools
                .get(&key)
                .cloned()
                .unwrap_or((Value::Null, false))
        }

        fn infer(
            &mut self,
            prompt: &str,
            _blackboard: &Blackboard,
            _reads: &[String],
            _schema: &BTreeMap<String, super::FieldType>,
            feedback: Option<&str>,
        ) -> Result<Value, String> {
            self.calls.push(format!("infer:{prompt}"));
            self.pending_tokens += self.tokens_per_call;
            self.infer_feedbacks.push(feedback.map(str::to_string));
            self.infers
                .pop_front()
                .unwrap_or(Err("no scripted infer answer".into()))
        }

        fn judge(
            &mut self,
            prompt: &str,
            _blackboard: &Blackboard,
            _reads: &[String],
            _choices: &[String],
        ) -> Option<String> {
            self.calls.push(format!("judge:{prompt}"));
            self.pending_tokens += self.tokens_per_call;
            self.judge_answer.clone()
        }

        fn take_tokens(&mut self) -> u64 {
            std::mem::take(&mut self.pending_tokens)
        }

        fn deadline_exceeded(&self) -> bool {
            self.deadline_after_calls
                .is_some_and(|n| self.calls.len() >= n)
        }

        fn run_body(&mut self, body: &Graph, seed: Blackboard) -> (Value, bool) {
            self.calls.push("body".to_string());
            match drive_seeded(body, self, 1000, seed) {
                DriveResult::Done(o) => (o.result, o.status != GraphStatus::Completed),
                DriveResult::Suspended(_) => (json!("body suspended"), true),
            }
        }

        fn spawn_subgraph(&mut self, graph: &Graph) -> Result<String, String> {
            self.calls.push("spawn_subgraph".to_string());
            self.spawned_subgraphs.push(graph.clone());
            Ok(format!("m.{}", self.spawned_subgraphs.len()))
        }

        fn await_handle(&mut self, handle: &str, _timeout_ms: u64) -> JoinWait {
            self.calls.push(format!("await:{handle}"));
            if self.join_pending.contains(handle) {
                return JoinWait::Pending;
            }
            match self.join_results.get(handle) {
                Some((v, e)) => JoinWait::Ready(v.clone(), *e),
                None => JoinWait::Ready(json!("no such child"), true),
            }
        }

        fn run_subgraph(
            &mut self,
            graph: &Graph,
            _async_: bool,
            _bb: &Blackboard,
        ) -> (Value, bool) {
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
        exec.agents
            .insert("load".into(), (json!({"loaded": 3}), false));
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
        assert_eq!(
            exec.last_blackboard.get("mid"),
            Some(&json!({"clean": true}))
        );
    }

    #[test]
    fn an_error_result_follows_the_error_edge() {
        let g = etl();
        let mut exec = MockExec::default();
        // The extract agent ERRORS → the `error` edge → the crashed Halt.
        exec.agents.insert("extract".into(), (json!("boom"), true));
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
        assert!(
            g.validate().is_ok(),
            "a cyclic graph with a reachable halt is valid"
        );
        let mut exec = MockExec {
            counting: Some("tick".into()),
            ..MockExec::default()
        };
        let out = run(&g, &mut exec, 1000);
        assert_eq!(out.status, GraphStatus::Completed);
        assert_eq!(
            out.result,
            json!({"n": 3}),
            "exited when the counter passed 2"
        );
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
        assert!(
            out.steps < 10,
            "stalls fast, not at the budget: {}",
            out.steps
        );
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
        exec.agents
            .insert("review".into(), (json!({"ok": true}), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(
            out.status,
            GraphStatus::Completed,
            "approve → approved halt"
        );
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
        exec.agents
            .insert("review".into(), (json!({"flag": true}), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(
            out.terminal,
            Some(TerminalStatus::Completed),
            "Tier-1 case won"
        );
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
        assert_eq!(
            out.terminal,
            Some(TerminalStatus::Refused),
            "None → default"
        );
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
        let DriveResult::Done(out) = resume(
            &g,
            s.state,
            &mut exec,
            WaitOutcome::Updated(json!({"msg": "hi"})),
        ) else {
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
        assert_eq!(
            out.terminal,
            Some(TerminalStatus::Deadline),
            "timeout edge → expired"
        );
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
        assert!(
            g.validate().is_ok(),
            "a graph with a nested subgraph validates"
        );
        let mut exec = MockExec::default();
        exec.agents
            .insert("sub-work".into(), (json!({"did": "it"}), false));
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
            fn run_agent(
                &mut self,
                _: &str,
                _: Option<&str>,
                _: &Blackboard,
                _: &[String],
            ) -> (Value, bool) {
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

    // ── W2: assign, $from substitution, infer, retry ─────────────────────────

    #[test]
    fn assign_shapes_data_and_tool_args_resolve_refs() {
        // fetch (agent, writes item) → shape (assign: project item/id + a constant)
        // → send (tool whose args embed $from refs) → done. Proves the data flowed
        // agent → assign → tool without a model call in between.
        let g: Graph = serde_json::from_value(json!({
            "start": "fetch",
            "nodes": {
                "fetch": {"kind": "agent", "instruction": "fetch", "writes": "item", "edges": {"ok": "shape", "error": "fail"}},
                "shape": {
                    "kind": "assign",
                    "value": {"id": {"$from": "item", "pointer": "/id"}, "mode": "fast", "missing_ok": {"$from": "item", "pointer": "/nope", "default": 0}},
                    "writes": "req",
                    "edges": {"ok": "send", "error": "fail"}
                },
                "send": {
                    "kind": "tool", "server": "q", "tool": "push",
                    "args": {"payload": {"$from": "req"}},
                    "writes": "out",
                    "edges": {"ok": "done", "error": "fail"}
                },
                "done": {"kind": "halt", "status": "completed", "result_from": "out"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap();
        assert!(g.validate().is_ok());
        let mut exec = MockExec::default();
        exec.agents
            .insert("fetch".into(), (json!({"id": 42, "noise": "x"}), false));
        exec.tools.insert("q.push".into(), (json!("queued"), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed);
        // The tool received the SHAPED args: the projected id, the constant, and
        // the defaulted missing pointer — not the raw item.
        assert_eq!(
            exec.tool_args.last().unwrap(),
            &json!({"payload": {"id": 42, "mode": "fast", "missing_ok": 0}})
        );
    }

    #[test]
    fn an_unresolvable_tool_ref_takes_the_error_edge_without_calling_the_tool() {
        let g: Graph = serde_json::from_value(json!({
            "start": "send",
            "nodes": {
                "send": {"kind": "tool", "server": "q", "tool": "push", "args": {"x": {"$from": "absent"}}, "writes": "err", "edges": {"ok": "done", "error": "fail"}},
                "done": {"kind": "halt", "status": "completed"},
                "fail": {"kind": "halt", "status": "crashed", "result_from": "err"}
            }
        }))
        .unwrap();
        let mut exec = MockExec::default();
        let out = run(&g, &mut exec, 100);
        assert_eq!(
            out.terminal,
            Some(TerminalStatus::Crashed),
            "error edge taken"
        );
        // The tool itself was NEVER called (no bad-shape call), and the error names
        // the missing key.
        assert!(exec.tool_args.is_empty(), "no tool call on a bad ref");
        assert!(
            out.result.as_str().unwrap().contains("absent"),
            "{:?}",
            out.result
        );
    }

    #[test]
    fn infer_validates_reasks_with_feedback_then_succeeds() {
        // First answer misses the schema (no "verdict"), the driver re-asks WITH
        // feedback, the second answer passes → ok edge, parsed object on the board.
        let g: Graph = serde_json::from_value(json!({
            "start": "classify",
            "nodes": {
                "classify": {
                    "kind": "infer", "prompt": "classify it",
                    "schema": {"verdict": "string", "score": "number"},
                    "retries": 2, "writes": "c",
                    "edges": {"ok": "done", "error": "fail"}
                },
                "done": {"kind": "halt", "status": "completed", "result_from": "c"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap();
        assert!(g.validate().is_ok());
        let mut exec = MockExec::default();
        exec.infers.push_back(Ok(json!({"score": 5})));
        exec.infers
            .push_back(Ok(json!({"verdict": "approve", "score": 5})));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed);
        assert_eq!(out.result, json!({"verdict": "approve", "score": 5}));
        // Two asks: the first with no feedback, the second carrying the miss.
        assert_eq!(exec.infer_feedbacks.len(), 2);
        assert!(exec.infer_feedbacks[0].is_none());
        assert!(
            exec.infer_feedbacks[1]
                .as_deref()
                .unwrap()
                .contains("verdict"),
            "{:?}",
            exec.infer_feedbacks[1]
        );
    }

    #[test]
    fn infer_exhausting_its_reasks_takes_the_error_edge() {
        let g: Graph = serde_json::from_value(json!({
            "start": "classify",
            "nodes": {
                "classify": {
                    "kind": "infer", "prompt": "classify",
                    "schema": {"verdict": "string"},
                    "retries": 1, "writes": "c",
                    "edges": {"ok": "done", "error": "fail"}
                },
                "done": {"kind": "halt", "status": "completed"},
                "fail": {"kind": "halt", "status": "refused", "result_from": "c"}
            }
        }))
        .unwrap();
        let mut exec = MockExec::default();
        exec.infers.push_back(Ok(json!({"wrong": 1})));
        exec.infers.push_back(Ok(json!(["not an object"])));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.terminal, Some(TerminalStatus::Refused), "error edge");
        assert!(
            out.result.as_str().unwrap().contains("failed the schema"),
            "{:?}",
            out.result
        );
    }

    #[test]
    fn a_retry_policy_reruns_a_flaky_tool_then_succeeds() {
        let g: Graph = serde_json::from_value(json!({
            "start": "t",
            "nodes": {
                "t": {"kind": "tool", "server": "s", "tool": "flaky", "retry": {"max": 2, "backoff_ms": 0}, "writes": "r", "edges": {"ok": "done", "error": "fail"}},
                "done": {"kind": "halt", "status": "completed", "result_from": "r"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap();
        assert!(g.validate().is_ok());
        let mut exec = MockExec::default();
        exec.tool_seq.insert(
            "s.flaky".into(),
            vec![
                (json!("boom"), true),
                (json!("boom"), true),
                (json!("finally"), false),
            ]
            .into(),
        );
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed, "3rd attempt succeeded");
        assert_eq!(out.result, json!("finally"));
        // 1 visit + 2 retries charged + the halt visit.
        assert_eq!(out.steps, 4, "each retry charges the budget");
        assert_eq!(exec.tool_args.len(), 3, "three attempts made");
    }

    // ── W3: shared token pool, workflow deadline, reasons, value clamp ───────

    #[test]
    fn the_token_pool_bounds_the_whole_workflow() {
        // Each agent/judge call costs 100 tokens; a pool of 350 stops the
        // otherwise-endless counter loop as Exhausted with a token reason —
        // independent of the (huge) step budget.
        let g = counter_loop(1_000_000);
        let mut exec = MockExec {
            counting: Some("tick".into()),
            tokens_per_call: 100,
            ..MockExec::default()
        };
        let DriveResult::Done(out) = drive_budgeted(&g, &mut exec, 100_000, 350) else {
            panic!("no wait — should terminate");
        };
        assert_eq!(out.status, GraphStatus::Exhausted);
        assert!(
            out.reason.as_deref().unwrap().contains("token"),
            "{:?}",
            out.reason
        );
        assert!(
            out.tokens >= 350,
            "the spent pool is reported: {}",
            out.tokens
        );
    }

    #[test]
    fn the_workflow_deadline_terminates_the_walk() {
        // The exec's wall-clock deadline flips after 3 calls: the walk stops
        // Exhausted with a deadline reason even though every node succeeds.
        let g = counter_loop(1_000_000);
        let mut exec = MockExec {
            counting: Some("tick".into()),
            deadline_after_calls: Some(3),
            ..MockExec::default()
        };
        let out = run(&g, &mut exec, 100_000);
        assert_eq!(out.status, GraphStatus::Exhausted);
        assert!(
            out.reason.as_deref().unwrap().contains("deadline"),
            "{:?}",
            out.reason
        );
    }

    #[test]
    fn engine_reasons_name_the_guard_and_the_node() {
        // Stall: the reason names the re-entered node.
        let g: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "const", "writes": "k", "edges": {"ok": "b"}},
                "b": {"kind": "branch", "cases": [{"when": {"op": "eq", "key": "k", "value": "never"}, "goto": "done"}], "default": "a"},
                "done": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let mut exec = MockExec::default();
        exec.agents.insert("const".into(), (json!("same"), false));
        let out = run(&g, &mut exec, 1000);
        assert_eq!(out.status, GraphStatus::Stalled);
        // The guard fires at the first node RE-ENTERED with an unchanged board —
        // the branch "b" (a → b → a → b: b sees the same board first).
        assert!(
            out.reason.as_deref().unwrap().contains("\"b\""),
            "{:?}",
            out.reason
        );
        // Unhandled label: the reason names the node and the label.
        let g2: Graph = serde_json::from_value(json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "x", "edges": {"error": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        }))
        .unwrap();
        let mut exec2 = MockExec::default();
        let out2 = run(&g2, &mut exec2, 10);
        assert_eq!(out2.status, GraphStatus::Crashed);
        let r = out2.reason.as_deref().unwrap();
        assert!(r.contains("\"a\"") && r.contains("\"ok\""), "{r}");
        // An author Halt carries NO engine reason.
        let g3 = etl();
        let mut exec3 = MockExec::default();
        let out3 = run(&g3, &mut exec3, 100);
        assert!(out3.reason.is_none(), "author halts are reason-free");
    }

    #[test]
    fn an_oversized_value_is_clamped_to_the_error_edge() {
        // A ~2 MiB agent result busts MAX_VALUE_BYTES: the blackboard gets a small
        // marker instead, and the node takes its error edge.
        let g: Graph = serde_json::from_value(json!({
            "start": "big",
            "nodes": {
                "big": {"kind": "agent", "instruction": "dump", "writes": "blob", "edges": {"ok": "done", "error": "fail"}},
                "done": {"kind": "halt", "status": "completed"},
                "fail": {"kind": "halt", "status": "crashed", "result_from": "blob"}
            }
        }))
        .unwrap();
        let mut exec = MockExec::default();
        exec.agents.insert(
            "dump".into(),
            (Value::String("x".repeat(2 * 1024 * 1024)), false),
        );
        let out = run(&g, &mut exec, 100);
        assert_eq!(
            out.terminal,
            Some(TerminalStatus::Crashed),
            "error edge taken"
        );
        assert!(
            out.result.get("error").is_some(),
            "a small marker replaced the blob: {:?}",
            out.result
        );
        let stored = serde_json::to_string(&out.result).unwrap();
        assert!(
            stored.len() < 1024,
            "the marker is small: {} bytes",
            stored.len()
        );
    }

    // ── W6: foreach — the deterministic fan-out primitive ────────────────────

    /// scan (tool: returns an items array) → fan (foreach: tool body per item,
    /// args computed from the scoped `item`) → done. NO agent nodes anywhere —
    /// the whole fan-out costs zero model calls.
    fn fanout_graph(on_error: &str, parallel: u32) -> Graph {
        serde_json::from_value(json!({
            "start": "scan",
            "nodes": {
                "scan": {"kind": "tool", "server": "q", "tool": "scan", "writes": "scan", "edges": {"ok": "fan", "error": "fail"}},
                "fan": {
                    "kind": "foreach",
                    "items": {"$from": "scan", "pointer": "/items"},
                    "body": {
                        "start": "handle",
                        "nodes": {
                            "handle": {"kind": "tool", "server": "q", "tool": "handle", "args": {"id": {"$from": "item", "pointer": "/id"}, "pos": {"$from": "index"}}, "writes": "out", "edges": {"ok": "bh", "error": "bf"}},
                            "bh": {"kind": "halt", "status": "completed", "result_from": "out"},
                            "bf": {"kind": "halt", "status": "crashed", "result_from": "out"}
                        }
                    },
                    "parallel": parallel,
                    "on_error": on_error,
                    "writes": "results",
                    "edges": {"ok": "done", "error": "fail"}
                },
                "done": {"kind": "halt", "status": "completed", "result_from": "results"},
                "fail": {"kind": "halt", "status": "crashed", "result_from": "results"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn foreach_fans_out_over_an_array_without_any_model_call() {
        let g = fanout_graph("fail_fast", 1);
        assert!(g.validate().is_ok());
        let mut exec = MockExec::default();
        exec.tools.insert(
            "q.scan".into(),
            (
                json!({"items": [{"id": "a"}, {"id": "b"}, {"id": "c"}]}),
                false,
            ),
        );
        exec.tools
            .insert("q.handle".into(), (json!("handled"), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed, "{:?}", out.reason);
        assert_eq!(out.result, json!(["handled", "handled", "handled"]));
        // The per-item tool calls received the SCOPED item/index via $from.
        let handle_args: Vec<&Value> = exec
            .tool_args
            .iter()
            .filter(|a| a.get("id").is_some())
            .collect();
        assert_eq!(handle_args.len(), 3);
        assert_eq!(handle_args[0], &json!({"id": "a", "pos": 0}));
        assert_eq!(handle_args[2], &json!({"id": "c", "pos": 2}));
        // ZERO model involvement: no agent/infer/judge calls anywhere.
        assert!(
            !exec.calls.iter().any(|c| c.starts_with("agent:")
                || c.starts_with("infer:")
                || c.starts_with("judge:")),
            "{:?}",
            exec.calls
        );
        // Steps: scan + fan visit + 3 items + (body nodes are inner budget) + done.
        assert!(out.steps >= 5, "items charged: {}", out.steps);
    }

    #[test]
    fn foreach_continue_records_per_item_errors_positionally() {
        let g = fanout_graph("continue", 1);
        let mut exec = MockExec::default();
        exec.tools.insert(
            "q.scan".into(),
            (
                json!({"items": [{"id": "a"}, {"id": "b"}, {"id": "c"}]}),
                false,
            ),
        );
        // b fails, a and c succeed.
        exec.tool_seq.insert(
            "q.handle".into(),
            vec![
                (json!("ok-a"), false),
                (json!("boom-b"), true),
                (json!("ok-c"), false),
            ]
            .into(),
        );
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed, "continue → ok edge");
        let arr = out.result.as_array().unwrap();
        assert_eq!(arr.len(), 3, "positional integrity");
        assert_eq!(arr[0], json!("ok-a"));
        assert_eq!(arr[1]["index"], json!(1), "failed slot carries the marker");
        assert!(arr[1]["error"].as_str().unwrap().contains("boom-b"));
        assert_eq!(arr[2], json!("ok-c"));
    }

    #[test]
    fn foreach_fail_fast_stops_at_the_first_failure() {
        let g = fanout_graph("fail_fast", 1);
        let mut exec = MockExec::default();
        exec.tools.insert(
            "q.scan".into(),
            (
                json!({"items": [{"id": "a"}, {"id": "b"}, {"id": "c"}]}),
                false,
            ),
        );
        exec.tool_seq.insert(
            "q.handle".into(),
            vec![(json!("ok-a"), false), (json!("boom"), true)].into(),
        );
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.terminal, Some(TerminalStatus::Crashed), "error edge");
        let arr = out.result.as_array().unwrap();
        assert_eq!(arr.len(), 2, "stopped after the failure — c never ran");
    }

    #[test]
    fn foreach_parallel_batch_restores_positions() {
        // parallel=3 through the DEFAULT (sequential fallback) batch hook: the
        // pre-charge + index restoration paths are exercised regardless of lanes.
        let g = fanout_graph("continue", 3);
        let mut exec = MockExec::default();
        exec.tools.insert(
            "q.scan".into(),
            (json!({"items": [{"id": "a"}, {"id": "b"}]}), false),
        );
        exec.tool_seq.insert(
            "q.handle".into(),
            vec![(json!("first"), false), (json!("second"), false)].into(),
        );
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed);
        assert_eq!(out.result, json!(["first", "second"]));
    }

    #[test]
    fn foreach_guards_items_shape_and_budget() {
        // Non-array items → error edge with the kind named.
        let g = fanout_graph("fail_fast", 1);
        let mut exec = MockExec::default();
        exec.tools
            .insert("q.scan".into(), (json!({"items": "not-an-array"}), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.terminal, Some(TerminalStatus::Crashed));
        assert!(
            out.result
                .as_str()
                .unwrap()
                .contains("must resolve to an array"),
            "{:?}",
            out.result
        );
        // Step budget exhausts MID-foreach (budget 4: scan + fan + 2 items).
        let g = fanout_graph("fail_fast", 1);
        let mut exec = MockExec::default();
        exec.tools.insert(
            "q.scan".into(),
            (
                json!({"items": [{"id": "a"}, {"id": "b"}, {"id": "c"}, {"id": "d"}]}),
                false,
            ),
        );
        exec.tools.insert("q.handle".into(), (json!("x"), false));
        let DriveResult::Done(out) = drive(&g, &mut exec, 4) else {
            panic!("terminates");
        };
        assert_eq!(out.status, GraphStatus::Exhausted);
        assert!(
            out.reason.as_deref().unwrap().contains("foreach"),
            "{:?}",
            out.reason
        );
    }

    #[cfg(feature = "cel")]
    #[test]
    fn a_computed_assign_shapes_data_with_zero_model_calls() {
        // scan (tool) → pick (assign expr: filter+map) → done. Deterministic
        // aggregation that previously needed an infer node (model tokens).
        let g: Graph = serde_json::from_value(json!({
            "start": "scan",
            "nodes": {
                "scan": {"kind": "tool", "server": "q", "tool": "scan", "writes": "scan", "edges": {"ok": "pick", "error": "fail"}},
                "pick": {"kind": "assign", "expr": "{'ids': scan.items.filter(i, i.ok).map(i, i.id), 'total': scan.items.size()}", "writes": "picked", "edges": {"ok": "done", "error": "fail"}},
                "done": {"kind": "halt", "status": "completed", "result_from": "picked"},
                "fail": {"kind": "halt", "status": "crashed", "result_from": "picked"}
            }
        }))
        .unwrap();
        assert!(g.validate().is_ok());
        let mut exec = MockExec::default();
        exec.tools.insert(
            "q.scan".into(),
            (json!({"items": [{"id": 1, "ok": true}, {"id": 2, "ok": false}, {"id": 3, "ok": true}]}), false),
        );
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed, "{:?}", out.result);
        assert_eq!(out.result, json!({"ids": [1, 3], "total": 3}));
        assert!(
            !exec
                .calls
                .iter()
                .any(|c| c.starts_with("agent:") || c.starts_with("infer:")),
            "no model involvement: {:?}",
            exec.calls
        );
    }

    #[cfg(feature = "cel")]
    #[test]
    fn an_infer_check_reasks_a_type_correct_but_out_of_bounds_answer() {
        let g: Graph = serde_json::from_value(json!({
            "start": "c",
            "nodes": {
                "c": {"kind": "infer", "prompt": "score it", "schema": {"score": "number"}, "check": "score >= 0.0 && score <= 1.0", "retries": 2, "writes": "s", "edges": {"ok": "done", "error": "fail"}},
                "done": {"kind": "halt", "status": "completed", "result_from": "s"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap();
        assert!(g.validate().is_ok());
        let mut exec = MockExec::default();
        exec.infers.push_back(Ok(json!({"score": 7.5}))); // type-correct, out of bounds
        exec.infers.push_back(Ok(json!({"score": 0.75})));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed);
        assert_eq!(out.result, json!({"score": 0.75}));
        assert!(
            exec.infer_feedbacks[1]
                .as_deref()
                .unwrap()
                .contains("failed the check"),
            "{:?}",
            exec.infer_feedbacks[1]
        );
    }

    // ── async subgraph + join — the spawn/join pair ──────────────────────────

    /// Two async subgraphs fanned out, handles gathered, then joined.
    fn spawn_join_graph(timeout_ms: u64) -> Graph {
        serde_json::from_value(json!({
            "start": "s1",
            "nodes": {
                "s1": {"kind": "subgraph", "async": true,
                       "graph": {"start": "h", "nodes": {"h": {"kind": "halt", "status": "completed"}}},
                       "writes": "h1", "edges": {"ok": "s2", "error": "fail"}},
                "s2": {"kind": "subgraph", "async": true,
                       "graph": {"start": "h", "nodes": {"h": {"kind": "halt", "status": "completed"}}},
                       "writes": "h2", "edges": {"ok": "gather", "error": "fail"}},
                "gather": {"kind": "assign", "value": [{"$from": "h1"}, {"$from": "h2"}], "writes": "hs", "edges": {"ok": "join", "error": "fail"}},
                "join": {"kind": "join", "handles": {"$from": "hs"}, "timeout_ms": timeout_ms, "writes": "results",
                         "edges": {"ok": "done", "error": "fail", "timeout": "late"}},
                "done": {"kind": "halt", "status": "completed", "result_from": "results"},
                "late": {"kind": "halt", "status": "deadline", "result_from": "results"},
                "fail": {"kind": "halt", "status": "crashed", "result_from": "results"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn async_subgraphs_spawn_and_join_collects_positionally() {
        let g = spawn_join_graph(5000);
        assert!(g.validate().is_ok());
        let mut exec = MockExec::default();
        exec.join_results
            .insert("m.1".into(), (json!("first done"), false));
        exec.join_results
            .insert("m.2".into(), (json!("second done"), false));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.status, GraphStatus::Completed, "{:?}", out.result);
        assert_eq!(out.result, json!(["first done", "second done"]));
        assert_eq!(exec.spawned_subgraphs.len(), 2, "both spawned");
        assert_eq!(
            exec.calls
                .iter()
                .filter(|c| c.starts_with("await:"))
                .count(),
            2
        );
    }

    #[test]
    fn a_failed_child_marks_its_slot_and_takes_the_error_edge() {
        let g = spawn_join_graph(5000);
        let mut exec = MockExec::default();
        exec.join_results.insert("m.1".into(), (json!("ok"), false));
        exec.join_results
            .insert("m.2".into(), (json!("boom"), true));
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.terminal, Some(TerminalStatus::Crashed), "error edge");
        let arr = out.result.as_array().unwrap();
        assert_eq!(arr[0], json!("ok"));
        assert_eq!(arr[1]["handle"], json!("m.2"));
    }

    #[test]
    fn a_straggler_takes_the_timeout_edge_with_partials() {
        let g = spawn_join_graph(200);
        let mut exec = MockExec::default();
        exec.join_results.insert("m.1".into(), (json!("ok"), false));
        exec.join_pending.insert("m.2".into());
        let out = run(&g, &mut exec, 100);
        assert_eq!(out.terminal, Some(TerminalStatus::Deadline), "timeout edge");
        assert_eq!(out.result, json!(["ok"]), "partials written");
    }

    #[test]
    fn an_executor_without_spawn_machinery_degrades_to_the_error_edge() {
        struct NoSpawn;
        impl GraphExec for NoSpawn {
            fn run_agent(
                &mut self,
                _: &str,
                _: Option<&str>,
                _: &Blackboard,
                _: &[String],
            ) -> (Value, bool) {
                (Value::Null, false)
            }
            fn call_tool(&mut self, _: &str, _: &str, _: &Value) -> (Value, bool) {
                (Value::Null, false)
            }
        }
        let g: Graph = serde_json::from_value(json!({
            "start": "s",
            "nodes": {
                "s": {"kind": "subgraph", "async": true,
                      "graph": {"start": "h", "nodes": {"h": {"kind": "halt", "status": "completed"}}},
                      "writes": "h1", "edges": {"ok": "done", "error": "fail"}},
                "done": {"kind": "halt", "status": "completed"},
                "fail": {"kind": "halt", "status": "crashed", "result_from": "h1"}
            }
        }))
        .unwrap();
        let mut exec = NoSpawn;
        let DriveResult::Done(out) = drive(&g, &mut exec, 10) else {
            panic!()
        };
        assert_eq!(out.terminal, Some(TerminalStatus::Crashed));
        assert!(
            out.result.as_str().unwrap().contains("cannot spawn"),
            "{:?}",
            out.result
        );
    }

    #[test]
    fn a_bad_handles_shape_takes_the_error_edge() {
        let g: Graph = serde_json::from_value(json!({
            "start": "j",
            "nodes": {
                "j": {"kind": "join", "handles": 42, "timeout_ms": 1000, "writes": "r",
                      "edges": {"ok": "done", "error": "fail", "timeout": "done"}},
                "done": {"kind": "halt", "status": "completed"},
                "fail": {"kind": "halt", "status": "crashed", "result_from": "r"}
            }
        }))
        .unwrap();
        let mut exec = MockExec::default();
        let out = run(&g, &mut exec, 10);
        assert_eq!(out.terminal, Some(TerminalStatus::Crashed));
        assert!(
            out.result.as_str().unwrap().contains("handles must be"),
            "{:?}",
            out.result
        );
    }

    #[test]
    fn retries_stop_when_the_budget_runs_out() {
        // Budget 2: the visit charges 1, the first retry charges 1, the second
        // retry is DENIED → Exhausted (not an infinite retry storm).
        let g: Graph = serde_json::from_value(json!({
            "start": "t",
            "nodes": {
                "t": {"kind": "tool", "server": "s", "tool": "dead", "retry": {"max": 5, "backoff_ms": 0}, "edges": {"ok": "done", "error": "fail"}},
                "done": {"kind": "halt", "status": "completed"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        }))
        .unwrap();
        let mut exec = MockExec::default();
        exec.tools.insert("s.dead".into(), (json!("x"), true));
        let out = run(&g, &mut exec, 2);
        assert_eq!(out.status, GraphStatus::Exhausted);
        assert_eq!(exec.tool_args.len(), 2, "visit + one retry, then stopped");
    }
}
