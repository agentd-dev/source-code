//! Process-wide metrics counters (RFC §20.3).
//!
//! Plain `AtomicU64`s — no histograms, no labels. Engines update
//! counters inline; the HTTP server exposes [`MetricsSnapshot`] on
//! `GET /metrics` in Prometheus text-exposition format (§6.2).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Shared handle. Engines hold an `Arc<Metrics>` and update counters
/// inline; consumers can read a `MetricsSnapshot` whenever.
#[derive(Debug, Default)]
pub struct Metrics {
    workflow_starts: AtomicU64,
    workflow_completions: AtomicU64,
    workflow_failures: AtomicU64,
    workflow_timeouts: AtomicU64,
    workflow_errored: AtomicU64,
    node_executions: AtomicU64,
    node_failures: AtomicU64,
    policy_denials: AtomicU64,
    llm_calls: AtomicU64,
    llm_tokens: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn inc_workflow_started(&self) {
        self.workflow_starts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_workflow_completed(&self) {
        self.workflow_completions.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_workflow_failed(&self) {
        self.workflow_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_workflow_timed_out(&self) {
        self.workflow_timeouts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_workflow_errored(&self) {
        self.workflow_errored.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_node_executed(&self) {
        self.node_executions.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_node_failed(&self) {
        self.node_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_policy_denied(&self) {
        self.policy_denials.fetch_add(1, Ordering::Relaxed);
    }
    /// Record one LLM call and the tokens it consumed (RFC 0006 §5).
    pub fn add_llm(&self, tokens: u64) {
        self.llm_calls.fetch_add(1, Ordering::Relaxed);
        self.llm_tokens.fetch_add(tokens, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            workflow_starts: self.workflow_starts.load(Ordering::Relaxed),
            workflow_completions: self.workflow_completions.load(Ordering::Relaxed),
            workflow_failures: self.workflow_failures.load(Ordering::Relaxed),
            workflow_timeouts: self.workflow_timeouts.load(Ordering::Relaxed),
            workflow_errored: self.workflow_errored.load(Ordering::Relaxed),
            node_executions: self.node_executions.load(Ordering::Relaxed),
            node_failures: self.node_failures.load(Ordering::Relaxed),
            policy_denials: self.policy_denials.load(Ordering::Relaxed),
            llm_calls: self.llm_calls.load(Ordering::Relaxed),
            llm_tokens: self.llm_tokens.load(Ordering::Relaxed),
        }
    }
}

/// Frozen read of all counters at one instant.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub workflow_starts: u64,
    pub workflow_completions: u64,
    pub workflow_failures: u64,
    pub workflow_timeouts: u64,
    pub workflow_errored: u64,
    pub node_executions: u64,
    pub node_failures: u64,
    pub policy_denials: u64,
    pub llm_calls: u64,
    pub llm_tokens: u64,
}

impl MetricsSnapshot {
    /// Render this snapshot as Prometheus 0.0.4 text-exposition
    /// format. `workflow_name` lands as a label on every counter so
    /// scrapers can distinguish multi-agent setups without
    /// per-process suffixing.
    ///
    /// Format rules: one `# HELP` + `# TYPE` pair per metric, counter
    /// names carry the `_total` suffix per convention, label values
    /// are escaped for `\`, `"`, and `\n`. One trailing newline.
    pub fn to_prometheus(&self, workflow_name: &str) -> String {
        let label = escape_label(workflow_name);
        let mut out = String::with_capacity(2048);
        for m in PROM_METRICS {
            let value = (m.read)(self);
            out.push_str("# HELP ");
            out.push_str(m.name);
            out.push(' ');
            out.push_str(m.help);
            out.push('\n');
            out.push_str("# TYPE ");
            out.push_str(m.name);
            out.push_str(" counter\n");
            out.push_str(m.name);
            out.push_str("{workflow=\"");
            out.push_str(&label);
            out.push_str("\"} ");
            out.push_str(&value.to_string());
            out.push('\n');
        }
        out.push_str("# HELP agentd_build_info Build info; always 1.\n");
        out.push_str("# TYPE agentd_build_info gauge\n");
        out.push_str("agentd_build_info{workflow=\"");
        out.push_str(&label);
        out.push_str("\",version=\"");
        out.push_str(env!("CARGO_PKG_VERSION"));
        out.push_str("\"} 1\n");
        out
    }
}

struct PromMetric {
    name: &'static str,
    help: &'static str,
    read: fn(&MetricsSnapshot) -> u64,
}

const PROM_METRICS: &[PromMetric] = &[
    PromMetric {
        name: "agentd_workflow_starts_total",
        help: "Workflow executions started.",
        read: |s| s.workflow_starts,
    },
    PromMetric {
        name: "agentd_workflow_completions_total",
        help: "Workflow executions that reached a terminal success state.",
        read: |s| s.workflow_completions,
    },
    PromMetric {
        name: "agentd_workflow_failures_total",
        help: "Workflow executions that ended in a Failed outcome.",
        read: |s| s.workflow_failures,
    },
    PromMetric {
        name: "agentd_workflow_timeouts_total",
        help: "Workflow executions terminated by the per-run deadline.",
        read: |s| s.workflow_timeouts,
    },
    PromMetric {
        name: "agentd_workflow_errors_total",
        help: "Workflow executions that aborted with an engine error.",
        read: |s| s.workflow_errored,
    },
    PromMetric {
        name: "agentd_node_executions_total",
        help: "Node dispatch attempts (includes retries).",
        read: |s| s.node_executions,
    },
    PromMetric {
        name: "agentd_node_failures_total",
        help: "Node dispatches that returned a non-success status.",
        read: |s| s.node_failures,
    },
    PromMetric {
        name: "agentd_policy_denials_total",
        help: "Tool invocations refused by the manifest policy.",
        read: |s| s.policy_denials,
    },
    PromMetric {
        name: "agentd_llm_calls_total",
        help: "Intelligence backend calls (llm_infer + agent_loop turns).",
        read: |s| s.llm_calls,
    },
    PromMetric {
        name: "agentd_llm_tokens_total",
        help: "Cumulative LLM tokens consumed (prompt + completion).",
        read: |s| s.llm_tokens,
    },
];

fn escape_label(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            '\n' => out.push_str(r"\n"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero() {
        let m = Metrics::new();
        let s = m.snapshot();
        assert_eq!(s.workflow_starts, 0);
        assert_eq!(s.node_executions, 0);
    }

    #[test]
    fn increments_show_up_in_snapshot() {
        let m = Metrics::new();
        m.inc_workflow_started();
        m.inc_node_executed();
        m.inc_node_executed();
        m.inc_policy_denied();
        let s = m.snapshot();
        assert_eq!(s.workflow_starts, 1);
        assert_eq!(s.node_executions, 2);
        assert_eq!(s.policy_denials, 1);
    }

    #[test]
    fn snapshot_is_serde_serializable() {
        let m = Metrics::new();
        m.inc_workflow_completed();
        let s = serde_json::to_string(&m.snapshot()).unwrap();
        assert!(s.contains("\"workflow_completions\":1"));
    }

    #[test]
    fn prometheus_text_is_well_formed() {
        let m = Metrics::new();
        m.inc_workflow_started();
        m.inc_workflow_completed();
        m.inc_node_executed();
        m.inc_node_executed();
        m.inc_policy_denied();
        let text = m.snapshot().to_prometheus("hello-world");

        for line in text.lines() {
            assert!(
                line.is_empty()
                    || line.starts_with("# HELP ")
                    || line.starts_with("# TYPE ")
                    || line.starts_with("agentd_"),
                "unexpected line: {line}"
            );
        }
        assert!(text.contains("# TYPE agentd_workflow_starts_total counter\n"));
        assert!(text.contains("agentd_workflow_starts_total{workflow=\"hello-world\"} 1\n"));
        assert!(text.contains("agentd_node_executions_total{workflow=\"hello-world\"} 2\n"));
        assert!(text.contains("agentd_policy_denials_total{workflow=\"hello-world\"} 1\n"));
        assert!(text.contains("# TYPE agentd_build_info gauge\n"));
        assert!(text.contains("version=\""));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn prometheus_escapes_label_specials() {
        let snap = Metrics::new().snapshot();
        let text = snap.to_prometheus(r#"risky"\name"#);
        assert!(text.contains(r#"workflow="risky\"\\name""#));
    }

    #[test]
    fn prometheus_emits_every_declared_counter() {
        let text = Metrics::new().snapshot().to_prometheus("w");
        for m in PROM_METRICS {
            assert!(text.contains(m.name), "missing counter: {}", m.name);
        }
    }

    #[test]
    fn metrics_is_shareable_across_threads() {
        let m = Metrics::new();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let m = m.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        m.inc_node_executed();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(m.snapshot().node_executions, 4_000);
    }
}
