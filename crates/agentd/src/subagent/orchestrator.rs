// SPDX-License-Identifier: Apache-2.0
//! The self-orchestration handler: the `subagent.spawn` self-tool. RFC 0005
//! §self-tools, RFC 0009 §nesting, RFC 0001 §self-orchestration.
//!
//! This is where the model splits its instruction into delegated child agents.
//! When the loop sees a `subagent.spawn` call it routes here; the orchestrator
//! mints a child [`SpawnPayload`] (depth + 1, a narrowed scope, an inherited
//! intelligence config), enforces the caps (depth / breadth) — **refusing as a
//! tool result, never crashing** — and supervises the child synchronously by
//! reusing [`supervise_once`]. The child is a real OS process parented to this
//! one, so `PDEATHSIG` + the subreaper keep the nested tree in the reaping
//! domain (RFC 0003).
//!
//! By default delegation is **synchronous**: the parent's agentic loop blocks
//! on the nested supervisor while the child runs (its control thread keeps
//! answering pings, so the grandparent sees it as busy, not stuck). `async=true`
//! instead returns a handle immediately and the parent collects the result later
//! via `subagent.await` / `subagent.status` / `agentd://subagent/<handle>` (see
//! `spawn_async`); `detach=true` is fire-and-forget.
//!
//! Known limitation: the tree-wide token ceiling is enforced per *local*
//! supervisor, not globally across the nested tree (each subagent bounds its
//! own children); per-child token/step/deadline limits still apply.

