// SPDX-License-Identifier: AGPL-3.0-only
//! **The Instruction Document** — the reference implementation of
//! the [Instruction Document Specification](https://github.com/instruction-md/specification).
//!
//! One Markdown file defines the whole agent. This module is the parser and the
//! model: it turns the document into a tree of typed blocks, classifies each by
//! disposition (prose degrades into what the model reads; machinery folds into
//! configuration and is stripped; structural resolves away), enforces the
//! lexical rules (`!` marks machinery; bare names are prose; a bare name that
//! shadows a machinery name is refused), resolves `@kind/name` references, and
//! gates each block family behind the operator's `document_capabilities` grant.
//!
//! The current spec version (1, the sigiled dialect) is the only one. A document
//! pinning a newer version is refused rather than mis-parsed.
//!
//! What this module does NOT do is execute anything. A `!function` becomes a
//! code-registered tool bound to a runtime *service*; a `!git` names a git MCP
//! server; a `!runtime` names an OCI service. agentd links no language runtime,
//! container engine, or vector store — every executing block dispatches through
//! the service catalogue, which is what keeps the dependency moat intact.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::Refusal;

/// How a block reaches (or does not reach) the model at delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Degrades into the delivered text the model reads (`note`, `must`, …).
    Prose,
    /// Stripped from delivery, folded into configuration, acknowledged by one
    /// line (`!workflow`, `!mcp`, …). Carries the `!` sigil.
    Machinery,
    /// Resolved away at delivery, producing neither config nor prose (`when`,
    /// `include`).
    Structural,
}

/// A way of writing a block (§4). Every form maps to the same kind, with the
/// same disposition, family, grant and identity rule — a form adds no meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Container,
    Leaf,
    Set,
    Section,
    Keyword,
    Alert,
}

/// How a kind's body is interpreted (§4.1's body-interpretation table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    None,
    Markdown,
    Yaml,
    Code,
    Table,
    Deflist,
    Text,
}

/// One kind's metadata, read verbatim from the spec's own JSON Schema. The
/// registry is not transcribed into Rust — it is the vendored schema, so a
/// kind, form, body or grant cannot drift from the specification: there is one
/// copy, and it is the normative one.
pub struct Kind {
    pub name: String,
    pub disposition: Disposition,
    pub forms: Vec<Form>,
    pub body: BodyKind,
    /// `x-identity`: the block HAS an identity when it is named — its `name`
    /// is unique per kind and `@kind/name` can address it. It does not say a
    /// name is required: from registry revision 1.1 every prose kind carries
    /// it, and an anonymous `MUST:` line is still the commonest block there is.
    pub identity: bool,
    /// Whether the schema REQUIRES a `name` (`then.properties.attrs.required`
    /// lists it): params, most machinery, `case` and `eval`. This, not
    /// `identity`, is what refuses an unnamed block.
    pub requires_name: bool,
    /// The capability family this machinery belongs to (`None` for prose and
    /// structural). Presentational: the grant is `grant`, not this.
    pub family: Option<String>,
    /// The `document_capabilities` token that must be granted, or `None` for
    /// the default rung (`x-grant: default`). Note `!data` and `!override`
    /// carry a family but sit on the default rung — the grant, not the family,
    /// governs. Preserved so grant-checking keys on the spec's own field.
    pub grant: Option<String>,
    /// For a sub-block, the parent kind it is valid inside; `None` for a
    /// top-level block. A sub-block has no document-level identity and is
    /// exempt from the uniqueness rule.
    pub sub_of: Option<String>,
    /// The one provenance line a machinery block delivers (spec `x-acknowledgement`).
    pub ack: Option<String>,
    /// The singular/plural nouns a set of this kind is acknowledged by
    /// (`x-noun`/`x-nouns`): `[3 human roles are declared: …]`.
    pub noun: Option<String>,
    pub nouns: Option<String>,
    /// `x-alias-of`: the kind this one is another spelling of (`always` →
    /// `must`, `avoid` → `should`, `info` → `note`). The tree keeps the
    /// alias as authored; delivery labels it as its canonical kind.
    pub alias_of: Option<String>,
    /// `x-acknowledgement-trigger`: the acknowledgement a block delivers when
    /// it carries a trigger (S11, `!skill`'s "use it when …").
    pub ack_trigger: Option<String>,
    /// `x-body-schema`: what the body is expected to hold, a JSON Schema or a
    /// URL naming one. Informative only (S22): a reader never refuses a
    /// document by it, so it is kept as data and nothing here reads it.
    pub body_schema: Option<Value>,
}

/// What the schema constrains an attribute's value to (`$defs.attrs.<kind>.
/// properties.<attr>`): an `enum` of admissible values, a `pattern`, or both.
/// Loaded here, enforced at validation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttrRule {
    pub values: Option<Vec<String>>,
    pub pattern: Option<String>,
}

/// The registry as loaded from the vendored JSON Schema — every kind, plus the
/// document-level tables (default-grant set, keyword→kind map, spec version).
pub struct Registry {
    kinds: BTreeMap<String, Kind>,
    grant_tokens: BTreeSet<String>,
    keywords: BTreeMap<String, String>,
    /// The keywords, longest first: the order `x-grammar.keywordLine`'s
    /// alternation tries them in, so `MUST NOT:` is never read as `MUST`
    /// followed by text.
    keywords_longest_first: Vec<String>,
    /// keyword → the attributes its `x-registry.keyword-flags` set true
    /// (`SHOULD NOT` → `not`): the keyword maps to the same kind as its
    /// positive form, so the flag is the only thing that keeps the rule's
    /// polarity.
    keyword_flags: BTreeMap<String, BTreeSet<String>>,
    /// kind → its delivered label (`x-registry.labels`; `always` → `MUST`).
    labels: BTreeMap<String, String>,
    /// kind → the label a NEGATED block of it delivers (`should` → `SHOULD
    /// NOT`), from `x-registry.negated-labels`.
    negated_labels: BTreeMap<String, String>,
    /// rule kind → its strength (`x-registry.strengths`). A kind listed here
    /// is a RULE: it can carry a reason (S14) and override another (S24).
    strengths: BTreeMap<String, u32>,
    /// The keyword that opens a rule's reason (`BECAUSE`).
    reason_keyword: String,
    /// The context keys a host may supply as facts (`agent`, `model`, …).
    context_keys: Vec<String>,
    /// The delivery label styles a document may choose (`bold`, …).
    label_styles: Vec<String>,
    /// The names a BARE block may not take (§3.3; S23): machinery that was in
    /// version 1. Machinery registered after it (`eval`) is not reserved.
    reserved_bare: BTreeSet<String>,
    /// sigil → the URI schemes a cross-document reference with it may use
    /// (`@` → `principal`, `agent`).
    sigils: BTreeMap<String, Vec<String>>,
    /// The grants never admissible in a document that arrived over the wire.
    wire_floor: Vec<String>,
    /// kind → the attribute names the schema marks `x-multivalued`
    /// (comma-separated within one value; normalized to arrays).
    multivalued: BTreeMap<String, BTreeSet<String>>,
    /// (kind, attribute) → the values or pattern its schema allows.
    attr_rules: BTreeMap<(String, String), AttrRule>,
    /// The kinds whose attributes the schema requires at least one of
    /// (`minProperties`): a `when` or an `unless` with no condition.
    needs_attrs: BTreeSet<String>,
    /// The machinery names in the SCHEMA's order (refusals cite the first few).
    machinery_order: Vec<String>,
    /// kind → the attribute names its schema declares.
    attrs: BTreeMap<String, BTreeSet<String>>,
    version: u32,
    /// The registry revision (`1.1`) — the version of the tables, where
    /// `version` is the document format's.
    revision: String,
}

/// The Instruction Document Specification's registry and grammar, vendored
/// verbatim from `github.com/instruction-md/specification`. It is the single
/// source of truth: the parser reads kinds, forms, bodies and grants from it.
const SCHEMA_JSON: &str = include_str!("instruction.schema.json");

static REGISTRY: std::sync::LazyLock<Registry> = std::sync::LazyLock::new(Registry::load);

/// The loaded registry (`&'static`, parsed once from the vendored schema).
pub fn registry() -> &'static Registry {
    &REGISTRY
}

/// The vendored Instruction Document JSON Schema, verbatim. The conformance
/// suite compares this against upstream to prove the vendor is faithful.
pub fn schema_json() -> &'static str {
    SCHEMA_JSON
}

/// Whether an instruction carries any Instruction Document block — a container
/// or set fence, a section heading, or a sigiled/structural leaf. This is what
/// the loader keys on to decide whether to run extraction: a document written
/// entirely in leaf or section form (no `:::` line at all) must still be
/// recognized, or its machinery is silently delivered as prose. A fence inside
/// an author note is not a block (§3.3 rule 11): a commented-out `:::!workflow`
/// is never parsed.
pub fn contains_blocks(text: &str) -> bool {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if note_opens(line) {
            match note_end(&lines, i, lines.len()) {
                Some(end) => i = end + 1,
                None => return false,
            }
            continue;
        }
        if open_fence(line).is_some()
            || section_open(line).is_some()
            || leaf_open(line).is_some_and(|lf| {
                lf.sigil || lookup(&lf.kind).is_some_and(|k| k.disposition != Disposition::Prose)
            })
        {
            return true;
        }
        i += 1;
    }
    false
}

impl Registry {
    fn load() -> Registry {
        let schema: Value = serde_json::from_str(SCHEMA_JSON)
            .expect("the vendored instruction schema is valid JSON");
        let reg = &schema["x-registry"];
        let version = reg["version"].as_u64().unwrap_or(1) as u32;
        let grant_tokens: BTreeSet<String> = reg["grants"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.keys().cloned())
            .filter(|g| g != "default")
            .collect();
        let mut keywords = BTreeMap::new();
        if let Some(m) = reg["keywords"].as_object() {
            for (kw, kind) in m {
                if let Some(k) = kind.as_str() {
                    keywords.insert(kw.clone(), k.to_string());
                }
            }
        }
        let mut keywords_longest_first: Vec<String> = keywords.keys().cloned().collect();
        keywords_longest_first.sort_by_key(|k| std::cmp::Reverse(k.len()));
        let keyword_flags: BTreeMap<String, BTreeSet<String>> = reg["keyword-flags"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(kw, flags)| {
                let set = flags
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(_, v)| v.as_bool() == Some(true))
                    .map(|(f, _)| f.clone())
                    .collect();
                (kw.clone(), set)
            })
            .collect();
        let str_map = |key: &str| -> BTreeMap<String, String> {
            reg[key]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        };
        let str_list = |v: &Value| -> Vec<String> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        };
        let labels = str_map("labels");
        let negated_labels = str_map("negated-labels");
        let strengths: BTreeMap<String, u32> = reg["strengths"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.clone(), u32::try_from(v.as_u64()?).ok()?)))
            .collect();
        let sigils: BTreeMap<String, Vec<String>> = reg["sigils"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(s, schemes)| (s.clone(), str_list(schemes)))
            .collect();
        let mut multivalued: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut attr_names: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut attr_rules: BTreeMap<(String, String), AttrRule> = BTreeMap::new();
        let mut needs_attrs = BTreeSet::new();
        if let Some(attrs) = schema["$defs"]["attrs"].as_object() {
            for (kind, spec) in attrs {
                if spec["minProperties"].as_u64().is_some_and(|n| n > 0) {
                    needs_attrs.insert(kind.clone());
                }
                if let Some(props) = spec["properties"].as_object() {
                    for (attr, a) in props {
                        attr_names
                            .entry(kind.clone())
                            .or_default()
                            .insert(attr.clone());
                        if a["x-multivalued"].as_bool() == Some(true) {
                            multivalued
                                .entry(kind.clone())
                                .or_default()
                                .insert(attr.clone());
                        }
                        let rule = AttrRule {
                            values: a.get("enum").map(str_list),
                            pattern: a["pattern"].as_str().map(str::to_string),
                        };
                        if rule != AttrRule::default() {
                            attr_rules.insert((kind.clone(), attr.clone()), rule);
                        }
                    }
                }
            }
        }
        let machinery_order: Vec<String> = reg["machinery"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let mut kinds = BTreeMap::new();
        let defs = schema["$defs"]["kinds"]
            .as_object()
            .expect("the schema carries $defs.kinds");
        for (name, d) in defs {
            let disposition = match d["x-disposition"].as_str() {
                Some("machinery") => Disposition::Machinery,
                Some("structural") => Disposition::Structural,
                _ => Disposition::Prose,
            };
            let forms = d["x-forms"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| match f.as_str() {
                    Some("container") => Some(Form::Container),
                    Some("leaf") => Some(Form::Leaf),
                    Some("set") => Some(Form::Set),
                    Some("section") => Some(Form::Section),
                    Some("keyword") => Some(Form::Keyword),
                    Some("alert") => Some(Form::Alert),
                    _ => None,
                })
                .collect();
            let body = match d["x-body"].as_str() {
                Some("yaml") => BodyKind::Yaml,
                Some("code") => BodyKind::Code,
                Some("table") => BodyKind::Table,
                Some("deflist") => BodyKind::Deflist,
                Some("text") => BodyKind::Text,
                Some("none") => BodyKind::None,
                _ => BodyKind::Markdown,
            };
            let grant = match d["x-grant"].as_str() {
                Some("default") | None => None,
                Some(g) => Some(g.to_string()),
            };
            kinds.insert(
                name.clone(),
                Kind {
                    name: name.clone(),
                    disposition,
                    forms,
                    body,
                    identity: d["x-identity"].as_bool().unwrap_or(false),
                    requires_name: d["then"]["properties"]["attrs"]["required"]
                        .as_array()
                        .is_some_and(|r| r.iter().any(|a| a == "name")),
                    family: d["x-family"].as_str().map(str::to_string),
                    grant,
                    sub_of: d["x-parent"].as_str().map(str::to_string),
                    ack: d["x-acknowledgement"].as_str().map(str::to_string),
                    noun: d["x-noun"].as_str().map(str::to_string),
                    nouns: d["x-nouns"].as_str().map(str::to_string),
                    alias_of: d["x-alias-of"].as_str().map(str::to_string),
                    ack_trigger: d["x-acknowledgement-trigger"].as_str().map(str::to_string),
                    body_schema: d.get("x-body-schema").cloned(),
                },
            );
        }
        Registry {
            kinds,
            grant_tokens,
            keywords,
            keywords_longest_first,
            keyword_flags,
            labels,
            negated_labels,
            strengths,
            reason_keyword: reg["reason-keyword"]
                .as_str()
                .expect("the schema names its reason keyword")
                .to_string(),
            context_keys: str_list(&reg["context-keys"]),
            label_styles: str_list(&reg["label-styles"]),
            reserved_bare: str_list(&reg["reserved-bare"]).into_iter().collect(),
            sigils,
            wire_floor: str_list(&reg["wire-floor"]),
            multivalued,
            attr_rules,
            needs_attrs,
            machinery_order,
            attrs: attr_names,
            version,
            revision: reg["revision"].as_str().unwrap_or_default().to_string(),
        }
    }

    /// The spec version this registry describes (currently 1).
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The registry revision the vendored schema carries (`1.1`).
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// The prose kind a keyword line introduces (`MUST` → `must`), if any.
    pub fn keyword_kind(&self, kw: &str) -> Option<&str> {
        self.keywords.get(kw).map(String::as_str)
    }

    /// Every keyword, longest first — the order a keyword line is matched in.
    pub fn keywords_longest_first(&self) -> &[String] {
        &self.keywords_longest_first
    }

    /// The attributes the keyword `kw` sets true on its block (`SHOULD NOT`
    /// → `not`), per `x-registry.keyword-flags`.
    pub fn keyword_flags(&self, kw: &str) -> impl Iterator<Item = &str> {
        self.keyword_flags
            .get(kw)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    /// The label a negated block of `kind` delivers (`should` → `SHOULD NOT`),
    /// if the registry gives it one.
    pub fn negated_label(&self, kind: &str) -> Option<&str> {
        self.negated_labels.get(kind).map(String::as_str)
    }

    /// Whether the keyword `kw` negates its kind (`SHOULD NOT`), per the
    /// registry's keyword flags.
    pub fn keyword_negates(&self, kw: &str) -> bool {
        self.keyword_flags(kw).any(|f| f == "not")
    }

    /// The label a block of `kind` delivers: the negated label when the block
    /// carries the `not` flag (S8), else the kind's own label, else its
    /// canonical kind's (an alias delivers what it aliases).
    pub fn label<'a>(&'a self, kind: &str, not: bool) -> Option<&'a str> {
        let canonical = self.kinds.get(kind).and_then(|k| k.alias_of.as_deref());
        let pick = |table: &'a BTreeMap<String, String>| -> Option<&'a str> {
            table
                .get(kind)
                .or_else(|| canonical.and_then(|c| table.get(c)))
                .map(String::as_str)
        };
        if not && let Some(l) = pick(&self.negated_labels) {
            return Some(l);
        }
        pick(&self.labels)
    }

    /// A rule kind's strength (`guardrail` 4 … `may` 1); `None` for a kind
    /// that is not a rule.
    pub fn strength(&self, kind: &str) -> Option<u32> {
        self.strengths.get(kind).copied()
    }

    /// Whether `kind` is a rule — a kind the strengths table ranks. Only a
    /// rule takes a reason (S14).
    pub fn is_rule(&self, kind: &str) -> bool {
        self.strengths.contains_key(kind)
    }

    /// The keyword that opens a reason (`BECAUSE`).
    pub fn reason_keyword(&self) -> &str {
        &self.reason_keyword
    }

    /// The context keys a host may supply as facts.
    pub fn context_keys(&self) -> &[String] {
        &self.context_keys
    }

    /// The delivery label styles a document may choose.
    pub fn label_styles(&self) -> &[String] {
        &self.label_styles
    }

    /// Whether a BARE block may not take `name` (version-1 machinery).
    pub fn reserved_bare(&self, name: &str) -> bool {
        self.reserved_bare.contains(name)
    }

    /// sigil → the URI schemes a reference written with it may use.
    pub fn sigils(&self) -> &BTreeMap<String, Vec<String>> {
        &self.sigils
    }

    /// The grants never admissible in a document that arrived over the wire.
    pub fn wire_floor(&self) -> &[String] {
        &self.wire_floor
    }

    /// The values or pattern the schema allows for `kind`'s attribute `attr`,
    /// if it constrains them.
    pub fn attr_rule(&self, kind: &str, attr: &str) -> Option<&AttrRule> {
        self.attr_rules.get(&(kind.to_string(), attr.to_string()))
    }

    /// Whether the schema requires `kind` to carry at least one attribute
    /// (`minProperties`): the variant kinds, whose attributes are their
    /// conditions.
    pub fn needs_attrs(&self, kind: &str) -> bool {
        self.needs_attrs.contains(kind)
    }

    /// The attribute names `kind`'s schema declares, if it declares any.
    pub fn attrs_of(&self, kind: &str) -> Option<&BTreeSet<String>> {
        self.attrs.get(kind)
    }

    /// The machinery names in the schema's own order (refusals cite them).
    pub fn machinery_in_order(&self) -> &[String] {
        &self.machinery_order
    }

    /// The PRIMARY keyword spelling for a kind (`must` → `MUST`), if the
    /// keyword table maps one — the reverse of [`Registry::keyword_kind`],
    /// preferring the entry that is the kind's own name upper-cased.
    pub fn keyword_kind_reverse(&self, kind: &str) -> Option<&str> {
        let upper = kind.to_uppercase();
        if self.keywords.get(&upper).map(String::as_str) == Some(kind) {
            return self.keywords.get_key_value(&upper).map(|(k, _)| k.as_str());
        }
        self.keywords
            .iter()
            .find(|(_, v)| v.as_str() == kind)
            .map(|(k, _)| k.as_str())
    }

    /// Whether the schema marks `kind`'s attribute `attr` multi-valued.
    pub fn is_multivalued(&self, kind: &str, attr: &str) -> bool {
        self.multivalued
            .get(kind)
            .is_some_and(|set| set.contains(attr))
    }
}

/// One kind's metadata by name.
pub fn lookup(name: &str) -> Option<&'static Kind> {
    registry().kinds.get(name)
}

/// The top-level machinery names, sigiled in every form. Which of them a
/// bare block may not take is [`reserved_bare_names`].
pub fn machinery_names() -> impl Iterator<Item = &'static str> {
    registry()
        .kinds
        .values()
        .filter(|k| k.disposition == Disposition::Machinery && k.sub_of.is_none())
        .map(|k| k.name.as_str())
}

/// The names a BARE block may not take (§3.3 rule 3; S23): the machinery of
/// version 1, refused bare. Machinery registered later (`eval`) is not among
/// them, so a version-1 document that wrote `:::eval` as prose stays prose.
pub fn reserved_bare_names() -> impl Iterator<Item = &'static str> {
    registry().reserved_bare.iter().map(String::as_str)
}

/// The full grant set — every capability family. Used for operator-authored
/// surfaces that are fully trusted (a subagent template's own instruction),
/// where the trust ladder's per-family gate does not apply.
pub fn all_families() -> BTreeSet<String> {
    registry().grant_tokens.clone()
}

/// The grant a kind requires, or `None` for the default rung. Keys on the
/// spec's `x-grant`, so `!data`/`!override` correctly need no grant.
pub fn grant_of(kind: &str) -> Option<&'static str> {
    lookup(kind).and_then(|k| k.grant.as_deref())
}

/// Whether a kind accepts a given form (§4; the schema's `x-forms`).
pub fn accepts_form(kind: &str, form: Form) -> bool {
    lookup(kind).is_some_and(|k| k.forms.contains(&form))
}

/// A parsed block: its kind, identity, attributes, body text, and — because
/// blocks nest by fence length — its child blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub kind: String,
    pub disposition: Disposition,
    pub name: Option<String>,
    pub attrs: BTreeMap<String, String>,
    /// Body text with child blocks removed (they live in `children`).
    pub body: String,
    pub children: Vec<Block>,
    pub line: usize,
    /// For a member expanded from a set (`:::kind[]`), an id shared by that
    /// set's members, so delivery can acknowledge the whole set in one line.
    /// `None` for a block written on its own.
    pub set_group: Option<u64>,
    /// The form this block was AUTHORED in (§4). A set member carries
    /// `Form::Set` plus its `set_group`; the §9.1 dump renders it as "member".
    pub form: Form,
    /// The body WITH lifted keyword/alert lines still in position — what
    /// delivery and the skill catalogue read (§3.5 normalizes those lines in
    /// place). `None` means nothing was lifted and `body` is the whole story.
    /// The tree (§9.1) always reads `body`, which excludes lifted children.
    pub raw_body: Option<String>,
    /// The block's line region in the document body (0-based, inclusive), set
    /// for TOP-LEVEL blocks by the walk. Delivery replaces exactly this region
    /// with the block's delivered form, leaving every other line untouched
    /// (§3.5 layout). `(0, 0)` on children, which delivery never addresses.
    /// A rule's region covers the reason that follows it.
    pub region: (usize, usize),
    /// The lines of the rule's `BECAUSE:` paragraph (S14; 0-based body
    /// lines, inclusive), when one follows it. Its text is `attrs.because`.
    pub reason: Option<(usize, usize)>,
    /// The author notes in this block's own body (S9; 0-based body lines,
    /// inclusive), not those of its children. Their lines stay in `body`, as
    /// the §9.1 tree keeps them, and are recorded for delivery to strip.
    pub notes: Vec<(usize, usize)>,
}

impl Block {
    /// The body text delivery reads: lifted keyword/alert lines re-included
    /// in position when any were lifted, else `body` itself.
    pub fn delivery_body(&self) -> &str {
        self.raw_body.as_deref().unwrap_or(&self.body)
    }

    /// The capability family this block belongs to, or `None` for prose,
    /// structural, and default-rung machinery.
    pub fn family(&self) -> Option<&'static str> {
        lookup(&self.kind).and_then(|k| k.family.as_deref())
    }

    /// The grant this block requires, or `None` for the default rung.
    pub fn grant(&self) -> Option<&'static str> {
        grant_of(&self.kind)
    }
}

/// A top-level document node, in source order: a run of prose text, an
/// author note, an inert block, or a block. Delivery walks these so the words
/// BETWEEN blocks are preserved.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Text(String),
    /// An author note (S9; §3.3 rule 11): body lines `start..=end` (0-based),
    /// from a column-0 `<!--` to the first line holding `-->`. Never scanned
    /// for blocks, references or keywords. Delivery still passes its lines
    /// through as text, as it did before notes were read; the 1.1 delivery
    /// strips them.
    Note {
        start: usize,
        end: usize,
    },
    /// An inert block (§3.3 rule 2; S23): an unknown bare name, or machinery
    /// registered after version 1 written bare (`:::eval`). It is no block
    /// and not in the tree; its body, read raw, is prose. `region` is its
    /// lines fences included (0-based, inclusive), `line` its opener.
    Inert {
        kind: String,
        line: usize,
        region: (usize, usize),
        body: String,
    },
    Block(Block),
}

/// A parsed document: front matter, and the top-level nodes (prose and blocks)
/// in source order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Document {
    pub front: BTreeMap<String, Value>,
    /// Whether the document opens with front matter at all — what tells an
    /// absent block from an empty one, which the §9.1 tree keeps apart.
    pub has_front_matter: bool,
    pub nodes: Vec<Node>,
    /// The end matter (S27; §3.1.1), when the document ends with one: its
    /// record — owners, approvals, changelog — which never reaches the model.
    pub end_matter: Option<BTreeMap<String, Value>>,
    /// The body the blocks were read from: the source with front matter and
    /// end matter stripped. Block regions index its lines.
    pub source: String,
    /// The original text verbatim (front and end matter included) — what an
    /// authored digest is computed over.
    pub raw: String,
}

impl Document {
    /// The top-level blocks, in order — a view over the block nodes.
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.nodes.iter().filter_map(|n| match n {
            Node::Block(b) => Some(b),
            Node::Text(_) | Node::Note { .. } | Node::Inert { .. } => None,
        })
    }
}

/// Parse a document to its node tree, or return every problem found.
///
/// Fail-closed and specific: each error names the line and what to write
/// instead. Nothing is half-parsed — a document with any error yields no tree.
pub fn parse(text: &str) -> Result<Document, Vec<Refusal>> {
    let mut errs = Vec::new();
    let (front, body_start) = parse_front_matter(text, &mut errs);
    let base = body_start_line(text, body_start);
    // End matter (S27) is the document's record, not its text: it is cut from
    // what the block scanner reads, so nothing in it is a block, a keyword or
    // a reference. Malformed end matter is refused, and still ends the body
    // where it opens.
    let (body, end_matter) = match end_matter_span(text, base) {
        Some(span) => {
            let fields = match span.fields {
                Ok(m) => Some(m),
                Err(r) => {
                    errs.push(r);
                    None
                }
            };
            (&text[body_start..span.before.len()], fields)
        }
        None => (&text[body_start..], None),
    };
    let lines: Vec<&str> = body.split('\n').collect();

    // Top-level walk that interleaves text runs with blocks, so delivery keeps
    // the prose between blocks. Every form is recognized here (all at column 0):
    // a sigiled heading opens a section; a `:::` line opens a container or a
    // set; a `::` line is a leaf. Nested blocks live inside their parent.
    let mut nodes: Vec<Node> = Vec::new();
    let mut pending = String::new();
    let mut i = 0;
    let mut in_code = None::<usize>;
    macro_rules! flush {
        () => {
            if !pending.is_empty() {
                nodes.push(Node::Text(std::mem::take(&mut pending)));
            }
        };
    }
    macro_rules! text {
        ($line:expr) => {
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str($line);
        };
    }
    while i < lines.len() {
        // Fenced code at top level suspends ALL recognition (§3.3 rule 5):
        // fences, leaves, headings and keywords inside it are content.
        if let Some(tl) = in_code {
            if code_fence_len(lines[i]) == Some(tl) {
                in_code = None;
            }
            text!(lines[i]);
            i += 1;
            continue;
        }
        if let Some(tl) = code_fence_len(lines[i]) {
            in_code = Some(tl);
            text!(lines[i]);
            i += 1;
            continue;
        }
        // An author note (S9) is its own node, out of the text runs: nothing
        // in it is read, so a commented-out `:::!workflow` never folds. One
        // that never closes runs to the end and is not refused.
        if note_opens(lines[i]) {
            flush!();
            let end = note_end(&lines, i, lines.len()).unwrap_or(lines.len() - 1);
            nodes.push(Node::Note { start: i, end });
            i = end + 1;
            continue;
        }
        // A keyword paragraph / list item, or a blockquote alert, is a block
        // of its prose kind (§4.5/§4.6) — lifted so the tree carries it.
        if (keyword_line(lines[i]).is_some() || alert_block_kind(lines[i]).is_some())
            && let Some((b, next)) = lift_keyword_or_alert(&lines, i, base, lines.len(), &mut errs)
        {
            flush!();
            nodes.push(Node::Block(b));
            i = next;
            continue;
        }
        // The region a top-level block occupies (0-based body lines, inclusive)
        // is `[i, next-1]` — delivery replaces exactly this run of source lines.
        if let Some(sec) = section_open(lines[i]) {
            flush!();
            let (block, next) = parse_section(&lines, i, sec, base, &mut errs);
            if let Some(mut b) = block {
                // A section's region ends at its LAST NON-BLANK line (§3.5) —
                // trailing blank lines before the next heading are delivered,
                // not swallowed.
                let mut end = next.saturating_sub(1);
                while end > i && lines[end].trim().is_empty() {
                    end -= 1;
                }
                b.region = (i, end);
                nodes.push(Node::Block(b));
            }
            i = next;
            continue;
        }
        if let Some(of) = open_fence(lines[i]) {
            flush!();
            match parse_fenced(&lines, i, of, base, lines.len(), &mut errs) {
                // A rule container's region covers the reason that follows it.
                Fenced::Blocks { blocks, next, .. } => {
                    for mut b in blocks {
                        b.region = (i, next.saturating_sub(1));
                        nodes.push(Node::Block(b));
                    }
                    i = next;
                }
                Fenced::Inert { kind, body, next } => {
                    nodes.push(Node::Inert {
                        kind,
                        line: base + i + 1,
                        region: (i, next.saturating_sub(1)),
                        body: body.join("\n"),
                    });
                    i = next;
                }
            }
            continue;
        }
        match leaf_open(lines[i]).map(|lf| parse_leaf(lf, base + i + 1, &mut errs)) {
            Some(Leafed::Block(mut b)) => {
                flush!();
                b.region = (i, i);
                nodes.push(Node::Block(*b));
            }
            Some(Leafed::Refused) => flush!(),
            // An inert leaf is a line of prose, as written.
            Some(Leafed::Inert) | None => {
                text!(lines[i]);
            }
        }
        i += 1;
    }
    if !pending.is_empty() {
        nodes.push(Node::Text(pending));
    }

    let blocks: Vec<&Block> = nodes
        .iter()
        .filter_map(|n| match n {
            Node::Block(b) => Some(b),
            Node::Text(_) | Node::Note { .. } | Node::Inert { .. } => None,
        })
        .collect();
    // Identity and references are document-wide (S26): a rule named inside a
    // `when`, a section or any container body is as addressable, and as
    // unique, as one at the top level.
    let all = every_block(&blocks);
    check_identity(&blocks, &all, &mut errs);
    check_refs(&all, &mut errs);
    check_placement(&blocks, &mut errs);
    check_attr_values(&all, &front, &mut errs);
    check_overrides(&all, &mut errs);
    // An author note is not scanned for references (§3.3 rule 11), at the
    // top level or in a body.
    let notes: Vec<(usize, usize)> = nodes
        .iter()
        .filter_map(|n| match n {
            Node::Note { start, end } => Some((*start, *end)),
            _ => None,
        })
        .chain(all.iter().flat_map(|b| b.notes.iter().copied()))
        .collect();
    check_inline_refs(&all, body, base, &notes, &mut errs);

    if errs.is_empty() {
        Ok(Document {
            front,
            has_front_matter: body_start > 0,
            nodes,
            end_matter,
            source: body.to_string(),
            raw: text.to_string(),
        })
    } else {
        Err(errs)
    }
}

