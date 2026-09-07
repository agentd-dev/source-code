// SPDX-License-Identifier: AGPL-3.0-only
//! **What am I actually running?** — the effective configuration, and where
//! each setting came from.
//!
//! A running config is assembled from more places every release: up to three
//! discovery rungs, the process environment, the flags, the conventional
//! `workflows/`, `skills/` and `subagents/` folders, and a `:::!config`
//! fragment inside an instruction that may itself have come from a folder, an
//! OCI artifact or a URL. `--validate-config` answers "is this valid"; nothing
//! answered "what is it, and who said so".
//!
//! The provenance is computed by SNAPSHOTTING the document at each layer
//! boundary as `load` assembles it, then attributing every leaf to the last
//! layer that changed it. Snapshots are taken only when the report was asked
//! for, and they are taken inside the real loader rather than by replaying its
//! rules here — a second implementation of the layering would be a second set
//! of answers, and the wrong one would be this module's.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// What replaces a credential-shaped value in the report.
const REDACTED: &str = "<redacted>";

/// The document as it stood after each layer, in the order they applied.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    snaps: Vec<(String, Value)>,
}

impl Trace {
    /// Take a snapshot, labelled with the layer that just applied.
    pub fn record(&mut self, label: impl Into<String>, doc: &Value) {
        self.snaps.push((label.into(), doc.clone()));
    }

    /// Whether anything was recorded — a `load` that short-circuits (a usage
    /// error, `--help`) records nothing, and an empty report says so rather
    /// than implying the config was empty.
    pub fn is_empty(&self) -> bool {
        self.snaps.is_empty()
    }
}

/// Every leaf path of a document, in `a.b.c` form. An ARRAY is a leaf: a
/// config's arrays (`workflows`, `mcp.servers`, endpoint lists) are replaced
/// wholesale by a later layer rather than merged into, so attributing their
/// elements separately would describe a merge that never happens.
fn leaves(v: &Value, prefix: &str, out: &mut BTreeMap<String, Value>) {
    match v {
        Value::Object(map) if !map.is_empty() => {
            for (k, child) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                leaves(child, &path, out);
            }
        }
        other => {
            if !prefix.is_empty() {
                out.insert(prefix.to_string(), other.clone());
            }
        }
    }
}

/// Attribute every leaf of the final document to the last layer that changed
/// it. A value a later layer restated identically is credited to the layer
/// that FIRST said it — restating a value changes nothing, and naming the
/// restater would send an operator to edit the wrong file.
pub fn provenance(trace: &Trace) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some((_, last)) = trace.snaps.last() else {
        return out;
    };
    let mut final_leaves = BTreeMap::new();
    leaves(last, "", &mut final_leaves);

    let mut previous: BTreeMap<String, Value> = BTreeMap::new();
    let mut credited: BTreeMap<String, String> = BTreeMap::new();
    for (label, doc) in &trace.snaps {
        let mut current = BTreeMap::new();
        leaves(doc, "", &mut current);
        for (path, value) in &current {
            if previous.get(path) != Some(value) {
                credited.insert(path.clone(), label.clone());
            }
        }
        previous = current;
    }
    for path in final_leaves.keys() {
        if let Some(label) = credited.get(path) {
            out.insert(path.clone(), label.clone());
        }
    }
    out
}

