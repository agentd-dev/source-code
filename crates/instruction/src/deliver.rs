// SPDX-License-Identifier: MIT OR Apache-2.0
//! **Block-scoped delivery** (§3.5, Appendix A): every region of a document
//! rendered once, from the block that occupies it, into the bytes one reader
//! receives.
//!
//! The renderer walks a document's body line by line. A line no block
//! occupies is prose and is delivered as written, its inline references
//! degraded. A line a block occupies is replaced by that block's delivered
//! form, built from the block itself: a rule's label from the registry's
//! label tables (an alias delivers its canonical label, `SHOULD NOT` its
//! negated one, a condition renders, a name never does), its reason on its own
//! line, an example quoted, machinery acknowledged. A prose body is not text
//! to pass through but a fragment, walked again by the same rules — so a rule
//! inside an `output`, a `context` or a `when` is a rule, and a note inside it
//! is stripped. Nothing re-reads the delivered text afterwards: the old model
//! re-normalised the whole spliced text, which could not tell a keyword line
//! in an example (quoted material) from one in prose, and could not deliver
//! an included document in a label style of its own.
//!
//! Modelled on the reference implementation's `deliver.ts` (`renderLines`,
//! `proseBlockLines`, `deliverFragment`). Parameter substitution stays the
//! last step, over the whole text (`doc::fold_full`), so a value is never
//! re-read as Markdown.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::doc::{
    self, Block, BodyKind, Disposition, Document, Form, IncludeResolver, Node, registry,
};

/// How labelled prose is delivered (S25), from the front matter's
/// `delivery: {labels: bold|plain|tags}`. Each document chooses its own — an
/// included document is delivered in its style, not its includer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    /// `**MUST:** x` — the default.
    Bold,
    /// `MUST: x`.
    Plain,
    /// `<must>x</must>`.
    Tags,
}

impl Style {
    /// The style a document asks for; anything but `plain` or `tags` is the
    /// default, as the reference reads it.
    fn of(doc: &Document) -> Style {
        match doc
            .front
            .get("delivery")
            .and_then(|d| d.get("labels"))
            .and_then(Value::as_str)
        {
            Some("plain") => Style::Plain,
            Some("tags") => Style::Tags,
            _ => Style::Bold,
        }
    }
}

/// The state one delivery walk carries: what the reader is entitled to and
/// the include recursion (shared down the include tree), and what the
/// document being walked declares (its own label style, parameters and
/// facts). Private, so a later rule adds a field here instead of threading
/// another argument through every renderer.
pub(crate) struct Walk<'a> {
    pub(crate) granted: &'a BTreeSet<String>,
    /// The values `${name}` resolves to in this document: its declared
    /// defaults, with the caller's parameters winning.
    pub(crate) params: BTreeMap<String, String>,
    /// What `when` matches against: the parameters plus the runtime facts,
    /// the facts winning a collision (they are runtime-authoritative).
    pub(crate) facts: BTreeMap<String, String>,
    pub(crate) resolver: IncludeResolver<'a>,
    /// How many includes deep this document is (0 = the delivered document).
    pub(crate) depth: usize,
    /// The include targets on the path to this document, for cycles.
    pub(crate) seen: BTreeSet<String>,
    pub(crate) style: Style,
    /// The parameter declarations, which a `form` lists.
    pub(crate) decls: BTreeMap<String, BTreeMap<String, String>>,
}

impl<'a> Walk<'a> {
    /// The walk of a document delivered on its own (depth 0).
    pub(crate) fn new(
        doc: &Document,
        granted: &'a BTreeSet<String>,
        params: &BTreeMap<String, String>,
        facts: &BTreeMap<String, String>,
        resolver: IncludeResolver<'a>,
    ) -> Walk<'a> {
        let mut w = Walk {
            granted,
            params: BTreeMap::new(),
            facts: BTreeMap::new(),
            resolver,
            depth: 0,
            seen: BTreeSet::new(),
            style: Style::Bold,
            decls: BTreeMap::new(),
        };
        w.enter(doc, params, facts);
        w
    }

    /// The walk of a document included as `id` from this one: one level
    /// deeper, `id` on the path, delivered with its OWN declarations and
    /// style. It receives no caller parameters, and the includer's `when`
    /// facts as its facts.
    pub(crate) fn include(&self, id: &str, doc: &Document) -> Walk<'a> {
        let mut seen = self.seen.clone();
        seen.insert(id.to_string());
        let mut w = Walk {
            granted: self.granted,
            params: BTreeMap::new(),
            facts: BTreeMap::new(),
            resolver: self.resolver,
            depth: self.depth + 1,
            seen,
            style: Style::Bold,
            decls: BTreeMap::new(),
        };
        w.enter(doc, &BTreeMap::new(), &self.facts);
        w
    }

    /// Take on what `doc` declares.
    fn enter(
        &mut self,
        doc: &Document,
        params: &BTreeMap<String, String>,
        facts: &BTreeMap<String, String>,
    ) {
        self.style = Style::of(doc);
        self.params = doc::param_values(doc, params);
        self.decls = doc::param_decls(doc);
        self.facts = self.params.clone();
        for (k, v) in facts {
            self.facts.insert(k.clone(), v.clone());
        }
    }
}

