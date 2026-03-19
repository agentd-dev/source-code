//! Outcomes produced by node handlers and by the overall engine run.
//!
//! A handler returns a [`NodeOutcome`] saying what should happen
//! next. The engine turns a sequence of [`NodeOutcome`]s into an
//! [`ExecutionOutcome`] once the run ends (terminate, fail, timeout,
//! or the DAG dead-ends).

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

/// What a node execution tells the engine to do next.
///
/// Handlers never pick successor edges themselves; that is the
/// engine's job, driven by `branch`.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeOutcome {
    /// Normal completion. The produced `value` is stored under the
    /// node's id in `ExecutionContext::node_outputs`. `branch` selects
    /// which out-edge the engine follows:
    ///
    /// - `None` → the unique unconditional out-edge (where
    ///   `edge.when.is_none()`).
    /// - `Some(label)` → the out-edge whose `when == Some(label)`.
    Continue {
        value: Value,
        branch: Option<String>,
    },
    /// End the workflow successfully. `value` becomes the workflow's
    /// final result.
    Terminate { value: Value },
    /// End the workflow with a declared failure. `reason` is the
    /// operator-facing explanation.
    Fail { reason: String },
}

impl NodeOutcome {
    /// Convenience constructor for an unconditional Continue with a
    /// null value. Used by every handler that has no interesting
    /// output to produce (e.g. Merge).
    pub fn ok_null() -> Self {
        NodeOutcome::Continue {
            value: Value::Null,
            branch: None,
        }
    }

    /// Convenience constructor for a branched Continue (used by
    /// Switch / Condition handlers).
    pub fn branch(label: impl Into<String>, value: Value) -> Self {
        NodeOutcome::Continue {
            value,
            branch: Some(label.into()),
        }
    }
}

/// Why the engine stopped. Returned from `Engine::run`.
///
/// Serialised JSON shape uses a `status` discriminator so CLI
/// consumers (scripts, tests, dashboards) can branch cleanly:
///
/// ```json
/// { "status": "completed", "final_value": …, "last_node": "…" }
/// { "status": "failed",    "reason": "…",   "last_node": "…" }
/// { "status": "timed_out", "elapsed_ms": 123, "last_node": "…" }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExecutionOutcome {
    /// Run reached a Terminate node or a dead-end successor.
    Completed {
        final_value: Value,
        last_node: Option<String>,
    },
    /// Run reached a Fail node or a handler explicitly rejected work.
    Failed {
        reason: String,
        last_node: Option<String>,
    },
    /// Deadline expired before the next node completed.
    TimedOut {
        #[serde(serialize_with = "serialize_duration_ms", rename = "elapsed_ms")]
        elapsed: Duration,
        last_node: Option<String>,
    },
}

fn serialize_duration_ms<S>(d: &Duration, s: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_u64(d.as_millis() as u64)
}

impl ExecutionOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, ExecutionOutcome::Completed { .. })
    }

    /// Short string label for log fields — `completed` / `failed`
    /// / `timed_out`. Used by trigger-side loops that want to emit
    /// a terse audit event without pattern-matching the variant.
    pub fn status_label(&self) -> &'static str {
        match self {
            ExecutionOutcome::Completed { .. } => "completed",
            ExecutionOutcome::Failed { .. } => "failed",
            ExecutionOutcome::TimedOut { .. } => "timed_out",
        }
    }
}
