// SPDX-License-Identifier: MIT OR Apache-2.0
//! How a corpus case is delivered — its context and its include resolver —
//! in one place, for the two programs that deliver one: the conformance
//! runner (`tests/corpus.rs`) and the fixture dumper (`examples/dump.rs`).
//! The dumper promises to reproduce the fixture bytes the runner compares,
//! so the two must read a case the same way, not by two copies of the rule.

use std::collections::BTreeMap;
use std::path::Path;

/// The documents includes resolve among: every `<case>/doc.md` under
/// `corpus`, by its front-matter `id`, by the id's last path segment
/// (`ins_x`), and by that segment without its `ins_` prefix (`x`). A case
/// whose document does not parse, or has no `id`, resolves nothing.
pub fn documents_by_id(corpus: &Path) -> BTreeMap<String, String> {
    let mut by_id = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(corpus) else {
        return by_id;
    };
    // In name order, so two cases claiming one id resolve the same way on
    // every filesystem.
    let mut cases: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    cases.sort();
    for case in cases {
        let Ok(text) = std::fs::read_to_string(case.join("doc.md")) else {
            continue;
        };
        if let Ok(d) = instruction_core::parse(&text)
            && let Some(id) = d.front.get("id").and_then(|v| v.as_str())
        {
            by_id.insert(id.to_string(), text.clone());
            if let Some(short) = id.rsplit('/').next() {
                by_id.insert(short.to_string(), text.clone());
                if let Some(bare) = short.strip_prefix("ins_") {
                    by_id.insert(bare.to_string(), text);
                }
            }
        }
    }
    by_id
}

/// A `context.json`'s `params` and `facts` — `{"params": {…}, "facts":
/// {…}}`, string values — or none of either when there is no file.
pub fn context_of(path: &Path) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let ctx: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).expect("context.json is JSON"),
        Err(_) => serde_json::json!({}),
    };
    let map_of = |key: &str| -> BTreeMap<String, String> {
        ctx[key]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    };
    (map_of("params"), map_of("facts"))
}
