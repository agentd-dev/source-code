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

/// Monotonically-incrementing execution id counter. Scoped to the
/// process; each engine instance shares it via a process-wide atomic.
static EXEC_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_execution_id() -> String {
    let n = EXEC_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("exec-{n:08x}")
}

pub struct Engine {
    pub registry: HandlerRegistry,
    pub metrics: Arc<Metrics>,
}

impl Engine {
    pub fn new(registry: HandlerRegistry) -> Self {
        Self {
            registry,
            metrics: Metrics::new(),
        }
    }

    /// Construct an engine that shares metrics with other engines —
    /// useful for serve mode where a single `Metrics` aggregates
    /// counters across every request.
    pub fn with_metrics(registry: HandlerRegistry, metrics: Arc<Metrics>) -> Self {
        Self { registry, metrics }
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
        let mut ctx = ExecutionContext::new(
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

        // 3) Walk the DAG.
        let mut current_id = entry_id.to_string();
        let started_at = Instant::now();

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

            // Per-node span. Everything the handler emits nests.
            let node_span = info_span!(
                "node.execute",
                node_id = %current_id,
                kind = %node.kind.name(),
            );
            let node_enter = node_span.enter();
            let node_started = Instant::now();
            self.metrics.inc_node_executed();

            let dispatch = self.registry.dispatch(node, &mut ctx);
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
                    });
                    ctx.node_outputs.insert(current_id.clone(), value);
                    let next_id = pick_next(workflow, &current_id, branch.as_deref())?;
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
) -> Result<Option<String>> {
    let matches: Vec<&Edge> = workflow
        .edges
        .iter()
        .filter(|e| e.from == current)
        .filter(|e| match (branch, e.when.as_deref()) {
            (Some(label), Some(edge_label)) => label == edge_label,
            (None, None) => true,
            _ => false,
        })
        .collect();

    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0].to.clone())),
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
            kind,
        }
    }

    fn e(from: &str, to: &str, when: Option<&str>) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            when: when.map(Into::into),
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
