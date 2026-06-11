//! pass^k discrimination: the reliability metric must separate a
//! workflow that handles every model answer (pass^k = 1.0) from one
//! that does not (pass^k < 1.0). If both scored 1.0 the metric would
//! be theatre.

use agentd_conformance::{Scenario, SuiteReport, run_scenario};

// A router that handles BOTH possible answers. Every trial completes.
const ROBUST: &str = r#"
name = "robust"
trials = 8

[workflow]
inline = """
name = "robust"
[[start_nodes]]
name = "main"
source = "manual"
entry_node = "classify"
[[nodes]]
id = "classify"
type = "llm_infer"
backend = "default"
prompt = "decide"
input_from = "trigger"
output_schema = "inline"
[[nodes]]
id = "route"
type = "switch"
expr = "classify.parsed.decision"
[[nodes]]
id = "a"
type = "terminate"
[[nodes]]
id = "b"
type = "terminate"
[[edges]]
from = "classify"
to = "route"
[[edges]]
from = "route"
when = "alpha"
to = "a"
[[edges]]
from = "route"
when = "beta"
to = "b"
"""

[[intel.turns]]
variants = [
    { content = '{"decision": "alpha"}' },
    { content = '{"decision": "beta"}' },
]

[expected]
status = "completed"
"#;

// Same shape, but the model sometimes returns malformed JSON the
// node rejects — and `output_schema` makes that a hard failure.
// Even trials get the valid answer, odd trials get garbage, so the
// run cannot be reliable.
const FRAGILE: &str = r#"
name = "fragile"
trials = 8

[workflow]
inline = """
name = "fragile"
[[start_nodes]]
name = "main"
source = "manual"
entry_node = "classify"
[[nodes]]
id = "classify"
type = "llm_infer"
backend = "default"
prompt = "decide"
input_from = "trigger"
output_schema = "inline"
[[nodes]]
id = "route"
type = "switch"
expr = "classify.parsed.decision"
[[nodes]]
id = "a"
type = "terminate"
[[edges]]
from = "classify"
to = "route"
[[edges]]
from = "route"
when = "alpha"
to = "a"
"""

[[intel.turns]]
variants = [
    { content = '{"decision": "alpha"}' },
    { content = 'not json at all' },
]

[expected]
status = "completed"
"#;

#[test]
fn robust_workflow_holds_pass_k_one() {
    let s = Scenario::from_toml(ROBUST).unwrap();
    let r = run_scenario(&s);
    assert!(
        r.passed(),
        "expected all trials to pass; failures: {:?}",
        r.failures
    );
    assert_eq!(r.pass_k(), 1.0);
    assert_eq!(r.passed_trials, 8);
}

// Same flaky shape (≈0.5 pass_rate) but it *declares* it tolerates
// 0.4 — so it passes its own contract, yet a stricter suite floor gates
// it. This is reliability-gated autonomy: trust is earned, measured.
const FLAKY_TOLERANT: &str = r#"
name = "flaky-tolerant"
trials = 8
min_pass_rate = 0.4

[workflow]
inline = """
name = "flaky"
[[start_nodes]]
name = "main"
source = "manual"
entry_node = "classify"
[[nodes]]
id = "classify"
type = "llm_infer"
backend = "default"
prompt = "decide"
input_from = "trigger"
output_schema = "inline"
[[nodes]]
id = "route"
type = "switch"
expr = "classify.parsed.decision"
[[nodes]]
id = "a"
type = "terminate"
[[edges]]
from = "classify"
to = "route"
[[edges]]
from = "route"
when = "alpha"
to = "a"
"""

[[intel.turns]]
variants = [
    { content = '{"decision": "alpha"}' },
    { content = 'not json at all' },
]

[expected]
status = "completed"
"#;

#[test]
fn reliability_gate_honours_declared_bar_and_a_stricter_floor() {
    let r = run_scenario(&Scenario::from_toml(FLAKY_TOLERANT).unwrap());
    // Strict pass^k is still honest about the flakiness.
    assert_eq!(r.pass_k(), 0.0);
    assert!((r.pass_rate() - 0.5).abs() < 1e-9, "rate {}", r.pass_rate());
    // It meets its declared 0.4 contract, so it "passes".
    assert!(r.passed(), "should meet its own min_pass_rate");

    let suite = SuiteReport::new(vec![r]);
    // No extra floor → no violation (it cleared its own bar).
    assert!(suite.reliability_violations(None).is_empty());
    // A stricter suite-wide floor of 0.6 gates it.
    let v = suite.reliability_violations(Some(0.6));
    assert_eq!(v.len(), 1);
    assert!((v[0].required - 0.6).abs() < 1e-9);
}

#[test]
fn fragile_workflow_decays_below_one() {
    let s = Scenario::from_toml(FRAGILE).unwrap();
    let r = run_scenario(&s);
    assert_eq!(r.pass_k(), 0.0, "fragile workflow must not score pass^k=1");
    // The valid-answer trials still pass, so it is a genuine decay,
    // not a total failure — the metric measures reliability, not
    // a binary works/broken.
    assert!(
        r.passed_trials > 0 && r.passed_trials < 8,
        "expected partial reliability, got {}/8",
        r.passed_trials
    );
}