fn body_start_line(text: &str, body_start: usize) -> usize {
    text[..body_start].bytes().filter(|&b| b == b'\n').count()
}

/// A `:::`-opened block: its fence length, machinery sigil, kind, the `[]` set
/// marker, and the raw attribute source.
struct OpenFence {
    len: usize,
    sigil: bool,
    kind: String,
    is_set: bool,
    attr_src: String,
}

/// A `::`-opened leaf (one line, no body).
struct LeafTok {
    sigil: bool,
    kind: String,
    attr_src: String,
}

/// A `## !kind name` section heading.
struct SectionTok {
    level: usize,
    kind: String,
    name: String,
    attr_src: String,
}

/// A `:::` opener, classified once (§3.3): blocks, or an inert block.
enum Fenced {
    /// The blocks it declares (none when it is refused), the index just past
    /// its close, and the index just past the reason that follows it.
    Blocks {
        blocks: Vec<Block>,
        closed: usize,
        next: usize,
    },
    /// An inert block (§3.3 rule 2; S23): its body lines, raw, and the index
    /// just past its close.
    Inert {
        kind: String,
        body: Vec<String>,
        next: usize,
    },
}

/// Classify the `:::` opener at `open_idx`, then parse it as a block or
/// swallow it as an inert one. `bound` is where the enclosing body ends.
fn parse_fenced(
    lines: &[&str],
    open_idx: usize,
    of: OpenFence,
    line_base: usize,
    bound: usize,
    errs: &mut Vec<Refusal>,
) -> Fenced {
    let line_no = line_base + open_idx + 1;
    let form = if of.is_set {
        Form::Set
    } else {
        Form::Container
    };
    let disposition = match classify(&of.kind, of.sigil, form, line_no, errs) {
        Class::Block(d) => Some(d),
        Class::Refused => None,
        Class::Inert => {
            // Read as the reference scanner reads one: raw, to the FIRST
            // close fence long enough — no nesting, no code, no notes — so
            // nothing inside an inert block opens a block of its own.
            let close = (open_idx + 1..bound)
                .find(|&j| fence_close_len(lines[j]).is_some_and(|n| n >= of.len));
            if close.is_none() {
                errs.push(Refusal::at(
                    line_no,
                    "unclosed-fence",
                    format!(
                        ":::{} is never closed (expected a line of ≥{} colons)",
                        of.kind, of.len
                    ),
                ));
            }
            let end = close.unwrap_or(bound);
            return Fenced::Inert {
                kind: of.kind,
                body: lines[open_idx + 1..end]
                    .iter()
                    .map(|l| l.to_string())
                    .collect(),
                next: close.map_or(bound, |c| c + 1),
            };
        }
    };
    let (blocks, closed, next) =
        parse_fence_and_reason(lines, open_idx, of, disposition, line_base, bound, errs);
    Fenced::Blocks {
        blocks,
        closed,
        next,
    }
}

/// Parse a `:::` block — a container (one block) or a set (many) — of the
/// `disposition` classify gave it, or `None` when it was refused (the body is
/// still read, to find its close). Returns the member blocks in source order
/// and the index just past the closing fence.
fn parse_fence(
    lines: &[&str],
    open_idx: usize,
    of: OpenFence,
    disposition: Option<Disposition>,
    line_base: usize,
    errs: &mut Vec<Refusal>,
) -> (Vec<Block>, usize) {
    let line_no = line_base + open_idx + 1;
    let attrs = attrs_or_empty(&of.attr_src, &of.kind, line_no, errs);
    let body_kind = lookup(&of.kind).map_or(BodyKind::Markdown, |k| k.body);
    let scan = BodyScan {
        // A `verbatim` body is quoted whole — nested fence syntax is content,
        // not structure (for tutorials that must show a fence).
        verbatim: attrs.contains_key("verbatim"),
        // Keyword/alert lifting applies only where the body is interpreted
        // as prose — never in YAML/code/table bodies, and never inside
        // `example` (its body is quoted material; the keyword-scope rule
        // excludes it).
        lift: !of.is_set && of.kind != "example" && body_kind == BodyKind::Markdown,
        // A note is a note wherever the body is Markdown, an example's
        // included; in a YAML or code body `<!--` is content (§3.3 rule 11).
        notes: body_kind == BodyKind::Markdown,
        set: of.is_set,
    };

    let (c, close_idx, closed) = collect_body(lines, open_idx + 1, of.len, line_base, scan, errs);
    if !closed {
        errs.push(Refusal::at(
            line_no,
            "unclosed-fence",
            format!(
                ":::{}{} is never closed (expected a line of ≥{} colons)",
                if of.sigil { "!" } else { "" },
                of.kind,
                of.len
            ),
        ));
    }

    let Some(disposition) = disposition else {
        return (Vec::new(), close_idx + 1); // classify recorded the error
    };

    if of.is_set {
        let members = parse_set_body(&of.kind, disposition, &attrs, &c.body, line_no, errs);
        (members, close_idx + 1)
    } else {
        let name = attrs.get("name").cloned();
        let body = c.body.join("\n");
        let raw_body = (c.raw != c.body).then(|| c.raw.join("\n"));
        (
            vec![Block {
                kind: of.kind,
                disposition,
                name,
                attrs,
                body,
                children: c.children,
                line: line_no,
                set_group: None,
                form: Form::Container,
                raw_body,
                region: (0, 0),
                reason: None,
                notes: c.notes,
            }],
            close_idx + 1,
        )
    }
}

/// A `::` leaf, classified: a block, a refusal (recorded), or an inert name
/// whose line stays prose, as written.
enum Leafed {
    Block(Box<Block>),
    Refused,
    Inert,
}

/// Parse a `::kind{attrs}` leaf — one instance, no body.
fn parse_leaf(lf: LeafTok, line_no: usize, errs: &mut Vec<Refusal>) -> Leafed {
    let disposition = match classify(&lf.kind, lf.sigil, Form::Leaf, line_no, errs) {
        Class::Block(d) => d,
        Class::Refused => return Leafed::Refused,
        Class::Inert => return Leafed::Inert,
    };
    let attrs = attrs_or_empty(&lf.attr_src, &lf.kind, line_no, errs);
    let name = attrs.get("name").cloned();
    Leafed::Block(Box::new(Block {
        kind: lf.kind,
        disposition,
        name,
        attrs,
        body: String::new(),
        children: Vec::new(),
        line: line_no,
        set_group: None,
        form: Form::Leaf,
        raw_body: None,
        region: (0, 0),
        reason: None,
        notes: Vec::new(),
    }))
}

/// Parse a `## !kind name` section — one instance whose body is the section
/// beneath the heading, up to the next same-or-higher heading (or any sigiled
/// heading). For a YAML/code-bodied kind the body is the single fenced code
/// block it must contain, and the surrounding prose becomes its description.
fn parse_section(
    lines: &[&str],
    open_idx: usize,
    sec: SectionTok,
    line_base: usize,
    errs: &mut Vec<Refusal>,
) -> (Option<Block>, usize) {
    let line_no = line_base + open_idx + 1;
    let body_kind = lookup(&sec.kind)
        .map(|k| k.body)
        .unwrap_or(BodyKind::Markdown);
    // Notes are read in a Markdown section only; in a YAML or code section,
    // its description included, `<!--` is content (§3.3 rule 11).
    let notes = body_kind == BodyKind::Markdown;
    let end = section_extent(lines, open_idx + 1, sec.level, notes);
    // `## !kind` is sigiled, so it is never inert.
    let disposition = match classify(&sec.kind, true, Form::Section, line_no, errs) {
        Class::Block(d) => Some(d),
        Class::Refused | Class::Inert => None,
    };

    let mut attrs = attrs_or_empty(&sec.attr_src, &sec.kind, line_no, errs);
    attrs.entry("name".to_string()).or_insert(sec.name.clone());

    let Some(disposition) = disposition else {
        return (None, end);
    };

    if matches!(body_kind, BodyKind::Yaml | BodyKind::Code) {
        // A YAML/code section is heading + description + the single fenced code
        // block that is its definition. It ENDS at that fence — content after it
        // returns to the document top level, so a workflow section does not
        // swallow the blocks that follow it (the section-boundary trap).
        match find_code_fence(lines, open_idx + 1, end) {
            Some((fo, fc)) => {
                let c = collect_range(lines, open_idx + 1, fo, line_base, false, errs);
                let desc = lines[open_idx + 1..fo].join("\n");
                if !desc.trim().is_empty() {
                    attrs
                        .entry("description".to_string())
                        .or_insert(desc.trim().to_string());
                }
                (
                    Some(Block {
                        kind: sec.kind,
                        disposition,
                        name: Some(sec.name),
                        attrs,
                        body: lines[fo + 1..fc].join("\n"),
                        children: c.children,
                        line: line_no,
                        set_group: None,
                        form: Form::Section,
                        raw_body: None,
                        region: (0, 0),
                        reason: None,
                        notes: Vec::new(),
                    }),
                    fc + 1,
                )
            }
            None => {
                errs.push(Refusal::at(
                    line_no,
                    "section-without-fence",
                    format!(
                        "a {} section must contain exactly one fenced {} block",
                        sec.kind,
                        if lookup(&sec.kind).map(|k| k.body) == Some(BodyKind::Code) {
                            "code"
                        } else {
                            "yaml"
                        }
                    ),
                ));
                (None, end)
            }
        }
    } else {
        // A Markdown section is the whole section beneath the heading. Nested
        // fences/leaves are its children; the rest is its prose.
        let c = collect_range(lines, open_idx + 1, end, line_base, notes, errs);
        (
            Some(Block {
                kind: sec.kind,
                disposition,
                name: Some(sec.name),
                attrs,
                body: c.body.join("\n"),
                children: c.children,
                line: line_no,
                set_group: None,
                form: Form::Section,
                raw_body: (c.raw != c.body).then(|| c.raw.join("\n")),
                region: (0, 0),
                reason: None,
                notes: c.notes,
            }),
            end,
        )
    }
}

/// The exclusive end of a Markdown section's body starting at `from`
/// (§4.4 rule 2), the EARLIEST of: the next same-or-shallower heading; any
/// sigiled heading; or a **sigiled** fence, leaf or set at column 0. A BARE
/// fence (a sub-block like `:::case`, or a `:::example`) belongs to the section
/// and is skipped whole — machinery, being sigiled, is never a section's child.
/// With `notes` (a Markdown section), nothing inside an author note ends it.
fn section_extent(lines: &[&str], from: usize, level: usize, notes: bool) -> usize {
    let mut i = from;
    let mut in_code = None::<usize>;
    while i < lines.len() {
        let line = lines[i];
        if let Some(tl) = in_code {
            if code_fence_len(line) == Some(tl) {
                in_code = None;
            }
            i += 1;
            continue;
        }
        if let Some(tl) = code_fence_len(line) {
            in_code = Some(tl);
            i += 1;
            continue;
        }
        if notes && note_opens(line) {
            match note_end(lines, i, lines.len()) {
                Some(end) => {
                    i = end + 1;
                    continue;
                }
                // A note that never closes runs to the end, and so does the
                // section it opened in.
                None => return lines.len(),
            }
        }
        if section_open(line).is_some() {
            return i; // (b) a sigiled heading
        }
        if let Some(l) = heading_level(line) {
            if l <= level {
                return i; // (a) a same-or-shallower heading
            }
            i += 1; // a deeper heading belongs to the body
            continue;
        }
        if let Some(of) = open_fence(line) {
            if of.sigil {
                return i; // (c) a sigiled fence or set at column 0
            }
            i = skip_fence(lines, i, of.len); // a bare sub-block: skip it whole
            continue;
        }
        if let Some(lf) = leaf_open(line)
            && lf.sigil
        {
            return i; // (c) a sigiled leaf at column 0
        }
        i += 1;
    }
    lines.len()
}

/// The index just past the closing fence of the bare block opened at
/// `open_idx` (fence length `open_len`), honouring code and nested fences.
fn skip_fence(lines: &[&str], open_idx: usize, open_len: usize) -> usize {
    let mut i = open_idx + 1;
    let mut in_code = None::<usize>;
    while i < lines.len() {
        let line = lines[i];
        if let Some(tl) = in_code {
            if code_fence_len(line) == Some(tl) {
                in_code = None;
            }
            i += 1;
            continue;
        }
        if let Some(tl) = code_fence_len(line) {
            in_code = Some(tl);
            i += 1;
            continue;
        }
        if let Some(len) = fence_close_len(line)
            && len >= open_len
            && open_fence(line).is_none()
        {
            return i + 1;
        }
        if let Some(of) = open_fence(line) {
            i = skip_fence(lines, i, of.len);
            continue;
        }
        i += 1;
    }
    lines.len()
}

/// The `(open, close)` line indices of the first fenced code block in
/// `[from, end)`, if any.
fn find_code_fence(lines: &[&str], from: usize, end: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < end {
        if let Some(tl) = code_fence_len(lines[i]) {
            let open = i;
            i += 1;
            while i < end && code_fence_len(lines[i]) != Some(tl) {
                i += 1;
            }
            if i < end {
                return Some((open, i));
            }
            return None; // unterminated
        }
        i += 1;
    }
    None
}

/// Split the single fenced code block out of a section's prose: the block's
/// content is the definition, everything else is the description. `None` unless
/// there is exactly one fenced code block.
fn extract_single_code_block(lines: &[String]) -> Option<(String, String)> {
    let mut code: Option<Vec<String>> = None;
    let mut desc: Vec<String> = Vec::new();
    let mut count = 0usize;
    let mut i = 0;
    while i < lines.len() {
        if let Some(tl) = code_fence_len(&lines[i]) {
            let mut block = Vec::new();
            i += 1;
            while i < lines.len() && code_fence_len(&lines[i]) != Some(tl) {
                block.push(lines[i].clone());
                i += 1;
            }
            i += 1; // skip the closing fence
            count += 1;
            code = Some(block);
        } else {
            desc.push(lines[i].clone());
            i += 1;
        }
    }
    (count == 1).then(|| (code.unwrap_or_default().join("\n"), desc.join("\n")))
}

/// How a container's body is read.
#[derive(Debug, Clone, Copy)]
struct BodyScan {
    /// `verbatim`: captured raw, nothing inside it parsed.
    verbatim: bool,
    /// Keyword and alert lines are child blocks (a Markdown body, never an
    /// example's or a set's).
    lift: bool,
    /// A column-0 `<!--` opens an author note (S9): the body is Markdown.
    notes: bool,
    /// The body is a set's rows or entries: a note's lines are kept out of
    /// what the set parser reads, as the reference parser skips them.
    set: bool,
}

/// What a body walk collects.
#[derive(Debug, Default)]
struct Collected {
    children: Vec<Block>,
    /// The body with child blocks removed — what the §9.1 tree reads.
    body: Vec<String>,
    /// The body WITH lifted keyword/alert lines still in place — delivery's
    /// view.
    raw: Vec<String>,
    /// The author notes directly in this body (0-based body lines,
    /// inclusive). Their lines are in `body` and `raw`, as the tree keeps
    /// them.
    notes: Vec<(usize, usize)>,
}

impl Collected {
    /// A line of the body, in both views.
    fn line(&mut self, l: &str) {
        self.body.push(l.to_string());
        self.raw.push(l.to_string());
    }
}

/// Collect a container's body: raw lines that are not part of a nested block,
/// plus the recursively-parsed children. `open_len` is the opening fence
/// length; the close is the first fence of `>= open_len` colons. Returns what
/// was collected, the close's index, and whether the body closed.
fn collect_body(
    lines: &[&str],
    from: usize,
    open_len: usize,
    line_base: usize,
    scan: BodyScan,
    errs: &mut Vec<Refusal>,
) -> (Collected, usize, bool) {
    let mut c = Collected::default();
    let mut i = from;
    let mut in_code = None::<usize>;
    while i < lines.len() {
        let line = lines[i];
        if let Some(tick_len) = in_code {
            if code_fence_len(line) == Some(tick_len) {
                in_code = None;
            }
            c.line(line);
            i += 1;
            continue;
        }
        // The close for THIS block — checked before code/nesting so a verbatim
        // body still terminates.
        if let Some(len) = fence_close_len(line)
            && len >= open_len
            && open_fence(line).is_none()
        {
            return (c, i, true);
        }
        if scan.verbatim {
            c.line(line);
            i += 1;
            continue;
        }
        // An author note: its lines stay in the body (the tree keeps them),
        // but nothing in it is read — no fence inside it opens or closes a
        // block, and no keyword inside it is lifted. One that never closes
        // runs to the end, and the container with it.
        if scan.notes && note_opens(line) {
            let end = note_end(lines, i, lines.len()).unwrap_or(lines.len() - 1);
            if !scan.set {
                for l in &lines[i..=end] {
                    c.line(l);
                }
            }
            c.notes.push((i, end));
            i = end + 1;
            continue;
        }
        if let Some(tl) = code_fence_len(line) {
            in_code = Some(tl);
            c.line(line);
            i += 1;
            continue;
        }
        // A nested container or set — recurse; an inert one's body is this
        // body's text, fences dropped, as the reference parser reads it.
        if let Some(of) = open_fence(line) {
            match parse_fenced(lines, i, of, line_base, lines.len(), errs) {
                Fenced::Blocks {
                    blocks,
                    closed,
                    next,
                } => {
                    // A child's reason is its own (the tree), and stays in the
                    // text delivery reads until delivery renders reasons.
                    for l in lines.get(closed..next).unwrap_or_default() {
                        c.raw.push(l.to_string());
                    }
                    c.children.extend(blocks);
                    i = next;
                }
                Fenced::Inert { body, next, .. } => {
                    for l in &body {
                        c.line(l);
                    }
                    i = next;
                }
            }
            continue;
        }
        // A nested leaf; an inert one is a line of the body.
        if let Some(lf) = leaf_open(line) {
            match parse_leaf(lf, line_base + i + 1, errs) {
                Leafed::Block(b) => c.children.push(*b),
                Leafed::Refused => {}
                Leafed::Inert => c.line(line),
            }
            i += 1;
            continue;
        }
        // A keyword paragraph or alert in a Markdown body is a CHILD block —
        // excluded from `body` (the §9.1 tree) but kept in `raw` (delivery
        // normalizes it in place). Only where the kind interprets its body as
        // prose: never inside YAML/code/table bodies, never inside `example`.
        if scan.lift
            && (keyword_line(line).is_some() || alert_block_kind(line).is_some())
            && let Some((b, next)) = lift_keyword_or_alert(lines, i, line_base, lines.len(), errs)
        {
            for l in &lines[i..next.min(lines.len())] {
                c.raw.push(l.to_string());
            }
            c.children.push(b);
            i = next;
            continue;
        }
        c.line(line);
        i += 1;
    }
    (c, lines.len(), false)
}

/// Walk a bounded range `[from, end)` (a section body), splitting nested
/// fences/leaves out as children and keeping the remaining prose lines. With
/// `notes` (a Markdown section), author notes are read as in a container.
fn collect_range(
    lines: &[&str],
    from: usize,
    end: usize,
    line_base: usize,
    notes: bool,
    errs: &mut Vec<Refusal>,
) -> Collected {
    let mut c = Collected::default();
    let mut i = from;
    let mut in_code = None::<usize>;
    while i < end {
        let line = lines[i];
        if let Some(tick_len) = in_code {
            if code_fence_len(line) == Some(tick_len) {
                in_code = None;
            }
            c.line(line);
            i += 1;
            continue;
        }
        if notes && note_opens(line) {
            let close = note_end(lines, i, end).unwrap_or(end - 1);
            for l in &lines[i..=close] {
                c.line(l);
            }
            c.notes.push((i, close));
            i = close + 1;
            continue;
        }
        if let Some(tl) = code_fence_len(line) {
            in_code = Some(tl);
            c.line(line);
            i += 1;
            continue;
        }
        if let Some(of) = open_fence(line) {
            match parse_fenced(lines, i, of, line_base, end, errs) {
                Fenced::Blocks {
                    blocks,
                    closed,
                    next,
                } => {
                    for l in lines.get(closed..next).unwrap_or_default() {
                        c.raw.push(l.to_string());
                    }
                    c.children.extend(blocks);
                    i = next.min(end);
                }
                Fenced::Inert { body, next, .. } => {
                    for l in &body {
                        c.line(l);
                    }
                    i = next.min(end);
                }
            }
            continue;
        }
        if let Some(lf) = leaf_open(line) {
            match parse_leaf(lf, line_base + i + 1, errs) {
                Leafed::Block(b) => c.children.push(*b),
                Leafed::Refused => {}
                Leafed::Inert => c.line(line),
            }
            i += 1;
            continue;
        }
        // A keyword paragraph or alert inside a section body is that section's
        // CHILD block (§4.5; the fixture corpus pins the shape) and is
        // excluded from the body prose.
        if (keyword_line(line).is_some() || alert_block_kind(line).is_some())
            && let Some((b, next)) = lift_keyword_or_alert(lines, i, line_base, end, errs)
        {
            for l in &lines[i..next.min(end)] {
                c.raw.push(l.to_string());
            }
            c.children.push(b);
            i = next.min(end);
            continue;
        }
        c.line(line);
        i += 1;
    }
    c
}

/// What the lexical rules (§3.3) make of a block opener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// A block of this disposition.
    Block(Disposition),
    /// Inert prose (§3.3 rule 2; S23): an unknown bare name, or machinery
    /// registered after version 1 written bare. No block, and never refused.
    Inert,
    /// Refused; the refusal is recorded.
    Refused,
}

/// Classify a block by kind, sigil and form, enforcing the lexical rules
/// (§3.3): the reserved-bare and sigiled-prose guards, unknown-kind policy, and
/// per-kind form acceptance (§4).
fn classify(kind: &str, sigil: bool, form: Form, line_no: usize, errs: &mut Vec<Refusal>) -> Class {
    match lookup(kind) {
        // Sub-blocks (`case`, `override`, `signature`, `schema`, `preview`) are
        // written UNSIGILED — the parent's fence and sigil govern them.
        Some(k) if k.sub_of.is_some() => {
            if sigil {
                // A sigiled sub-block is no machinery kind of its own.
                errs.push(Refusal::at(
                    line_no,
                    "unknown-machinery-kind",
                    format!(
                        "`{kind}` is a sub-block — write it bare (no `!`) inside its `!{}`",
                        k.sub_of.as_deref().unwrap_or("")
                    ),
                ));
                return Class::Refused;
            }
            if !k.forms.contains(&form) {
                errs.push(form_refusal(k, form, line_no));
                return Class::Refused;
            }
            Class::Block(Disposition::Machinery)
        }
        Some(k) => {
            let want_sigil = k.disposition == Disposition::Machinery;
            // The reserved-bare / sigiled-prose guards do not apply to the
            // section form: `## !kind` is always machinery by syntax, and a
            // bare heading never reaches here (section_open needs the `!`).
            if form != Form::Section {
                if want_sigil && !sigil {
                    // Only version-1 machinery is reserved bare (S23): a
                    // version-1 document may have written `:::eval` as prose,
                    // and it stays prose.
                    if !registry().reserved_bare(kind) {
                        return Class::Inert;
                    }
                    errs.push(Refusal::at(
                        line_no,
                        "bare-machinery-kind",
                        format!("{kind:?} is a machinery kind — did you mean :::!{kind}"),
                    ));
                    return Class::Refused;
                }
                if !want_sigil && sigil {
                    // A sigiled structural kind is the same mistake: the `!`
                    // on a kind that is not machinery.
                    errs.push(Refusal::at(
                        line_no,
                        "sigiled-prose-kind",
                        format!(
                            "{kind:?} is a {} kind — did you mean :::{kind}",
                            if k.disposition == Disposition::Prose {
                                "prose"
                            } else {
                                "structural"
                            }
                        ),
                    ));
                    return Class::Refused;
                }
            } else if !want_sigil {
                errs.push(Refusal::at(
                    line_no,
                    "sigiled-prose-kind",
                    format!(
                        "`## !{kind}` — the section form is machinery only, and `{kind}` is {}",
                        if k.disposition == Disposition::Prose {
                            "prose"
                        } else {
                            "structural"
                        }
                    ),
                ));
                return Class::Refused;
            }
            if !k.forms.contains(&form) {
                if form == Form::Set && k.body == BodyKind::Deflist {
                    // Its container body is ALREADY a list of entries (§4.3.3).
                    errs.push(Refusal::at(
                        line_no,
                        "redundant-set",
                        format!("{} is already a list — write :::{}", k.name, k.name),
                    ));
                } else {
                    errs.push(form_refusal(k, form, line_no));
                }
                return Class::Refused;
            }
            Class::Block(k.disposition)
        }
        None => {
            if sigil {
                let head: Vec<&str> = registry()
                    .machinery_in_order()
                    .iter()
                    .take(3)
                    .map(String::as_str)
                    .collect();
                errs.push(Refusal::at(
                    line_no,
                    "unknown-machinery-kind",
                    format!(
                        "unknown machinery kind {kind:?} — this reader implements version {} \
                         (known: {}, …)",
                        registry().version(),
                        head.join(", ")
                    ),
                ));
                Class::Refused
            } else {
                // Unknown bare name — fail OPEN: inert prose, delivered verbatim.
                Class::Inert
            }
        }
    }
}

/// The refusal for a kind written in a form it does not accept, naming the
/// forms it does. A body-required message for the common leaf case.
fn form_refusal(k: &Kind, form: Form, line_no: usize) -> Refusal {
    if form == Form::Leaf && k.body != BodyKind::None {
        // Machinery names what the body is; a keyword-capable prose kind
        // points at the one-line spelling first.
        // What the body IS, from the schema's own `x-body` — never from the
        // kind's disposition (a prose `glossary` still names "its entries").
        // `workflow` is the single exception the corpus pins.
        let noun = match (k.name.as_str(), k.body) {
            ("workflow", _) => "its steps",
            (_, BodyKind::Yaml) => "its definition",
            (_, BodyKind::Code) => "its code",
            (_, BodyKind::Table) => "its rows",
            (_, BodyKind::Deflist) => "its entries",
            (_, BodyKind::Text) => "a text body",
            _ => "text",
        };
        let sig = if k.disposition == Disposition::Machinery {
            "!"
        } else {
            ""
        };
        // A keyword-capable prose kind points at the one-line spelling first.
        let message = match registry().keyword_kind_reverse(&k.name) {
            Some(kw) => format!(
                "{} requires a body ({noun}) — write \"{kw}: …\" or use :::{}",
                k.name, k.name
            ),
            None => format!(
                "{} requires a body ({noun}) — use :::{sig}{}",
                k.name, k.name
            ),
        };
        return Refusal::at(line_no, "body-required", message);
    }
    let names: Vec<&str> = k
        .forms
        .iter()
        .map(|f| match f {
            Form::Container => "container",
            Form::Leaf => "leaf",
            Form::Set => "set",
            Form::Section => "section",
            Form::Keyword => "keyword",
            Form::Alert => "alert",
        })
        .collect();
    let wrote = match form {
        Form::Container => "the container form",
        Form::Leaf => "the leaf form",
        Form::Set => "the set form",
        Form::Section => "the section form",
        Form::Keyword => "a keyword",
        Form::Alert => "an alert",
    };
    Refusal::at(
        line_no,
        "form-not-accepted",
        format!(
            "`{}` does not take {wrote} — its forms are: {}",
            k.name,
            names.join(", ")
        ),
    )
}

/// A set body → one member block per entry. The body is entirely a table or
/// entirely a definition list (spec §4.3.3); anything else is a refusal.
fn parse_set_body(
    kind: &str,
    disposition: Disposition,
    shared: &BTreeMap<String, String>,
    body_lines: &[String],
    line_no: usize,
    errs: &mut Vec<Refusal>,
) -> Vec<Block> {
    let first = body_lines.iter().find(|l| !l.trim().is_empty());
    let Some(first) = first else {
        return Vec::new(); // an empty set is valid and declares nothing
    };
    if first.trim_start().starts_with('|') {
        parse_table_set(kind, disposition, shared, body_lines, line_no, errs)
    } else {
        parse_deflist_set(kind, disposition, shared, body_lines, line_no, errs)
    }
}

