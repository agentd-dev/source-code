// SPDX-License-Identifier: AGPL-3.0-only
//! **A document's humans, as its gates address them.**
//!
//! An instruction document declares the people it involves with `::!human`
//! — a name, and a `principal` and/or a `channel` — and its workflows address
//! a `human` step to one of them with `to: "@human/<name>"` (spec §3.4). The
//! instruction crate's fold resolves that reference to ONE string, the
//! channel when there is one: that is the spec's reference output, and it is
//! not agentd's to change. But a channel (`@channel/ops`) names no principal,
//! and a gate may wait only for an operator (`Addressee::check_gate`), so the
//! folded value alone would refuse the spec's own examples.
//!
//! agentd needs both halves, so it reads them from the parsed document
//! itself rather than from the folded string:
//!
//! - **who answers**: the human's `principal` when it declares one — held to
//!   the gate rule unchanged, so `principal=operator` loads and
//!   `principal=user:alice` is refused with the usual reason — and otherwise
//!   the operators, exactly as `to: operator`;
//! - **where it is announced**: the human's `channel`, kept OUT of the
//!   definition and recorded here per workflow and step. The runtime carries
//!   it onto the gate (`human.asked`, the task's annotations), where a bridge
//!   can route the question; it never changes who may answer.
//!
//! Keeping the channel out of the definition is what keeps a `to:` written
//! anywhere else — config YAML, `workflow.create`, a model's `ask_human` —
//! principal syntax only: nothing but a document's own declaration can put a
//! channel on a gate, because only there is the channel's meaning declared.

use std::collections::BTreeMap;

use serde_json::Value;

use super::idoc::{Disposition, Document};

/// The channel each document-addressed gate is announced on: workflow name →
/// step path → the declaring human's `channel`. A top-level step's path is
/// its id; a step inside a body is `<parent>.<id>`, and inside a branch
/// `<parent>{<branch>}.<id>` — the run-record id with its `[<index>]`s
/// dropped (`runtime::nested::definition_path`), since every element of a
/// fan-out runs the one definition.
pub type GateChannels = BTreeMap<String, BTreeMap<String, String>>;

/// One `human` step a `:::!workflow` block's body wrote as
/// `to: "@human/<name>"`.
struct GateRef {
    /// The object keys from the workflow's root down to the step.
    keys: Vec<String>,
    /// The step's path, as [`GateChannels`] keys it.
    path: String,
    /// The `<name>` it addresses.
    human: String,
}

/// Every `human` step under `steps` addressed to a document human, at any
/// depth. A body or branches are descended into only for the kinds that hold
/// a nested definition there — the engine's own list (`engine::model`'s raw
/// fields), so an `http` step's request `body` is never taken for one.
fn gate_refs(steps: Option<&Value>, keys: &[String], prefix: &str, out: &mut Vec<GateRef>) {
    let Some(steps) = steps.and_then(Value::as_object) else {
        return;
    };
    for (id, s) in steps {
        let mut here = keys.to_vec();
        here.push(id.clone());
        let path = if prefix.is_empty() {
            id.clone()
        } else {
            format!("{prefix}.{id}")
        };
        let kind = s.get("kind").and_then(Value::as_str).unwrap_or_default();
        if kind == "human"
            && let Some(human) = s
                .get("to")
                .and_then(Value::as_str)
                .and_then(|t| t.strip_prefix("@human/"))
        {
            out.push(GateRef {
                keys: here.clone(),
                path: path.clone(),
                human: human.to_string(),
            });
        }
        if crate::engine::model::is_raw_field(kind, "body") {
            let mut k = here.clone();
            k.extend(["body".to_string(), "steps".to_string()]);
            gate_refs(s.get("body").and_then(|b| b.get("steps")), &k, &path, out);
        }
        if crate::engine::model::is_raw_field(kind, "branches") {
            for (branch, b) in s
                .get("branches")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                let mut k = here.clone();
                k.extend(["branches".to_string(), branch.clone(), "steps".to_string()]);
                gate_refs(b.get("steps"), &k, &format!("{path}{{{branch}}}"), out);
            }
        }
    }
}

