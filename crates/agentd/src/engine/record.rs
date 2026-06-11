//! Run records — a structured, serialisable account of one workflow
//! run: its outcome (or the error that aborted it), the per-node trace
//! with each node's output and timing, the cost, and wall-clock.
//!
//! A record is the substrate a run inspector renders. `agentd --record
//! run.json` writes one; `agentd inspect run.json` renders it. The
//! renderer ([`render`]) works over a parsed [`Value`] rather than the
//! typed struct so it can read records written by any version (and
//! because `TraceEntry::outcome` is a `&'static str`, which does not
//! round-trip through `Deserialize`).

use serde::Serialize;
use serde_json::{Value, json};

use crate::engine::outcome::{ExecutionOutcome, ExecutionTrace};
use crate::observability::metrics::MetricsSnapshot;

/// A captured run. Serialise to JSON for `--record`; render with
/// [`render`].
#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    pub workflow: String,
    pub start_node: String,
    pub execution_id: String,
    /// `completed` | `failed` | `timed_out` | `errored`.
    pub status: String,
    pub last_node: Option<String>,
    /// Outcome-specific: the final value, or `{reason}` / `{elapsed_ms}`
    /// / `{error}`.
    pub detail: Value,
    pub wall_ms: u64,
    pub cost: MetricsSnapshot,
    pub trace: ExecutionTrace,
}

impl RunRecord {
    /// Build from a completed/failed/timed-out run.
    pub fn from_outcome(
        workflow: impl Into<String>,
        start_node: impl Into<String>,
        wall_ms: u64,
        cost: MetricsSnapshot,
        outcome: &ExecutionOutcome,
        trace: ExecutionTrace,
    ) -> Self {
        let (status, last_node, detail) = match outcome {
            ExecutionOutcome::Completed {
                final_value,
                last_node,
                http_response,
            } => {
                let detail = match http_response {
                    // Surface the declared reply in the record so
                    // `agentd inspect` shows what the caller was told.
                    Some(spec) => json!({
                        "final_value": final_value,
                        "http_response": spec,
                    }),
                    None => final_value.clone(),
                };
                ("completed", last_node.clone(), detail)
            }
            ExecutionOutcome::Failed { reason, last_node } => {
                ("failed", last_node.clone(), json!({ "reason": reason }))
            }
            ExecutionOutcome::TimedOut { elapsed, last_node } => (
                "timed_out",
                last_node.clone(),
                json!({ "elapsed_ms": elapsed.as_millis() as u64 }),
            ),
            ExecutionOutcome::Paused {
                run_id,
                last_node,
                reason,
            } => (
                "paused",
                last_node.clone(),
                json!({ "run_id": run_id, "reason": reason }),
            ),
        };
        Self {
            workflow: workflow.into(),
            start_node: start_node.into(),
            execution_id: trace.execution_id.clone(),
            status: status.to_string(),
            last_node,
            detail,
            wall_ms,
            cost,
            trace,
        }
    }