/// A pipe-table set: the header row names attributes, each body row is one
/// instance (no body). Cells are attribute values under §3.2's grammar.
fn parse_table_set(
    kind: &str,
    disposition: Disposition,
    shared: &BTreeMap<String, String>,
    body_lines: &[String],
    line_no: usize,
    errs: &mut Vec<Refusal>,
) -> Vec<Block> {
    let rows: Vec<&String> = body_lines
        .iter()
        .filter(|l| l.trim_start().starts_with('|'))
        .collect();
    if rows.len() < 2 {
        errs.push(Refusal::at(
            line_no,
            "set-body-mixed",
            format!("a set body must be a table or a definition list ({kind}[])"),
        ));
        return Vec::new();
    }
    let header: Vec<String> = split_cells(rows[0])
        .into_iter()
        .map(|c| c.to_lowercase())
        .collect();
    let wants_name = lookup(kind).is_some_and(|k| k.requires_name);
    let mut out = Vec::new();
    // Every header cell must be an attribute of the kind (§4.3.1) — an
    // unknown column is a refusal naming it.
    if let Some(known) = registry().attrs_of(kind) {
        for col in &header {
            if !known.contains(col) {
                errs.push(Refusal::at(
                    line_no,
                    "unknown-column",
                    format!("{col:?} is not an attribute of {kind}"),
                ));
            }
        }
    }
    // rows[0] is the header, rows[1] the separator; instances start at rows[2].
    for (row_no, row) in rows.iter().skip(2).enumerate() {
        let row_no = row_no + 1;
        let cells = split_cells(row);
        let mut attrs = shared.clone();
        for (key, cell) in header.iter().zip(cells.iter()) {
            if !cell.is_empty() {
                attrs.insert(key.clone(), cell.clone());
            }
        }
        if wants_name && attrs.get("name").is_none_or(|n| n.is_empty()) {
            errs.push(Refusal::at(
                line_no,
                "set-row-without-name",
                format!("row {row_no} has no name — every member of {kind}[] needs one"),
            ));
            continue;
        }
        let name = attrs.get("name").cloned();
        out.push(Block {
            kind: kind.to_string(),
            disposition,
            name,
            attrs,
            body: String::new(),
            children: Vec::new(),
            line: line_no,
            set_group: Some(line_no as u64),
            form: Form::Set,
            raw_body: None,
            region: (0, 0),
            reason: None,
            notes: Vec::new(),
        });
    }
    out
}

/// A definition-list set: a term line (the instance `name` plus optional
/// attributes), then a `:`-prefixed definition that is the instance's body.
fn parse_deflist_set(
    kind: &str,
    disposition: Disposition,
    shared: &BTreeMap<String, String>,
    body_lines: &[String],
    line_no: usize,
    errs: &mut Vec<Refusal>,
) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < body_lines.len() {
        let line = &body_lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        // A term line: `name {attrs}?` at column 0.
        let (name, attr_src) = match parse_deflist_term(line) {
            Some(t) => t,
            None => {
                errs.push(Refusal::at(
                    line_no,
                    "set-body-mixed",
                    format!("a set body must be a table or a definition list ({kind}[])"),
                ));
                i += 1;
                continue;
            }
        };
        i += 1;
        // The definition: `:` lines and their indented continuations.
        let mut def: Vec<String> = Vec::new();
        while i < body_lines.len() {
            let l = &body_lines[i];
            if let Some(rest) = l.trim_start().strip_prefix(':') {
                def.push(rest.trim_start().to_string());
                i += 1;
            } else if l.trim().is_empty() {
                break;
            } else if l.starts_with([' ', '\t']) {
                def.push(l.trim_start().to_string());
                i += 1;
            } else {
                break;
            }
        }
        let mut attrs = shared.clone();
        for (k, v) in attrs_or_empty(&attr_src, kind, line_no, errs) {
            attrs.insert(k, v);
        }
        attrs.insert("name".to_string(), name.clone());
        out.push(Block {
            kind: kind.to_string(),
            disposition,
            name: Some(name),
            attrs,
            body: def.join("\n"),
            children: Vec::new(),
            line: line_no,
            set_group: Some(line_no as u64),
            form: Form::Set,
            raw_body: None,
            region: (0, 0),
            reason: None,
            notes: Vec::new(),
        });
    }
    out
}

/// A definition-list term line: `name` then an optional `{attrs}`.
fn parse_deflist_term(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    let (name_part, attr_part) = match t.split_once('{') {
        Some((n, a)) => (n.trim(), format!("{{{a}")),
        None => (t, String::new()),
    };
    if name_part.is_empty() || !is_name(name_part) {
        return None;
    }
    Some((name_part.to_string(), attr_part))
}

/// The `name` grammar of §3.2: `[A-Za-z0-9][A-Za-z0-9._-]*`.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Split a Markdown table row into trimmed cells, honouring `\|` escapes.
fn split_cells(row: &str) -> Vec<String> {
    let t = row.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => {
                cells.push(dequote(cur.trim()));
                cur = String::new();
            }
            _ => cur.push(c),
        }
    }
    cells.push(dequote(cur.trim()));
    cells
}

/// Strip one layer of surrounding double quotes from a cell value.
fn dequote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Parse an attribute source, recording a refusal and returning an empty map on
/// error rather than aborting the block.
fn attrs_or_empty(
    src: &str,
    kind: &str,
    line_no: usize,
    errs: &mut Vec<Refusal>,
) -> BTreeMap<String, String> {
    let (attrs, attr_errs) = parse_attrs(src, kind, line_no);
    errs.extend(attr_errs);
    attrs
}

/// Open-fence tokenizer: `:::[!]kind[]?{attrs}` at column 0. Returns the fence
/// length, the machinery sigil, the kind, the `[]` set marker, and the raw
/// attribute source. Recognized at column 0 only (§ fence-column-zero): an
/// indented fence is prose.
fn open_fence(line: &str) -> Option<OpenFence> {
    if !line.starts_with(":::") {
        return None;
    }
    let len = line.chars().take_while(|&c| c == ':').count();
    let rest = &line[len..];
    // A line of only colons is a CLOSE, not an open.
    if rest.trim().is_empty() {
        return None;
    }
    let (sigil, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let (kind, after) = take_kind(rest)?;
    let (is_set, after) = match after.strip_prefix("[]") {
        Some(a) => (true, a),
        None => (false, after),
    };
    let attr_src = after.trim().to_string();
    Some(OpenFence {
        len,
        sigil,
        kind,
        is_set,
        attr_src,
    })
}

/// Leaf tokenizer: `::[!]kind{attrs}` at column 0 — exactly two colons.
fn leaf_open(line: &str) -> Option<LeafTok> {
    if !line.starts_with("::") || line.starts_with(":::") {
        return None;
    }
    let rest = &line[2..];
    let (sigil, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let (kind, after) = take_kind(rest)?;
    // A leaf has no set marker and no body; trailing text after the attrs is not
    // a leaf (avoid eating a `:: ` used in prose).
    let after = after.trim();
    if !after.is_empty() && !after.starts_with('{') {
        return None;
    }
    Some(LeafTok {
        sigil,
        kind,
        attr_src: after.to_string(),
    })
}

/// Section tokenizer: `#{1,6} !kind name {attrs}?` at column 0.
fn section_open(line: &str) -> Option<SectionTok> {
    if !line.starts_with('#') {
        return None;
    }
    let level = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = line[level..].strip_prefix([' ', '\t'])?.trim_start();
    let rest = rest.strip_prefix('!')?;
    let (kind, after) = take_kind(rest)?;
    let after = after.trim_start();
    // The name is required and follows the kind.
    let mut it = after.splitn(2, [' ', '\t']);
    let name = it.next().unwrap_or("");
    if name.is_empty() || !is_name(name) {
        return None;
    }
    // Trailing closing `#`s are permitted and ignored; attributes may follow.
    let tail = it.next().unwrap_or("").trim();
    let attr_src = if tail.starts_with('{') {
        tail.rsplit_once('}')
            .map(|(a, _)| format!("{a}}}"))
            .unwrap_or_else(|| tail.to_string())
    } else {
        String::new()
    };
    Some(SectionTok {
        level,
        kind: kind.to_string(),
        name: name.to_string(),
        attr_src,
    })
}

/// An ATX heading's level (`#`-count), if the line is one at column 0.
fn heading_level(line: &str) -> Option<usize> {
    if !line.starts_with('#') {
        return None;
    }
    let n = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&n) && line[n..].starts_with([' ', '\t']) {
        Some(n)
    } else {
        None
    }
}

/// Read a kind token (`[A-Za-z][A-Za-z0-9_-]*`) from the front of `s`, returning
/// it and the remainder. `None` if the front is not a kind (so a `::: ` divider
/// in prose is not a directive).
fn take_kind(s: &str) -> Option<(String, &str)> {
    let mut end = s.len();
    for (idx, c) in s.char_indices() {
        if idx == 0 {
            if !c.is_ascii_alphabetic() {
                return None;
            }
            continue;
        }
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            end = idx;
            break;
        }
    }
    let kind = &s[..end];
    if kind.is_empty() {
        None
    } else {
        Some((kind.to_string(), &s[end..]))
    }
}

/// The length of a pure closing fence (a line of only colons, `>=3`), else None.
fn fence_close_len(line: &str) -> Option<usize> {
    let t = line.trim();
    if t.len() >= 3 && t.chars().all(|c| c == ':') {
        Some(t.len())
    } else {
        None
    }
}

/// The backtick/tilde count of a fenced-code delimiter line, else None.
fn code_fence_len(line: &str) -> Option<usize> {
    let t = line.trim_start();
    for delim in ['`', '~'] {
        let n = t.chars().take_while(|&c| c == delim).count();
        if n >= 3 {
            return Some(n);
        }
    }
    None
}

/// Whether `line` opens an author note (S9; §3.3 rule 11, `x-grammar.
/// authorNoteOpen`): `<!--` at column 0. One that begins mid-line is inline
/// HTML, and prose.
fn note_opens(line: &str) -> bool {
    line.starts_with("<!--")
}

/// The index of the line that closes the author note opened at `open`: the
/// first line holding `-->` (`authorNoteClose`), searched past the opener's
/// own four characters, so `<!-->` does not close itself. `None` when no line
/// before `bound` closes it.
fn note_end(lines: &[&str], open: usize, bound: usize) -> Option<usize> {
    (open..bound).find(|&j| {
        let l = if j == open { &lines[j][4..] } else { lines[j] };
        l.contains("-->")
    })
}

/// `{key=value key2="quoted"}` → map, collecting EVERY problem (Appendix B
/// shapes) rather than stopping at the first. A bare token is a legal flag
/// only when the kind's schema declares it (or `verbatim`) — `{name=on call}`
/// refuses `"call"` with the quote-it fix, instead of minting a flag out of a
/// value that lost its quotes. `#id`/`.class` shorthands are named refusals.
fn parse_attrs(src: &str, kind: &str, line_no: usize) -> (BTreeMap<String, String>, Vec<Refusal>) {
    let mut out = BTreeMap::new();
    let mut errs = Vec::new();
    let src = src.trim();
    if src.is_empty() {
        return (out, errs);
    }
    let Some(inner) = src.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        errs.push(Refusal::at(
            line_no,
            "malformed-attributes",
            "attributes must be wrapped in { }",
        ));
        return (out, errs);
    };
    let flag_ok = |key: &str| {
        key == "verbatim"
            || registry()
                .attrs_of(kind)
                .is_none_or(|set| set.contains(key))
    };
    let mut chars = inner.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        // `#id` / `.class` are not this grammar (§3.2's table): name the
        // identity fix for `#`, and refuse the punctuation itself.
        if c == '#' || c == '.' {
            if c == '#' {
                let token: String = inner
                    .chars()
                    .skip_while(|&x| x != '#')
                    .take_while(|x| !x.is_whitespace())
                    .collect();
                errs.push(Refusal::at(
                    line_no,
                    "id-class-shorthand",
                    format!(
                        "{token:?} is not an attribute — identity is name={}",
                        token.trim_start_matches('#')
                    ),
                ));
            }
            errs.push(Refusal::at(
                line_no,
                "malformed-attributes",
                format!("attributes: expected key=value, found \"{c}\" — quote values with spaces"),
            ));
            chars.next();
            continue;
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c.is_whitespace() {
                break;
            }
            key.push(c);
            chars.next();
        }
        if key.is_empty() {
            errs.push(Refusal::at(
                line_no,
                "malformed-attributes",
                "empty attribute name",
            ));
            chars.next();
            continue;
        }
        // Bare token: a schema-declared flag, or a refusal (a value that lost
        // its quotes is the common cause).
        if chars.peek() != Some(&'=') {
            if flag_ok(&key) {
                if out.insert(key.clone(), String::new()).is_some() {
                    errs.push(Refusal::at(
                        line_no,
                        "repeated-attribute",
                        format!("attribute {key:?} is repeated"),
                    ));
                }
            } else {
                errs.push(Refusal::at(
                    line_no,
                    "malformed-attributes",
                    format!(
                        "attributes: expected key=value, found {key:?} — quote values with spaces"
                    ),
                ));
            }
            continue;
        }
        chars.next(); // '='
        let mut val = String::new();
        match chars.peek() {
            Some(&'"') => {
                chars.next();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                val.push(n);
                            }
                        }
                        _ => val.push(c),
                    }
                }
            }
            _ => {
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    if c == '}' {
                        // §3.2: a bare value runs to whitespace or `}` — an
                        // embedded `}` means the closer was inside the value.
                        errs.push(Refusal::at(
                            line_no,
                            "malformed-attributes",
                            format!(
                                "bare value {val:?} runs into '}}' — quote values containing '}}'"
                            ),
                        ));
                        return (out, errs);
                    }
                    val.push(c);
                    chars.next();
                }
            }
        }
        if out.insert(key.clone(), val).is_some() {
            errs.push(Refusal::at(
                line_no,
                "repeated-attribute",
                format!("attribute {key:?} is repeated"),
            ));
        }
    }
    (out, errs)
}

/// Front matter: a leading `---\n … \n---`. Returns the parsed map and the byte
/// offset where the body begins. A document with no front matter, or one whose
/// front matter lacks `spec`, is version 1 (spec rule `front-matter-absent`);
/// a document pinning a higher version is refused rather than mis-read.
fn parse_front_matter(text: &str, errs: &mut Vec<Refusal>) -> (BTreeMap<String, Value>, usize) {
    let mut map = BTreeMap::new();
    let Some((block, body_start)) = front_matter_bounds(text) else {
        return (map, 0);
    };
    match crate::yaml::parse(block) {
        Ok(Value::Object(m)) => {
            for (kk, v) in m {
                map.insert(kk, v);
            }
        }
        // Line 1, the front matter's opening fence, as every port names it:
        // conformance compares the line with the code.
        Ok(_) => errs.push(Refusal::at(
            1,
            "front-matter-yaml",
            "front matter must be a YAML mapping",
        )),
        Err(e) => errs.push(Refusal::at(
            1,
            "front-matter-yaml",
            format!("front matter is not valid YAML: {e}"),
        )),
    }
    if let Some(spec) = map.get("spec") {
        let s = spec
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| spec.to_string());
        let major: u32 = s
            .trim_matches('"')
            .split('.')
            .next()
            .unwrap_or("")
            .parse()
            .unwrap_or(0);
        // The sigiled Instruction Document dialect is spec version 1 (the sole
        // version). A document pinning a higher version is written for a newer
        // spec this agentd does not implement — refused rather than mis-read.
        let trimmed = s.trim_matches('"');
        if !trimmed.chars().all(|c| c.is_ascii_digit()) || trimmed.is_empty() {
            errs.push(Refusal::new(
                "non-integer-version",
                format!("front matter: spec {trimmed:?} is not a version — versions are integers"),
            ));
            // The second refusal the fixture pins: the schema's own pattern on
            // `spec`, as a schema-validating reader reports it.
            errs.push(Refusal::new(
                "schema",
                "/frontMatter/spec: must match pattern \"^[0-9]+$\"",
            ));
        } else if major > 1 {
            errs.push(Refusal::new(
                "unimplemented-version",
                format!("front matter: spec {trimmed:?} is not implemented by this reader"),
            ));
        }
    }
    (map, body_start)
}

/// Where front matter lies: the YAML between a leading `---\n` and the next
/// `\n---`, and the byte offset the body begins at, past the closing line.
/// `None` when the text opens with no front matter.
fn front_matter_bounds(text: &str) -> Option<(&str, usize)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let after = "---\n".len() + end + "\n---".len();
    let body_start = text[after..]
        .find('\n')
        .map(|n| after + n + 1)
        .unwrap_or(text.len());
    Some((&rest[..end], body_start))
}

/// A document split at its end matter (S27; §3.1.1).
#[derive(Debug, Clone, PartialEq)]
pub struct SplitEndMatter<'a> {
    /// The text before the end matter's opening `---`, front matter
    /// included; the whole text when there is no end matter.
    pub before: &'a str,
    /// The end matter's keys, when the text ends with end matter.
    pub end_matter: Option<BTreeMap<String, Value>>,
    /// The 1-based line of the end matter's opening `---`.
    pub opener_line: Option<usize>,
}

/// Split a whole document — front matter and all — at its end matter, as
/// [`parse`] does before it reads a block: for a reader that needs the text
/// without its record (a folder of documents, a prompt) and never parses it.
/// End matter that is not YAML, or not a mapping, is refused
/// (`end-matter-yaml`), naming its first line.
pub fn split_end_matter(text: &str) -> Result<SplitEndMatter<'_>, Refusal> {
    let from = front_matter_bounds(text).map_or(0, |(_, start)| body_start_line(text, start));
    Ok(match end_matter_span(text, from) {
        None => SplitEndMatter {
            before: text,
            end_matter: None,
            opener_line: None,
        },
        Some(span) => SplitEndMatter {
            before: span.before,
            end_matter: Some(span.fields?),
            opener_line: Some(span.opener_line),
        },
    })
}

/// End matter found in a text, its YAML judged: what precedes its opener,
/// the opener's 1-based line, and its keys or the refusal.
struct EndMatterSpan<'a> {
    before: &'a str,
    opener_line: usize,
    fields: Result<BTreeMap<String, Value>, Refusal>,
}

/// The end matter of `text`, whose body starts at line `from` (0-based), if
/// it has any.
fn end_matter_span(text: &str, from: usize) -> Option<EndMatterSpan<'_>> {
    let lines: Vec<&str> = text.split('\n').collect();
    let (open, close) = find_end_matter(&lines, from)?;
    let before: usize = lines[..open].iter().map(|l| l.len() + 1).sum();
    Some(EndMatterSpan {
        before: &text[..before],
        opener_line: open + 1,
        // The first end-matter line: the opener's 1-based line plus one.
        fields: end_matter_fields(&lines[open + 1..close].join("\n"), open + 2),
    })
}

/// End matter's YAML as its keys, or the refusal. Both refusals name `first`,
/// the first end-matter line, as the reference parser does; the corpus does
/// not pin it.
fn end_matter_fields(yaml: &str, first: usize) -> Result<BTreeMap<String, Value>, Refusal> {
    match crate::yaml::parse(yaml) {
        Ok(Value::Object(m)) => Ok(m.into_iter().collect()),
        Ok(_) => Err(Refusal::at(
            first,
            "end-matter-yaml",
            "end matter is not a YAML mapping — write key: value lines",
        )),
        Err(e) => Err(Refusal::at(
            first,
            "end-matter-yaml",
            format!(
                "end matter is not valid YAML: {}",
                e.to_string().lines().next().unwrap_or_default()
            ),
        )),
    }
}

/// The end matter's opening and closing lines (0-based), found as the
/// reference implementation's `findEndMatter` finds them. `from` is the first
/// body line, so the front matter's closing fence is never an opener.
fn find_end_matter(lines: &[&str], from: usize) -> Option<(usize, usize)> {
    // The close: the last non-blank line, a `---` after the first body line.
    let close = (from..lines.len())
        .rev()
        .find(|&k| !lines[k].trim().is_empty())?;
    if close <= from || lines[close].trim_end() != "---" {
        return None;
    }
    // The opener: the NEAREST `---` before the close, and only that one. If
    // it fails a condition there is no end matter — never a search further
    // back, which would reach past a `---` in fenced code to a thematic break
    // and swallow the prose between them.
    let open = (from..close)
        .rev()
        .find(|&k| lines[k].trim_end() == "---")?;
    if open + 1 >= close {
        return None;
    }
    // It follows a blank line, or begins the body.
    if open > from && !lines[open - 1].trim().is_empty() {
        return None;
    }
    if !is_mapping_key_line(lines[open + 1]) {
        return None;
    }
    scanner_reaches(lines, from, open).then_some((open, close))
}

/// Whether `line` opens with a YAML mapping key, as the reference pattern
/// `^[A-Za-z_][\w.-]*[ \t]*:([ \t]|$)` reads one (its `\w` is ASCII): `name:`,
/// `a.b:` and `key :` do; `- x:` does not.
fn is_mapping_key_line(line: &str) -> bool {
    let mut chars = line.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return false;
    }
    let rest = chars
        .as_str()
        .trim_start_matches(|c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .trim_start_matches([' ', '\t']);
    rest.strip_prefix(':')
        .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t']))
}

/// Condition 3 of §3.1.1, ported from the reference implementation's
/// `scannerReaches`: walking `lines[from..to)` as the block scanner does,
/// does it reach line `to` outside fenced code, an author note, and the body
/// of an inert or a verbatim block? The walk is reduced to the decisions that
/// consume lines, and made of this parser's own primitives, so the two agree
/// on what is code, a fence and a note. Its `section` and stack decide what
/// encloses a line, which decides whether `<!--` there opens a note.
fn scanner_reaches(lines: &[&str], from: usize, to: usize) -> bool {
    // classify's refusals are the parse's to report; this only reads.
    let mut scratch = Vec::new();
    let body_of = |kind: &str| lookup(kind).map_or(BodyKind::Markdown, |k| k.body);
    // The open containers: (fence length, kind).
    let mut stack: Vec<(usize, String)> = Vec::new();
    // The open section: (heading level, kind).
    let mut section: Option<(usize, String)> = None;
    let mut code: Option<usize> = None;
    let closes = |l: &str, n: usize| fence_close_len(l).is_some_and(|m| m >= n);
    let mut k = from;
    while k < to {
        let l = lines[k];
        if let Some(tl) = code {
            if code_fence_len(l) == Some(tl) {
                code = None;
                // A YAML or code section ends at its fence.
                if stack.is_empty()
                    && section.as_ref().is_some_and(|(_, kind)| {
                        matches!(body_of(kind), BodyKind::Yaml | BodyKind::Code)
                    })
                {
                    section = None;
                }
            }
            k += 1;
            continue;
        }
        if let Some(tl) = code_fence_len(l) {
            code = Some(tl);
            k += 1;
            continue;
        }
        let enclosing = stack
            .last()
            .map(|(_, kind)| kind.as_str())
            .or(section.as_ref().map(|(_, kind)| kind.as_str()));
        if note_opens(l) && enclosing.is_none_or(|kind| body_of(kind) == BodyKind::Markdown) {
            match note_end(lines, k, to) {
                Some(end) => {
                    k = end + 1;
                    continue;
                }
                None => return false,
            }
        }
        if stack.last().is_some_and(|(n, _)| closes(l, *n)) {
            stack.pop();
            k += 1;
            continue;
        }
        if let Some(of) = open_fence(l) {
            if of.sigil && stack.is_empty() {
                section = None;
            }
            let form = if of.is_set {
                Form::Set
            } else {
                Form::Container
            };
            let class = classify(&of.kind, of.sigil, form, 0, &mut scratch);
            // An inert or a verbatim body is read raw to its first close.
            if class == Class::Inert
                || parse_attrs(&of.attr_src, &of.kind, 0)
                    .0
                    .contains_key("verbatim")
            {
                match (k + 1..to).find(|&j| closes(lines[j], of.len)) {
                    Some(j) => {
                        k = j + 1;
                        continue;
                    }
                    None => return false,
                }
            }
            stack.push((of.len, of.kind));
            k += 1;
            continue;
        }
        if let Some(lf) = leaf_open(l) {
            if lf.sigil && stack.is_empty() {
                section = None;
            }
            k += 1;
            continue;
        }
        if stack.is_empty()
            && let Some(level) = heading_level(l)
        {
            if let Some(sec) = section_open(l) {
                section = matches!(
                    classify(&sec.kind, true, Form::Section, 0, &mut scratch),
                    Class::Block(_)
                )
                .then_some((sec.level, sec.kind));
            } else if section.as_ref().is_some_and(|(at, _)| level <= *at) {
                section = None;
            }
        }
        k += 1;
    }
    code.is_none()
}

/// Every block in the document, children included, in source order (a
/// parent before its children).
fn every_block<'a>(top: &[&'a Block]) -> Vec<&'a Block> {
    fn walk<'a>(b: &'a Block, out: &mut Vec<&'a Block>) {
        out.push(b);
        for c in &b.children {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    for b in top {
        walk(b, &mut out);
    }
    out
}

/// The identity a block declares, `kind/name`: a named block of a kind the
/// registry gives identity (`x-identity`, every prose kind from revision 1.1;
/// S26), anywhere in the document. A sub-block's name is scoped to its
/// parent, so it declares no document identity.
fn identity_of(b: &Block) -> Option<(String, String)> {
    let name = b.name.as_deref().filter(|n| !n.is_empty())?;
    lookup(&b.kind)
        .is_some_and(|k| k.identity && k.sub_of.is_none())
        .then(|| (b.kind.clone(), name.to_string()))
}

/// Identity (§3.3 rule 9): a required `name` is present, and `kind/name` is
/// unique across the WHOLE document (S26) — a named rule inside a container
/// body clashes with one at the top level. `top` are the top-level blocks,
/// `all` every block.
fn check_identity(top: &[&Block], all: &[&Block], errs: &mut Vec<Refusal>) {
    for b in top {
        // A required name is the `name` ATTRIBUTE and nothing else. A
        // `name:` key inside a YAML body is a body field that happens to be
        // called name — it does not make the block named. Set MEMBERS carry
        // their names per row and are checked as the set is parsed.
        // `requires_name`, not `identity`: a named prose block has identity,
        // an unnamed one is still a valid block.
        if lookup(&b.kind).is_some_and(|k| k.requires_name)
            && b.set_group.is_none()
            && b.name.as_deref().unwrap_or("").is_empty()
        {
            errs.push(Refusal::at(
                b.line,
                "missing-attribute",
                format!("{} requires name", b.kind),
            ));
        }
    }
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    for b in all {
        let Some(id) = identity_of(b) else { continue };
        match seen.get(&id) {
            Some(first) => errs.push(Refusal::at(
                b.line,
                "duplicate-identity",
                format!(
                    "duplicate {}/{} (first declared at line {first})",
                    id.0, id.1
                ),
            )),
            None => {
                seen.insert(id, b.line);
            }
        }
    }
}

/// Attributes whose value is text — for the model, or for a person reading
/// the document — and never a reference, so an `@` in one is prose: `if`
/// and `because` (S14, S15), `title`, `description`, a skill's `trigger`
/// (S11) and its version-1 alias `when`. Every other attribute's
/// `@`-prefixed value is a reference and must be qualified (§3.4). The
/// schema has no marker this could be derived from (an `x-reference` or
/// `x-free-text` annotation would be one), so this is the one list.
const FREE_TEXT_ATTRS: &[&str] = &["if", "because", "title", "trigger", "description"];

fn is_free_text(kind: &str, attr: &str) -> bool {
    FREE_TEXT_ATTRS.contains(&attr) || (kind == "skill" && attr == "when")
}

/// The attribute references a block carries: each `@`-prefixed value, or
/// comma-separated part of a multi-valued one (`may="@workflow/a,
/// @workflow/b"`), as `(attribute, text after the @)`.
fn attr_refs(b: &Block) -> impl Iterator<Item = (&str, &str)> {
    b.attrs
        .iter()
        .filter(|(attr, _)| !is_free_text(&b.kind, attr))
        .flat_map(|(attr, val)| {
            val.split(',')
                .filter_map(move |part| Some((attr.as_str(), part.trim().strip_prefix('@')?)))
        })
}

/// `@kind/name` references: every one must resolve to a declared block, and the
/// graph must be acyclic. References live in attribute values, on blocks at
/// any depth.
fn check_refs(blocks: &[&Block], errs: &mut Vec<Refusal>) {
    // Each identity's first declaration — the line a cycle through it names.
    let mut ids: BTreeMap<(String, String), usize> = BTreeMap::new();
    for b in blocks {
        if let Some(id) = identity_of(b) {
            ids.entry(id).or_insert(b.line);
        }
    }
    for b in blocks {
        for (attr, target) in attr_refs(b) {
            let (kind, name) = match target.split_once('/') {
                Some((k, n)) => (k.to_string(), n.to_string()),
                None => {
                    errs.push(Refusal::at(
                        b.line,
                        "attribute-value",
                        format!("{attr}=@{target} must be qualified as @kind/name"),
                    ));
                    continue;
                }
            };
            if lookup(&kind).is_some_and(|k| k.sub_of.is_some()) {
                errs.push(Refusal::at(
                    b.line,
                    "subblock-reference",
                    format!("{kind}/{name}: sub-blocks cannot be referenced"),
                ));
            } else if !ids.contains_key(&(kind.clone(), name.clone())) {
                errs.push(Refusal::at(
                    b.line,
                    "dangling-reference",
                    format!("@{kind}/{name} does not resolve — no {kind} named {name:?}"),
                ));
            }
        }
    }
    // Acyclicity across attribute refs (a block that names itself, or a cycle).
    let mut edges: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
    for b in blocks {
        let Some(from) = identity_of(b) else { continue };
        for (_, target) in attr_refs(b) {
            let Some((k, nm)) = target.split_once('/') else {
                continue;
            };
            edges
                .entry(from.clone())
                .or_default()
                .push((k.to_string(), nm.to_string()));
        }
    }
    let mut state: BTreeMap<(String, String), u8> = BTreeMap::new();
    for node in edges.keys() {
        let mut path = Vec::new();
        if let Some(cycle) = find_cycle(node, &edges, &mut state, &mut path) {
            // Appendix B's shape, as every port writes it: the line of the
            // block the cycle starts at, and the cycle spelled out
            // (`reference cycle: workflow/a → workflow/b → workflow/a`).
            // Conformance compares the line with the code, so a refusal of
            // a located condition names it.
            let line = ids.get(&cycle[0]).copied().unwrap_or(0);
            let spelled: Vec<String> = cycle.iter().map(|(k, n)| format!("{k}/{n}")).collect();
            errs.push(Refusal::at(
                line,
                "reference-cycle",
                format!("reference cycle: {}", spelled.join(" → ")),
            ));
            break;
        }
    }
}

