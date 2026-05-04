//! Auto-discovered fixture suite.
//!
//! Walks `tests/fixtures/` and runs every directory that carries
//! both `workflow.toml` and `fixture.toml`. One `#[test]` that
//! reports all failures (not just the first) so a bad fixture
//! doesn't mask the state of the others.
//!
//! New fixtures are added by creating a sibling directory — no code
//! change needed.

use std::path::PathBuf;

use agentd::testing::{FixtureStatus, discover_fixtures, run_fixture};

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

#[test]
fn every_in_tree_fixture_passes() {
    let root = fixtures_root();
    let fixtures = discover_fixtures(&root)
        .unwrap_or_else(|e| panic!("failed to enumerate {}: {e}", root.display()));
    assert!(
        !fixtures.is_empty(),
        "expected at least one fixture under {}",
        root.display()
    );

    let mut report: Vec<String> = Vec::new();
    let total = fixtures.len();
    let mut passed = 0usize;

    for dir in fixtures {
        let result = run_fixture(&dir);
        match result.status {
            FixtureStatus::Pass => {
                passed += 1;
                eprintln!("  fixture `{}` ... ok", dir.display());
            }
            other => {
                eprintln!("  fixture `{}` ... FAIL ({other:?})", dir.display());
                report.push(format!(
                    "{}:\n    - {}",
                    dir.display(),
                    result.failures.join("\n    - ")
                ));
            }
        }
    }

    if !report.is_empty() {
        panic!(
            "{}/{} fixtures failed:\n{}",
            total - passed,
            total,
            report.join("\n")
        );
    }
}
