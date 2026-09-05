// SPDX-License-Identifier: AGPL-3.0-only
//! The **Instruction Document Spec conformance corpus**, run against the real
//! binary.
//!
//! Each fixture in `tests/instruction-spec-corpus/` is a bare instruction
//! document plus its expected OBSERVABLE outcome: does it validate, which error
//! substrings appear, and what registers (`--capabilities`). The corpus is the
//! spec's teeth — the spec is owned by instruction.md (CC-BY 4.0 text), and
//! agentd conforms by running the corpus, not by claiming to. The registry the
//! parser uses IS the vendored `instruction.schema.json`; behaviour
//! pinned here is CONTRACT: a change that fails a fixture is a spec change, not
//! a refactor.
#![cfg(all(unix, feature = "workflow"))]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn run_case(doc_path: &Path, grants: &[Value]) -> (bool, String, Value) {
    let doc = std::fs::read_to_string(doc_path).unwrap();
    let cfg = serde_json::json!({
        "config_version": "1",
        "agent": {
            "name": "conf", "preflight": "never", "instruction": doc,
            // Grants are declared PER FIXTURE (the corpus's `grants:` key,
            // default none). A runner that granted everything could not express
            // the fail-closed fixtures at all, leaving the trust ladder's
            // guarantee permanently untested — so the default is none.
            "document_capabilities": grants,
        },
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
    // Name the binary this result is made against — a conformance claim without
    // its version is an assertion, not evidence (the lesson of the two
    // simultaneously-true green/red reports across implementations).
    let ver = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    eprintln!("conformance corpus vs {ver:?}");
    // Extraction-presence probe: a fresh build implements it, so its absence is
    // a regression, reported as one clear line rather than every fixture failing.
    let (pv, _, pc) = run_case(&root.join("core/001-core-happy.instruction.md"), &[]);
    assert!(
        pv && !pc["workflows"].as_array().is_none_or(|a| a.is_empty()),
        "{ver} validates the probe but extracts no directives — extraction regressed"
    );
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
            let grants: Vec<Value> = exp["grants"].as_array().cloned().unwrap_or_default();
            let (valid, errtext, caps) = run_case(&doc, &grants);
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

/// The registry is the vendored JSON Schema itself, so it cannot drift from the
/// parser — but the schema states its machinery set TWICE (the flat
/// `x-registry.machinery` list, and the per-kind `$defs.kinds.*.x-disposition`),
/// and those two views must agree or the file is internally inconsistent. Since
/// the parser derives its set from `$defs`, this also proves the accessor reads
/// the file the reference implementation actually ships.
#[test]
fn the_schema_registry_agrees_with_the_parser() {
    let schema: Value = serde_json::from_str(agentd::config::idoc::schema_json()).unwrap();
    let mut listed: Vec<&str> = schema["x-registry"]["machinery"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    listed.sort_unstable();
    let mut known: Vec<&str> = agentd::config::idoc::machinery_names().collect();
    known.sort_unstable();
    assert_eq!(
        listed, known,
        "the schema's x-registry.machinery drifted from its own $defs.kinds"
    );
}

/// Drift check against the spec repo, when it is reachable: the vendored JSON
/// Schema's registry and grammar must match upstream's. Compared SEMANTICALLY
/// (the parsed `x-registry`/`x-grammar`/`$defs`), so a reformat upstream is not
/// a false alarm but a real registry change is. Skips cleanly where upstream is
/// absent (CI, before any clone); an EXPLICIT but missing path fails rather than
/// skips — a drift check that skips on a bad path reports health it never
/// performed, the vacuous-pass trap this whole effort has been hunting.
#[test]
fn the_vendored_schema_matches_upstream_when_present() {
    let explicit = std::env::var("INSTRUCTION_SPEC_REPO");
    let upstream = explicit
        .clone()
        .unwrap_or_else(|_| "/root/instruction-md/specification".into());
    let up = Path::new(&upstream).join("instruction.schema.json");
    if !up.exists() {
        assert!(
            explicit.is_err(),
            "INSTRUCTION_SPEC_REPO={upstream:?} was set but has no \
             instruction.schema.json — a drift check pointed at a \
             missing path must fail, not skip"
        );
        eprintln!("no upstream spec clone at the default path; drift check skipped");
        return;
    }
    let up_schema: Value = serde_json::from_str(&std::fs::read_to_string(&up).unwrap())
        .expect("upstream schema is valid JSON");
    let ours: Value = serde_json::from_str(agentd::config::idoc::schema_json()).unwrap();
    for key in ["x-registry", "x-grammar", "$defs"] {
        assert_eq!(
            ours[key], up_schema[key],
            "the vendored schema's {key:?} drifted from upstream — \
             re-vendor crates/agentd/src/config/instruction.schema.json"
        );
    }
}