use crate::agentloop::action::SelfHandler;
use crate::agentloop::stop::{ScheduleRequest, SubscriptionAction, SubscriptionRequest};
use crate::config::McpServerSpec;
use crate::obs::log::Logger;
use crate::subagent::protocol::{
    AgentMsg, IntelConfig, Limits, SeedMessage, SpawnPayload, Telemetry,
};
use crate::supervisor::reactor::{SuperviseResult, supervise_once};
use crate::supervisor::spawn::{Subagent, spawn};
use crate::supervisor::tree::{NodeId, TokenBucket};
use crate::wire::intel::ToolDef;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Conservative per-node breadth cap (depth comes from the payload's
/// `max_depth`). RFC 0009.
const MAX_CHILDREN: u32 = 8;
/// Spawn-rate token bucket (RFC 0009 §3.6): 8 burst, 2 tokens/s refill. Catches a
/// fast churn loop that stays under the absolute breadth cap — a wedged agent
/// hammering `subagent.spawn` just keeps getting refusals, never a fork bomb.
const SPAWN_RATE_BURST: u32 = 8;
const SPAWN_RATE_PER_SEC: f64 = 2.0;
/// Cap on the distilled result handed back to the parent model (~chars).
const DISTILL_CAP: usize = 8_000;
/// Cap on self-scheduled wake-ups per run (bounds a runaway self-scheduler).
const MAX_SCHEDULED: usize = 8;
/// Bounds on a self-scheduled delay (≥ 1s; ≤ 30 days so `now + delay` is safe).
const MIN_SCHEDULE_SECS: u64 = 1;
const MAX_SCHEDULE_SECS: u64 = 30 * 24 * 3600;
/// Cap on resource (un)subscriptions an agent may request per run.
const MAX_SUBSCRIPTIONS: usize = 16;
/// Poll granularity while awaiting an async child.
const AWAIT_POLL: Duration = Duration::from_millis(50);
/// Bounded per-delegation deadline for a remote A2A delegation (RFC 0020 §3):
/// the `a2a.delegate` SendMessage→poll loop is capped here so it never hangs the
/// agentic loop. The remote agent owns its own budget; this is agentd's backstop.
#[cfg(feature = "a2a")]
const A2A_DELEGATE_DEADLINE: Duration = Duration::from_secs(120);
/// Bounded `subagent.await` window: a single call blocks at most this long, then
/// hands control back ("still running") so the loop stays responsive to cancel.
const AWAIT_MAX: Duration = Duration::from_secs(30);

/// A backgrounded child from `subagent.spawn{async|detach}`: its handle's
/// process + control channel, and its distilled outcome once it reports a
/// terminal frame. The parent supervises it lazily — draining on status/await —
/// and `Subagent`'s Drop kills + reaps it when the orchestrator is dropped, so
/// no async child outlives the parent's tree (RFC 0009 §async).
struct AsyncChild {
    /// Held only for its `Drop` (kills + reaps the child) — never read directly,
    /// but dropping it early would tear the child down, so it must outlive the
    /// entry. `#[allow(dead_code)]` makes that RAII intent explicit.
    #[allow(dead_code)]
    sub: Subagent,
    rx: Receiver<(NodeId, AgentMsg)>,
    /// `Some((distilled, is_error))` once a terminal `Result`/`Failed` (or the
    /// channel disconnecting) was seen.
    outcome: Option<(String, bool)>,
    detached: bool,
}

pub struct Orchestrator {
    exe: PathBuf,
    parent_depth: u32,
    parent_path: String,
    max_depth: u32,
    child_count: u32,
    /// Tree-wide spawn-rate limiter (RFC 0009 §3.6), checked in `spawn`.
    spawn_bucket: TokenBucket,
    intelligence: IntelConfig,
    mcp_servers: Vec<McpServerSpec>,
    /// Declared remote-A2A delegation peers (RFC 0020 §3), propagated from the
    /// payload so children inherit them and the `a2a.delegate` self-tool can dial
    /// them by name. Carried on every build (it is inert config) but only
    /// surfaced as a tool under `--features a2a`.
    a2a_peers: Vec<crate::config::A2aPeerSpec>,
    run_id: String,
    trace_id: Option<String>,
    log_level: String,
    child_limits: Limits,
    drain_timeout: Duration,
    /// Future wake-ups the root agent requested via `schedule` this run; drained
    /// into the run's `Outcome` for a daemon supervisor to arm (RFC 0008).
    scheduled: Vec<ScheduleRequest>,
    /// Resource (un)subscriptions the root agent requested this run (RFC 0008).
    subscriptions: Vec<SubscriptionRequest>,
    /// Backgrounded children from `subagent.spawn{async|detach}`, keyed by handle
    /// (= the child's agent_path). Drained on status/await; reaped on Drop.
    async_children: HashMap<String, AsyncChild>,
    /// Author-defined workflows (pivot Phase 7), keyed by a generated id. Populated
    /// by `workflow.define` (validate-then-store); run by `workflow.run` (a later phase).
    #[cfg(feature = "workflow")]
    workflows: std::collections::BTreeMap<String, crate::graph::Graph>,
    /// Monotone id counter for `workflow.define`.
    #[cfg(feature = "workflow")]
    workflow_seq: u64,
    log: Logger,
}

impl Orchestrator {
    /// Build from the running subagent's own payload. Children inherit its
    /// intelligence + (narrowable) MCP scope and carry depth + 1.
    pub fn from_payload(
        exe: PathBuf,
        payload: &SpawnPayload,
        drain_timeout: Duration,
        log: Logger,
    ) -> Orchestrator {
        Orchestrator {
            exe,
            parent_depth: payload.depth,
            parent_path: payload.telemetry.agent_path.clone(),
            max_depth: payload.limits.max_depth,
            child_count: 0,
            spawn_bucket: TokenBucket::new(SPAWN_RATE_BURST, SPAWN_RATE_PER_SEC),
            intelligence: payload.intelligence.clone(),
            mcp_servers: payload.mcp_servers.clone(),
            a2a_peers: payload.a2a_peers.clone(),
            run_id: payload.telemetry.run_id.clone(),
            trace_id: payload.telemetry.trace_id.clone(),
            log_level: payload.telemetry.log_level.clone(),
            // Children inherit the parent's per-run bounds (v1).
            child_limits: payload.limits.clone(),
            drain_timeout,
            scheduled: Vec::new(),
            subscriptions: Vec::new(),
            async_children: HashMap::new(),
            #[cfg(feature = "workflow")]
            workflows: std::collections::BTreeMap::new(),
            #[cfg(feature = "workflow")]
            workflow_seq: 0,
            log,
        }
    }

    /// Request a resource (un)subscription for the daemon to apply after this
    /// run (RFC 0008). Bounded; refused-as-tool-result, never crashes.
    fn subscription(&mut self, action: SubscriptionAction, args: &Value) -> (String, bool) {
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            return refused("maximum self-subscription changes reached for this run");
        }
        let uri = args.get("uri").and_then(Value::as_str).unwrap_or("").trim();
        if uri.is_empty() {
            return (
                "error: subscribe/unsubscribe requires a non-empty 'uri'".into(),
                true,
            );
        }
        let verb = match action {
            SubscriptionAction::Subscribe => "subscribe",
            SubscriptionAction::Unsubscribe => "unsubscribe",
        };
        self.subscriptions.push(SubscriptionRequest {
            uri: uri.to_string(),
            action,
            condition: None,
        });
        self.log
            .info("self.subscribe", json!({"action": verb, "uri": uri}));
        (
            format!("requested: the daemon will {verb} {uri} after this run"),
            false,
        )
    }

    /// `subagent.await_resource` (pivot Phase 5.2) — the in-turn WAIT primitive.
    /// Park on a resource until its content satisfies a predicate, then re-enter.
    /// It registers a CONDITIONAL self-subscription (a warm continue-session keyed
    /// by the URI, like `subscribe`, but the daemon fires it only when the
    /// condition matches); the model then ends this turn, and the daemon re-enters
    /// the warm session with the changed resource when the wait is satisfied. The
    /// condition is validated HERE so a malformed predicate is a clear tool-result
    /// refusal (never a crash, never a route that can't be armed). Bounded by the
    /// same self-subscription budget. Effective only under a daemon (a one-shot run
    /// has no reactor to re-enter — the request rides out on the `Outcome` and is
    /// dropped with a logged warning, like any deferred effect).
    fn await_resource(&mut self, args: &Value) -> (String, bool) {
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            return refused("maximum self-subscription changes reached for this run");
        }
        let uri = args.get("uri").and_then(Value::as_str).unwrap_or("").trim();
        if uri.is_empty() {
            return ("error: await_resource requires a non-empty 'uri'".into(), true);
        }
        let Some(cond) = args.get("condition") else {
            return (
                "error: await_resource requires a 'condition' object (e.g. {\"pointer\":\"/status\",\"op\":\"eq\",\"value\":\"ready\"})".into(),
                true,
            );
        };
        // Validate the predicate now so a bad one is refused as a tool result.
        if let Err(e) = crate::triggers::router::Condition::from_json(cond) {
            return (format!("error: invalid await_resource condition: {e}"), true);
        }
        self.subscriptions.push(SubscriptionRequest {
            uri: uri.to_string(),
            action: SubscriptionAction::Subscribe,
            condition: Some(cond.clone()),
        });
        self.log
            .info("self.await_resource", json!({"uri": uri, "condition": cond}));
        (
            format!(
                "parked: the daemon will re-enter this session when {uri} satisfies the condition"
            ),
            false,
        )
    }

    /// `workflow.define` (pivot Phase 7, `--features workflow`) — validate an authored
    /// workflow and STORE it under a generated id for a later `workflow.run`. Validation
    /// is structural + fail-closed: a graph that can't reach a `Halt`, dangles an
    /// edge, or busts the caps is REFUSED as a tool result (never stored, never a
    /// crash). Bounded by a per-run graph budget (fork-bomb hygiene).
    #[cfg(feature = "workflow")]
    fn workflow_define(&mut self, args: &Value) -> (String, bool) {
        const MAX_WORKFLOWS: usize = 16;
        if self.workflows.len() >= MAX_WORKFLOWS {
            return refused("maximum workflows defined for this run");
        }
        let Some(graph_val) = args.get("workflow") else {
            return ("error: workflow.define requires a 'workflow' object".into(), true);
        };
        let graph: crate::graph::Graph = match serde_json::from_value(graph_val.clone()) {
            Ok(g) => g,
            Err(e) => return (format!("error: malformed workflow: {e}"), true),
        };
        if let Err(errs) = graph.validate() {
            return (format!("error: invalid workflow: {}", errs.join("; ")), true);
        }
        self.workflow_seq += 1;
        let id = format!("w{}", self.workflow_seq);
        let n = graph.nodes.len();
        self.workflows.insert(id.clone(), graph);
        self.log
            .info("workflow.define", json!({"workflow_id": id, "nodes": n}));
        (
            format!("workflow defined: {id} ({n} nodes) — run it with workflow.run"),
            false,
        )
    }

    /// `workflow.patch` (pivot Phase 7 · P5, `--features workflow`) — grow a stored graph
    /// ADDITIVELY (new nodes/edges only; no overwrite, no retarget), so the model can
    /// extend its own plan at runtime without breaking a live run's reachability or
    /// termination. The patch is applied to a clone + re-validated; a rejected patch
    /// leaves the stored graph UNCHANGED and is refused as a tool result.
    #[cfg(feature = "workflow")]
    fn workflow_patch(&mut self, args: &Value) -> (String, bool) {
        let id = args.get("workflow_id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return ("error: workflow.patch requires 'workflow_id'".into(), true);
        }
        if !self.workflows.contains_key(id) {
            return (format!("error: no such workflow '{id}'"), true);
        }
        let patch: crate::graph::GraphPatch =
            match serde_json::from_value(args.get("patch").cloned().unwrap_or_else(|| json!({}))) {
                Ok(p) => p,
                Err(e) => return (format!("error: malformed patch: {e}"), true),
            };
        let mut patched = self.workflows[id].clone();
        if let Err(errs) = patched.apply_patch(patch) {
            return (format!("error: patch rejected: {}", errs.join("; ")), true);
        }
        let n = patched.nodes.len();
        self.workflows.insert(id.to_string(), patched);
        self.log.info("workflow.patch", json!({"workflow_id": id, "nodes": n}));
        (format!("workflow {id} patched ({n} nodes)"), false)
    }

    /// `workflow.run` (pivot Phase 7 · P6, `--features workflow`) — the "agent
    /// orchestrates BY ITSELF" primitive: drive a graph the agent defined to a
    /// terminal status, returning its status + projected result as the tool result.
    /// SYNCHRONOUS — it runs in the agent's own (child) process (per the design:
    /// the driver is NOT in the daemon), bounded by the graph's budget + termination
    /// layers. Reuses [`drive_pinned`](crate::graph::drive_pinned), so it behaves
    /// identically to the operator `--mode graph`: a `Wait` node blocks in-process
    /// until its resource updates or the timeout elapses, then resumes. The whole call
    /// blocks until the graph terminates (uncancellable mid-run — a documented v1 limit).
    #[cfg(feature = "workflow")]
    fn workflow_run(&mut self, args: &Value) -> (String, bool) {
        use crate::graph::GraphStatus;
        let id = args.get("workflow_id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return ("error: workflow.run requires 'workflow_id'".into(), true);
        }
        let Some(graph) = self.workflows.get(id).cloned() else {
            return (
                format!("error: no such workflow '{id}' — define it with workflow.define first"),
                true,
            );
        };
        // detach: hand the workflow to a SPAWNED subagent (pivot Phase 7 · W4) —
        // the child process drives it under full supervision while this agent
        // keeps working; collect via subagent.status / subagent.await with the
        // returned handle. Reuses the spawn path, so depth/breadth/rate caps and
        // scope narrowing apply exactly as for any delegation.
        if args.get("detach").and_then(Value::as_bool).unwrap_or(false) {
            let wf = match serde_json::to_value(&graph) {
                Ok(v) => v,
                Err(e) => return (format!("error: workflow serialize: {e}"), true),
            };
            return self.spawn(&json!({
                "instruction": format!("drive workflow {id}"),
                "detach": true,
                "workflow": wf,
            }));
        }
        let model = self.intelligence.model.clone().unwrap_or_default();
        let node_timeout = Duration::from_millis(self.child_limits.deadline_ms);
        // The whole workflow shares the child's wall budget: each node may use up
        // to the child deadline, and the WALK stops at that same wall (checked per
        // node entry) — plus the shared token pool below.
        let deadline = Some(std::time::Instant::now() + node_timeout);
        let result = crate::graph::drive_pinned(
            &graph,
            &self.intelligence.uri,
            self.intelligence.token.clone(),
            &model,
            &self.mcp_servers,
            self.child_limits.max_steps,
            self.child_limits.max_tokens,
            node_timeout,
            deadline,
            &self.log,
        );
        match result {
            Ok(o) => {
                let is_err = o.status != GraphStatus::Completed;
                self.log.info(
                    "workflow.run",
                    json!({"workflow_id": id, "status": format!("{:?}", o.status), "steps": o.steps}),
                );
                let summary = json!({
                    "workflow_id": id,
                    "status": format!("{:?}", o.status),
                    "reason": o.reason,
                    "steps": o.steps,
                    "tokens": o.tokens,
                    "result": o.result,
                });
                (summary.to_string(), is_err)
            }
            Err(e) => (format!("error: workflow.run setup failed: {e}"), true),
        }
    }

    /// Queue a future self-wake-up (RFC 0008 §self-scheduling). Bounded; refused
    /// as a tool result (never crashes). Effective only under a daemon — the
    /// requests ride out on the run's `Outcome`.
    fn schedule(&mut self, args: &Value) -> (String, bool) {
        if self.scheduled.len() >= MAX_SCHEDULED {
            return refused("maximum self-scheduled wake-ups reached for this run");
        }
        let after = match args.get("after_seconds").and_then(Value::as_u64) {
            Some(s) if (MIN_SCHEDULE_SECS..=MAX_SCHEDULE_SECS).contains(&s) => s,
            _ => {
                return (
                    format!(
                        "error: schedule needs 'after_seconds' in {MIN_SCHEDULE_SECS}..={MAX_SCHEDULE_SECS}"
                    ),
                    true,
                );
            }
        };
        let instruction = args
            .get("instruction")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if instruction.is_empty() {
            return (
                "error: schedule requires a non-empty 'instruction'".into(),
                true,
            );
        }
        self.scheduled.push(ScheduleRequest {
            after_ms: after.saturating_mul(1000),
            instruction: instruction.to_string(),
        });
        self.log.info(
            "self.schedule",
            json!({"after_s": after, "queued": self.scheduled.len()}),
        );
        (
            format!("scheduled: a wake-up in {after}s will run the given instruction"),
            false,
        )
    }

    fn can_nest(&self) -> bool {
        // A child would be at `parent_depth + 1`, which must be ≤ max_depth.
        self.parent_depth < self.max_depth
    }

    /// Publish the subagent-tree shape + saturation gauges (RFC 0016 §4.3 /
    /// RFC 0019 §5.1), called at each spawn and reap so `agentd_active_subagents`
    /// reflects the live children — it is the load-bearing saturation numerator.
    /// `active` = currently-held live children (backgrounded async/detached ones;
    /// a synchronous spawn blocks inside `supervise_once`, so it is not "held");
    /// `breadth` = the lifetime child-count cap counter; `depth` = this node's own
    /// depth (best-effort). `agentd_saturation` = live / capacity where capacity is
    /// the tree total cap (`max_total`, RFC 0009 — the per-route product is not
    /// cheaply reachable from this local orchestrator, so the tree cap alone is
    /// used, per RFC 0019 §5.1's `min(…, max_total_subagents)`). No-op without the
    /// `metrics` feature.
    fn publish_tree_shape(&self) {
        // Live = held async/detached children that have not yet recorded a terminal
        // outcome. A detached child has no observable outcome, so it counts live
        // while held; a completed-but-uncollected child does not.
        let active = self
            .async_children
            .values()
            .filter(|c| c.detached || c.outcome.is_none())
            .count() as u64;
        crate::obs::metrics::set_tree_shape(
            active,
            u64::from(self.parent_depth),
            u64::from(self.child_count),
        );
        let capacity = u64::from(crate::supervisor::tree::Caps::default().max_total);
        crate::obs::metrics::set_saturation(active, capacity);
    }

    fn spawn(&mut self, args: &Value) -> (String, bool) {
        // Caps — refused as a tool result so the model adapts (RFC 0009).
        if !self.can_nest() {
            return refused("maximum subagent depth reached; do this step yourself");
        }
        // Spawn-rate cap (RFC 0009 §3.6): a fast churn loop is throttled by the
        // token bucket *before* the absolute breadth count — so a runaway loop
        // that spawns in a tight burst is refused on rate (the more actionable
        // signal: "back off"), while the breadth cap below remains the hard
        // tree-shape ceiling for a slow drip of spawns. Refused as a tool result,
        // never a fork bomb.
        if !self.spawn_bucket.try_take() {
            return refused("spawn rate exceeded");
        }
        if self.child_count >= MAX_CHILDREN {
            return refused("maximum number of child subagents reached for this agent");
        }
        // Memory backpressure: refuse nesting when the unit is at its memory.high
        // soft limit (best-effort; never fires off-cgroup). The model adapts.
        if crate::supervisor::cgroup::under_memory_pressure() {
            return refused(
                "memory pressure (cgroup at memory.high); do this step yourself or retry",
            );
        }
        // An attached WORKFLOW (pivot Phase 7 · W4): the child drives this graph
        // instead of running the ReAct loop on `instruction`. Validated here at
        // the authoring boundary (fail-closed as a tool result, like
        // workflow.define) and again by the child across the process boundary.
        #[cfg(feature = "workflow")]
        let workflow: Option<crate::graph::Graph> = match args.get("workflow") {
            None => None,
            Some(v) => match serde_json::from_value::<crate::graph::Graph>(v.clone()) {
                Ok(g) => {
                    if let Err(errs) = g.validate() {
                        return (format!("error: invalid workflow: {}", errs.join("; ")), true);
                    }
                    Some(g)
                }
                Err(e) => return (format!("error: malformed workflow: {e}"), true),
            },
        };
        #[cfg(not(feature = "workflow"))]
        let has_workflow = false;
        #[cfg(feature = "workflow")]
        let has_workflow = workflow.is_some();

        let instruction = args
            .get("instruction")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if instruction.is_empty() && !has_workflow {
            return (
                "error: subagent.spawn requires a non-empty 'instruction' (or a 'workflow')".into(),
                true,
            );
        }
        let instruction = if instruction.is_empty() {
            "drive the attached workflow"
        } else {
            instruction
        };

        let output_contract = str_arg(args, "output_contract");
        let context_seed = str_arg(args, "context")
            .map(|c| {
                vec![SeedMessage {
                    role: "user".into(),
                    content: c,
                }]
            })
            .unwrap_or_default();
        let mcp_servers = self.narrow_servers(args);

        let idx = self.child_count;
        let child_path = format!("{}.{}", self.parent_path, idx);
        let payload = SpawnPayload {
            instruction: instruction.to_string(),
            output_contract,
            context_seed,
            intelligence: self.intelligence.clone(),
            mcp_servers,
            a2a_peers: self.a2a_peers.clone(),
            limits: self.child_limits.clone(),
            telemetry: Telemetry {
                run_id: self.run_id.clone(),
                agent_id: child_path.clone(),
                agent_path: child_path.clone(),
                trace_id: self.trace_id.clone(),
                log_level: self.log_level.clone(),
                // Content-capture policy flows down the tree (RFC 0010 §2.9).
                log_content: self.log.content_capture(),
            },
            depth: self.parent_depth + 1,
            warm: false, // a delegated subagent is a one-shot distilled subtask
            #[cfg(feature = "workflow")]
            workflow,
        };
        let is_async = args.get("async").and_then(Value::as_bool).unwrap_or(false);
        let detach = args.get("detach").and_then(Value::as_bool).unwrap_or(false);
        self.child_count += 1;
        self.log.info(
            "subagent.delegate",
            json!({"child": child_path, "depth": payload.depth, "servers": payload.mcp_servers.len(), "async": is_async || detach}),
        );

        // Async / detach: spawn in the background and return a handle now (RFC
        // 0009 §async). Default is synchronous: block on the nested supervisor.
        if is_async || detach {
            return self.spawn_async(payload, child_path, detach);
        }

        // A synchronous spawn is live for the duration of this blocking call; the
        // breadth counter advanced above, so publish the shape now (it reflects the
        // bumped `child_count`). The result returns once the child has completed.
        self.publish_tree_shape();
        match supervise_once(
            self.exe.clone(),
            &payload,
            self.drain_timeout,
            self.log.clone(),
        ) {
            Ok(SuperviseResult::Completed(outcome)) => (distill(&outcome.result), false),
            Ok(SuperviseResult::Failed(e)) => (format!("subagent failed: {e}"), true),
            Ok(SuperviseResult::Killed(r)) => (format!("subagent terminated ({r:?})"), true),
            Err(e) => (format!("subagent could not start: {e}"), true),
        }
    }

    /// `a2a.delegate` — delegate an objective to a declared **remote A2A agent**
    /// (RFC 0020 §3), the remote backend beside the local `subagent.spawn`. Looks
    /// the named peer up in `a2a_peers`, then runs the A2A client (SendMessage →
    /// poll GetTask → distillate) bounded by [`A2A_DELEGATE_DEADLINE`]. A remote
    /// delegation counts against the SAME breadth cap a local spawn does (it *is*
    /// a delegation), and the spawn-rate bucket throttles churn. Every failure —
    /// an unknown peer, a transport error, a non-completed remote terminal — is an
    /// `isError` tool result (an observation the model adapts to), never a crash.
    #[cfg(feature = "a2a")]
    fn delegate(&mut self, args: &Value) -> (String, bool) {
        // Breadth + rate caps: a remote delegation is a delegation, counted like a
        // local spawn so the node's fan-out ceiling bounds both backends together.
        if !self.spawn_bucket.try_take() {
            return a2a_refused("delegation rate exceeded");
        }
        if self.child_count >= MAX_CHILDREN {
            return a2a_refused("maximum number of delegations reached for this agent");
        }
        let peer_name = args
            .get("peer")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if peer_name.is_empty() {
            return (
                "error: a2a.delegate requires a non-empty 'peer' name".into(),
                true,
            );
        }
        let objective = args
            .get("objective")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if objective.is_empty() {
            return (
                "error: a2a.delegate requires a non-empty 'objective'".into(),
                true,
            );
        }
        let output_contract = str_arg(args, "output_contract");

        // Resolve the peer endpoint. An unknown peer name is an observation (the
        // model may have hallucinated a peer), not a crash.
        let Some(peer) = self.a2a_peers.iter().find(|p| p.name == peer_name) else {
            let known: Vec<&str> = self.a2a_peers.iter().map(|p| p.name.as_str()).collect();
            return (
                format!(
                    "error: no A2A peer named '{peer_name}' (declared peers: {})",
                    known.join(", ")
                ),
                true,
            );
        };
        // The endpoint scheme was validated at startup; a parse error here is
        // surfaced as an observation rather than trusted to be infallible.
        let endpoint = match peer.endpoint_of() {
            Ok(e) => e,
            Err(msg) => return (format!("error: a2a peer '{peer_name}': {msg}"), true),
        };

        // Count this delegation against the breadth cap (like a spawned child).
        self.child_count += 1;
        let deadline = Instant::now() + A2A_DELEGATE_DEADLINE;
        self.log.info(
            "a2a.delegate",
            json!({"peer": peer_name, "endpoint": peer.endpoint}),
        );
        match crate::mcp::a2a_client::delegate(
            &endpoint,
            objective,
            output_contract.as_deref(),
            deadline,
        ) {
            crate::mcp::a2a_client::DelegateOutcome::Distillate(s) => {
                (distill(&Value::String(s)), false)
            }
            crate::mcp::a2a_client::DelegateOutcome::Error(e) => (e, true),
        }
    }

    /// Spawn a backgrounded child and return its handle immediately — the parent
    /// keeps working while it runs. The result is collected later via
    /// `subagent.await` / `subagent.status`; if never collected it is reaped when
    /// the orchestrator drops (no async child outlives the parent's tree).
    fn spawn_async(
        &mut self,
        payload: SpawnPayload,
        handle: String,
        detach: bool,
    ) -> (String, bool) {
        let (tx, rx) = mpsc::channel();
        let node = NodeId(u64::from(self.child_count));
        match spawn(&self.exe, &payload, node, tx) {
            Ok(sub) => {
                self.async_children.insert(
                    handle.clone(),
                    AsyncChild {
                        sub,
                        rx,
                        outcome: None,
                        detached: detach,
                    },
                );
                self.log.info(
                    "subagent.spawn_async",
                    json!({"handle": handle, "detach": detach}),
                );
                // A held live child just appeared — refresh the shape/saturation
                // gauges (this one is "active" until reaped).
                self.publish_tree_shape();
                if detach {
                    (
                        format!(
                            "spawned detached subagent (handle={handle}); fire-and-forget — it runs independently and is reaped on completion"
                        ),
                        false,
                    )
                } else {
                    let uri = crate::agentd_uri::subagent_uri(&handle);
                    (
                        format!(
                            "spawned async subagent (handle={handle}); keep working, then get its result with subagent.await (waits for it) — or peek anytime with subagent.status / resource.read {uri} (all idempotent)"
                        ),
                        false,
                    )
                }
            }
            Err(e) => {
                self.async_children.remove(&handle);
                (format!("subagent could not start: {e}"), true)
            }
        }
    }

    /// Pull any pending frames for one async child, recording its terminal
    /// outcome (idempotent once terminal).
    fn drain_child(child: &mut AsyncChild) {
        if child.outcome.is_some() {
            return;
        }
        loop {
            match child.rx.try_recv() {
                Ok((_, AgentMsg::Result { outcome })) => {
                    child.outcome = Some((distill(&outcome.result), false));
                    return;
                }
                Ok((_, AgentMsg::Failed { error })) => {
                    child.outcome = Some((format!("subagent failed: {error}"), true));
                    return;
                }
                Ok(_) => {} // Ready / Turn / Pong / Event / Usage — progress
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    child.outcome = Some(("subagent exited without a result".into(), true));
                    return;
                }
            }
        }
    }

    /// Idempotent peek at an async child: drain its channel and report the
    /// current state — "still running", the distilled result, or (for a detached
    /// child) a not-collectable notice. `None` if no such handle. Does **not**
    /// consume the handle, so status / await / `agentd://` reads of one child are
    /// all consistent and repeatable; the child is reaped at [`Drop`] (or via the
    /// breadth cap). Shared by `status`, `await_child`, and `read_resource`.
    fn peek_child(&mut self, handle: &str) -> Option<(String, bool)> {
        let child = self.async_children.get_mut(handle)?;
        if child.detached {
            return Some((
                format!(
                    "subagent {handle} was spawned detached (fire-and-forget); its result is not collectable"
                ),
                false,
            ));
        }
        Self::drain_child(child);
        let result = match &child.outcome {
            None => (format!("subagent {handle} is still running"), false),
            Some((result, is_err)) => (result.clone(), *is_err),
        };
        // A child that just transitioned to terminal here is no longer live —
        // refresh the active-subagents / saturation gauges to reflect the reap.
        self.publish_tree_shape();
        Some(result)
    }

    /// `subagent.status` — non-blocking, idempotent peek (see [`Self::peek_child`]).
    fn status(&mut self, args: &Value) -> (String, bool) {
        let handle = args
            .get("handle")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        self.peek_child(&handle).unwrap_or_else(|| {
            (
                format!("error: no async subagent with handle '{handle}'"),
                true,
            )
        })
    }

    /// `subagent.await` — block (bounded by [`AWAIT_MAX`]) until the child
    /// finishes, then hand back its distilled result (idempotent — the handle is
    /// not consumed). On timeout returns "still running" so the loop regains
    /// control (await again).
    fn await_child(&mut self, args: &Value) -> (String, bool) {
        let handle = args
            .get("handle")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        match self.async_children.get(&handle) {
            None => {
                return (
                    format!("error: no async subagent with handle '{handle}'"),
                    true,
                );
            }
            // A detached child is not awaitable (fire-and-forget).
            Some(c) if c.detached => return self.peek_child(&handle).unwrap(),
            Some(_) => {}
        }
        // (the immutable borrow above ends here; the loop below re-borrows mutably)
        let deadline = Instant::now() + AWAIT_MAX;
        loop {
            if let Some(child) = self.async_children.get_mut(&handle) {
                Self::drain_child(child);
                if let Some((result, is_err)) = &child.outcome {
                    let resolved = (result.clone(), *is_err);
                    // Reaped: this child is now terminal — refresh the gauges.
                    self.publish_tree_shape();
                    return resolved;
                }
            }
            if Instant::now() >= deadline {
                return (
                    format!(
                        "subagent {handle} is still running (awaited {}s); await again or check status",
                        AWAIT_MAX.as_secs()
                    ),
                    false,
                );
            }
            std::thread::sleep(AWAIT_POLL);
        }
    }

    /// Narrow the child's MCP servers to the requested subset (if any). Names
    /// the parent doesn't have are dropped — scope only ever shrinks.
    fn narrow_servers(&self, args: &Value) -> Vec<McpServerSpec> {
        match args.get("servers").and_then(Value::as_array) {
            Some(names) => {
                let wanted: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
                self.mcp_servers
                    .iter()
                    .filter(|s| wanted.contains(&s.name.as_str()))
                    .cloned()
                    .collect()
            }
            None => self.mcp_servers.clone(),
        }
    }
}

