//! Security conformance: a policy denial must *prevent* the privileged
//! side-effect, not merely record it. This test drives the injection
//! end-to-end against a real temp directory and asserts the escaping
//! write never reached the disk.

use std::fs;

use agentd_conformance::{Scenario, run_scenario};

#[test]
fn injection_write_is_denied_before_disk_touch() {
    let tmp = std::env::temp_dir().join(format!("agentd-conf-sec-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let allowed = tmp.join("allowed");
    fs::create_dir_all(&allowed).unwrap();
    // The injection target sits OUTSIDE the allowlisted directory.
    let escape_target = tmp.join("escaped.txt");
    assert!(!escape_target.exists());

    let scenario = format!(
        r#"
name = "sec-escape"
capabilities = ["llm_infer", "write_file", "policy_fs", "security_injection"]

[policy.fs]
write = ["{allowed}/**"]

[workflow]
inline = '''
name = "esc"
[[start_nodes]]
name = "main"
source = "manual"
entry_node = "g"
[[nodes]]
id = "g"
type = "llm_infer"
backend = "default"
prompt = "x"
input_from = "trigger"
output_schema = "inline"
[[nodes]]
id = "save"
type = "write_file"
path_from = "g.parsed.path"
content_from = "g.parsed.body"
[[edges]]
from = "g"
to = "save"
'''

[[intel.turns]]
content = '{{"path": "{escape}", "body": "owned"}}'

[expected]
status = "errored"
reason_contains = "denied"
min_policy_denials = 1
"#,
        allowed = allowed.display(),
        escape = escape_target.display(),
    );

    let s = Scenario::from_toml(&scenario).unwrap();
    let r = run_scenario(&s);
    assert!(r.passed(), "expected denial; failures: {:?}", r.failures);
    assert!(
        !escape_target.exists(),
        "the escaping write must never have reached disk"
    );
    assert_eq!(r.cost.policy_denials, 1);

    let _ = fs::remove_dir_all(&tmp);
}
