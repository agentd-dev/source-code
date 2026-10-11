// SPDX-License-Identifier: AGPL-3.0-only
//! **The Instruction Document** — a re-export of the extracted
//! [`instruction_core::doc`] reference implementation (C-01), so every
//! existing `crate::config::idoc::…` call site keeps working and agentd and
//! the platform run byte-identical parser/validator/delivery code. The
//! agentd-specific configuration FOLDING (workflows splice, mcp servers,
//! tools.narrow, secret-ref rewriting) lives in the crate too — it is plain
//! serde_json shapes, not agentd types — which is what made the extraction a
//! move rather than a rewrite.

pub use instruction_core::doc::*;

use instruction_core::Refusal;
use std::collections::BTreeMap;

/// The §7.8 hard floor: a document that arrived over the wire — fetched from
/// a URL, pulled from an OCI registry, read from an MCP server — never uses
/// the families of the spec's floor (`compose`, `identity`), whatever the
/// operator granted or its author signed. Each block of one is refused,
/// naming its line; the list is the registry's, so it is the spec's.
pub fn check_wire_floor(doc: &Document) -> Result<(), Vec<Refusal>> {
    fn recur(b: &Block, floor: &[String], errs: &mut Vec<Refusal>) {
        if let Some(family) = b.grant()
            && floor.iter().any(|f| f == family)
        {
            errs.push(Refusal::at(
                b.line,
                "wire-floor",
                format!(
                    "`:::!{}` is in the `{family}` family, which a document that arrived over \
                     the wire never uses (§7.8) — declare it in the operator's configuration",
                    b.kind
                ),
            ));
        }
        for c in &b.children {
            recur(c, floor, errs);
        }
    }
    let mut errs = Vec::new();
    for b in doc.blocks() {
        recur(b, registry().wire_floor(), &mut errs);
    }
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

/// What a document's fold would give it of the floor's families through
/// blocks every document may use: `a2a.peers` written by `:::!config`, and a
/// secret named anywhere in its configuration, workflows or declarations
/// (`{{secret:…}}`, `{{secret-file:…}}`; an `@secret-ref/` folds into one) —
/// which a header or a step would then send wherever the document points
/// it. Empty when it reaches neither.
pub fn wire_floor_effects(ex: &Extraction) -> Vec<String> {
    let mut out = Vec::new();
    if ex
        .config
        .get("a2a")
        .and_then(|a| a.get("peers"))
        .is_some_and(|p| !p.is_null())
    {
        out.push("writes a2a.peers".to_string());
    }
    let names_a_secret = |v: &serde_json::Value| {
        let s = v.to_string();
        s.contains("{{secret") || s.contains("@secret-ref/")
    };
    if names_a_secret(&serde_json::Value::Object(ex.config.clone()))
        || ex.workflows.iter().any(names_a_secret)
        || ex.declarations.values().flatten().any(names_a_secret)
    {
        out.push("names a secret".to_string());
    }
    out
}

/// [`extract_with_facts`] for a document that arrived over the wire: the
/// §7.8 floor is checked before anything folds.
pub fn extract_from_wire(
    text: &str,
    granted: &std::collections::BTreeSet<String>,
    facts: &BTreeMap<String, String>,
) -> Result<Extraction, Vec<Refusal>> {
    let doc = parse(text)?;
    check_wire_floor(&doc)?;
    fold_full(&doc, granted, &BTreeMap::new(), facts, &|_| None)
}

/// The runtime FACTS agentd supplies to `when` and `unless` (§5.2 context
/// keys) — the one place they are made, so the agent's own instruction, a
/// subagent template, an OCI re-pull and a registry read are all judged by the
/// same answers.
///
/// `host` is always `agentd`: the application the agent runs in. `model` is
/// the model the agent's turns use by default, as sent to the provider, and
/// `agent` its family when [`model_family`] recognises it. With no model, or
/// an unrecognised one, the key is simply absent, and a condition on it keeps
/// its content (rule 3) — never a guess that would drop guidance. Revision 1.1
/// made `agent` the model FAMILY, so agentd no longer names itself there.
/// `environment` and `locale` are not supplied: agentd has no setting that
/// states either, and inventing one would tailor documents on a value nobody
/// chose.
pub fn instruction_facts(model: Option<&str>) -> BTreeMap<String, String> {
    let mut facts = BTreeMap::from([("host".to_string(), "agentd".to_string())]);
    if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
        if let Some(family) = model_family(model) {
            facts.insert("agent".into(), family.into());
        }
        facts.insert("model".into(), model.to_string());
    }
    facts
}

