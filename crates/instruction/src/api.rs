// SPDX-License-Identifier: AGPL-3.0-only
//! The crate's **stable public API** — the thin, typed surface the platform
//! (publish-time validation, resolved reads, the TypeScript twin's shared
//! fixtures) programs against, over the internals in [`crate::doc`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::doc::{self, Block, BodyKind, Disposition, Document, Form};

/// One refusal: the line it names (when it names one) and the message, shaped
/// per Appendix B — the construct, and what to write instead.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Refusal {
    pub line: Option<u32>,
    pub message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The wire/display form is the full message (which already leads with
        // `line N: …` when a line is known) — byte-identical to what agentd
        // has always printed.
        f.write_str(&self.message)
    }
}

impl From<String> for Refusal {
    fn from(message: String) -> Refusal {
        let line = message
            .strip_prefix("line ")
            .and_then(|r| r.split_once(':'))
            .and_then(|(n, _)| n.trim().parse().ok());
        Refusal { line, message }
    }
}

fn refusals(errs: Vec<String>) -> Vec<Refusal> {
    errs.into_iter().map(Refusal::from).collect()
}

/// The delivery context: what this reader is entitled to and who it is.
#[derive(Default)]
pub struct Context<'a> {
    /// The operator's trust-ladder grants (`document_capabilities`).
    pub grants: BTreeSet<String>,
    /// `${parameter}` values, winning over declared defaults.
    pub params: BTreeMap<String, String>,
    /// Runtime FACTS `when` conditions evaluate against (§5.2), alongside the
    /// resolved parameters — e.g. `agent`, supplied by the consuming runtime
    /// (this library assumes none).
    pub facts: BTreeMap<String, String>,
    /// Resolves an `::include{id|uri}` to the included document's source;
    /// `None` (or a `None` return) degrades the include to its visible note.
    #[allow(clippy::type_complexity)]
    pub resolve_include: Option<&'a dyn Fn(&str) -> Option<String>>,
}

/// What delivery produces: the byte-exact §3.5 text, and the §7.4 resolution
/// manifest accounting for every input that shaped it.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub text: String,
    pub manifest: Manifest,
}

/// The §7.4 resolution manifest. The digest STRINGS are filled only when the
/// `sign` feature is built (they are empty otherwise, and a consumer that
/// needs them enables the feature).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub authored: Authored,
    #[serde(default)]
    pub parameters: Vec<Value>,
    #[serde(default)]
    pub facts: Vec<Value>,
    pub variants: Variants,
    #[serde(default)]
    pub includes: Vec<Value>,
    #[serde(default)]
    pub limits: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Authored {
    pub version: String,
    pub digest: String,
}

/// The `when` variants kept and dropped for this reader. `dropped` is REQUIRED
/// on the wire (§7.4 rule 5): a reader must be able to tell content was
/// withheld, or `when` is indistinguishable from censorship by a compromised
/// resolver.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Variants {
    #[serde(default)]
    pub kept: Vec<String>,
    pub dropped: Vec<String>,
}

/// Parse a document to its block tree, or return every problem found —
/// fail-closed and specific, nothing half-parsed.
pub fn parse(text: &str) -> Result<Document, Vec<Refusal>> {
    doc::parse(text).map_err(refusals)
}

/// Validate a parsed document for a reader with `ctx.grants`: the trust
/// ladder, and every machinery body folding the way loading would fold it.
/// Empty = valid. (Identity, references, placement and the lexical rules are
/// already enforced by [`parse`] — a `Document` in hand has passed them.)
pub fn validate(document: &Document, ctx: &Context) -> Vec<Refusal> {
    match doc::fold(document, &ctx.grants) {
        Ok(_) => Vec::new(),
        Err(errs) => refusals(errs),
    }
}

/// Run the §3.5 delivery pipeline: the byte-exact text one reader receives,
/// plus the §7.4 manifest of every input that shaped it.
pub fn deliver(document: &Document, ctx: &Context) -> Result<Delivery, Vec<Refusal>> {
    let resolve_none = |_: &str| -> Option<String> { None };
    let resolver: &dyn Fn(&str) -> Option<String> = match ctx.resolve_include {
        Some(r) => r,
        None => &resolve_none,
    };
    let ex = doc::fold_full(
        document,
        &ctx.grants,
        &ctx.params,
        &ctx.facts,
        &resolver,
        0,
        &BTreeSet::new(),
    )
    .map_err(refusals)?;
    Ok(Delivery {
        text: ex.cleaned,
        manifest: build_manifest(document, ctx, resolver),
    })
}