/// Inline references in prose (§4.7): every `[[kind/name]]` and `[text](#kind/name)`
/// whose kind is a known kind must resolve to a declared block of that kind.
/// Dangling ones are refused — the class of bug a real check catches that
/// eyeballing does not. Refs inside fenced code are inert (code-suspends), and
/// so are the lines of an author note (`notes`, 0-based body lines; §3.3 rule
/// 11).
fn check_inline_refs(
    blocks: &[&Block],
    body: &str,
    base: usize,
    notes: &[(usize, usize)],
    errs: &mut Vec<Refusal>,
) {
    let ids: BTreeSet<(String, String)> = blocks.iter().filter_map(|b| identity_of(b)).collect();
    // Scan the SOURCE line by line (code fences suspend recognition, §4.7
    // rule 4) so every refusal names its line. YAML bodies rarely carry the
    // inline forms, and a `[[x/y]]` whose kind is unknown is prose, not a ref.
    let mut in_code = None::<usize>;
    for (i, line) in body.split('\n').enumerate() {
        // A note is read outside code only, so its lines are skipped before
        // a fence inside one could be taken for code.
        if notes.iter().any(|&(start, end)| (start..=end).contains(&i)) {
            continue;
        }
        if let Some(tl) = in_code {
            if code_fence_len(line) == Some(tl) {
                in_code = None;
            }
            continue;
        }
        if let Some(tl) = code_fence_len(line) {
            in_code = Some(tl);
            continue;
        }
        let mut refs: Vec<(String, String)> = Vec::new();
        scan_line_refs(line, &mut refs);
        for (kind, name) in refs {
            let line_no = base + i + 1;
            if lookup(&kind).is_some_and(|k| k.sub_of.is_some()) {
                errs.push(Refusal::at(
                    line_no,
                    "subblock-reference",
                    format!("{kind}/{name}: sub-blocks cannot be referenced"),
                ));
            } else if lookup(&kind).is_some() && !ids.contains(&(kind.clone(), name.clone())) {
                errs.push(Refusal::at(
                    line_no,
                    "dangling-reference",
                    format!("[[{kind}/{name}]] does not resolve — no {kind} named {name:?}"),
                ));
            }
        }
    }
}

fn scan_line_refs(line: &str, refs: &mut Vec<(String, String)>) {
    let mut rest = line;
    while let Some(pos) = rest.find("[[") {
        let after = &rest[pos + 2..];
        let Some(end) = after.find("]]") else { break };
        let target = after[..end].split('|').next().unwrap_or("");
        push_ref(target, refs);
        rest = &after[end + 2..];
    }
    let mut rest = line;
    while let Some(pos) = rest.find("](#") {
        let after = &rest[pos + 3..];
        let Some(end) = after.find(')') else { break };
        push_ref(&after[..end], refs);
        rest = &after[end + 1..];
    }
}

fn push_ref(target: &str, refs: &mut Vec<(String, String)>) {
    if let Some((k, n)) = target.split_once('/')
        && is_kind_token(k)
        && is_name(n)
    {
        refs.push((k.to_string(), n.to_string()));
    }
}

/// The `kind` grammar of §3.2: `[A-Za-z][A-Za-z0-9_-]*`.
fn is_kind_token(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// Depth-first search for a reference cycle reachable from `node`, returning
/// it as the nodes walked, first node repeated at the end.
fn find_cycle(
    node: &(String, String),
    edges: &BTreeMap<(String, String), Vec<(String, String)>>,
    state: &mut BTreeMap<(String, String), u8>,
    path: &mut Vec<(String, String)>,
) -> Option<Vec<(String, String)>> {
    match state.get(node) {
        // On the current path: the cycle is the path from this node's first
        // visit, closed by the node again.
        Some(1) => {
            let from = path.iter().position(|p| p == node).unwrap_or(0);
            let mut cycle = path[from..].to_vec();
            cycle.push(node.clone());
            return Some(cycle);
        }
        Some(2) => return None, // done
        _ => {}
    }
    state.insert(node.clone(), 1);
    path.push(node.clone());
    if let Some(next) = edges.get(node) {
        for n in next {
            if let Some(cycle) = find_cycle(n, edges, state, path) {
                return Some(cycle);
            }
        }
    }
    path.pop();
    state.insert(node.clone(), 2);
    None
}

/// The grants a document actually requires (the non-default families it uses),
/// for reporting. `!data`/`!override` carry a family but need no grant, so they
/// do not appear here — this is the grant surface, not the family census.
pub fn families_used(doc: &Document) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn recur(b: &Block, out: &mut BTreeSet<String>) {
        if let Some(grant) = b.grant() {
            out.insert(grant.to_string());
        }
        for c in &b.children {
            recur(c, out);
        }
    }
    for b in doc.blocks() {
        recur(b, &mut out);
    }
    out
}

/// Refuse any block whose grant is not held by `document_capabilities`.
/// Fail-closed: names the block, the grant, and the exact token to add. Keys on
/// the spec's `x-grant`, so default-rung machinery (`!data`, `!override`) passes.
pub fn check_grants(doc: &Document, granted: &BTreeSet<String>, errs: &mut Vec<Refusal>) {
    fn recur(b: &Block, granted: &BTreeSet<String>, errs: &mut Vec<Refusal>) {
        if let Some(grant) = b.grant()
            && !granted.contains(grant)
        {
            errs.push(Refusal::at(
                b.line,
                "ungranted-family",
                format!(
                    "`:::!{}` needs the `{grant}` capability — add it to \
                     `agent.document_capabilities`",
                    b.kind
                ),
            ));
        }
        for c in &b.children {
            recur(c, granted, errs);
        }
    }
    for b in doc.blocks() {
        recur(b, granted, errs);
    }
}

/// Sub-block placement (spec §5.4): a sub-block appears only inside its parent.
/// A top-level sub-block, or one inside the wrong parent, is refused naming the
/// parent it needs.
fn check_placement(blocks: &[&Block], errs: &mut Vec<Refusal>) {
    fn walk(b: &Block, parent_kind: Option<&str>, errs: &mut Vec<Refusal>) {
        if let Some(want) = lookup(&b.kind).and_then(|k| k.sub_of.as_deref())
            && parent_kind != Some(want)
        {
            errs.push(Refusal::at(
                b.line,
                "subblock-out-of-place",
                format!("{} is valid only inside a {want}", b.kind),
            ));
        }
        for c in &b.children {
            walk(c, Some(b.kind.as_str()), errs);
        }
    }
    for b in blocks {
        walk(b, None, errs);
    }
}

/// Attribute values (validate.ts): every value the schema constrains by an
/// `enum` or a `pattern`, on every block — a set's members included — at the
/// block's line, and on each front-matter `parameters[]` entry, a `param`
/// declared where there is no block line. A variant kind written with no
/// condition (`minProperties`) is refused here too.
fn check_attr_values(all: &[&Block], front: &BTreeMap<String, Value>, errs: &mut Vec<Refusal>) {
    for b in all {
        for (attr, value) in &b.attrs {
            if let Some(why) = attr_value_problem(&b.kind, attr, &Value::String(value.clone())) {
                errs.push(Refusal::at(b.line, "attribute-value", why));
            }
        }
        // The reference parser reports this through the schema (code
        // `schema`); `missing-attribute` is the Appendix B condition.
        if b.attrs.is_empty() && registry().needs_attrs(&b.kind) {
            errs.push(Refusal::at(
                b.line,
                "missing-attribute",
                format!(
                    "{kind} requires at least one condition — write :::{kind}{{key=\"value\"}}",
                    kind = b.kind
                ),
            ));
        }
    }
    let params = front.get("parameters").and_then(Value::as_array);
    for entry in params.into_iter().flatten().filter_map(Value::as_object) {
        for (attr, value) in entry {
            if let Some(why) = attr_value_problem("param", attr, value) {
                errs.push(Refusal::new("attribute-value", why));
            }
        }
    }
}

/// Why `value` is not admissible for `kind`'s attribute `attr`, if it is not:
/// outside its `enum`, or not matching its `pattern`. Every pattern the
/// registry carries is the `@kind/name` reference grammar (a registry test
/// holds it so), which [`is_reference`] matches without a regex engine.
fn attr_value_problem(kind: &str, attr: &str, value: &Value) -> Option<String> {
    let rule = registry().attr_rule(kind, attr)?;
    if let Some(values) = &rule.values
        && !value
            .as_str()
            .is_some_and(|v| values.iter().any(|a| a == v))
    {
        return Some(format!("{attr} must be one of: {}", values.join(", ")));
    }
    if rule.pattern.is_some() && value.as_str().is_some_and(|v| !is_reference(v)) {
        return Some(format!("{attr} must be a reference written @kind/name"));
    }
    None
}

/// `@kind/name`, as `x-grammar.attrRef` matches it.
fn is_reference(s: &str) -> bool {
    s.strip_prefix('@')
        .and_then(|r| r.split_once('/'))
        .is_some_and(|(k, n)| is_kind_token(k) && is_name(n))
}

/// Overrides (S24): a rule may name rules it overrides (`overrides="kind/
/// name"`), but never a guardrail and never a rule stronger than itself. A
/// target is looked up among this document's blocks; one not found here is
/// not refused (it may be in an included document, which delivery looks in).
/// The refusal names the overriding block's line.
fn check_overrides(all: &[&Block], errs: &mut Vec<Refusal>) {
    let reg = registry();
    let ids: BTreeSet<(String, String)> = all.iter().filter_map(|b| identity_of(b)).collect();
    for b in all {
        let Some(own) = reg.strength(&b.kind) else {
            continue;
        };
        let Some(list) = b.attrs.get("overrides") else {
            continue;
        };
        for target in list.split(',').map(str::trim) {
            let Some((kind, name)) = target.split_once('/') else {
                continue;
            };
            if !ids.contains(&(kind.to_string(), name.to_string())) {
                continue;
            }
            if kind == "guardrail" {
                errs.push(Refusal::at(
                    b.line,
                    "override-guardrail",
                    format!("{target} is a guardrail — a guardrail is never overridden"),
                ));
            } else if reg.strength(kind).unwrap_or(0) > own {
                errs.push(Refusal::at(
                    b.line,
                    "override-stronger",
                    format!("a {} may not override the stronger {target}", b.kind),
                ));
            }
        }
    }
}

// ── extraction: folding blocks into configuration + delivered prose ──────────

/// A skill lifted from the document into the catalogue.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineSkill {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub body: String,
}

/// What a document yields once folded: the delivered prose the model reads, a
/// configuration fragment to merge into the agent document, the skills lifted
/// into the catalogue, and the extended-family declarations recorded by kind
/// (parsed, grant-checked, and visible in `--capabilities`, with their runtime
/// effect delegated to services per the spec).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Extraction {
    pub cleaned: String,
    pub config: serde_json::Map<String, Value>,
    /// Root-level `workflows[]` entries — spliced into the document array, not
    /// folded under a section.
    pub workflows: Vec<Value>,
    pub skills: Vec<InlineSkill>,
    pub declarations: BTreeMap<String, Vec<Value>>,
    /// The grants this document actually required, for introspection.
    pub families: Vec<String>,
}

/// Parse and fold an instruction document in one step — the entry point the
/// config loader and the subagent-template compiler call. `granted` is the
/// operator's `document_capabilities`; the trust ladder refuses any block whose
/// family is not in it.
pub fn extract(text: &str, granted: &BTreeSet<String>) -> Result<Extraction, Vec<Refusal>> {
    let doc = parse(text)?;
    fold(&doc, granted)
}

/// As [`extract`], with runtime FACTS for `when` selection (§5.2) — e.g. the
/// consuming runtime's own `agent` name. The library itself assumes no facts.
pub fn extract_with_facts(
    text: &str,
    granted: &BTreeSet<String>,
    facts: &BTreeMap<String, String>,
) -> Result<Extraction, Vec<Refusal>> {
    let doc = parse(text)?;
    fold_full(
        &doc,
        granted,
        &BTreeMap::new(),
        facts,
        &|_| None,
        0,
        &BTreeSet::new(),
    )
}

/// Merge a fragment UNDER a document: a key already present in `into` wins, so
/// an explicit config key always beats what a directive contributed. Arrays of
/// the same key concatenate (fragment first) so a document's `!mcp` servers add
/// to, rather than replace, any `mcp.servers` written explicitly.
pub fn merge_missing(
    into: &mut serde_json::Map<String, Value>,
    add: serde_json::Map<String, Value>,
) {
    for (k, v) in add {
        match (into.get_mut(&k), v) {
            (Some(Value::Object(d)), Value::Object(s)) => merge_missing(d, s),
            (Some(Value::Array(have)), Value::Array(mut more)) => {
                more.extend(std::mem::take(have));
                *have = more;
            }
            (Some(_), _) => {}
            (None, v) => {
                into.insert(k, v);
            }
        }
    }
}

fn frag<'a>(
    cfg: &'a mut serde_json::Map<String, Value>,
    key: &str,
) -> &'a mut serde_json::Map<String, Value> {
    cfg.entry(key)
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .expect("fragment section is an object")
}

fn push_into<'a>(
    cfg: &'a mut serde_json::Map<String, Value>,
    section: &str,
    list: &str,
) -> &'a mut Vec<Value> {
    frag(cfg, section)
        .entry(list)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .expect("list is an array")
}

/// A machinery block's body parsed as a YAML mapping, with `{attr}` merged over
/// it and a `name` guaranteed present when required.
fn body_map(b: &Block, errs: &mut Vec<Refusal>) -> Option<serde_json::Map<String, Value>> {
    let mut m = if b.body.trim().is_empty() {
        serde_json::Map::new()
    } else {
        match crate::yaml::parse(&b.body) {
            Ok(Value::Object(m)) => m,
            Ok(_) => {
                errs.push(Refusal::at(
                    b.line,
                    "invalid-yaml-body",
                    format!(":::!{} body must be a YAML mapping", b.kind),
                ));
                return None;
            }
            Err(e) => {
                errs.push(Refusal::at(
                    b.line,
                    "invalid-yaml-body",
                    format!(":::!{} body is not valid YAML: {e}", b.kind),
                ));
                return None;
            }
        }
    };
    for (k, v) in &b.attrs {
        // The fence wins over a same-named body key.
        m.insert(k.clone(), attr_scalar(&b.kind, k, v));
    }
    Some(m)
}

/// Resolve the `@kind/name` references a workflow step carries in its `to`,
/// `schema` and `template` fields (§3.4) to the concrete value the declared
/// block provides: `@human/x` → the human's channel/principal, `@ui/x` → the
/// ui's `schema` sub-block, `@agent/x` → the agent's `template`. A reference to
/// an undeclared block is left as written for the runtime to report.
fn resolve_workflow_refs(workflows: &mut [Value], doc: &Document) {
    let mut human_to: BTreeMap<String, String> = BTreeMap::new();
    let mut ui_schema: BTreeMap<String, Value> = BTreeMap::new();
    let mut agent_template: BTreeMap<String, String> = BTreeMap::new();
    for b in doc.blocks() {
        let Some(name) = b.name.clone() else { continue };
        match b.kind.as_str() {
            "human" => {
                if let Some(to) = b.attrs.get("channel").or_else(|| b.attrs.get("principal")) {
                    human_to.insert(name, to.clone());
                }
            }
            "ui" => {
                if let Some(schema) = b.children.iter().find(|c| c.kind == "schema")
                    && let Ok(v) = crate::yaml::parse(&schema.body)
                {
                    ui_schema.insert(name, v);
                }
            }
            "agent" => {
                if let Some(t) = b.attrs.get("template") {
                    agent_template.insert(name, t.clone());
                }
            }
            _ => {}
        }
    }
    for wf in workflows {
        let Some(steps) = wf.get_mut("steps").and_then(Value::as_object_mut) else {
            continue;
        };
        for step in steps.values_mut() {
            let Some(obj) = step.as_object_mut() else {
                continue;
            };
            if let Some(name) = obj
                .get("to")
                .and_then(Value::as_str)
                .and_then(|s| s.strip_prefix("@human/"))
                .map(str::to_string)
                && let Some(to) = human_to.get(&name)
            {
                obj.insert("to".into(), Value::String(to.clone()));
            }
            if let Some(name) = obj
                .get("schema")
                .and_then(Value::as_str)
                .and_then(|s| s.strip_prefix("@ui/"))
                .map(str::to_string)
                && let Some(schema) = ui_schema.get(&name)
            {
                obj.insert("schema".into(), schema.clone());
            }
            if let Some(name) = obj
                .get("template")
                .and_then(Value::as_str)
                .and_then(|s| s.strip_prefix("@agent/"))
                .map(str::to_string)
                && let Some(t) = agent_template.get(&name)
            {
                obj.insert("template".into(), Value::String(t.clone()));
            }
        }
    }
}

/// Rewrite `@secret-ref/NAME` string values (§3.4) to agentd's own secret
/// reference form, chosen by the declared `!secret-ref`'s kind: a `kind=file`
/// ref resolves from the mounted file at its `path` (`{{secret-file:PATH}}`);
/// any other kind resolves the named value (`{{secret:NAME}}`). A reference to
/// an undeclared secret-ref falls back to `{{secret:NAME}}`.
fn resolve_secret_refs(v: &mut Value, refs: &BTreeMap<String, (String, String)>) {
    match v {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix("@secret-ref/")
                && !name.is_empty()
            {
                *s = match refs.get(name) {
                    Some((kind, path)) if kind == "file" && !path.is_empty() => {
                        format!("{{{{secret-file:{path}}}}}")
                    }
                    _ => format!("{{{{secret:{name}}}}}"),
                };
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| resolve_secret_refs(x, refs)),
        Value::Object(m) => m.values_mut().for_each(|x| resolve_secret_refs(x, refs)),
        _ => {}
    }
}

/// The declared `!secret-ref` blocks (name → (kind, path)).
fn secret_ref_decls(doc: &Document) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for b in doc.blocks() {
        if b.kind == "secret-ref"
            && let Some(name) = b.name.clone().or_else(|| b.attrs.get("name").cloned())
        {
            let kind = b.attrs.get("kind").cloned().unwrap_or_default();
            let path = b.attrs.get("path").cloned().unwrap_or_default();
            out.insert(name, (kind, path));
        }
    }
    out
}

fn attr_value(s: &str) -> Value {
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        // A structured attribute/cell value is a YAML flow mapping or sequence
        // written inline (§4.3.1) — parse it so a typed field (a stream's
        // `retention`, a test case's `given`) receives a mapping, not a string.
        _ if (s.starts_with('{') && s.ends_with('}'))
            || (s.starts_with('[') && s.ends_with(']')) =>
        {
            match crate::yaml::parse(s) {
                Ok(v @ (Value::Object(_) | Value::Array(_))) => v,
                _ => Value::String(s.to_string()),
            }
        }
        _ => s
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(s.to_string())),
    }
}

/// An attribute's value, expanded to an array where the schema marks the
/// kind's attribute `x-multivalued` (comma-separated within one value).
fn attr_scalar(kind: &str, key: &str, s: &str) -> Value {
    if registry().is_multivalued(kind, key) {
        Value::Array(
            s.split(',')
                .map(|p| Value::String(p.trim().to_string()))
                .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                .collect(),
        )
    } else {
        attr_value(s)
    }
}

/// Resolves an `include`'s id or uri to the included document's source, or
/// `None` when the reader may not see it or it does not exist (§5.2 rule 2).
pub type IncludeResolver<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The include depth cap (§5.2 rule 3); deeper transclusion degrades to a note.
const INCLUDE_DEPTH_CAP: usize = 8;

/// Fold a parsed document into configuration + delivered prose, after checking
/// grants. The whole document is refused if any block errors — no partial load.
pub fn fold(doc: &Document, granted: &BTreeSet<String>) -> Result<Extraction, Vec<Refusal>> {
    fold_with_params(doc, granted, &BTreeMap::new())
}

/// As [`fold`], with an explicit override map for `${parameter}` substitution.
/// Includes are not resolved (they degrade to a note); use [`fold_full`] with a
/// resolver to transclude.
pub fn fold_with_params(
    doc: &Document,
    granted: &BTreeSet<String>,
    overrides: &BTreeMap<String, String>,
) -> Result<Extraction, Vec<Refusal>> {
    fold_full(
        doc,
        granted,
        overrides,
        &BTreeMap::new(),
        &|_| None,
        0,
        &BTreeSet::new(),
    )
}

/// The full fold: `${parameter}` overrides, and an `include` resolver so a
/// document can transclude others (§5.2), recursively, resolved with their own
/// parameters. The delivered text runs the §3.5 pipeline: prose degraded to its
/// Appendix A form, machinery replaced by its acknowledgement line (a set as
/// one line), includes inlined, inline references degraded, and `${}`
/// substituted LAST so a value is never re-parsed as Markdown.
pub fn fold_full(
    doc: &Document,
    granted: &BTreeSet<String>,
    overrides: &BTreeMap<String, String>,
    facts: &BTreeMap<String, String>,
    resolve: IncludeResolver,
    depth: usize,
    seen: &BTreeSet<String>,
) -> Result<Extraction, Vec<Refusal>> {
    let mut errs = Vec::new();
    check_grants(doc, granted, &mut errs);
    let mut out = Extraction {
        families: families_used(doc).into_iter().collect(),
        ..Extraction::default()
    };
    // The parameter context is needed during the walk to select `when`
    // variants, again at the end for `${}` substitution, and (as full
    // declarations) to render a `form` block's input list.
    let params = param_values(doc, overrides);
    let decls = param_decls(doc);
    // `when` evaluates over the resolved parameters PLUS the runtime facts
    // (facts win a collision — they are runtime-authoritative); `${}`
    // substitution uses the parameters only.
    let mut when_facts = params.clone();
    for (k, v) in facts {
        when_facts.insert(k.clone(), v.clone());
    }

    // Pass 1: fold configuration, and record each TOP-LEVEL block's delivered
    // lines against the source region it occupies. Consecutive members of one
    // set are gathered so the set delivers a single line for its whole region.
    let nodes = &doc.nodes;
    let mut regions: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut i = 0;
    while i < nodes.len() {
        match &nodes[i] {
            // An author note rides as text until delivery strips it.
            Node::Text(_) | Node::Note { .. } => i += 1,
            // An inert block delivers its body, fences gone.
            Node::Inert { region, body, .. } => {
                regions.push((region.0, region.1, body_lines(body)));
                i += 1;
            }
            Node::Block(b) if b.set_group.is_some() => {
                let g = b.set_group;
                let mut members = vec![b];
                let mut j = i + 1;
                while let Some(Node::Block(bb)) = nodes.get(j) {
                    if bb.set_group == g {
                        members.push(bb);
                        j += 1;
                    } else {
                        break;
                    }
                }
                for m in &members {
                    fold_config(m, &mut out, &mut errs);
                }
                regions.push((b.region.0, b.region.1, deliver_set_lines(&members)));
                i = j;
            }
            Node::Block(b) if matches!(b.form, Form::Keyword | Form::Alert) => {
                // A lifted keyword/alert delivers its RAW source lines through
                // the same normalizer the un-lifted text pass applies — so the
                // delivered bytes are identical by construction to when these
                // lines rode in a prose run.
                let src: Vec<String> = doc.source.split('\n').collect::<Vec<_>>()
                    [b.region.0..=b.region.1]
                    .iter()
                    .map(|l| l.to_string())
                    .collect();
                regions.push((b.region.0, b.region.1, normalize_lines(src)));
                i += 1;
            }
            Node::Block(b) => {
                fold_config(b, &mut out, &mut errs);
                let mut delivered =
                    deliver_block_lines(b, &when_facts, &decls, granted, resolve, depth, seen);
                // A rule container's region covers its reason (S14). Until
                // delivery renders reasons, the reason's source lines are
                // delivered as they were when they were prose after the
                // block: a blank line when blanks separated them (runs of
                // blanks collapse to one anyway), then the lines verbatim.
                if let Some((start, end)) = b.reason {
                    let src: Vec<&str> = doc.source.split('\n').collect();
                    if src[start - 1].trim().is_empty() {
                        delivered.push(String::new());
                    }
                    delivered.extend(src[start..=end].iter().map(|l| l.to_string()));
                }
                regions.push((b.region.0, b.region.1, delivered));
                i += 1;
            }
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }

    // Resolve the workflow-step references that name a declared block (§3.4):
    // `to: @human/x` → the human's channel, `schema: @ui/x` → the ui's schema
    // sub-block, `template: @agent/x` → the agent's template.
    resolve_workflow_refs(&mut out.workflows, doc);

    // Rewrite `@secret-ref/name` references anywhere in the folded config and
    // workflows to agentd's secret form, chosen by the secret-ref's kind — a
    // post-pass, so the declaration is seen regardless of block order.
    let secret_refs = secret_ref_decls(doc);
    let mut cfg = Value::Object(std::mem::take(&mut out.config));
    resolve_secret_refs(&mut cfg, &secret_refs);
    if let Value::Object(m) = cfg {
        out.config = m;
    }
    for wf in &mut out.workflows {
        resolve_secret_refs(wf, &secret_refs);
    }

    // Pass 2 (§3.5 layout): rebuild the delivered text line by line — each
    // block's region is REPLACED by its delivered form; every other line
    // (prose, headings, blank lines) is delivered unchanged.
    let body_lines: Vec<&str> = doc.source.split('\n').collect();
    let mut lines: Vec<String> = Vec::new();
    let mut li = 0;
    let mut ri = 0;
    while li < body_lines.len() {
        if ri < regions.len() && regions[ri].0 == li {
            lines.extend(regions[ri].2.iter().cloned());
            li = regions[ri].1 + 1;
            ri += 1;
        } else {
            lines.push(body_lines[li].to_string());
            li += 1;
        }
    }

    // Pass 3 (§3.5 steps 4–6): keyword/alert forms normalize to their bold
    // delivered form; runs of blank lines collapse to one and the document's
    // leading/trailing blanks are trimmed; inline references degrade; and
    // `${}` is substituted LAST so a value is never re-parsed.
    let lines = normalize_lines(lines);
    let text = collapse_blanks(lines).join("\n");
    let text = degrade_inline_refs(&text);
    let mut text = substitute_params(&text, &params);
    // The delivered text ends with exactly one newline (§3.5) — part of the
    // bytes the delivery digest covers.
    if !text.is_empty() {
        text.push('\n');
    }
    out.cleaned = text;
    Ok(out)
}

/// Fold one block into CONFIGURATION only (delivery is separate). Prose and
/// structural blocks contribute no configuration.
fn fold_config(b: &Block, out: &mut Extraction, errs: &mut Vec<Refusal>) {
    if b.disposition == Disposition::Machinery {
        fold_machinery(b, out, errs);
    }
}

/// The lines a block delivers, replacing its source region (§3.5 steps 4–5).
#[allow(clippy::too_many_arguments)]
fn deliver_block_lines(
    b: &Block,
    params: &BTreeMap<String, String>,
    decls: &BTreeMap<String, BTreeMap<String, String>>,
    granted: &BTreeSet<String>,
    resolve: IncludeResolver,
    depth: usize,
    seen: &BTreeSet<String>,
) -> Vec<String> {
    match b.disposition {
        Disposition::Prose => deliver_prose_lines(b, decls),
        Disposition::Structural if b.kind == "when" => {
            // A KEPT `when` delivers its body unwrapped; a dropped one nothing.
            if when_kept(b, params) {
                body_lines(b.delivery_body())
            } else {
                Vec::new()
            }
        }
        Disposition::Structural if b.kind == "include" => {
            deliver_include(b, granted, params, resolve, depth, seen)
        }
        // A structural kind with no body (`param`) delivers nothing. One that
        // has a body and no delivery semantics here yet (`unless`,
        // `otherwise`, registered in revision 1.1) delivers that body
        // unwrapped, as it did when it was an unknown bare kind: a catch-all
        // that delivered nothing would silently drop the guidance inside —
        // typically the safety fallback an `otherwise` carries — while the
        // load still reported success. Keyed on the schema's `x-body`, so a
        // structural kind a later registry adds keeps its text too.
        Disposition::Structural => match lookup(&b.kind).map(|k| k.body) {
            Some(BodyKind::None) => Vec::new(),
            _ => body_lines(b.delivery_body()),
        },
        Disposition::Machinery => machinery_ack(b).into_iter().collect(),
    }
}

/// Transclude an `include` (§5.2): resolve the referenced document, deliver it
/// with its OWN parameters, and inline its lines. An unavailable document, a
/// cycle, or a too-deep include degrades to a visible note rather than looping
/// or revealing existence.
fn deliver_include(
    b: &Block,
    granted: &BTreeSet<String>,
    facts: &BTreeMap<String, String>,
    resolve: IncludeResolver,
    depth: usize,
    seen: &BTreeSet<String>,
) -> Vec<String> {
    let Some(id) = b.attrs.get("id").or_else(|| b.attrs.get("uri")).cloned() else {
        return vec!["> _(included instruction not available)_".into()];
    };
    if depth >= INCLUDE_DEPTH_CAP || seen.contains(&id) {
        return vec!["> _(included instruction not available: cycle or depth cap)_".into()];
    }
    let Some(text) = resolve(&id) else {
        return vec!["> _(included instruction not available)_".into()];
    };
    let mut seen2 = seen.clone();
    seen2.insert(id);
    let inlined = parse(&text).and_then(|d| {
        fold_full(
            &d,
            granted,
            &BTreeMap::new(),
            facts,
            resolve,
            depth + 1,
            &seen2,
        )
    });
    match inlined {
        Ok(ex) => ex
            .cleaned
            .trim_end_matches('\n')
            .split('\n')
            .map(str::to_string)
            .collect(),
        Err(_) => vec!["> _(included instruction not available)_".into()],
    }
}

/// A body split into delivered lines, with outer blank lines trimmed but inner
/// line breaks preserved.
fn body_lines(body: &str) -> Vec<String> {
    let t = body.trim_matches('\n');
    if t.is_empty() {
        Vec::new()
    } else {
        t.split('\n').map(str::to_string).collect()
    }
}

/// Whether a `when` variant is kept for this reader (§5.2 rules 2–3):
/// conditions AND together; a condition's value is a comma-separated SET of
/// admissible values (`agent="claude, gpt"` matches either); and a condition
/// whose key is UNKNOWN to this reader KEEPS the content — a host that cannot
/// evaluate a dimension keeps the guidance rather than censoring it.
pub(crate) fn when_kept(b: &Block, facts: &BTreeMap<String, String>) -> bool {
    b.attrs.iter().all(|(k, allowed)| match facts.get(k) {
        None => true,
        Some(actual) => allowed.split(',').any(|v| v.trim() == actual),
    })
}

/// A set of machinery delivers ONE line naming its members (§4.3 / Appendix A);
/// a set of a kind that delivers nothing (no `x-acknowledgement`) is silent.
fn deliver_set_lines(members: &[&Block]) -> Vec<String> {
    let first = members[0];
    let Some(k) = lookup(&first.kind) else {
        return Vec::new();
    };
    if k.ack.is_none() {
        return Vec::new(); // e.g. a `param[]` set, or a non-delivering kind
    }
    let nouns = k
        .nouns
        .clone()
        .unwrap_or_else(|| format!("{}s", first.kind));
    let names: Vec<&str> = members.iter().filter_map(|m| m.name.as_deref()).collect();
    vec![format!(
        "[{} {nouns} are declared: {}]",
        names.len(),
        names.join(", ")
    )]
}

/// A machinery block's acknowledgement line from the schema's `x-acknowledgement`
/// template, or `None` for a kind that delivers nothing (§3.5 step 5).
fn machinery_ack(b: &Block) -> Option<String> {
    let kind = lookup(&b.kind)?;
    let tmpl = kind.ack.as_deref()?;
    // Identity is the `name` attribute (`x-identity`); a block that reaches
    // delivery without one was already refused at parse.
    let name = b
        .name
        .clone()
        .or_else(|| b.attrs.get("name").cloned())
        .unwrap_or_default();
    let path = b.attrs.get("path").cloned().unwrap_or_default();
    let target = b.attrs.get("target").cloned().unwrap_or_default();
    Some(
        tmpl.replace("{name}", &name)
            .replace("{path}", &path)
            .replace("{target}", &target),
    )
}

/// Prose degrades to its delivered form (Appendix A): a normativity keyword
/// becomes `**KEYWORD:** body` (label on the first body line only);
/// `example`/`context`/`form`/`tool`/`glossary` take their own shapes. (An
/// unknown bare name is no block: its body is an inert node's, §3.3 rule 2.)
fn deliver_prose_lines(
    b: &Block,
    decls: &BTreeMap<String, BTreeMap<String, String>>,
) -> Vec<String> {
    let title = b.attrs.get("title").cloned();
    let body = body_lines(b.delivery_body());
    match b.kind.as_str() {
        "context" => {
            let t = title.map(|t| format!(" title=\"{t}\"")).unwrap_or_default();
            let mut out = vec![format!("<reference{t}>")];
            out.extend(body);
            out.push("</reference>".into());
            out
        }
        "example" => {
            let t = title.map(|t| format!(" — {t}")).unwrap_or_default();
            let mut out = vec![format!("**EXAMPLE{t}:**")];
            out.extend(body);
            out
        }
        "form" => {
            // The body is a capture TEMPLATE for an editor, not delivered
            // (§5.1). The delivered text is a list of the parameters the body
            // references, in first-reference order, with their metadata.
            let t = title.map(|t| format!(" — {t}")).unwrap_or_default();
            let mut out = vec![format!("**Inputs to collect{t}**")];
            for name in extract_param_refs(&b.body) {
                let mut line = format!("- **{name}**");
                let mut segs: Vec<String> = Vec::new();
                if let Some(d) = decls.get(&name) {
                    if let Some(desc) = d.get("description").filter(|s| !s.is_empty()) {
                        segs.push(desc.clone());
                    }
                    if d.get("required").map(|v| v != "false").unwrap_or(false) {
                        segs.push("required".into());
                    }
                    if let Some(values) = d.get("values").filter(|s| !s.is_empty()) {
                        segs.push(format!("one of: {values}"));
                    }
                    if let Some(def) = d.get("default").filter(|s| !s.is_empty()) {
                        segs.push(format!("default: {def}"));
                    }
                }
                if !segs.is_empty() {
                    line.push_str(&format!(" — {}", segs.join("; ")));
                }
                out.push(line);
            }
            out
        }
        "tool" => {
            let cap = b.attrs.get("cap").cloned().unwrap_or_default();
            // The label is explicit, else the block's name, else the last path
            // segment of the capability, title-cased.
            let label = b
                .attrs
                .get("label")
                .or(b.name.as_ref())
                .cloned()
                .or_else(|| {
                    cap.rsplit(['/', ':'])
                        .find(|s| !s.is_empty())
                        .map(title_case)
                });
            let mut head = label
                .map(|l| format!("**Tool — {l}** (`{cap}`)"))
                .unwrap_or_else(|| format!("**Tool** (`{cap}`)"));
            if let Some(allow) = b.attrs.get("allow").filter(|a| !a.is_empty()) {
                head.push_str(&format!(" — allowed: {allow}"));
            }
            if let Some(deny) = b.attrs.get("deny").filter(|d| !d.is_empty()) {
                head.push_str(&format!("; denied: {deny}"));
            }
            let mut out = vec![head];
            out.extend(body);
            out
        }
        "glossary" => deflist_entries(&b.body)
            .into_iter()
            .map(|(term, def)| format!("**{term}** — {def}"))
            .collect(),
        k => {
            // The keyword prefixes the FIRST body line; the rest are unchanged.
            // `:::should{not}` is the container spelling of `SHOULD NOT:` and
            // keeps its polarity the same way, through the negated label.
            let negated = b
                .attrs
                .get("not")
                .is_some_and(|v| v.is_empty() || v == "true");
            let kw = match registry().negated_label(k) {
                Some(label) if negated => label.to_string(),
                _ => k.to_uppercase(),
            };
            let mut out = Vec::new();
            for (n, line) in body.iter().enumerate() {
                if n == 0 {
                    out.push(format!("**{kw}:** {}", line.trim_start()));
                } else {
                    out.push(line.clone());
                }
            }
            if out.is_empty() {
                out.push(format!("**{kw}:**"));
            }
            out
        }
    }
}

/// Upper-case the first character of a word (`ticketing` → `Ticketing`).
fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Parse a definition-list body into `(term, definition)` pairs (for
/// `glossary` delivery).
pub(crate) fn deflist_entries(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut term: Option<String> = None;
    let mut def: Vec<String> = Vec::new();
    let flush =
        |term: &mut Option<String>, def: &mut Vec<String>, out: &mut Vec<(String, String)>| {
            if let Some(t) = term.take() {
                out.push((t, def.join(" ").trim().to_string()));
            }
            def.clear();
        };
    for line in body.lines() {
        if let Some(rest) = line.trim_start().strip_prefix(':') {
            def.push(rest.trim().to_string());
        } else if line.trim().is_empty() {
            flush(&mut term, &mut def, &mut out);
        } else if term.is_some() && line.starts_with([' ', '\t']) {
            def.push(line.trim().to_string());
        } else {
            flush(&mut term, &mut def, &mut out);
            let name = line.split('{').next().unwrap_or(line).trim().to_string();
            if !name.is_empty() {
                term = Some(name);
            }
        }
    }
    flush(&mut term, &mut def, &mut out);
    out
}

/// The parameter values available for `${}` substitution: declared defaults
/// (front-matter `parameters` and `param` blocks), with `overrides` winning.
/// The full parameter declarations (name → attributes), from front-matter
/// `parameters` and `param` blocks — used to render a `form` block's input
/// list (description/required/values/default).
pub(crate) fn param_decls(doc: &Document) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    if let Some(Value::Array(params)) = doc.front.get("parameters") {
        for p in params {
            if let Some(obj) = p.as_object()
                && let Some(name) = obj.get("name").and_then(Value::as_str)
            {
                let m = obj
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| v.to_string()),
                        )
                    })
                    .collect();
                out.insert(name.to_string(), m);
            }
        }
    }
    for b in doc.blocks() {
        if b.kind == "param"
            && let Some(name) = b.name.clone().or_else(|| b.attrs.get("name").cloned())
        {
            out.insert(name, b.attrs.clone());
        }
    }
    out
}

