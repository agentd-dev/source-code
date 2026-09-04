// SPDX-License-Identifier: AGPL-3.0-only
//! The **Instruction Document Spec conformance corpus**, run against the real
//! binary.
//!
//! Each fixture in `tests/instruction-spec-corpus/` is a bare instruction
//! document plus its expected OBSERVABLE outcome: does it validate, which error
//! substrings appear, and what registers (`--capabilities`). The corpus is the
//! spec's teeth — the spec is being re-homed to instruction.md as its owner
//! (CC-BY 4.0), and agentd conforms by running the corpus, not by claiming to.
//! Dialect-1 behaviour pinned here is CONTRACT: a change that fails a fixture
//! is a spec change, not a refactor.
#![cfg(all(unix, feature = "workflow"))]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn run_case(doc_path: &Path) -> (bool, String, Value) {
    let doc = std::fs::read_to_string(doc_path).unwrap();
    let cfg = serde_json::json!({
        "config_version": "1",
        "agent": {"name": "conf", "preflight": "never", "instruction": doc},
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
    });
    let cfg_path = std::env::temp_dir().join(format!(
        "spec-corpus-{}-{}.json",
        std::process::id(),
        doc_path.file_stem().unwrap().to_string_lossy()
    ));
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let v = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", cfg_path.to_str().unwrap(), "--validate-config"])
        .output()
        .unwrap();
    let valid = v.status.success();
    let errtext = format!(
        "{}{}",
        String::from_utf8_lossy(&v.stdout),
        String::from_utf8_lossy(&v.stderr)
    );
    let caps = if valid {
        let c = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["-c", cfg_path.to_str().unwrap(), "--capabilities"])
            .output()
            .unwrap();
        serde_json::from_slice(&c.stdout).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let _ = std::fs::remove_file(&cfg_path);
    (valid, errtext, caps)
}

#[test]
fn the_conformance_corpus_passes_against_this_binary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/instruction-spec-corpus");
    let mut cases = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .expect("corpus directory exists")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    dirs.sort_by_key(|e| e.file_name());
    for dir in dirs {
        let mut docs: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(".instruction.md"))
            })
            .collect();
        docs.sort();
        for doc in docs {
            cases += 1;
            let name = doc.file_stem().unwrap().to_string_lossy().to_string();
            let exp_path = doc.with_file_name(
                doc.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .replace(".instruction.md", ".expected.json"),
            );
            let exp: Value =
                serde_json::from_str(&std::fs::read_to_string(&exp_path).unwrap()).unwrap();
            // A fixture pins the spec dialect it is written against; one
            // declaring a dialect this implementation does not speak is
            // SKIPPED, not failed — dialect-2 fixtures may enter the shared
            // corpus without failing dialect-1 runtimes (the runtime's own
            // refusal of dialect-2 DOCUMENTS is separately pinned by the
            // forward-compat guard's tests).
            if let Some(spec) = exp["spec"].as_str()
                && spec != "1"
            {
                eprintln!("  skip {name} (spec {spec}; this implementation speaks 1)");
                continue;
            }
            let (valid, errtext, caps) = run_case(&doc);
            if Some(valid) != exp["valid"].as_bool() {
                failures.push(format!(
                    "{name}: valid={valid}, expected {} — said: {}",
                    exp["valid"],
                    errtext.trim()
                ));
                continue;
            }
            for needle in exp["errors"].as_array().into_iter().flatten() {
                let needle = needle.as_str().unwrap();
                if !errtext.contains(needle) {
                    failures.push(format!("{name}: error text missing {needle:?}: {errtext}"));
                }
            }
            if let Some(want) = exp["registers"]["workflows"].as_array() {
                let got: Vec<&str> = caps["workflows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|w| w["name"].as_str())
                    .collect();
                let want: Vec<&str> = want.iter().filter_map(Value::as_str).collect();
                if got != want {
                    failures.push(format!("{name}: workflows={got:?}, expected {want:?}"));
                }
            }
            if let Some(want) = exp["registers"]["mcp_servers"].as_array()
                && caps["mcp_servers"].as_array() != Some(want)
            {
                failures.push(format!(
                    "{name}: mcp_servers={}, expected {want:?}",
                    caps["mcp_servers"]
                ));
            }
        }
    }
    assert!(cases >= 6, "the corpus is present ({cases} cases found)");
    assert!(
        failures.is_empty(),
        "{} corpus failures:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// The spec repo's per-version registry, checked AGAINST the reference
/// implementation: its spec-1 entry must equal the parser's own closed set.
/// A registry that drifts from the code it describes is the schema-vs-loader
/// failure all over again, so the equality is asserted, not assumed.
#[test]
fn the_registry_spec_1_entry_matches_the_shipped_closed_set() {
    let reg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/instruction-spec-corpus/registry/kinds.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let v1 = &reg["versions"]["1"];
    let mut registry_set: Vec<&str> = ["machinery", "prose", "structural"]
        .iter()
        .flat_map(|k| v1[*k].as_array().into_iter().flatten())
        .filter_map(serde_json::Value::as_str)
        .collect();
    registry_set.sort_unstable();
    let mut known: Vec<&str> = agentd::config::directives::known_kinds().to_vec();
    known.sort_unstable();
    assert_eq!(
        registry_set, known,
        "the spec registry's dialect-1 closed set drifted from the parser"
    );
}

/// Drift check against the spec repo, when it is reachable: every vendored
/// fixture document and the registry must be byte-identical to upstream.
/// Skips cleanly where upstream is absent (CI, until the repo has a URL) —
/// the behavioural fixtures above still run there.
#[test]
fn vendored_corpus_matches_upstream_when_present() {
    let upstream = std::env::var("INSTRUCTION_SPEC_REPO")
        .unwrap_or_else(|_| "/root/instruction-md/spec".into());
    let up = Path::new(&upstream).join("conformance");
    if !up.exists() {
        eprintln!("upstream spec repo not present; drift check skipped");
        return;
    }
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/instruction-spec-corpus");
    let mut checked = 0usize;
    for rel in std::fs::read_dir(up.join("core"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".instruction.md"))
        })
    {
        let name = rel.file_name().unwrap();
        let ours = local.join("core").join(name);
        assert_eq!(
            std::fs::read(&rel).unwrap(),
            std::fs::read(&ours).unwrap_or_default(),
            "vendored fixture {name:?} drifted from upstream"
        );
        checked += 1;
    }
    assert_eq!(
        std::fs::read(up.join("registry/kinds.json")).unwrap(),
        std::fs::read(local.join("registry/kinds.json")).unwrap(),
        "vendored registry drifted from upstream"
    );
    assert!(
        checked >= 6,
        "upstream corpus present but near-empty ({checked})"
    );
}
