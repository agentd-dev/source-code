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

/// A scenario whose measured `pass_rate` fell below the reliability bar
/// it was held to (its own `min_pass_rate`, or a suite-wide floor).
#[derive(Debug, Clone)]
pub struct ReliabilityViolation {
    pub name: String,
    pub pass_rate: f64,
    pub required: f64,
}

/// Projected spend at a given trigger rate.
#[derive(Debug, Clone)]
pub struct Forecast {
    pub runs_per_day: f64,
    pub cost_per_success_tokens: f64,
    pub tokens_per_day: f64,
    pub tokens_per_month: f64,
    pub usd_per_month: Option<f64>,
}

/// How one scenario moved relative to a saved baseline.
#[derive(Debug, Clone)]
pub struct Drift {
    pub name: String,
    pub old_pass_rate: f64,
    pub new_pass_rate: f64,
    pub old_tokens: u64,
    pub new_tokens: u64,
    /// `pass_rate` dropped — the regression a model update would cause.
    pub regressed: bool,
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

    /// Scenarios that fell below their reliability bar — the heart of
    /// reliability-gated autonomy. Each scenario is held to the higher
    /// of its own declared `min_pass_rate` and the suite-wide `floor`
    /// (`--min-pass-rate`); a scenario with neither is not gated.
    pub fn reliability_violations(&self, floor: Option<f64>) -> Vec<ReliabilityViolation> {
        self.scenarios
            .iter()
            .filter_map(|s| {
                let required = match (floor, s.min_pass_rate) {
                    (Some(f), Some(m)) => f.max(m),
                    (Some(f), None) => f,
                    (None, Some(m)) => m,
                    (None, None) => return None,
                };
                let rate = s.pass_rate();
                (rate + 1e-9 < required).then(|| ReliabilityViolation {
                    name: s.name.clone(),
                    pass_rate: rate,
                    required,
                })
            })
            .collect()
    }

    /// Passing trials summed across the suite.
    pub fn total_passed_trials(&self) -> u64 {
        self.scenarios.iter().map(|s| s.passed_trials as u64).sum()
    }

    /// Tokens spent across *every* trial of *every* scenario.
    pub fn total_trial_tokens(&self) -> u64 {
        self.scenarios.iter().map(|s| s.total_cost.llm_tokens).sum()
    }

    /// Suite cost-per-success: tokens spent across all trials divided by
    /// the number that actually passed. Reliability shows up here — a
    /// suite that retries its way to green pays for it. `None` if
    /// nothing passed.
    pub fn cost_per_success(&self) -> Option<f64> {
        let passed = self.total_passed_trials();
        if passed == 0 {
            return None;
        }
        Some(self.total_trial_tokens() as f64 / passed as f64)
    }

    /// Project spend from cost-per-success and a trigger rate. The
    /// deterministic substrate makes this honest: cost-per-success is a
    /// measured constant, so spend scales linearly with run volume.
    pub fn forecast(&self, runs_per_day: f64, price_per_mtok: Option<f64>) -> Option<Forecast> {
        let cps = self.cost_per_success()?;
        let tokens_per_day = cps * runs_per_day;
        let tokens_per_month = tokens_per_day * 30.0;
        Some(Forecast {
            runs_per_day,
            cost_per_success_tokens: cps,
            tokens_per_day,
            tokens_per_month,
            usd_per_month: price_per_mtok.map(|p| tokens_per_month * p / 1_000_000.0),
        })
    }

    /// Compare this run against a saved baseline report (the JSON
    /// [`to_json`](Self::to_json) emitted earlier). Drift is reported
    /// per scenario present in both; a *regression* is a drop in
    /// `pass_rate` — the "a model update broke it" signal.
    pub fn drift_vs(&self, baseline: &Value) -> Vec<Drift> {
        let base = baseline
            .get("scenarios")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let lookup = |name: &str| -> Option<(f64, u64)> {
            base.iter().find_map(|s| {
                if s.get("name").and_then(Value::as_str) == Some(name) {
                    let rate = s.get("pass_rate").and_then(Value::as_f64).unwrap_or(0.0);
                    let toks = s
                        .get("cost")
                        .and_then(|c| c.get("llm_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    Some((rate, toks))
                } else {
                    None
                }
            })
        };
        self.scenarios
            .iter()
            .filter_map(|s| {
                let (old_rate, old_tokens) = lookup(&s.name)?;
                let new_rate = s.pass_rate();
                Some(Drift {
                    name: s.name.clone(),
                    old_pass_rate: old_rate,
                    new_pass_rate: new_rate,
                    old_tokens,
                    new_tokens: s.cost.llm_tokens,
                    regressed: new_rate + 1e-9 < old_rate,
                })
            })
            .collect()
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
        if let Some(cps) = self.cost_per_success() {
            s.push_str(&format!(
                "cost-per-success: {:.0} tokens/success ({} tokens over {} passing trials)\n",
                cps,
                self.total_trial_tokens(),
                self.total_passed_trials(),
            ));
        }
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
                    "pass_rate": r.pass_rate(),
                    "min_pass_rate": r.min_pass_rate,
                    "passed": r.passed(),
                    "cost": {
                        "llm_calls": r.cost.llm_calls,
                        "llm_tokens": r.cost.llm_tokens,
                        "node_executions": r.cost.node_executions,
                        "policy_denials": r.cost.policy_denials,
                    },
                    "cost_per_success_tokens": r.cost_per_success(),
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
                "cost_per_success_tokens": self.cost_per_success(),
                "total_trial_tokens": self.total_trial_tokens(),
                "total_passed_trials": self.total_passed_trials(),
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
            min_pass_rate: None,
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
            total_cost: Cost {
                llm_calls: trials as u64,
                llm_tokens: 100 * trials as u64,
                node_executions: 2 * trials as u64,
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

    #[test]
    fn forecast_scales_cost_per_success_by_rate() {
        // rep("a",4,4): 400 tokens over 4 passing trials → 100 / success.
        let suite = SuiteReport::new(vec![rep("a", 4, 4)]);
        let f = suite.forecast(1_000.0, Some(5.0)).unwrap();
        assert!((f.cost_per_success_tokens - 100.0).abs() < 1e-9);
        assert!((f.tokens_per_day - 100_000.0).abs() < 1e-6);
        assert!((f.tokens_per_month - 3_000_000.0).abs() < 1e-6);
        // 3M tokens/month at $5 / 1M = $15.
        assert!((f.usd_per_month.unwrap() - 15.0).abs() < 1e-9);
    }

    #[test]
    fn drift_flags_a_pass_rate_regression() {
        let baseline = SuiteReport::new(vec![rep("a", 4, 4)]).to_json(); // rate 1.0
        let now = SuiteReport::new(vec![rep("a", 4, 2)]); // rate 0.5
        let drift = now.drift_vs(&baseline);
        assert_eq!(drift.len(), 1);
        assert!(drift[0].regressed);
        assert!((drift[0].old_pass_rate - 1.0).abs() < 1e-9);
        assert!((drift[0].new_pass_rate - 0.5).abs() < 1e-9);

        // No regression when reliability holds.
        let stable = SuiteReport::new(vec![rep("a", 4, 4)]);
        assert!(!stable.drift_vs(&baseline)[0].regressed);
    }
}
