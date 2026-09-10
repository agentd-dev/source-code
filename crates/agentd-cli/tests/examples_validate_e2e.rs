// SPDX-License-Identifier: AGPL-3.0-only
//! **The shipped examples still load.**
//!
//! `examples/` is documentation people copy. When a config setting is renamed
//! — and this project renames rather than deprecating — an example written
//! against the old spelling stops working, silently, until somebody runs it.
//! Two shipped examples had rotted that way (`instruction_sources:` after it
//! moved under `agent.instruction.trust`, and `--instruction-file` in the
//! runner scripts and the systemd unit), and nothing in the tree noticed.
//!
//! So: every example that IS an agentd config goes through `--validate-config`,
//! the same authority startup runs, and every shipped script and unit file is
//! checked for a flag the CLI now refuses.
#![cfg(unix)]

use std::path::{Path, PathBuf};
#[cfg(all(feature = "cel", feature = "sign"))]
use std::process::Command;

fn examples_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// Every `.yaml`/`.yml` under `examples/`, recursively.
#[cfg(all(feature = "cel", feature = "sign"))]
fn yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            yaml_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "yaml" || e == "yml") {
            out.push(p);
        }
    }
}

/// Every `{{secret:NAME}}` an example references.
#[cfg(all(feature = "cel", feature = "sign"))]
fn secret_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("{{secret:") {
        rest = &rest[i + "{{secret:".len()..];
        if let Some(end) = rest.find("}}") {
            out.push(rest[..end].trim().to_string());
            rest = &rest[end..];
        }
    }
    out
}

/// The examples are written against the RELEASE build. Two use a CEL `when:`
/// and one carries a `trust` pin, so a build without `cel` or `sign` refuses
/// them for a reason that has nothing to do with whether the example is
/// current. This asks its question only on a build that can answer it — if a
/// future example needs another feature, it says so by failing here.
#[test]
#[cfg(all(feature = "cel", feature = "sign"))]
fn every_shipped_example_config_validates() {
    let mut files = Vec::new();
    yaml_files(&examples_root(), &mut files);
    assert!(!files.is_empty(), "no examples found — wrong path?");

    let mut checked = 0;
    let mut failures = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        // An agentd config declares its version. Kubernetes manifests and
        // standalone workflow documents live here too and are not ours to load.
        if !text.lines().any(|l| l.starts_with("config_version:")) {
            continue;
        }
        // A `services.yaml` beside it is a shared catalog, not a config: the
        // desks that use it are meant to be run as `-c services.yaml -c
        // <desk>.yaml`, and validating a desk alone reports every service it
        // references as undeclared. Layer it exactly as the README does.
        let catalog = f.with_file_name("services.yaml");
        let layered = catalog.exists() && catalog != *f;
        if !layered && f.file_name().is_some_and(|n| n == "services.yaml") {
            continue; // the catalog on its own declares no agent
        }
        checked += 1;
        // An example that references a secret is CORRECT — that is the
        // idiom. Supply each one so validation gets past the environment and
        // on to the question this test asks: does the config still parse, and
        // are its keys still keys?
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
        cmd.arg("--validate-config");
        if layered {
            cmd.arg("-c").arg(&catalog);
        }
        cmd.arg("-c").arg(f);
        for name in secret_names(&text).into_iter().chain(
            layered
                .then(|| std::fs::read_to_string(&catalog).unwrap_or_default())
                .map(|t| secret_names(&t))
                .unwrap_or_default(),
        ) {
            cmd.env(name, "test-value");
        }
        let out = cmd.output().unwrap();
        if !out.status.success() {
            failures.push(format!(
                "{}\n{}",
                f.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    assert!(
        checked >= 5,
        "only {checked} example configs found — wrong path?"
    );
    assert!(
        failures.is_empty(),
        "shipped example configs no longer load:\n\n{}",
        failures.join("\n\n")
    );
}

/// The scripts and unit files people copy invoke the CLI directly, so a removed
/// flag breaks them exactly as a removed config key breaks a YAML example —
/// and `--validate-config` cannot see them.
#[test]
fn no_shipped_script_or_unit_uses_a_removed_flag() {
    // The WHOLE table, not a subset. This once covered two of the sixteen
    // spellings, with a comment explaining that shipped scripts still invoked
    // several of the rest — which is a note that the check does not check,
    // and three runner scripts stayed broken behind it. Reading the authority
    // means a flag retired tomorrow is covered tomorrow.
    let removed: Vec<&str> = agentd::config::v2::REMOVED_FLAGS
        .iter()
        .map(|(f, _)| *f)
        .collect();

    let root = examples_root();
    let mut files = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    walk(&root, &mut files);
    walk(&root.join("../packaging"), &mut files);
    // The benchmark harness drives the same CLI, and rotted the same way: it
    // still spelled `--mode once` long after modes were removed, so every
    // offline run in bench/README.md's quick start exited 2.
    walk(&root.join("../bench"), &mut files);

    let mut hits = Vec::new();
    for f in &files {
        // Prose may NAME a removed flag — a migration note saying it is gone is
        // correct and useful. Only things that get executed are scanned.
        if f.extension().is_some_and(|e| e == "md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            // A comment explaining the migration is fine; an invocation is not.
            if line.trim_start().starts_with('#') {
                continue;
            }
            for flag in &removed {
                // A token boundary, not a substring: `--model` contains
                // `--mode`, and `--claim` prefixes `--claim-ttl`. Matching
                // loosely reported three false positives on the first run.
                if line
                    .split(|c: char| c.is_whitespace() || c == '=')
                    .any(|t| t.trim_matches(|c| c == '"' || c == '\'' || c == '\\') == *flag)
                {
                    hits.push(format!("{}:{}: {}", f.display(), n + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "shipped files invoke flags the CLI refuses:\n{}",
        hits.join("\n")
    );
}
