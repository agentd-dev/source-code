//! Conformance, reliability, and cost suite for the agentd runtime.
//!
//! A *scenario* ([`Scenario`]) declares a workflow, its trigger, canned
//! intelligence responses, an optional enforced policy, and the
//! expected outcome / trace / cost. [`run_scenario`] drives the real
//! engine — see [`harness`] — once per trial and aggregates the trials
//! into a [`ScenarioReport`].
//!
//! The suite is the executable form of the runtime's promises: a
//! corpus that passes is a runtime that still does what the RFCs say.
//! Scenarios are tagged against a capability matrix, so corpus
//! coverage doubles as goal tracking.

pub mod capability;
pub mod harness;
pub mod report;
pub mod scenario;

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use capability::Coverage;
pub use harness::{Cost, TrialOutcome};
pub use report::SuiteReport;
pub use scenario::Scenario;

/// Aggregate result of running every trial of one scenario.
#[derive(Debug, Clone)]
pub struct ScenarioReport {
    pub name: String,
    pub capabilities: Vec<String>,
    pub trials: u32,
    pub passed_trials: u32,
    /// One line per failed assertion, prefixed with its trial index.
    pub failures: Vec<String>,
    /// Representative per-run cost (trial 0).
    pub cost: Cost,
    /// Wall-clock summed across all trials.
    pub total_latency: Duration,
    /// Set if the scenario could not be loaded / built / validated —
    /// distinct from a trial assertion failure.
    pub load_error: Option<String>,
}

impl ScenarioReport {
    /// A scenario passes iff it built and *every* trial passed
    /// (tau-bench per-scenario pass^k semantics).
    pub fn passed(&self) -> bool {
        self.load_error.is_none() && self.trials > 0 && self.passed_trials == self.trials
    }

    /// pass^k for this scenario: 1.0 iff all k trials passed, else 0.0.
    pub fn pass_k(&self) -> f64 {
        if self.passed() { 1.0 } else { 0.0 }
    }
}

/// Run every trial of an already-parsed scenario and aggregate.
pub fn run_scenario(scenario: &Scenario) -> ScenarioReport {
    let mut report = ScenarioReport {
        name: scenario.name.clone(),
        capabilities: scenario.capabilities.clone(),
        trials: scenario.trials.max(1),
        passed_trials: 0,
        failures: Vec::new(),
        cost: Cost::default(),
        total_latency: Duration::ZERO,
        load_error: None,
    };

    let doc = match scenario.workflow_doc() {
        Ok(d) => d,
        Err(e) => {
            report.load_error = Some(e);
            return report;
        }
    };

    // A malformed workflow is a scenario failure, surfaced like the
    // daemon would (validation before any execution).
    let vr = agentd::workflow::validate(&doc);
    if !vr.ok() {
        let issues = vr
            .issues
            .iter()
            .map(|i| format!("[{}] {}", i.code, i.message))
            .collect::<Vec<_>>()
            .join("; ");
        report.load_error = Some(format!("workflow invalid: {issues}"));
        return report;
    }

    let start = match scenario.start_name(&doc) {
        Ok(s) => s,
        Err(e) => {
            report.load_error = Some(e);
            return report;
        }
    };

    for trial in 0..report.trials {
        match harness::run_trial(scenario, &doc, &start, trial) {
            Ok(o) => {
                report.total_latency += o.latency;
                if trial == 0 {
                    report.cost = o.cost;
                }
                if o.passed {
                    report.passed_trials += 1;
                } else {
                    for f in o.failures {
                        report.failures.push(format!("trial {trial}: {f}"));
                    }
                }
            }
            Err(e) => report.failures.push(format!("trial {trial}: {e}")),
        }
    }

    report
}

/// Load a scenario file and run it.
pub fn run_scenario_file(path: &Path) -> ScenarioReport {
    match Scenario::load(path) {
        Ok(s) => run_scenario(&s),
        Err(e) => ScenarioReport {
            name: path.display().to_string(),
            capabilities: Vec::new(),
            trials: 0,
            passed_trials: 0,
            failures: Vec::new(),
            cost: Cost::default(),
            total_latency: Duration::ZERO,
            load_error: Some(e),
        },
    }
}

/// Discover and run every scenario under `root`, aggregating into a
/// [`SuiteReport`].
pub fn run_corpus(root: &Path) -> std::io::Result<SuiteReport> {
    let files = discover_scenarios(root)?;
    let scenarios = files.iter().map(|p| run_scenario_file(p)).collect();
    Ok(SuiteReport::new(scenarios))
}

/// Recursively collect scenario `*.toml` files under `root`, sorted.
pub fn discover_scenarios(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            out.push(path);
        }
    }
    Ok(())
}