/// Address every `human` step of the document's folded `workflows` that the
/// document wrote as `to: "@human/<name>"` to the human it declares, and
/// return the channels those gates are announced on.
///
/// The reference is read from the workflow block's own body, since the fold
/// has already replaced it at the top level. A step nested in a body or a
/// branch is addressed the same way: the fold resolves only top-level steps
/// and leaves a nested reference as written, but it is the same document's
/// reference to the same human, and a gate there waits exactly as one at the
/// top does. A reference to a human the document does not declare is left
/// as the fold left it, `@human/<name>`, for the gate rule to refuse at load.
/// A workflow name two blocks share is left alone too: which block's
/// reference a step came from is then unknowable, and the duplicate is
/// refused at load anyway.
pub fn address_document_gates(doc: &Document, workflows: &mut [Value]) -> GateChannels {
    let nonempty = |v: Option<&String>| v.filter(|s| !s.trim().is_empty()).cloned();
    // The same set the fold resolves against — top-level `human` blocks by
    // name, the last declaration winning — but with BOTH attributes kept.
    let mut humans: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    for b in doc.blocks().filter(|b| b.kind == "human") {
        if let Some(name) = &b.name {
            humans.insert(
                name.clone(),
                (
                    nonempty(b.attrs.get("principal")),
                    nonempty(b.attrs.get("channel")),
                ),
            );
        }
    }
    if humans.is_empty() {
        return GateChannels::new();
    }
    // workflow name → its references, or `None` once the name is seen twice.
    let mut refs: BTreeMap<String, Option<Vec<GateRef>>> = BTreeMap::new();
    for b in doc
        .blocks()
        .filter(|b| b.kind == "workflow" && b.disposition == Disposition::Machinery)
    {
        let body = super::yaml::parse(&b.body).unwrap_or(Value::Null);
        // The fence attribute names the workflow and wins over a body `name:`,
        // as it does in the fold.
        let Some(name) = b
            .attrs
            .get("name")
            .cloned()
            .or_else(|| body.get("name").and_then(Value::as_str).map(str::to_string))
        else {
            continue;
        };
        let mut found = Vec::new();
        gate_refs(body.get("steps"), &["steps".to_string()], "", &mut found);
        match refs.get_mut(&name) {
            Some(seen) => *seen = None,
            None => {
                refs.insert(name, Some(found));
            }
        }
    }
    let mut channels = GateChannels::new();
    for wf in workflows.iter_mut() {
        let Some(name) = wf.get("name").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let Some(Some(found)) = refs.get(&name) else {
            continue;
        };
        for r in found {
            let Some((principal, channel)) = humans.get(&r.human) else {
                continue;
            };
            let Some(step) = r
                .keys
                .iter()
                .try_fold(&mut *wf, |v, k| v.get_mut(k))
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            let to = principal.as_deref().unwrap_or("operator");
            step.insert("to".into(), Value::String(to.to_string()));
            if let Some(channel) = channel {
                channels
                    .entry(name.clone())
                    .or_default()
                    .insert(r.path.clone(), channel.clone());
            }
        }
    }
    channels
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fold(text: &str) -> (Vec<Value>, GateChannels) {
        let doc = crate::config::idoc::parse(text).expect("parses");
        let granted = crate::config::idoc::all_families();
        let mut ex = crate::config::idoc::fold(&doc, &granted).expect("folds");
        let channels = address_document_gates(&doc, &mut ex.workflows);
        (ex.workflows, channels)
    }

    fn doc(humans: &str, to: &str) -> String {
        format!(
            "---\nspec: \"1\"\n---\n:::!channel{{name=ops}}\n:::\n\n{humans}\n\n\
             :::!workflow{{name=w}}\n\
             steps:\n\
             \x20 s:   {{kind: manual}}\n\
             \x20 ask: {{kind: human, question: \"ok?\", to: \"{to}\", depends_on: [s]}}\n\
             \x20 f:   {{kind: finish, depends_on: [ask]}}\n\
             :::\n"
        )
    }

    /// A human that declares only a channel is answered by the operators and
    /// announced on the channel; one that declares a principal is addressed
    /// to it (the gate rule then decides whether it loads), its channel still
    /// announced; one that declares neither is the operators', unannounced.
    #[test]
    fn a_documents_human_addresses_its_principal_or_the_operators() {
        for (human, to, channel) in [
            (
                ":::!human{name=oncall channel=@channel/ops}\n:::",
                "operator",
                Some("@channel/ops"),
            ),
            (
                ":::!human{name=oncall principal=user:alice channel=#ops}\n:::",
                "user:alice",
                Some("#ops"),
            ),
            (
                ":::!human{name=oncall principal=operator}\n:::",
                "operator",
                None,
            ),
            (
                ":::!human{name=oncall}\nrole: approver\n:::",
                "operator",
                None,
            ),
        ] {
            let (wfs, channels) = fold(&doc(human, "@human/oncall"));
            assert_eq!(wfs[0]["steps"]["ask"]["to"], json!(to), "{human}");
            assert_eq!(
                channels
                    .get("w")
                    .and_then(|w| w.get("ask"))
                    .map(String::as_str),
                channel,
                "{human}"
            );
        }
    }

    /// A gate inside a body or a branch is the same document's reference to
    /// the same human: it is addressed like a top-level one, and its channel
    /// is recorded under the step's path — the run-record id without its
    /// element index. An `http` step's request `body` is not a definition and
    /// is never descended into.
    #[test]
    fn a_nested_gate_is_addressed_and_keyed_by_its_path() {
        let text = "---\nspec: \"1\"\n---\n:::!channel{name=ops}\n:::\n\n\
                    :::!human{name=oncall channel=@channel/ops}\n:::\n\n\
                    :::!workflow{name=w}\n\
                    steps:\n\
                    \x20 s: {kind: manual}\n\
                    \x20 each:\n\
                    \x20   kind: iterate\n\
                    \x20   max_iterations: 2\n\
                    \x20   depends_on: [s]\n\
                    \x20   body: {steps: {ask: {kind: human, question: \"ok?\", to: \"@human/oncall\"}}}\n\
                    \x20 par:\n\
                    \x20   kind: parallel\n\
                    \x20   depends_on: [each]\n\
                    \x20   branches:\n\
                    \x20     a: {steps: {ask: {kind: human, question: \"ok?\", to: \"@human/oncall\"}}}\n\
                    \x20 post:\n\
                    \x20   kind: http\n\
                    \x20   depends_on: [par]\n\
                    \x20   url: \"https://example.test\"\n\
                    \x20   body: {steps: {x: {kind: human, to: \"@human/oncall\"}}}\n\
                    \x20 f: {kind: finish, depends_on: [post]}\n\
                    :::\n";
        let (wfs, channels) = fold(text);
        let steps = &wfs[0]["steps"];
        assert_eq!(
            steps["each"]["body"]["steps"]["ask"]["to"],
            json!("operator")
        );
        assert_eq!(
            steps["par"]["branches"]["a"]["steps"]["ask"]["to"],
            json!("operator")
        );
        assert_eq!(
            steps["post"]["body"]["steps"]["x"]["to"],
            json!("@human/oncall"),
            "a request body is data, not a nested definition"
        );
        let want: BTreeMap<String, String> = [
            ("each.ask".to_string(), "@channel/ops".to_string()),
            ("par{a}.ask".to_string(), "@channel/ops".to_string()),
        ]
        .into();
        assert_eq!(channels.get("w"), Some(&want));
    }

    /// A reference to a human the document never declared is left as the
    /// fold left it, for the gate rule to refuse at load — and a `to` that is
    /// not a reference is not touched at all.
    #[test]
    fn only_a_declared_humans_reference_is_addressed() {
        let human = ":::!human{name=oncall channel=@channel/ops}\n:::";
        let (wfs, channels) = fold(&doc(human, "@human/nobody"));
        assert_eq!(wfs[0]["steps"]["ask"]["to"], json!("@human/nobody"));
        assert!(channels.is_empty(), "{channels:?}");
        let (wfs, channels) = fold(&doc(human, "#ops"));
        assert_eq!(wfs[0]["steps"]["ask"]["to"], json!("#ops"));
        assert!(channels.is_empty(), "{channels:?}");
    }
}
