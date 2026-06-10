//! The conformance corpus is a CI gate: every scenario under
//! `corpus/conformance/` must pass on every commit.

use std::path::Path;

use agentd_conformance::{discover_scenarios, run_corpus, run_scenario_file};

#[test]
fn conformance_corpus_all_pass() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/conformance");
    let files = discover_scenarios(&root).expect("read corpus dir");
    assert!(!files.is_empty(), "no conformance scenarios discovered");

    let mut failures = Vec::new();
    for path in &files {
        let report = run_scenario_file(path);
        if !report.passed() {
            failures.push(format!(
                "  {} (pass^{}): load_error={:?} failures={:?}",
                report.name, report.trials, report.load_error, report.failures
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} conformance scenario(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn corpus_capability_tags_are_all_known() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
    let suite = run_corpus(&root).expect("run corpus");
    let cov = suite.coverage();
    assert!(
        cov.unknown_tags.is_empty(),
        "corpus uses capability tags not in the matrix: {:?}",
        cov.unknown_tags
    );
}
