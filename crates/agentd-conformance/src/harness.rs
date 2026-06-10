//! The engine harness: build the runtime for a scenario and run a
//! single trial, capturing outcome, trace, and cost.
//!
//! The harness drives the *real* engine through agentd's public API —
//! the same control handlers, tool families, intelligence handler, and
//! policy enforcement the daemon uses. Only the intelligence backend
//! is a mock (seeded canned responses), so a scenario is deterministic
//! given its trial index.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentd::engine::{
    Engine, ExecutionOutcome, ExecutionTrace, HandlerRegistry, RunOptions, StubHandler, TriggerMeta,
};
use agentd::intelligence::MockClient;
use agentd::intelligence::client::IntelligenceRef;
use agentd::intelligence::protocol::{Response, Usage};
use agentd::observability::Metrics;
use agentd::tools::policy::{AllowAll, PolicyRef};
use agentd::workflow::WorkflowDoc;

use crate::scenario::{Expected, Scenario, TriggerKind};

/// Per-run cost, read from the metrics counters the engine drove.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    pub llm_calls: u64,
    pub llm_tokens: u64,
    pub node_executions: u64,
    pub policy_denials: u64,
}

/// The result of one trial of a scenario.
#[derive(Debug, Clone)]
pub struct TrialOutcome {
    pub passed: bool,
    /// Empty iff `passed`. One human-readable line per failed assertion.
    pub failures: Vec<String>,
    /// `completed` | `failed` | `timed_out`.
    pub status: String,
    pub cost: Cost,
    pub latency: Duration,
}

/// Build the engine for `scenario` and run trial number `trial`.
///
/// `doc` and `start` are resolved once by the caller and reused across
/// trials so per-trial cost excludes parse time.
pub fn run_trial(
    scenario: &Scenario,
    doc: &WorkflowDoc,
    start: &str,
    trial: u32,
) -> Result<TrialOutcome, String> {
    let mut registry = HandlerRegistry::with_builtin_controls();

    // Policy precedence: an explicit scenario `[policy]` (security
    // scenarios) wins; else the workflow's own `[policy]`; else
    // AllowAll. Tool handlers enforce whichever applies.
    let manifest = scenario.policy.clone().or_else(|| doc.policy.clone());
    let policy: PolicyRef = match manifest {
        Some(m) => Arc::new(
            agentd::policy::ManifestPolicy::new(m).map_err(|e| format!("policy load: {e}"))?,
        ),
        None => Arc::new(AllowAll),
    };

    let budget = agentd::budget::unbounded();
    agentd::tools::register_default_tools(&mut registry, policy.clone(), budget.clone());

    // Our own metrics handle — snapshot after the run gives cost.
    let metrics = Metrics::new();

    // Intelligence: enqueue this trial's seeded variant for each turn,
    // in call order, each carrying the declared usage so cost is real.
    if !scenario.intel.turns.is_empty() {
        let mock = Arc::new(MockClient::new());
        for (i, turn) in scenario.intel.turns.iter().enumerate() {
            let variants = turn.variants();
            let v = &variants[pick_variant(trial, i, variants.len())];
            mock.enqueue(Response {
                content: v.content.clone(),
                usage: Usage {
                    prompt_tokens: v.prompt_tokens,
                    completion_tokens: v.completion_tokens,
                },
            });
        }
        let arc: IntelligenceRef = mock;
        agentd::intelligence::handler::register(
            &mut registry,
            agentd::intelligence::backends::single_backend(arc),
            budget.clone(),
            metrics.clone(),
        );
    }

    registry.set_fallback(Box::new(StubHandler));
    let engine = Engine::new(registry);

    let trigger = match scenario.trigger.kind {
        TriggerKind::Manual => TriggerMeta::manual(scenario.trigger.payload.clone()),
        TriggerKind::Http => TriggerMeta::http(scenario.trigger.payload.clone()),
        TriggerKind::Event => TriggerMeta::event(scenario.trigger.payload.clone()),
    };
    let options = RunOptions {
        timeout: Duration::from_secs(scenario.timeout_secs.max(1)),
        dry_run: false,
    };

    let t0 = Instant::now();
    let (outcome, trace) = engine
        .run_with_trace(doc, start, trigger, options)
        .map_err(|e| format!("engine error: {e}"))?;
    let latency = t0.elapsed();

    let snap = metrics.snapshot();
    let cost = Cost {
        llm_calls: snap.llm_calls,
        llm_tokens: snap.llm_tokens,
        node_executions: snap.node_executions,
        policy_denials: snap.policy_denials,
    };

    let failures = diff_expected(&scenario.expected, &outcome, &trace, &cost);
    Ok(TrialOutcome {
        passed: failures.is_empty(),
        failures,
        status: outcome.status_label().to_string(),
        cost,
        latency,
    })
}