/// The model family (`claude`, `gpt`, `gemini`, `llama`, `mistral`) of a
/// model id as a provider takes it, or `None` when it is not recognisable.
///
/// Read case-insensitively from the id's last `/` segment, so a gateway's
/// `anthropic/claude-…` or a hub's `meta-llama/Llama-…` names the family its
/// model belongs to, and with Bedrock's `anthropic.` provider prefix (and the
/// `<region>.` before it) removed. `o<digit>` is OpenAI's reasoning line
/// (`o1`, `o3`, `o4-mini`); `opus` is not. A fine-tune named anything else is
/// not guessed at.
pub fn model_family(model: &str) -> Option<&'static str> {
    let id = model.trim().to_ascii_lowercase();
    let last = id.rsplit('/').next().unwrap_or(&id);
    let bare = match last.split_once("anthropic.") {
        // `anthropic.claude-…`, or one region segment before it
        // (`us.anthropic.claude-…`) — never a dot inside the region.
        Some((region, rest))
            if region.is_empty()
                || region
                    .strip_suffix('.')
                    .is_some_and(|r| !r.is_empty() && !r.contains('.')) =>
        {
            rest
        }
        _ => last,
    };
    let o_series = bare
        .strip_prefix('o')
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()));
    if bare.starts_with("claude-") {
        Some("claude")
    } else if bare.starts_with("gpt-") || o_series {
        Some("gpt")
    } else if bare.starts_with("gemini-") {
        Some("gemini")
    } else if bare.starts_with("llama") {
        Some("llama")
    } else if bare.starts_with("mistral") {
        Some("mistral")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    /// A document reaches the floor's families through ordinary blocks too:
    /// peers by `:::!config`, a credential by a secret in a header.
    #[test]
    fn the_wire_floor_sees_peers_and_secrets_an_ordinary_block_writes() {
        let all = super::all_families();
        let peers = super::extract(
            "Hi.\n\n:::!config\na2a:\n  peers:\n    - {name: p, endpoint: \"https://p.example\"}\n:::\n",
            &all,
        )
        .unwrap();
        assert_eq!(super::wire_floor_effects(&peers), ["writes a2a.peers"]);
        let secret = super::extract(
            "Hi.\n\n:::!mcp{name=s}\nendpoint: https://s.example/mcp\nheaders: {Authorization: \"Bearer {{secret:TOKEN}}\"}\n:::\n",
            &all,
        )
        .unwrap();
        assert_eq!(super::wire_floor_effects(&secret), ["names a secret"]);
        let plain = super::extract(
            "Hi.\n\n:::!mcp{name=s}\nendpoint: https://s.example/mcp\n:::\n",
            &all,
        )
        .unwrap();
        assert!(super::wire_floor_effects(&plain).is_empty());
    }

    /// The floor refuses each block in its families, naming the line, and
    /// leaves every other block alone.
    #[test]
    fn the_wire_floor_refuses_compose_and_identity_blocks() {
        let doc = super::parse(
            "Hi.\n\n:::!agent{name=r}\ntemplate: t\n:::\n\n:::!peer{name=p}\nendpoint: https://p.example\n:::\n",
        )
        .unwrap();
        let refused = super::check_wire_floor(&doc).unwrap_err();
        assert_eq!(refused.len(), 2, "{refused:?}");
        assert!(
            refused.iter().all(|r| r.code == "wire-floor"),
            "{refused:?}"
        );
        assert_eq!(refused[0].line, Some(3));
        let ok = super::parse(
            "Hi.\n\n:::!workflow{name=w}\nsteps:\n  s: {kind: once}\n  f: {kind: finish, depends_on: [s]}\n:::\n",
        )
        .unwrap();
        assert!(super::check_wire_floor(&ok).is_ok());
    }

    use super::*;

    #[test]
    fn model_family_reads_the_family_from_the_id() {
        for (id, want) in [
            ("claude-sonnet-4-5", Some("claude")),
            (
                "us.anthropic.claude-3-5-sonnet-20241022-v2:0",
                Some("claude"),
            ),
            ("anthropic.claude-v2", Some("claude")),
            ("anthropic/claude-opus-4", Some("claude")),
            ("Claude-Haiku-4-5", Some("claude")),
            ("gpt-5.1", Some("gpt")),
            ("openai/gpt-4o", Some("gpt")),
            ("o3", Some("gpt")),
            ("o4-mini", Some("gpt")),
            ("opus-x", None),
            ("gemini-2.5-pro", Some("gemini")),
            ("meta-llama/Llama-3.1-8B", Some("llama")),
            ("mistral-large", Some("mistral")),
            ("my-finetune", None),
            ("mock", None),
            // A prefix that only LOOKS like Bedrock's: two segments before
            // `anthropic.` is not a region.
            ("a.b.anthropic.claude-x", None),
            ("xanthropic.claude-x", None),
        ] {
            assert_eq!(model_family(id), want, "{id}");
        }
    }

    #[test]
    fn facts_name_the_host_always_and_the_model_when_there_is_one() {
        assert_eq!(
            instruction_facts(None),
            BTreeMap::from([("host".to_string(), "agentd".to_string())])
        );
        let f = instruction_facts(Some("claude-sonnet-4-5"));
        assert_eq!(f["host"], "agentd");
        assert_eq!(f["model"], "claude-sonnet-4-5");
        assert_eq!(f["agent"], "claude");
        // An unrecognised model is a fact; its family is not guessed.
        let f = instruction_facts(Some("my-finetune"));
        assert_eq!(f["model"], "my-finetune");
        assert!(!f.contains_key("agent"), "{f:?}");
        // `agent` is the model family in 1.1 — never agentd's own name.
        assert_eq!(instruction_facts(Some("gpt-5.1"))["agent"], "gpt");
        assert!(!instruction_facts(Some("agentd")).contains_key("agent"));
    }
}
