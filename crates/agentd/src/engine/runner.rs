//! Sequential DAG traversal engine.
//!
//! One engine instance owns a [`HandlerRegistry`]. `Engine::run`
//! resolves the entry node, walks the DAG one node at a time, and
//! returns an [`ExecutionOutcome`] once the run ends (terminate,
//! fail, deadline, or dead-end).
//!
//! Phase 2 is deliberately **sequential-only** — no parallel
//! branches. A node has at most one unconditional out-edge and any
//! number of `when`-labelled out-edges (for Switch / Condition).
//! Parallel fan-out is tracked as a later-tier extension (RFC §9.1).

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tracing::{debug, error, info, info_span, warn};

use crate::engine::context::{ExecutionContext, RunOptions, TriggerMeta};
use crate::engine::handler::HandlerRegistry;
use crate::engine::outcome::{ExecutionOutcome, ExecutionTrace, NodeOutcome, TraceEntry};
use crate::error::{Error, Result};
use crate::observability::Metrics;
use crate::workflow::{Edge, WorkflowDoc};

/// Hard safety bound on the number of node steps per run. Well above
/// anything a legitimate Phase 2 workflow would hit; exists to catch
/// engine bugs that would otherwise walk forever. (Graph acyclicity
/// is already validator-checked; this is pure belt-and-suspenders.)
const MAX_STEPS: usize = 10_000;

/// How deep `call` nodes may nest before the engine refuses — a
/// belt-and-suspenders bound against mutually-recursive workflows.
const MAX_CALL_DEPTH: u32 = 8;

/// Monotonically-incrementing execution id counter. Scoped to the
/// process; each engine instance shares it via a process-wide atomic.
static EXEC_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_execution_id() -> String {
    let n = EXEC_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // A per-process counter alone resets to 1 every invocation, which
    // would collide checkpoint files across separate runs. Mix in the
    // wall-clock second and the pid so a run id is unique across
    // processes (for `--resume`) while staying sortable and terse.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("exec-{secs:x}-{:x}-{n:x}", std::process::id())
}

pub struct Engine {
    pub registry: HandlerRegistry,
    pub metrics: Arc<Metrics>,
    /// Hot-reload handles for SIGHUP-replaceable engine components
    ///. Each handler already holds an `Arc` clone of the
    /// corresponding reloadable wrapper; updating the inner state
    /// through these handles takes effect on the next check without
    /// rebuilding the registry.
    pub reload: ReloadHandles,
    /// Where `pause_for_approval` writes checkpoints, and where
    /// `resume` reads them. `None` → a `pause_for_approval` node is a
    /// configuration error (no place to persist the run).
    pub state_dir: Option<std::path::PathBuf>,
}

/// Handles for components the runtime can hot-swap on SIGHUP. Each
/// field is optional because a given workflow may not use that
/// component at all (no intelligence adapter, no MCP child, no
/// policy block ...).
#[derive(Default)]
pub struct ReloadHandles {
    /// The `ReloadablePolicy` wrapper the tool handlers reach
    /// through when checking fs/env/http/shell side effects.
    /// `None` only when no tool feature is compiled in — policy
    /// handlers exist even when the underlying workflow declares
    /// no `[policy]` block (fallback is `AllowAll` wrapped in the
    /// reloadable).
    pub policy: Option<Arc<crate::tools::policy::ReloadablePolicy>>,
    /// The `ReloadableIntelClient` wrapper the intelligence node
    /// handler reaches through. `Some` iff `--intel-unix` or
    /// `--intel-http` was passed at startup (there's no way to
    /// turn intelligence on *during* a reload — the registration
    /// requires a handler registry change, which is out of scope
    /// for the reload pass).
    pub intel: Option<Arc<crate::intelligence::client::ReloadableIntelClient>>,
    /// Named intelligence backends (`[[intelligence.backends]]`,
    /// RFC 0006). SIGHUP rebuilds each entry's inner client —
    /// re-reading `api_key_env` — and swaps in place. Empty when no
    /// named backends are configured.
    pub intel_backends:
        std::collections::HashMap<String, Arc<crate::intelligence::client::ReloadableIntelClient>>,
    /// Process-wide MCP server registry. Each handle inside owns
    /// its own `ReloadableMcpClient` + `ReloadableMcpAllowlist` so
    /// SIGHUP can respawn individual servers / rotate per-server
    /// allowlists without rebuilding the registry. `None` iff no
    /// `[[mcp_servers]]` entries + no `--mcp-stdio` CLI arg. Adding
    /// or removing whole servers across reloads is out of scope
    /// for the hot-reload pass.
    pub mcp: Option<Arc<crate::mcp::McpRegistry>>,
}

impl Engine {
    pub fn new(registry: HandlerRegistry) -> Self {
        Self {
            registry,
            metrics: Metrics::new(),
            reload: ReloadHandles::default(),
            state_dir: None,
        }
    }

    /// Construct an engine that shares metrics with other engines —
    /// useful for `agent serve` where a single `Metrics` aggregates
    /// counters across every request.
    pub fn with_metrics(registry: HandlerRegistry, metrics: Arc<Metrics>) -> Self {
        Self {
            registry,
            metrics,
            reload: ReloadHandles::default(),
            state_dir: None,
        }
    }

    /// Set the directory where `pause_for_approval` checkpoints are
    /// written and `resume` reads them.
    pub fn with_state_dir(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.state_dir = dir;
        self
    }