impl SelfHandler for Orchestrator {
    fn tools(&self) -> Vec<ToolDef> {
        let mut t = Vec::new();
        // Advertise delegation only when there is depth budget to use it. The
        // async-collection tools ride alongside (status/await on returned handles).
        if self.can_nest() {
            t.push(spawn_tool_def());
            t.push(status_tool_def());
            t.push(await_tool_def());
        }
        // Remote-A2A delegation (RFC 0020 §3): advertised ONLY when peers are
        // declared (and the `a2a` feature is on). It is a delegation backend, so —
        // like local spawn — it shares the breadth/rate caps; advertising it
        // independently of `can_nest` is deliberate: delegating to a remote agent
        // does not deepen the LOCAL supervised tree (the remote owns its own
        // depth), it only counts against this node's fan-out breadth.
        #[cfg(feature = "a2a")]
        if !self.a2a_peers.is_empty() {
            t.push(a2a_delegate_tool_def(&self.a2a_peers));
        }
        // Self-scheduling + self-subscription are root-agent capabilities: a
        // nested child's request would be lost to its parent, which only
        // distills the child's result.
        if self.parent_depth == 0 {
            t.push(schedule_tool_def());
            t.push(subscribe_tool_def());
            t.push(unsubscribe_tool_def());
            t.push(await_resource_tool_def());
            // Run-graph authoring (pivot Phase 7) is a root orchestration capability,
            // like the reactive self-tools — a nested child distils a result, it does
            // not drive a graph.
            #[cfg(feature = "workflow")]
            {
                t.push(workflow_define_tool_def());
                t.push(workflow_patch_tool_def());
                t.push(workflow_run_tool_def());
            }
        }
        t
    }