    /// Build from a run that aborted with a handler-level error (no
    /// outcome, partial trace discarded by the engine).
    pub fn errored(
        workflow: impl Into<String>,
        start_node: impl Into<String>,
        wall_ms: u64,
        cost: MetricsSnapshot,
        error: impl Into<String>,
    ) -> Self {
        Self {
            workflow: workflow.into(),
            start_node: start_node.into(),
            execution_id: String::new(),
            status: "errored".to_string(),
            last_node: None,
            detail: json!({ "error": error.into() }),
            wall_ms,
            cost,
            trace: ExecutionTrace::default(),
        }
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Render a run record (as parsed JSON) into a human-readable timeline.
pub fn render(v: &Value) -> String {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);

    let cost = v.get("cost").cloned().unwrap_or(Value::Null);
    let calls = cost.get("llm_calls").and_then(Value::as_u64).unwrap_or(0);
    let tokens = cost.get("llm_tokens").and_then(Value::as_u64).unwrap_or(0);
    let denials = cost
        .get("policy_denials")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut out = String::new();
    out.push_str(&format!(
        "run {}  workflow={}  status={}\n",
        nonempty(s("execution_id"), "?"),
        s("workflow"),
        s("status"),
    ));
    out.push_str(&format!(
        "  start={}  {} ms  {} llm call(s) / {} tokens  {} policy denial(s)\n",
        s("start_node"),
        n("wall_ms"),
        calls,
        tokens,
        denials,
    ));

    let entries = v
        .get("trace")
        .and_then(|t| t.get("entries"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if entries.is_empty() {
        out.push_str("  (no node trace captured)\n");
    } else {
        out.push_str("  path:\n");
        for (i, e) in entries.iter().enumerate() {
            let node = e.get("node_id").and_then(Value::as_str).unwrap_or("?");
            let kind = e.get("kind").and_then(Value::as_str).unwrap_or("?");
            let flavour = e.get("outcome").and_then(Value::as_str).unwrap_or("?");
            let branch = e
                .get("branch")
                .and_then(Value::as_str)
                .map(|b| format!(" →{b}"))
                .unwrap_or_default();
            let ms = e.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0);
            out.push_str(&format!(
                "    {:>2}. {} [{}] {}{}  {} ms\n",
                i + 1,
                node,
                kind,
                flavour,
                branch,
                ms,
            ));
            if let Some(output) = e.get("output")
                && !output.is_null()
            {
                out.push_str(&format!("        output: {}\n", truncate(output, 160)));
            }
        }
    }

    if let Some(detail) = v.get("detail")
        && !detail.is_null()
    {
        out.push_str(&format!("  outcome: {}\n", truncate(detail, 200)));
    }
    out
}

fn nonempty<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() { fallback } else { s }
}

/// Compact single-line JSON, truncated to `max` chars.
fn truncate(v: &Value, max: usize) -> String {
    let s = v.to_string();
    if s.chars().count() > max {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::outcome::TraceEntry;

    fn trace() -> ExecutionTrace {
        let mut t = ExecutionTrace::new("exec-42");
        t.entries.push(TraceEntry {
            node_id: "classify".into(),
            kind: "llm_infer".into(),
            outcome: "continue",
            branch: Some("alpha".into()),
            output: json!({"decision": "alpha"}),
            elapsed_ms: 12,
        });
        t.entries.push(TraceEntry {
            node_id: "done".into(),
            kind: "terminate".into(),
            outcome: "terminate",
            branch: None,
            output: Value::Null,
            elapsed_ms: 0,
        });
        t
    }

    #[test]
    fn record_from_completed_outcome() {
        let outcome = ExecutionOutcome::Completed {
            final_value: json!({"ok": true}),
            last_node: Some("done".into()),
            http_response: None,
        };
        let rec = RunRecord::from_outcome(
            "wf",
            "main",
            7,
            MetricsSnapshot::default(),
            &outcome,
            trace(),
        );
        assert_eq!(rec.status, "completed");
        assert_eq!(rec.execution_id, "exec-42");
        assert_eq!(rec.last_node.as_deref(), Some("done"));
    }

    #[test]
    fn render_shows_path_and_cost() {
        let outcome = ExecutionOutcome::Completed {
            final_value: json!(null),
            last_node: Some("done".into()),
            http_response: None,
        };
        let rec = RunRecord::from_outcome(
            "wf",
            "main",
            7,
            MetricsSnapshot::default(),
            &outcome,
            trace(),
        );
        let v: Value = serde_json::from_str(&rec.to_json_pretty()).unwrap();
        let text = render(&v);
        assert!(text.contains("workflow=wf"));
        assert!(text.contains("classify [llm_infer]"));
        assert!(text.contains("→alpha"));
        assert!(text.contains("output:"));
    }

    #[test]
    fn errored_record_has_no_trace() {
        let rec = RunRecord::errored("wf", "main", 3, MetricsSnapshot::default(), "boom");
        let v: Value = serde_json::from_str(&rec.to_json_pretty()).unwrap();
        let text = render(&v);
        assert!(text.contains("status=errored"));
        assert!(text.contains("no node trace captured"));
        assert!(text.contains("boom"));
    }
}
