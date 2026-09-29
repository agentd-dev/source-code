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
        // ci.yml writes the list twice — the matrix row and the step that
        // builds the binary the image ships — and the second copy was a
        // release behind, so the static-link gate compiled a narrower binary
        // than the one it was gating.
        ".github/workflows/ci.yml",
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

/// deny.toml is enforced, in CI and before a push. It sat unenforced from
/// v1.16.0 on, failing on licences nobody saw, because no job ran it. And
/// agentd's own AGPL is excepted per crate, never allowed: on the allow-list
/// it would admit an AGPL dependency without a word.
#[test]
fn the_supply_chain_gate_runs_in_ci_and_locally() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();

    let ci = workflow("ci.yml");
    let job = ci
        .split("\n  deny:\n")
        .nth(1)
        .expect("ci.yml has a `deny` job");
    let job = job.split("\n\n").next().unwrap_or(job);
    assert!(
        job.lines().any(|l| l.trim() == "- run: cargo deny check"),
        "ci.yml's deny job does not run `cargo deny check`:\n{job}"
    );
    assert!(
        read("scripts/ci-gate.sh")
            .lines()
            .any(|l| l.trim().starts_with("cargo deny check")),
        "scripts/ci-gate.sh does not run `cargo deny check`"
    );

    let deny = read("deny.toml");
    let allow = deny
        .split("\nallow = [")
        .nth(1)
        .and_then(|s| s.split("\n]").next())
        .expect("deny.toml has a [licenses] allow list");
    let allowed: Vec<&str> = allow
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .collect();
    assert!(
        !allowed.iter().any(|l| l.contains("AGPL")),
        "deny.toml allows AGPL for every dependency; except agentd's own crates by name instead: {allowed:?}"
    );

    // The exceptions name exactly the workspace's AGPL crates — read from the
    // workspace's own manifests, not from a list kept beside them. An
    // exception for any other crate would let an AGPL dependency in by name.
    let members: Vec<String> = read("Cargo.toml")
        .lines()
        .find_map(|l| l.trim().strip_prefix("members = ["))
        .expect("the workspace lists its members")
        .trim_end_matches(']')
        .split(',')
        .map(|m| m.trim().trim_matches('"').to_string())
        .filter(|m| !m.is_empty())
        .collect();
    assert!(members.len() > 3, "members parsed as {members:?}");
    let package = |manifest: &str, key: &str| -> String {
        manifest
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once('=')?;
                (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
            })
            .unwrap_or_else(|| panic!("no `{key}` in a member manifest"))
    };
    let mut agpl: Vec<String> = members
        .iter()
        .map(|m| read(&format!("{m}/Cargo.toml")))
        .filter(|t| package(t, "license") == "AGPL-3.0-only")
        .map(|t| package(&t, "name"))
        .collect();
    agpl.sort();
    let exceptions = deny
        .split("\nexceptions = [")
        .nth(1)
        .and_then(|s| s.split("\n]").next())
        .expect("deny.toml has [licenses] exceptions");
    let mut excepted: Vec<String> = exceptions
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .map(|l| {
            assert!(
                l.contains(r#"allow = ["AGPL-3.0-only"]"#),
                "an exception grants something other than agentd's own licence: {l}"
            );
            l.split("crate = \"")
                .nth(1)
                .and_then(|r| r.split('"').next())
                .unwrap_or_else(|| panic!("an exception names no crate: {l}"))
                .to_string()
        })
        .collect();
    excepted.sort();
    assert_eq!(
        excepted, agpl,
        "deny.toml's AGPL exceptions must name exactly the workspace's AGPL-3.0-only crates"
    );

    // The local gate judges with CI's cargo-deny, or says it does not: it
    // reads the version from ci.yml rather than keeping a copy.
    let gate = read("scripts/ci-gate.sh");
    assert!(
        gate.contains("CARGO_DENY_VERSION") && gate.contains("--version $deny_version"),
        "scripts/ci-gate.sh must read cargo-deny's version from ci.yml"
    );
}

/// The `[features]` a manifest declares, by name.
fn declared_features(manifest: &str) -> Vec<String> {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(manifest))
        .unwrap_or_else(|e| panic!("{manifest}: {e}"));
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && !t.starts_with("[\"") {
            inside = t == "[features]";
            continue;
        }
        if inside
            && let Some((name, _)) = t.split_once('=')
            && !t.starts_with('#')
            && !t.starts_with('"')
        {
            out.push(name.trim().to_string());
        }
    }
    out
}

/// The feature names each `--features <list>` in `text` passes.
fn features_named(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let mut rest = line;
        while let Some(i) = rest.find("--features") {
            rest = &rest[i + "--features".len()..];
            let Some(list) = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('=')) else {
                continue;
            };
            let list = list.trim_start_matches(['"', '\'']);
            let run: String = list
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == ',' || *c == '-' || *c == '/')
                .collect();
            for name in run.split(',').filter(|s| !s.is_empty()) {
                // `agentd/tls` forwards to the library's feature of the same name.
                let name = name.rsplit('/').next().unwrap_or(name);
                // A placeholder (`--features X`) names nothing.
                if !name.chars().any(|c| c.is_ascii_uppercase()) {
                    out.push((n + 1, name.to_string()));
                }
            }
        }
    }
    out
}

/// Every `--features` a reader is told to pass names a feature that exists.
///
/// A deleted cargo feature makes cargo refuse the whole command before it
/// builds anything, and the prose that still names it is what people copy.
/// History (the changelog, RFCs and design notes) records what was, and is
/// left alone.
#[test]
fn every_feature_a_reader_is_told_to_pass_exists() {
    let mut known = declared_features("Cargo.toml");
    known.extend(declared_features("../agentd/Cargo.toml"));
    assert!(
        known.iter().any(|f| f == "a2a"),
        "features parsed as {known:?}"
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for p in rd.filter_map(Result::ok).map(|e| e.path()) {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if p.is_dir() {
                if !matches!(name, "target" | "node_modules" | "dist" | ".git" | "design") {
                    walk(&p, out);
                }
            } else {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    for dir in [
        "docs",
        "examples",
        "bench",
        "packaging",
        "scripts",
        "crates",
        ".github",
        "skills",
        "interface",
        "web/public",
    ] {
        walk(&root.join(dir), &mut files);
    }
    for f in [
        "README.md",
        "CONTRIBUTING.md",
        "CONFORMANCE.md",
        "Dockerfile",
        "install.sh",
    ] {
        files.push(root.join(f));
    }
    let mut seen = 0;
    let mut stale = Vec::new();
    for f in &files {
        // This file spells a deleted feature on purpose, to test the scan.
        if f.ends_with(file!().rsplit('/').next().unwrap()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for (line, name) in features_named(&text) {
            seen += 1;
            if !known.contains(&name) {
                stale.push(format!("{}:{line}: {name}", f.display()));
            }
        }
    }
    assert!(seen >= 20, "only {seen} feature names found — wrong root?");
    assert!(
        stale.is_empty(),
        "these tell a reader to pass a cargo feature that does not exist:\n{}",
        stale.join("\n")
    );
}

#[test]
fn the_feature_scan_reads_each_spelling() {
    let text = "$ cargo build --features a2a,workflow\nFEATURES=x cargo b --features=\"tls,agentd/oci\"\nbuild with --features X\n";
    assert_eq!(
        features_named(text),
        vec![
            (1, "a2a".into()),
            (1, "workflow".into()),
            (2, "tls".into()),
            (2, "oci".into())
        ]
    );
}