/// Deterministic per-trial variant selection. Single-variant turns are
/// invariant (always 0); multi-variant turns mix the trial and turn
/// indices so trial 3 picks a reproducible-but-different combination
/// than trial 4. No RNG — replayable from the trial index alone.
fn pick_variant(trial: u32, turn: usize, n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let h = (trial as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (turn as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
    (h % n as u64) as usize
}

fn diff_expected(
    expected: &Expected,
    outcome: &ExecutionOutcome,
    trace: &ExecutionTrace,
    cost: &Cost,
) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(want) = &expected.status {
        let got = outcome.status_label();
        if want != got {
            out.push(format!("status: expected `{want}`, got `{got}`"));
        }
    }

    let last_node = match outcome {
        ExecutionOutcome::Completed { last_node, .. }
        | ExecutionOutcome::Failed { last_node, .. }
        | ExecutionOutcome::TimedOut { last_node, .. } => last_node.clone(),
    };
    if let Some(want) = &expected.last_node {
        if last_node.as_deref() != Some(want.as_str()) {
            out.push(format!("last_node: expected `{want}`, got `{last_node:?}`"));
        }
    }

    if let Some(needle) = &expected.reason_contains {
        match outcome {
            ExecutionOutcome::Failed { reason, .. } if reason.contains(needle) => {}
            ExecutionOutcome::Failed { reason, .. } => {
                out.push(format!("reason_contains `{needle}` not in `{reason}`"))
            }
            _ => out.push("reason_contains set but outcome is not `failed`".to_string()),
        }
    }

    if !expected.path.is_empty() {
        let got = trace.node_ids();
        if expected.path_exact {
            if got != expected.path {
                out.push(format!(
                    "path (exact): expected {:?}, got {:?}",
                    expected.path, got
                ));
            }
        } else {
            let n = expected.path.len();
            if got.len() < n || got[..n] != expected.path[..] {
                out.push(format!(
                    "path (prefix): expected {:?}, got {:?}",
                    expected.path, got
                ));
            }
        }
    }

    if let Some(max) = expected.max_llm_calls {
        if cost.llm_calls > max {
            out.push(format!("max_llm_calls: {} > {max}", cost.llm_calls));
        }
    }
    if let Some(max) = expected.max_total_tokens {
        if cost.llm_tokens > max {
            out.push(format!("max_total_tokens: {} > {max}", cost.llm_tokens));
        }
    }
    if let Some(min) = expected.min_policy_denials {
        if cost.policy_denials < min {
            out.push(format!(
                "min_policy_denials: {} < {min}",
                cost.policy_denials
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_variant_is_invariant_across_trials() {
        assert_eq!(pick_variant(0, 0, 1), 0);
        assert_eq!(pick_variant(99, 5, 1), 0);
    }

    #[test]
    fn multi_variant_selection_is_in_range_and_deterministic() {
        for trial in 0..50 {
            let a = pick_variant(trial, 0, 3);
            let b = pick_variant(trial, 0, 3);
            assert_eq!(a, b, "selection must be deterministic per trial");
            assert!(a < 3);
        }
    }
}
