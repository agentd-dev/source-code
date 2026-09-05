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
    pub identity: bool,
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
}

/// The registry as loaded from the vendored JSON Schema — every kind, plus the
/// document-level tables (default-grant set, keyword→kind map, spec version).
pub struct Registry {
    kinds: BTreeMap<String, Kind>,
    grant_tokens: BTreeSet<String>,
    keywords: BTreeMap<String, String>,
    version: u32,
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
/// recognized, or its machinery is silently delivered as prose.
pub fn contains_blocks(text: &str) -> bool {
    text.split('\n').any(|line| {
        open_fence(line).is_some()
            || section_open(line).is_some()
            || leaf_open(line).is_some_and(|lf| {
                lf.sigil || lookup(&lf.kind).is_some_and(|k| k.disposition != Disposition::Prose)
            })
    })
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
                    family: d["x-family"].as_str().map(str::to_string),
                    grant,
                    sub_of: d["x-parent"].as_str().map(str::to_string),
                    ack: d["x-acknowledgement"].as_str().map(str::to_string),
                    noun: d["x-noun"].as_str().map(str::to_string),
                    nouns: d["x-nouns"].as_str().map(str::to_string),
                },
            );
        }
        Registry {
            kinds,
            grant_tokens,
            keywords,
            version,
        }
    }

    /// The spec version this registry describes (currently 1).
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The prose kind a keyword line introduces (`MUST` → `must`), if any.
    pub fn keyword_kind(&self, kw: &str) -> Option<&str> {
        self.keywords.get(kw).map(String::as_str)
    }
}

/// One kind's metadata by name.
pub fn lookup(name: &str) -> Option<&'static Kind> {
    registry().kinds.get(name)
}

/// The machinery names — reserved in the bare namespace: a bare `:::workflow`
/// (sigil forgotten) is refused rather than silently demoted to prose.
pub fn machinery_names() -> impl Iterator<Item = &'static str> {
    registry()
        .kinds
        .values()
        .filter(|k| k.disposition == Disposition::Machinery && k.sub_of.is_none())
        .map(|k| k.name.as_str())
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
}

impl Block {
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

/// A top-level document node, in source order: a run of prose text, or a block.
/// Delivery walks these so the words BETWEEN blocks are preserved.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Text(String),
    Block(Block),
}

/// A parsed document: front matter, and the top-level nodes (prose and blocks)
/// in source order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Document {
    pub front: BTreeMap<String, Value>,
    pub nodes: Vec<Node>,
    /// The whole source with front matter stripped.
    pub source: String,
}

impl Document {
    /// The top-level blocks, in order — a view over the block nodes.
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.nodes.iter().filter_map(|n| match n {
            Node::Block(b) => Some(b),
            Node::Text(_) => None,
        })
    }
}