/// The `${name}` parameters a body references, in first-reference order,
/// de-duplicated.
fn extract_param_refs(body: &str) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(pos) = rest.find("${") {
        let after = &rest[pos + 2..];
        let Some(end) = after.find('}') else { break };
        let name = &after[..end];
        if !name.is_empty() && seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
        rest = &after[end + 1..];
    }
    out
}

pub(crate) fn param_values(
    doc: &Document,
    overrides: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut v = BTreeMap::new();
    if let Some(Value::Array(params)) = doc.front.get("parameters") {
        for p in params {
            if let (Some(name), Some(def)) = (
                p.get("name").and_then(Value::as_str),
                p.get("default").and_then(Value::as_str),
            ) {
                v.insert(name.to_string(), def.to_string());
            }
        }
    }
    for b in doc.blocks() {
        if b.kind == "param"
            && let Some(name) = b
                .name
                .as_deref()
                .or_else(|| b.attrs.get("name").map(String::as_str))
            && let Some(def) = b.attrs.get("default")
        {
            v.insert(name.to_string(), def.clone());
        }
    }
    for (k, val) in overrides {
        v.insert(k.clone(), val.clone());
    }
    v
}

/// Substitute `${name}` with its value (§3.5 step 6) — inserted as plain text,
/// never re-parsed. An undeclared `${x}` is left verbatim for the resolver to
/// report.
fn substitute_params(text: &str, params: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("${") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        if let Some(end) = after.find('}') {
            let name = &after[..end];
            match params.get(name) {
                Some(val) => out.push_str(val),
                None => {
                    out.push_str("${");
                    out.push_str(name);
                    out.push('}');
                }
            }
            rest = &after[end + 1..];
        } else {
            out.push_str("${");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Degrade inline references in the delivered text (Appendix A): `[[kind/name]]`
/// and `[[kind/name|Label]]` to the label (or `name`), and `[Label](#kind/name)`
/// to `Label`. References inside fenced code are left untouched.
fn degrade_inline_refs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = None::<usize>;
    for line in text.split_inclusive('\n') {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        if let Some(tl) = in_code {
            if code_fence_len(bare) == Some(tl) {
                in_code = None;
            }
            out.push_str(line);
            continue;
        }
        if let Some(tl) = code_fence_len(bare) {
            in_code = Some(tl);
            out.push_str(line);
            continue;
        }
        out.push_str(&degrade_line_refs(line));
    }
    out
}

fn degrade_line_refs(line: &str) -> String {
    // [[kind/name]] or [[kind/name|Label]] → Label or name.
    let mut s = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find("[[") {
        let after = &rest[pos + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let inner = &after[..end];
        let shown = match inner.split_once('|') {
            Some((target, label)) => {
                if is_ref_target(target) {
                    label.to_string()
                } else {
                    format!("[[{inner}]]")
                }
            }
            None => match inner.split_once('/') {
                Some((_, name)) if is_ref_target(inner) => name.to_string(),
                _ => format!("[[{inner}]]"),
            },
        };
        s.push_str(&rest[..pos]);
        s.push_str(&shown);
        rest = &after[end + 2..];
    }
    s.push_str(rest);
    // [Label](#kind/name) → Label.
    let mut t = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else { break };
        let label = &after[..close];
        let tail = &after[close + 1..];
        if let Some(frag) = tail.strip_prefix("(#")
            && let Some(fend) = frag.find(')')
            && is_ref_target(&frag[..fend])
        {
            t.push_str(&rest[..open]);
            t.push_str(label);
            rest = &frag[fend + 1..];
            continue;
        }
        // Not a fragment reference — keep the `[` and continue past it.
        t.push_str(&rest[..open + 1]);
        rest = after;
    }
    t.push_str(rest);
    t
}

/// Whether `kind/name` names a known kind (so it is a reference, not prose).
fn is_ref_target(s: &str) -> bool {
    matches!(s.split_once('/'), Some((k, n)) if is_kind_token(k) && is_name(n) && lookup(k).is_some())
}

/// Normalize keyword lines (`MUST: …`, `**NEVER:**`, list items) and blockquote
/// alerts (`> [!TIP]`) to their delivered bold form (Appendix A), driven by the
/// schema's keyword map. The keyword label prefixes the FIRST line only;
/// continuation lines keep their breaks (an alert's `> ` prefixes are removed).
/// Fenced code is left untouched.
fn normalize_lines(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_code = None::<usize>;
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        if let Some(tl) = in_code {
            if code_fence_len(line) == Some(tl) {
                in_code = None;
            }
            out.push(line.clone());
            i += 1;
            continue;
        }
        if let Some(tl) = code_fence_len(line) {
            in_code = Some(tl);
            out.push(line.clone());
            i += 1;
            continue;
        }
        // An alert: `> [!KIND]`, then the `>`-quoted body. The label prefixes
        // the first body line; the rest keep their own lines, `> ` stripped.
        if let Some(kw) = alert_open_kind(line) {
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim_start();
                if let Some(rest) = t.strip_prefix('>') {
                    body.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                    i += 1;
                } else {
                    break;
                }
            }
            for (n, l) in body.iter().enumerate() {
                out.push(if n == 0 {
                    format!("**{kw}:** {}", l.trim_start())
                } else {
                    l.clone()
                });
            }
            continue;
        }
        out.push(normalize_keyword_line(line).unwrap_or_else(|| line.clone()));
        i += 1;
    }
    out
}

/// Collapse runs of two or more blank lines into one, and trim the document's
/// leading and trailing blank lines (§3.5 layout).
fn collapse_blanks(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut prev_blank = false;
    for l in lines {
        let blank = l.trim().is_empty();
        if blank && prev_blank {
            continue;
        }
        prev_blank = blank;
        out.push(l);
    }
    while out.first().is_some_and(|l| l.trim().is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out
}

/// The canonical delivered keyword for a `> [!KIND]` alert opener, if the line
/// is one and KIND is a known keyword.
fn alert_open_kind(line: &str) -> Option<String> {
    let t = line.trim_start().strip_prefix('>')?.trim();
    let inner = t.strip_prefix("[!")?.strip_suffix(']')?;
    canonical_keyword(&inner.to_uppercase())
}

/// Normalize one keyword line to `[- ]**KEYWORD:** rest`, preserving a leading
/// list marker; `None` if the line is not a keyword line.
fn normalize_keyword_line(line: &str) -> Option<String> {
    let (marker, rest) = split_list_marker(line);
    let rest = rest.strip_prefix("**").unwrap_or(rest);
    // The keyword runs up to the first colon; check longest keywords first.
    let colon = rest.find(':')?;
    let kw_raw = rest[..colon].trim_end_matches("**").trim();
    let canon = canonical_keyword(kw_raw)?;
    let after = rest[colon + 1..].trim_start_matches("**");
    let after = after.strip_prefix([' ', '\t'])?;
    Some(format!("{marker}**{canon}:** {}", after.trim_start()))
}

/// Split a leading unordered/ordered list marker (`- `, `* `, `1. `) off a
/// line, returning `(marker_including_trailing_space, remainder)`.
fn split_list_marker(line: &str) -> (String, &str) {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return (format!("{indent}- "), rest);
    }
    // Ordered: digits then `.`/`)` then space.
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let after = &trimmed[digits.len()..];
        if let Some(rest) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            return (format!("{indent}{digits}. "), rest);
        }
    }
    (indent.to_string(), trimmed)
}

/// The canonical uppercase keyword a keyword token maps to (`MUST NOT` →
/// `NEVER`, `INFO` → `NOTE`), from the schema's keyword table. A negating
/// keyword (`SHOULD NOT`) maps to the same kind as its positive form, so its
/// label comes from the registry's negated labels — upper-casing the kind
/// alone delivered `SHOULD NOT:` as `**SHOULD:**`, the rule's opposite.
fn canonical_keyword(kw: &str) -> Option<String> {
    let reg = registry();
    let kind = reg.keyword_kind(kw)?;
    if reg.keyword_negates(kw)
        && let Some(label) = reg.negated_label(kind)
    {
        return Some(label.to_string());
    }
    Some(kind.to_uppercase())
}

/// A line that opens a keyword block (§4.5; S12, S15), as
/// `x-grammar.keywordLine` matches it: an optional column-0 list marker, an
/// optional `**`, a keyword, an optional `[name]`, an optional
/// ` (if condition)`, then `:`, an optional `**` and at least one space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeywordLine<'a> {
    /// The keyword as written, a key of the registry's keyword table.
    pub keyword: &'a str,
    /// `MUST[name]:` — the block's identity (S12).
    pub name: Option<&'a str>,
    /// `MUST (if c):` — the condition, trimmed (S15).
    pub condition: Option<&'a str>,
    /// The text after the label.
    pub text: &'a str,
}

/// Match `x-grammar.keywordLine` by hand (the crate has no regex
/// dependency). The keyword alternation is tried longest first, as the
/// pattern lists it, and a keyword that does not complete the match lets a
/// shorter one try — the backtracking the regex does.
pub(crate) fn keyword_line(line: &str) -> Option<KeywordLine<'_>> {
    let rest = &line[list_marker_len(line)..];
    let rest = rest.strip_prefix("**").unwrap_or(rest);
    registry().keywords_longest_first().iter().find_map(|kw| {
        let (name, condition, text) = keyword_tail(rest.strip_prefix(kw.as_str())?)?;
        Some(KeywordLine {
            keyword: kw.as_str(),
            name,
            condition,
            text,
        })
    })
}

/// What follows the keyword: `([name])?([ \t]+\(if[ \t]+[^()]+?\))?:(\*\*)?[ \t]+`.
fn keyword_tail(after: &str) -> Option<(Option<&str>, Option<&str>, &str)> {
    let mut s = after;
    let mut name = None;
    if let Some(b) = s.strip_prefix('[') {
        let close = b.find(']')?;
        if !is_name(&b[..close]) {
            return None;
        }
        name = Some(&b[..close]);
        s = &b[close + 1..];
    }
    let mut condition = None;
    let ws = s.trim_start_matches([' ', '\t']);
    if ws.len() != s.len() {
        // `[ \t]+(if[ \t]+([^()]+?))` — at least one blank after `if` and at
        // least one character after that, none of them a parenthesis.
        let c = ws.strip_prefix("(if")?;
        let close = c.find(')')?;
        let inner = &c[..close];
        if inner.contains('(') || !inner.starts_with([' ', '\t']) || inner.len() < 2 {
            return None;
        }
        condition = Some(inner.trim());
        s = &c[close + 1..];
    }
    let s = s.strip_prefix(':')?;
    let s = s.strip_prefix("**").unwrap_or(s);
    let text = s.trim_start_matches([' ', '\t']);
    (text.len() != s.len()).then_some((name, condition, text))
}

/// The length of a column-0 list marker (`[-*+][ \t]+` or
/// `[0-9]+[.)][ \t]+`) at the start of `line`, or 0.
fn list_marker_len(line: &str) -> usize {
    let after = if let Some(r) = line.strip_prefix(['-', '*', '+']) {
        r
    } else {
        let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        match line[digits..].strip_prefix(['.', ')']) {
            Some(r) if digits > 0 => r,
            _ => return 0,
        }
    };
    let rest = after.trim_start_matches([' ', '\t']);
    if rest.len() == after.len() {
        0
    } else {
        line.len() - rest.len()
    }
}

/// A line that opens a reason (S14), as `x-grammar.reasonLine` matches it:
/// optional indentation, an optional list marker, an optional `**`, the
/// registry's reason keyword and `:`, an optional `**`, at least one blank.
/// Returns the indentation's length and the length of the whole label.
fn reason_line(line: &str) -> Option<(usize, usize)> {
    let t = line.trim_start_matches([' ', '\t']);
    let indent = line.len() - t.len();
    let r = &t[list_marker_len(t)..];
    let r = r.strip_prefix("**").unwrap_or(r);
    let r = r
        .strip_prefix(registry().reason_keyword())?
        .strip_prefix(':')?;
    let r = r.strip_prefix("**").unwrap_or(r);
    let text = r.trim_start_matches([' ', '\t']);
    (text.len() != r.len()).then_some((indent, line.len() - text.len()))
}

/// Where the paragraph or list item that began at `k` ends (exclusive): the
/// lines a keyword or a reason owns (§4.5 rule 3). It stops at a blank line
/// and at anything that starts something else — a keyword, a reason, a list
/// item or quote, a block form, a heading, a code fence — as the reference
/// implementation's `paragraphEnd` does, and so does an author note: a note
/// right after a rule is never its text. A code fence stops it even when
/// indented, as the walk's own code tracking reads one, so a paragraph never
/// swallows the line that opens code.
fn paragraph_end(lines: &[&str], k: usize, bound: usize) -> usize {
    let mut j = k + 1;
    while j < bound {
        let l = lines[j];
        if l.trim().is_empty()
            || keyword_line(l).is_some()
            || reason_line(l).is_some()
            || list_marker_len(l) > 0
            || l.starts_with('>')
            || alert_block_kind(l).is_some()
            || open_fence(l).is_some()
            || fence_close_len(l).is_some()
            || leaf_open(l).is_some()
            || section_open(l).is_some()
            || heading_level(l).is_some()
            || code_fence_len(l).is_some()
            || note_opens(l)
        {
            break;
        }
        j += 1;
    }
    j
}

/// The reason after a rule (S14): a `BECAUSE:` paragraph or list item that
/// starts at `after`, or after blank lines only — and then at column 0, so
/// an indented `BECAUSE:` under a later item is not taken for this rule's.
/// Returns its first line, the index just past it, and its text: the first
/// line's remainder and the trimmed continuation lines.
fn take_reason(lines: &[&str], after: usize, bound: usize) -> Option<(usize, usize, String)> {
    let mut k = after;
    while k < bound && lines[k].trim().is_empty() {
        k += 1;
    }
    if k >= bound {
        return None;
    }
    let (indent, label) = reason_line(lines[k])?;
    if k > after && indent != 0 {
        return None;
    }
    let end = paragraph_end(lines, k, bound);
    let mut text = vec![&lines[k][label..]];
    text.extend(lines[k + 1..end].iter().map(|l| l.trim()));
    Some((k, end, text.join("\n").trim().to_string()))
}

/// Attach the reason that follows a rule block, if one does (S14), and return
/// the index just past it. A rule carries one reason: a `because=` attribute
/// and a `BECAUSE:` paragraph together are refused at the paragraph. A
/// `BECAUSE:` after a block that is not a rule stays prose.
fn attach_reason(
    b: &mut Block,
    lines: &[&str],
    after: usize,
    bound: usize,
    line_base: usize,
    errs: &mut Vec<Refusal>,
) -> Option<usize> {
    if !registry().is_rule(&b.kind) {
        return None;
    }
    let (start, end, text) = take_reason(lines, after, bound)?;
    if b.attrs.contains_key("because") {
        errs.push(Refusal::at(
            line_base + start + 1,
            "because-repeated",
            format!(
                "{} already has a reason (because=) — a rule has one reason",
                b.kind
            ),
        ));
    } else {
        b.attrs.insert("because".to_string(), text);
    }
    b.reason = Some((start, end - 1));
    Some(end)
}

/// Parse a `:::` block, then take the reason that follows a rule container.
/// Returns the blocks, the index just past the closing fence, and the index
/// just past the reason (the same index when there is none).
fn parse_fence_and_reason(
    lines: &[&str],
    open_idx: usize,
    of: OpenFence,
    disposition: Option<Disposition>,
    line_base: usize,
    bound: usize,
    errs: &mut Vec<Refusal>,
) -> (Vec<Block>, usize, usize) {
    let (mut blocks, next) = parse_fence(lines, open_idx, of, disposition, line_base, errs);
    let end = match blocks.as_mut_slice() {
        [b] if b.form == Form::Container => {
            attach_reason(b, lines, next, bound, line_base, errs).unwrap_or(next)
        }
        _ => next,
    };
    (blocks, next, end)
}

/// If `line` opens a blockquote alert (§4.6): the kind it maps to.
pub(crate) fn alert_block_kind(line: &str) -> Option<String> {
    alert_keyword(line).and_then(|kw| registry().keyword_kind(&kw).map(str::to_string))
}

/// The keyword a `> [!KEYWORD]` opener names, upper-cased.
fn alert_keyword(line: &str) -> Option<String> {
    let t = line.trim_start().strip_prefix('>')?.trim();
    let inner = t.strip_prefix("[!")?.strip_suffix(']')?;
    Some(inner.to_uppercase())
}

/// Lift a keyword or alert starting at `lines[i]` into a block, with the
/// reason that follows it. Returns the block and the index just past it. The
/// block's BODY is the label-stripped text (§9.1); its REGION covers the raw
/// lines, reason included, which is what delivery and any raw reconstruction
/// slice from the source. `bound` is where the enclosing body ends.
pub(crate) fn lift_keyword_or_alert(
    lines: &[&str],
    i: usize,
    base: usize,
    bound: usize,
    errs: &mut Vec<Refusal>,
) -> Option<(Block, usize)> {
    let line = lines[i];
    let lifted = |kind: String, attrs, name, body: String, form| Block {
        kind,
        disposition: Disposition::Prose,
        name,
        attrs,
        body,
        children: Vec::new(),
        line: base + i + 1,
        set_group: None,
        form,
        raw_body: None,
        region: (0, 0), // set below, once the reason is known
        reason: None,
        notes: Vec::new(),
    };
    // A keyword's flags (`SHOULD NOT` → `not`) are the only thing that keep
    // its polarity: it maps to the same kind as its positive form (S8).
    let flags = |kw: &str| -> BTreeMap<String, String> {
        registry()
            .keyword_flags(kw)
            .map(|f| (f.to_string(), "true".to_string()))
            .collect()
    };
    let (mut b, j) = if let Some(kw) = alert_keyword(line)
        && let Some(kind) = registry().keyword_kind(&kw)
    {
        let mut body = Vec::new();
        let mut j = i + 1;
        while j < bound {
            let t = lines[j].trim_start();
            if let Some(rest) = t.strip_prefix('>') {
                body.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                j += 1;
            } else {
                break;
            }
        }
        let b = lifted(
            kind.to_string(),
            flags(&kw),
            None,
            body.join("\n"),
            Form::Alert,
        );
        (b, j)
    } else {
        let km = keyword_line(line)?;
        let kind = registry().keyword_kind(km.keyword)?.to_string();
        let j = paragraph_end(lines, i, bound);
        let mut body = vec![km.text];
        body.extend(&lines[i + 1..j]);
        let mut attrs = flags(km.keyword);
        if let Some(name) = km.name {
            attrs.insert("name".to_string(), name.to_string());
        }
        if let Some(c) = km.condition {
            attrs.insert("if".to_string(), c.to_string());
        }
        let name = km.name.map(str::to_string);
        let b = lifted(
            kind,
            attrs,
            name,
            body.join("\n").trim().to_string(),
            Form::Keyword,
        );
        (b, j)
    };
    let end = attach_reason(&mut b, lines, j, bound, base, errs).unwrap_or(j);
    b.region = (i, end - 1);
    Some((b, end))
}

/// Definition-list entries for the §9.1 tree: term free text, definition
/// lines joined by NEWLINE (delivery joins by spaces; the tree keeps breaks).
pub(crate) fn tree_deflist_entries(body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut term: Option<String> = None;
    let mut def: Vec<String> = Vec::new();
    let flush =
        |term: &mut Option<String>, def: &mut Vec<String>, out: &mut Vec<(String, String)>| {
            if let Some(t) = term.take() {
                out.push((t, def.join("\n")));
            }
            def.clear();
        };
    for line in body.lines() {
        if let Some(rest) = line.trim_start().strip_prefix(':') {
            def.push(rest.trim().to_string());
        } else if line.trim().is_empty() {
            flush(&mut term, &mut def, &mut out);
        } else if term.is_some() && line.starts_with([' ', '\t']) {
            def.push(line.trim().to_string());
        } else {
            flush(&mut term, &mut def, &mut out);
            let name = line.split('{').next().unwrap_or(line).trim().to_string();
            if !name.is_empty() {
                term = Some(name);
            }
        }
    }
    flush(&mut term, &mut def, &mut out);
    out
}

/// An `override` sub-block (inside `!mcp`) narrows one of the server's tools —
/// append-only, folded into real registry config: disable, add trifecta tags,
/// append an operator annotation. It may only make a tool MORE careful, and it
/// delivers nothing (the spec gives it no acknowledgement).
fn fold_override(b: &Block, out: &mut Extraction, errs: &mut Vec<Refusal>) {
    let Some(target) = b.attrs.get("target").cloned() else {
        errs.push(Refusal::at(
            b.line,
            "missing-attribute",
            "`override` needs a target=<tool> (the server tool to narrow)",
        ));
        return;
    };
    let body = body_map(b, errs).unwrap_or_default();
    // `disabled: false` is a RE-ENABLE — behavioural steering, not narrowing
    // (§5.3): an override may only make a tool more careful.
    if b.attrs.get("disabled").map(String::as_str) == Some("false")
        || body.get("disabled").and_then(Value::as_bool) == Some(false)
    {
        errs.push(Refusal::at(
            b.line,
            "widening-override",
            "override may not re-enable a tool — overrides only narrow",
        ));
        return;
    }
    let disabled = b
        .attrs
        .get("disabled")
        .map(|v| v != "false")
        .unwrap_or(false)
        || body
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if disabled {
        push_into(&mut out.config, "tools", "disabled").push(Value::String(target));
        return;
    }
    let mut narrow = serde_json::Map::new();
    if let Some(tags) = body.get("tags").and_then(Value::as_array) {
        narrow.insert("tags".into(), Value::Array(tags.clone()));
    }
    // A description narrows to an operator annotation, appended beneath the
    // server's own description — never a replacement (spec §5.3).
    if let Some(desc) = body
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| b.attrs.get("description").map(String::as_str))
    {
        narrow.insert("describe".into(), Value::String(desc.trim().to_string()));
    }
    if !narrow.is_empty() {
        frag(&mut out.config, "tools")
            .entry("narrow")
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .expect("narrow is an object")
            .insert(target, Value::Object(narrow));
    }
}

