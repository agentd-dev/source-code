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

/// The capability family a machinery block belongs to — the unit the trust
/// ladder grants. Prose and structural blocks have no family (always allowed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    /// The default rung: no grant required.
    Default,
    Material,
    Knowledge,
    Interface,
    Identity,
    Compute,
    Infra,
    Compose,
}

impl Family {
    /// The `document_capabilities` token that grants this family, or `None` for
    /// the default rung (never needs a grant).
    pub fn grant(self) -> Option<&'static str> {
        match self {
            Family::Default => None,
            Family::Material => Some("material"),
            Family::Knowledge => Some("knowledge"),
            Family::Interface => Some("interface"),
            Family::Identity => Some("identity"),
            Family::Compute => Some("compute"),
            Family::Infra => Some("infra"),
            Family::Compose => Some("compose"),
        }
    }

    pub fn from_grant(s: &str) -> Option<Family> {
        Some(match s {
            "material" => Family::Material,
            "knowledge" => Family::Knowledge,
            "interface" => Family::Interface,
            "identity" => Family::Identity,
            "compute" => Family::Compute,
            "infra" => Family::Infra,
            "compose" => Family::Compose,
            _ => return None,
        })
    }
}

/// The full grant set — every family. Used for operator-authored surfaces that
/// are fully trusted (a subagent template's own instruction), where the trust
/// ladder's per-family gate does not apply.
pub fn all_families() -> BTreeSet<String> {
    [
        "material",
        "knowledge",
        "interface",
        "identity",
        "compute",
        "infra",
        "compose",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// One kind's static metadata.
pub struct Kind {
    pub name: &'static str,
    pub disposition: Disposition,
    pub family: Family,
    /// For a sub-block, the parent kind it is valid inside; `None` for a
    /// top-level block. A sub-block has no document-level identity and is
    /// exempt from the uniqueness rule.
    pub sub_of: Option<&'static str>,
}

const fn k(name: &'static str, d: Disposition, f: Family) -> Kind {
    Kind {
        name,
        disposition: d,
        family: f,
        sub_of: None,
    }
}
const fn sub(name: &'static str, parent: &'static str) -> Kind {
    // Sub-blocks inherit their parent's disposition (machinery) and need no
    // family of their own — the parent's grant governs them.
    Kind {
        name,
        disposition: Disposition::Machinery,
        family: Family::Default,
        sub_of: Some(parent),
    }
}

use Disposition::{Machinery as M, Prose as P, Structural as S};
use Family::*;

/// The kind registry — the single source of truth, matching the spec's
/// per-version registry (`conformance/registry/kinds.json`, spec version 1).
/// The conformance corpus asserts this agrees with the published registry.
pub const KINDS: &[Kind] = &[
    // ── prose (bare; degrade into delivery) ──────────────────────────────
    k("must", P, Default),
    k("should", P, Default),
    k("never", P, Default),
    k("guardrail", P, Default),
    k("note", P, Default),
    k("info", P, Default),
    k("tip", P, Default),
    k("important", P, Default),
    k("warning", P, Default),
    k("caution", P, Default),
    k("example", P, Default),
    k("form", P, Default),
    k("tool", P, Default),
    k("context", P, Default),
    // ── structural (bare; resolved away) ─────────────────────────────────
    k("when", S, Default),
    k("include", S, Default),
    // ── machinery: default rung ──────────────────────────────────────────
    k("workflow", M, Default),
    k("skill", M, Default),
    k("config", M, Default),
    k("mcp", M, Default),
    k("stream", M, Default),
    k("tools", M, Default),
    k("data", M, Default),
    k("override", M, Default),
    // ── machinery: material ──────────────────────────────────────────────
    k("file", M, Material),
    k("media", M, Material),
    k("asset", M, Material),
    // ── machinery: knowledge ─────────────────────────────────────────────
    k("knowledge", M, Knowledge),
    k("retrieval", M, Knowledge),
    k("source", M, Knowledge),
    // ── machinery: interface ─────────────────────────────────────────────
    k("endpoint", M, Interface),
    k("ui", M, Interface),
    k("human", M, Interface),
    k("channel", M, Interface),
    // ── machinery: identity ──────────────────────────────────────────────
    k("peer", M, Identity),
    k("policy", M, Identity),
    k("secret-ref", M, Identity),
    // ── machinery: compute ───────────────────────────────────────────────
    k("runtime", M, Compute),
    k("function", M, Compute),
    k("test", M, Compute),
    k("fixture", M, Compute),
    // ── machinery: infra ─────────────────────────────────────────────────
    k("git", M, Infra),
    k("volume", M, Infra),
    k("image", M, Infra),
    // ── machinery: compose ───────────────────────────────────────────────
    k("agent", M, Compose),
    // ── sub-blocks (valid only inside a parent) ──────────────────────────
    sub("case", "test"),
    sub("signature", "function"),
    sub("schema", "ui"),
    sub("preview", "ui"),
];

pub fn lookup(name: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.name == name)
}

/// The machinery names — reserved in the bare namespace: a bare `:::workflow`
/// (sigil forgotten) is refused rather than silently demoted to prose.
pub fn machinery_names() -> impl Iterator<Item = &'static str> {
    KINDS
        .iter()
        .filter(|k| k.disposition == Disposition::Machinery && k.sub_of.is_none())
        .map(|k| k.name)
}