/// The digest of some bytes, `sha256:<hex>` (§7.2). Empty without `sign`.
pub fn digest(bytes: &[u8]) -> String {
    #[cfg(feature = "sign")]
    {
        crate::sign::digest(bytes)
    }
    #[cfg(not(feature = "sign"))]
    {
        let _ = bytes;
        String::new()
    }
}

fn build_manifest(
    document: &Document,
    ctx: &Context,
    resolver: &dyn Fn(&str) -> Option<String>,
) -> Manifest {
    let params_declared = doc::param_values(document, &ctx.params);
    let mut when_facts = params_declared.clone();
    for (k, v) in &ctx.facts {
        when_facts.insert(k.clone(), v.clone());
    }
    let mut parameters = Vec::new();
    // Every parameter that COULD have shaped the text: declared ones, plus
    // explicit overrides. Values appear as digests, never as values (§7.4).
    for (name, value) in &params_declared {
        parameters.push(json!({"name": name, "value_digest": digest(value.as_bytes())}));
    }
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut includes = Vec::new();
    for b in document.blocks() {
        match b.kind.as_str() {
            "when" => {
                let id = format!("when#{}", b.line);
                if doc::when_kept(b, &when_facts) {
                    kept.push(id);
                } else {
                    dropped.push(id);
                }
            }
            "include" => {
                if let Some(id) = b.attrs.get("id").or_else(|| b.attrs.get("uri")) {
                    let d = resolver(id)
                        .map(|text| digest(text.as_bytes()))
                        .unwrap_or_default();
                    includes.push(json!({"uri": id, "digest": d}));
                }
            }
            _ => {}
        }
    }
    Manifest {
        authored: Authored {
            version: String::new(),
            digest: authored_digest(document),
        },
        parameters,
        facts: Vec::new(),
        variants: Variants { kept, dropped },
        includes,
        limits: json!({"include_depth": 8}),
    }
}

fn authored_digest(document: &Document) -> String {
    #[cfg(feature = "sign")]
    {
        crate::sign::author_digest(document.raw.as_bytes())
    }
    #[cfg(not(feature = "sign"))]
    {
        let _ = document;
        String::new()
    }
}

/// The spec §9.1 block-tree dump — the shape the fixture corpus compares
/// implementations by. Consecutive members of one authored set are grouped
/// under a synthetic `form: "set"` node with `members`.
pub fn tree_json(document: &Document) -> Value {
    let nodes: Vec<&Block> = document.blocks().collect();
    json!({
        "spec": document.front.get("spec").cloned().unwrap_or_else(|| json!("1")),
        "frontMatter": document.front,
        "blocks": grouped_json(&nodes),
    })
}

/// Render a run of sibling blocks, grouping consecutive members of one
/// authored set under a synthetic `form: "set"` node — at the top level and
/// inside `children` alike (a `:::case[]` inside a `!test` groups the same
/// way).
fn grouped_json(nodes: &[&Block]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < nodes.len() {
        let b = nodes[i];
        if b.set_group.is_some() {
            let g = b.set_group;
            let mut members = Vec::new();
            while i < nodes.len() && nodes[i].set_group == g {
                members.push(block_json(nodes[i], true));
                i += 1;
            }
            out.push(json!({
                "kind": b.kind, "sigil": sigil_of(b), "form": "set",
                "line": b.line, "attrs": {}, "members": members,
            }));
        } else {
            out.push(block_json(b, false));
            i += 1;
        }
    }
    out
}

/// Split a code body into `(lang, inner_text)` — the surrounding ``` fence
/// lines are shape, not content, and the info string is the language.
fn split_code_fence(body: &str) -> (String, String) {
    let all: Vec<&str> = body.lines().collect();
    let Some(open) = all.iter().position(|l| {
        let t = l.trim_start();
        t.starts_with("```") || t.starts_with("~~~")
    }) else {
        return (String::new(), body.to_string());
    };
    let lines = &all[open..];
    let lang = lines[0]
        .trim_start()
        .trim_start_matches(['`', '~'])
        .trim()
        .to_string();
    // The closer is the LAST line after the opener that is nothing but fence
    // characters; an unterminated fence runs to the end.
    let end = lines
        .iter()
        .skip(1)
        .rposition(|l| {
            let t = l.trim();
            t.len() >= 3 && (t.chars().all(|c| c == '`') || t.chars().all(|c| c == '~'))
        })
        .map(|p| p + 1)
        .unwrap_or(lines.len());
    (lang, lines[1..end].join("\n"))
}