    /// Attach hot-reload handles after construction. Returned by
    /// value to chain with `.new()` / `.with_metrics()` at the call
    /// site (`build_engine` in `runtime.rs`).
    pub fn with_reload_handles(mut self, reload: ReloadHandles) -> Self {
        self.reload = reload;
        self
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    /// Run one workflow, starting from the given start-node name and
    /// the given trigger. Returns the final [`ExecutionOutcome`].
    pub fn run(
        &self,
        workflow: &WorkflowDoc,
        start_name: &str,
        trigger: TriggerMeta,
        options: RunOptions,
    ) -> Result<ExecutionOutcome> {
        self.run_with_trace(workflow, start_name, trigger, options)
            .map(|(outcome, _)| outcome)
    }

    /// Same as [`run`] but also returns the [`ExecutionTrace`] — the
    /// ordered list of nodes the engine walked, with their outcome
    /// flavour and any branch label. Fixture-driven tests use this
    /// to assert the expected graph path.
    pub fn run_with_trace(
        &self,
        workflow: &WorkflowDoc,
        start_name: &str,
        trigger: TriggerMeta,
        options: RunOptions,
    ) -> Result<(ExecutionOutcome, ExecutionTrace)> {
        self.metrics.inc_workflow_started();
        let mut trace = ExecutionTrace::default();

        // 1) Resolve the start node + its entry node id.
        let start = workflow
            .start_node(start_name)
            .ok_or_else(|| Error::Workflow {
                workflow: workflow.name.clone(),
                reason: format!("unknown start node `{start_name}`"),
            })?;
        let entry_id = resolve_entry(workflow, start)?;

        // 2) Build the context.
        let execution_id = next_execution_id();
        trace.execution_id = execution_id.clone();
        let ctx = ExecutionContext::new(
            execution_id.clone(),
            workflow.name.clone(),
            start_name,
            trigger,
            &options,
        );

        // Root span for the whole run. Every per-node span and
        // event nests under this so log aggregators can group by
        // execution_id.
        let workflow_span = info_span!(
            "workflow.run",
            execution_id = %execution_id,
            workflow_id = %workflow.name,
            start_node = %start_name,
            dry_run = options.dry_run,
        );
        let _run_guard = workflow_span.enter();
        info!(target: "agentd::audit", event = "workflow.started");

        // 3) Walk the DAG. Extracted into `walk_loop` so `resume` can
        // re-enter the same traversal at a checkpoint.
        self.walk_loop(workflow, ctx, entry_id.to_string(), trace, 0)
    }

    /// Resume a paused run: rebuild the context from its checkpoint and
    /// continue at the node after the pause. A resumed run gets a fresh
    /// deadline; on any terminal outcome the checkpoint is discarded.
    pub fn resume(
        &self,
        workflow: &WorkflowDoc,
        checkpoint: crate::engine::checkpoint::Checkpoint,
        options: RunOptions,
    ) -> Result<(ExecutionOutcome, ExecutionTrace)> {
        if checkpoint.workflow != workflow.name {
            return Err(Error::Workflow {
                workflow: workflow.name.clone(),
                reason: format!(
                    "checkpoint is for workflow `{}`, not `{}`",
                    checkpoint.workflow, workflow.name
                ),
            });
        }
        self.metrics.inc_workflow_started();
        let trace = ExecutionTrace::new(checkpoint.run_id.clone());

        let workflow_span = info_span!(
            "workflow.run",
            execution_id = %checkpoint.run_id,
            workflow_id = %workflow.name,
            start_node = %checkpoint.start_node,
            dry_run = options.dry_run,
        );
        let _run_guard = workflow_span.enter();
        info!(
            target: "agentd::audit",
            event = "workflow.resumed",
            paused_at = %checkpoint.paused_at,
        );

        let Some(resume_node) = checkpoint.resume_node.clone() else {
            // The pause node had no successor — resuming just completes.
            self.metrics.inc_workflow_completed();
            return Ok((
                ExecutionOutcome::Completed {
                    final_value: Value::Null,
                    last_node: Some(checkpoint.paused_at.clone()),
                },
                trace,
            ));
        };

        let mut ctx = ExecutionContext::new(
            checkpoint.run_id.clone(),
            workflow.name.clone(),
            checkpoint.start_node.clone(),
            TriggerMeta::from_kind(checkpoint.trigger_kind, checkpoint.trigger_input.clone()),
            &options,
        );
        // Restore the accumulated node outputs from before the pause.
        ctx.node_outputs = checkpoint.node_outputs.clone();

        let result = self.walk_loop(workflow, ctx, resume_node, trace, 0);
        if let (Some(dir), Ok((outcome, _))) = (self.state_dir.as_ref(), &result) {
            if !matches!(outcome, ExecutionOutcome::Paused { .. }) {
                crate::engine::checkpoint::Checkpoint::discard(dir, &checkpoint.run_id);
            }
        }
        result
    }

    /// The DAG traversal itself, shared by a fresh run and a resume.
    fn walk_loop(
        &self,
        workflow: &WorkflowDoc,
        mut ctx: ExecutionContext,
        mut current_id: String,
        mut trace: ExecutionTrace,
        depth: u32,
    ) -> Result<(ExecutionOutcome, ExecutionTrace)> {
        let started_at = Instant::now();
        // Per-run traversal counts for loop edges (declared
        // `max_iterations`), keyed by edge index.
        let mut loop_counts: std::collections::HashMap<usize, u32> =
            std::collections::HashMap::new();

        for _step in 0..MAX_STEPS {
            // Deadline check.
            if Instant::now() >= ctx.deadline {
                let elapsed = Instant::now().duration_since(started_at);
                self.metrics.inc_workflow_timed_out();
                warn!(
                    event = "workflow.timed_out",
                    elapsed_ms = elapsed.as_millis() as u64,
                    last_node = ctx.current_node_id.as_deref().unwrap_or(""),
                );
                return Ok((
                    ExecutionOutcome::TimedOut {
                        elapsed,
                        last_node: ctx.current_node_id,
                    },
                    trace,
                ));
            }

            // Look up the node.
            let node = workflow.node(&current_id).ok_or_else(|| Error::Workflow {
                workflow: workflow.name.clone(),
                reason: format!("node `{current_id}` referenced in traversal is not declared"),
            })?;
            ctx.current_node_id = Some(current_id.clone());

            // pause_for_approval: the engine checkpoints and suspends
            // here instead of dispatching to a handler.
            if let crate::workflow::NodeKind::PauseForApproval { reason } = &node.kind {
                let reason = reason.clone();
                let outcome = self.pause_run(workflow, &ctx, &current_id, reason, &mut trace)?;
                return Ok((outcome, trace));
            }

            // Per-node span. Everything the handler emits nests.
            let node_span = info_span!(
                "node.execute",
                node_id = %current_id,
                kind = %node.kind.name(),
            );
            let node_enter = node_span.enter();
            let node_started = Instant::now();
            self.metrics.inc_node_executed();

            // Dispatch — with optional retry. The first attempt plus
            // `retry.max_attempts - 1` re-tries.
            // `call` runs a sub-workflow on this same engine; every
            // other node kind dispatches to its handler.
            let dispatch = if matches!(node.kind, crate::workflow::NodeKind::Call { .. }) {
                self.run_call(node, &mut ctx, depth)
            } else {
                dispatch_with_retry(&self.registry, node, &mut ctx)
            };
            let latency_ms = node_started.elapsed().as_millis() as u64;

            let outcome = match dispatch {
                Ok(o) => {
                    debug!(event = "node.completed", latency_ms);
                    o
                }
                Err(e) => {
                    self.metrics.inc_node_failed();
                    if matches!(&e, Error::Policy(_)) {
                        self.metrics.inc_policy_denied();
                        warn!(
                            target: "agentd::audit",
                            event = "policy.denied",
                            reason = %e,
                            latency_ms,
                        );
                    } else {
                        error!(
                            event = "node.failed",
                            reason = %e,
                            latency_ms,
                        );
                    }
                    drop(node_enter);
                    self.metrics.inc_workflow_errored();
                    return Err(e);
                }
            };
            drop(node_enter);

            match outcome {
                NodeOutcome::Terminate { value } => {
                    trace.entries.push(TraceEntry {
                        node_id: current_id.clone(),
                        kind: node.kind.name().to_string(),
                        outcome: "terminate",
                        branch: None,
                        output: value.clone(),
                        elapsed_ms: latency_ms,
                    });
                    ctx.node_outputs.insert(current_id.clone(), value.clone());
                    self.metrics.inc_workflow_completed();
                    info!(
                        target: "agentd::audit",
                        event = "workflow.completed",
                        last_node = %current_id,
                        elapsed_ms = started_at.elapsed().as_millis() as u64,
                    );
                    return Ok((
                        ExecutionOutcome::Completed {
                            final_value: value,
                            last_node: Some(current_id),
                        },
                        trace,
                    ));
                }
                NodeOutcome::Fail { reason } => {
                    trace.entries.push(TraceEntry {
                        node_id: current_id.clone(),
                        kind: node.kind.name().to_string(),
                        outcome: "fail",
                        branch: None,
                        output: Value::Null,
                        elapsed_ms: latency_ms,
                    });
                    self.metrics.inc_workflow_failed();
                    warn!(
                        target: "agentd::audit",
                        event = "workflow.failed",
                        last_node = %current_id,
                        reason = %reason,
                        elapsed_ms = started_at.elapsed().as_millis() as u64,
                    );
                    return Ok((
                        ExecutionOutcome::Failed {
                            reason,
                            last_node: Some(current_id),
                        },
                        trace,
                    ));
                }
                NodeOutcome::Continue { value, branch } => {
                    if let Some(label) = &branch {
                        debug!(event = "node.branch", label = %label);
                    }
                    trace.entries.push(TraceEntry {
                        node_id: current_id.clone(),
                        kind: node.kind.name().to_string(),
                        outcome: "continue",
                        branch: branch.clone(),
                        output: value.clone(),
                        elapsed_ms: latency_ms,
                    });
                    ctx.node_outputs.insert(current_id.clone(), value);
                    let next_id =
                        pick_next(workflow, &current_id, branch.as_deref(), &mut loop_counts)?;
                    match next_id {
                        Some(id) => current_id = id,
                        None => {
                            // Dead-end — treat as completion carrying the last value.
                            let final_value = ctx
                                .node_outputs
                                .get(&current_id)
                                .cloned()
                                .unwrap_or(Value::Null);
                            self.metrics.inc_workflow_completed();
                            info!(
                                target: "agentd::audit",
                                event = "workflow.completed",
                                last_node = %current_id,
                                reason = "dead_end",
                                elapsed_ms = started_at.elapsed().as_millis() as u64,
                            );
                            return Ok((
                                ExecutionOutcome::Completed {
                                    final_value,
                                    last_node: Some(current_id),
                                },
                                trace,
                            ));
                        }
                    }
                }
            }
        }

        self.metrics.inc_workflow_errored();
        Err(Error::Workflow {
            workflow: workflow.name.clone(),
            reason: format!(
                "safety cap hit: engine walked {MAX_STEPS} nodes without reaching a \
                 terminal outcome (cycle slipped past the validator?)"
            ),
        })
    }

    /// Write a checkpoint for a `pause_for_approval` node and return the
    /// `Paused` outcome. The resume node is this node's single
    /// successor; a missing state directory is a configuration error.
    fn pause_run(
        &self,
        workflow: &WorkflowDoc,
        ctx: &ExecutionContext,
        node_id: &str,
        reason: Option<String>,
        trace: &mut ExecutionTrace,
    ) -> Result<ExecutionOutcome> {
        let Some(dir) = self.state_dir.clone() else {
            self.metrics.inc_workflow_errored();
            return Err(Error::Workflow {
                workflow: workflow.name.clone(),
                reason: format!(
                    "node `{node_id}` is pause_for_approval but no state directory is \
                     configured (set --state-dir) — nowhere to checkpoint the run"
                ),
            });
        };
        // A pause node's successor is a normal forward edge (not a loop).
        let resume_node = pick_next(
            workflow,
            node_id,
            None,
            &mut std::collections::HashMap::new(),
        )?;
        let checkpoint = crate::engine::checkpoint::Checkpoint {
            run_id: ctx.execution_id.clone(),
            workflow: workflow.name.clone(),
            start_node: ctx.start_node.clone(),
            trigger_kind: ctx.trigger.kind,
            trigger_input: ctx.trigger.input.clone(),
            node_outputs: ctx.node_outputs.clone(),
            paused_at: node_id.to_string(),
            resume_node,
            reason: reason.clone(),
        };
        let path = checkpoint.save(&dir).map_err(|e| Error::Workflow {
            workflow: workflow.name.clone(),
            reason: format!("checkpoint: {e}"),
        })?;
        trace.entries.push(TraceEntry {
            node_id: node_id.to_string(),
            kind: "pause_for_approval".to_string(),
            outcome: "pause",
            branch: None,
            output: Value::Null,
            elapsed_ms: 0,
        });
        info!(
            target: "agentd::audit",
            event = "workflow.paused",
            node = %node_id,
            run_id = %ctx.execution_id,
            checkpoint = %path.display(),
        );
        Ok(ExecutionOutcome::Paused {
            run_id: ctx.execution_id.clone(),
            last_node: Some(node_id.to_string()),
            reason,
        })
    }

    /// Run a `call` node's child workflow as a sub-DAG on this engine
    /// (sharing its registry → policy, tools, and metrics). The child
    /// inherits the parent's remaining deadline; its `Completed` value
    /// becomes `{result: …}`, a failure/timeout routes the `error`
    /// branch. Depth-bounded to stop mutual recursion.
    fn run_call(
        &self,
        node: &crate::workflow::Node,
        ctx: &mut ExecutionContext,
        depth: u32,
    ) -> Result<NodeOutcome> {
        let crate::workflow::NodeKind::Call {
            workflow: path,
            input_from,
            start,
        } = &node.kind
        else {
            return Err(Error::Workflow {
                workflow: ctx.workflow_id.clone(),
                reason: format!("node `{}` dispatched as call but is not one", node.id),
            });
        };
        if depth + 1 > MAX_CALL_DEPTH {
            return Err(Error::Workflow {
                workflow: ctx.workflow_id.clone(),
                reason: format!("call depth exceeded {MAX_CALL_DEPTH} at node `{}`", node.id),
            });
        }

        // Load + validate the child like any workflow.
        let src = std::fs::read_to_string(path).map_err(|e| Error::Workflow {
            workflow: ctx.workflow_id.clone(),
            reason: format!("call `{}`: read child workflow {path}: {e}", node.id),
        })?;
        let child = WorkflowDoc::from_toml(&src).map_err(|e| Error::Workflow {
            workflow: ctx.workflow_id.clone(),
            reason: format!("call `{}`: parse child {path}: {e}", node.id),
        })?;
        let report = crate::workflow::validate(&child);
        if !report.ok() {
            return Err(Error::Workflow {
                workflow: child.name.clone(),
                reason: format!("call `{}`: child {path} failed validation", node.id),
            });
        }

        // Resolve the child's input + start node.
        let input = match input_from {
            Some(p) => ctx.resolve_path(p).cloned().unwrap_or(Value::Null),
            None => ctx.trigger.input.clone(),
        };
        let start_name = start
            .clone()
            .or_else(|| child.start_nodes.first().map(|s| s.name.clone()))
            .ok_or_else(|| Error::Workflow {
                workflow: child.name.clone(),
                reason: "child workflow declares no start nodes".to_string(),
            })?;
        let start_node = child
            .start_node(&start_name)
            .ok_or_else(|| Error::Workflow {
                workflow: child.name.clone(),
                reason: format!("child start node `{start_name}` not found"),
            })?;
        let entry = resolve_entry(&child, start_node)?.to_string();

        // The child shares the parent's *remaining* deadline.
        let remaining = ctx.deadline.saturating_duration_since(Instant::now());
        let exec_id = next_execution_id();
        let child_ctx = ExecutionContext::new(
            exec_id.clone(),
            child.name.clone(),
            &start_name,
            TriggerMeta::manual(input),
            &RunOptions {
                timeout: remaining,
                dry_run: ctx.dry_run,
            },
        );
        let child_trace = ExecutionTrace::new(exec_id);

        let (outcome, _child_trace) =
            self.walk_loop(&child, child_ctx, entry, child_trace, depth + 1)?;
        match outcome {
            ExecutionOutcome::Completed { final_value, .. } => Ok(NodeOutcome::Continue {
                value: serde_json::json!({ "result": final_value }),
                branch: None,
            }),
            ExecutionOutcome::Failed { reason, .. } => Ok(NodeOutcome::Continue {
                value: serde_json::json!({ "error": reason }),
                branch: Some("error".to_string()),
            }),
            ExecutionOutcome::TimedOut { .. } => Ok(NodeOutcome::Continue {
                value: serde_json::json!({ "error": "sub-workflow timed out" }),
                branch: Some("error".to_string()),
            }),
            ExecutionOutcome::Paused { .. } => Err(Error::Workflow {
                workflow: child.name.clone(),
                reason: "sub-workflow paused; nested pause_for_approval is not supported"
                    .to_string(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Retry wrapping
// ---------------------------------------------------------------------------

/// Dispatch a node through the registry, honouring the node's
/// optional [`RetryPolicy`]. Linear backoff: attempt N waits
/// `backoff_ms * N` by default; when `jitter > 0` the sleep is
/// randomised by `[1 - j, 1 + j]` to smooth thundering-herd.
/// Non-retryable errors short-circuit the loop.
fn dispatch_with_retry(
    registry: &HandlerRegistry,
    node: &crate::workflow::Node,
    ctx: &mut ExecutionContext,
) -> Result<NodeOutcome> {
    let Some(policy) = node.retry.clone() else {
        return registry.dispatch(node, ctx);
    };
    let max_attempts = policy.max_attempts.max(1);

    let mut attempt: u32 = 1;
    loop {
        match registry.dispatch(node, ctx) {
            Ok(outcome) => return Ok(outcome),
            Err(e) if attempt < max_attempts && is_retryable(&e, policy.on) => {
                // Jitter source: a pseudo-random u64 derived from
                // system-clock nanos XOR'd with an attempt counter.
                // Not cryptographic — the goal is decorrelation
                // across a fleet of agents retrying the same
                // upstream, not unpredictability.
                let rng_bits = jitter_bits(attempt);
                let wait = policy.backoff_for(attempt, rng_bits);
                tracing::warn!(
                    target: "agentd::audit",
                    event = "node.retry",
                    node_id = %node.id,
                    attempt,
                    max_attempts,
                    backoff_ms = policy.backoff_ms,
                    jitter = policy.clamped_jitter() as f64,
                    wait_ms = wait.as_millis() as u64,
                    reason = %e,
                );
                // Honour the engine deadline while backing off.
                let target = std::time::Instant::now() + wait;
                if target > ctx.deadline {
                    return Err(Error::Timeout(wait));
                }
                std::thread::sleep(wait);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Cheap, non-cryptographic jitter source. XOR a monotonic clock
/// reading with the attempt counter so siblings retrying at the
/// same moment still diverge. Good enough for herd smoothing.
fn jitter_bits(attempt: u32) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    now ^ (attempt as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn is_retryable(err: &Error, policy: crate::workflow::model::RetryOn) -> bool {
    use crate::workflow::model::RetryOn;
    match policy {
        RetryOn::Any => true,
        RetryOn::Transient => matches!(
            err,
            Error::Tool { .. } | Error::Intelligence(_) | Error::Mcp(_)
        ),
    }
}

// ---------------------------------------------------------------------------
// Entry resolution + edge picking
// ---------------------------------------------------------------------------

/// Resolve a start node's entry id. If `entry_node` is explicit, use
/// it. Otherwise fall back to the single node in the workflow with
/// zero incoming edges; error if that's ambiguous. Matches the
/// validator's `resolve_entry` helper (kept intentionally parallel so
/// the two stay in sync).
fn resolve_entry<'a>(
    workflow: &'a WorkflowDoc,
    start: &'a crate::workflow::StartNode,
) -> Result<&'a str> {
    if let Some(entry) = &start.entry_node {
        if workflow.node(entry).is_none() {
            return Err(Error::Workflow {
                workflow: workflow.name.clone(),
                reason: format!(
                    "start node `{}` references unknown entry node `{}`",
                    start.name, entry
                ),
            });
        }
        return Ok(entry.as_str());
    }

    // Find root candidates (zero in-degree).
    let mut roots: Vec<&str> = workflow.nodes.iter().map(|n| n.id.as_str()).collect();
    let pointed_at: std::collections::HashSet<&str> =
        workflow.edges.iter().map(|e| e.to.as_str()).collect();
    roots.retain(|id| !pointed_at.contains(id));

    match roots.len() {
        1 => Ok(roots[0]),
        _ => Err(Error::Workflow {
            workflow: workflow.name.clone(),
            reason: format!(
                "start node `{}` has no `entry_node` and the workflow has {} root nodes; \
                 specify `entry_node` explicitly",
                start.name,
                roots.len()
            ),
        }),
    }
}

/// Choose the next node id to visit given the current node and the
/// branch label the handler produced.
///
/// Rules:
/// - `branch = Some(label)` → the edge where `when == Some(label)`.
/// - `branch = None` → the single edge where `when.is_none()`.
/// - Multiple matches for either case → workflow error.
/// - Zero matches → `Ok(None)` (dead-end; engine ends run).
fn pick_next(
    workflow: &WorkflowDoc,
    current: &str,
    branch: Option<&str>,
    loop_counts: &mut std::collections::HashMap<usize, u32>,
) -> Result<Option<String>> {
    let matches: Vec<(usize, &Edge)> = workflow
        .edges
        .iter()
        .enumerate()
        .filter(|(_, e)| e.from == current)
        .filter(|(_, e)| match (branch, e.when.as_deref()) {
            (Some(label), Some(edge_label)) => label == edge_label,
            (None, None) => true,
            _ => false,
        })
        // A loop edge whose budget is spent is no longer a candidate —
        // the loop is forced to exit (dead-end, or another edge).
        .filter(|(idx, e)| match e.max_iterations {
            Some(max) => loop_counts.get(idx).copied().unwrap_or(0) < max,
            None => true,
        })
        .collect();

    match matches.len() {
        0 => Ok(None),
        1 => {
            let (idx, edge) = matches[0];
            if edge.max_iterations.is_some() {
                *loop_counts.entry(idx).or_insert(0) += 1;
            }
            Ok(Some(edge.to.clone()))
        }
        _ => Err(Error::Workflow {
            workflow: workflow.name.clone(),
            reason: format!(
                "node `{current}` has {} matching out-edges for branch `{:?}`; \
                 workflow graphs must select exactly one",
                matches.len(),
                branch
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::RunOptions;
    use crate::engine::handler::{HandlerRegistry, NodeHandler, StubHandler};
    use crate::workflow::model::*;
    use serde_json::json;
    use std::time::Duration;

    fn n(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.into(),
            retry: None,
            kind,
        }
    }

    fn e(from: &str, to: &str, when: Option<&str>) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            when: when.map(Into::into),
            max_iterations: None,
        }
    }

    fn loop_edge(from: &str, to: &str, when: Option<&str>, max: u32) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            when: when.map(Into::into),
            max_iterations: Some(max),
        }
    }

    fn start(name: &str, entry: &str) -> StartNode {
        StartNode {
            name: name.into(),
            source: StartSource::Manual,
            entry_node: Some(entry.into()),
        }
    }

    fn engine_with_stub() -> Engine {
        let mut r = HandlerRegistry::with_builtin_controls();
        r.set_fallback(Box::new(StubHandler));
        Engine::new(r)
    }

    #[test]
    fn linear_workflow_terminates() {
        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![start("main", "a")],
            nodes: vec![n("a", NodeKind::Merge), n("b", NodeKind::Terminate)],
            edges: vec![e("a", "b", None)],
            ..Default::default()
        };
        let out = engine_with_stub()
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap();
        assert!(matches!(out, ExecutionOutcome::Completed { .. }));
    }

    #[test]
    fn pause_then_resume_completes() {
        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![start("main", "a")],
            nodes: vec![
                n("a", NodeKind::Merge),
                n(
                    "gate",
                    NodeKind::PauseForApproval {
                        reason: Some("ok?".into()),
                    },
                ),
                n("done", NodeKind::Terminate),
            ],
            edges: vec![e("a", "gate", None), e("gate", "done", None)],
            ..Default::default()
        };
        let dir = tempfile::TempDir::new().unwrap();
        let mut r = HandlerRegistry::with_builtin_controls();
        r.set_fallback(Box::new(StubHandler));
        let engine = Engine::new(r).with_state_dir(Some(dir.path().to_path_buf()));

        let (out, trace) = engine
            .run_with_trace(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap();
        let run_id = match &out {
            ExecutionOutcome::Paused {
                run_id, last_node, ..
            } => {
                assert_eq!(last_node.as_deref(), Some("gate"));
                run_id.clone()
            }
            other => panic!("expected paused, got {other:?}"),
        };
        assert_eq!(trace.node_ids(), vec!["a", "gate"]);

        let cp = crate::engine::checkpoint::Checkpoint::load(dir.path(), &run_id).unwrap();
        assert_eq!(cp.resume_node.as_deref(), Some("done"));

        let (out2, trace2) = engine.resume(&wf, cp, RunOptions::default()).unwrap();
        assert!(matches!(out2, ExecutionOutcome::Completed { .. }));
        assert_eq!(trace2.node_ids(), vec!["done"]);
        // The checkpoint retires once the resumed run reaches a terminal.
        assert!(crate::engine::checkpoint::Checkpoint::load(dir.path(), &run_id).is_err());
    }

    #[test]
    fn pause_without_state_dir_is_an_error() {
        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![start("main", "gate")],
            nodes: vec![
                n("gate", NodeKind::PauseForApproval { reason: None }),
                n("done", NodeKind::Terminate),
            ],
            edges: vec![e("gate", "done", None)],
            ..Default::default()
        };
        let err = engine_with_stub()
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("state directory"), "{err}");
    }

    #[test]
    #[cfg(feature = "tools-data")] // the child uses template_render
    fn call_runs_child_and_returns_result() {
        let dir = tempfile::TempDir::new().unwrap();
        let child_path = dir.path().join("child.toml");
        std::fs::write(
            &child_path,
            r#"
            name = "child"
            [[start_nodes]]
            name = "main"
            source = "manual"
            entry_node = "render"
            [[nodes]]
            id = "render"
            type = "template_render"
            template = "hi {{who}}"
            input_from = "trigger"
            "#,
        )
        .unwrap();

        let wf = WorkflowDoc {
            name: "parent".into(),
            start_nodes: vec![start("main", "greet")],
            nodes: vec![
                n(
                    "greet",
                    NodeKind::Call {
                        workflow: child_path.to_string_lossy().into_owned(),
                        input_from: Some("trigger".into()),
                        start: None,
                    },
                ),
                n("done", NodeKind::Terminate),
            ],
            edges: vec![e("greet", "done", None)],
            ..Default::default()
        };
        let engine = {
            let mut r = HandlerRegistry::with_builtin_controls();
            crate::tools::register_default_tools(
                &mut r,
                crate::tools::policy::allow_all(),
                crate::budget::unbounded(),
            );
            r.set_fallback(Box::new(StubHandler));
            Engine::new(r)
        };
        let (out, trace) = engine
            .run_with_trace(
                &wf,
                "main",
                TriggerMeta::manual(json!({"who": "there"})),
                RunOptions::default(),
            )
            .unwrap();
        assert!(matches!(out, ExecutionOutcome::Completed { .. }));
        assert_eq!(trace.node_ids(), vec!["greet", "done"]);
        // The call node's output carries the child's result.
        let greet = &trace.entries[0];
        assert_eq!(greet.output["result"]["rendered"], "hi there");
    }

    #[test]
    fn call_depth_is_bounded() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("loop.toml");
        // A workflow that calls itself — bounded by MAX_CALL_DEPTH.
        std::fs::write(
            &path,
            format!(
                r#"
                name = "looper"
                [[start_nodes]]
                name = "main"
                source = "manual"
                entry_node = "again"
                [[nodes]]
                id = "again"
                type = "call"
                workflow = "{}"
                "#,
                // Forward slashes: backslashes from a Windows temp path
                // would be TOML escapes. fs accepts `/` on Windows too.
                path.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        let doc = WorkflowDoc::from_toml(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let err = engine_with_stub()
            .run(
                &doc,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("call depth"), "{err}");
    }

    #[test]
    fn bounded_loop_edge_caps_iterations() {
        // gen → eval; eval always routes "retry" back to gen via a loop
        // edge with max_iterations = 3. The engine follows it 3 times,
        // then the budget is spent and the run dead-ends at eval.
        let wf = WorkflowDoc {
            name: "optimizer".into(),
            start_nodes: vec![start("main", "gen")],
            nodes: vec![
                n("gen", NodeKind::Merge),
                n(
                    "eval",
                    NodeKind::Switch {
                        expr: "trigger.verdict".into(),
                    },
                ),
            ],
            edges: vec![
                e("gen", "eval", None),
                loop_edge("eval", "gen", Some("retry"), 3),
            ],
            ..Default::default()
        };
        let (out, trace) = engine_with_stub()
            .run_with_trace(
                &wf,
                "main",
                TriggerMeta::manual(json!({ "verdict": "retry" })),
                RunOptions::default(),
            )
            .unwrap();
        assert!(matches!(out, ExecutionOutcome::Completed { .. }));
        // eval is visited max_iterations + 1 = 4 times (the last time the
        // loop edge is exhausted, so the run stops there).
        let evals = trace.node_ids().iter().filter(|id| *id == "eval").count();
        assert_eq!(evals, 4, "loop ran {evals} times; expected 4");
    }

    #[test]
    fn switch_picks_matching_branch() {
        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![start("main", "set")],
            nodes: vec![
                n(
                    "set",
                    NodeKind::ReadFile {
                        path_from: "ignored".into(),
                    },
                ),
                n(
                    "sw",
                    NodeKind::Switch {
                        expr: "set.stub".into(),
                    },
                ),
                n("comment", NodeKind::Terminate),
                n("ignore", NodeKind::Fail { reason: None }),
            ],
            edges: vec![
                e("set", "sw", None),
                e("sw", "comment", Some("read_file")),
                e("sw", "ignore", Some("other")),
            ],
            ..Default::default()
        };

        // StubHandler writes `{"stub": "<kind>"}` for every non-control
        // node — so `set.stub` resolves to the kind name "read_file",
        // which matches the `when = "read_file"` branch.
        let out = engine_with_stub()
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap();
        assert!(
            matches!(
                &out,
                ExecutionOutcome::Completed { last_node: Some(id), .. } if id == "comment"
            ),
            "got: {out:?}"
        );
    }

    #[test]
    fn fail_node_returns_failed_outcome() {
        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![start("main", "boom")],
            nodes: vec![n(
                "boom",
                NodeKind::Fail {
                    reason: Some("kaboom".into()),
                },
            )],
            ..Default::default()
        };
        let out = engine_with_stub()
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap();
        assert_eq!(
            out,
            ExecutionOutcome::Failed {
                reason: "kaboom".into(),
                last_node: Some("boom".into()),
            }
        );
    }

    #[test]
    fn dead_end_without_outgoing_edge_completes() {
        // Merge node with no out-edge — engine should treat it as a
        // successful end.
        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![start("main", "a")],
            nodes: vec![n("a", NodeKind::Merge)],
            ..Default::default()
        };
        let out = engine_with_stub()
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap();
        assert!(matches!(out, ExecutionOutcome::Completed { .. }));
    }

    #[test]
    fn unknown_start_node_errors() {
        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![],
            nodes: vec![n("a", NodeKind::Terminate)],
            ..Default::default()
        };
        let err = engine_with_stub()
            .run(
                &wf,
                "nope",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("unknown start node"));
    }

    #[test]
    fn ambiguous_unconditional_edge_errors() {
        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![start("main", "a")],
            nodes: vec![
                n("a", NodeKind::Merge),
                n("b", NodeKind::Terminate),
                n("c", NodeKind::Terminate),
            ],
            edges: vec![e("a", "b", None), e("a", "c", None)],
            ..Default::default()
        };
        let err = engine_with_stub()
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("matching out-edges"));
    }

    #[test]
    fn timeout_is_reported() {
        // Handler that stalls long enough to trip a tiny deadline.
        struct SlowHandler;
        impl NodeHandler for SlowHandler {
            fn handle(&self, _node: &Node, _ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
                std::thread::sleep(Duration::from_millis(15));
                Ok(NodeOutcome::ok_null())
            }
        }

        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![start("main", "a")],
            nodes: vec![
                n("a", NodeKind::Merge),
                n("b", NodeKind::Merge),
                n("c", NodeKind::Terminate),
            ],
            edges: vec![e("a", "b", None), e("b", "c", None)],
            ..Default::default()
        };

        let mut registry = HandlerRegistry::new();
        registry.register("merge", Box::new(SlowHandler));
        registry.register(
            "terminate",
            Box::new(super::super::handler::TerminateHandler),
        );
        let engine = Engine::new(registry);

        let out = engine
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions {
                    timeout: Duration::from_millis(5),
                    dry_run: false,
                },
            )
            .unwrap();
        assert!(matches!(out, ExecutionOutcome::TimedOut { .. }));
    }