fn fold_machinery(b: &Block, out: &mut Extraction, errs: &mut Vec<Refusal>) {
    check_pins(b, errs);
    match b.kind.as_str() {
        // ── core: fold into real agentd configuration ───────────────────────
        "workflow" => {
            if let Some(mut m) = body_map(b, errs) {
                if let Some(armed) = b.attrs.get("armed") {
                    m.insert(
                        "armed".into(),
                        Value::Bool(armed == "true" || armed.is_empty()),
                    );
                }
                out.workflows.push(Value::Object(m));
            }
        }
        "config" => {
            if let Some(m) = body_map(b, errs) {
                // The document cannot grant itself trust (§6 rule 4): a served
                // document is never a source of its own trust configuration, so
                // `!config` may not write the operator-only trust surface.
                for key in [
                    "document_capabilities",
                    "instruction_sources",
                    "instruction",
                ] {
                    if m.contains_key(key) {
                        errs.push(Refusal::at(
                            b.line,
                            "self-grant",
                            format!(
                                "!config may not write `{key}` — it is operator \
                                 configuration, not a document's to set"
                            ),
                        ));
                    }
                }
                merge_into(&mut out.config, m);
            }
        }
        "mcp" => {
            if let Some(mut m) = body_map(b, errs) {
                // `deny` is the spec-normative attribute (§5.3); agentd's field
                // for the same thing is `exclude`. Map it here rather than
                // asking authors to know the runtime's name.
                if let Some(deny) = m.remove("deny") {
                    m.entry("exclude".to_string()).or_insert(deny);
                }
                match m.get("name").and_then(Value::as_str) {
                    Some(_) => {
                        push_into(&mut out.config, "mcp", "servers").push(Value::Object(m));
                        // `override` sub-blocks (§5.3) adjust this server's
                        // tools — append-only: disable, add trifecta tags, or
                        // annotate. They deliver nothing of their own.
                        for child in &b.children {
                            if child.kind == "override" {
                                fold_override(child, out, errs);
                            }
                        }
                    }
                    None => errs.push(Refusal::at(
                        b.line,
                        "missing-attribute",
                        "mcp requires name",
                    )),
                }
            }
        }
        "stream" => {
            if let Some(mut m) = body_map(b, errs) {
                match m
                    .remove("name")
                    .and_then(|v| v.as_str().map(str::to_string))
                {
                    Some(name) => {
                        frag(&mut out.config, "streams").insert(name.clone(), Value::Object(m));
                    }
                    None => errs.push(Refusal::at(
                        b.line,
                        "missing-attribute",
                        "stream requires name",
                    )),
                }
            }
        }
        "tools" => {
            if let Some(m) = body_map(b, errs) {
                merge_map(frag(&mut out.config, "tools"), m);
            }
        }
        // `!eval` (S23) holds a rule's test cases for an evaluation harness:
        // it configures nothing here, declares nothing, and has no
        // acknowledgement. Its body is still YAML and must parse as YAML; its
        // body schema is informative (S22), so the shape is never refused.
        "eval" => {
            if !b.body.trim().is_empty()
                && let Err(e) = crate::yaml::parse(&b.body)
            {
                errs.push(Refusal::at(
                    b.line,
                    "invalid-yaml-body",
                    format!(":::!eval body is not valid YAML: {e}"),
                ));
            }
        }
        "skill" => match b.attrs.get("name") {
            Some(name) => {
                out.skills.push(InlineSkill {
                    name: name.clone(),
                    description: b.attrs.get("description").cloned().unwrap_or_default(),
                    // `trigger` is the attribute (S11); `when` is its
                    // version-1 alias, and `trigger` wins when both are given.
                    when_to_use: b
                        .attrs
                        .get("trigger")
                        .or_else(|| b.attrs.get("when"))
                        .cloned(),
                    // The catalogue body keeps lifted keyword/alert guidance
                    // IN POSITION (the tree-facing `body` excludes it).
                    body: b.delivery_body().to_string(),
                });
            }
            None => errs.push(Refusal::at(
                b.line,
                "missing-attribute",
                "skill requires name",
            )),
        },
        // ── extended families: cleanly map to real config where one exists ───
        "endpoint" => {
            // A live listener route: folds into a real workflow with a single
            // `webhook` start node. `into:` makes it append to a stream (no
            // run); otherwise it fires the workflow. The listener address is
            // `webhooks.listen` (agent-level); this block declares the ROUTE.
            let Some(name) = b.name.clone().or_else(|| b.attrs.get("name").cloned()) else {
                errs.push(Refusal::at(
                    b.line,
                    "missing-attribute",
                    "endpoint requires name",
                ));
                return;
            };
            let body = body_map(b, errs).unwrap_or_default();
            let mut node = serde_json::Map::new();
            node.insert("kind".into(), Value::String("webhook".into()));
            if let Some(p) = b.attrs.get("path") {
                node.insert("path".into(), Value::String(p.clone()));
            }
            for key in ["path", "methods", "auth", "into", "rate", "respond"] {
                if let Some(v) = body.get(key) {
                    node.insert(key.into(), v.clone());
                }
            }
            let workflow = serde_json::json!({
                "name": format!("endpoint-{name}"),
                "steps": { "hook": Value::Object(node) },
            });
            out.workflows.push(workflow);
        }
        "peer" => {
            if let Some(m) = body_map(b, errs) {
                push_into(&mut out.config, "a2a", "peers").push(Value::Object(m));
            }
        }
        // ── content-bearing kinds: the body is literal, not config ──────────
        // A file's body IS the file; a media/asset body is human description.
        // Path/mode/src come from the fence attributes.
        // ── everything else: parse per the kind's BODY interpretation (§4.1),
        // grant-checked, recorded as a declaration (visible in --capabilities;
        // runtime effect delegated to a service). A YAML body folds to a
        // mapping; a table to rows; code to `code` + a description; markdown or
        // text to `content`.
        other => {
            let body_kind = lookup(other).map(|k| k.body).unwrap_or(BodyKind::Yaml);
            let mut rec = serde_json::Map::new();
            for (k, v) in &b.attrs {
                rec.insert(k.clone(), attr_scalar(other, k, v));
            }
            match body_kind {
                BodyKind::Yaml => {
                    if !b.body.trim().is_empty() {
                        match crate::yaml::parse(&b.body) {
                            // The fence attributes win over same-named body keys.
                            Ok(Value::Object(m)) => {
                                for (k, v) in m {
                                    rec.entry(k).or_insert(v);
                                }
                            }
                            Ok(_) => {
                                errs.push(Refusal::at(
                                    b.line,
                                    "invalid-yaml-body",
                                    format!(":::!{other} body must be a YAML mapping"),
                                ));
                                return;
                            }
                            Err(e) => {
                                errs.push(Refusal::at(
                                    b.line,
                                    "invalid-yaml-body",
                                    format!(":::!{other} body is not valid YAML: {e}"),
                                ));
                                return;
                            }
                        }
                    }
                }
                BodyKind::Table => {
                    rec.insert("rows".into(), Value::Array(table_rows(&b.body)));
                }
                BodyKind::Code => {
                    let lines: Vec<String> = b.body.split('\n').map(str::to_string).collect();
                    let (code, desc) = extract_single_code_block(&lines)
                        .unwrap_or((b.body.clone(), String::new()));
                    rec.insert("code".into(), Value::String(code));
                    if !desc.trim().is_empty() {
                        rec.entry("description".to_string())
                            .or_insert(Value::String(desc.trim().to_string()));
                    }
                }
                BodyKind::Markdown | BodyKind::Text | BodyKind::Deflist => {
                    let content = b.delivery_body();
                    if !content.is_empty() {
                        rec.insert("content".into(), Value::String(content.to_string()));
                    }
                }
                BodyKind::None => {}
            }
            if let Some(n) = &b.name {
                rec.entry("name".to_string())
                    .or_insert(Value::String(n.clone()));
            }
            // Record sub-blocks (from children) under the declaration too.
            if !b.children.is_empty() {
                let subs: Vec<Value> = b
                    .children
                    .iter()
                    .map(|c| serde_json::json!({"kind": c.kind, "name": c.name, "body": c.body}))
                    .collect();
                rec.insert("_sub".into(), Value::Array(subs));
            }
            out.declarations
                .entry(other.to_string())
                .or_default()
                .push(Value::Object(rec));
        }
    }
}

/// Parse a Markdown-table body into one record per row (header cells name the
/// fields), for a `!data`/`!fixture` block.
pub(crate) fn table_rows(body: &str) -> Vec<Value> {
    let rows: Vec<&str> = body
        .lines()
        .filter(|l| l.trim_start().starts_with('|'))
        .collect();
    if rows.len() < 2 {
        return Vec::new();
    }
    let header: Vec<String> = split_cells(rows[0])
        .into_iter()
        .map(|c| c.to_lowercase())
        .collect();
    rows.iter()
        .skip(2)
        .map(|r| {
            let cells = split_cells(r);
            let mut o = serde_json::Map::new();
            for (h, c) in header.iter().zip(cells.iter()) {
                if !c.is_empty() {
                    o.insert(h.clone(), Value::String(c.clone()));
                }
            }
            Value::Object(o)
        })
        .collect()
}

/// The `pins` semantic rule (§5.3): image digests required, remote media/asset
/// sources content-addressed, `network: any` refused, and no literal
/// credential anywhere in a machinery body.
fn check_pins(b: &Block, errs: &mut Vec<Refusal>) {
    match b.kind.as_str() {
        "image" => {
            if !b
                .attrs
                .get("digest")
                .is_some_and(|d| d.starts_with("sha256:"))
            {
                errs.push(Refusal::at(
                    b.line,
                    "mutable-image-tag",
                    format!(
                        "image {:?} is not digest-pinned",
                        b.name.as_deref().unwrap_or("")
                    ),
                ));
            }
        }
        "runtime" => {
            if b.attrs.get("network").map(String::as_str) == Some("any")
                || b.body.lines().any(|l| l.trim() == "network: any")
            {
                errs.push(Refusal::at(
                    b.line,
                    "network-any",
                    format!(
                        "runtime {:?}: network: any is refused — the sandbox boundary is the \
                         security boundary",
                        b.name.as_deref().unwrap_or("")
                    ),
                ));
            }
        }
        "asset" | "media" => {
            let remote = b
                .attrs
                .get("src")
                .is_some_and(|s| s.starts_with("http://") || s.starts_with("https://"));
            if remote && !b.attrs.contains_key("sha256") {
                errs.push(Refusal::at(
                    b.line,
                    "unpinned-remote-asset",
                    "remote src requires sha256",
                ));
            }
        }
        _ => {}
    }
    // Literal credentials: a recognizable secret shape in a machinery body is
    // refused by CLASS, naming its line — values belong in a `!secret-ref`.
    for (i, line) in b.body.lines().enumerate() {
        if let Some(label) = credential_class(line) {
            errs.push(Refusal::at(
                b.line + 1 + i,
                "literal-credential",
                format!("a literal credential is never allowed ({label}) — use a secret-ref"),
            ));
        }
    }
}

/// The class label for a line that carries a recognizable literal credential,
/// or `None`. Deliberately a small, high-precision set — references
/// (`@secret-ref/…`, `{{secret:…}}`) never match.
fn credential_class(line: &str) -> Option<&'static str> {
    let l = line;
    let has = |pat: &str, min_tail: usize| {
        l.find(pat).is_some_and(|at| {
            l[at + pat.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .count()
                >= min_tail
        })
    };
    if has("sk-", 20) {
        return Some("API key");
    }
    if has("AKIA", 16) {
        return Some("AWS access key");
    }
    if has("ghp_", 20) || has("github_pat_", 20) {
        return Some("GitHub token");
    }
    if has("xoxb-", 10) || has("xoxp-", 10) {
        return Some("Slack token");
    }
    None
}

fn merge_into(cfg: &mut serde_json::Map<String, Value>, add: serde_json::Map<String, Value>) {
    for (k, v) in add {
        cfg.insert(k, v);
    }
}