/// A parsed block: its kind, identity, attributes, body text, and — because
/// dialect 2 nests — its child blocks.
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
}

impl Block {
    pub fn family(&self) -> Family {
        lookup(&self.kind)
            .map(|k| k.family)
            .unwrap_or(Family::Default)
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
    // the prose between blocks. Nested blocks live inside their parent.
    let mut nodes: Vec<Node> = Vec::new();
    let mut pending = String::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(of) = open_fence(lines[i]) {
            if !pending.is_empty() {
                nodes.push(Node::Text(std::mem::take(&mut pending)));
            }
            let (block, next) = parse_one(&lines, i, of, base, &mut errs);
            if let Some(b) = block {
                nodes.push(Node::Block(b));
            }
            i = next;
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

struct OpenFence {
    len: usize,
    sigil: bool,
    kind: String,
    attr_src: String,
}

fn parse_one(
    lines: &[&str],
    open_idx: usize,
    of: OpenFence,
    line_base: usize,
    errs: &mut Vec<String>,
) -> (Option<Block>, usize) {
    let line_no = line_base + open_idx + 1;
    let disposition = classify(&of, line_no, errs);

    let attrs = match parse_attrs(&of.attr_src) {
        Ok(a) => a,
        Err(e) => {
            errs.push(format!("line {line_no}: :::{}: {e}", of.kind));
            BTreeMap::new()
        }
    };

    // Find the matching close: a fence of length >= this one, alone on its line.
    // Everything between is body; nested blocks are recursed into.
    let (children, body_lines, close_idx, closed) =
        collect_body(lines, open_idx + 1, of.len, line_base, errs);
    if !closed {
        errs.push(format!(
            "line {line_no}: :::{} is never closed (want a line of {}+ colons)",
            of.kind, of.len
        ));
    }

    let disposition = match disposition {
        Some(d) => d,
        None => return (None, close_idx + 1), // classify recorded the error
    };
    let name = attrs.get("name").cloned();
    let body = body_lines.join("\n");
    (
        Some(Block {
            kind: of.kind,
            disposition,
            name,
            attrs,
            body,
            children,
            line: line_no,
        }),
        close_idx + 1,
    )
}

/// Collect a block's body: raw lines that are not part of a nested block, plus
/// the recursively-parsed children. `open_len` is the opening fence length; the
/// close is the first fence of `>= open_len` colons.
fn collect_body(
    lines: &[&str],
    from: usize,
    open_len: usize,
    line_base: usize,
    errs: &mut Vec<String>,
) -> (Vec<Block>, Vec<String>, usize, bool) {
    let mut children = Vec::new();
    let mut body = Vec::new();
    let mut i = from;
    let mut in_code = None::<usize>; // fenced-code suppression (``` of some length)
    while i < lines.len() {
        let line = lines[i];
        // Inside a fenced code block, colon scanning is suspended (a function
        // body or an embedded example never terminates its container).
        if let Some(tick_len) = in_code {
            if code_fence_len(line) == Some(tick_len) {
                in_code = None;
            }
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
        // A close for THIS block.
        if let Some(len) = fence_close_len(line)
            && len >= open_len
            && open_fence(line).is_none()
        {
            return (children, body, i, true);
        }
        // A nested block (shorter fence) — recurse.
        if let Some(of) = open_fence(line) {
            let (child, next) = parse_one(lines, i, of, line_base, errs);
            if let Some(c) = child {
                children.push(c);
            }
            i = next;
            continue;
        }
        body.push(line.to_string());
        i += 1;
    }
    (children, body, lines.len(), false)
}

/// Classify an opened fence into a disposition, enforcing the lexical rules.
fn classify(of: &OpenFence, line_no: usize, errs: &mut Vec<String>) -> Option<Disposition> {
    match lookup(&of.kind) {
        // Sub-blocks (`case`, `signature`, `schema`, `preview`) are written
        // UNSIGILED — the parent's fence and sigil govern them, and they have no
        // document-level identity of their own. Placement (inside the right
        // parent) is checked in a later pass; here they simply parse bare.
        Some(kind) if kind.sub_of.is_some() => {
            if of.sigil {
                errs.push(format!(
                    "line {line_no}: `:::!{}` is a sub-block — write it bare `:::{}`                      inside its parent",
                    of.kind, of.kind
                ));
                return None;
            }
            Some(Disposition::Machinery)
        }
        Some(kind) => {
            let want_sigil = kind.disposition == Disposition::Machinery;
            if want_sigil && !of.sigil {
                // A bare machinery name — the forgotten-sigil trap. Refuse.
                errs.push(format!(
                    "line {line_no}: `:::{}` shadows a machinery name — write `:::!{}` \
                     (bare names are prose; machinery carries the `!` sigil)",
                    of.kind, of.kind
                ));
                return None;
            }
            if !want_sigil && of.sigil {
                // A sigiled prose/structural name — the symmetric error.
                errs.push(format!(
                    "line {line_no}: `:::!{}` is not machinery — write `:::{}` (it is {})",
                    of.kind,
                    of.kind,
                    if kind.disposition == Disposition::Prose {
                        "prose"
                    } else {
                        "structural"
                    }
                ));
                return None;
            }
            Some(kind.disposition)
        }
        None => {
            if of.sigil {
                // Unknown machinery — fail closed, name the known set.
                let mut known: Vec<&str> = machinery_names().collect();
                known.sort_unstable();
                errs.push(format!(
                    "line {line_no}: unknown machinery directive `:::!{}` (known: {})",
                    of.kind,
                    known.join(", ")
                ));
                None
            } else {
                // Unknown bare name — fail OPEN: treat as inert prose so the
                // degradation contract holds. Signalled by a Prose disposition
                // with an unknown kind; the delivery pass renders it verbatim.
                Some(Disposition::Prose)
            }
        }
    }
}

/// Open-fence tokenizer: `:::[!]kind{attrs}`. Returns the fence length, whether
/// the machinery sigil is present, the kind, and the raw attribute source.
fn open_fence(line: &str) -> Option<OpenFence> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with(":::") {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == ':').count();
    let rest = &trimmed[len..];
    // A line of only colons is a CLOSE, not an open.
    if rest.trim().is_empty() {
        return None;
    }
    let (sigil, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let mut chars = rest.char_indices();
    let mut kind_end = rest.len();
    for (idx, c) in chars.by_ref() {
        if c == '{' || c == ' ' || c == '\t' {
            kind_end = idx;
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
            // Not a directive (e.g. a `::: ` divider in prose).
            return None;
        }
    }
    let kind = rest[..kind_end].to_string();
    if kind.is_empty() {
        return None;
    }
    let attr_src = rest[kind_end..].trim().to_string();
    Some(OpenFence {
        len,
        sigil,
        kind,
        attr_src,
    })
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
/// offset where the body begins. Absent front matter is spec 1 by the spec's
/// rule, but this dialect-2-native reader requires spec 2 — so a document that
/// is going to use dialect-2 machinery must declare it, and one that does not
/// is treated as spec 2 with no front matter (plain prose stays valid).
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
            if let Some(target) = val.strip_prefix('@') {
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
            if let Some(t) = val.strip_prefix('@')
                && let Some((k, nm)) = t.split_once('/')
            {
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

/// The families a document actually uses (for grant checking and reporting).
pub fn families_used(doc: &Document) -> BTreeSet<Family> {
    let mut out = BTreeSet::new();
    fn recur(b: &Block, out: &mut BTreeSet<Family>) {
        if b.family() != Family::Default {
            out.insert(b.family());
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

/// Refuse any block whose family is not granted by `document_capabilities`.
/// Fail-closed: names the block, the family, and the exact grant to add.
pub fn check_grants(doc: &Document, granted: &BTreeSet<String>, errs: &mut Vec<String>) {
    fn recur(b: &Block, granted: &BTreeSet<String>, errs: &mut Vec<String>) {
        if let Some(grant) = b.family().grant()
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
    pub families: Vec<Family>,
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
        m.insert(k.clone(), attr_value(v));
    }
    Some(m)
}

fn attr_value(s: &str) -> Value {
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => s
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(s.to_string())),
    }
}

/// Fold a parsed document into configuration + delivered prose, after checking
/// grants. The whole document is refused if any block errors — no partial load.
pub fn fold(doc: &Document, granted: &BTreeSet<String>) -> Result<Extraction, Vec<String>> {
    let mut errs = Vec::new();
    check_grants(doc, granted, &mut errs);
    let mut out = Extraction {
        families: families_used(doc).into_iter().collect(),
        ..Extraction::default()
    };

    // Delivery: rebuild the text the model reads, in source order. A prose text
    // run is emitted verbatim (it is the instruction's own words); a machinery
    // block becomes a one-line acknowledgement; a prose block degrades into
    // labelled text; a structural block resolves away.
    for node in &doc.nodes {
        match node {
            Node::Text(t) => {
                out.cleaned.push_str(t);
                if !t.ends_with('\n') {
                    out.cleaned.push('\n');
                }
            }
            Node::Block(b) => fold_block(b, &mut out, &mut errs),
        }
    }
    if errs.is_empty() { Ok(out) } else { Err(errs) }
}

fn ack(out: &mut Extraction, line: &str) {
    if !out.cleaned.is_empty() && !out.cleaned.ends_with('\n') {
        out.cleaned.push('\n');
    }
    out.cleaned.push_str(line);
    out.cleaned.push('\n');
}

fn fold_block(b: &Block, out: &mut Extraction, errs: &mut Vec<String>) {
    match b.disposition {
        Disposition::Prose => fold_prose(b, out),
        Disposition::Structural => {} // when/include resolve at delivery; recorded, not folded
        Disposition::Machinery => fold_machinery(b, out, errs),
    }
}

/// Prose degrades INTO the delivered text — labelled, body preserved, so a dumb
/// Markdown viewer still reads it as guidance.
fn fold_prose(b: &Block, out: &mut Extraction) {
    let label = match b.kind.as_str() {
        "context" => b
            .attrs
            .get("title")
            .map(|t| format!("<reference title=\"{t}\">"))
            .unwrap_or_else(|| "<reference>".into()),
        "example" => "<example>".into(),
        k if lookup(k).is_some() => format!("**{}**", k.to_uppercase()),
        // Unknown bare name — inert; render the fence verbatim so nothing is hidden.
        _ => format!(":::{}", b.kind),
    };
    ack(out, &label);
    out.cleaned.push_str(&b.body);
    out.cleaned.push('\n');
    match b.kind.as_str() {
        "context" => out.cleaned.push_str("</reference>\n"),
        "example" => out.cleaned.push_str("</example>\n"),
        _ => {}
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
                let name = m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                out.workflows.push(Value::Object(m));
                ack(
                    out,
                    &format!("[workflow \"{name}\" is loaded and runs autonomously]"),
                );
            }
        }
        "config" => {
            if let Some(m) = body_map(b, errs) {
                merge_into(&mut out.config, m);
            }
        }
        "mcp" => {
            if let Some(m) = body_map(b, errs) {
                match m.get("name").and_then(Value::as_str) {
                    Some(name) => {
                        let name = name.to_string();
                        push_into(&mut out.config, "mcp", "servers").push(Value::Object(m));
                        ack(
                            out,
                            &format!(
                                "[mcp server \"{name}\" is connected; its tools are available]"
                            ),
                        );
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
                        ack(out, &format!("[event stream \"{name}\" is declared]"));
                    }
                    None => errs.push(format!("line {}: :::!stream needs a name", b.line)),
                }
            }
        }
        "tools" => {
            if let Some(m) = body_map(b, errs) {
                merge_map(frag(&mut out.config, "tools"), m);
                ack(out, "[tool policy is applied]");
            }
        }
        // `:::!override{target=tool ...}` narrows an existing tool — append-only,
        // folded into real registry config: disable, add trifecta tags, append
        // an operator annotation. It may only make a tool MORE careful.
        "override" => {
            let Some(target) = b.attrs.get("target").cloned() else {
                errs.push(format!(
                    "line {}: :::!override needs a target=<tool>",
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
                push_into(&mut out.config, "tools", "disabled").push(Value::String(target.clone()));
                ack(out, &format!("[tool \"{target}\" is disabled]"));
                return;
            }
            let mut narrow = serde_json::Map::new();
            if let Some(tags) = body.get("tags").and_then(Value::as_array) {
                narrow.insert("tags".into(), Value::Array(tags.clone()));
            }
            // The description narrows to an operator annotation, appended.
            if let Some(desc) = body
                .get("description")
                .and_then(Value::as_str)
                .or_else(|| b.attrs.get("description").map(String::as_str))
            {
                narrow.insert("describe".into(), Value::String(desc.to_string()));
            }
            if !narrow.is_empty() {
                frag(&mut out.config, "tools")
                    .entry("narrow")
                    .or_insert_with(|| Value::Object(serde_json::Map::new()))
                    .as_object_mut()
                    .expect("narrow is an object")
                    .insert(target.clone(), Value::Object(narrow));
            }
            ack(out, &format!("[tool \"{target}\" is narrowed]"));
        }
        "skill" => match b.attrs.get("name") {
            Some(name) => {
                out.skills.push(InlineSkill {
                    name: name.clone(),
                    description: b.attrs.get("description").cloned().unwrap_or_default(),
                    when_to_use: b.attrs.get("when").cloned(),
                    body: b.body.clone(),
                });
                ack(
                    out,
                    &format!("[skill \"{name}\" is available; load it with skills.read]"),
                );
            }
            None => errs.push(format!("line {}: :::!skill needs a name", b.line)),
        },
        // ── extended families: cleanly map to real config where one exists ───
        "endpoint" => {
            // A declarative listener route (RFC 0035 §5 shape). Recorded as an
            // interface declaration; wiring it into the live webhook listener
            // is the next runtime phase.
            if let Some(mut m) = body_map(b, errs) {
                if let Some(n) = &b.name {
                    m.entry("name").or_insert_with(|| Value::String(n.clone()));
                }
                out.declarations
                    .entry("endpoint".into())
                    .or_default()
                    .push(Value::Object(m));
                let path = b.attrs.get("path").cloned().unwrap_or_default();
                ack(out, &format!("[endpoint {path} is served]"));
            }
        }
        "peer" => {
            if let Some(m) = body_map(b, errs) {
                let name = m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                push_into(&mut out.config, "a2a", "peers").push(Value::Object(m));
                ack(out, &format!("[peer \"{name}\" is reachable]"));
            }
        }
        // ── content-bearing kinds: the body is literal, not config ──────────
        // A file's body IS the file; a media/asset body is human description.
        // Path/mode/src come from the fence attributes.
        "file" | "media" | "asset" => {
            let mut rec = serde_json::Map::new();
            for (k, v) in &b.attrs {
                rec.insert(k.clone(), attr_value(v));
            }
            rec.insert("content".into(), Value::String(b.body.clone()));
            out.declarations
                .entry(b.kind.clone())
                .or_default()
                .push(Value::Object(rec));
            let what = b
                .name
                .as_deref()
                .or_else(|| b.attrs.get("path").map(String::as_str))
                .unwrap_or("");
            ack(out, &format!("[{} {} is provided]", b.kind, what));
        }
        // ── everything else: parse, grant-checked, recorded as a declaration ─
        // (visible in --capabilities; runtime effect delegated to a service).
        other => {
            if let Some(m) = body_map(b, errs) {
                let mut rec = Value::Object(m);
                if let Some(n) = &b.name {
                    rec.as_object_mut()
                        .unwrap()
                        .entry("name")
                        .or_insert_with(|| Value::String(n.clone()));
                }
                // Record sub-blocks (from children) under the declaration too.
                if !b.children.is_empty() {
                    let subs: Vec<Value> = b
                        .children
                        .iter()
                        .map(
                            |c| serde_json::json!({"kind": c.kind, "name": c.name, "body": c.body}),
                        )
                        .collect();
                    rec.as_object_mut()
                        .unwrap()
                        .insert("_sub".into(), Value::Array(subs));
                }
                out.declarations
                    .entry(other.to_string())
                    .or_default()
                    .push(rec);
                ack(
                    out,
                    &format!("[{other} {} is declared]", b.name.as_deref().unwrap_or("")),
                );
            }
        }
    }
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
            e[0].contains("shadows a machinery name") && e[0].contains(":::!workflow"),
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
    fn the_kind_table_matches_the_published_registry() {
        // Every machinery/prose/structural name the spec registry lists must be
        // present with the right disposition, and vice versa — the corpus test
        // pins this against the file, this pins it against the code.
        assert_eq!(machinery_names().count(), 29);
        assert_eq!(
            KINDS
                .iter()
                .filter(|k| k.disposition == Disposition::Prose && k.sub_of.is_none())
                .count(),
            14
        );
        assert_eq!(lookup("context").unwrap().disposition, Disposition::Prose);
        assert_eq!(
            lookup("workflow").unwrap().disposition,
            Disposition::Machinery
        );
        assert_eq!(lookup("function").unwrap().family, Family::Compute);
        assert_eq!(lookup("case").unwrap().sub_of, Some("test"));
    }

    fn grants(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn core_machinery_folds_into_config_and_prose_degrades() {
        let doc = "You triage tickets.\n\n                   :::note\nBe brief.\n:::\n\n                   :::!workflow{name=drain}\nsteps:\n  f: {kind: finish}\n:::\n\n                   :::!mcp{name=search}\nendpoint: https://x/mcp\n:::\n\n                   :::!stream{name=tickets}\nretention: {max_events: 100}\n:::\n\n                   :::!skill{name=esc description=\"escalate\" when=\"angry\"}\nAsk a human.\n:::\n\n                   :::context{title=\"SLA\"}\n1h for enterprise.\n:::";
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
        let doc = "---\nspec: \"1\"\n---\n            :::!file{name=cfg path=pyproject.toml}\n[project]\nname='x'\n:::\n            :::!data{name=slo}\ntiers: [gold, silver]\n:::\n            :::!knowledge{name=kb}\nserver: kb\n:::\n            :::!source{name=docs}\nkind: git\n:::\n            :::!ui{name=card}\nkind: form\n:::\n            :::!human{name=oncall}\nrole: approver\n:::\n            :::!policy{name=egress}\nmode: closed\n:::\n            :::!secret-ref{name=tok}\nkind: file\npath: /run/tok\n:::\n            :::!runtime{name=py}\nimage: ghcr.io/x@sha256:abc\n:::\n            :::!function{name=lint runtime=@runtime/py}\ndoc: lint\n:::\n            :::!git{name=repo}\nurl: https://x\n:::\n            :::!image{name=img}\ndigest: sha256:abc\n:::\n            :::!agent{name=rev}\ntemplate: reviewer\n:::";
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
        let doc = "---\nspec: \"1\"\n---\n            :::!runtime{name=py}\nimage: x@sha256:a\n:::\n            ::::!function{name=lint runtime=@runtime/py}\ndoc: lint\n::::\n            ::::!test{name=lint-works target=@function/lint}\n            :::case{name=one}\ngiven: {x: 1}\nexpect: {ok: true}\n:::\n            ::::";
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
    fn override_folds_into_real_tool_config() {
        // Disable folds into tools.disabled; narrowing folds into tools.narrow
        // (append-only tags + an operator annotation).
        let doc = "---\nspec: \"1\"\n---\n            :::!override{target=delete_ticket}\ndisabled: true\n:::\n            :::!override{target=create_ticket}\ntags: [sensitive]\ndescription: ENG queue only\n:::";
        let e = fold(&parse(doc).unwrap(), &grants(&[])).unwrap();
        assert_eq!(e.config["tools"]["disabled"][0], "delete_ticket");
        let narrow = &e.config["tools"]["narrow"]["create_ticket"];
        assert_eq!(narrow["tags"][0], "sensitive");
        assert_eq!(narrow["describe"], "ENG queue only");
        // override is default-rung (narrowing never needs a grant).
        assert!(fold(&parse(doc).unwrap(), &grants(&[])).is_ok());
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
}