    #[test]
    fn retry_recovers_from_transient_errors() {
        use crate::workflow::model::{RetryOn, RetryPolicy};

        // Handler that fails its first 2 calls, then succeeds.
        struct Flaky {
            attempts: std::sync::Mutex<u32>,
        }
        impl NodeHandler for Flaky {
            fn handle(&self, _node: &Node, _ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
                let mut n = self.attempts.lock().unwrap();
                *n += 1;
                if *n < 3 {
                    Err(Error::Tool {
                        tool: "flaky".into(),
                        reason: "transient".into(),
                    })
                } else {
                    Ok(NodeOutcome::ok_null())
                }
            }
        }

        let wf = WorkflowDoc {
            name: "retry".into(),
            start_nodes: vec![start("main", "a")],
            nodes: vec![
                Node {
                    id: "a".into(),
                    retry: Some(RetryPolicy {
                        max_attempts: 3,
                        backoff_ms: 1, // virtually instant for test
                        on: RetryOn::Transient,
                        jitter: 0.0,
                    }),
                    kind: NodeKind::Merge, // handler is the Flaky stub; kind only used for name
                },
                n("b", NodeKind::Terminate),
            ],
            edges: vec![e("a", "b", None)],
            ..Default::default()
        };

        let mut reg = HandlerRegistry::new();
        reg.register(
            "merge",
            Box::new(Flaky {
                attempts: std::sync::Mutex::new(0),
            }),
        );
        reg.register(
            "terminate",
            Box::new(super::super::handler::TerminateHandler),
        );
        let engine = Engine::new(reg);
        let out = engine
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap();
        assert!(matches!(out, ExecutionOutcome::Completed { .. }));
    }