    fn handle(&mut self, name: &str, args: &Value) -> Option<(String, bool)> {
        match name {
            "subagent.spawn" => Some(self.spawn(args)),
            // Remote-A2A delegation (RFC 0020 §3) — routed only when peers are
            // declared (else it falls through to MCP / unknown-tool, same as a
            // non-advertised tool). Feature-gated.
            #[cfg(feature = "a2a")]
            "a2a.delegate" if !self.a2a_peers.is_empty() => Some(self.delegate(args)),
            // Collection tools accept a handle regardless of depth budget — an
            // agent at its breadth cap still needs to await its in-flight children.
            "subagent.status" => Some(self.status(args)),
            "subagent.await" => Some(self.await_child(args)),
            "schedule" if self.parent_depth == 0 => Some(self.schedule(args)),
            "subscribe" if self.parent_depth == 0 => {
                Some(self.subscription(SubscriptionAction::Subscribe, args))
            }
            "unsubscribe" if self.parent_depth == 0 => {
                Some(self.subscription(SubscriptionAction::Unsubscribe, args))
            }
            "await_resource" if self.parent_depth == 0 => Some(self.await_resource(args)),
            #[cfg(feature = "workflow")]
            "workflow.define" if self.parent_depth == 0 => Some(self.workflow_define(args)),
            #[cfg(feature = "workflow")]
            "workflow.patch" if self.parent_depth == 0 => Some(self.workflow_patch(args)),
            #[cfg(feature = "workflow")]
            "workflow.run" if self.parent_depth == 0 => Some(self.workflow_run(args)),
            _ => None,
        }
    }

