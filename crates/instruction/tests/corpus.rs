// SPDX-License-Identifier: AGPL-3.0-only
//! **The shared fixture corpus** (E1-06): the same cases the TypeScript twin
//! (`@instruction-md/spec`) runs, so the two implementations cannot drift.
//! Each case is a directory `{doc.md, context.json, delivered.txt}` (plus,
//! when present, `tree.json` and `refusals.json`); `context.json` carries
//! `{"params": {...}, "facts": {...}}`, and includes resolve by front-matter
//! `id` among the corpus documents themselves.
//!
//! The corpus is VENDORED at `tests/conformance/` (Apache-2.0, copied from
//! the specification repo) for the same reason `instruction.schema.json` is:
//! a contract that only runs where someone happens to have a sibling checkout
//! does not run. It was skipping silently in CI — reporting `ok` on every
//! hosted runner — which is how a suite reports health it never performed.
//! `the_vendored_corpus_matches_upstream_when_present` is the drift check.

use std::collections::BTreeMap;
use std::path::Path;

use instruction_core::{Context, deliver, parse, tree_json};

// The spec repo's conformance corpus is THE contract (proposals/S5); the
// twin's packages/spec/fixtures mirrors it.
/// The vendored corpus — always present, so these tests always run.
const DEFAULT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/conformance/corpus");
const DEFAULT_REFUSALS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/conformance/refusals");

#[test]
fn the_shared_corpus_delivers_byte_exactly() {
    let explicit = std::env::var("INSTRUCTION_FIXTURES");
    let root = explicit.clone().unwrap_or_else(|_| DEFAULT.to_string());
    let root = Path::new(&root);
    assert!(
        root.exists(),
        "corpus missing at {root:?} — it is vendored in-tree and must be present; \
         a skipped conformance run is not a passing one"
    );

    // Build the include resolver: front-matter `id` → document text, over the
    // whole corpus.
    let mut by_id: BTreeMap<String, String> = BTreeMap::new();
    let mut cases: Vec<std::path::PathBuf> = Vec::new();
    for entry in std::fs::read_dir(root).unwrap().filter_map(|e| e.ok()) {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let doc_path = dir.join("doc.md");
        if !doc_path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&doc_path).unwrap();
        if let Ok(d) = parse(&text)
            && let Some(id) = d.front.get("id").and_then(|v| v.as_str())
        {
            by_id.insert(id.to_string(), text.clone());
            // Also index by the short id (`ins_x` and bare `x`).
            if let Some(short) = id.rsplit('/').next() {
                by_id.insert(short.to_string(), text.clone());
                if let Some(bare) = short.strip_prefix("ins_") {
                    by_id.insert(bare.to_string(), text);
                }
            }
        }
        cases.push(dir);
    }
    cases.sort();
    assert!(!cases.is_empty(), "corpus present but empty at {root:?}");

    let resolver = |id: &str| by_id.get(id).cloned();
    let mut failures = Vec::new();
    let mut ran = 0;
    for dir in &cases {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(dir.join("doc.md")).unwrap();
        let ctx_path = dir.join("context.json");
        let ctx_v: serde_json::Value = if ctx_path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&ctx_path).unwrap()).unwrap()
        } else {
            serde_json::json!({})
        };
        let map_of = |key: &str| -> BTreeMap<String, String> {
            ctx_v[key]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        };
        let ctx = Context {
            grants: instruction_core::doc::all_families(),
            params: map_of("params"),
            facts: map_of("facts"),
            resolve_include: Some(&resolver),
        };

        let doc = match parse(&text) {
            Ok(d) => d,
            Err(errs) => {
                // A refusals-only case: compare against refusals.json if present.
                if dir.join("refusals.json").exists() {
                    ran += 1;
                    let want: serde_json::Value = serde_json::from_str(
                        &std::fs::read_to_string(dir.join("refusals.json")).unwrap(),
                    )
                    .unwrap();
                    let got = serde_json::to_value(&errs).unwrap();
                    if got != want {
                        failures.push(format!(
                            "{name}: refusals differ\n got: {got}\nwant: {want}"
                        ));
                    }
                } else {
                    failures.push(format!(
                        "{name}: refused but no refusals.json:\n  {}",
                        errs.iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("\n  ")
                    ));
                }
                continue;
            }
        };

        if dir.join("delivered.txt").exists() {
            ran += 1;
            let want = std::fs::read_to_string(dir.join("delivered.txt")).unwrap();
            match deliver(&doc, &ctx) {
                Ok(out) if out.text == want => {}
                Ok(out) => failures.push(format!(
                    "{name}: delivered text differs (got {} bytes, want {})\n--- first diff ---\n{}",
                    out.text.len(),
                    want.len(),
                    first_diff(&want, &out.text)
                )),
                Err(errs) => failures.push(format!(
                    "{name}: delivery refused: {}",
                    errs.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
                )),
            }
        }
        if dir.join("tree.json").exists() {
            ran += 1;
            let want: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("tree.json")).unwrap())
                    .unwrap();
            let got = tree_json(&doc);
            if got != want {
                failures.push(format!("{name}: tree.json differs"));
            }
        }
    }
    assert!(ran > 0, "corpus present but no comparable artifacts ran");
    assert!(
        failures.is_empty(),
        "{} corpus failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!(
        "shared corpus: {} cases, {ran} artifact comparisons, all exact",
        cases.len()
    );
}