/// Parse a document to its node tree, or return every problem found.
///
/// Fail-closed and specific: each error names the line and what to write
/// instead. Nothing is half-parsed — a document with any error yields no tree.
pub fn parse(text: &str) -> Result<Document, Vec<String>> {
    let mut errs = Vec::new();
    let (front, body_start) = parse_front_matter(text, &mut errs);
    let body = &text[body_start..];
    let lines: Vec<&str> = body.split('\n').collect();
    let base = body_start_line(text, body_start);

    // Top-level walk that interleaves text runs with blocks, so delivery keeps
    // the prose between blocks. Every form is recognized here (all at column 0):
    // a sigiled heading opens a section; a `:::` line opens a container or a
    // set; a `::` line is a leaf. Nested blocks live inside their parent.
    let mut nodes: Vec<Node> = Vec::new();
    let mut pending = String::new();
    let mut i = 0;
    macro_rules! flush {
        () => {
            if !pending.is_empty() {
                nodes.push(Node::Text(std::mem::take(&mut pending)));
            }
        };
    }
    while i < lines.len() {
        if let Some(sec) = section_open(lines[i]) {
            flush!();
            let (block, next) = parse_section(&lines, i, sec, base, &mut errs);
            if let Some(b) = block {
                nodes.push(Node::Block(b));
            }
            i = next;
        } else if let Some(of) = open_fence(lines[i]) {
            flush!();
            let (blocks, next) = parse_fence(&lines, i, of, base, &mut errs);
            for b in blocks {
                nodes.push(Node::Block(b));
            }
            i = next;
        } else if let Some(lf) = leaf_open(lines[i]) {
            flush!();
            if let Some(b) = parse_leaf(lf, base + i + 1, &mut errs) {
                nodes.push(Node::Block(b));
            }
            i += 1;
        } else {
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str(lines[i]);
            i += 1;
        }
    }
    if !pending.is_empty() {
        nodes.push(Node::Text(pending));
    }

    let blocks: Vec<&Block> = nodes
        .iter()
        .filter_map(|n| match n {
            Node::Block(b) => Some(b),
            Node::Text(_) => None,
        })
        .collect();
    check_identity(&blocks, &mut errs);
    check_refs(&blocks, &mut errs);
    check_placement(&blocks, &mut errs);
    check_inline_refs(&nodes, &mut errs);

    if errs.is_empty() {
        Ok(Document {
            front,
            nodes,
            source: body.to_string(),
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

/// Parse a `:::` block — a container (one block) or a set (many). Returns the
/// member blocks in source order and the index just past the closing fence.
fn parse_fence(
    lines: &[&str],
    open_idx: usize,
    of: OpenFence,
    line_base: usize,
    errs: &mut Vec<String>,
) -> (Vec<Block>, usize) {
    let line_no = line_base + open_idx + 1;
    let form = if of.is_set {
        Form::Set
    } else {
        Form::Container
    };
    let disposition = classify(&of.kind, of.sigil, form, line_no, errs);

    let attrs = attrs_or_empty(&of.attr_src, &of.kind, line_no, errs);
    // A `verbatim` body is quoted whole — nested fence syntax is content, not
    // structure (for tutorials that must show a fence).
    let verbatim = attrs.contains_key("verbatim");

    let (children, body_lines, close_idx, closed) =
        collect_body(lines, open_idx + 1, of.len, line_base, verbatim, errs);
    if !closed {
        errs.push(format!(
            "line {line_no}: :::{} is never closed (want a line of {}+ colons)",
            of.kind, of.len
        ));
    }

    let Some(disposition) = disposition else {
        return (Vec::new(), close_idx + 1); // classify recorded the error
    };

    if of.is_set {
        let members = parse_set_body(&of.kind, disposition, &attrs, &body_lines, line_no, errs);
        (members, close_idx + 1)
    } else {
        let name = attrs.get("name").cloned();
        let body = body_lines.join("\n");
        (
            vec![Block {
                kind: of.kind,
                disposition,
                name,
                attrs,
                body,
                children,
                line: line_no,
                set_group: None,
            }],
            close_idx + 1,
        )
    }
}

/// Parse a `::kind{attrs}` leaf — one instance, no body.
fn parse_leaf(lf: LeafTok, line_no: usize, errs: &mut Vec<String>) -> Option<Block> {
    let disposition = classify(&lf.kind, lf.sigil, Form::Leaf, line_no, errs)?;
    let attrs = attrs_or_empty(&lf.attr_src, &lf.kind, line_no, errs);
    let name = attrs.get("name").cloned();
    Some(Block {
        kind: lf.kind,
        disposition,
        name,
        attrs,
        body: String::new(),
        children: Vec::new(),
        line: line_no,
        set_group: None,
    })
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
    errs: &mut Vec<String>,
) -> (Option<Block>, usize) {
    let line_no = line_base + open_idx + 1;
    let end = section_extent(lines, open_idx + 1, sec.level);
    let disposition = classify(&sec.kind, true, Form::Section, line_no, errs);

    let mut attrs = attrs_or_empty(&sec.attr_src, &sec.kind, line_no, errs);
    attrs.entry("name".to_string()).or_insert(sec.name.clone());

    let Some(disposition) = disposition else {
        return (None, end);
    };

    let body_kind = lookup(&sec.kind)
        .map(|k| k.body)
        .unwrap_or(BodyKind::Markdown);
    if matches!(body_kind, BodyKind::Yaml | BodyKind::Code) {
        // A YAML/code section is heading + description + the single fenced code
        // block that is its definition. It ENDS at that fence — content after it
        // returns to the document top level, so a workflow section does not
        // swallow the blocks that follow it (the section-boundary trap).
        match find_code_fence(lines, open_idx + 1, end) {
            Some((fo, fc)) => {
                let (children, _) = collect_range(lines, open_idx + 1, fo, line_base, errs);
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
                        children,
                        line: line_no,
                        set_group: None,
                    }),
                    fc + 1,
                )
            }
            None => {
                errs.push(format!(
                    "line {line_no}: a `## !{}` section must contain exactly one fenced \
                     code block (its definition)",
                    sec.kind
                ));
                (None, end)
            }
        }
    } else {
        // A Markdown section is the whole section beneath the heading. Nested
        // fences/leaves are its children; the rest is its prose.
        let (children, prose_lines) = collect_range(lines, open_idx + 1, end, line_base, errs);
        (
            Some(Block {
                kind: sec.kind,
                disposition,
                name: Some(sec.name),
                attrs,
                body: prose_lines.join("\n"),
                children,
                line: line_no,
                set_group: None,
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
fn section_extent(lines: &[&str], from: usize, level: usize) -> usize {
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

/// Collect a container's body: raw lines that are not part of a nested block,
/// plus the recursively-parsed children. `open_len` is the opening fence
/// length; the close is the first fence of `>= open_len` colons. A `verbatim`
/// body is captured raw — no nested parsing.
fn collect_body(
    lines: &[&str],
    from: usize,
    open_len: usize,
    line_base: usize,
    verbatim: bool,
    errs: &mut Vec<String>,
) -> (Vec<Block>, Vec<String>, usize, bool) {
    let mut children = Vec::new();
    let mut body = Vec::new();
    let mut i = from;
    let mut in_code = None::<usize>;
    while i < lines.len() {
        let line = lines[i];
        if let Some(tick_len) = in_code {
            if code_fence_len(line) == Some(tick_len) {
                in_code = None;
            }
            body.push(line.to_string());
            i += 1;
            continue;
        }
        // The close for THIS block — checked before code/nesting so a verbatim
        // body still terminates.
        if let Some(len) = fence_close_len(line)
            && len >= open_len
            && open_fence(line).is_none()
        {
            return (children, body, i, true);
        }
        if verbatim {
            body.push(line.to_string());
            i += 1;
            continue;
        }
        if let Some(tl) = code_fence_len(line) {
            in_code = Some(tl);
            body.push(line.to_string());
            i += 1;
            continue;
        }
        // A nested container or set (shorter fence) — recurse.
        if let Some(of) = open_fence(line) {
            let (kids, next) = parse_fence(lines, i, of, line_base, errs);
            children.extend(kids);
            i = next;
            continue;
        }
        // A nested leaf.
        if let Some(lf) = leaf_open(line) {
            if let Some(c) = parse_leaf(lf, line_base + i + 1, errs) {
                children.push(c);
            }
            i += 1;
            continue;
        }
        body.push(line.to_string());
        i += 1;
    }
    (children, body, lines.len(), false)
}

/// Walk a bounded range `[from, end)` (a section body), splitting nested
/// fences/leaves out as children and returning the remaining prose lines.
fn collect_range(
    lines: &[&str],
    from: usize,
    end: usize,
    line_base: usize,
    errs: &mut Vec<String>,
) -> (Vec<Block>, Vec<String>) {
    let mut children = Vec::new();
    let mut prose = Vec::new();
    let mut i = from;
    let mut in_code = None::<usize>;
    while i < end {
        let line = lines[i];
        if let Some(tick_len) = in_code {
            if code_fence_len(line) == Some(tick_len) {
                in_code = None;
            }
            prose.push(line.to_string());
            i += 1;
            continue;
        }
        if let Some(tl) = code_fence_len(line) {
            in_code = Some(tl);
            prose.push(line.to_string());
            i += 1;
            continue;
        }
        if let Some(of) = open_fence(line) {
            let (kids, next) = parse_fence(lines, i, of, line_base, errs);
            children.extend(kids);
            i = next.min(end);
            continue;
        }
        if let Some(lf) = leaf_open(line) {
            if let Some(c) = parse_leaf(lf, line_base + i + 1, errs) {
                children.push(c);
            }
            i += 1;
            continue;
        }
        prose.push(line.to_string());
        i += 1;
    }
    (children, prose)
}

/// Classify a block by kind, sigil and form, enforcing the lexical rules
/// (§3.3): the reserved-bare and sigiled-prose guards, unknown-kind policy, and
/// per-kind form acceptance (§4). Returns the disposition, or `None` having
/// recorded a refusal.
fn classify(
    kind: &str,
    sigil: bool,
    form: Form,
    line_no: usize,
    errs: &mut Vec<String>,
) -> Option<Disposition> {
    match lookup(kind) {
        // Sub-blocks (`case`, `override`, `signature`, `schema`, `preview`) are
        // written UNSIGILED — the parent's fence and sigil govern them.
        Some(k) if k.sub_of.is_some() => {
            if sigil {
                errs.push(format!(
                    "line {line_no}: `{kind}` is a sub-block — write it bare (no `!`) \
                     inside its `!{}`",
                    k.sub_of.as_deref().unwrap_or("")
                ));
                return None;
            }
            if !k.forms.contains(&form) {
                errs.push(form_refusal(k, form, line_no));
                return None;
            }
            Some(Disposition::Machinery)
        }
        Some(k) => {
            let want_sigil = k.disposition == Disposition::Machinery;
            // The reserved-bare / sigiled-prose guards do not apply to the
            // section form: `## !kind` is always machinery by syntax, and a
            // bare heading never reaches here (section_open needs the `!`).
            if form != Form::Section {
                if want_sigil && !sigil {
                    errs.push(format!(
                        "line {line_no}: `{kind}` shadows a machinery name — write \
                         `{}` (bare names are prose; machinery carries the `!` sigil)",
                        form_spelling(kind, form, true)
                    ));
                    return None;
                }
                if !want_sigil && sigil {
                    errs.push(format!(
                        "line {line_no}: `!{kind}` is not machinery — write `{}` (it is {})",
                        form_spelling(kind, form, false),
                        if k.disposition == Disposition::Prose {
                            "prose"
                        } else {
                            "structural"
                        }
                    ));
                    return None;
                }
            } else if !want_sigil {
                errs.push(format!(
                    "line {line_no}: `## !{kind}` — the section form is machinery only, and \
                     `{kind}` is {}",
                    if k.disposition == Disposition::Prose {
                        "prose"
                    } else {
                        "structural"
                    }
                ));
                return None;
            }
            if !k.forms.contains(&form) {
                errs.push(form_refusal(k, form, line_no));
                return None;
            }
            Some(k.disposition)
        }
        None => {
            if sigil {
                let mut known: Vec<&str> = machinery_names().collect();
                known.sort_unstable();
                errs.push(format!(
                    "line {line_no}: unknown machinery directive `!{kind}` (known: {})",
                    known.join(", ")
                ));
                None
            } else {
                // Unknown bare name — fail OPEN: inert prose, delivered verbatim.
                Some(Disposition::Prose)
            }
        }
    }
}

/// How a kind is spelled in a given form, with (`sigiled`) or without the `!`
/// — so a refusal points at the exact fix: `:::!workflow`, `::!human`,
/// `:::!source[]`.
fn form_spelling(kind: &str, form: Form, sigiled: bool) -> String {
    let s = if sigiled { "!" } else { "" };
    match form {
        Form::Leaf => format!("::{s}{kind}"),
        Form::Set => format!(":::{s}{kind}[]"),
        _ => format!(":::{s}{kind}"),
    }
}

/// The refusal for a kind written in a form it does not accept, naming the
/// forms it does. A body-required message for the common leaf case.
fn form_refusal(k: &Kind, form: Form, line_no: usize) -> String {
    let sig = if k.disposition == Disposition::Machinery {
        "!"
    } else {
        ""
    };
    if form == Form::Leaf && k.body != BodyKind::None {
        return format!(
            "line {line_no}: `::{sig}{}` needs a body — use the container `:::{sig}{}`",
            k.name, k.name
        );
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
    format!(
        "line {line_no}: `{}` does not take {wrote} — its forms are: {}",
        k.name,
        names.join(", ")
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
    errs: &mut Vec<String>,
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
    errs: &mut Vec<String>,
) -> Vec<Block> {
    let rows: Vec<&String> = body_lines
        .iter()
        .filter(|l| l.trim_start().starts_with('|'))
        .collect();
    if rows.len() < 2 {
        errs.push(format!(
            "line {line_no}: `:::{kind}[]` table needs a header row and a separator"
        ));
        return Vec::new();
    }
    let header: Vec<String> = split_cells(rows[0])
        .into_iter()
        .map(|c| c.to_lowercase())
        .collect();
    let wants_name = lookup(kind).is_some_and(|k| k.identity);
    let mut out = Vec::new();
    // rows[0] is the header, rows[1] the separator; instances start at rows[2].
    for row in rows.iter().skip(2) {
        let cells = split_cells(row);
        let mut attrs = shared.clone();
        for (key, cell) in header.iter().zip(cells.iter()) {
            if !cell.is_empty() {
                attrs.insert(key.clone(), cell.clone());
            }
        }
        if wants_name && attrs.get("name").is_none_or(|n| n.is_empty()) {
            errs.push(format!(
                "line {line_no}: every row of `:::{kind}[]` needs a name"
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
    errs: &mut Vec<String>,
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
                errs.push(format!(
                    "line {line_no}: `:::{kind}[]` definition list expects a term, found {:?}",
                    line.trim()
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
    errs: &mut Vec<String>,
) -> BTreeMap<String, String> {
    match parse_attrs(src) {
        Ok(a) => a,
        Err(e) => {
            errs.push(format!("line {line_no}: {kind}: {e}"));
            BTreeMap::new()
        }
    }
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

/// `{key=value key2="quoted"}` → map. Bare `{flag}` → `flag=""`.
fn parse_attrs(src: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let src = src.trim();
    if src.is_empty() {
        return Ok(out);
    }
    let inner = src
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or("attributes must be wrapped in { }")?;
    let mut chars = inner.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
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
            return Err("empty attribute name".into());
        }
        // Bare flag.
        if chars.peek() != Some(&'=') {
            out.insert(key, String::new());
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
                    val.push(c);
                    chars.next();
                }
            }
        }
        out.insert(key, val);
    }
    Ok(out)
}

/// Front matter: a leading `---\n … \n---`. Returns the parsed map and the byte
/// offset where the body begins. A document with no front matter, or one whose
/// front matter lacks `spec`, is version 1 (spec rule `front-matter-absent`);
/// a document pinning a higher version is refused rather than mis-read.
fn parse_front_matter(text: &str, errs: &mut Vec<String>) -> (BTreeMap<String, Value>, usize) {
    let mut map = BTreeMap::new();
    let Some(rest) = text.strip_prefix("---\n") else {
        return (map, 0);
    };
    let Some(end) = rest.find("\n---") else {
        return (map, 0);
    };
    let block = &rest[..end];
    match crate::config::yaml::parse(block) {
        Ok(Value::Object(m)) => {
            for (kk, v) in m {
                map.insert(kk, v);
            }
        }
        Ok(_) => errs.push("front matter must be a YAML mapping".into()),
        Err(e) => errs.push(format!("front matter is not valid YAML: {e}")),
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
        if major > 1 {
            errs.push(format!(
                "front matter pins `spec: {s}`; this agentd implements spec \
                 version 1"
            ));
        }
    }
    // Advance past the closing `---` line.
    let after = end + "\n---".len();
    let abs = "---\n".len() + after;
    let nl = text[abs..]
        .find('\n')
        .map(|n| abs + n + 1)
        .unwrap_or(text.len());
    (map, nl)
}

/// Identity: `name` unique per kind (top-level only — sub-blocks are
/// parent-scoped and exempt).
fn check_identity(blocks: &[&Block], errs: &mut Vec<String>) {
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for b in blocks {
        if let Some(name) = &b.name
            && lookup(&b.kind).is_some_and(|k| k.sub_of.is_none())
            && !seen.insert((b.kind.clone(), name.clone()))
        {
            errs.push(format!(
                "line {}: duplicate {}/{} — `name` is unique per kind",
                b.line, b.kind, name
            ));
        }
    }
}

/// `@kind/name` references: every one must resolve to a declared block, and the
/// graph must be acyclic. References live in attribute values.
fn check_refs(blocks: &[&Block], errs: &mut Vec<String>) {
    let mut ids: BTreeSet<(String, String)> = BTreeSet::new();
    for b in blocks {
        if let Some(n) = &b.name {
            ids.insert((b.kind.clone(), n.clone()));
        }
    }
    for b in blocks {
        for (attr, val) in &b.attrs {
            // An attribute may be multi-valued (comma-separated), so each `@ref`
            // in it resolves independently: `may="@workflow/a, @workflow/b"`.
            for part in val.split(',') {
                let part = part.trim();
                let Some(target) = part.strip_prefix('@') else {
                    continue;
                };
                let (kind, name) = match target.split_once('/') {
                    Some((k, n)) => (k.to_string(), n.to_string()),
                    None => {
                        errs.push(format!(
                            "line {}: {attr}=@{target} must be qualified as @kind/name",
                            b.line
                        ));
                        continue;
                    }
                };
                if !ids.contains(&(kind.clone(), name.clone())) {
                    errs.push(format!(
                        "line {}: {attr}=@{kind}/{name} references no declared block",
                        b.line
                    ));
                }
            }
        }
    }
    // Acyclicity across attribute refs (a block that names itself, or a cycle).
    let mut edges: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
    for b in blocks {
        let Some(n) = &b.name else { continue };
        let from = (b.kind.clone(), n.clone());
        for val in b.attrs.values() {
            for part in val.split(',') {
                let Some((k, nm)) = part
                    .trim()
                    .strip_prefix('@')
                    .and_then(|t| t.split_once('/'))
                else {
                    continue;
                };
                edges
                    .entry(from.clone())
                    .or_default()
                    .push((k.to_string(), nm.to_string()));
            }
        }
    }
    let mut state: BTreeMap<(String, String), u8> = BTreeMap::new();
    for node in edges.keys() {
        if has_cycle(node, &edges, &mut state) {
            errs.push(format!("reference cycle through {}/{}", node.0, node.1));
            break;
        }
    }
}

/// Inline references in prose (§4.7): every `[[kind/name]]` and `[text](#kind/name)`
/// whose kind is a known kind must resolve to a declared block of that kind.
/// Dangling ones are refused — the class of bug a real check catches that
/// eyeballing does not. Refs inside fenced code are inert (code-suspends).
fn check_inline_refs(nodes: &[Node], errs: &mut Vec<String>) {
    let mut ids: BTreeSet<(String, String)> = BTreeSet::new();
    for n in nodes {
        if let Node::Block(b) = n
            && let Some(name) = &b.name
        {
            ids.insert((b.kind.clone(), name.clone()));
        }
    }
    let mut refs: Vec<(String, String)> = Vec::new();
    for n in nodes {
        match n {
            Node::Text(t) => collect_inline_refs(t, &mut refs),
            Node::Block(b) => collect_block_inline_refs(b, &mut refs),
        }
    }
    for (kind, name) in refs {
        // Only a KNOWN kind is a reference; `[[see/this]]` in prose is not.
        if lookup(&kind).is_some() && !ids.contains(&(kind.clone(), name.clone())) {
            errs.push(format!(
                "inline reference to {kind}/{name} resolves to no declared block"
            ));
        }
    }
}

/// Collect inline refs from a block's prose body (Markdown-bodied kinds only —
/// YAML/code/table bodies are not prose), recursing into children.
fn collect_block_inline_refs(b: &Block, refs: &mut Vec<(String, String)>) {
    let is_prose = b.disposition == Disposition::Prose
        || lookup(&b.kind).is_some_and(|k| k.body == BodyKind::Markdown);
    if is_prose {
        collect_inline_refs(&b.body, refs);
    }
    for c in &b.children {
        collect_block_inline_refs(c, refs);
    }
}

/// Scan text for `[[kind/name]]` and `](#kind/name)`, skipping fenced code.
fn collect_inline_refs(text: &str, refs: &mut Vec<(String, String)>) {
    let mut in_code = None::<usize>;
    for line in text.split('\n') {
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
        scan_line_refs(line, refs);
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

fn has_cycle(
    node: &(String, String),
    edges: &BTreeMap<(String, String), Vec<(String, String)>>,
    state: &mut BTreeMap<(String, String), u8>,
) -> bool {
    match state.get(node) {
        Some(1) => return true,  // on the current path
        Some(2) => return false, // done
        _ => {}
    }
    state.insert(node.clone(), 1);
    if let Some(next) = edges.get(node) {
        for n in next {
            if has_cycle(n, edges, state) {
                return true;
            }
        }
    }
    state.insert(node.clone(), 2);
    false
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
pub fn check_grants(doc: &Document, granted: &BTreeSet<String>, errs: &mut Vec<String>) {
    fn recur(b: &Block, granted: &BTreeSet<String>, errs: &mut Vec<String>) {
        if let Some(grant) = b.grant()
            && !granted.contains(grant)
        {
            errs.push(format!(
                "line {}: `:::!{}` needs the `{grant}` capability — add it to \
                 `agent.document_capabilities`",
                b.line, b.kind
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
fn check_placement(blocks: &[&Block], errs: &mut Vec<String>) {
    fn walk(b: &Block, parent_kind: Option<&str>, errs: &mut Vec<String>) {
        if let Some(want) = lookup(&b.kind).and_then(|k| k.sub_of.as_deref())
            && parent_kind != Some(want)
        {
            errs.push(format!(
                "line {}: `{}` is a sub-block of `!{want}` — it must sit inside a \
                 `!{want}` block",
                b.line, b.kind
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
pub fn extract(text: &str, granted: &BTreeSet<String>) -> Result<Extraction, Vec<String>> {
    let doc = parse(text)?;
    fold(&doc, granted)
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
fn body_map(b: &Block, errs: &mut Vec<String>) -> Option<serde_json::Map<String, Value>> {
    let mut m = if b.body.trim().is_empty() {
        serde_json::Map::new()
    } else {
        match crate::config::yaml::parse(&b.body) {
            Ok(Value::Object(m)) => m,
            Ok(_) => {
                errs.push(format!(
                    "line {}: :::!{} body must be a YAML mapping",
                    b.line, b.kind
                ));
                return None;
            }
            Err(e) => {
                errs.push(format!(
                    "line {}: :::!{} body is not valid YAML: {e}",
                    b.line, b.kind
                ));
                return None;
            }
        }
    };
    for (k, v) in &b.attrs {
        // The fence wins over a same-named body key.
        m.insert(k.clone(), attr_scalar(k, v));
    }
    // `@secret-ref/name` is the spec's reference to a declared `!secret-ref`
    // location (§3.4) — never a literal credential. Rewrite it to agentd's own
    // secret-reference form so the inline-credential guard is satisfied and the
    // value resolves at use, not read as a token.
    let mut root = Value::Object(m);
    resolve_secret_refs(&mut root);
    match root {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// Recursively rewrite `@secret-ref/NAME` string values to `{{secret:NAME}}`.
fn resolve_secret_refs(v: &mut Value) {
    match v {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix("@secret-ref/")
                && !name.is_empty()
            {
                *s = format!("{{{{secret:{name}}}}}");
            }
        }
        Value::Array(a) => a.iter_mut().for_each(resolve_secret_refs),
        Value::Object(m) => m.values_mut().for_each(resolve_secret_refs),
        _ => {}
    }
}

/// Attribute names the spec treats as multi-valued (comma-separated within one
/// value, §4.3.1). agentd's config types these as arrays, so a comma-string
/// attribute is split — `allow="read, run:tests"` becomes `["read", "run:tests"]`.
const MULTI_VALUED: &[&str] = &["allow", "deny", "methods", "scopes", "tags"];

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
            match crate::config::yaml::parse(s) {
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

/// An attribute's value, expanded to an array for the multi-valued names the
/// spec's encoding carries as comma-separated strings.
fn attr_scalar(key: &str, s: &str) -> Value {
    if MULTI_VALUED.contains(&key) {
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

/// Fold a parsed document into configuration + delivered prose, after checking
/// grants. The whole document is refused if any block errors — no partial load.
pub fn fold(doc: &Document, granted: &BTreeSet<String>) -> Result<Extraction, Vec<String>> {
    fold_with_params(doc, granted, &BTreeMap::new())
}

/// As [`fold`], with an explicit override map for `${parameter}` substitution
/// (a value here wins over a declared default). The delivered text runs the
/// §3.5 pipeline: prose degraded to its Appendix A form, machinery replaced by
/// its acknowledgement line (a set as one line), inline references degraded,
/// and `${}` substituted LAST so a value is never re-parsed as Markdown.
pub fn fold_with_params(
    doc: &Document,
    granted: &BTreeSet<String>,
    overrides: &BTreeMap<String, String>,
) -> Result<Extraction, Vec<String>> {
    let mut errs = Vec::new();
    check_grants(doc, granted, &mut errs);
    let mut out = Extraction {
        families: families_used(doc).into_iter().collect(),
        ..Extraction::default()
    };
    // The parameter context is needed during the walk to select `when`
    // variants, and again at the end for `${}` substitution.
    let params = param_values(doc, overrides);

    // Config + delivery in source order. Consecutive members of one set are
    // gathered so the set delivers a single acknowledgement line.
    let nodes = &doc.nodes;
    let mut i = 0;
    while i < nodes.len() {
        match &nodes[i] {
            Node::Text(t) => {
                out.cleaned.push_str(t);
                if !t.ends_with('\n') {
                    out.cleaned.push('\n');
                }
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
                deliver_set(&members, &mut out);
                i = j;
            }
            Node::Block(b) => {
                fold_config(b, &mut out, &mut errs);
                deliver_block(b, &mut out, &params);
                i += 1;
            }
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }

    // §3.5 steps 4–6, finishing passes over the assembled text: keyword and
    // alert forms normalize to their bold delivered form, inline references
    // degrade to their label, then `${}` is substituted last.
    out.cleaned = normalize_prose_lines(&out.cleaned);
    out.cleaned = degrade_inline_refs(&out.cleaned);
    out.cleaned = substitute_params(&out.cleaned, &params);
    Ok(out)
}

fn ack(out: &mut Extraction, line: &str) {
    if !out.cleaned.is_empty() && !out.cleaned.ends_with('\n') {
        out.cleaned.push('\n');
    }
    out.cleaned.push_str(line);
    out.cleaned.push('\n');
}

/// Fold one block into CONFIGURATION only (delivery is separate). Prose and
/// structural blocks contribute no configuration.
fn fold_config(b: &Block, out: &mut Extraction, errs: &mut Vec<String>) {
    if b.disposition == Disposition::Machinery {
        fold_machinery(b, out, errs);
    }
}

/// Deliver one block into the model-visible text (§3.5 steps 4–5).
fn deliver_block(b: &Block, out: &mut Extraction, params: &BTreeMap<String, String>) {
    match b.disposition {
        Disposition::Prose => deliver_prose(b, out),
        Disposition::Structural => {
            // A KEPT `when` delivers its body unwrapped; a dropped one delivers
            // nothing (and would be recorded in the manifest). `include`/`param`
            // deliver nothing (an include is inlined upstream of this reader).
            if b.kind == "when" && when_kept(b, params) && !b.body.trim().is_empty() {
                ack(out, b.body.trim_end());
            }
        }
        Disposition::Machinery => {
            if let Some(line) = machinery_ack(b) {
                ack(out, &line);
            }
        }
    }
}

/// Whether a `when` variant is kept for this reader: every `key="value"`
/// attribute must equal the reader's parameter of that name. A condition
/// referencing a parameter that is not in the context is not selected (its
/// content is withheld rather than shown speculatively). A `when` with no
/// simple attributes is always kept.
fn when_kept(b: &Block, params: &BTreeMap<String, String>) -> bool {
    b.attrs
        .iter()
        .all(|(k, v)| params.get(k).map(|pv| pv == v).unwrap_or(false))
}

/// A set of machinery delivers ONE line naming its members (§4.3 / Appendix A);
/// a set of a kind that delivers nothing (no `x-acknowledgement`) is silent.
fn deliver_set(members: &[&Block], out: &mut Extraction) {
    let first = members[0];
    let Some(k) = lookup(&first.kind) else { return };
    if k.ack.is_none() {
        return; // e.g. a `param[]` set, or a set of a non-delivering kind
    }
    let nouns = k
        .nouns
        .clone()
        .unwrap_or_else(|| format!("{}s", first.kind));
    let names: Vec<&str> = members.iter().filter_map(|m| m.name.as_deref()).collect();
    ack(
        out,
        &format!(
            "[{} {nouns} are declared: {}]",
            names.len(),
            names.join(", ")
        ),
    );
}

/// A machinery block's acknowledgement line from the schema's `x-acknowledgement`
/// template, or `None` for a kind that delivers nothing (§3.5 step 5).
fn machinery_ack(b: &Block) -> Option<String> {
    let tmpl = lookup(&b.kind)?.ack.as_deref()?;
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
/// becomes `**KEYWORD:** body`; `example`/`context`/`form`/`tool`/`glossary`
/// take their own shapes; an unknown bare block delivers its body as prose.
fn deliver_prose(b: &Block, out: &mut Extraction) {
    let title = b.attrs.get("title").cloned();
    match b.kind.as_str() {
        "context" => {
            let t = title.map(|t| format!(" title=\"{t}\"")).unwrap_or_default();
            ack(out, &format!("<reference{t}>"));
            out.cleaned.push_str(b.body.trim_end());
            out.cleaned.push_str("\n</reference>\n");
        }
        "example" => {
            let t = title.map(|t| format!(" — {t}")).unwrap_or_default();
            ack(out, &format!("**EXAMPLE{t}:**"));
            out.cleaned.push_str(b.body.trim_end());
            out.cleaned.push('\n');
        }
        "form" => {
            let t = title.map(|t| format!(" — {t}")).unwrap_or_default();
            ack(out, &format!("**Inputs to collect{t}**"));
            out.cleaned.push_str(b.body.trim_end());
            out.cleaned.push('\n');
        }
        "tool" => {
            let cap = b.attrs.get("cap").cloned().unwrap_or_default();
            // The label is explicit, else the block's name, else the last path
            // segment of the capability, title-cased (`server://…/ticketing`
            // → `Ticketing`).
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
            let head = label
                .map(|l| format!("**Tool — {l}** (`{cap}`)"))
                .unwrap_or_else(|| format!("**Tool** (`{cap}`)"));
            let allow = b.attrs.get("allow").cloned().unwrap_or_default();
            let deny = b.attrs.get("deny").cloned().unwrap_or_default();
            let mut line = head;
            if !allow.is_empty() {
                line.push_str(&format!(" — allowed: {allow}"));
            }
            if !deny.is_empty() {
                line.push_str(&format!("; denied: {deny}"));
            }
            ack(out, &line);
            out.cleaned.push_str(b.body.trim_end());
            out.cleaned.push('\n');
        }
        "glossary" => {
            for (term, def) in deflist_entries(&b.body) {
                ack(out, &format!("**{term}** — {def}"));
            }
        }
        k if NORMATIVITY.contains(&k) => {
            ack(out, &format!("**{}:** {}", k.to_uppercase(), b.body.trim()));
        }
        k if lookup(k).is_some() => {
            ack(out, &format!("**{}:** {}", k.to_uppercase(), b.body.trim()));
        }
        // Unknown bare name — inert: the body is delivered as prose, fences gone.
        _ => {
            ack(out, b.body.trim_end());
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

/// The normativity prose kinds, whose delivered form is `**KEYWORD:** body`.
const NORMATIVITY: &[&str] = &[
    "must",
    "should",
    "never",
    "guardrail",
    "note",
    "tip",
    "warning",
    "caution",
    "important",
];

/// Parse a definition-list body into `(term, definition)` pairs (for
/// `glossary` delivery).
fn deflist_entries(body: &str) -> Vec<(String, String)> {
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
fn param_values(doc: &Document, overrides: &BTreeMap<String, String>) -> BTreeMap<String, String> {
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
/// schema's keyword map. Fenced code is left untouched.
fn normalize_prose_lines(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_code = None::<usize>;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(tl) = in_code {
            if code_fence_len(line) == Some(tl) {
                in_code = None;
            }
            out.push(line.to_string());
            i += 1;
            continue;
        }
        if let Some(tl) = code_fence_len(line) {
            in_code = Some(tl);
            out.push(line.to_string());
            i += 1;
            continue;
        }
        // An alert: `> [!KIND]`, then the `>`-quoted body.
        if let Some(kw) = alert_open_kind(line) {
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim_start();
                if let Some(rest) = t.strip_prefix('>') {
                    body.push(rest.trim().to_string());
                    i += 1;
                } else {
                    break;
                }
            }
            out.push(format!("**{kw}:** {}", body.join(" ").trim()));
            continue;
        }
        if let Some(norm) = normalize_keyword_line(line) {
            out.push(norm);
        } else {
            out.push(line.to_string());
        }
        i += 1;
    }
    out.join("\n")
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
/// `NEVER`, `INFO` → `NOTE`), from the schema's keyword table.
fn canonical_keyword(kw: &str) -> Option<String> {
    registry().keyword_kind(kw).map(|kind| kind.to_uppercase())
}

/// An `override` sub-block (inside `!mcp`) narrows one of the server's tools —
/// append-only, folded into real registry config: disable, add trifecta tags,
/// append an operator annotation. It may only make a tool MORE careful, and it
/// delivers nothing (the spec gives it no acknowledgement).
fn fold_override(b: &Block, out: &mut Extraction, errs: &mut Vec<String>) {
    let Some(target) = b.attrs.get("target").cloned() else {
        errs.push(format!(
            "line {}: `override` needs a target=<tool> (the server tool to narrow)",
            b.line
        ));
        return;
    };
    let body = body_map(b, errs).unwrap_or_default();
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

fn fold_machinery(b: &Block, out: &mut Extraction, errs: &mut Vec<String>) {
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
                    None => errs.push(format!("line {}: :::!mcp needs a name", b.line)),
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
                    None => errs.push(format!("line {}: :::!stream needs a name", b.line)),
                }
            }
        }
        "tools" => {
            if let Some(m) = body_map(b, errs) {
                merge_map(frag(&mut out.config, "tools"), m);
            }
        }
        "skill" => match b.attrs.get("name") {
            Some(name) => {
                out.skills.push(InlineSkill {
                    name: name.clone(),
                    description: b.attrs.get("description").cloned().unwrap_or_default(),
                    when_to_use: b.attrs.get("when").cloned(),
                    body: b.body.clone(),
                });
            }
            None => errs.push(format!("line {}: :::!skill needs a name", b.line)),
        },
        // ── extended families: cleanly map to real config where one exists ───
        "endpoint" => {
            // A live listener route: folds into a real workflow with a single
            // `webhook` start node. `into:` makes it append to a stream (no
            // run); otherwise it fires the workflow. The listener address is
            // `webhooks.listen` (agent-level); this block declares the ROUTE.
            let Some(name) = b.name.clone().or_else(|| b.attrs.get("name").cloned()) else {
                errs.push(format!("line {}: :::!endpoint needs a name", b.line));
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
                rec.insert(k.clone(), attr_scalar(k, v));
            }
            match body_kind {
                BodyKind::Yaml => {
                    if !b.body.trim().is_empty() {
                        match crate::config::yaml::parse(&b.body) {
                            // The fence attributes win over same-named body keys.
                            Ok(Value::Object(m)) => {
                                for (k, v) in m {
                                    rec.entry(k).or_insert(v);
                                }
                            }
                            Ok(_) => {
                                errs.push(format!(
                                    "line {}: :::!{other} body must be a YAML mapping",
                                    b.line
                                ));
                                return;
                            }
                            Err(e) => {
                                errs.push(format!(
                                    "line {}: :::!{other} body is not valid YAML: {e}",
                                    b.line
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
                    if !b.body.is_empty() {
                        rec.insert("content".into(), Value::String(b.body.clone()));
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
fn table_rows(body: &str) -> Vec<Value> {
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
            e[0].contains("shadows a machinery name") && e[0].contains("!workflow"),
            "{e:?}"
        );
        // Sigiled prose — the symmetric error.
        let e = parse(":::!note\nhi\n:::").unwrap_err();
        assert!(
            e[0].contains("is not machinery") && e[0].contains(":::note"),
            "{e:?}"
        );
    }

    #[test]
    fn prose_is_bare_and_unknown_bare_is_inert() {
        let d = parse(":::note\nremember this\n:::").unwrap();
        assert_eq!(d.blocks().next().unwrap().disposition, Disposition::Prose);
        // Unknown bare name fails OPEN — inert prose, still parses.
        let d = parse(":::whatever\nfree text\n:::").unwrap();
        assert_eq!(d.blocks().next().unwrap().disposition, Disposition::Prose);
        // Unknown MACHINERY fails closed.
        let e = parse(":::!whatever\nx\n:::").unwrap_err();
        assert!(e[0].contains("unknown machinery"), "{e:?}");
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
        assert!(e[0].contains("duplicate workflow/dup"), "{e:?}");
        // Same name, DIFFERENT kinds is fine.
        assert!(parse(":::!workflow{name=x}\na: {}\n:::\n:::!stream{name=x}\nr: {}\n:::").is_ok());
    }

    #[test]
    fn refs_must_resolve_and_be_qualified_and_acyclic() {
        // Unresolvable.
        let e = parse(":::!function{name=f target=@runtime/missing}\nx\n:::").unwrap_err();
        assert!(e[0].contains("references no declared block"), "{e:?}");
        // Unqualified.
        let e = parse(":::!function{name=f target=@bare}\nx\n:::").unwrap_err();
        assert!(e.iter().any(|m| m.contains("must be qualified")), "{e:?}");
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
        assert_eq!(
            machinery_names().count(),
            28,
            "28 machinery kinds (override is a sub-block)"
        );
        assert_eq!(
            r.kinds
                .values()
                .filter(|k| k.disposition == Disposition::Prose && k.sub_of.is_none())
                .count(),
            15,
            "15 prose kinds (glossary added)"
        );
        assert_eq!(
            r.kinds
                .values()
                .filter(|k| k.disposition == Disposition::Structural)
                .count(),
            3,
            "3 structural kinds (param added)"
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
        assert!(none.iter().any(|m| m.contains("compute")), "{none:?}");
        assert!(none.iter().any(|m| m.contains("material")), "{none:?}");
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
            e[0].contains("sub-block of `!mcp`"),
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
            errs.iter().any(|e| e.contains("`compute` capability")),
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
            e[0].contains("needs a body") && e[0].contains(":::!workflow"),
            "{e:?}"
        );
        // A bare leaf shadowing machinery is the same reserved-bare trap.
        let e = parse("::human{name=x}").unwrap_err();
        assert!(e[0].contains("shadows a machinery name"), "{e:?}");
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
        // `@secret-ref/name` (§3.4) becomes agentd's `{{secret:name}}` form —
        // a reference resolved at use, never an inline token.
        let doc = "---\nspec: \"1\"\n---\n:::!mcp{name=t endpoint=https://x/mcp}\nauth: { kind: static, token: \"@secret-ref/tok\" }\n:::";
        let e = fold(&parse(doc).unwrap(), &grants(&[])).unwrap();
        assert_eq!(
            e.config["mcp"]["servers"][0]["auth"]["token"], "{{secret:tok}}",
            "the secret reference is rewritten, not left literal"
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
        assert!(e[0].contains("machinery only"), "{e:?}");
        // A bare heading is always just a heading — the guard never applies.
        assert!(parse("## workflow oncall\n\nfree prose").is_ok());
    }

    #[test]
    fn a_leaf_only_kind_refuses_the_container_form() {
        // `!image` is leaf-only (its body is none); a container is refused.
        let e =
            parse("---\nspec: \"1\"\n---\n:::!image{name=x}\ndigest: sha256:a\n:::").unwrap_err();
        assert!(e[0].contains("does not take the container form"), "{e:?}");
    }

    #[test]
    fn a_dangling_inline_reference_is_refused() {
        // A wiki-link to a declared human resolves; a ghost is refused.
        let ok = "::!human{name=oncall role=approver}\n\nAsk [[human/oncall]] first.";
        assert!(parse(ok).is_ok());
        let bad = "::!human{name=oncall role=approver}\n\nAsk [[human/ghost]] first.";
        let e = parse(bad).unwrap_err();
        assert!(e[0].contains("human/ghost"), "{e:?}");
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
}
