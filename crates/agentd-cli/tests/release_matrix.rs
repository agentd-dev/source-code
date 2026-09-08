// SPDX-License-Identifier: AGPL-3.0-only
//! **The feature set CI lints is the feature set the release ships.**
//!
//! `release.yml` builds the binaries and the container with one `FEATURES`
//! list. `ci.yml` lints a matrix of feature rows, one of which is commented
//! "the release-artifact set, exactly". It was not: `sign`, `oci` and
//! `decrypt` were added to the release long after that row was written, so the
//! exact combination that ships was the one combination nothing linted — and
//! `-D warnings` breaks appeared in a solo `--features oci` build that no row
//! covered.
//!
//! A comment cannot keep two files in step. This can.
#![cfg(unix)]

use std::path::Path;

fn workflow(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `FEATURES: "a,b,c"` from release.yml.
fn release_features() -> Vec<String> {
    let text = workflow("release.yml");
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("FEATURES:"))
        .expect("release.yml declares FEATURES");
    let list = line.split_once(':').unwrap().1.trim().trim_matches('"');
    list.split(',').map(|s| s.trim().to_string()).collect()
}

#[test]
fn ci_lints_exactly_what_the_release_ships() {
    let want = release_features();
    assert!(want.len() > 5, "FEATURES parsed as {want:?}");

    let ci = workflow("ci.yml");
    // Every row in the clippy matrix, as its feature list.
    let rows: Vec<Vec<String>> = ci
        .lines()
        .filter_map(|l| l.trim().strip_prefix("- \""))
        .filter_map(|l| l.split_once('"').map(|(row, _)| row))
        .filter_map(|row| row.split("--features ").nth(1))
        .map(|f| {
            f.split_whitespace()
                .next()
                .unwrap_or("")
                .split(',')
                .map(str::to_string)
                .collect()
        })
        .collect();
    assert!(!rows.is_empty(), "no --features rows found in ci.yml");

    // The exact release combination is a row.
    let mut sorted_want = want.clone();
    sorted_want.sort();
    let has_exact = rows.iter().any(|r| {
        let mut s = r.clone();
        s.sort();
        s == sorted_want
    });
    assert!(
        has_exact,
        "no ci.yml row is the release set exactly.\n  release.yml FEATURES: {}\n  add: - \"--features {}\"",
        want.join(","),
        want.join(",")
    );

    // …and every feature it ships is also linted on its own, because
    // `--all-features` unification hides a solo-build break.
    let solo: Vec<&Vec<String>> = rows.iter().filter(|r| r.len() == 1).collect();
    let missing: Vec<&String> = want
        .iter()
        .filter(|f| !solo.iter().any(|r| &&r[0] == f))
        .collect();
    assert!(
        missing.is_empty(),
        "shipped features with no solo lint row in ci.yml: {missing:?}"
    );
}

/// The local pre-push gate runs the same rows as CI. A gate that is a subset of
/// CI is a gate that says "clean" and then goes red on push.
#[test]
fn the_local_gate_runs_the_same_rows_as_ci() {
    let ci = workflow("ci.yml");
    let gate = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/ci-gate.sh"),
    )
    .unwrap();

    let ci_rows: Vec<String> = ci
        .lines()
        .filter_map(|l| l.trim().strip_prefix("- \""))
        .filter_map(|l| l.split_once('"').map(|(row, _)| row.to_string()))
        .filter(|r| r.contains("--features") || r.contains("--release"))
        .collect();
    let missing: Vec<&String> = ci_rows
        .iter()
        .filter(|r| !gate.contains(r.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "scripts/ci-gate.sh is missing rows CI runs: {missing:#?}"
    );
}
