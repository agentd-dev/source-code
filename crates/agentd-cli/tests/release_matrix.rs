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

/// The IMAGE ships what the binaries ship.
///
/// The container job left `FEATURES` to the Dockerfile's own default, and the
/// default stopped matching `release.yml`: `sign`, `oci` and `decrypt` reached
/// the standalone binaries and never reached the image, so
/// `ghcr.io/agentd-dev/agentd` could not verify an instruction signature the
/// release notes said it could. Two artifacts of the same release must have the
/// same capabilities.
#[test]
fn the_image_is_built_with_the_same_features_as_the_binaries() {
    let want = release_features();
    let dockerfile =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Dockerfile"))
            .unwrap();
    let line = dockerfile
        .lines()
        .find(|l| l.starts_with("ARG FEATURES="))
        .expect("the Dockerfile declares ARG FEATURES");
    let mut have: Vec<String> = line
        .split_once('=')
        .unwrap()
        .1
        .trim()
        .trim_matches('"')
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let mut want_sorted = want.clone();
    have.sort();
    want_sorted.sort();
    assert_eq!(
        have, want_sorted,
        "the Dockerfile default and release.yml FEATURES disagree — a local \
         `docker build .` would not produce the released image"
    );

    // …and the workflow passes it rather than trusting the default to stay
    // right, which is how it drifted in the first place.
    let release = workflow("release.yml");
    assert!(
        release.contains("FEATURES=${{ env.FEATURES }}"),
        "the container job must pass FEATURES explicitly as a build-arg"
    );
}

/// Every place that WRITES OUT the shipped feature set writes the same one.
///
/// The list is copied into the README, the deployment guide, the architecture
/// note and the Dockerfile's own header. Every one of those copies was a
/// release behind. A reader who follows any of them builds something other
/// than what ships.
#[test]
fn every_documented_copy_of_the_feature_set_is_current() {
    let want = release_features().join(",");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut stale = Vec::new();
    for f in [
        "README.md",
        "Dockerfile",
        "docs/deployment.md",
        "docs/architecture.md",
    ] {
        let text = std::fs::read_to_string(root.join(f)).unwrap();
        for (n, line) in text.lines().enumerate() {
            // A list naming `aauth` alongside `oauth` is claiming to BE the
            // shipped set. Deliberate subsets (the Dockerfile's
            // `--build-arg FEATURES=a2a,metrics,cron,otel` example) name
            // neither, and are left alone.
            let mut rest = line;
            while let Some(i) = rest.find("a2a,metrics") {
                let run: String = rest[i..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == ',' || *c == '-')
                    .collect();
                if run.contains("aauth") && run.contains("oauth") && run != want {
                    stale.push(format!("{f}:{}: {run}", n + 1));
                }
                rest = &rest[i + 3..];
            }
        }
    }
    assert!(
        stale.is_empty(),
        "these name the shipped feature set and are out of date (want {want}):\n{}",
        stale.join("\n")
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