fn merge_map(dst: &mut serde_json::Map<String, Value>, src: serde_json::Map<String, Value>) {
    for (k, v) in src {
        match (dst.get_mut(&k), v) {
            (Some(Value::Object(d)), Value::Object(s)) => merge_map(d, s),
            (_, v) => {
                dst.insert(k, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds_of(doc: &Document) -> Vec<&str> {
        doc.blocks().map(|b| b.kind.as_str()).collect()
    }

    #[test]
    fn plain_prose_is_a_valid_empty_document() {
        let d = parse("just guidance, no blocks").unwrap();
        assert!(d.blocks().next().is_none());
    }

    #[test]
    fn machinery_needs_the_sigil_and_bare_shadow_is_refused() {
        // Correct sigiled machinery parses.
        let d = parse(":::!workflow{name=w}\nsteps: {}\n:::").unwrap();
        assert_eq!(kinds_of(&d), ["workflow"]);
        assert_eq!(
            d.blocks().next().unwrap().disposition,
            Disposition::Machinery
        );
        // Bare machinery name — the forgotten-sigil trap — is refused.
        let e = parse(":::workflow{name=w}\nsteps: {}\n:::").unwrap_err();
        assert!(
            e[0].message.contains("is a machinery kind") && e[0].message.contains(":::!workflow"),
            "{e:?}"
        );
        // Sigiled prose — the symmetric error.
        let e = parse(":::!note\nhi\n:::").unwrap_err();
        assert!(
            e[0].message.contains("is a prose kind") && e[0].message.contains(":::note"),
            "{e:?}"
        );
    }

    #[test]
    fn prose_is_bare_and_unknown_bare_is_inert() {
        let d = parse(":::note\nremember this\n:::").unwrap();
        assert_eq!(d.blocks().next().unwrap().disposition, Disposition::Prose);
        // Unknown bare name fails OPEN — inert prose, still parses. It is no
        // block (the tree never shows one); its body is kept as text.
        let d = parse(":::whatever\nfree text\n:::").unwrap();
        assert!(d.blocks().next().is_none());
        assert_eq!(
            d.nodes,
            [Node::Inert {
                kind: "whatever".into(),
                line: 1,
                region: (0, 2),
                body: "free text".into(),
            }]
        );
        // Unknown MACHINERY fails closed.
        let e = parse(":::!whatever\nx\n:::").unwrap_err();
        assert!(e[0].message.contains("unknown machinery"), "{e:?}");
    }

    #[test]
    fn blocks_nest_by_fence_length() {
        let doc = "::::!test{name=t target=@function/f}\n\
                   :::case{name=one}\n\
                   given: {x: 1}\n\
                   :::\n\
                   ::::\n\
                   :::!function{name=f}\nsig\n:::";
        let d = parse(doc).unwrap();
        assert_eq!(kinds_of(&d), ["test", "function"]);
        assert_eq!(d.blocks().next().unwrap().children.len(), 1);
        assert_eq!(d.blocks().next().unwrap().children[0].kind, "case");
    }

    #[test]
    fn code_fences_suspend_colon_scanning() {
        let doc = "::::!function{name=f}\n\
                   ```python\n\
                   x = 1  # ::: not a fence\n\
                   :::\n\
                   ```\n\
                   ::::";
        let d = parse(doc).unwrap();
        assert_eq!(kinds_of(&d), ["function"]);
        assert!(
            d.blocks().next().unwrap().body.contains(":::"),
            "the inner colons stay in the body"
        );
    }

    #[test]
    fn duplicate_names_per_kind_are_refused() {
        let e = parse(":::!workflow{name=dup}\na: {}\n:::\n:::!workflow{name=dup}\nb: {}\n:::")
            .unwrap_err();
        assert!(e[0].message.contains("duplicate workflow/dup"), "{e:?}");
        // Same name, DIFFERENT kinds is fine.
        assert!(parse(":::!workflow{name=x}\na: {}\n:::\n:::!stream{name=x}\nr: {}\n:::").is_ok());
    }

    #[test]
    fn refs_must_resolve_and_be_qualified_and_acyclic() {
        // Unresolvable.
        let e = parse(":::!function{name=f target=@runtime/missing}\nx\n:::").unwrap_err();
        assert!(e[0].message.contains("does not resolve"), "{e:?}");
        // Unqualified.
        let e = parse(":::!function{name=f target=@bare}\nx\n:::").unwrap_err();
        assert!(
            e.iter().any(|m| m.message.contains("must be qualified")),
            "{e:?}"
        );
        // Resolvable + qualified is fine.
        assert!(parse(
            ":::!runtime{name=r}\nimage: x\n:::\n:::!function{name=f runtime=@runtime/r}\nx\n:::"
        ).is_ok());
    }

    #[test]
    fn the_registry_loads_from_the_vendored_schema() {
        // The registry IS the vendored JSON Schema — these counts (§5) are the
        // spec's own, and a drift would fail here at load, not silently.
        let r = registry();
        assert_eq!(r.version(), 1);
        assert_eq!(r.revision(), "1.1");
        assert_eq!(
            machinery_names().count(),
            29,
            "29 machinery kinds (override, case, signature, schema and preview are sub-blocks)"
        );
        assert_eq!(
            r.kinds
                .values()
                .filter(|k| k.disposition == Disposition::Prose && k.sub_of.is_none())
                .count(),
            19,
            "19 prose kinds (1.1 adds may, always, avoid and output)"
        );
        assert_eq!(
            r.kinds
                .values()
                .filter(|k| k.disposition == Disposition::Structural)
                .count(),
            5,
            "5 structural kinds (1.1 adds unless and otherwise)"
        );
        // Revision 1.1 gives every prose kind identity (`x-identity`), but a
        // name is REQUIRED only where the schema's `then` lists it — so an
        // anonymous `MUST:` stays valid and an unnamed `!workflow` does not.
        assert!(lookup("must").unwrap().identity);
        assert!(!lookup("must").unwrap().requires_name);
        assert!(lookup("workflow").unwrap().requires_name);
        assert!(lookup("param").unwrap().requires_name);
        assert!(lookup("eval").unwrap().requires_name);
        assert!(!lookup("config").unwrap().requires_name);
        // A set row needs a name exactly when the kind REQUIRES one
        // (`parse_table_set` keys on `requires_name`). Every set-form kind
        // in the schema both requires a name and has identity, so today the
        // two fields pick the same rows and nothing else tells the choice
        // apart: a registry that gives a name-optional kind the set form
        // fails here, and must bring a set-row test deciding which it means.
        let apart: Vec<&str> = r
            .kinds
            .values()
            .filter(|k| k.forms.contains(&Form::Set) && !(k.requires_name && k.identity))
            .map(|k| k.name.as_str())
            .collect();
        assert!(
            apart.is_empty(),
            "set-form kinds whose requires_name and identity no longer agree: {apart:?}"
        );
        assert!(parse(":::must\nCite sources.\n:::\nMUST: be brief.\n").is_ok());
        let e = parse(":::!workflow\nsteps: []\n:::").unwrap_err();
        assert!(
            e.iter()
                .any(|m| m.message.contains("workflow requires name")),
            "{e:?}"
        );
        assert_eq!(lookup("context").unwrap().disposition, Disposition::Prose);
        assert_eq!(
            lookup("workflow").unwrap().disposition,
            Disposition::Machinery
        );
        assert_eq!(
            lookup("function").unwrap().family.as_deref(),
            Some("compute")
        );
        assert_eq!(
            lookup("function").unwrap().grant.as_deref(),
            Some("compute")
        );
        // `!data` and `!override` carry a family but sit on the default rung.
        assert_eq!(lookup("data").unwrap().family.as_deref(), Some("material"));
        assert_eq!(grant_of("data"), None, "data needs no grant");
        assert_eq!(lookup("override").unwrap().sub_of.as_deref(), Some("mcp"));
        assert_eq!(grant_of("override"), None, "override needs no grant");
        assert_eq!(lookup("case").unwrap().sub_of.as_deref(), Some("test"));
        // The forms table is read from the schema (§4).
        assert!(accepts_form("human", Form::Leaf) && accepts_form("human", Form::Set));
        assert!(accepts_form("skill", Form::Section));
        assert!(
            !accepts_form("workflow", Form::Leaf),
            "workflow needs a body"
        );
        assert_eq!(all_families().len(), 7, "seven grant tokens");
    }

    fn grants(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn core_machinery_folds_into_config_and_prose_degrades() {
        let doc = r#"You triage tickets.

:::note
Be brief.
:::

:::!workflow{name=drain}
steps:
  f: {kind: finish}
:::

:::!mcp{name=search}
endpoint: https://x/mcp
:::

:::!stream{name=tickets}
retention: {max_events: 100}
:::

:::!skill{name=esc description="escalate" when="angry"}
Ask a human.
:::

:::context{title="SLA"}
1h for enterprise.
:::"#;
        let d = parse(doc).unwrap();
        let e = fold(&d, &grants(&[])).unwrap();
        // workflow lifted for the root-array splice; mcp + stream folded.
        assert_eq!(e.workflows.len(), 1);
        assert_eq!(e.config["mcp"]["servers"].as_array().unwrap().len(), 1);
        assert!(e.config["streams"]["tickets"].is_object());
        assert_eq!(e.skills.len(), 1);
        assert_eq!(e.skills[0].name, "esc");
        // Prose degraded INTO the delivery; machinery acknowledged, body stripped.
        assert!(e.cleaned.contains("Be brief."), "note body degrades in");
        assert!(
            e.cleaned.contains("<reference title=\"SLA\">"),
            "context wraps"
        );
        assert!(
            e.cleaned.contains("workflow \"drain\" is loaded"),
            "machinery acknowledged"
        );
        assert!(
            !e.cleaned.contains("kind: finish"),
            "machinery body is NOT delivered"
        );
    }

    #[test]
    fn every_family_loads_with_its_grant() {
        // One document exercising all seven grant-gated families plus the
        // default rung — the "all elements load" proof.
        let doc = r#"---
spec: "1"
---
:::!file{name=cfg path=pyproject.toml}
[project]
name='x'
:::
:::!data{name=slo}
tiers: [gold, silver]
:::
:::!knowledge{name=kb}
server: kb
:::
:::!source{name=docs}
kind: git
:::
:::!ui{name=card}
kind: form
:::
:::!human{name=oncall}
role: approver
:::
:::!policy{name=egress}
mode: closed
:::
::!secret-ref{name=tok kind=file path=/run/tok}
:::!runtime{name=py}
image: ghcr.io/x@sha256:abc
:::
:::!function{name=lint runtime=@runtime/py}
doc: lint
:::
:::!git{name=repo}
url: https://x
:::
::!image{name=img digest=sha256:abc}
:::!agent{name=rev}
template: reviewer
:::"#;
        let d = parse(doc).unwrap();
        let all = grants(&[
            "material",
            "knowledge",
            "interface",
            "identity",
            "compute",
            "infra",
            "compose",
        ]);
        let e = fold(&d, &all).unwrap();
        // Each extended kind is recorded as a loaded declaration.
        for kind in [
            "file",
            "data",
            "knowledge",
            "source",
            "ui",
            "human",
            "policy",
            "secret-ref",
            "runtime",
            "function",
            "git",
            "image",
            "agent",
        ] {
            assert!(e.declarations.contains_key(kind), "{kind} did not load");
        }
        // Without the grants, the same document is refused, naming each family.
        let none = fold(&d, &grants(&[])).unwrap_err();
        assert!(
            none.iter().any(|m| m.message.contains("compute")),
            "{none:?}"
        );
        assert!(
            none.iter().any(|m| m.message.contains("material")),
            "{none:?}"
        );
    }

    #[test]
    fn a_function_with_a_test_carries_its_case_sub_blocks() {
        let doc = r#"---
spec: "1"
---
:::!runtime{name=py}
image: x@sha256:a
:::
::::!function{name=lint runtime=@runtime/py}
doc: lint
::::
::::!test{name=lint-works target=@function/lint}
:::case{name=one}
given: {x: 1}
expect: {ok: true}
:::
::::"#;
        let d = parse(doc).unwrap();
        let e = fold(&d, &grants(&["compute"])).unwrap();
        let tests = &e.declarations["test"];
        assert_eq!(tests.len(), 1);
        assert_eq!(
            tests[0]["_sub"].as_array().unwrap().len(),
            1,
            "the case sub-block is recorded"
        );
        assert_eq!(tests[0]["_sub"][0]["kind"], "case");
    }

    #[test]
    fn override_is_a_subblock_of_mcp_and_folds_into_real_tool_config() {
        // `override` sits INSIDE `!mcp` (spec §5.3/§5.4): disable folds into
        // tools.disabled; narrowing folds into tools.narrow (append-only tags +
        // an operator annotation). override is default-rung — no grant needed.
        let doc = r#"---
spec: "1"
---
::::!mcp{name=ticketing endpoint=https://x/mcp}
:::override{target=delete_ticket}
disabled: true
:::
:::override{target=create_ticket}
tags: [sensitive]
description: ENG queue only
:::
::::"#;
        let e = fold(&parse(doc).unwrap(), &grants(&[])).unwrap();
        assert_eq!(e.config["mcp"]["servers"].as_array().unwrap().len(), 1);
        assert_eq!(e.config["tools"]["disabled"][0], "delete_ticket");
        let narrow = &e.config["tools"]["narrow"]["create_ticket"];
        assert_eq!(narrow["tags"][0], "sensitive");
        assert_eq!(narrow["describe"], "ENG queue only");
        // A top-level `override` (outside its parent) is refused, naming `!mcp`.
        let orphan = "---\nspec: \"1\"\n---\n:::override{target=x}\ndisabled: true\n:::";
        let e = parse(orphan).unwrap_err();
        assert!(
            e[0].message.contains("valid only inside a mcp"),
            "orphan override names its parent: {e:?}"
        );
    }

    #[test]
    fn endpoint_folds_into_a_real_webhook_workflow() {
        let doc = r#"---
spec: "1"
---
:::!endpoint{name=hook path=/hooks/x methods=[POST]}
into: {stream: s, subject: x.y}
:::"#;
        let e = fold(&parse(doc).unwrap(), &grants(&["interface"])).unwrap();
        assert_eq!(e.workflows.len(), 1, "an endpoint is a real workflow");
        let wf = &e.workflows[0];
        assert_eq!(wf["name"], "endpoint-hook");
        assert_eq!(wf["steps"]["hook"]["kind"], "webhook");
        assert_eq!(wf["steps"]["hook"]["path"], "/hooks/x");
        assert_eq!(wf["steps"]["hook"]["into"]["stream"], "s");
        // interface-gated: no grant → refused.
        assert!(fold(&parse(doc).unwrap(), &grants(&[])).is_err());
    }

    #[test]
    fn grants_gate_families_fail_closed() {
        let d = parse(
            ":::!function{name=f runtime=@runtime/r}\nx\n:::\n:::!runtime{name=r}\ni: y\n:::",
        )
        .unwrap();
        let none = BTreeSet::new();
        let mut errs = Vec::new();
        check_grants(&d, &none, &mut errs);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("`compute` capability")),
            "{errs:?}"
        );
        // Granted → clean.
        let mut granted = BTreeSet::new();
        granted.insert("compute".to_string());
        let mut errs = Vec::new();
        check_grants(&d, &granted, &mut errs);
        assert!(errs.is_empty(), "{errs:?}");
    }

    // ── forms (§4) ───────────────────────────────────────────────────────

    #[test]
    fn a_leaf_is_one_instance_with_no_body() {
        let d = parse("::!human{name=lead role=reviewer}").unwrap();
        let b = d.blocks().next().unwrap();
        assert_eq!(b.kind, "human");
        assert_eq!(b.name.as_deref(), Some("lead"));
        assert_eq!(b.attrs.get("role").map(String::as_str), Some("reviewer"));
        assert!(b.body.is_empty() && b.children.is_empty());
        // A leaf of a body-required kind is refused, pointing at the container.
        let e = parse("::!workflow{name=drain}").unwrap_err();
        assert!(
            e[0].message.contains("requires a body") && e[0].message.contains(":::!workflow"),
            "{e:?}"
        );
        // A bare leaf shadowing machinery is the same reserved-bare trap.
        let e = parse("::human{name=x}").unwrap_err();
        assert!(e[0].message.contains("is a machinery kind"), "{e:?}");
    }

    #[test]
    fn a_table_set_declares_one_instance_per_row() {
        let doc = "---\nspec: \"1\"\n---\n:::!human[]{escalate_after=1h}\n| name   | role     |\n|--------|----------|\n| oncall | approver |\n| lead   | reviewer |\n:::";
        let d = parse(doc).unwrap();
        let humans: Vec<&Block> = d.blocks().filter(|b| b.kind == "human").collect();
        assert_eq!(humans.len(), 2, "one block per row");
        assert_eq!(humans[0].name.as_deref(), Some("oncall"));
        assert_eq!(
            humans[0].attrs.get("role").map(String::as_str),
            Some("approver")
        );
        // The fence attribute applies to every row unless overridden.
        assert_eq!(
            humans[1].attrs.get("escalate_after").map(String::as_str),
            Some("1h")
        );
        // Each is folded as its own declaration.
        let e = fold(&d, &grants(&["interface"])).unwrap();
        assert_eq!(e.declarations["human"].len(), 2);
    }

    #[test]
    fn a_definition_list_set_gives_each_entry_a_body() {
        let doc = "---\nspec: \"1\"\n---\n:::!skill[]\ntone {when=\"customers\"}\n:   Warm and concise.\n\nrefunds\n:   Never above the plan limit.\n:::";
        let d = parse(doc).unwrap();
        let e = fold(&d, &grants(&[])).unwrap();
        assert_eq!(e.skills.len(), 2);
        assert_eq!(e.skills[0].name, "tone");
        assert_eq!(e.skills[0].when_to_use.as_deref(), Some("customers"));
        assert!(e.skills[0].body.contains("Warm and concise"));
        assert_eq!(e.skills[1].name, "refunds");
    }

    #[test]
    fn a_markdown_section_is_a_block_whose_body_is_the_section() {
        let doc = "# Agent\n\nIntro.\n\n## !skill support-tone {when=\"writing\"}\n\nWarm, concise.\n\n### Escalation\n\nHand off to a human.\n\n## Refund rules\n\nThis heading ends the skill.";
        let d = parse(doc).unwrap();
        let e = fold(&d, &grants(&[])).unwrap();
        assert_eq!(e.skills.len(), 1);
        assert_eq!(e.skills[0].name, "support-tone");
        assert!(
            e.skills[0].body.contains("Warm, concise"),
            "{:?}",
            e.skills[0].body
        );
        assert!(
            e.skills[0].body.contains("Escalation"),
            "deeper heading is body"
        );
        assert!(
            !e.skills[0].body.contains("Refund rules"),
            "same-level heading ends it"
        );
        // The heading after the section is delivered as prose, not swallowed.
        assert!(e.cleaned.contains("Refund rules"));
    }

    #[test]
    fn a_yaml_section_takes_its_definition_from_the_code_fence() {
        let doc = "## !workflow nightly\n\nRuns at 02:00 and posts a summary.\n\n```yaml\nsteps:\n  wake: {kind: schedule, cron: \"0 2 * * *\"}\n```\n";
        let d = parse(doc).unwrap();
        let e = fold(&d, &grants(&[])).unwrap();
        assert_eq!(e.workflows.len(), 1);
        assert_eq!(e.workflows[0]["name"], "nightly");
        assert!(
            e.workflows[0]["steps"]["wake"].is_object(),
            "definition from the fence"
        );
        assert_eq!(
            e.workflows[0]["description"], "Runs at 02:00 and posts a summary.",
            "surrounding prose becomes the description"
        );
    }

    #[test]
    fn a_sigiled_block_ends_a_markdown_section_never_its_child() {
        // §4.4 boundary (c): a `## !skill` section is ended by the following
        // sigiled `:::!mcp` — machinery is never a section's child. A BARE
        // `:::example` before it belongs to the skill.
        let doc = "## !skill tone\n\nBe warm.\n\n:::example\nHello!\n:::\n\n:::!mcp{name=srv}\nendpoint: https://x/mcp\n:::";
        let d = parse(doc).unwrap();
        let kinds: Vec<&str> = d.blocks().map(|b| b.kind.as_str()).collect();
        assert_eq!(kinds, ["skill", "mcp"], "the mcp is top-level, not a child");
        let e = fold(&d, &grants(&[])).unwrap();
        assert_eq!(e.skills.len(), 1);
        assert!(e.skills[0].body.contains("Be warm"));
        assert_eq!(e.config["mcp"]["servers"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn mcp_deny_maps_to_exclude() {
        // `deny` (the spec's normative attribute) folds to agentd's `exclude`.
        let doc = "::!mcp{name=git endpoint=https://x/mcp deny=\"push:main, force-push\"}";
        let e = fold(&parse(doc).unwrap(), &grants(&[])).unwrap();
        let srv = &e.config["mcp"]["servers"][0];
        assert!(srv.get("deny").is_none(), "deny is renamed");
        assert_eq!(srv["exclude"][0], "push:main");
        assert_eq!(srv["exclude"][1], "force-push");
    }

    #[test]
    fn a_secret_ref_reference_is_not_read_as_a_literal_credential() {
        // `@secret-ref/name` (§3.4) becomes agentd's secret form, chosen by the
        // declared secret-ref's kind: a `kind=file` ref resolves from the file
        // at its path; any other kind resolves the named value.
        let doc = "---\nspec: \"1\"\n---\n::!secret-ref{name=deployer kind=file path=/run/secrets/deployer}\n::!secret-ref{name=tok kind=env}\n:::!mcp{name=t endpoint=https://x/mcp}\nauth: { kind: static, token: \"@secret-ref/tok\" }\nheaders: { X-Deploy: \"@secret-ref/deployer\" }\n:::";
        let e = fold(&parse(doc).unwrap(), &grants(&["identity"])).unwrap();
        let srv = &e.config["mcp"]["servers"][0];
        assert_eq!(
            srv["auth"]["token"], "{{secret:tok}}",
            "kind=env → named value"
        );
        assert_eq!(
            srv["headers"]["X-Deploy"], "{{secret-file:/run/secrets/deployer}}",
            "kind=file → the mounted file at its path"
        );
    }

    #[test]
    fn a_structured_table_cell_parses_as_a_yaml_flow_value() {
        // §4.3.1: a cell carrying `{ … }` is a YAML flow mapping, so a typed
        // field (a stream's `retention`) receives a mapping, not a string.
        let doc = "---\nspec: \"1\"\n---\n:::!stream[]\n| name | retention             |\n|------|------------------------|\n| ev   | { max_events: 5000 }   |\n:::";
        let e = fold(&parse(doc).unwrap(), &grants(&[])).unwrap();
        assert_eq!(e.config["streams"]["ev"]["retention"]["max_events"], 5000);
    }

    #[test]
    fn a_section_form_is_machinery_only() {
        // A sigiled heading for a prose kind is refused (§4.4 rule 5).
        let e = parse("## !note something\n\nbody").unwrap_err();
        assert!(e[0].message.contains("machinery only"), "{e:?}");
        // A bare heading is always just a heading — the guard never applies.
        assert!(parse("## workflow oncall\n\nfree prose").is_ok());
    }

    #[test]
    fn a_leaf_only_kind_refuses_the_container_form() {
        // `!image` is leaf-only (its body is none); a container is refused.
        let e =
            parse("---\nspec: \"1\"\n---\n:::!image{name=x}\ndigest: sha256:a\n:::").unwrap_err();
        assert!(
            e[0].message.contains("does not take the container form"),
            "{e:?}"
        );
    }

    #[test]
    fn a_dangling_inline_reference_is_refused() {
        // A wiki-link to a declared human resolves; a ghost is refused.
        let ok = "::!human{name=oncall role=approver}\n\nAsk [[human/oncall]] first.";
        assert!(parse(ok).is_ok());
        let bad = "::!human{name=oncall role=approver}\n\nAsk [[human/ghost]] first.";
        let e = parse(bad).unwrap_err();
        assert!(e[0].message.contains("human/ghost"), "{e:?}");
        // A fragment link resolves the same way.
        let frag = "::!human{name=oncall role=approver}\n\nSee [the approver](#human/oncall).";
        assert!(parse(frag).is_ok());
        // A `[[…]]` whose kind is not a kind is inert prose, not a reference.
        assert!(parse("See [[the/handbook]] for details.").is_ok());
    }

    // ── delivery pipeline (§3.5 / Appendix A) ────────────────────────────

    #[test]
    fn a_set_delivers_one_grouped_line() {
        let doc = "---\nspec: \"1\"\n---\n:::!human[]\n| name   | role     |\n|--------|----------|\n| oncall | approver |\n| lead   | reviewer |\n| sre    | operator |\n:::";
        let e = fold(&parse(doc).unwrap(), &grants(&["interface"])).unwrap();
        assert!(
            e.cleaned
                .contains("[3 human roles are declared: oncall, lead, sre]"),
            "one grouped line, x-nouns, names only:\n{}",
            e.cleaned
        );
    }

    #[test]
    fn non_delivering_machinery_is_silent() {
        // runtime/secret-ref have no x-acknowledgement — they deliver nothing.
        let doc = "---\nspec: \"1\"\n---\n::!secret-ref{name=tok kind=file path=/run/tok}\n:::!runtime{name=py}\nimage: ghcr.io/x@sha256:abc\n:::";
        let e = fold(&parse(doc).unwrap(), &grants(&["identity", "compute"])).unwrap();
        assert!(
            !e.cleaned.contains("secret-ref") && !e.cleaned.contains("runtime"),
            "{}",
            e.cleaned
        );
        assert!(
            !e.cleaned.contains("[runtime"),
            "runtime delivers nothing:\n{}",
            e.cleaned
        );
    }

    #[test]
    fn normativity_delivers_as_a_keyword_line() {
        let e = fold(
            &parse(":::must\nRun the tests.\n:::").unwrap(),
            &grants(&[]),
        )
        .unwrap();
        assert!(
            e.cleaned.contains("**MUST:** Run the tests."),
            "{}",
            e.cleaned
        );
    }

    #[test]
    fn parameters_substitute_last_and_inline_refs_degrade() {
        let doc = "---\nspec: \"1\"\n---\n::param{name=env default=prod}\n::!human{name=oncall role=approver}\n\nDeploy to ${env}; ask [[human/oncall]] and [the approver](#human/oncall). ${missing} stays.";
        let e = fold(&parse(doc).unwrap(), &grants(&["interface"])).unwrap();
        assert!(
            e.cleaned.contains("Deploy to prod;"),
            "param substituted:\n{}",
            e.cleaned
        );
        assert!(
            e.cleaned.contains("ask oncall and the approver."),
            "refs degraded:\n{}",
            e.cleaned
        );
        assert!(
            e.cleaned.contains("${missing} stays"),
            "undeclared left verbatim:\n{}",
            e.cleaned
        );
    }

    #[test]
    fn config_may_not_grant_itself_trust() {
        // §6 rule 4 / Appendix B self-grant: a document's !config cannot write
        // the operator-only trust surface.
        for key in [
            "document_capabilities",
            "instruction_sources",
            "instruction",
        ] {
            let doc = format!("---\nspec: \"1\"\n---\n:::!config\n{key}: [x]\n:::");
            let e = fold(&parse(&doc).unwrap(), &grants(&[])).unwrap_err();
            assert!(
                e.iter()
                    .any(|m| m.message.contains(key) && m.message.contains("may not write")),
                "!config writing {key} is refused: {e:?}"
            );
        }
    }

    #[test]
    fn an_include_inlines_the_resolved_document_and_degrades_when_not() {
        let base = "Before.\n\n::include{id=\"child\"}\n\nAfter.";
        let child = "# Child\n\nchild body.";
        let resolve = |id: &str| (id == "child").then(|| child.to_string());
        let doc = parse(base).unwrap();
        let e = fold_full(
            &doc,
            &all_families(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &resolve,
            0,
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(
            e.cleaned.contains("# Child") && e.cleaned.contains("child body."),
            "the resolved document is inlined:\n{}",
            e.cleaned
        );
        assert!(e.cleaned.contains("Before.") && e.cleaned.contains("After."));
        // With no resolver the include degrades to a visible note (§5.2 rule 2).
        let e2 = fold(&doc, &all_families()).unwrap();
        assert!(
            e2.cleaned.contains("included instruction not available"),
            "{}",
            e2.cleaned
        );
    }

    #[test]
    fn workflow_step_references_resolve_to_declared_blocks() {
        // `to: @human/x`, `schema: @ui/x`, `template: @agent/x` in a workflow
        // step resolve to the declared block's value (§3.4).
        let doc = r#"---
spec: "1"
---
::!human{name=oncall channel=#ops role=approver}
::::!ui{name=card kind=card}
:::schema
type: object
:::
::::
::!agent{name=rev template=reviewer}
:::!workflow{name=w}
steps:
  ask:    { kind: human, question: "ok?", to: "@human/oncall", schema: "@ui/card" }
  branch: { kind: subagent, template: "@agent/rev" }
:::"#;
        let e = fold(&parse(doc).unwrap(), &grants(&["interface", "compose"])).unwrap();
        let wf = &e.workflows[0];
        assert_eq!(wf["steps"]["ask"]["to"], "#ops", "human → channel");
        assert_eq!(
            wf["steps"]["ask"]["schema"]["type"], "object",
            "ui → schema sub-block"
        );
        assert_eq!(
            wf["steps"]["branch"]["template"], "reviewer",
            "agent → template"
        );
    }

    #[test]
    fn a_form_delivers_its_input_list_not_its_body() {
        // §5.1: a `form` body is an editor template, NOT delivered; the model
        // sees a list of the referenced parameters with their metadata.
        let doc = "---\nspec: \"1\"\n---\n:::param[]\n| name      | required | description             |\n|-----------|----------|-------------------------|\n| ticket_id | true     | The ticket being worked |\n:::\n\n:::form{title=\"Before we start\"}\nWhich ticket are we working on? ${ticket_id}\n:::";
        let e = fold(&parse(doc).unwrap(), &grants(&[])).unwrap();
        assert!(
            e.cleaned.contains("**Inputs to collect — Before we start**\n- **ticket_id** — The ticket being worked; required"),
            "form delivers the input list:\n{}",
            e.cleaned
        );
        assert!(
            !e.cleaned.contains("Which ticket"),
            "the template body is not delivered"
        );
        assert!(
            !e.cleaned.contains("${ticket_id}"),
            "the placeholder does not appear"
        );
    }

    #[test]
    fn delivery_replaces_a_block_region_and_keeps_surrounding_blank_lines() {
        // §3.5 layout: a block's ack replaces exactly its fence region; the
        // blank lines around it are delivered unchanged; a section's region
        // ends at its last non-blank line so the blank before the next heading
        // survives; runs of blanks left by a dropped block collapse to one.
        let doc = "Intro.\n\n:::!workflow{name=w}\nsteps: { f: { kind: manual } }\n:::\n\n::param{name=x default=1}\n\nOutro ${x}.";
        let e = fold(&parse(doc).unwrap(), &grants(&[])).unwrap();
        assert_eq!(
            e.cleaned, "Intro.\n\n[workflow \"w\" is loaded and runs autonomously]\n\nOutro 1.\n",
            "region replaced, blanks kept, dropped param collapsed, one trailing newline:\n{:?}",
            e.cleaned
        );
    }

    #[test]
    fn delivered_text_has_no_front_matter_fences_or_declared_placeholders() {
        // The peer's guard: after delivery, no `---`, no `:::`, no declared `${`.
        let doc = "---\nspec: \"1\"\n---\n# Title\n\n::param{name=env default=prod}\n\nGuidance for ${env}.\n\n:::!workflow{name=w}\nsteps: { f: { kind: manual } }\n:::";
        let e = fold(&parse(doc).unwrap(), &grants(&[])).unwrap();
        assert!(
            !e.cleaned.contains("---"),
            "no front matter:\n{}",
            e.cleaned
        );
        assert!(!e.cleaned.contains(":::"), "no fences:\n{}", e.cleaned);
        assert!(
            !e.cleaned.contains("${env}"),
            "declared param substituted:\n{}",
            e.cleaned
        );
    }

    #[test]
    fn an_indented_fence_is_prose_not_machinery() {
        // fence-column-zero: an indented `:::!workflow` is delivered as text.
        let d = parse("Here is an example:\n\n    :::!workflow{name=x}\n    steps: {}\n    :::")
            .unwrap();
        assert!(d.blocks().next().is_none(), "no machinery parsed");
        let e = fold(&d, &grants(&[])).unwrap();
        assert!(e.workflows.is_empty());
        assert!(e.cleaned.contains(":::!workflow"), "shown verbatim");
    }

    /// Until the 1.1 variant semantics land, a structural block with a body
    /// (`unless`, `otherwise`) delivers that body unwrapped — the behaviour
    /// it had as an unknown bare kind. A load that reports success must never
    /// have dropped the guidance inside, whatever the facts say.
    #[test]
    fn a_structural_block_with_a_body_never_loses_it() {
        let doc = parse(
            "# T\n\n:::unless{environment=\"prod\"}\nDebug freely.\n:::\n\n\
             :::otherwise\nBe careful.\n:::\n\n::param{name=p default=x}\n\nUse ${p}.",
        )
        .unwrap();
        for env in ["prod", "dev"] {
            let facts: BTreeMap<String, String> = [("environment".into(), env.into())].into();
            let out = fold_full(
                &doc,
                &all_families(),
                &BTreeMap::new(),
                &facts,
                &|_| None,
                0,
                &BTreeSet::new(),
            )
            .unwrap()
            .cleaned;
            assert_eq!(
                out, "# T\n\nDebug freely.\n\nBe careful.\n\nUse x.\n",
                "environment={env}"
            );
        }
    }

    /// `SHOULD NOT:` maps to the same kind as `SHOULD:`; the registry's
    /// keyword flag and negated label are all that keep it a prohibition, in
    /// every spelling: the line, a list item, and `:::should{not}`.
    #[test]
    fn should_not_keeps_its_polarity_in_delivery() {
        let out = fold(
            &parse(
                "SHOULD NOT: store data.\n\n- SHOULD NOT: paste tickets.\n- SHOULD: link.\n\n\
                 :::should{not}\nkeep copies.\n:::\n\n:::should\nask first.\n:::",
            )
            .unwrap(),
            &all_families(),
        )
        .unwrap()
        .cleaned;
        assert_eq!(
            out,
            "**SHOULD NOT:** store data.\n\n- **SHOULD NOT:** paste tickets.\n- **SHOULD:** link.\n\n\
             **SHOULD NOT:** keep copies.\n\n**SHOULD:** ask first.\n"
        );
    }

    /// Every refusal comes from the document, as a reader would see it: the
    /// parse refusals, else what folding under `grants` refuses.
    fn refusals_of(doc: &Document, granted: &BTreeSet<String>) -> Vec<Refusal> {
        fold(doc, granted).err().unwrap_or_default()
    }

    /// The code is the contract (S20), so every constructor site's choice of
    /// code is pinned here — line, code, and a fragment of the message that
    /// names the site — including the sites no `refusals.json` fixture
    /// reaches and the codes several sites share, where the accounting test
    /// in `refusal.rs` only proves SOME site builds the code.
    #[test]
    fn every_refusal_site_names_its_line_and_code() {
        let all = all_families();
        // (document, all families granted?, line, code, message fragment)
        let cases: &[(&str, bool, Option<u32>, &str, &str)] = &[
            (
                ":::note\nopen",
                true,
                Some(1),
                "unclosed-fence",
                "never closed",
            ),
            (
                "## !workflow nightly\n\nprose only",
                true,
                Some(1),
                "section-without-fence",
                "",
            ),
            // A sigiled sub-block is no machinery kind of its own.
            (
                ":::!case{name=c}\ngiven: {}\n:::",
                true,
                Some(1),
                "unknown-machinery-kind",
                "is a sub-block",
            ),
            (
                ":::workflow{name=w}\nsteps: {}\n:::",
                true,
                Some(1),
                "bare-machinery-kind",
                "is a machinery kind",
            ),
            (
                ":::!note\nhi\n:::",
                true,
                Some(1),
                "sigiled-prose-kind",
                "is a prose kind",
            ),
            (
                ":::!when{env=prod}\nhi\n:::",
                true,
                Some(1),
                "sigiled-prose-kind",
                "is a structural kind",
            ),
            (
                "## !must r1\n\ntext",
                true,
                Some(1),
                "sigiled-prose-kind",
                "the section form is machinery only",
            ),
            (
                ":::glossary[]\nTerm\n:   def\n:::",
                true,
                Some(1),
                "redundant-set",
                "already a list",
            ),
            (
                ":::!nope\nx\n:::",
                true,
                Some(1),
                "unknown-machinery-kind",
                "unknown machinery kind",
            ),
            (
                "::!workflow{name=w}",
                true,
                Some(1),
                "body-required",
                "requires a body",
            ),
            (
                ":::!image{name=i digest=sha256:ab}\n:::",
                true,
                Some(1),
                "form-not-accepted",
                "does not take the container form",
            ),
            (
                ":::!human[]\n| name |\n:::",
                true,
                Some(1),
                "set-body-mixed",
                "a table or a definition list",
            ),
            (
                ":::!human[]\n| name | channel_id |\n|---|---|\n| a | x |\n:::",
                true,
                Some(1),
                "unknown-column",
                "not an attribute of human",
            ),
            (
                ":::!human[]\n| channel |\n|---|\n| x |\n:::",
                true,
                Some(1),
                "set-row-without-name",
                "has no name",
            ),
            (
                ":::!human[]\nnot a table\n:::",
                true,
                Some(1),
                "set-body-mixed",
                "a table or a definition list",
            ),
            (
                ":::note #x\nhi\n:::",
                true,
                Some(1),
                "malformed-attributes",
                "wrapped in { }",
            ),
            (
                ":::must{#ops}\nhi\n:::",
                true,
                Some(1),
                "id-class-shorthand",
                "identity is name=ops",
            ),
            (
                ":::must{.loud}\nhi\n:::",
                true,
                Some(1),
                "malformed-attributes",
                "found \".\"",
            ),
            (
                ":::must{=x}\nhi\n:::",
                true,
                Some(1),
                "malformed-attributes",
                "empty attribute name",
            ),
            (
                ":::must{verbatim verbatim}\nhi\n:::",
                true,
                Some(1),
                "repeated-attribute",
                "is repeated",
            ),
            (
                ":::must{name=on call}\nhi\n:::",
                true,
                Some(1),
                "malformed-attributes",
                "found \"call\"",
            ),
            (
                ":::must{title=a}b}\nhi\n:::",
                true,
                Some(1),
                "malformed-attributes",
                "runs into",
            ),
            (
                ":::must{title=a title=b}\nhi\n:::",
                true,
                Some(1),
                "repeated-attribute",
                "is repeated",
            ),
            (
                "---\n- a\n---\nhi",
                true,
                Some(1),
                "front-matter-yaml",
                "must be a YAML mapping",
            ),
            (
                "---\na: [\n---\nhi",
                true,
                Some(1),
                "front-matter-yaml",
                "not valid YAML",
            ),
            (
                "---\nspec: \"1.0\"\n---\nhi",
                true,
                None,
                "non-integer-version",
                "versions are integers",
            ),
            (
                "---\nspec: \"1.0\"\n---\nhi",
                true,
                None,
                "schema",
                "must match pattern",
            ),
            (
                "---\nspec: \"2\"\n---\nhi",
                true,
                None,
                "unimplemented-version",
                "not implemented",
            ),
            (
                "text\n\n:::!workflow\nsteps: {}\n:::",
                true,
                Some(3),
                "missing-attribute",
                "workflow requires name",
            ),
            (
                ":::!workflow{name=w}\nsteps: {}\n:::\n\n:::!workflow{name=w}\nsteps: {}\n:::",
                true,
                Some(5),
                "duplicate-identity",
                "first declared at line 1",
            ),
            (
                ":::!skill{name=s may=@bare}\nhi\n:::",
                true,
                Some(1),
                "attribute-value",
                "must be qualified",
            ),
            (
                ":::!skill{name=s may=@case/c}\nhi\n:::",
                true,
                Some(1),
                "subblock-reference",
                "cannot be referenced",
            ),
            (
                ":::!skill{name=s may=@workflow/nope}\nhi\n:::",
                true,
                Some(1),
                "dangling-reference",
                "does not resolve",
            ),
            (
                "x\n\n:::!skill{name=a may=@skill/b}\nhi\n:::\n\n:::!skill{name=b may=@skill/a}\nhi\n:::",
                true,
                Some(3),
                "reference-cycle",
                "reference cycle: skill/a → skill/b → skill/a",
            ),
            (
                "x\n\n:::must{because=\"a\"}\nx\n:::\n\nBECAUSE: b",
                true,
                Some(7),
                "because-repeated",
                "must already has a reason (because=) — a rule has one reason",
            ),
            (
                "See [[case/c]].",
                true,
                Some(1),
                "subblock-reference",
                "cannot be referenced",
            ),
            (
                "x\nSee [[workflow/nope]].",
                true,
                Some(2),
                "dangling-reference",
                "does not resolve",
            ),
            (
                ":::!runtime{name=r}\nimage: x\n:::",
                false,
                Some(1),
                "ungranted-family",
                "needs the `compute` capability",
            ),
            (
                ":::override{target=t}\ndisabled: true\n:::",
                true,
                Some(1),
                "subblock-out-of-place",
                "only inside a mcp",
            ),
            // body_map, the explicit kinds' body.
            (
                ":::!workflow{name=w}\n- a\n:::",
                true,
                Some(1),
                "invalid-yaml-body",
                "!workflow body must be a YAML mapping",
            ),
            (
                ":::!workflow{name=w}\na: [\n:::",
                true,
                Some(1),
                "invalid-yaml-body",
                "!workflow body is not valid YAML",
            ),
            // The fallback for every other YAML-bodied kind.
            (
                ":::!runtime{name=r}\n- a\n:::",
                true,
                Some(1),
                "invalid-yaml-body",
                "!runtime body must be a YAML mapping",
            ),
            (
                ":::!runtime{name=r}\na: [\n:::",
                true,
                Some(1),
                "invalid-yaml-body",
                "!runtime body is not valid YAML",
            ),
            (
                "::::!mcp{name=m}\n:::override\ndisabled: true\n:::\n::::",
                true,
                Some(2),
                "missing-attribute",
                "needs a target",
            ),
            (
                "::::!mcp{name=m}\n:::override{target=t}\ndisabled: false\n:::\n::::",
                true,
                Some(2),
                "widening-override",
                "may not re-enable",
            ),
            (
                ":::!config\ndocument_capabilities: [compute]\n:::",
                true,
                Some(1),
                "self-grant",
                "may not write `document_capabilities`",
            ),
            (
                "::!image{name=i}",
                true,
                Some(1),
                "mutable-image-tag",
                "not digest-pinned",
            ),
            (
                ":::!runtime{name=r}\nnetwork: any\n:::",
                true,
                Some(1),
                "network-any",
                "network: any is refused",
            ),
            (
                "::!asset{name=a src=https://x.example/a.png}",
                true,
                Some(1),
                "unpinned-remote-asset",
                "requires sha256",
            ),
            (
                ":::!config\nkey: sk-ant-abcdefghijklmnopqrstuvwxyz0123\n:::",
                true,
                Some(2),
                "literal-credential",
                "literal credential",
            ),
            // An inert block that never closes (S23).
            (
                "intro\n:::aside\nopen",
                true,
                Some(2),
                "unclosed-fence",
                ":::aside is never closed",
            ),
            (
                "Body.\n\n---\nk: [a\n---\n",
                true,
                Some(4),
                "end-matter-yaml",
                "end matter is not valid YAML",
            ),
            (
                "::param{name=n source=env}",
                true,
                Some(1),
                "attribute-value",
                "source must be one of: static, workspace, agent_attribute, prompt",
            ),
            (
                "\n:::output{schema=reply}\nx\n:::",
                true,
                Some(2),
                "attribute-value",
                "schema must be a reference written @kind/name",
            ),
            (
                ":::unless\nx\n:::",
                true,
                Some(1),
                "missing-attribute",
                "unless requires at least one condition",
            ),
            (
                "GUARDRAIL[g]: no.\n\n:::must{overrides=guardrail/g}\nx\n:::",
                true,
                Some(3),
                "override-guardrail",
                "guardrail/g is a guardrail",
            ),
            (
                "MUST[m]: a\n\n:::should{overrides=must/m}\nx\n:::",
                true,
                Some(3),
                "override-stronger",
                "a should may not override the stronger must/m",
            ),
            (
                "MUST[m]: a\n\n:::!eval{name=e target=@must/m}\ncases: [\n:::",
                true,
                Some(3),
                "invalid-yaml-body",
                ":::!eval body is not valid YAML",
            ),
        ];
        let mut misses = Vec::new();
        for (text, granted_all, line, code, fragment) in cases {
            let granted = if *granted_all {
                all.clone()
            } else {
                BTreeSet::new()
            };
            let got = match parse(text) {
                Err(e) => e,
                Ok(d) => refusals_of(&d, &granted),
            };
            if !got
                .iter()
                .any(|r| r.line == *line && r.code == *code && r.message.contains(fragment))
            {
                misses.push(format!(
                    "{text:?}\n  want [{line:?}] {code} ~{fragment:?}\n  got  {got:?}"
                ));
            }
        }
        // The fold-time name checks sit behind parse's own (`check_identity`
        // refuses an unnamed block first), so they are reached the way a
        // `Document` built by hand reaches them: with the name taken away.
        for (text, fragment) in [
            (
                "::::!mcp{name=m}\nurl: https://x.example\n::::",
                "mcp requires name",
            ),
            (
                ":::!stream{name=s}\nretain: 1h\n:::",
                "stream requires name",
            ),
            (":::!skill{name=s}\nhi\n:::", "skill requires name"),
            (
                ":::!endpoint{name=e}\npath: /x\n:::",
                "endpoint requires name",
            ),
        ] {
            let mut d = parse(text).unwrap();
            for n in &mut d.nodes {
                if let Node::Block(b) = n {
                    b.name = None;
                    b.attrs.remove("name");
                }
            }
            let got = refusals_of(&d, &all);
            if !got.iter().any(|r| {
                r.line == Some(1) && r.code == "missing-attribute" && r.message == fragment
            }) {
                misses.push(format!(
                    "{text:?} (unnamed)\n  want [1] missing-attribute {fragment:?}\n  got  {got:?}"
                ));
            }
        }
        assert!(misses.is_empty(), "{}", misses.join("\n"));
    }

    // ── the 1.1 keyword grammar, reasons and identity (S8, S11-S15, S26) ──

    /// The single block a document parses to, at the top level.
    fn only_block(text: &str) -> Block {
        let d = parse(text).unwrap();
        let blocks: Vec<&Block> = d.blocks().collect();
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        blocks[0].clone()
    }

    /// `SHOULD NOT:` maps to the same kind as `SHOULD:`, so the `not` flag is
    /// the only thing in the TREE that keeps it a prohibition (S8) — on the
    /// line, on a list item, and on the container.
    #[test]
    fn should_not_carries_the_not_flag_in_the_tree() {
        let d = parse(
            "SHOULD NOT: store data.\n\n- SHOULD NOT: paste tickets.\n- SHOULD: link.\n\n\
             :::should{not}\nkeep copies.\n:::",
        )
        .unwrap();
        let got: Vec<(&str, Form, Option<&str>)> = d
            .blocks()
            .map(|b| {
                (
                    b.kind.as_str(),
                    b.form,
                    b.attrs.get("not").map(String::as_str),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("should", Form::Keyword, Some("true")),
                ("should", Form::Keyword, Some("true")),
                ("should", Form::Keyword, None),
                ("should", Form::Container, Some("")),
            ]
        );
        let t = crate::tree_json(&d);
        for (i, want) in [true, true, false, true].into_iter().enumerate() {
            assert_eq!(
                t["blocks"][i]["attrs"].get("not").is_some(),
                want,
                "block {i}: {}",
                t["blocks"][i]
            );
        }
    }

    /// `MUST[name] (if c):` (S12, S15): the name is the block's identity, the
    /// condition its `if`, and the text after the label its body.
    #[test]
    fn a_keyword_carries_its_name_and_condition() {
        let b = only_block("NEVER[refund-ceiling] (if the refund exceeds ${limit}): promise it.");
        assert_eq!(b.kind, "never");
        assert_eq!(b.name.as_deref(), Some("refund-ceiling"));
        assert_eq!(b.attrs["name"], "refund-ceiling");
        assert_eq!(b.attrs["if"], "the refund exceeds ${limit}");
        assert_eq!(b.body, "promise it.");
        // Each alone, on a list item, inside bold.
        let b = only_block("- **MUST (if  asked ):** reply.");
        assert_eq!((b.name.as_deref(), b.attrs["if"].as_str()), (None, "asked"));
        assert_eq!(b.body, "reply.");
        let b = only_block("1) MAY[skip]: skip it.");
        assert_eq!((b.kind.as_str(), b.name.as_deref()), ("may", Some("skip")));
        assert!(!b.attrs.contains_key("if"));
        // The longer keyword wins, and a near miss is prose: no name grammar,
        // parentheses in the condition, no `if`, no space after the colon,
        // an indented line, lower case.
        assert_eq!(only_block("MUST NOT: push.").kind, "never");
        // A paragraph runs on to the next blank line, and stops where a list
        // item or a quote starts.
        let d = parse("MAY: skip the greeting\nfor insiders.\n- an item\n> a quote").unwrap();
        assert_eq!(
            d.blocks().next().unwrap().body,
            "skip the greeting\nfor insiders."
        );
        for prose in [
            "MUST NOTE: x",
            "MUST[-x]: y",
            "MUST (if a (b)): y",
            "MUST (if a (b): y",
            "MUST (when a): y",
            "MUST (if ): y",
            "MUST:y",
            "  MUST: indented",
            "must: lower",
        ] {
            assert!(
                parse(prose).unwrap().blocks().next().is_none(),
                "{prose:?} is prose"
            );
        }
    }

    /// An alias kind stays as authored in the tree (`always`, `avoid`,
    /// `may`), with no flags: `AVOID` is an alias of `SHOULD NOT`, not a
    /// `should` block with `not`. The registry's label carries the rest.
    #[test]
    fn alias_kinds_stay_as_authored() {
        let d = parse(
            "ALWAYS: cite.\n\nAVOID: jargon.\n\n> [!MAY]\n> skip it.\n\n:::avoid\nslang.\n:::",
        )
        .unwrap();
        let got: Vec<(&str, Form, usize)> = d
            .blocks()
            .map(|b| (b.kind.as_str(), b.form, b.attrs.len()))
            .collect();
        assert_eq!(
            got,
            [
                ("always", Form::Keyword, 0),
                ("avoid", Form::Keyword, 0),
                ("may", Form::Alert, 0),
                ("avoid", Form::Container, 0),
            ]
        );
        let r = registry();
        assert_eq!(r.label("always", false), Some("MUST"));
        assert_eq!(r.label("avoid", false), Some("SHOULD NOT"));
        assert_eq!(r.label("info", false), Some("NOTE"));
        assert_eq!(r.label("should", true), Some("SHOULD NOT"));
        assert_eq!(r.label("should", false), Some("SHOULD"));
        assert_eq!(lookup("always").unwrap().alias_of.as_deref(), Some("must"));
        assert!(r.is_rule("avoid") && !r.is_rule("note"));
    }

    /// A `BECAUSE:` is the reason of the rule before it (S14): directly
    /// after the text (indented on a list item), or after blank lines at
    /// column 0. Its lines leave the rule's body and join its region.
    #[test]
    fn a_reason_attaches_to_the_rule_before_it() {
        let d = parse(
            "MUST: confirm the plan.\nBECAUSE: limits differ\n  by plan.\n\n\
             - MUST: link the ticket.\n  BECAUSE: the audit follows links.\n\n\
             NEVER: guess.\n\nBECAUSE: a wrong answer costs more.\n\n\
             :::never\nPromise a refund.\n:::\n- BECAUSE: finance decides.\n\n\
             NOTE: not a rule.\n\nBECAUSE: orphan.\n\n\
             SHOULD: reply.\n\n  BECAUSE: indented after a blank.",
        )
        .unwrap();
        let blocks: Vec<&Block> = d.blocks().collect();
        let because = |i: usize| blocks[i].attrs.get("because").map(String::as_str);
        assert_eq!(because(0), Some("limits differ\nby plan."));
        assert_eq!(blocks[0].body, "confirm the plan.");
        assert_eq!((blocks[0].region, blocks[0].reason), ((0, 2), Some((1, 2))));
        assert_eq!(because(1), Some("the audit follows links."));
        assert_eq!(blocks[1].body, "link the ticket.");
        assert_eq!(because(2), Some("a wrong answer costs more."));
        assert_eq!(blocks[2].region, (7, 9));
        assert_eq!(because(3), Some("finance decides."));
        assert_eq!(
            (blocks[3].kind.as_str(), blocks[3].region),
            ("never", (11, 14))
        );
        // A `BECAUSE:` after a block that is not a rule, or indented after a
        // blank line, is prose.
        assert_eq!((blocks[4].kind.as_str(), because(4)), ("note", None));
        assert_eq!((blocks[5].kind.as_str(), because(5)), ("should", None));
        assert_eq!(blocks.len(), 6);
        assert!(
            d.nodes
                .iter()
                .any(|n| matches!(n, Node::Text(t) if t.contains("BECAUSE: orphan.")))
        );
        // A reason never steals the next rule's text, nor follows one past a
        // container's close.
        let d = parse(":::when{agent=x}\nMUST: a.\n:::\nBECAUSE: b.").unwrap();
        let w = d.blocks().next().unwrap();
        assert_eq!(w.children[0].attrs.get("because"), None);
    }

    /// A rule has one reason (S14): `because=` and a `BECAUSE:` paragraph
    /// together are refused at the paragraph, nested or not.
    #[test]
    fn a_second_reason_is_refused() {
        for (text, line) in [
            (":::must{because=\"a\"}\nx\n:::\nBECAUSE: b", 4),
            (
                "::::when{agent=x}\n:::must{because=\"a\"}\nx\n:::\n\nBECAUSE: b\n::::",
                6,
            ),
        ] {
            let e = parse(text).unwrap_err();
            assert_eq!(
                (e[0].line, e[0].code, e[0].message.as_str()),
                (
                    Some(line),
                    "because-repeated",
                    "must already has a reason (because=) — a rule has one reason"
                ),
                "{text:?}"
            );
        }
        assert!(parse(":::must{because=\"a\"}\nx\n:::").is_ok());
    }

    /// Until delivery renders reasons, a container's `BECAUSE:` paragraph,
    /// now inside the rule's region, is delivered exactly as it was when it
    /// was prose after the block — never dropped.
    #[test]
    fn a_containers_reason_is_still_delivered() {
        for (doc, want) in [
            (
                ":::must\nx\n:::\n\nBECAUSE: y\nand z.\n\nAfter.",
                "**MUST:** x\n\nBECAUSE: y\nand z.\n\nAfter.\n",
            ),
            (":::must\nx\n:::\nBECAUSE: y", "**MUST:** x\nBECAUSE: y\n"),
            (
                "::::context\n:::must\nx\n:::\nBECAUSE: y\n::::",
                "<reference>\nBECAUSE: y\n</reference>\n",
            ),
        ] {
            let d = parse(doc).unwrap();
            assert!(
                d.blocks()
                    .any(|b| b.reason.is_some() || b.children.iter().any(|c| c.reason.is_some())),
                "{doc:?}"
            );
            assert_eq!(fold(&d, &all_families()).unwrap().cleaned, want, "{doc:?}");
        }
    }

    /// Identity is document-wide (S26): a named rule inside a container body
    /// clashes with one at the top level, and is what a reference resolves.
    #[test]
    fn identity_and_references_reach_nested_blocks() {
        let e = parse("MUST[a]: x\n\n:::when{agent=y}\nMUST[a]: z\n:::").unwrap_err();
        assert_eq!(
            (e[0].line, e[0].code, e[0].message.as_str()),
            (
                Some(4),
                "duplicate-identity",
                "duplicate must/a (first declared at line 1)"
            )
        );
        let e =
            parse("## !skill s\n\n:::note{name=n}\nx\n:::\n\n:::note{name=n}\ny\n:::").unwrap_err();
        assert_eq!(e[0].code, "duplicate-identity", "{e:?}");
        // Nested names resolve, from a wiki-link and from an attribute.
        assert!(
            parse(":::when{agent=y}\nNEVER[cap]: exceed it.\n:::\n\nSee [[never/cap]].").is_ok()
        );
        assert!(
            parse(":::when{agent=y}\n:::!workflow{name=w}\nsteps: {}\n:::\n:::\n\n:::!skill{name=s may=@workflow/w}\nx\n:::")
                .is_ok()
        );
        // A sub-block's name is scoped to its parent: two tests may each
        // have a case called `one`.
        assert!(
            parse(
                ":::!function{name=f}\nx\n:::\n\
             ::::!test{name=a target=@function/f}\n:::case{name=one}\ngiven: {}\n:::\n::::\n\
             ::::!test{name=b target=@function/f}\n:::case{name=one}\ngiven: {}\n:::\n::::"
            )
            .is_ok()
        );
    }

    /// An `@` in a free-text attribute is prose, never a reference; in any
    /// other attribute it is one, and an unqualified one is still refused —
    /// `function.target` has no schema pattern to catch it otherwise.
    #[test]
    fn an_at_in_free_text_is_prose_and_elsewhere_a_reference() {
        for doc in [
            ":::must{if=\"@ops asks\" because=\"@ops said so\" title=\"@ops\"}\nx\n:::",
            "MUST (if @ops asks): x",
            ":::!skill{name=s description=\"ask @ops\" trigger=\"@ops pings\"}\nx\n:::",
            ":::!skill{name=s when=\"@ops pings\"}\nx\n:::",
        ] {
            assert!(parse(doc).is_ok(), "{doc:?}: {:?}", parse(doc).err());
        }
        let e = parse(":::!function{name=f target=@bare}\nx\n:::").unwrap_err();
        assert_eq!(
            (e[0].code, e[0].message.as_str()),
            (
                "attribute-value",
                "target=@bare must be qualified as @kind/name"
            )
        );
        // `when` is free text on a skill only.
        let e = parse("::!human{name=h when=@bare}").unwrap_err();
        assert_eq!(e[0].code, "attribute-value", "{e:?}");
    }

    /// A skill's `trigger` (S11) is what the catalogue says it is for;
    /// `when` is its alias, and `trigger` wins when both are given.
    #[test]
    fn a_skill_trigger_wins_over_when() {
        let when_to_use = |doc: &str| {
            fold(&parse(doc).unwrap(), &all_families()).unwrap().skills[0]
                .when_to_use
                .clone()
        };
        assert_eq!(
            when_to_use(":::!skill{name=s trigger=\"t\" when=\"w\"}\nx\n:::").as_deref(),
            Some("t")
        );
        assert_eq!(
            when_to_use(":::!skill{name=s when=\"w\"}\nx\n:::").as_deref(),
            Some("w")
        );
        assert_eq!(
            when_to_use("## !skill s {trigger=\"t\"}\n\nx").as_deref(),
            Some("t")
        );
    }

    /// The 1.1 registry tables load from the schema: the sigil→scheme table
    /// a reference is checked against, the wire floor, the rule strengths,
    /// the context keys and the attribute rules.
    #[test]
    fn the_registry_tables_load() {
        let r = registry();
        let sigils: Vec<(&str, Vec<&str>)> = r
            .sigils()
            .iter()
            .map(|(s, v)| (s.as_str(), v.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            sigils,
            [
                ("#", vec!["instruction"]),
                (
                    "&",
                    vec![
                        "server",
                        "skill",
                        "model",
                        "service",
                        "sandbox",
                        "connector"
                    ]
                ),
                ("@", vec!["principal", "agent"]),
            ]
        );
        assert_eq!(r.wire_floor(), ["compose", "identity"]);
        assert_eq!(
            (r.strength("guardrail"), r.strength("may")),
            (Some(4), Some(1))
        );
        assert_eq!(r.reason_keyword(), "BECAUSE");
        assert!(r.context_keys().iter().any(|k| k == "agent"));
        assert!(r.label_styles().iter().any(|s| s == "tags"));
        assert!(r.reserved_bare("workflow") && !r.reserved_bare("eval"));
        assert_eq!(
            r.attr_rule("param", "source")
                .and_then(|a| a.values.clone()),
            Some(vec![
                "static".to_string(),
                "workspace".into(),
                "agent_attribute".into(),
                "prompt".into()
            ])
        );
        assert!(
            r.attr_rule("eval", "target")
                .is_some_and(|a| a.pattern.is_some())
        );
        assert!(
            lookup("skill")
                .unwrap()
                .ack_trigger
                .as_deref()
                .is_some_and(|t| t.contains("{trigger}"))
        );
        assert!(lookup("mcp").unwrap().body_schema.is_some());
        assert!(
            r.keywords_longest_first()
                .windows(2)
                .all(|w| w[0].len() >= w[1].len()),
            "longest first"
        );
    }

    /// The wire floor `sign` enforces is the registry's: the hard-coded list
    /// cannot drift from the schema.
    #[cfg(feature = "sign")]
    #[test]
    fn the_wire_floor_is_the_registrys() {
        assert_eq!(registry().wire_floor(), crate::sign::WIRE_FLOOR);
    }

    // ── author notes, end matter, inert blocks, attribute values and
    //    overrides (S9, S10, S16-S17, S23, S24, S27) ──

    /// The top-level nodes' shapes, without their contents.
    fn shapes(d: &Document) -> Vec<String> {
        d.nodes
            .iter()
            .map(|n| match n {
                Node::Text(t) => format!("text {t:?}"),
                Node::Note { start, end } => format!("note {start}-{end}"),
                Node::Inert { kind, .. } => format!("inert {kind}"),
                Node::Block(b) => format!("{} @{}", b.kind, b.line),
            })
            .collect()
    }

    /// A column-0 `<!--` opens a note that ends at the first `-->`; it is its
    /// own node, nothing in it is read, and one that never closes runs to the
    /// end without a refusal (§3.3 rule 11).
    #[test]
    fn an_author_note_is_its_own_node_and_nothing_in_it_is_read() {
        // One line, and a keyword right after it with no blank line between.
        let d = parse("<!-- legal asked for this -->\nMUST: quote it.\n").unwrap();
        assert_eq!(shapes(&d), ["note 0-0", "must @2"]);
        // Several lines: the commented-out machinery is never parsed or folded.
        let doc = "Intro.\n<!--\n:::!workflow{name=old}\nsteps: {}\n:::\n-->\nAfter.\n";
        let d = parse(doc).unwrap();
        assert_eq!(
            shapes(&d),
            ["text \"Intro.\"", "note 1-5", "text \"After.\\n\""]
        );
        assert!(extract(doc, &BTreeSet::new()).unwrap().workflows.is_empty());
        // `<!-->` does not close itself: the search starts past the opener.
        let d = parse("<!-->\nMUST: hidden\n-->\nshown\n").unwrap();
        assert_eq!(shapes(&d), ["note 0-2", "text \"shown\\n\""]);
        // Unclosed: to the end, and not refused.
        let d = parse("Intro.\n<!-- open\n:::!workflow{name=w}\n").unwrap();
        assert_eq!(shapes(&d), ["text \"Intro.\"", "note 1-3"]);
        // A comment that begins mid-line is inline HTML: prose.
        let d = parse("Inline <!-- stays --> text.\n").unwrap();
        assert_eq!(shapes(&d), ["text \"Inline <!-- stays --> text.\\n\""]);
        // A note inside fenced code is code.
        let d = parse("```\n<!--\n```\nMUST: x\n").unwrap();
        assert_eq!(d.blocks().count(), 1);
    }

    /// A reference inside a note is never resolved, at the top level or in a
    /// body, while the same reference outside one is refused.
    #[test]
    fn a_reference_inside_a_note_is_not_scanned() {
        assert!(parse("<!-- see [[must/nowhere]] -->\n").is_ok());
        assert!(parse("<!--\nsee [[must/nowhere]]\n-->\n").is_ok());
        assert!(parse(":::note\n<!-- see [[must/nowhere]] -->\nx\n:::\n").is_ok());
        let e = parse("see [[must/nowhere]]\n").unwrap_err();
        assert_eq!(e[0].code, "dangling-reference");
        // A fence inside a note is no code: the line after the note is read.
        let e = parse("<!--\n```\n-->\nsee [[must/nowhere]]\n").unwrap_err();
        assert_eq!((e[0].line, e[0].code), (Some(4), "dangling-reference"));
    }

    /// In a Markdown body a note's lines stay in the body text (the tree
    /// keeps them) and are recorded for delivery to strip, but no fence in it
    /// opens or closes a block and no keyword in it is lifted.
    #[test]
    fn a_note_in_a_markdown_body_stays_in_its_text_and_is_never_read() {
        let b = only_block(":::context{title=\"T\"}\nA.\n<!--\nMUST: hidden\n:::\n-->\nB.\n:::\n");
        assert_eq!(b.body, "A.\n<!--\nMUST: hidden\n:::\n-->\nB.");
        assert!(b.children.is_empty(), "{:?}", b.children);
        assert_eq!(b.notes, [(2, 5)]);
        // In a section, a heading inside a note does not end the section.
        let b = only_block("## !skill s\nDo it.\n<!--\n## !skill t\n-->\nMore.\n");
        assert!(b.body.contains("## !skill t") && b.body.contains("More."));
        assert_eq!(b.notes, [(2, 4)]);
        // A keyword paragraph stops at a note: the note is not its text.
        let b = only_block("MUST: quote it.\n<!-- why -->\n");
        assert_eq!(b.body, "quote it.");
    }

    /// In a YAML or code body `<!--` is content, and a verbatim body is raw:
    /// the close after it closes the block.
    #[test]
    fn a_note_opener_in_a_yaml_code_or_verbatim_body_is_content() {
        let d = parse(":::!workflow{name=w}\n<!--\n:::\nAfter.\n").unwrap();
        let b = d.blocks().next().unwrap();
        assert_eq!((b.body.as_str(), b.notes.is_empty()), ("<!--", true));
        let d = parse(":::note{verbatim}\n<!--\n:::\nAfter.\n").unwrap();
        let b = d.blocks().next().unwrap();
        assert_eq!((b.body.as_str(), b.notes.is_empty()), ("<!--", true));
        // In a YAML section's description too.
        let b = only_block("## !workflow w\n<!--\n```yaml\nsteps: {}\n```\n");
        assert_eq!(b.body, "steps: {}");
    }

    #[test]
    fn contains_blocks_ignores_a_fence_inside_a_note() {
        assert!(!contains_blocks(
            "<!--\n:::!workflow{name=w}\nsteps: {}\n:::\n-->\n"
        ));
        assert!(!contains_blocks("<!-- open\n::!human{name=a}\n"));
        assert!(contains_blocks("<!-- x -->\n::!human{name=a}\n"));
    }

    /// The end matter a document is split at, if any — `split_end_matter`'s
    /// keys, or `None`.
    fn end_matter_of(text: &str) -> Option<BTreeMap<String, Value>> {
        split_end_matter(text).unwrap().end_matter
    }

    /// End matter is the document's record (S27): read into the tree, cut
    /// from the body, and kept in `raw`, which the author digest covers.
    #[test]
    fn end_matter_is_the_documents_record_and_not_its_body() {
        let text = "# Desk\n\nMUST: quote it.\n\n---\nowners: [ana]\n---\n";
        let split = split_end_matter(text).unwrap();
        assert_eq!(split.before, "# Desk\n\nMUST: quote it.\n\n");
        assert_eq!(split.opener_line, Some(5));
        let d = parse(text).unwrap();
        assert_eq!(
            d.end_matter,
            Some([("owners".to_string(), serde_json::json!(["ana"]))].into())
        );
        assert!(!d.source.contains("owners"), "{:?}", d.source);
        assert_eq!(d.raw, text);
        assert_eq!(d.blocks().count(), 1);
        // No end matter: the whole text, nothing split.
        let split = split_end_matter("plain\n").unwrap();
        assert_eq!(
            (split.before, split.end_matter, split.opener_line),
            ("plain\n", None, None)
        );
    }

    /// Each condition of `findEndMatter` (§3.1.1), one by one.
    #[test]
    fn end_matter_is_found_only_where_the_reference_finds_it() {
        // A thematic break followed by a blank line stays prose…
        assert_eq!(end_matter_of("Intro.\n\n---\n\nk: v\n---\n"), None);
        // …and so does a setext heading's underline.
        assert_eq!(end_matter_of("Intro.\n\nkey: value\n---\n"), None);
        // The opener follows a blank line.
        assert_eq!(end_matter_of("Text.\n---\nk: v\n---\n"), None);
        // The opener may be the first body line after front matter.
        let text = "---\nspec: \"1\"\n---\n---\nk: v\n---\n";
        assert!(end_matter_of(text).is_some());
        assert_eq!(parse(text).unwrap().source, "");
        // The front matter's closing fence is never an opener: the search
        // starts at the first body line.
        let text = "---\nspec: \"1\"\n\n---\nk: v\n---\n";
        assert_eq!(end_matter_of(text), None);
        let d = parse(text).unwrap();
        assert!(d.end_matter.is_none() && d.source.contains("k: v"));
        // Only the NEAREST `---` before the close is considered: inside fenced
        // code it is no opener, and the thematic break before it is never
        // reached.
        assert_eq!(
            end_matter_of("Intro.\n\n---\na: b\n```\n\n---\nk: v\n```\n---\n"),
            None
        );
        // Not inside an unclosed note, an inert block or a verbatim body.
        assert_eq!(end_matter_of("Intro.\n<!--\n\n---\nk: v\n---\n"), None);
        assert_eq!(end_matter_of(":::aside\nx\n\n---\nk: v\n---\n"), None);
        assert_eq!(
            end_matter_of(":::note{verbatim}\nx\n\n---\nk: v\n---\n"),
            None
        );
        // A closed note before it is passed over.
        assert!(end_matter_of("<!-- n -->\n\n---\nk: v\n---\n").is_some());
        // The line after the opener begins with a mapping key.
        assert!(end_matter_of("Body.\n\n---\na.b: 1\n---\n").is_some());
        assert!(end_matter_of("Body.\n\n---\nkey : 1\n---\n").is_some());
        assert_eq!(end_matter_of("Body.\n\n---\n- x: 1\n---\n"), None);
        assert!(is_mapping_key_line("_k:") && !is_mapping_key_line("k:v"));
    }

    /// End matter ends a document, never a body: a trailing section ends
    /// before it, and a container whose body ends in `---`/`k: v`/`---` keeps
    /// those lines.
    #[test]
    fn end_matter_ends_the_document_and_never_a_body() {
        let d = parse("## !skill s\nDo it.\n\n---\nowner: ana\n---\n").unwrap();
        let b = d.blocks().next().unwrap();
        assert!(!b.body.contains("owner"), "{:?}", b.body);
        assert!(d.end_matter.is_some_and(|m| m.contains_key("owner")));
        let d = parse(":::when{env=\"x\"}\nText.\n\n---\nk: v\n---\n:::\n").unwrap();
        assert!(d.end_matter.is_none());
        assert!(d.blocks().next().unwrap().body.contains("k: v"));
    }

    /// Malformed end matter is refused at its first line (the opener's plus
    /// one), from `split_end_matter` and from `parse` alike, and still ends
    /// the body: nothing in it is read as a block.
    #[test]
    fn malformed_end_matter_is_refused_at_its_first_line() {
        let text = "Body.\n\n---\nk: [a\n:::!workflow\n---\n";
        let r = split_end_matter(text).unwrap_err();
        assert_eq!((r.line, r.code), (Some(4), "end-matter-yaml"));
        assert!(
            r.message.starts_with("end matter is not valid YAML: "),
            "{r}"
        );
        assert_eq!(parse(text).unwrap_err(), [r]);
        let r = end_matter_fields("- a", 7).unwrap_err();
        assert_eq!(
            (r.line, r.code, r.message.as_str()),
            (
                Some(7),
                "end-matter-yaml",
                "end matter is not a YAML mapping — write key: value lines"
            )
        );
    }

    /// Machinery registered after version 1 (`eval`, S23) is not reserved
    /// bare: `:::eval` is inert prose, while a version-1 name written bare
    /// is still refused. `:::!eval` names a rule by `@kind/name`, which must
    /// resolve, has a YAML body, configures nothing and acknowledges nothing.
    #[test]
    fn a_bare_eval_is_prose_and_a_sigiled_one_is_inert_machinery() {
        let d = parse(":::eval\nA bare eval is prose.\n:::\n").unwrap();
        assert_eq!(shapes(&d), ["inert eval"]);
        assert_eq!(
            parse(":::workflow{name=w}\nsteps: {}\n:::").unwrap_err()[0].code,
            "bare-machinery-kind"
        );
        // An inert leaf is a line of prose, as written; inside a body too.
        let d = parse("::eval{name=x}\n:::note\n::eval{name=y}\n:::\n").unwrap();
        assert_eq!(shapes(&d)[0], "text \"::eval{name=x}\"");
        assert_eq!(d.blocks().next().unwrap().body, "::eval{name=y}");
        // An inert block in a body is that body's text, fences dropped, read
        // raw: a keyword inside it is not lifted.
        let b = only_block(":::note\nA.\n:::aside\nMUST: raw\n:::\nB.\n:::\n");
        assert_eq!(
            (b.body.as_str(), b.children.len()),
            ("A.\nMUST: raw\nB.", 0)
        );

        let doc = "MUST[escalate]: escalate.\n\n:::!eval{name=e target=@must/escalate}\n\
                   cases:\n  - name: c\n    given: { message: hi }\n    expect: { contains: [x] }\n:::\n";
        let d = parse(doc).unwrap();
        let ex = fold(&d, &BTreeSet::new()).unwrap();
        assert!(ex.declarations.is_empty() && ex.config.is_empty(), "{ex:?}");
        assert!(!ex.cleaned.contains("eval"), "{}", ex.cleaned);
        // A name is required; the target is a reference, and it resolves.
        let e = parse(":::!eval{target=@must/m}\ncases: []\n:::\n").unwrap_err();
        assert!(e.iter().any(|r| r.code == "missing-attribute"), "{e:?}");
        let e =
            parse("MUST[m]: x\n\n:::!eval{name=e target=must/m}\ncases: []\n:::\n").unwrap_err();
        assert_eq!(
            (e[0].line, e[0].code, e[0].message.as_str()),
            (
                Some(3),
                "attribute-value",
                "target must be a reference written @kind/name"
            )
        );
        let e = parse(":::!eval{name=e target=@must/gone}\ncases: []\n:::\n").unwrap_err();
        assert_eq!(e[0].code, "dangling-reference");
        // Its body shape is informative (S22): YAML that is no mapping loads.
        let d = parse("MUST[m]: x\n\n:::!eval{name=e target=@must/m}\n- a\n:::\n").unwrap();
        assert!(fold(&d, &BTreeSet::new()).is_ok());
    }

    /// Every attribute the schema constrains by `enum` or `pattern` is held
    /// to it: on a block, on each row of a set, and on a front-matter
    /// `parameters[]` entry, which has no block line.
    #[test]
    fn attribute_values_are_held_to_the_schema() {
        let enum_msg = "type must be one of: string, number, boolean, enum, list, url, duration";
        let e = parse("::param{name=n type=integer}").unwrap_err();
        assert_eq!(
            (e[0].line, e[0].code, e[0].message.as_str()),
            (Some(1), "attribute-value", enum_msg)
        );
        // A flag where a value is required is no admissible value either.
        assert_eq!(
            parse("::param{name=n type}").unwrap_err()[0].code,
            "attribute-value"
        );
        // Each row of a set, at the set's line.
        let e = parse("\n:::param[]\n| name | type |\n|---|---|\n| a | string |\n| b | int |\n:::")
            .unwrap_err();
        assert_eq!(
            (e[0].line, e[0].code, e[0].message.as_str()),
            (Some(2), "attribute-value", enum_msg)
        );
        // Front matter: no line.
        let e = parse("---\nparameters:\n  - {name: n, source: env}\n---\nx\n").unwrap_err();
        assert_eq!((e[0].line, e[0].code), (None, "attribute-value"));
        assert!(e[0].message.starts_with("source must be one of:"), "{e:?}");
        // Admissible values load.
        assert!(
            parse("---\nparameters:\n  - {name: n, type: number}\n---\n::param{name=m source=prompt}\n")
                .is_ok()
        );
        assert!(
            parse("::!data{name=d format=table}\n")
                .is_err_and(|e| e.iter().all(|r| r.code != "attribute-value"))
        );
        // A pattern: the schema's reference grammar.
        assert!(
            parse(":::!data{name=reply}\na: 1\n:::\n:::output{schema=@data/reply}\nx\n:::\n")
                .is_ok()
        );
        assert!(is_reference("@a-b/c.d_e") && !is_reference("@1a/b") && !is_reference("a/b"));
    }

    /// Every pattern the registry carries is `x-grammar.attrRef`, which
    /// `is_reference` matches. A re-vendor that adds another pattern fails
    /// here rather than going unenforced.
    #[test]
    fn every_attribute_pattern_is_the_reference_grammar() {
        let schema: Value = serde_json::from_str(schema_json()).unwrap();
        let attr_ref = schema["x-grammar"]["attrRef"].as_str().unwrap();
        let patterns: Vec<_> = registry()
            .attr_rules
            .iter()
            .filter_map(|((k, a), r)| r.pattern.as_ref().map(|p| (k, a, p)))
            .collect();
        assert!(!patterns.is_empty());
        for (kind, attr, p) in patterns {
            assert_eq!(
                p, attr_ref,
                "{kind}.{attr} carries a pattern is_reference does not match"
            );
        }
    }

    /// `unless` (and `when`) need a condition: the schema's `minProperties`.
    /// `unless` and `otherwise` are structural containers, `otherwise` takes
    /// any attributes, and blocks nested in any variant stay its children.
    #[test]
    fn variant_containers_parse_and_need_a_condition() {
        for kind in ["unless", "when"] {
            let e = parse(&format!(":::{kind}\nx\n:::")).unwrap_err();
            assert_eq!(
                (e[0].line, e[0].code, e[0].message.clone()),
                (
                    Some(1),
                    "missing-attribute",
                    format!(
                        "{kind} requires at least one condition — write :::{kind}{{key=\"value\"}}"
                    )
                )
            );
        }
        let needs: Vec<&str> = registry().needs_attrs.iter().map(String::as_str).collect();
        assert_eq!(
            needs,
            ["unless", "when"],
            "the message speaks of conditions"
        );
        let d = parse(
            ":::unless{env=\"prod\"}\nA.\n:::\n:::otherwise{ignored=\"yes\"}\nB.\n\n::::when{tier=\"x\"}\nMUST: c.\n::::\n:::\n",
        )
        .unwrap();
        let blocks: Vec<&Block> = d.blocks().collect();
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            (blocks[0].kind.as_str(), blocks[0].disposition),
            ("unless", Disposition::Structural)
        );
        assert_eq!(blocks[1].kind, "otherwise");
        assert_eq!(blocks[1].children[0].kind, "when");
        assert_eq!(blocks[1].children[0].children[0].kind, "must");
    }

    /// Overrides (S24): never a guardrail, never a stronger rule; a weaker or
    /// equal rule may be overridden, and a target this document does not
    /// declare is not refused (it may be in an include).
    #[test]
    fn an_override_may_not_name_a_guardrail_or_a_stronger_rule() {
        let e =
            parse("GUARDRAIL[g]: no.\n\n:::must{overrides=\"guardrail/g\"}\nx\n:::").unwrap_err();
        assert_eq!(
            (e[0].line, e[0].code, e[0].message.as_str()),
            (
                Some(3),
                "override-guardrail",
                "guardrail/g is a guardrail — a guardrail is never overridden"
            )
        );
        let e = parse("MUST[m]: a\n\n:::should{overrides=\"must/m\"}\nb\n:::").unwrap_err();
        assert_eq!(
            (e[0].line, e[0].code, e[0].message.as_str()),
            (
                Some(3),
                "override-stronger",
                "a should may not override the stronger must/m"
            )
        );
        assert!(
            parse("SHOULD[s]: a\n\n:::must{overrides=\"should/s, must/elsewhere\"}\nb\n:::")
                .is_ok()
        );
        assert!(parse("MUST[m]: a\n\n:::must{overrides=\"must/m\"}\nb\n:::").is_ok());
    }

    #[test]
    fn reserved_bare_names_are_the_registrys_version_1_machinery() {
        let names: Vec<&str> = reserved_bare_names().collect();
        assert!(names.contains(&"workflow") && !names.contains(&"eval"));
        assert!(names.iter().all(|n| machinery_names().any(|m| m == *n)));
    }
}