    #[test]
    fn retry_gives_up_after_max_attempts() {
        use crate::workflow::model::{RetryOn, RetryPolicy};

        struct AlwaysFail;
        impl NodeHandler for AlwaysFail {
            fn handle(&self, _node: &Node, _ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
                Err(Error::Tool {
                    tool: "x".into(),
                    reason: "nope".into(),
                })
            }
        }

        let wf = WorkflowDoc {
            name: "fail".into(),
            start_nodes: vec![start("main", "a")],
            nodes: vec![Node {
                id: "a".into(),
                retry: Some(RetryPolicy {
                    max_attempts: 2,
                    backoff_ms: 1,
                    on: RetryOn::Any,
                    jitter: 0.0,
                }),
                kind: NodeKind::Merge,
            }],
            ..Default::default()
        };

        let mut reg = HandlerRegistry::new();
        reg.register("merge", Box::new(AlwaysFail));
        let engine = Engine::new(reg);
        let err = engine
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("nope"));
    }

    #[test]
    fn retry_policy_filter_declines_non_matching_errors() {
        use crate::workflow::model::{RetryOn, RetryPolicy};

        struct PolicyDeny;
        impl NodeHandler for PolicyDeny {
            fn handle(&self, _node: &Node, _ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
                Err(Error::Policy("denied".into()))
            }
        }

        let wf = WorkflowDoc {
            name: "pd".into(),
            start_nodes: vec![start("main", "a")],
            nodes: vec![Node {
                id: "a".into(),
                retry: Some(RetryPolicy {
                    max_attempts: 5,
                    backoff_ms: 1,
                    on: RetryOn::Transient, // policy errors are not transient
                    jitter: 0.0,
                }),
                kind: NodeKind::Merge,
            }],
            ..Default::default()
        };

        let mut reg = HandlerRegistry::new();
        reg.register("merge", Box::new(PolicyDeny));
        let engine = Engine::new(reg);
        let err = engine
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap_err();
        // Policy denial short-circuits the retry loop — a transient
        // policy would retry 5 times; this must return after one.
        assert!(matches!(err, Error::Policy(_)));
    }

    #[test]
    fn condition_true_false_routes() {
        // Condition reads trigger.flag; routes to t_done or f_done.
        let wf = WorkflowDoc {
            name: "wf".into(),
            start_nodes: vec![start("main", "c")],
            nodes: vec![
                n(
                    "c",
                    NodeKind::Condition {
                        expr: "trigger.flag".into(),
                    },
                ),
                n("t_done", NodeKind::Terminate),
                n(
                    "f_done",
                    NodeKind::Fail {
                        reason: Some("flag was false".into()),
                    },
                ),
            ],
            edges: vec![
                e("c", "t_done", Some("true")),
                e("c", "f_done", Some("false")),
            ],
            ..Default::default()
        };

        let engine = Engine::new(HandlerRegistry::with_builtin_controls());

        // true branch
        let out_true = engine
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({ "flag": true })),
                RunOptions::default(),
            )
            .unwrap();
        assert!(matches!(out_true, ExecutionOutcome::Completed { .. }));

        // false branch
        let out_false = engine
            .run(
                &wf,
                "main",
                TriggerMeta::manual(json!({ "flag": false })),
                RunOptions::default(),
            )
            .unwrap();
        assert_eq!(
            out_false,
            ExecutionOutcome::Failed {
                reason: "flag was false".into(),
                last_node: Some("f_done".into()),
            }
        );
    }
}