fn sigil_of(b: &Block) -> bool {
    // The sigil is a property of the kind's disposition; sub-blocks are
    // written bare even though their disposition is machinery.
    b.disposition == Disposition::Machinery
        && doc::lookup(&b.kind).is_none_or(|k| k.sub_of.is_none())
}

fn form_name(b: &Block, member: bool) -> &'static str {
    if member {
        return "member";
    }
    match b.form {
        Form::Container => "container",
        Form::Leaf => "leaf",
        Form::Set => "set",
        Form::Section => "section",
        Form::Keyword => "keyword",
        Form::Alert => "alert",
    }
}

fn block_json(b: &Block, member: bool) -> Value {
    let mut attrs = serde_json::Map::new();
    for (k, v) in &b.attrs {
        // §9.1 normalization: flags are true; multi-valued attributes are
        // arrays; everything else the (already unescaped) string.
        let val = if v.is_empty() || v == "true" {
            json!(true)
        } else if v == "false" {
            json!(false)
        } else if doc::registry().is_multivalued(&b.kind, k) {
            json!(v.split(',').map(str::trim).collect::<Vec<_>>())
        } else {
            json!(v)
        };
        attrs.insert(k.clone(), val);
    }
    let body_type = doc::lookup(&b.kind)
        .map(|k| match k.body {
            BodyKind::None => "none",
            BodyKind::Markdown => "markdown",
            BodyKind::Yaml => "yaml",
            BodyKind::Code => "code",
            BodyKind::Table => "table",
            BodyKind::Deflist => "deflist",
            BodyKind::Text => "text",
        })
        .unwrap_or("markdown");
    let mut o = serde_json::Map::new();
    o.insert("kind".into(), json!(b.kind));
    o.insert("sigil".into(), json!(sigil_of(b)));
    o.insert("form".into(), json!(form_name(b, member)));
    o.insert("line".into(), json!(b.line));
    o.insert("attrs".into(), Value::Object(attrs));
    // The §9.1 body shape per interpretation: tables carry parsed `rows`,
    // definition lists parsed `entries`, code its inner text plus `lang`.
    // A leaf (or a set member) has no body at all.
    if !(b.form == Form::Leaf || member) {
        let body = match body_type {
            "table" => json!({"type": "table", "rows": doc::table_rows(&b.body)}),
            "deflist" => json!({
                "type": "deflist",
                "entries": doc::tree_deflist_entries(&b.body)
                    .into_iter()
                    .map(|(term, definition)| json!({"term": term, "definition": definition}))
                    .collect::<Vec<_>>(),
            }),
            "code" => {
                let (lang, text) = split_code_fence(&b.body);
                json!({"type": "code", "text": text, "lang": lang})
            }
            t => json!({"type": t, "text": b.body}),
        };
        o.insert("body".into(), body);
    }
    if !b.children.is_empty() {
        let kids: Vec<&Block> = b.children.iter().collect();
        o.insert("children".into(), json!(grouped_json(&kids)));
    }
    Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\nspec: \"1\"\n---\n# Agent\n\n::param{name=env default=prod}\n\n:::when{env=\"prod\"}\nReal work.\n:::\n\n:::when{env=\"staging\"}\nRehearsal.\n:::\n\n::include{id=\"house\"}\n\n:::!workflow{name=w}\nsteps: { s: { kind: manual } }\n:::\n\nUse ${env}.\n";

    #[test]
    fn parse_validate_deliver_round_trip() {
        let d = parse(DOC).unwrap();
        let ctx = Context::default();
        assert!(validate(&d, &ctx).is_empty(), "default-rung doc validates");
        let out = deliver(&d, &ctx).unwrap();
        assert!(out.text.contains("Real work."), "{}", out.text);
        assert!(!out.text.contains("Rehearsal."), "staging variant dropped");
        assert!(out.text.contains("Use prod."), "param substituted last");
        // The manifest accounts for the variants and the (unresolved) include.
        assert_eq!(out.manifest.variants.kept, vec!["when#8".to_string()]);
        assert_eq!(out.manifest.variants.dropped.len(), 1);
        assert_eq!(out.manifest.includes[0]["uri"], "house");
        // A refusal carries its line as data.
        let errs = parse(":::workflow{name=w}\nsteps: {}\n:::").unwrap_err();
        assert_eq!(errs[0].line, Some(1));
        assert!(errs[0].message.contains("shadows a machinery name"));
    }

    #[test]
    fn when_keeps_unknown_dimensions_and_matches_comma_sets() {
        // §5.2 rules 2–3: an UNKNOWN key keeps the guidance; a known key
        // matches any of the comma-separated values.
        let d = parse(
            ":::when{agent=\"claude, gpt\"}\nFor those agents.\n:::\n\n:::when{env=\"prod\"}\nProd only.\n:::",
        )
        .unwrap();
        // No facts at all: both kept (no dimension can be evaluated).
        let out = deliver(&d, &Context::default()).unwrap();
        assert!(out.text.contains("For those agents.") && out.text.contains("Prod only."));
        // agent fact outside the set drops the first; env unknown keeps the second.
        let ctx = Context {
            facts: [("agent".to_string(), "agentd".to_string())].into(),
            ..Context::default()
        };
        let out = deliver(&d, &ctx).unwrap();
        assert!(!out.text.contains("For those agents."), "{}", out.text);
        assert!(out.text.contains("Prod only."));
        assert_eq!(out.manifest.variants.dropped.len(), 1);
        // A fact IN the comma-set keeps it.
        let ctx = Context {
            facts: [("agent".to_string(), "gpt".to_string())].into(),
            ..Context::default()
        };
        assert!(
            deliver(&d, &ctx)
                .unwrap()
                .text
                .contains("For those agents.")
        );
    }

    #[test]
    fn a_bare_attribute_value_running_into_a_brace_is_refused() {
        // §3.2: a bare value runs to whitespace or `}` — an unquoted value
        // containing `}` (e.g. an unquoted `${var}`) is a refusal, with the
        // quote-it fix named.
        let errs = parse("::!git{name=src url=https://git.example/acme/${repo} ref=${branch}}")
            .unwrap_err();
        assert!(
            errs[0].message.contains("quote values containing"),
            "{}",
            errs[0]
        );
        // Quoted, the same reference is fine.
        assert!(parse("::!git{name=src url=\"https://git.example/acme/${repo}\"}").is_ok());
    }

    #[test]
    fn keywords_and_alerts_are_tree_blocks_and_deliver_identically() {
        let d = parse(
            "MUST: run the tests.\n\n- NEVER: push to main.\n\n> [!TIP]\n> Sleep on it.\n> Then decide.\n",
        )
        .unwrap();
        let t = tree_json(&d);
        let blocks = t["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(
            (blocks[0]["kind"].as_str(), blocks[0]["form"].as_str()),
            (Some("must"), Some("keyword"))
        );
        assert_eq!(blocks[0]["body"]["text"], "run the tests.");
        assert_eq!(blocks[1]["kind"], "never");
        assert_eq!(
            (blocks[2]["kind"].as_str(), blocks[2]["form"].as_str()),
            (Some("tip"), Some("alert"))
        );
        assert_eq!(blocks[2]["body"]["text"], "Sleep on it.\nThen decide.");
        // Delivery is the same normalized text as before lifting.
        let out = deliver(&d, &Context::default()).unwrap();
        assert_eq!(
            out.text,
            "**MUST:** run the tests.\n\n- **NEVER:** push to main.\n\n**TIP:** Sleep on it.\nThen decide.\n"
        );
    }

    #[test]
    fn the_tree_dump_matches_the_9_1_shape() {
        let d = parse(
            "---\nspec: \"1\"\n---\n::!human{name=lead role=reviewer flagged}\n\n:::!human[]\n| name | role |\n|------|------|\n| a    | ops  |\n| b    | ops  |\n:::\n\n::::!mcp{name=t endpoint=https://x deny=\"a, b\"}\n:::override{target=x}\ndisabled: true\n:::\n::::\n",
        )
        .unwrap();
        let t = tree_json(&d);
        let blocks = t["blocks"].as_array().unwrap();
        // Leaf: sigil true, form leaf, flag normalized to true.
        assert_eq!(blocks[0]["form"], "leaf");
        assert_eq!(blocks[0]["sigil"], true);
        assert_eq!(blocks[0]["attrs"]["flagged"], true);
        // The set groups its members.
        assert_eq!(blocks[1]["form"], "set");
        assert_eq!(blocks[1]["members"].as_array().unwrap().len(), 2);
        assert_eq!(blocks[1]["members"][0]["form"], "member");
        // Container with a bare sub-block child; multi-valued attr is an array.
        assert_eq!(blocks[2]["form"], "container");
        assert_eq!(blocks[2]["attrs"]["deny"], json!(["a", "b"]));
        assert_eq!(blocks[2]["children"][0]["kind"], "override");
        assert_eq!(blocks[2]["children"][0]["sigil"], false);
    }
}