/// A document's delivered lines (§3.5): its body walked once, then runs of
/// blank lines collapsed and the outer ones trimmed. Front matter and end
/// matter (S27) are not in the body the walk reads, so neither is ever
/// delivered. `${}` is not yet substituted.
pub(crate) fn document(doc: &Document, w: &mut Walk) -> Vec<String> {
    let lines: Vec<&str> = doc.source.split('\n').collect();
    finalize(render(&lines, &doc.nodes, w, true))
}

/// A skill's catalogue body, rendered as its prose would be delivered — in
/// the document's style, notes stripped, rules labelled and never named — so
/// the catalogue never shows a `MUST[name]:` the reader was never meant to
/// see. `lines` is the document body the skill's region indexes.
pub(crate) fn skill_body(b: &Block, lines: &[&str], w: &mut Walk) -> String {
    let src = if b.set_group.is_some() {
        // A set member's body is its entry's, which has no region of its own.
        b.body.split('\n').map(str::to_string).collect()
    } else {
        body_source(b, lines)
    };
    finalize(fragment(&src, w)).join("\n")
}

/// A prose body delivered by the same rules, as lines. A body is not a
/// document: it has no front or end matter, and it is not finalized — its
/// blank lines are the enclosing document's to collapse.
fn fragment(body: &[String], w: &mut Walk) -> Vec<String> {
    let lines: Vec<&str> = body.iter().map(String::as_str).collect();
    // The document parsed whole before delivery began, so reading a piece
    // of it again finds nothing to refuse that parse did not.
    let (nodes, _) = doc::walk_nodes(&lines, 0, &mut Vec::new());
    render(&lines, &nodes, w, false)
}

