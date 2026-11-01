// SPDX-License-Identifier: AGPL-3.0-only
//! agentd's **instruction-document probes**, run against the real binary.
//!
//! Each fixture in `tests/instruction-spec-corpus/` is a bare instruction
//! document plus its expected OBSERVABLE outcome: does it validate, which error
//! substrings appear, and what registers (`--capabilities`). They are
//! agentd-owned black-box probes of the loader — the spec's own conformance
//! cases are the instruction.md SDK's, which agentd reads documents with. This
//! file also holds the checks that keep the SDK's registry the pinned
//! specification's.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn run_case(doc_path: &Path, grants: &[Value]) -> (bool, String, Value) {
    let doc = std::fs::read_to_string(doc_path).unwrap();
    let cfg = serde_json::json!({
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

/// Whether THIS test binary was built without the named cargo feature. Only
/// the features a document can need at validation are listed; any other name
/// answers `false`, so a new feature-gated refusal fails the corpus (and gets
/// an arm here) rather than being skipped unexamined.
fn lacks_feature(feature: &str) -> bool {
    match feature {
        "cron" => !cfg!(feature = "cron"),
        "cel" => !cfg!(feature = "cel"),
        "a2a" => !cfg!(feature = "a2a"),
        "tls" => !cfg!(feature = "tls"),
        _ => false,
    }
}

/// The features named when EVERY `config.invalid` line in `errtext` is a
/// "needs the '<feature>' build feature" (or unquoted) refusal for a feature this build
/// lacks; `None` when any line is another error (or there are none), so a
/// document refused for a real reason still fails.
fn only_absent_feature_refusals(errtext: &str) -> Option<Vec<String>> {
    let mut features = Vec::new();
    for line in errtext.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if ev["event"] != "config.invalid" {
            continue;
        }
        let msg = ev["msg"].as_str().or(ev["error"].as_str())?;
        // Both spellings agentd uses: "the 'cron' build feature" and
        // "needs the a2a build feature, which this binary was built without".
        let (head, _) = msg.split_once(" build feature")?;
        let (_, feature) = head.rsplit_once("the ")?;
        let feature = feature.trim_matches('\'');
        if !lacks_feature(feature) {
            return None;
        }
        features.push(feature.to_string());
    }
    (!features.is_empty()).then_some(features)
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
        "{ver} validates the probe but maps no blocks — extraction regressed"
    );
    let mut cases = 0usize;
    let mut skipped = 0usize;
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
            let grants: Vec<Value> = exp["grants"].as_array().cloned().unwrap_or_default();
            let (valid, errtext, caps) = run_case(&doc, &grants);
            // A valid document this build refuses ONLY because it uses an
            // optional capability compiled out of it (a `cron:` schedule
            // without the `cron` feature) is SKIPPED, not failed: refusing
            // it is the honest answer — accepting it would register a
            // workflow that never runs. The skip is narrow so it cannot hide
            // a real regression: every refusal line must name a feature that
            // `lacks_feature` confirms is absent from THIS build, and the
            // rows that build the feature run the fixture in full.
            if !valid
                && exp["valid"].as_bool() == Some(true)
                && let Some(features) = only_absent_feature_refusals(&errtext)
            {
                eprintln!("  skip {name} (needs build feature {features:?})");
                skipped += 1;
                continue;
            }
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
    // The feature skip must leave a corpus that still tests something: if a
    // refusal ever started naming a missing feature for most documents, this
    // run would otherwise report green over nothing.
    assert!(
        skipped * 2 < cases,
        "{skipped} of {cases} corpus cases skipped for missing build features"
    );
    assert!(
        failures.is_empty(),
        "{} corpus failures:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// agentd reads documents with the instruction.md SDK, which vendors the
/// specification's registry at one commit (`instruction_md::SPEC_COMMIT`).
/// CI checks the specification out at a pinned revision for the drift check
/// below; the two must name the same commit, or the drift check would hold
/// the SDK's registry to a specification it was never taken from.
#[test]
fn the_sdk_reads_the_pinned_specification() {
    let ci = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml"),
    )
    .expect("ci.yml is readable");
    let pin = ci
        .split("repository: instruction-md/specification")
        .nth(1)
        .and_then(|rest| rest.lines().find_map(|l| l.trim().strip_prefix("ref: ")))
        .map(str::trim)
        .expect("ci.yml pins the specification");
    let sdk = agentd::config::idoc::SPEC_COMMIT;
    assert!(
        !sdk.is_empty() && pin.starts_with(sdk),
        "ci.yml pins the specification at {pin}, but the SDK's registry is from {sdk}"
    );
}

/// Drift check against the spec repo, when it is reachable: the registry the
/// SDK reads by must be the specification's `registry.json` at the pinned
/// revision — the WHOLE parsed document, compared semantically, so a
/// reformat upstream is not a false alarm but any real change is. Skips
/// cleanly where upstream is absent (a machine with no spec clone); CI checks
/// out the pinned revision and `scripts/ci-gate.sh` refuses a run that
/// skipped. An EXPLICIT but missing path fails rather than skips — a drift
/// check that skips on a bad path reports health it never performed.
#[test]
fn the_vendored_schema_matches_upstream_when_present() {
    let explicit = std::env::var("INSTRUCTION_SPEC_REPO");
    let upstream = explicit
        .clone()
        .unwrap_or_else(|_| "/root/instruction-md/specification".into());
    let up = Path::new(&upstream).join("registry.json");
    if !up.exists() {
        assert!(
            explicit.is_err(),
            "INSTRUCTION_SPEC_REPO={upstream:?} was set but has no registry.json — a drift \
             check pointed at a missing path must fail, not skip"
        );
        eprintln!("no upstream spec clone at the default path; drift check skipped");
        return;
    }
    let up_registry: Value = serde_json::from_str(&std::fs::read_to_string(&up).unwrap())
        .expect("upstream registry is valid JSON");
    let ours: Value = serde_json::from_str(agentd::config::idoc::registry_json()).unwrap();
    // Name the top-level keys that differ, so a failure says where to look.
    let keys = |v: &Value| -> std::collections::BTreeSet<String> {
        v.as_object()
            .into_iter()
            .flat_map(|m| m.keys().cloned())
            .collect()
    };
    let drifted: Vec<String> = keys(&ours)
        .union(&keys(&up_registry))
        .filter(|k| ours.get(k.as_str()) != up_registry.get(k.as_str()))
        .cloned()
        .collect();
    assert!(
        drifted.is_empty() && ours == up_registry,
        "the SDK's registry drifted from {upstream} at {drifted:?} — move the instruction-md \
         dependency to an SDK revision vendored from this specification revision, or move \
         ci.yml's pin"
    );
    eprintln!("registry drift check: identical to {upstream}");
}