    fn read_resource(&mut self, uri: &str) -> Option<(String, bool)> {
        // `agentd://subagent/<handle>` reads an async child's completion as a
        // resource (completion-as-self-resource, RFC 0009) — the same idempotent
        // peek as subagent.status (a detached child is not collectable).
        match crate::agentd_uri::AgentdResource::parse(uri) {
            Some(crate::agentd_uri::AgentdResource::Subagent(handle)) => Some(
                self.peek_child(&handle)
                    .unwrap_or_else(|| (format!("no async subagent with handle '{handle}'"), true)),
            ),
            // agentd://status is a served-only resource; not served from a subagent.
            _ => None,
        }
    }

    fn serves_self_resources(&self) -> bool {
        // Async children (and thus agentd://subagent/<handle> resources) are
        // possible exactly when delegation is.
        self.can_nest()
    }

    fn take_scheduled(&mut self) -> Vec<ScheduleRequest> {
        std::mem::take(&mut self.scheduled)
    }

    fn take_subscriptions(&mut self) -> Vec<SubscriptionRequest> {
        std::mem::take(&mut self.subscriptions)
    }
}

impl Drop for Orchestrator {
    fn drop(&mut self) {
        // Async children that were never collected die with the parent — no
        // orphan outlives the tree (each `Subagent` reaps on its own Drop). Just
        // record what was force-reaped (and how many were detached).
        if !self.async_children.is_empty() {
            let detached = self.async_children.values().filter(|c| c.detached).count();
            self.log.info(
                "subagent.async_reaped",
                json!({"uncollected": self.async_children.len(), "detached": detached}),
            );
        }
        // The tree is collapsing — clear the live-children gauges so a daemon's
        // next run doesn't inherit a stale active/saturation reading.
        self.async_children.clear();
        self.publish_tree_shape();
    }
}

fn refused(why: &str) -> (String, bool) {
    (format!("subagent.spawn refused: {why}"), true)
}

#[cfg(feature = "a2a")]
fn a2a_refused(why: &str) -> (String, bool) {
    (format!("a2a.delegate refused: {why}"), true)
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn distill(result: &Value) -> String {
    let s = match result {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    if s.len() > DISTILL_CAP {
        format!("{}… [truncated]", &s[..DISTILL_CAP])
    } else {
        s
    }
}

fn spawn_tool_def() -> ToolDef {
    #[allow(unused_mut)]
    let mut def = ToolDef {
        name: "subagent.spawn".into(),
        description: "Delegate a focused subtask to a fresh child agent. By DEFAULT the call BLOCKS \
            until the child finishes and returns its distilled result. Pass async=true to instead \
            get a handle back immediately and keep working — then collect the result later with \
            subagent.await (blocks) or subagent.status (peek). Pass detach=true for fire-and-forget \
            (you will not collect a result). Give a clear 'instruction' and (strongly recommended) \
            an 'output_contract' stating exactly what the child should return. Optionally pass \
            'context' (only the facts the child needs — it does not see your conversation) and \
            'servers' (a subset of tool-server names to grant it). Use async to run independent \
            subtasks in parallel."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "instruction": {"type": "string", "description": "the subtask for the child agent"},
                "output_contract": {"type": "string", "description": "exactly what the child should return"},
                "context": {"type": "string", "description": "only the facts the child needs"},
                "servers": {"type": "array", "items": {"type": "string"}, "description": "subset of MCP server names to grant"},
                "async": {"type": "boolean", "description": "return a handle immediately instead of blocking (collect later)"},
                "detach": {"type": "boolean", "description": "fire-and-forget; do not collect a result"}
            },
            "required": ["instruction"]
        }),
    };
    // A workflow build lets a parent hand the child a whole WORKFLOW to drive
    // instead of an instruction (pivot Phase 7 · W4).
    #[cfg(feature = "workflow")]
    {
        def.input_schema["properties"]["workflow"] = json!({
            "type": "object",
            "description": "a workflow graph {start, nodes} for the child to drive instead of the instruction (same shape as workflow.define); 'instruction' becomes optional"
        });
        def.input_schema["required"] = json!([]);
    }
    def
}

fn status_tool_def() -> ToolDef {
    ToolDef {
        name: "subagent.status".into(),
        description: "Check on an async child you spawned (by 'handle'). Returns 'still running', or \
            — once it has finished — its distilled result (repeatable: the handle is not consumed, so \
            you can check again). Non-blocking."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {"handle": {"type": "string", "description": "the handle returned by subagent.spawn async=true"}},
            "required": ["handle"]
        }),
    }
}

fn await_tool_def() -> ToolDef {
    ToolDef {
        name: "subagent.await".into(),
        description: "Wait for an async child you spawned (by 'handle') to finish and return its \
            distilled result. Blocks until it completes; if it is taking a while it returns 'still \
            running' so you can do other work and await again."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {"handle": {"type": "string", "description": "the handle returned by subagent.spawn async=true"}},
            "required": ["handle"]
        }),
    }
}

/// The `a2a.delegate` tool def (RFC 0020 §3). Lists the declared peer names in
/// the description + as a schema `enum` so the model can only pick a real peer.
#[cfg(feature = "a2a")]
fn a2a_delegate_tool_def(peers: &[crate::config::A2aPeerSpec]) -> ToolDef {
    let names: Vec<String> = peers.iter().map(|p| p.name.clone()).collect();
    let peer_list = names.join(", ");
    ToolDef {
        name: "a2a.delegate".into(),
        description: format!(
            "Delegate a focused objective to a REMOTE agent over A2A (Agent2Agent) and get its \
            distilled result back. This is the cross-mesh alternative to subagent.spawn: instead \
            of a local supervised child, the work runs on another agent reachable through a \
            declared peer. BLOCKS until the remote task reaches a terminal state (bounded by a \
            deadline), then returns its final artifact (the distillate) — or an error observation \
            if the remote failed/rejected/timed out. Give a clear 'objective' and (strongly \
            recommended) an 'output_contract' stating exactly what to return. Available peers: \
            {peer_list}."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "peer": {"type": "string", "enum": names, "description": "the declared A2A peer to delegate to"},
                "objective": {"type": "string", "description": "the objective for the remote agent"},
                "output_contract": {"type": "string", "description": "exactly what the remote agent should return"}
            },
            "required": ["peer", "objective"]
        }),
    }
}

fn schedule_tool_def() -> ToolDef {
    ToolDef {
        name: "schedule".into(),
        description: "Schedule a future wake-up of yourself. After 'after_seconds' have elapsed, \
            agentd re-invokes you with 'instruction' as a fresh run. Use it to defer work, poll a \
            slow resource later, or set your own next tick instead of blocking. Effective only when \
            agentd runs as a long-lived daemon (reactive/loop/schedule); ignored in a one-shot run."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "after_seconds": {"type": "integer", "description": "delay in seconds before the wake-up fires"},
                "instruction": {"type": "string", "description": "what the woken agent should do"}
            },
            "required": ["after_seconds", "instruction"]
        }),
    }
}

fn subscribe_tool_def() -> ToolDef {
    ToolDef {
        name: "subscribe".into(),
        description: "Subscribe yourself to an MCP resource by uri. When that resource changes, \
            agentd wakes you with its current content — so you can watch something and react to it \
            later instead of polling. Effective only when agentd runs as a long-lived daemon."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {"uri": {"type": "string", "description": "the resource uri to watch"}},
            "required": ["uri"]
        }),
    }
}

fn unsubscribe_tool_def() -> ToolDef {
    ToolDef {
        name: "unsubscribe".into(),
        description:
            "Stop watching an MCP resource you previously subscribed to (by uri). Use this \
            when you no longer need to react to its changes."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {"uri": {"type": "string", "description": "the resource uri to stop watching"}},
            "required": ["uri"]
        }),
    }
}

