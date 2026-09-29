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
/// step id → the declaring human's `channel`.
pub type GateChannels = BTreeMap<String, BTreeMap<String, String>>;

/// Address every top-level `human` step of the document's folded
/// `workflows` that the document wrote as `to: "@human/<name>"` to the human
/// it declares, and return the channels those gates are announced on.
///
/// The reference is read from the workflow block's own body, since the fold
/// has already replaced it. Only what the fold itself resolves is touched —
/// top-level steps of a `:::!workflow` — and a reference to a human the
/// document does not declare is left as the fold left it, `@human/<name>`, for
/// the gate rule to refuse at load. A workflow name two blocks share is left
/// alone too: which block's reference a step came from is then unknowable,
/// and the duplicate is refused at load anyway.
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
    // workflow name → [(step id, human name)], or `None` once the name is
    // seen twice.
    let mut refs: BTreeMap<String, Option<Vec<(String, String)>>> = BTreeMap::new();
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
        let found: Vec<(String, String)> = body
            .get("steps")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(_, s)| s.get("kind").and_then(Value::as_str) == Some("human"))
            .filter_map(|(id, s)| {
                let human = s.get("to")?.as_str()?.strip_prefix("@human/")?;
                Some((id.clone(), human.to_string()))
            })
            .collect();
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
        let Some(steps) = wf.get_mut("steps").and_then(Value::as_object_mut) else {
            continue;
        };
        for (id, human) in found {
            let (Some((principal, channel)), Some(step)) = (
                humans.get(human),
                steps.get_mut(id).and_then(Value::as_object_mut),
            ) else {
                continue;
            };
            let to = principal.as_deref().unwrap_or("operator");
            step.insert("to".into(), Value::String(to.to_string()));
            if let Some(channel) = channel {
                channels
                    .entry(name.clone())
                    .or_default()
                    .insert(id.clone(), channel.clone());
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