fn first_diff(want: &str, got: &str) -> String {
    for (i, (w, g)) in want.lines().zip(got.lines()).enumerate() {
        if w != g {
            return format!("line {}:\nwant: {w}\n got: {g}", i + 1);
        }
    }
    format!(
        "line count differs: want {} got {}",
        want.lines().count(),
        got.lines().count()
    )
}

#[test]
fn the_shared_refusal_corpus_matches() {
    let explicit = std::env::var("INSTRUCTION_REFUSALS");
    let root = explicit
        .clone()
        .unwrap_or_else(|_| DEFAULT_REFUSALS.to_string());
    let root = Path::new(&root);
    assert!(
        root.exists(),
        "refusal corpus missing at {root:?} — it is vendored in-tree and must be present"
    );
    let mut cases: Vec<std::path::PathBuf> = std::fs::read_dir(root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("doc.md").exists() && p.join("refusals.json").exists())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "refusal corpus present but empty");
    let mut failures = Vec::new();
    for dir in &cases {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(dir.join("doc.md")).unwrap();
        let want: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("refusals.json")).unwrap())
                .unwrap();
        let got: Vec<instruction_core::Refusal> = match parse(&text) {
            Err(errs) => errs,
            Ok(d) => instruction_core::validate(
                &d,
                &Context {
                    grants: instruction_core::doc::all_families(),
                    ..Context::default()
                },
            ),
        };
        // The matching rule both runners use: message equality, and line
        // equality when the fixture's line is not null.
        let mut msgs = Vec::new();
        for w in &want {
            let wmsg = w["message"].as_str().unwrap_or("");
            let wline = w["line"].as_u64();
            let hit = got.iter().any(|g| {
                g.message_body() == wmsg && (wline.is_none() || g.line.map(u64::from) == wline)
            });
            if !hit {
                msgs.push(format!(
                    "  want {:?} (line {:?}); got: {}",
                    wmsg,
                    wline,
                    got.iter()
                        .map(|g| format!("[{:?}] {:?}", g.line, g.message_body()))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
        }
        if !msgs.is_empty() {
            failures.push(format!("{name}:\n{}", msgs.join("\n")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} refusal-corpus failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("refusal corpus: {} cases, all matched", cases.len());
}

/// The vendored corpus must equal the specification's, when a checkout of it
/// is at hand. Same contract as the vendored schema: vendoring buys "it always
/// runs", and this buys "it is still the spec's corpus and not a local fork".
#[test]
fn the_vendored_corpus_matches_upstream_when_present() {
    let explicit = std::env::var("INSTRUCTION_SPEC_REPO");
    let upstream = explicit
        .clone()
        .unwrap_or_else(|_| "/root/instruction-md/specification".into());
    let up = Path::new(&upstream).join("conformance");
    if !up.exists() {
        assert!(
            explicit.is_err(),
            "INSTRUCTION_SPEC_REPO={upstream:?} was set but has no conformance/ — \
             fail, not skip"
        );
        eprintln!("no upstream checkout; drift check skipped");
        return;
    }
    let vendored = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/conformance"));
    let mut differences = Vec::new();
    for sub in ["corpus", "refusals"] {
        let (u, v) = (up.join(sub), vendored.join(sub));
        let list = |root: &Path| -> BTreeMap<String, Vec<u8>> {
            let mut out = BTreeMap::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if let Ok(bytes) = std::fs::read(&p)
                        && let Ok(rel) = p.strip_prefix(root)
                    {
                        out.insert(rel.to_string_lossy().into_owned(), bytes);
                    }
                }
            }
            out
        };
        let (uf, vf) = (list(&u), list(&v));
        for (name, bytes) in &uf {
            match vf.get(name) {
                None => differences.push(format!("{sub}/{name}: upstream case is not vendored")),
                Some(mine) if mine != bytes => {
                    differences.push(format!("{sub}/{name}: vendored copy differs from upstream"))
                }
                _ => {}
            }
        }
        for name in vf.keys() {
            if !uf.contains_key(name) {
                differences.push(format!("{sub}/{name}: vendored case is not upstream"));
            }
        }
    }
    assert!(
        differences.is_empty(),
        "the vendored conformance corpus has drifted from {upstream}:\n  {}\n\
         re-vendor with: cp -r {upstream}/conformance/{{corpus,refusals}} \
         crates/instruction/tests/conformance/",
        differences.join("\n  ")
    );
}