/// `await_resource` (pivot Phase 5.2) — the conditional WAIT self-tool. Parks the
/// session on a resource until its content satisfies a predicate, then re-enters.
fn await_resource_tool_def() -> ToolDef {
    ToolDef {
        name: "await_resource".into(),
        description: "Wait for an MCP resource to reach a specific state before continuing. \
            Give a resource uri and a condition on its JSON content; agentd wakes you with \
            the resource's current content only once the condition holds — so you can pause \
            on a dependency (a job finishing, a flag flipping) instead of polling. End your \
            turn after calling this; the daemon re-enters this session when the wait is \
            satisfied. Effective only when agentd runs as a long-lived daemon."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "uri": {"type": "string", "description": "the resource uri to wait on"},
                "condition": {
                    "type": "object",
                    "description": "a predicate on the resource's JSON content",
                    "properties": {
                        "pointer": {"type": "string", "description": "RFC 6901 JSON Pointer into the content (e.g. \"/status\"); empty = whole document"},
                        "op": {"type": "string", "enum": ["exists", "eq", "ne", "gt", "lt", "contains"], "description": "the comparison to apply at the pointer (default: exists)"},
                        "value": {"description": "the value to compare against (required for eq/ne/gt/lt/contains)"}
                    }
                }
            },
            "required": ["uri", "condition"]
        }),
    }
}

/// `workflow.define` (pivot Phase 7) — author + store a workflow for `workflow.run`.
#[cfg(feature = "workflow")]
fn workflow_define_tool_def() -> ToolDef {
    ToolDef {
        name: "workflow.define".into(),
        description: "Define a workflow: a graph of nodes connected by labelled edges — cycles and \
            conditional branches allowed — that agentd drives to process work items by itself. \
            Node kinds: agent (a full agentic turn), tool (one MCP call; args may embed \
            {\"$from\": key, \"pointer\": \"/p\", \"default\": v} blackboard references), assign \
            (pure data shaping, no model call), infer (one structured intelligence call validated \
            against a schema of field->type, with automatic re-asks), branch (deterministic \
            predicates over the blackboard, plus an optional semantic judgement), wait (suspend on \
            an MCP resource), subgraph, and halt. Effectful nodes accept a retry {max, backoff_ms} \
            policy. Provide the workflow as a JSON object {start, nodes}; agentd validates it \
            structurally (it must be able to reach a halt) and returns a workflow id you then pass \
            to workflow.run. Use this to orchestrate multi-step, looping, or conditional work \
            without hand-holding each turn."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "workflow": {
                    "type": "object",
                    "description": "the workflow graph: {\"start\": nodeId, \"nodes\": {id: {kind, ...}}}"
                }
            },
            "required": ["workflow"]
        }),
    }
}

/// `workflow.run` (pivot Phase 7 · P6) — drive a workflow the agent defined to completion.
#[cfg(feature = "workflow")]
fn workflow_run_tool_def() -> ToolDef {
    ToolDef {
        name: "workflow.run".into(),
        description: "Run a workflow you defined (by workflow_id) to completion, and get back its \
            final status + result. agentd drives the whole graph itself — running each agent/tool \
            node, taking branches on the data or a judgement, and looping as the graph directs — \
            so you orchestrate multi-step work by defining a graph once and running it, instead of \
            steering every step. Runs synchronously and returns when the graph terminates."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "workflow_id": {"type": "string", "description": "the id returned by workflow.define"},
                "detach": {"type": "boolean", "description": "run it in a spawned subagent and return a handle immediately (collect via subagent.status/subagent.await); default false = run synchronously here"}
            },
            "required": ["workflow_id"]
        }),
    }
}