/// Replace credential-shaped values with a placeholder.
///
/// The config file itself may not carry a live credential — validation refuses
/// that — but an env var or a flag may, and `${VAR}` expansion happens before
/// this report is built. A `{{secret:…}}` / `{{secret-file:…}}` REFERENCE is
/// kept verbatim: it is not a credential, and which reference a setting uses
/// is one of the things an operator most needs to see.
pub fn redact(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if let Value::String(s) = child
                    && crate::config::is_secret_shaped_key(k)
                    && !crate::sec::secret::has_secret_ref(s)
                {
                    *child = Value::String(REDACTED.into());
                    continue;
                }
                redact(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// The report: the effective document, where each setting came from, and the
/// instruction-declared fragment that merges under it.
pub fn report(trace: &Trace, files: &[String], document_config: &Map<String, Value>) -> Value {
    let mut config = trace
        .snaps
        .last()
        .map(|(_, d)| d.clone())
        .unwrap_or_else(|| Value::Object(Map::new()));
    redact(&mut config);
    let mut fragment = Value::Object(document_config.clone());
    redact(&mut fragment);

    let mut notes = vec![
        "`config` is the document the loader assembled: files, then environment, \
         then flags, then the conventional folders."
            .to_string(),
        format!(
            "values under a credential-shaped key are shown as {REDACTED:?}; a \
             {{{{secret:…}}}} reference is shown as written."
        ),
    ];
    if !document_config.is_empty() {
        notes.push(
            "`document_config` is the `:::!config` fragment the instruction declared. It \
             merges UNDER `config`, so an explicit setting still wins, and it is applied \
             after this document is typed — it is not folded into `config` above."
                .to_string(),
        );
    }
    notes.push(
        "defaults are not shown: a path absent here takes the default in \
         `--config-schema`."
            .to_string(),
    );

    json!({
        "config": config,
        "provenance": provenance(trace),
        "document_config": fragment,
        "files": files,
        "notes": notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn a_leaf_is_credited_to_the_layer_that_changed_it() {
        let mut t = Trace::default();
        t.record(
            "file a.yaml",
            &doc(r#"{"agent":{"name":"one","instruction":"x"}}"#),
        );
        t.record(
            "file b.yaml",
            &doc(r#"{"agent":{"name":"two","instruction":"x"}}"#),
        );
        t.record(
            "env",
            &doc(r#"{"agent":{"name":"two","instruction":"x"},"store":{"kind":"memory"}}"#),
        );
        let p = provenance(&t);
        assert_eq!(p["agent.name"], "file b.yaml", "the layer that CHANGED it");
        assert_eq!(
            p["agent.instruction"], "file a.yaml",
            "restating a value unchanged credits the layer that first said it"
        );
        assert_eq!(p["store.kind"], "env");
    }

    /// A setting a later layer REMOVED is not in the report at all — the
    /// report describes what is running, not what was written and undone.
    #[test]
    fn a_removed_leaf_is_not_reported() {
        let mut t = Trace::default();
        t.record("file", &doc(r#"{"a":1,"b":2}"#));
        t.record("flag", &doc(r#"{"a":1}"#));
        let p = provenance(&t);
        assert!(p.contains_key("a"));
        assert!(!p.contains_key("b"), "b is gone: {p:?}");
    }

    #[test]
    fn credential_shaped_values_are_redacted_and_references_are_not() {
        let mut v = doc(
            r#"{"mcp":{"servers":[{"headers":{"authorization":"Bearer live-abc",
               "x-trace":"keep-me"}}]},
               "intelligence":{"token":"{{secret:OPENAI}}","endpoints":["https://a"]},
               "store":{"password":"hunter2"}}"#,
        );
        redact(&mut v);
        assert_eq!(v["mcp"]["servers"][0]["headers"]["authorization"], REDACTED);
        assert_eq!(
            v["mcp"]["servers"][0]["headers"]["x-trace"], "keep-me",
            "a header that is not credential-shaped is left alone"
        );
        assert_eq!(
            v["intelligence"]["token"], "{{secret:OPENAI}}",
            "a reference is not a credential"
        );
        assert_eq!(v["store"]["password"], REDACTED);
        assert_eq!(v["intelligence"]["endpoints"][0], "https://a");
    }

    #[test]
    fn an_empty_trace_reports_nothing_rather_than_an_empty_config() {
        let t = Trace::default();
        assert!(t.is_empty());
        assert_eq!(provenance(&t).len(), 0);
    }
}
