//! Cost-per-success, not raw cost: two workflows that spend the same
//! tokens per call but differ in reliability must report different
//! cost-per-success. The flaky one pays for the trials it wastes.

use agentd_conformance::{Scenario, run_scenario};

// Every trial routes to success; each spends 100 tokens.
const RELIABLE: &str = r#"
name = "reliable"
trials = 4

[workflow]
inline = '''
name = "reliable"
[[start_nodes]]
name = "main"
source = "manual"
entry_node = "ask"
[[nodes]]
id = "ask"
type = "llm_infer"
backend = "default"
prompt = "x"
input_from = "trigger"
output_schema = "inline"
[[nodes]]
id = "route"
type = "switch"
expr = "ask.parsed.decision"
[[nodes]]
id = "ok"
type = "terminate"
[[edges]]
from = "ask"
to = "route"
[[edges]]
from = "route"
when = "alpha"
to = "ok"
'''

[[intel.turns]]
content = '{"decision": "alpha"}'
prompt_tokens = 80
completion_tokens = 20

[expected]
status = "completed"
"#;

// Same per-call cost, but odd trials route to a declared failure: the
// call still spends its 100 tokens, the run still does not succeed.
const FLAKY: &str = r#"
name = "flaky"
trials = 4

[workflow]
inline = '''
name = "flaky"
[[start_nodes]]
name = "main"
source = "manual"
entry_node = "ask"
[[nodes]]
id = "ask"
type = "llm_infer"
backend = "default"
prompt = "x"
input_from = "trigger"
output_schema = "inline"
[[nodes]]
id = "route"
type = "switch"
expr = "ask.parsed.decision"
[[nodes]]
id = "ok"
type = "terminate"
[[nodes]]
id = "bad"
type = "fail"
reason = "unrouted answer"
[[edges]]
from = "ask"
to = "route"
[[edges]]
from = "route"
when = "alpha"
to = "ok"
[[edges]]
from = "route"
when = "beta"
to = "bad"
'''

[[intel.turns]]
variants = [
    { content = '{"decision": "alpha"}', prompt_tokens = 80, completion_tokens = 20 },
    { content = '{"decision": "beta"}', prompt_tokens = 80, completion_tokens = 20 },
]

[expected]
status = "completed"
"#;

#[test]
fn cost_per_success_penalises_unreliability() {
    let reliable = run_scenario(&Scenario::from_toml(RELIABLE).unwrap());
    let flaky = run_scenario(&Scenario::from_toml(FLAKY).unwrap());

    // The reliable workflow passes every trial: 100 tokens each, all 4
    // succeed → 100 tokens per success.
    assert!(reliable.passed());
    let reliable_cps = reliable.cost_per_success().unwrap();
    assert!((reliable_cps - 100.0).abs() < 1e-9, "got {reliable_cps}");

    // The flaky workflow spent the same 100 tokens on all 4 trials but
    // only 2 succeeded → 200 tokens per success. Reliability shows up
    // as cost, exactly as intended.
    assert!(!flaky.passed());
    assert_eq!(flaky.passed_trials, 2);
    let flaky_cps = flaky.cost_per_success().unwrap();
    assert!((flaky_cps - 200.0).abs() < 1e-9, "got {flaky_cps}");

    assert!(
        flaky_cps > reliable_cps,
        "unreliability must raise cost-per-success ({flaky_cps} vs {reliable_cps})"
    );
}
