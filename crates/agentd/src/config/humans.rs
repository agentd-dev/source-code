// SPDX-License-Identifier: AGPL-3.0-only
//! **A document's people, as its gates address them.**
//!
//! An instruction document declares the people it involves with `::person`
//! (or a deployment-bound `::role`), and its workflows address a `human`
//! step to one of them by id: `to: nataliia`, or `to: "#nataliia"`. A person
//! in instruction/1 is an identity and a label, not a principal: what binds
//! one to an account is the host's (§10, §19). agentd's gate rule is that a
//! gate waits for an operator (`Addressee::check_gate`), so a step addressed
//! to a person the document declares is answered by the operators, exactly
//! as `to: operator` — the person's name stays in the question the agent
//! reads, and a bridge announcing the gate can route it.
//!
//! A `to` that names nobody the document declares is left as written, for the
//! gate rule to refuse at load.
//!
//! Where a gate is announced is the operator's: `agent.gate_channels` (how a
//! subagent template's child gets its parent's), never a document's.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// The channel each gate is announced on: workflow name → step path →
/// channel. A top-level step's path is its id; a step inside a body is
/// `<parent>.<id>`, and inside a branch `<parent>{<branch>}.<id>` — the
/// run-record id with its `[<index>]`s dropped
/// (`runtime::nested::definition_path`), since every element of a fan-out
/// runs the one definition.
pub type GateChannels = BTreeMap<String, BTreeMap<String, String>>;

/// Address every `human` step under `steps`, at any depth, whose `to` names
/// one of `people`. A body or branches are descended into only for the kinds
/// that hold a nested definition there — the engine's own list
/// (`engine::model`'s raw fields), so an `http` step's request `body` is
/// never taken for one.
fn address(steps: Option<&mut Value>, people: &BTreeSet<String>) {
    let Some(steps) = steps.and_then(Value::as_object_mut) else {
        return;
    };
    for s in steps.values_mut() {
        let kind = s
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if kind == "human"
            && let Some(to) = s.get("to").and_then(Value::as_str)
            && people.contains(to.strip_prefix('#').unwrap_or(to))
        {
            s["to"] = Value::String("operator".into());
        }
        if crate::engine::model::is_raw_field(&kind, "body") {
            address(s.get_mut("body").and_then(|b| b.get_mut("steps")), people);
        }
        if crate::engine::model::is_raw_field(&kind, "branches")
            && let Some(branches) = s.get_mut("branches").and_then(Value::as_object_mut)
        {
            for b in branches.values_mut() {
                address(b.get_mut("steps"), people);
            }
        }
    }
}

/// Address the `human` steps of a document's `workflows` that name one of
/// the people it declares to the operators.
pub fn address_document_gates(people: &BTreeSet<String>, workflows: &mut [Value]) {
    if people.is_empty() {
        return;
    }
    for wf in workflows.iter_mut() {
        address(wf.get_mut("steps"), people);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fold(text: &str) -> Vec<Value> {
        let mut ex =
            crate::config::idoc::extract(text, &crate::config::idoc::all_families()).unwrap();
        address_document_gates(&ex.people, &mut ex.workflows);
        ex.workflows
    }

    fn doc(people: &str, to: &str) -> String {
        format!(
            "---\nspec: \"1\"\n---\n{people}\n\n\
             :::workflow{{id=w dialect=\"instruction/workflow\"}}\n\
             name: w\n\
             steps:\n\
             \x20 s:   {{kind: manual}}\n\
             \x20 ask: {{kind: human, question: \"ok?\", to: \"{to}\", depends_on: [s]}}\n\
             \x20 f:   {{kind: finish, depends_on: [ask]}}\n\
             :::\n"
        )
    }

    /// A person or a role the document declares is answered by the
    /// operators, by its bare id or its `#id`.
    #[test]
    fn a_documents_person_is_answered_by_the_operators() {
        for (people, to) in [
            ("::person{id=nataliia title=\"Nataliia\"}", "nataliia"),
            ("::person{id=nataliia title=\"Nataliia\"}", "#nataliia"),
            ("::role{id=oncall title=\"On call\"}", "oncall"),
        ] {
            let wfs = fold(&doc(people, to));
            assert_eq!(
                wfs[0]["steps"]["ask"]["to"],
                json!("operator"),
                "{people} {to}"
            );
        }
    }

    /// A gate inside a body or a branch is the same document's reference to
    /// the same person; an `http` step's request `body` is data.
    #[test]
    fn a_nested_gate_is_addressed() {
        let text = "::person{id=oncall title=\"On call\"}\n\n\
                    :::workflow{id=w dialect=\"instruction/workflow\"}\n\
                    name: w\n\
                    steps:\n\
                    \x20 s: {kind: manual}\n\
                    \x20 each:\n\
                    \x20   kind: iterate\n\
                    \x20   max_iterations: 2\n\
                    \x20   depends_on: [s]\n\
                    \x20   body: {steps: {ask: {kind: human, question: \"ok?\", to: oncall}}}\n\
                    \x20 par:\n\
                    \x20   kind: parallel\n\
                    \x20   depends_on: [each]\n\
                    \x20   branches:\n\
                    \x20     a: {steps: {ask: {kind: human, question: \"ok?\", to: oncall}}}\n\
                    \x20 post:\n\
                    \x20   kind: http\n\
                    \x20   depends_on: [par]\n\
                    \x20   url: \"https://example.test\"\n\
                    \x20   body: {steps: {x: {kind: human, to: oncall}}}\n\
                    \x20 f: {kind: finish, depends_on: [post]}\n\
                    :::\n";
        let wfs = fold(text);
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
            json!("oncall"),
            "a request body is data, not a nested definition"
        );
    }

    /// A `to` naming nobody the document declares is left for the gate rule.
    #[test]
    fn only_a_declared_person_is_addressed() {
        let wfs = fold(&doc("::person{id=nataliia title=\"Nataliia\"}", "nobody"));
        assert_eq!(wfs[0]["steps"]["ask"]["to"], json!("nobody"));
    }
}
