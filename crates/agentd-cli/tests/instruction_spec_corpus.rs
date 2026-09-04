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