/// `workflow.patch` (pivot Phase 7 · P5) — grow a stored workflow additively.
#[cfg(feature = "workflow")]
fn workflow_patch_tool_def() -> ToolDef {
    ToolDef {
        name: "workflow.patch".into(),
        description: "Extend a workflow you defined, additively: add new nodes and/or new edges \
            to existing nodes (never overwrite a node or retarget an edge). Give the workflow_id and \
            a patch {add_nodes, add_edges}; agentd re-validates the grown graph and rejects the \
            patch if it would break termination. Use this to elaborate your plan as you learn more, \
            without redefining the whole graph."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "workflow_id": {"type": "string", "description": "the id returned by workflow.define"},
                "patch": {
                    "type": "object",
                    "description": "additive changes",
                    "properties": {
                        "add_nodes": {"type": "object", "description": "new nodes {id: {kind, ...}}"},
                        "add_edges": {"type": "array", "description": "new edges [{from, label, to}]"}
                    }
                }
            },
            "required": ["workflow_id", "patch"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::log::{Comp, Level, LogCtx};

    fn logger() -> Logger {
        Logger::new(
            LogCtx {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Agent,
                pid: 0,
                trace_id: None,
            },
            Level::Error,
        )
    }

    fn payload(depth: u32, max_depth: u32) -> SpawnPayload {
        SpawnPayload {
            instruction: "parent".into(),
            output_contract: None,
            context_seed: Vec::new(),
            intelligence: IntelConfig {
                uri: "https://intel.example".into(),
                token: None,
                model: None,
            },
            mcp_servers: vec![
                McpServerSpec {
                    name: "fs".into(),
                    endpoint: "unix:/a.sock".into(),
                    tags: Vec::new(),
                    ..Default::default()
                },
                McpServerSpec {
                    name: "db".into(),
                    endpoint: "unix:/b.sock".into(),
                    tags: Vec::new(),
                    ..Default::default()
                },
            ],
            a2a_peers: Vec::new(),
            limits: Limits {
                max_steps: 10,
                max_tokens: 1000,
                deadline_ms: 1000,
                max_depth,
            },
            telemetry: Telemetry {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                trace_id: None,
                log_level: "error".into(),
                log_content: false,
            },
            depth,
            warm: false,
            #[cfg(feature = "workflow")]
            workflow: None,
        }
    }

    #[test]
    fn refuses_when_at_max_depth() {
        // depth 4, max_depth 4 → can't nest (child would be depth 5).
        let mut o = Orchestrator::from_payload(
            "agentd".into(),
            &payload(4, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(o.tools().is_empty());
        let (msg, is_err) = o.spawn(&json!({"instruction": "x"}));
        assert!(is_err);
        assert!(msg.contains("depth"));
    }

    #[test]
    fn advertises_tool_with_depth_budget() {
        // The root (depth 0) advertises delegation + self-scheduling + self-subscribe.
        let o = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        let tools = o.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"subagent.spawn"));
        assert!(names.contains(&"schedule"));
        assert!(names.contains(&"subscribe"));
        assert!(names.contains(&"unsubscribe"));
    }

    #[test]
    fn subscribe_is_root_only_and_accumulates() {
        // A nested child does not get the self-subscription tools.
        let mut child = Orchestrator::from_payload(
            "agentd".into(),
            &payload(1, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(!child.tools().iter().any(|t| t.name == "subscribe"));
        assert!(
            child
                .handle("subscribe", &json!({"uri": "file:///x"}))
                .is_none()
        );

        // The root accumulates subscribe/unsubscribe requests, drained by take.
        let mut root = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(
            !root
                .handle("subscribe", &json!({"uri": "file:///watch"}))
                .unwrap()
                .1
        );
        assert!(
            !root
                .handle("unsubscribe", &json!({"uri": "file:///old"}))
                .unwrap()
                .1
        );
        assert!(
            root.handle("subscribe", &json!({"uri": "  "})).unwrap().1,
            "empty uri → error"
        );
        let drained = root.take_subscriptions();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].uri, "file:///watch");
        assert_eq!(drained[0].action, SubscriptionAction::Subscribe);
        assert_eq!(drained[1].action, SubscriptionAction::Unsubscribe);
        assert!(root.take_subscriptions().is_empty(), "take drains");
    }

    #[test]
    fn await_resource_is_root_only_validates_and_arms_a_conditional_subscription() {
        // Root-only, like the other reactive self-tools: a nested child does not
        // get it (its wait would be lost to the parent), and handle() declines it.
        let mut child = Orchestrator::from_payload(
            "agentd".into(),
            &payload(1, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(!child.tools().iter().any(|t| t.name == "await_resource"));
        assert!(
            child
                .handle(
                    "await_resource",
                    &json!({"uri": "file:///w", "condition": {"op": "exists"}})
                )
                .is_none()
        );

        let mut root = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(
            root.tools().iter().any(|t| t.name == "await_resource"),
            "root advertises await_resource"
        );
        // A valid await arms a CONDITIONAL subscribe request + a parked observation.
        let (obs, err) = root
            .handle(
                "await_resource",
                &json!({"uri": "file:///job", "condition": {"pointer": "/status", "op": "eq", "value": "done"}}),
            )
            .unwrap();
        assert!(!err, "{obs}");
        assert!(obs.contains("re-enter"), "parked observation: {obs}");
        // Missing condition / missing uri / a malformed predicate are refused as
        // tool results (not crashes) and arm NOTHING.
        assert!(
            root.handle("await_resource", &json!({"uri": "file:///x"}))
                .unwrap()
                .1,
            "missing condition → error"
        );
        assert!(
            root.handle("await_resource", &json!({"condition": {"op": "exists"}}))
                .unwrap()
                .1,
            "missing uri → error"
        );
        assert!(
            root.handle(
                "await_resource",
                &json!({"uri": "file:///x", "condition": {"op": "nope"}})
            )
            .unwrap()
            .1,
            "bad op → error"
        );
        // Only the one valid await armed a request; it carries the condition.
        let drained = root.take_subscriptions();
        assert_eq!(drained.len(), 1, "only the valid await armed a request");
        assert_eq!(drained[0].uri, "file:///job");
        assert_eq!(drained[0].action, SubscriptionAction::Subscribe);
        assert_eq!(
            drained[0].condition.as_ref().expect("carries a condition")["op"],
            "eq"
        );
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_patch_grows_a_defined_graph_additively() {
        let mut root = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(root.tools().iter().any(|t| t.name == "workflow.patch"));
        // Define a graph, then patch it additively.
        let g = json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "do", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        });
        let (obs, err) = root.handle("workflow.define", &json!({"workflow": g})).unwrap();
        assert!(!err, "{obs}");
        let id = obs.split_whitespace().nth(2).unwrap().to_string(); // "workflow defined: w1 (...)"
        // A valid additive patch is accepted.
        let (obs, err) = root
            .handle(
                "workflow.patch",
                &json!({"workflow_id": id, "patch": {"add_edges": [{"from": "a", "label": "error", "to": "h"}]}}),
            )
            .unwrap();
        assert!(!err, "additive patch accepted: {obs}");
        // Unknown graph id + a retargeting patch are refused.
        assert!(
            root.handle("workflow.patch", &json!({"workflow_id": "gX", "patch": {}}))
                .unwrap()
                .1,
            "unknown id refused"
        );
        assert!(
            root.handle(
                "workflow.patch",
                &json!({"workflow_id": id, "patch": {"add_edges": [{"from": "a", "label": "ok", "to": "a"}]}})
            )
            .unwrap()
            .1,
            "retarget refused"
        );
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_run_drives_a_defined_graph_and_refuses_unknown_ids() {
        // Empty servers (no connect) + loopback-refused intel (`payload` sets a 1s
        // deadline, so the agent node fails FAST without a live LLM): the agent node
        // errors, the graph follows its `error` edge to the crashed halt, and
        // workflow.run returns a terminal graph summary — no hang, no panic.
        let mut p = payload(0, 4);
        p.mcp_servers = vec![];
        p.intelligence.uri = "http://127.0.0.1:9".into(); // loopback, nothing listening
        let mut root =
            Orchestrator::from_payload("agentd".into(), &p, Duration::from_secs(5), logger());
        assert!(
            root.tools().iter().any(|t| t.name == "workflow.run"),
            "root advertises workflow.run"
        );
        let g = json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "work", "writes": "out", "edges": {"ok": "done", "error": "fail"}},
                "done": {"kind": "halt", "status": "completed", "result_from": "out"},
                "fail": {"kind": "halt", "status": "crashed"}
            }
        });
        let (obs, err) = root.handle("workflow.define", &json!({"workflow": g})).unwrap();
        assert!(!err, "{obs}");
        let id = obs.split_whitespace().nth(2).unwrap().to_string();
        // Run it: the agent node fails on refused intel → error edge → crashed halt.
        let (obs, err) = root.handle("workflow.run", &json!({"workflow_id": id})).unwrap();
        assert!(err, "a graph that halts crashed is an error result: {obs}");
        assert!(obs.contains("\"status\""), "workflow.run returns a status summary: {obs}");
        // An unknown / missing id is refused (never drives).
        assert!(
            root.handle("workflow.run", &json!({"workflow_id": "gX"})).unwrap().1,
            "unknown id refused"
        );
        assert!(
            root.handle("workflow.run", &json!({})).unwrap().1,
            "missing id refused"
        );
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_define_is_root_only_validates_and_stores() {
        // Nested child does not get the graph-authoring surface.
        let mut child = Orchestrator::from_payload(
            "agentd".into(),
            &payload(1, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(!child.tools().iter().any(|t| t.name == "workflow.define"));
        assert!(child.handle("workflow.define", &json!({"workflow": {}})).is_none());

        let mut root = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(
            root.tools().iter().any(|t| t.name == "workflow.define"),
            "root advertises workflow.define"
        );
        // A valid graph is stored + an id returned.
        let g = json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "do", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        });
        let (obs, err) = root.handle("workflow.define", &json!({"workflow": g})).unwrap();
        assert!(!err, "{obs}");
        assert!(obs.contains("workflow defined"), "{obs}");
        // A structurally invalid graph (a bare self-loop, no reachable halt) is
        // REFUSED as a tool result, not stored.
        let bad = json!({
            "start": "a",
            "nodes": {"a": {"kind": "agent", "instruction": "spin", "edges": {"ok": "a"}}}
        });
        let (obs, err) = root.handle("workflow.define", &json!({"workflow": bad})).unwrap();
        assert!(err, "no-halt graph must be refused: {obs}");
        assert!(obs.contains("invalid workflow"), "{obs}");
        // Missing 'graph' arg is refused.
        assert!(
            root.handle("workflow.define", &json!({})).unwrap().1,
            "missing graph refused"
        );
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn spawn_validates_an_attached_workflow_at_the_boundary() {
        let mut root = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        // Malformed workflow → tool error naming the parse problem.
        let (obs, err) = root
            .handle("subagent.spawn", &json!({"workflow": {"start": 1}}))
            .unwrap();
        assert!(err, "{obs}");
        assert!(obs.contains("malformed workflow"), "{obs}");
        // Structurally invalid (no reachable halt) → refused, never spawned.
        let bad = json!({"start": "a", "nodes": {"a": {"kind": "agent", "instruction": "spin", "edges": {"ok": "a"}}}});
        let (obs, err) = root
            .handle("subagent.spawn", &json!({"workflow": bad}))
            .unwrap();
        assert!(err, "{obs}");
        assert!(obs.contains("invalid workflow"), "{obs}");
        // Neither instruction nor workflow → the combined requirement message.
        let (obs, err) = root.handle("subagent.spawn", &json!({})).unwrap();
        assert!(err);
        assert!(obs.contains("'instruction' (or a 'workflow')"), "{obs}");
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_run_detach_hands_the_workflow_to_a_spawned_child() {
        // `/bin/true` stands in for the agentd binary: the process SPAWN succeeds
        // (which is all detach needs to mint a handle); the "child" then exits
        // without speaking the protocol, which the async registry reports as a
        // terminal failure — the plumbing under test is the workflow→spawn hand-off.
        let mut root = Orchestrator::from_payload(
            "/bin/true".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        let g = json!({
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "do", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed"}
            }
        });
        let (obs, err) = root.handle("workflow.define", &json!({"workflow": g})).unwrap();
        assert!(!err, "{obs}");
        let id = obs.split_whitespace().nth(2).unwrap().to_string();
        // detach → the spawn path returns a HANDLE immediately (the child drives
        // the workflow in the background; collect via subagent.status/await).
        let (obs, err) = root
            .handle("workflow.run", &json!({"workflow_id": id, "detach": true}))
            .unwrap();
        assert!(!err, "detach returns a handle observation: {obs}");
        assert!(obs.contains("handle="), "{obs}");
        // The handle is trackable through the normal subagent surface.
        let h = obs
            .split("handle=")
            .nth(1)
            .and_then(|r| r.split(')').next())
            .unwrap()
            .to_string();
        let (obs, err) = root
            .handle("subagent.status", &json!({"handle": h}))
            .unwrap();
        assert!(!err, "the handle is queryable: {obs}");
    }

    #[test]
    fn schedule_is_root_only_and_accumulates() {
        // A nested child (depth 1) does NOT get `schedule` (its request would be
        // lost to the parent), and handle() declines it.
        let mut child = Orchestrator::from_payload(
            "agentd".into(),
            &payload(1, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(!child.tools().iter().any(|t| t.name == "schedule"));
        assert!(
            child
                .handle("schedule", &json!({"after_seconds": 5, "instruction": "x"}))
                .is_none()
        );

        // The root accumulates valid requests, drained by take_scheduled.
        let mut root = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        let (_m, err) = root
            .handle(
                "schedule",
                &json!({"after_seconds": 30, "instruction": "poll again"}),
            )
            .unwrap();
        assert!(!err);
        let drained = root.take_scheduled();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].after_ms, 30_000);
        assert_eq!(drained[0].instruction, "poll again");
        assert!(root.take_scheduled().is_empty(), "take_scheduled drains");
    }

    #[test]
    fn schedule_validates_delay_and_instruction() {
        let mut o = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        // out-of-range / missing delay → error observation, nothing queued
        assert!(
            o.schedule(&json!({"after_seconds": 0, "instruction": "x"}))
                .1
        );
        assert!(o.schedule(&json!({"instruction": "x"})).1);
        // empty instruction → error
        assert!(
            o.schedule(&json!({"after_seconds": 5, "instruction": "  "}))
                .1
        );
        assert!(o.take_scheduled().is_empty());
        // cap is enforced
        for _ in 0..MAX_SCHEDULED {
            assert!(
                !o.schedule(&json!({"after_seconds": 5, "instruction": "x"}))
                    .1
            );
        }
        assert!(
            o.schedule(&json!({"after_seconds": 5, "instruction": "x"}))
                .1,
            "over cap → refused"
        );
        assert_eq!(o.take_scheduled().len(), MAX_SCHEDULED);
    }

    #[test]
    fn status_and_await_reject_unknown_handles() {
        let mut o = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        let (msg, err) = o.status(&json!({"handle": "0.7"}));
        assert!(
            err && msg.contains("no async subagent"),
            "status on an unknown handle errors: {msg}"
        );
        let (msg, err) = o.await_child(&json!({"handle": "0.7"}));
        assert!(
            err && msg.contains("no async subagent"),
            "await on an unknown handle errors: {msg}"
        );
    }

    #[test]
    fn spawn_schema_advertises_async_and_detach() {
        let def = spawn_tool_def();
        let props = &def.input_schema["properties"];
        assert!(
            props.get("async").is_some(),
            "spawn schema must offer async"
        );
        assert!(
            props.get("detach").is_some(),
            "spawn schema must offer detach"
        );
    }

    #[test]
    fn spawn_rate_cap_refuses_after_burst() {
        // The rate cap is checked before any child launch, so we can exhaust the
        // burst straight from the bucket (no real processes) and assert that the
        // next spawn is refused on rate — an isError tool result, never a crash.
        let mut o = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        for i in 0..SPAWN_RATE_BURST {
            assert!(
                o.spawn_bucket.try_take(),
                "burst token {i} should be available"
            );
        }
        let (msg, is_err) = o.spawn(&json!({"instruction": "churn"}));
        assert!(is_err, "a rate-limited spawn is an error observation");
        assert!(
            msg.contains("spawn rate exceeded"),
            "refusal must name the rate cap; got: {msg}"
        );
    }

    #[test]
    fn empty_instruction_is_rejected() {
        let mut o = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        let (_msg, is_err) = o.spawn(&json!({"instruction": "   "}));
        assert!(is_err);
    }

    #[test]
    fn narrow_servers_filters_to_subset() {
        let o = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        let narrowed = o.narrow_servers(&json!({"servers": ["fs", "ghost"]}));
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].name, "fs");
        // no subset → inherit all
        assert_eq!(o.narrow_servers(&json!({})).len(), 2);
    }

    #[test]
    fn distill_truncates() {
        let big = Value::String("x".repeat(DISTILL_CAP + 100));
        assert!(distill(&big).ends_with("[truncated]"));
        assert_eq!(distill(&Value::String("short".into())), "short");
    }

    #[test]
    fn advertised_self_tools_are_all_members_of_the_named_class() {
        use crate::agentloop::action::SELF_CONTROL_TOOLS;
        // Drift guard (pivot Phase 5.1 — name the class): everything a handler can
        // advertise must be a member of the named self/control class, so a newly
        // added self-tool cannot silently escape the class boundary. A ROOT handler
        // advertises the widest set (delegation + the root-only reactive tools).
        let root = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        let mut names: Vec<String> = root.tools().into_iter().map(|t| t.name).collect();
        // resource.read is added by the runner (not the handler) but is a member of
        // the class — include it so the guard covers the whole self/control surface.
        names.push("resource.read".into());
        for n in &names {
            assert!(
                SELF_CONTROL_TOOLS.contains(&n.as_str()),
                "advertised self-tool {n} must be a member of the named class"
            );
        }
        // Sanity: the guard is checking a non-trivial set (the root reactive +
        // delegation primitives are actually present).
        for expect in ["subagent.spawn", "schedule", "subscribe", "unsubscribe"] {
            assert!(names.iter().any(|n| n == expect), "root advertises {expect}");
        }
        // Principle 2: no advertised self-tool is a local-exec primitive.
        for n in &names {
            for bad in ["exec", "shell", "bash", "command", "system"] {
                assert!(
                    !n.contains(bad),
                    "self-tool {n} must not be a local-exec primitive"
                );
            }
        }
    }

    // ── a2a.delegate (RFC 0020 §3) ───────────────────────────────────────────

    #[cfg(feature = "a2a")]
    fn payload_with_peers(peers: Vec<crate::config::A2aPeerSpec>) -> SpawnPayload {
        let mut p = payload(0, 4);
        p.a2a_peers = peers;
        p
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn a2a_delegate_advertised_only_when_peers_configured() {
        // No peers → the tool is not advertised and handle() declines it.
        let mut bare = Orchestrator::from_payload(
            "agentd".into(),
            &payload(0, 4),
            Duration::from_secs(5),
            logger(),
        );
        assert!(
            !bare.tools().iter().any(|t| t.name == "a2a.delegate"),
            "a2a.delegate must not be advertised with no peers"
        );
        assert!(
            bare.handle("a2a.delegate", &json!({"peer": "p", "objective": "x"}))
                .is_none(),
            "a2a.delegate must fall through to MCP when no peers are declared"
        );

        // One declared peer → the tool is advertised, with the peer in its enum.
        let peers = vec![crate::config::A2aPeerSpec {
            name: "mesh".into(),
            endpoint: "unix:/run/peer.sock".into(),
        }];
        let with = Orchestrator::from_payload(
            "agentd".into(),
            &payload_with_peers(peers),
            Duration::from_secs(5),
            logger(),
        );
        let def = with
            .tools()
            .into_iter()
            .find(|t| t.name == "a2a.delegate")
            .expect("a2a.delegate advertised with a peer");
        assert_eq!(def.input_schema["properties"]["peer"]["enum"][0], "mesh");
        // Drift guard (Phase 5.1): the remote-delegation self-tool is a member of
        // the named self/control class.
        assert!(
            crate::agentloop::action::SELF_CONTROL_TOOLS.contains(&"a2a.delegate"),
            "a2a.delegate is a self/control class member"
        );
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn a2a_delegate_unknown_peer_is_an_error_observation() {
        let peers = vec![crate::config::A2aPeerSpec {
            name: "mesh".into(),
            endpoint: "unix:/run/peer.sock".into(),
        }];
        let mut o = Orchestrator::from_payload(
            "agentd".into(),
            &payload_with_peers(peers),
            Duration::from_secs(5),
            logger(),
        );
        let (msg, is_err) = o
            .handle(
                "a2a.delegate",
                &json!({"peer": "ghost", "objective": "do x"}),
            )
            .expect("a2a.delegate is a self-tool when peers exist");
        assert!(is_err, "unknown peer → isError, not a crash: {msg}");
        assert!(msg.contains("no A2A peer named 'ghost'"), "got: {msg}");
        // The breadth count is NOT consumed by an unknown-peer refusal.
        assert_eq!(o.child_count, 0, "a failed lookup must not consume breadth");
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn a2a_delegate_missing_args_are_errors() {
        let peers = vec![crate::config::A2aPeerSpec {
            name: "mesh".into(),
            endpoint: "unix:/run/peer.sock".into(),
        }];
        let mut o = Orchestrator::from_payload(
            "agentd".into(),
            &payload_with_peers(peers),
            Duration::from_secs(5),
            logger(),
        );
        assert!(
            o.delegate(&json!({"objective": "x"})).1,
            "missing peer → error"
        );
        assert!(
            o.delegate(&json!({"peer": "mesh"})).1,
            "missing objective → error"
        );
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn a2a_delegate_refuses_at_the_breadth_cap() {
        // The remote delegation shares the local-spawn breadth cap: at the cap it
        // is refused-as-tool-result, never attempted. We saturate child_count by
        // hand (no real peer is dialed) and assert the refusal.
        let peers = vec![crate::config::A2aPeerSpec {
            name: "mesh".into(),
            endpoint: "unix:/run/peer.sock".into(),
        }];
        let mut o = Orchestrator::from_payload(
            "agentd".into(),
            &payload_with_peers(peers),
            Duration::from_secs(5),
            logger(),
        );
        o.child_count = MAX_CHILDREN;
        let (msg, is_err) = o.delegate(&json!({"peer": "mesh", "objective": "do x"}));
        assert!(is_err, "at the breadth cap a delegation is refused: {msg}");
        assert!(
            msg.contains("maximum number of delegations"),
            "refusal must name the breadth cap; got: {msg}"
        );
    }
}
