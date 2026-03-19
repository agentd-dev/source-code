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

use std::time::Instant;

use serde_json::Value;

use crate::engine::context::{ExecutionContext, RunOptions, TriggerMeta};
use crate::engine::handler::HandlerRegistry;
use crate::engine::outcome::{ExecutionOutcome, NodeOutcome};
use crate::error::{Error, Result};
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
}

impl Engine {
    pub fn new(registry: HandlerRegistry) -> Self {
        Self { registry }
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

        // 3) Walk the DAG.
        let mut current_id = entry_id.to_string();
        let started_at = Instant::now();

        for _step in 0..MAX_STEPS {
            // Deadline check.
            if Instant::now() >= ctx.deadline {
                let elapsed = Instant::now().duration_since(started_at);
                return Ok(ExecutionOutcome::TimedOut {
                    elapsed,
                    last_node: ctx.current_node_id,
                });
            }

            // Look up the node.
            let node = workflow.node(&current_id).ok_or_else(|| Error::Workflow {
                workflow: workflow.name.clone(),
                reason: format!("node `{current_id}` referenced in traversal is not declared"),
            })?;
            ctx.current_node_id = Some(current_id.clone());

            let outcome = self.registry.dispatch(node, &mut ctx)?;

            match outcome {
                NodeOutcome::Terminate { value } => {
                    ctx.node_outputs.insert(current_id.clone(), value.clone());
                    return Ok(ExecutionOutcome::Completed {
                        final_value: value,
                        last_node: Some(current_id),
                    });
                }
                NodeOutcome::Fail { reason } => {
                    return Ok(ExecutionOutcome::Failed {
                        reason,
                        last_node: Some(current_id),
                    });
                }
                NodeOutcome::Continue { value, branch } => {
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
                            return Ok(ExecutionOutcome::Completed {
                                final_value,
                                last_node: Some(current_id),
                            });
                        }
                    }
                }
            }
        }

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