/// What occupies a run of lines.
enum Item<'n> {
    Block(&'n Block),
    /// The members of one set, which share the set's region.
    Set(Vec<&'n Block>),
    /// An inert block (§3.3 rule 2; S23): its body is prose, its fences go.
    Inert,
    /// An author note (S9): delivers nothing.
    Note,
}

/// Render `lines`, which `nodes` were walked from. `top` is a document's
/// own body, as opposed to a body inside one: only there is machinery
/// folded, so only there is it acknowledged. A machinery block nested in a
/// prose or variant body configures nothing — the fold reads the top level
/// — and an acknowledgement would tell the model a workflow is loaded that
/// is not; it delivers nothing, as it did before bodies were rendered.
fn render(lines: &[&str], nodes: &[Node], w: &mut Walk, top: bool) -> Vec<String> {
    let mut spans: Vec<(usize, usize, Item)> = Vec::new();
    let mut k = 0;
    while k < nodes.len() {
        match &nodes[k] {
            Node::Text(_) => {}
            Node::Note { start, end } => spans.push((*start, *end, Item::Note)),
            Node::Inert { region, .. } => spans.push((region.0, region.1, Item::Inert)),
            Node::Block(b) if b.set_group.is_some() => {
                let mut members = vec![b];
                while let Some(Node::Block(m)) = nodes.get(k + 1)
                    && m.set_group == b.set_group
                {
                    members.push(m);
                    k += 1;
                }
                spans.push((b.region.0, b.region.1, Item::Set(members)));
            }
            Node::Block(b) => spans.push((b.region.0, b.region.1, Item::Block(b))),
        }
        k += 1;
    }

    let mut out = Vec::new();
    let mut in_code = None::<usize>;
    let mut prose = |l: &str, out: &mut Vec<String>| {
        // Fenced code is delivered as written: nothing in it is a reference.
        if let Some(tl) = in_code {
            if doc::code_fence_len(l) == Some(tl) {
                in_code = None;
            }
            out.push(l.to_string());
        } else if let Some(tl) = doc::code_fence_len(l) {
            in_code = Some(tl);
            out.push(l.to_string());
        } else {
            out.push(degrade_inline(l));
        }
    };
    let mut li = 0;
    let mut si = 0;
    while li < lines.len() {
        while spans.get(si).is_some_and(|s| s.0 < li) {
            si += 1;
        }
        let Some((start, end, item)) = spans.get(si).filter(|s| s.0 == li) else {
            prose(lines[li], &mut out);
            li += 1;
            continue;
        };
        match item {
            Item::Note => {}
            Item::Inert => {
                // Its body, raw — nothing in it is lifted — with the fences
                // removed.
                for l in lines.get(start + 1..*end).unwrap_or_default() {
                    prose(l, &mut out);
                }
            }
            Item::Set(_) | Item::Block(_) if !top && machinery(item) => {}
            Item::Set(members) => out.extend(doc::deliver_set_lines(members)),
            Item::Block(b) => out.extend(block_lines(b, lines, w)),
        }
        li = end + 1;
        si += 1;
    }
    out
}

/// Whether a block, or a set's members, is machinery.
fn machinery(item: &Item) -> bool {
    match item {
        Item::Block(b) => b.disposition == Disposition::Machinery,
        Item::Set(members) => members[0].disposition == Disposition::Machinery,
        Item::Inert | Item::Note => false,
    }
}

/// The lines one block delivers in place of its region.
fn block_lines(b: &Block, lines: &[&str], w: &mut Walk) -> Vec<String> {
    match b.disposition {
        Disposition::Machinery => doc::machinery_ack(b).into_iter().collect(),
        Disposition::Structural => match b.kind.as_str() {
            "include" => doc::deliver_include(b, w),
            // A kept `when` delivers its body unwrapped, a dropped one
            // nothing.
            "when" if !doc::when_kept(b, &w.facts) => Vec::new(),
            // A structural kind with a body (a kept `when`, and `unless` and
            // `otherwise`, whose selection is not decided here) delivers it
            // unwrapped: a catch-all that delivered nothing would silently
            // drop the guidance inside — typically the safety fallback an
            // `otherwise` carries. Keyed on the schema's `x-body`, so a
            // structural kind a later registry adds keeps its text too.
            k => match doc::lookup(k).map(|k| k.body) {
                Some(BodyKind::None) => Vec::new(),
                _ => fragment(&body_source(b, lines), w),
            },
        },
        Disposition::Prose => prose_lines(b, lines, w),
    }
}

/// The source lines of a block's body, its reason excluded (TS
/// `regionBodyLines`): a container's between its fences, a section's under
/// its heading, an alert's quoted lines unquoted, a keyword's text after its
/// label and the lines of its paragraph. A leaf has none.
fn body_source(b: &Block, lines: &[&str]) -> Vec<String> {
    let (start, end) = b.region;
    // The region covers the reason that follows a rule; only blank lines
    // separate the two, so the block's own last line is the last non-blank
    // one before the reason.
    let own_end = match b.reason {
        Some((reason, _)) => {
            let mut j = reason;
            while j > start + 1 && lines[j - 1].trim().is_empty() {
                j -= 1;
            }
            j - 1
        }
        None => end,
    };
    let slice = |from: usize, to: usize| -> Vec<String> {
        lines
            .get(from..to)
            .unwrap_or_default()
            .iter()
            .map(|l| l.to_string())
            .collect()
    };
    match b.form {
        Form::Container | Form::Set => slice(start + 1, own_end),
        Form::Section => slice(start + 1, own_end + 1),
        Form::Leaf => Vec::new(),
        Form::Alert => slice(start + 1, own_end + 1)
            .into_iter()
            .map(|l| {
                let l = l.strip_prefix('>').unwrap_or(&l);
                l.strip_prefix([' ', '\t'])
                    .unwrap_or(l)
                    .trim_end()
                    .to_string()
            })
            .collect(),
        Form::Keyword => {
            let text = doc::keyword_line(lines[start]).map_or("", |k| k.text);
            let mut out = vec![text.to_string()];
            out.extend(slice(start + 1, own_end + 1));
            out
        }
    }
}

/// Whether an attribute is set as a flag (`{not}`, `{avoid}`).
fn flag(b: &Block, attr: &str) -> bool {
    b.attrs
        .get(attr)
        .is_some_and(|v| v.is_empty() || v == "true")
}

/// The label a registry entry delivers (`x-registry.labels`).
fn label_of(kind: &str) -> String {
    registry()
        .label(kind, false)
        .map_or_else(|| kind.to_uppercase(), str::to_string)
}

/// Prose degrades to its delivered form (Appendix A, S13-S17, S25).
fn prose_lines(b: &Block, lines: &[&str], w: &mut Walk) -> Vec<String> {
    let src = body_source(b, lines);
    let verbatim = b.attrs.contains_key("verbatim");
    // A `verbatim` body is quoted whole; any other body is a fragment.
    let body = |w: &mut Walk| {
        if verbatim {
            src.clone()
        } else {
            fragment(&src, w)
        }
    };
    let title = b.attrs.get("title");
    match b.kind.as_str() {
        // S16: quoted material — delivered as written, nothing in it
        // normalised, its author notes stripped.
        "example" => {
            let avoid = flag(b, "avoid");
            let mut suffix = String::new();
            let mut attrs = String::new();
            if avoid {
                suffix.push_str(" (avoid)");
            }
            if let Some(t) = title {
                suffix.push_str(&format!(" — {t}"));
                attrs.push_str(&format!(" title=\"{}\"", xml_attr(t)));
            }
            if avoid {
                attrs.push_str(" avoid");
            }
            let head = Head {
                label: label_of("example"),
                suffix,
                attrs,
            };
            wrap(w.style, &head, note_free(b, src), "", true)
        }
        // S17: the body is prose (a rule in it is a rule); the schema is
        // the host's and never delivered.
        "output" => {
            let mut suffix = String::new();
            let mut attrs = String::new();
            if let Some(f) = b.attrs.get("format") {
                suffix.push_str(&format!(" ({f})"));
                attrs.push_str(&format!(" format=\"{}\"", xml_attr(f)));
            }
            if let Some(t) = title {
                suffix.push_str(&format!(" — {t}"));
                attrs.push_str(&format!(" title=\"{}\"", xml_attr(t)));
            }
            let head = Head {
                label: label_of("output"),
                suffix,
                attrs,
            };
            let body = body(w);
            wrap(w.style, &head, body, "", true)
        }
        // context, form, tool and glossary deliver the same in every style.
        "context" => {
            let t = title.map(|t| format!(" title=\"{t}\"")).unwrap_or_default();
            let mut out = vec![format!("<reference{t}>")];
            out.extend(body(w));
            out.push("</reference>".into());
            out
        }
        "form" => doc::form_lines(b, &w.decls),
        "tool" => {
            let mut out = vec![doc::tool_head(b)];
            out.extend(body(w));
            out
        }
        "glossary" => doc::deflist_entries(&b.body)
            .into_iter()
            .map(|(term, def)| format!("**{term}** — {}", degrade_inline(&def)))
            .collect(),
        k if registry().label(k, false).is_some() => {
            // A rule, or an admonition: its label (the negated one for
            // `SHOULD NOT`, the canonical one for an alias — S8), its
            // condition (S15), never its name (S12).
            let label = registry().label(k, flag(b, "not")).unwrap_or(k).to_string();
            let (suffix, attrs) = match b.attrs.get("if") {
                Some(c) => (format!(" (if {c})"), format!(" if=\"{}\"", xml_attr(c))),
                None => (String::new(), String::new()),
            };
            // A keyword keeps the list marker it was written with.
            let lead = if b.form == Form::Keyword {
                &lines[b.region.0][..doc::list_marker_len(lines[b.region.0])]
            } else {
                ""
            };
            let head = Head {
                label,
                suffix,
                attrs,
            };
            let body = body(w);
            let mut out = wrap(w.style, &head, body, lead, false);
            // S14: the reason, on its own line after the rule's text — the
            // blank lines that separated a `BECAUSE:` paragraph go. It keeps
            // the indentation and list marker its line was written with; a
            // container's `because=` has none.
            if let Some(because) = b.attrs.get("because") {
                let lead = b.reason.map_or("", |(r, _)| reason_lead(lines[r]));
                let head = Head {
                    label: label_of("because"),
                    suffix: String::new(),
                    attrs: String::new(),
                };
                let text: Vec<String> = degrade_inline(because)
                    .split('\n')
                    .map(str::to_string)
                    .collect();
                out.extend(wrap(w.style, &head, text, lead, false));
            }
            out
        }
        // A prose kind the registry gives no delivery shape delivers its
        // body as written.
        _ => src,
    }
}

/// The indentation and list marker a reason's line opens with.
fn reason_lead(line: &str) -> &str {
    let t = line.trim_start_matches([' ', '\t']);
    let indent = line.len() - t.len();
    &line[..indent + doc::list_marker_len(t)]
}

/// An example's body lines without the author notes the parser found in it.
/// A keyword `EXAMPLE:` has one paragraph and no notes.
fn note_free(b: &Block, src: Vec<String>) -> Vec<String> {
    if b.form != Form::Container {
        return src;
    }
    // The notes index the walked lines; the body starts under the opener.
    let from = b.region.0 + 1;
    src.into_iter()
        .enumerate()
        .filter(|(k, _)| {
            let at = from + k;
            !b.notes.iter().any(|&(s, e)| s <= at && at <= e)
        })
        .map(|(_, l)| l)
        .collect()
}

/// A delivered label: its text (`MUST`), what follows it inside the label
/// (` (if c)`, ` (avoid) — T`), and the attributes its tag carries.
struct Head {
    label: String,
    suffix: String,
    attrs: String,
}

/// Wrap delivered body lines under a label, in a style (TS `wrapLabelled`).
/// `lead` is the indentation and list marker kept from the source. A label
/// `own_line` (example, output) stands above its body; any other opens its
/// first line.
fn wrap(style: Style, head: &Head, body: Vec<String>, lead: &str, own_line: bool) -> Vec<String> {
    if style == Style::Tags {
        let tag = head.label.to_lowercase().replace(' ', "-");
        let open = format!("<{tag}{}>", head.attrs);
        let close = format!("</{tag}>");
        if own_line {
            let mut out = vec![format!("{lead}{open}")];
            out.extend(body);
            out.push(close);
            return out;
        }
        // One line holds both tags; more open on the first and close on the
        // last.
        return match body.as_slice() {
            [] => vec![format!("{lead}{open}{close}")],
            [only] => vec![format!("{lead}{open}{only}{close}")],
            [first, middle @ .., last] => {
                let mut out = vec![format!("{lead}{open}{first}")];
                out.extend(middle.iter().cloned());
                out.push(format!("{last}{close}"));
                out
            }
        };
    }
    let text = format!("{}{}:", head.label, head.suffix);
    let labelled = if style == Style::Bold {
        format!("**{text}**")
    } else {
        text
    };
    let mut body = body.into_iter();
    if own_line {
        let mut out = vec![format!("{lead}{labelled}")];
        out.extend(body);
        return out;
    }
    let first = format!("{lead}{labelled} {}", body.next().unwrap_or_default());
    // The reference drops the one space an empty first line leaves.
    let first = first
        .strip_suffix(' ')
        .map_or(first.clone(), str::to_string);
    let mut out = vec![first];
    out.extend(body);
    out
}

/// An attribute value inside a tag: `&` and `"` escaped.
fn xml_attr(v: &str) -> String {
    v.replace('&', "&amp;").replace('"', "&quot;")
}

/// Runs of blank lines collapse to one, a blank line is empty, and the outer
/// blank lines go (§3.5 layout).
fn finalize(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in lines {
        let blank = l.trim().is_empty();
        if blank && out.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        out.push(if blank { String::new() } else { l });
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

/// Inline degradation of a prose line (Appendix A): `[[kind/name]]` and
/// `[[kind/name|Label]]` to the label (or the name), and `[Label](#kind/name)`
/// to `Label` when `kind` is a registered kind. Inert inside a code span, as
/// the reference reads one: a backtick to the next backtick.
pub(crate) fn degrade_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let Some(close) = rest[open + 1..].find('`').map(|c| open + 1 + c) else {
            break;
        };
        out.push_str(&degrade_refs(&rest[..open]));
        out.push_str(&rest[open..=close]);
        rest = &rest[close + 1..];
    }
    out.push_str(&degrade_refs(rest));
    out
}

/// `x-grammar.wikiLink` then the fragment-link form, each matched leftmost
/// and replaced, as the reference's two global replacements do.
fn degrade_refs(text: &str) -> String {
    let wiki = replace_all(text, |s| {
        let inner = s.strip_prefix("[[")?;
        let (_, after) = take_while_kind(inner)?;
        let after = after.strip_prefix('/')?;
        let (name, after) = take_name(after)?;
        let (label, after) = match after.strip_prefix('|') {
            Some(l) => {
                let end = l.find(']').filter(|&e| e > 0)?;
                (Some(&l[..end]), &l[end..])
            }
            None => (None, after),
        };
        let after = after.strip_prefix("]]")?;
        Some((label.unwrap_or(name).to_string(), s.len() - after.len()))
    });
    replace_all(&wiki, |s| {
        let inner = s.strip_prefix('[')?;
        let end = inner.find(']').filter(|&e| e > 0)?;
        let label = &inner[..end];
        let after = inner[end..].strip_prefix("](#")?;
        let (kind, after) = take_while_kind(after)?;
        let after = after.strip_prefix('/')?;
        let (_, after) = take_name(after)?;
        let after = after.strip_prefix(')')?;
        doc::lookup(kind)?;
        Some((label.to_string(), s.len() - after.len()))
    })
}

/// Replace every leftmost match `m` finds: at each position, `m` returns the
/// replacement and the length it consumed, or `None` to move one character
/// on.
fn replace_all(text: &str, m: impl Fn(&str) -> Option<(String, usize)>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some((with, len)) = m(&text[i..]) {
            out.push_str(&with);
            i += len;
            continue;
        }
        let c = text[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// `[A-Za-z][A-Za-z0-9_-]*` at the start of `s`: the token and the rest.
fn take_while_kind(s: &str) -> Option<(&str, &str)> {
    let end = s
        .char_indices()
        .find(|&(i, c)| {
            !(if i == 0 {
                c.is_ascii_alphabetic()
            } else {
                c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
            })
        })
        .map_or(s.len(), |(i, _)| i);
    (end > 0).then(|| s.split_at(end))
}

/// `[A-Za-z0-9][A-Za-z0-9._-]*` at the start of `s`: the name and the rest.
fn take_name(s: &str) -> Option<(&str, &str)> {
    let end = s
        .char_indices()
        .find(|&(i, c)| {
            !(if i == 0 {
                c.is_ascii_alphanumeric()
            } else {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
            })
        })
        .map_or(s.len(), |(i, _)| i);
    (end > 0).then(|| s.split_at(end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{all_families, fold, fold_full, parse};

    /// The delivered text of a document read whole, every family granted.
    fn delivered(text: &str) -> String {
        fold(&parse(text).unwrap(), &all_families())
            .unwrap()
            .cleaned
    }

    /// The delivered text with one include resolvable.
    fn delivered_with(text: &str, id: &str, included: &str) -> String {
        let resolve = |want: &str| (want == id).then(|| included.to_string());
        let doc = parse(text).unwrap();
        fold_full(
            &doc,
            &all_families(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &resolve,
        )
        .unwrap()
        .cleaned
    }

    const RULES: &str = "MUST[plan] (if a refund is requested): confirm the plan.\n\
                         BECAUSE: limits differ.\n\nMAY: skip the greeting\nfor internal users.\n\n\
                         :::example{title=\"A \\\"good\\\" one\" avoid}\nDone.\n:::\n\n\
                         :::output{format=json title=\"R&D\"}\nOne object.\n:::\n";

    /// S25: every label style renders the same blocks — a condition, a
    /// reason, a multi-line body, an example and an output — and none of
    /// them delivers the rule's name (S12).
    #[test]
    fn each_label_style_renders_every_label() {
        let bold = delivered(RULES);
        assert_eq!(
            bold,
            "**MUST (if a refund is requested):** confirm the plan.\n**BECAUSE:** limits differ.\n\n\
             **MAY:** skip the greeting\nfor internal users.\n\n\
             **EXAMPLE (avoid) — A \"good\" one:**\nDone.\n\n**OUTPUT (json) — R&D:**\nOne object.\n"
        );
        let plain = delivered(&format!("---\ndelivery: {{labels: plain}}\n---\n{RULES}"));
        assert_eq!(
            plain,
            "MUST (if a refund is requested): confirm the plan.\nBECAUSE: limits differ.\n\n\
             MAY: skip the greeting\nfor internal users.\n\n\
             EXAMPLE (avoid) — A \"good\" one:\nDone.\n\nOUTPUT (json) — R&D:\nOne object.\n"
        );
        // Tags: a one-line body holds both tags, a multi-line one opens on
        // its first line and closes on its last, an example and an output
        // stand their tags on lines of their own, and attribute values are
        // escaped.
        let tags = delivered(&format!("---\ndelivery: {{labels: tags}}\n---\n{RULES}"));
        assert_eq!(
            tags,
            "<must if=\"a refund is requested\">confirm the plan.</must>\n<because>limits differ.</because>\n\n\
             <may>skip the greeting\nfor internal users.</may>\n\n\
             <example title=\"A &quot;good&quot; one\" avoid>\nDone.\n</example>\n\n\
             <output format=\"json\" title=\"R&amp;D\">\nOne object.\n</output>\n"
        );
        for out in [&bold, &plain, &tags] {
            assert!(
                !out.contains("plan]"),
                "the name is never delivered:\n{out}"
            );
        }
        // A tag is the label lower-cased, its spaces hyphens.
        assert_eq!(
            delivered("---\ndelivery: {labels: tags}\n---\nSHOULD NOT: guess.\n"),
            "<should-not>guess.</should-not>\n"
        );
        // context and tool are the same in every style.
        let same = ":::context{title=\"Policy\"}\nThirty days.\n:::\n";
        assert_eq!(
            delivered(&format!("---\ndelivery: {{labels: tags}}\n---\n{same}")),
            delivered(same)
        );
    }

    /// S25: an included document is delivered in its OWN style, whatever
    /// its includer's.
    #[test]
    fn an_include_is_delivered_in_its_own_style() {
        let tagged = "---\ndelivery: {labels: tags}\n---\nMUST: be warm.\n";
        let bold = "MUST: be brief.\n";
        assert_eq!(
            delivered_with(
                "MUST: answer.\n\n::include{id=\"house\"}\n",
                "house",
                tagged
            ),
            "**MUST:** answer.\n\n<must>be warm.</must>\n"
        );
        assert_eq!(
            delivered_with(
                "---\ndelivery: {labels: plain}\n---\nMUST: answer.\n\n::include{id=\"house\"}\n",
                "house",
                bold
            ),
            "MUST: answer.\n\n**MUST:** be brief.\n"
        );
        // An include's end matter (S27) never arrives.
        assert_eq!(
            delivered_with(
                "::include{id=\"house\"}\n",
                "house",
                "MUST: be brief.\n\n---\nowners: [ana]\n---\n"
            ),
            "**MUST:** be brief.\n"
        );
    }

    /// S14: a reason keeps the indentation and list marker its line was
    /// written with; the blank lines between a rule and its reason go.
    #[test]
    fn a_reason_keeps_its_lead_and_its_blank_lines_go() {
        assert_eq!(
            delivered(
                "- MUST: link the ticket.\n  BECAUSE: audits follow links.\n- SHOULD: reply.\n"
            ),
            "- **MUST:** link the ticket.\n  **BECAUSE:** audits follow links.\n- **SHOULD:** reply.\n"
        );
        assert_eq!(
            delivered("1. MUST: link it.\n2. BECAUSE: audits.\n"),
            "1. **MUST:** link it.\n2. **BECAUSE:** audits.\n"
        );
        assert_eq!(
            delivered("NEVER: guess.\n\nBECAUSE: a wrong answer costs more.\n"),
            "**NEVER:** guess.\n**BECAUSE:** a wrong answer costs more.\n"
        );
        // A container's `because=` follows its body, with no lead.
        assert_eq!(
            delivered(":::never{because=\"finance decides\"}\nPromise a refund.\n:::\n"),
            "**NEVER:** Promise a refund.\n**BECAUSE:** finance decides\n"
        );
    }

    /// S16: an example is quoted material — a keyword line in it is not a
    /// rule, a reference in it is not degraded — and its author notes are
    /// stripped.
    #[test]
    fn an_example_body_is_quoted_and_note_free() {
        assert_eq!(
            delivered(
                ":::example\nMUST: this is what the customer wrote.\n<!-- the reviewer's aside -->\n\
                 See [[must/x]].\n:::\n\nMUST[x]: y.\n"
            ),
            "**EXAMPLE:**\nMUST: this is what the customer wrote.\nSee [[must/x]].\n\n**MUST:** y.\n"
        );
    }

    /// Notes (S9) are stripped from prose and from every prose body; a
    /// `verbatim` body is quoted whole and keeps `<!--` as written.
    #[test]
    fn notes_are_stripped_and_verbatim_keeps_them() {
        assert_eq!(
            delivered(
                "<!-- top -->\nMUST: a.\n\n:::output\nOne line.\n<!-- inner -->\n:::\n\n\
                 ::::context{verbatim}\n<!-- shown -->\n:::!human{name=h role=r}\n:::\n::::\n"
            ),
            "**MUST:** a.\n\n**OUTPUT:**\nOne line.\n\n<reference>\n<!-- shown -->\n\
             :::!human{name=h role=r}\n:::\n</reference>\n"
        );
    }

    /// A prose body is delivered as a fragment: a rule inside an output, a
    /// context or a kept `when` is rendered as a rule.
    #[test]
    fn rules_inside_bodies_are_rendered() {
        assert_eq!(
            delivered(
                ":::output{format=json}\nOne object.\n\nNEVER[raw]: add prose around it.\n:::\n\n\
                 ::::when{host=\"agentd\"}\n:::context\nSHOULD (if asked): cite.\n:::\n::::\n"
            ),
            "**OUTPUT (json):**\nOne object.\n\n**NEVER:** add prose around it.\n\n\
             <reference>\n**SHOULD (if asked):** cite.\n</reference>\n"
        );
    }

    /// Machinery nested in a body is not folded (the fold reads the top
    /// level), so it is not acknowledged either: the model is never told a
    /// workflow is loaded that is not. At the top level it is.
    #[test]
    fn nested_machinery_claims_nothing() {
        let wf = ":::!workflow{name=w}\nsteps: { s: { kind: manual } }\n:::";
        let nested = format!("::::when{{env=\"prod\"}}\nKept.\n{wf}\n::::\n");
        let ex = fold(&parse(&nested).unwrap(), &all_families()).unwrap();
        assert!(ex.workflows.is_empty());
        assert_eq!(ex.cleaned, "Kept.\n");
        let ex = fold(&parse(wf).unwrap(), &all_families()).unwrap();
        assert_eq!(ex.workflows.len(), 1);
        assert_eq!(
            ex.cleaned,
            "[workflow \"w\" is loaded and runs autonomously]\n"
        );
    }

    /// S11: a skill with a trigger says when to use it — `trigger`, or its
    /// version-1 alias `when`, `trigger` winning both. Without one the
    /// acknowledgement is unchanged.
    #[test]
    fn a_skill_trigger_is_acknowledged() {
        assert_eq!(
            delivered(
                ":::!skill{name=a trigger=\"writing\"}\nx\n:::\n\n:::!skill{name=b when=\"refunding\"}\nx\n:::\n\n\
                 :::!skill{name=c trigger=\"first\" when=\"second\"}\nx\n:::\n\n:::!skill{name=d}\nx\n:::\n"
            ),
            "[skill \"a\" is available — use it when writing; reference it as @skill/a]\n\n\
             [skill \"b\" is available — use it when refunding; reference it as @skill/b]\n\n\
             [skill \"c\" is available — use it when first; reference it as @skill/c]\n\n\
             [skill \"d\" is available — reference it as @skill/d]\n"
        );
    }

    /// The skill catalogue's body is rendered as its prose would be
    /// delivered (§4.5 rule 4 reads keywords inside a `!skill`): no rule
    /// name, the reason and condition rendered, an alias canonical, a note
    /// stripped — in the document's style.
    #[test]
    fn a_skill_body_is_rendered_like_prose() {
        let skill = ":::!skill{name=tone}\nMUST[warm] (if upset): be warm.\nBECAUSE: tone matters.\n\n\
                     <!-- draft: revisit -->\nALWAYS: sign off.\n:::\n";
        let ex = fold(&parse(skill).unwrap(), &all_families()).unwrap();
        assert_eq!(
            ex.skills[0].body,
            "**MUST (if upset):** be warm.\n**BECAUSE:** tone matters.\n\n**MUST:** sign off."
        );
        let tagged = format!("---\ndelivery: {{labels: tags}}\n---\n{skill}");
        let ex = fold(&parse(&tagged).unwrap(), &all_families()).unwrap();
        assert_eq!(
            ex.skills[0].body,
            "<must if=\"upset\">be warm.</must>\n<because>tone matters.</because>\n\n<must>sign off.</must>"
        );
        // A section skill's body is the section beneath its heading.
        let ex = fold(
            &parse("## !skill refunds\n\nNEVER[x]: promise.\n").unwrap(),
            &all_families(),
        )
        .unwrap();
        assert_eq!(ex.skills[0].body, "**NEVER:** promise.");
    }

    /// A keyword keeps its list marker as written (`*`, `+`, `1)`), and a
    /// keyword line that is indented is no keyword line at all (the grammar
    /// matches at column 0) — it is prose, delivered as written.
    #[test]
    fn list_markers_are_kept_and_an_indented_keyword_is_prose() {
        assert_eq!(
            delivered("* MUST: a.\n+ NEVER: b.\n1) MAY: c.\n\nNotes:\n  MUST: not a rule.\n"),
            "* **MUST:** a.\n+ **NEVER:** b.\n1) **MAY:** c.\n\nNotes:\n  MUST: not a rule.\n"
        );
    }

    /// An alias delivers its canonical label: `info` is labelled NOTE,
    /// `always` MUST, `avoid` SHOULD NOT.
    #[test]
    fn an_alias_delivers_its_canonical_label() {
        assert_eq!(
            delivered(
                ":::info\nOffice hours are 9-5.\n:::\n\nALWAYS: cite.\n\nAVOID: jargon.\n\n> [!INFO]\n> Quoted.\n"
            ),
            "**NOTE:** Office hours are 9-5.\n\n**MUST:** cite.\n\n**SHOULD NOT:** jargon.\n\n**NOTE:** Quoted.\n"
        );
    }

    /// An inert block (S23) delivers its body raw, fences removed: a keyword
    /// line in it is not lifted, so it is not bolded.
    #[test]
    fn an_inert_body_is_raw() {
        assert_eq!(
            delivered(":::aside\nMUST: not a rule here.\n:::\n\n:::eval\nprose.\n:::\n"),
            "MUST: not a rule here.\n\nprose.\n"
        );
    }

    /// Inline references degrade in prose, never inside a code span or
    /// fenced code.
    #[test]
    fn references_degrade_outside_code_only() {
        assert_eq!(
            degrade_inline(
                "ask [[human/lead|the lead]] or [her](#human/lead), not `[[human/lead]]`"
            ),
            "ask the lead or her, not `[[human/lead]]`"
        );
        // A fragment link to a kind the registry does not know is a link.
        assert_eq!(degrade_inline("[x](#nope/y)"), "[x](#nope/y)");
        assert_eq!(degrade_inline("[[[must/a]]]"), "[a]");
    }
}
