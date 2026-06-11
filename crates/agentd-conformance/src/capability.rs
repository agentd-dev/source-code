//! The capability matrix — the canonical list of runtime capabilities
//! the corpus tracks coverage over.
//!
//! Goal tracking, concretely: every scenario tags the capabilities it
//! exercises; coverage is the fraction of the matrix touched by at
//! least one *passing* scenario. Uncovered capabilities are the
//! suite's backlog — the things the runtime claims to do that the
//! corpus does not yet prove. Tags outside the matrix are flagged so a
//! typo can't silently inflate coverage.
//!
//! Node-kind capability ids are exactly the node `type` strings, so a
//! scenario's tags read like the nodes it uses.

use std::collections::BTreeSet;

use crate::ScenarioReport;

/// One tracked capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    pub id: &'static str,
    pub group: &'static str,
    pub description: &'static str,
}

const fn c(id: &'static str, group: &'static str, description: &'static str) -> Capability {
    Capability {
        id,
        group,
        description,
    }
}

/// The canonical capability matrix. Adding a runtime capability here
/// without a scenario that covers it lowers coverage — that is the
/// point: the gap is visible.
static MATRIX: &[Capability] = &[
    // Control plane.
    c("condition", "control", "boolean branch"),
    c("switch", "control", "multi-way branch on a value"),
    c("merge", "control", "join converging paths"),
    c("fail", "control", "declared failure terminal"),
    c(
        "pause_for_approval",
        "control",
        "checkpoint + suspend for human approval",
    ),
    c("terminate", "control", "success terminal"),
    // Data plane.
    c("parse_json", "data", "parse a JSON string"),
    c("json_select", "data", "dotted-path select into a value"),
    c("template_render", "data", "{{key}} substitution"),
    c("diff_compute", "data", "structural diff of two values"),
    // I/O.
    c("read_file", "io", "read a file (policy-gated)"),
    c("write_file", "io", "write a file (policy-gated)"),
    c("create_dir", "io", "create a directory (policy-gated)"),
    c("read_env", "io", "read an environment variable"),
    // Network / exec / MCP.
    c("http_request", "net", "outbound HTTP (policy-gated)"),
    c("shell_run", "exec", "allowlisted command (policy-gated)"),
    c("call_mcp_tool", "mcp", "invoke an MCP tool"),
    c("read_mcp_resource", "mcp", "read an MCP resource"),
    // Intelligence.
    c("llm_infer", "intel", "one bounded LLM node"),
    c("agent_loop", "intel", "bounded ReAct inside one node"),
    // Triggers.
    c("trigger_manual", "trigger", "manual / --input trigger"),
    c("trigger_http", "trigger", "HTTP webhook trigger"),
    c("trigger_event", "trigger", "internal event trigger"),
    c("trigger_cron", "trigger", "cron / interval trigger"),
    c("trigger_fs_watch", "trigger", "filesystem-watch trigger"),
    // Policy families.
    c("policy_fs", "policy", "filesystem allowlist enforced"),
    c("policy_env", "policy", "env-var allowlist enforced"),
    c("policy_http", "policy", "HTTP allowlist enforced"),
    c("policy_shell", "policy", "shell allowlist enforced"),
    c(
        "policy_mcp",
        "policy",
        "MCP tool/resource allowlist enforced",
    ),
    // Assurance dimensions.
    c(
        "reliability_passk",
        "assurance",
        "pass^k under response variation",
    ),
    c(
        "security_injection",
        "assurance",
        "policy denial under prompt injection",
    ),
    c(
        "fault_tolerance",
        "assurance",
        "bounded degradation under fault",
    ),
];

/// The canonical capability matrix (see [`MATRIX`]).
pub fn matrix() -> &'static [Capability] {
    MATRIX
}

/// Is `id` a known capability?
pub fn is_known(id: &str) -> bool {
    matrix().iter().any(|c| c.id == id)
}

/// Coverage of the matrix by a set of scenario reports.
#[derive(Debug, Clone)]
pub struct Coverage {
    /// Matrix ids exercised by at least one passing scenario, sorted.
    pub covered: Vec<String>,
    /// Matrix ids not yet exercised by any passing scenario, sorted.
    pub uncovered: Vec<String>,
    /// Scenario tags that are not in the matrix (typos / unknowns).
    pub unknown_tags: Vec<String>,
}

impl Coverage {
    /// Compute coverage. Only *passing* scenarios count toward
    /// coverage — a failing scenario does not prove its capabilities.
    pub fn compute(reports: &[ScenarioReport]) -> Self {
        let mut covered = BTreeSet::new();
        let mut unknown = BTreeSet::new();
        for r in reports {
            for tag in &r.capabilities {
                if !is_known(tag) {
                    unknown.insert(tag.clone());
                } else if r.passed() {
                    covered.insert(tag.clone());
                }
            }
        }
        let uncovered = matrix()
            .iter()
            .map(|c| c.id)
            .filter(|id| !covered.contains(*id))
            .map(String::from)
            .collect();
        Coverage {
            covered: covered.into_iter().collect(),
            uncovered,
            unknown_tags: unknown.into_iter().collect(),
        }
    }

    /// Fraction of the matrix covered, in `[0, 1]`.
    pub fn fraction(&self) -> f64 {
        let total = matrix().len();
        if total == 0 {
            return 1.0;
        }
        self.covered.len() as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cost;
    use std::time::Duration;

    fn rep(name: &str, caps: &[&str], passed: bool) -> ScenarioReport {
        ScenarioReport {
            name: name.into(),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            trials: 1,
            passed_trials: if passed { 1 } else { 0 },
            failures: vec![],
            cost: Cost::default(),
            total_cost: Cost::default(),
            total_latency: Duration::ZERO,
            load_error: None,
        }
    }

    #[test]
    fn matrix_ids_are_unique() {
        let mut seen = BTreeSet::new();
        for c in matrix() {
            assert!(seen.insert(c.id), "duplicate capability id `{}`", c.id);
        }
    }

    #[test]
    fn only_passing_scenarios_cover() {
        let reports = vec![
            rep("a", &["merge", "terminate"], true),
            rep("b", &["switch"], false), // failing → does not cover switch
        ];
        let cov = Coverage::compute(&reports);
        assert!(cov.covered.contains(&"merge".to_string()));
        assert!(!cov.covered.contains(&"switch".to_string()));
        assert!(cov.uncovered.contains(&"switch".to_string()));
        assert!(cov.fraction() > 0.0 && cov.fraction() < 1.0);
    }

    #[test]
    fn unknown_tags_are_flagged() {
        let reports = vec![rep("a", &["merge", "bogus_capability"], true)];
        let cov = Coverage::compute(&reports);
        assert_eq!(cov.unknown_tags, vec!["bogus_capability".to_string()]);
    }
}
