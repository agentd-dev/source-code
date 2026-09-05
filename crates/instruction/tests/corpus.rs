// SPDX-License-Identifier: AGPL-3.0-only
//! **The shared fixture corpus** (E1-06): the same cases the TypeScript twin
//! (`@instruction-md/spec`) runs, so the two implementations cannot drift.
//! Each case is a directory `{doc.md, context.json, delivered.txt}` (plus,
//! when present, `tree.json` and `refusals.json`); `context.json` carries
//! `{"params": {...}, "facts": {...}}`, and includes resolve by front-matter
//! `id` among the corpus documents themselves.
//!
//! Skips cleanly when the corpus is absent (CI without the sibling checkout);
//! an EXPLICIT `INSTRUCTION_FIXTURES` that does not exist FAILS — a drift
//! check that skips on a bad path reports health it never performed.

use std::collections::BTreeMap;
use std::path::Path;

use instruction_core::{Context, deliver, parse, tree_json};

const DEFAULT: &str = "/root/instruction-md/source-code/packages/spec/fixtures/corpus";

#[test]
fn the_shared_corpus_delivers_byte_exactly() {
    let explicit = std::env::var("INSTRUCTION_FIXTURES");
    let root = explicit.clone().unwrap_or_else(|_| DEFAULT.to_string());
    let root = Path::new(&root);
    if !root.exists() {
        assert!(
            explicit.is_err(),
            "INSTRUCTION_FIXTURES={root:?} was set but does not exist — fail, not skip"
        );
        eprintln!("fixture corpus not present; skipped");
        return;
    }

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
