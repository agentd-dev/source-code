//! Suite-level aggregation and rendering.
//!
//! A [`SuiteReport`] rolls up every scenario's [`ScenarioReport`] into
//! the headline reliability and cost figures, and renders them as
//! human-readable text or machine-readable JSON (for CI dashboards).

use serde_json::{Value, json};

use crate::capability::Coverage;
use crate::{Cost, ScenarioReport};

/// The result of running a whole corpus.
pub struct SuiteReport {
    pub scenarios: Vec<ScenarioReport>,
}

impl SuiteReport {
    pub fn new(scenarios: Vec<ScenarioReport>) -> Self {
        Self { scenarios }
    }

    pub fn passed(&self) -> usize {
        self.scenarios.iter().filter(|s| s.passed()).count()
    }

    pub fn failed(&self) -> usize {
        self.scenarios.len() - self.passed()
    }

    pub fn all_passed(&self) -> bool {
        self.failed() == 0
    }

    /// Mean pass^k across scenarios — each contributes 1.0 (all its
    /// trials passed) or 0.0. This is the suite's headline reliability
    /// number: 1.0 means every scenario held under every trial's
    /// seeded response variation.
    pub fn mean_pass_k(&self) -> f64 {
        if self.scenarios.is_empty() {
            return 0.0;
        }
        self.scenarios.iter().map(|s| s.pass_k()).sum::<f64>() / self.scenarios.len() as f64
    }

    /// Capability-matrix coverage across the passing scenarios.
    pub fn coverage(&self) -> Coverage {
        Coverage::compute(&self.scenarios)
    }

    /// Sum of each scenario's representative per-run cost.
    pub fn total_cost(&self) -> Cost {
        self.scenarios.iter().fold(Cost::default(), |mut acc, s| {
            acc.llm_calls += s.cost.llm_calls;
            acc.llm_tokens += s.cost.llm_tokens;
            acc.node_executions += s.cost.node_executions;
            acc.policy_denials += s.cost.policy_denials;
            acc
        })
    }

    /// One line per scenario plus a summary footer.
    pub fn render_text(&self) -> String {
        let mut s = String::new();
        for r in &self.scenarios {
            if r.passed() {
                s.push_str(&format!(
                    "  ok   {:<34} pass^{:<2} = 1.0   {} calls / {} tok\n",
                    r.name, r.trials, r.cost.llm_calls, r.cost.llm_tokens
                ));
            } else {
                s.push_str(&format!(
                    "  FAIL {:<34} pass^{:<2} = {:.2}\n",
                    r.name,
                    r.trials,
                    r.pass_k()
                ));
                if let Some(e) = &r.load_error {
                    s.push_str(&format!("         load error: {e}\n"));
                }
                for f in r.failures.iter().take(8) {
                    s.push_str(&format!("         {f}\n"));
                }
            }
        }
        let cost = self.total_cost();
        s.push_str(&format!(
            "\n{} scenario(s): {} passed, {} failed | mean pass^k = {:.3} | {} llm calls, {} tokens\n",
            self.scenarios.len(),
            self.passed(),
            self.failed(),
            self.mean_pass_k(),
            cost.llm_calls,
            cost.llm_tokens,
        ));
        let cov = self.coverage();
        s.push_str(&format!(
            "coverage: {}/{} capabilities ({:.0}%)\n",
            cov.covered.len(),
            crate::capability::matrix().len(),
            cov.fraction() * 100.0,
        ));
        if !cov.uncovered.is_empty() {
            s.push_str(&format!("  uncovered: {}\n", cov.uncovered.join(", ")));
        }
        if !cov.unknown_tags.is_empty() {
            s.push_str(&format!(
                "  WARNING unknown capability tags: {}\n",
                cov.unknown_tags.join(", ")
            ));
        }
        s
    }

    /// Machine-readable form for CI dashboards and trend tracking.
    pub fn to_json(&self) -> Value {
        let cost = self.total_cost();
        let cov = self.coverage();
        let scenarios: Vec<Value> = self
            .scenarios
            .iter()
            .map(|r| {
                json!({
                    "name": r.name,
                    "capabilities": r.capabilities,
                    "trials": r.trials,
                    "passed_trials": r.passed_trials,
                    "pass_k": r.pass_k(),
                    "passed": r.passed(),
                    "cost": {
                        "llm_calls": r.cost.llm_calls,
                        "llm_tokens": r.cost.llm_tokens,
                        "node_executions": r.cost.node_executions,
                        "policy_denials": r.cost.policy_denials,
                    },
                    "total_latency_ms": r.total_latency.as_millis() as u64,
                    "load_error": r.load_error,
                    "failures": r.failures,
                })
            })
            .collect();
        json!({
            "summary": {
                "scenarios": self.scenarios.len(),
                "passed": self.passed(),
                "failed": self.failed(),
                "mean_pass_k": self.mean_pass_k(),
                "total_llm_calls": cost.llm_calls,
                "total_llm_tokens": cost.llm_tokens,
                "coverage": {
                    "covered": cov.covered.len(),
                    "total": crate::capability::matrix().len(),
                    "fraction": cov.fraction(),
                    "uncovered": cov.uncovered,
                    "unknown_tags": cov.unknown_tags,
                },
            },
            "scenarios": scenarios,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rep(name: &str, trials: u32, passed_trials: u32) -> ScenarioReport {
        ScenarioReport {
            name: name.into(),
            capabilities: vec!["terminate".into()],
            trials,
            passed_trials,
            failures: if passed_trials == trials {
                vec![]
            } else {
                vec!["trial 1: boom".into()]
            },
            cost: Cost {
                llm_calls: 1,
                llm_tokens: 100,
                node_executions: 2,
                policy_denials: 0,
            },
            total_latency: Duration::from_millis(3),
            load_error: None,
        }
    }

    #[test]
    fn mean_pass_k_and_totals() {
        let suite = SuiteReport::new(vec![rep("a", 8, 8), rep("b", 8, 5)]);
        assert_eq!(suite.passed(), 1);
        assert_eq!(suite.failed(), 1);
        assert!((suite.mean_pass_k() - 0.5).abs() < 1e-9);
        assert_eq!(suite.total_cost().llm_tokens, 200);
        assert!(!suite.all_passed());
    }

    #[test]
    fn json_shape_has_summary_and_scenarios() {
        let suite = SuiteReport::new(vec![rep("a", 4, 4)]);
        let v = suite.to_json();
        assert_eq!(v["summary"]["scenarios"], 1);
        assert_eq!(v["summary"]["passed"], 1);
        assert_eq!(v["scenarios"][0]["pass_k"], 1.0);
        assert_eq!(v["scenarios"][0]["name"], "a");
    }
}
