// SPDX-License-Identifier: MIT OR Apache-2.0
//! **Advisories** (Appendix C; S19): diagnostics that never refuse. A
//! document with advisories loads and delivers exactly as it would without
//! them — [`advise`] reads a parsed [`Document`] and changes nothing — and
//! each one points at what the writer probably meant: `Must:` that is prose,
//! a `when` key no reader supplies, a `${x}` nothing declares.
//!
//! A port of the reference implementation's `advise.ts`, which the Go port
//! mirrors and `conformance/advisories/` pins, line, code, severity and
//! message. The line scans (near misses, orphan reasons, keywords in
//! examples, links, placeholders) read prose only (Appendix C): never an
//! author note — one in a block's body included, where `advise.ts` still
//! scans — fenced code, a machinery body, or front or end matter. The block
//! checks (variants, a skill's `when`, overrides, parameters) apply wherever
//! the block is. The crate has no regex engine, so each pattern the
//! reference writes as a regex is matched by hand here, and says which.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::doc::{self, Block, Disposition, Document, Form, Node, registry};

/// How sure an advisory is that the writer meant something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Probably not what the writer meant.
    Warning,
    /// Worth knowing; often deliberate.
    Info,
}

impl Severity {
    /// The severity as Appendix C and `advisories.json` spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// One advisory: its line, its Appendix C code and severity, and the message.
/// Unlike a [`crate::Refusal`]'s, the message carries its `line N: ` prefix,
/// as `advisories.json` pins it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Advisory {
    /// The 1-based line in the whole text, front matter included.
    pub line: usize,
    /// The Appendix C code: what conformance compares.
    pub code: &'static str,
    pub severity: Severity,
    /// `line N: ` and what the writer probably meant.
    pub message: String,
}

/// Every Appendix C code with its severity: the one table [`advise`] reads a
/// code's severity from. A test holds it to the specification's Appendix C.
pub const ADVISORY_CODES: &[(&str, Severity)] = &[
    ("near-miss-keyword", Severity::Warning),
    ("when-unknown-key", Severity::Warning),
    ("undeclared-parameter", Severity::Warning),
    ("unused-parameter", Severity::Info),
    ("unqualified-wikilink", Severity::Warning),
    ("sigil-scheme-mismatch", Severity::Warning),
    ("orphan-because", Severity::Warning),
    ("orphan-otherwise", Severity::Warning),
    ("otherwise-conditions", Severity::Warning),
    ("empty-variant", Severity::Info),
    ("skill-when-alias", Severity::Info),
    ("keyword-in-example", Severity::Info),
    ("param-default-type", Severity::Warning),
    ("overrides-external", Severity::Info),
];

/// The Appendix C advisories for a parsed document, sorted by line and then
/// code (a stable sort: two of one code on one line keep the order they were
/// found in, which for a variant's unknown keys is name order). Reading them
/// changes neither the parse nor the delivery.
pub fn advise(doc: &Document) -> Vec<Advisory> {
    let mut out: Vec<Advisory> = Vec::new();
    let mut add = |line: usize, code: &'static str, message: String| {
        let severity = ADVISORY_CODES
            .iter()
            .find(|(c, _)| *c == code)
            .map_or(Severity::Warning, |(_, s)| *s);
        out.push(Advisory {
            line,
            code,
            severity,
            message,
        });
    };
    let reg = registry();
    let lines: Vec<&str> = doc.raw.split('\n').collect();
    // Regions and reasons index the body; lines are the whole text's.
    let at = |body_line: usize| doc.outline.base + body_line + 1;
    let top: Vec<&Block> = doc.blocks().collect();
    let all = doc::every_block(&top);
    let notes = note_lines(doc, &all);
    let prose = prose_lines(doc, &lines, &all, &notes);
    let decls = declarations(doc, &lines);

    let mut example_lines = BTreeSet::new();
    let mut reason_starts = BTreeSet::new();
    for b in &all {
        if b.kind == "example" && b.form == Form::Container {
            example_lines.extend(at(b.region.0) + 1..at(b.region.1));
        }
        if let Some((start, _)) = b.reason {
            reason_starts.insert(at(start));
        }
    }

    // Near-miss keywords, orphan reasons, keywords inside examples — at the
    // start of a paragraph or a list item.
    let near = NearMiss::new();
    for (i, l) in lines.iter().enumerate() {
        let n = i + 1;
        if !prose.contains(&n) {
            continue;
        }
        let prev = if i == 0 { "" } else { lines[i - 1] };
        let starts = prev.trim().is_empty()
            || doc::list_marker_len(l) > 0
            || l.starts_with('>')
            || doc::heading_level(prev).is_some()
            || is_open_fence(prev)
            || is_close_fence(prev);
        if !starts {
            continue;
        }
        if example_lines.contains(&n) {
            if let Some(k) = doc::keyword_line(l) {
                add(
                    n,
                    "keyword-in-example",
                    format!(
                        "line {n}: \"{}:\" inside an example is quoted text, not a rule",
                        k.keyword
                    ),
                );
            }
            continue;
        }
        if let Some((0, _)) = doc::reason_line(l) {
            if !reason_starts.contains(&n) {
                add(
                    n,
                    "orphan-because",
                    format!("line {n}: BECAUSE: follows no rule — it is delivered as written"),
                );
            }
            continue;
        }
        if doc::keyword_line(l).is_some() {
            continue;
        }
        if let Some((shown, word)) = near.find(l) {
            let what = if word == reg.reason_keyword() {
                "reason"
            } else {
                "rule"
            };
            add(
                n,
                "near-miss-keyword",
                format!("line {n}: \"{shown}\" is prose — write \"{word}:\" to make it a {what}"),
            );
        }
    }

    // References and parameters in prose. A `${x}` anywhere counts as a use
    // of `x`: in a note, in code, in front matter.
    let mut used: BTreeSet<String> = BTreeSet::new();
    let sigils = reg.sigils();
    for (i, l) in lines.iter().enumerate() {
        let n = i + 1;
        used.extend(placeholders(l).map(str::to_string));
        if !prose.contains(&n) {
            continue;
        }
        let text = without_code_spans(l);
        for name in unqualified_wikilinks(&text) {
            add(
                n,
                "unqualified-wikilink",
                format!("line {n}: [[{name}]] is not a reference — write [[kind/name]]"),
            );
        }
        for (sigil, scheme) in sigil_links(&text) {
            let known = sigils.values().flatten().any(|s| s == scheme);
            let own = sigils
                .get(sigil)
                .is_some_and(|schemes| schemes.iter().any(|s| s == scheme));
            if known && !own {
                let what = match sigil {
                    "#" => "for instructions",
                    "@" => "for people and agents",
                    _ => "for capabilities",
                };
                add(
                    n,
                    "sigil-scheme-mismatch",
                    format!("line {n}: \"{sigil}\" is {what} — this is an ordinary link"),
                );
            }
        }
        for name in placeholders(&text) {
            if !decls.iter().any(|d| d.name == name) {
                add(
                    n,
                    "undeclared-parameter",
                    format!("line {n}: ${{{name}}} is not declared — it is delivered as written"),
                );
            }
        }
    }

    // Blocks: variant keys, skill aliases, overrides targets.
    let ids: BTreeSet<String> = all
        .iter()
        .filter_map(|b| doc::identity_of(b))
        .map(|(kind, name)| format!("{kind}/{name}"))
        .collect();
    for b in &all {
        if b.kind == "when" || b.kind == "unless" {
            // In name order, as the Go port reports them: `attrs` keeps no
            // source order, which `advise.ts` and the Python port use.
            for k in b.attrs.keys().filter(|k| *k != "verbatim") {
                used.insert(k.clone());
                if !reg.context_keys().contains(k) && !decls.iter().any(|d| &d.name == k) {
                    add(
                        b.line,
                        "when-unknown-key",
                        format!(
                            "line {}: \"{k}\" is not a context key or a parameter — the \
                             content is kept for every reader",
                            b.line
                        ),
                    );
                }
            }
        }
        // A bare `when` flag is read as `when=""`, and the tree keeps no
        // trace of which was written, so it is reported too; the reference
        // ports report a string value only.
        if b.kind == "skill" && b.attrs.contains_key("when") {
            add(
                b.line,
                "skill-when-alias",
                format!(
                    "line {}: write trigger= on a skill; when= is read as its alias",
                    b.line
                ),
            );
        }
        // A list only where the schema makes `overrides` one, as the
        // reference's attribute reader does.
        if reg.is_multivalued(&b.kind, "overrides")
            && let Some(list) = b.attrs.get("overrides")
        {
            for target in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
                if !ids.contains(target) {
                    add(
                        b.line,
                        "overrides-external",
                        format!(
                            "line {}: {target} is not in this document — it is looked up in \
                             included documents at delivery",
                            b.line
                        ),
                    );
                }
            }
        }
    }

    // Parameters: unused, a default of the wrong type.
    for d in &decls {
        if !used.contains(&d.name) {
            add(
                d.line,
                "unused-parameter",
                format!(
                    "line {}: parameter \"{}\" is declared but never used",
                    d.line, d.name
                ),
            );
        }
        if let Some(default) = d.attrs.get("default")
            && !doc::value_fits(Some(&d.attrs), default)
        {
            add(
                d.line,
                "param-default-type",
                format!(
                    "line {}: default \"{default}\" is not {}",
                    d.line,
                    article(d.attrs.get("type").map_or("", String::as_str))
                ),
            );
        }
    }

    // Variant groups (S10): an `otherwise` with no group, conditions on one,
    // an empty variant. Regions in source order, each `(first, last)` line.
    let mut variants: Vec<(&Block, usize, usize)> = all
        .iter()
        .filter(|b| matches!(b.kind.as_str(), "when" | "unless" | "otherwise"))
        .map(|b| (*b, at(b.region.0), at(b.region.1)))
        .collect();
    variants.sort_by_key(|&(_, start, _)| start);
    // Two variants are one group when only blank lines and notes lie
    // between them — a note in a container's or a set's body as well as a
    // top-level one, as delivery groups them. `advise.ts` counts top-level
    // notes only, and reports the `otherwise` after a nested note an orphan
    // that its own delivery keeps in the group.
    let adjacent = |end: usize, start: usize| {
        (end + 1..start).all(|k| lines[k - 1].trim().is_empty() || notes.contains(&k))
    };
    for (i, &(b, start, end)) in variants.iter().enumerate() {
        if (start + 1..end).all(|k| lines[k - 1].trim().is_empty()) {
            add(
                b.line,
                "empty-variant",
                format!("line {}: this {} has no content", b.line, b.kind),
            );
        }
        if b.kind != "otherwise" {
            continue;
        }
        if !b.attrs.is_empty() {
            add(
                b.line,
                "otherwise-conditions",
                format!(
                    "line {}: otherwise takes no conditions — they are ignored",
                    b.line
                ),
            );
        }
        let grouped = i
            .checked_sub(1)
            .map(|p| variants[p])
            .is_some_and(|(prev, _, prev_end)| {
                prev.kind != "otherwise" && adjacent(prev_end, start)
            });
        if !grouped {
            add(
                b.line,
                "orphan-otherwise",
                format!(
                    "line {}: otherwise follows no when or unless — it is always kept",
                    b.line
                ),
            );
        }
    }

    out.sort_by(|a, b| a.line.cmp(&b.line).then(a.code.cmp(b.code)));
    out
}

/// The lines of every author note (S9), 1-based: those at the top level,
/// those in a block's body, and those in a top-level set's body. The
/// reference reads the top-level ones only; see the module docs.
fn note_lines(doc: &Document, all: &[&Block]) -> BTreeSet<usize> {
    let at = |body_line: usize| doc.outline.base + body_line + 1;
    doc.nodes
        .iter()
        .filter_map(|n| match n {
            Node::Note { start, end } => Some((*start, *end)),
            _ => None,
        })
        .chain(all.iter().flat_map(|b| b.notes.iter().copied()))
        .chain(doc.outline.set_notes.iter().copied())
        .flat_map(|(start, end)| at(start)..=at(end))
        .collect()
}

/// The 1-based lines the advisory scans read as prose: not front or end
/// matter, not an author note, not a machinery block, not fenced code. Code
/// is told as the reference tells it: every line that opens with three
/// backticks or tildes toggles it, wherever the line is.
fn prose_lines(
    doc: &Document,
    lines: &[&str],
    all: &[&Block],
    notes: &BTreeSet<usize>,
) -> BTreeSet<usize> {
    let at = |body_line: usize| doc.outline.base + body_line + 1;
    let mut skip: BTreeSet<usize> = notes.clone();
    for (start, end) in [doc.outline.front_matter, doc.outline.end_matter]
        .into_iter()
        .flatten()
    {
        skip.extend(start..=end);
    }
    for b in all {
        if b.disposition == Disposition::Machinery {
            skip.extend(at(b.region.0)..=at(b.region.1));
        }
    }
    let mut out = BTreeSet::new();
    let mut in_code = false;
    for (i, l) in lines.iter().enumerate() {
        // `x-grammar.codeFenceOpen`: ^(`{3,}|~{3,})(.*)$
        if l.starts_with("```") || l.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if !in_code && !skip.contains(&(i + 1)) {
            out.insert(i + 1);
        }
    }
    out
}

/// A parameter as the advisories see it: its last declaration's attributes,
/// at the line of that declaration, in the order names were first declared.
struct Declared {
    name: String,
    attrs: BTreeMap<String, String>,
    line: usize,
}

/// Every declared parameter. A front-matter entry is placed at its `name:`
/// line — the first front-matter line holding `name:`, blanks, an optional
/// quote and the name, as the reference's `name:\s*["']?<name>\b` finds it
/// — or at line 1 when none does.
fn declarations(doc: &Document, lines: &[&str]) -> Vec<Declared> {
    let front_end = doc.outline.front_matter.map_or(0, |(_, end)| end);
    let mut out: Vec<Declared> = Vec::new();
    for d in doc::param_declarations(doc) {
        let line = d.line.unwrap_or_else(|| {
            lines[..front_end]
                .iter()
                .position(|l| declares_name(l, &d.name))
                .map_or(1, |i| i + 1)
        });
        match out.iter_mut().find(|e| e.name == d.name) {
            Some(e) => {
                e.attrs = d.attrs;
                e.line = line;
            }
            None => out.push(Declared {
                name: d.name,
                attrs: d.attrs,
                line,
            }),
        }
    }
    out
}

/// Whether `line` holds `name:\s*["']?<name>\b` anywhere.
fn declares_name(line: &str, name: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    line.match_indices("name:").any(|(at, _)| {
        let rest = line[at + 5..].trim_start();
        [rest.strip_prefix(['"', '\'']), Some(rest)]
            .into_iter()
            .flatten()
            .any(|r| {
                r.strip_prefix(name).is_some_and(|after| {
                    let before = name.chars().next_back().is_some_and(word);
                    before != after.chars().next().is_some_and(word)
                })
            })
    })
}

/// "a number", as a default that fails its type is described.
fn article(ty: &str) -> String {
    match ty {
        "enum" => "one of its values".into(),
        "url" => "a URL".into(),
        "duration" => "a duration".into(),
        "boolean" => "true or false".into(),
        other => format!("a {other}"),
    }
}

/// The reference's near-miss patterns. `NEAR_COLON`: a keyword in any casing
/// with its colon but not in the keyword form (`Must:`, `**Never**:`).
/// `NEAR_CAPS`: a rule keyword in capitals with no colon (`MUST verify`,
/// `NEVER - push`). Both after an optional list marker and `*`s. The words
/// are the registry's: every keyword and the reason keyword for the first,
/// the keywords of rule kinds for the second, longest first as an
/// alternation tries them.
struct NearMiss {
    any_case: Vec<String>,
    rules: Vec<String>,
}

impl NearMiss {
    fn new() -> NearMiss {
        let reg = registry();
        let mut any_case: Vec<String> = reg.keywords_longest_first().to_vec();
        any_case.push(reg.reason_keyword().to_string());
        any_case.sort_by_key(|w| std::cmp::Reverse(w.len()));
        let rules = reg
            .keywords_longest_first()
            .iter()
            .filter(|k| reg.keyword_kind(k).is_some_and(|kind| reg.is_rule(kind)))
            .cloned()
            .collect();
        NearMiss { any_case, rules }
    }

    /// What the writer typed — without the list marker, and for the capitals
    /// form without the text after it — and the keyword they meant, in
    /// capitals; `None` when `line` is no near miss.
    fn find(&self, line: &str) -> Option<(String, String)> {
        let (matched, word) = self.colon(line).or_else(|| self.caps(line))?;
        let typed = &matched[doc::list_marker_len(matched)..];
        Some((
            strip_shown_tail(typed).trim().to_string(),
            word.to_ascii_uppercase(),
        ))
    }

    /// `NEAR_COLON`, case-insensitive: the matched text and the word as typed.
    fn colon<'l>(&self, line: &'l str) -> Option<(&'l str, &'l str)> {
        let from = doc::list_marker_len(line);
        let rest = line[from..].trim_start_matches('*');
        let at = line.len() - rest.len();
        self.any_case.iter().find_map(|w| {
            let typed = rest.get(..w.len()).filter(|t| t.eq_ignore_ascii_case(w))?;
            let after = rest[w.len()..]
                .trim_start_matches('*')
                .trim_start_matches([' ', '\t']);
            after.strip_prefix(':')?;
            let end = line.len() - after.len() + 1;
            Some((&line[..end], &line[at..at + typed.len()]))
        })
    }

    /// `NEAR_CAPS`: the keyword in capitals, `*`s, then blanks, an optional
    /// dash with blanks after it, and one more character that is not one.
    fn caps<'l>(&self, line: &'l str) -> Option<(&'l str, &'l str)> {
        let from = doc::list_marker_len(line);
        let rest = line[from..].trim_start_matches('*');
        let at = line.len() - rest.len();
        self.rules.iter().find_map(|w| {
            let after = rest.strip_prefix(w.as_str())?.trim_start_matches('*');
            let blanks = after.trim_start_matches([' ', '\t']);
            if blanks.len() == after.len() {
                return None;
            }
            // `[ \t]+[-–—][ \t]+\S` is tried first, then `[ \t]+\S`.
            let dashed = blanks.strip_prefix(['-', '–', '—']).and_then(|d| {
                let past = d.trim_start_matches([' ', '\t']);
                (past.len() < d.len() && past.chars().next().is_some_and(|c| !c.is_whitespace()))
                    .then_some(past)
            });
            let tail = dashed.unwrap_or(blanks);
            let c = tail.chars().next().filter(|c| !c.is_whitespace())?;
            let end = line.len() - tail.len() + c.len_utf8();
            Some((&line[..end], &line[at..at + w.len()]))
        })
    }
}

/// The reference's `.replace(/(?:[ \t]+[-–—])?[ \t]+\S$/, "")`: the
/// leftmost tail that is blanks and one last character, or blanks, a dash,
/// blanks and one last character, cut off.
fn strip_shown_tail(s: &str) -> &str {
    fn blanks(t: &str) -> &str {
        t.trim_start_matches([' ', '\t'])
    }
    let last = |t: &str| {
        let mut cs = t.chars();
        cs.next().is_some_and(|c| !c.is_whitespace()) && cs.next().is_none()
    };
    let tail = |t: &str| {
        let b = blanks(t);
        if b.len() == t.len() {
            return false;
        }
        if last(b) {
            return true;
        }
        b.strip_prefix(['-', '–', '—']).is_some_and(|d| {
            let past = blanks(d);
            past.len() < d.len() && last(past)
        })
    };
    s.char_indices()
        .map(|(i, _)| i)
        .find(|&i| tail(&s[i..]))
        .map_or(s, |i| &s[..i])
}

/// Whether `line` is a block opener as `x-grammar.openFence` reads one:
/// `^(:{3,})(!?)(kind)(\[\])?\s*(\{.*\})?\s*$`.
fn is_open_fence(line: &str) -> bool {
    let colons = line.len() - line.trim_start_matches(':').len();
    if colons < 3 {
        return false;
    }
    let rest = &line[colons..];
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    let mut chars = rest.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let after = chars
        .as_str()
        .trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let after = after.strip_prefix("[]").unwrap_or(after).trim();
    after.is_empty()
        || (after.starts_with('{')
            && after.ends_with('}')
            && !after.contains(['\r', '\u{2028}', '\u{2029}']))
}

/// Whether `line` is a closing fence as `x-grammar.closeFence` reads one:
/// `^:{3,}\s*$`.
fn is_close_fence(line: &str) -> bool {
    let rest = line.trim_start_matches(':');
    line.len() - rest.len() >= 3 && rest.trim().is_empty()
}

/// The `${name}` placeholders in `text`, as the global `x-grammar.param`
/// finds them.
fn placeholders(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        while let Some(pos) = rest.find("${") {
            let after = &rest[pos + 2..];
            let len = doc::placeholder_name_len(after);
            if len > 0 && after[len..].starts_with('}') {
                rest = &after[len + 1..];
                return Some(&after[..len]);
            }
            rest = after;
        }
        None
    })
}

/// `text` with its inline code spans removed: `` `[^`]* ` `` pairs, as the
/// reference strips them before the reference scans.
fn without_code_spans(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let Some(close) = rest[open + 1..].find('`') else {
            break;
        };
        out.push_str(&rest[..open]);
        rest = &rest[open + 1 + close + 1..];
    }
    out.push_str(rest);
    out
}

/// The names of the `[[name]]` links with no kind, as the reference's
/// `\[\[([^\]/|]+)(?:\|[^\]]*)?\]\]` finds them.
fn unqualified_wikilinks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(pos) = text[i..].find("[[") {
        let start = i + pos;
        let inner = &text[start + 2..];
        let name_len = inner.find([']', '/', '|']).unwrap_or(inner.len());
        let matched = (name_len > 0)
            .then(|| {
                let after = &inner[name_len..];
                let after = match after.strip_prefix('|') {
                    Some(alias) => &alias[alias.find(']').unwrap_or(alias.len())..],
                    None => after,
                };
                after.strip_prefix("]]")
            })
            .flatten();
        match matched {
            Some(past) => {
                out.push(&inner[..name_len]);
                i = text.len() - past.len();
            }
            None => i = start + 1,
        }
    }
    out
}

/// The `(sigil, scheme)` of each link that opens like a sigiled reference,
/// as the reference's `\[([#@&])[^\]]*\]\(([a-z][a-z0-9+.-]*):\/\/` finds
/// them.
fn sigil_links(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(pos) = text[i..].find('[') {
        let start = i + pos;
        i = start + 1;
        let inner = &text[start + 1..];
        let Some(sigil) = inner.get(..1).filter(|s| matches!(*s, "#" | "@" | "&")) else {
            continue;
        };
        let Some(close) = inner.find(']') else {
            continue;
        };
        let Some(target) = inner[close + 1..].strip_prefix('(') else {
            continue;
        };
        let scheme_len = target
            .char_indices()
            .find(|&(k, c)| {
                !(c.is_ascii_lowercase() || (k > 0 && (c.is_ascii_digit() || "+.-".contains(c))))
            })
            .map_or(target.len(), |(k, _)| k);
        if scheme_len > 0 && target[scheme_len..].starts_with("://") {
            out.push((sigil, &target[..scheme_len]));
            i = text.len() - target.len() + scheme_len + 3;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The advisories of `text`, which must parse, as `(line, code)`.
    fn advised(text: &str) -> Vec<(usize, &'static str)> {
        let doc = doc::parse(text).unwrap_or_else(|e| panic!("{text:?} refused: {e:?}"));
        advise(&doc).into_iter().map(|a| (a.line, a.code)).collect()
    }

    #[test]
    fn a_rule_word_in_prose_is_no_near_miss() {
        assert_eq!(advised("Never do this in prose.\n"), []);
        assert_eq!(advised("Mustard: on the side.\n"), []);
        assert_eq!(
            advised("Must: verify identity.\n"),
            [(1, "near-miss-keyword")]
        );
    }

    #[test]
    fn an_ordinary_link_with_a_sigil_is_no_sigil_scheme_hit() {
        assert_eq!(advised("Ask [@Ana](https://example.com/ana).\n"), []);
        assert_eq!(
            advised("Ask [@Ops](server://ops).\n"),
            [(1, "sigil-scheme-mismatch")]
        );
        // A scheme of the sigil's own row is a reference, not a mismatch.
        assert_eq!(advised("Ask [@Ops](agent://ops).\n"), []);
    }

    #[test]
    fn nothing_is_advised_inside_a_note() {
        assert_eq!(
            advised("<!--\n\nMust: not advised.\nAsk [[oncall]].\n-->\n"),
            []
        );
        // A note in a container's body is a note too (Appendix C: never
        // inside author notes), and so is one in a set's body.
        assert_eq!(
            advised(":::when{agent=\"claude\"}\nx\n\n<!--\n\nMust: no.\n-->\n:::\n"),
            []
        );
        assert_eq!(
            advised(
                ":::param[]\n| name | description |\n|---|---|\n| who | ${who} |\n\
                 <!--\n\nMust: no.\n-->\n:::\n"
            ),
            []
        );
    }

    #[test]
    fn nothing_is_advised_inside_fenced_code() {
        assert_eq!(advised("```\nMust: code\n```\n"), []);
        assert_eq!(advised("~~~md\n[[oncall]]\n~~~\n"), []);
    }

    #[test]
    fn nothing_is_advised_inside_a_machinery_body() {
        assert_eq!(
            advised(":::!skill{name=tone}\nMust: warm.\n\nAsk [[oncall]].\n:::\n"),
            []
        );
        // Placed past the front matter: a block's region indexes the body.
        assert_eq!(
            advised("---\nspec: 1\n---\n:::!skill{name=t}\nMust: x\n:::\n\nMust: y\n"),
            [(8, "near-miss-keyword")]
        );
    }

    #[test]
    fn nothing_is_advised_inside_front_or_end_matter() {
        assert_eq!(
            advised("---\nspec: 1\nowner: \"[[oncall]]\"\n---\nHello.\n"),
            []
        );
        assert_eq!(advised("Hello.\n\n---\nowner: \"[[oncall]]\"\n---\n"), []);
    }

    #[test]
    fn a_variant_in_a_section_is_read_at_its_own_lines() {
        // Only machinery takes the section form, so nothing in a section is
        // prose; but a variant there is still judged by its region, which a
        // section's child carries as a container's does.
        let text = "## !skill tone\n\nWarm.\n\n:::when{agent=\"claude\"}\nC.\n:::\n\n\
                    :::otherwise\nO.\n:::\n\n:::when{agent=\"gpt\"}\n:::\n";
        assert_eq!(advised(text), [(13, "empty-variant")]);
    }

    #[test]
    fn a_keyword_in_an_example_is_quoted_text() {
        assert_eq!(
            advised("# Tone\n\n:::example\nMUST: quoted.\n:::\n"),
            [(4, "keyword-in-example")]
        );
        // Not a near miss either: what an example holds is quoted.
        assert_eq!(advised(":::example\nMust: quoted.\n:::\n"), []);
    }

    #[test]
    fn a_front_matter_parameter_is_placed_at_its_name_line() {
        let text = "---\nspec: 1\nparameters:\n  - name: \"tone\"\n    type: number\n    \
                    default: warm\n---\nHello.\n";
        assert_eq!(
            advised(text),
            [(4, "param-default-type"), (4, "unused-parameter")]
        );
        assert!(declares_name("  - name: tone", "tone"));
        assert!(!declares_name("  - name: tones", "tone"));
        assert!(declares_name("  - name: 'tone'", "tone"));
    }

    #[test]
    fn an_otherwise_after_a_note_still_closes_its_group() {
        let grouped = ":::when{agent=\"claude\"}\nC.\n:::\n<!-- x -->\n\n:::otherwise\nO.\n:::\n";
        assert_eq!(advised(grouped), []);
        let parted = ":::when{agent=\"claude\"}\nC.\n:::\nprose\n:::otherwise\nO.\n:::\n";
        assert_eq!(advised(parted), [(5, "orphan-otherwise")]);
    }

    #[test]
    fn a_keyword_and_its_reason_are_no_near_miss_and_no_orphan() {
        assert_eq!(advised("MUST: verify.\n\nBECAUSE: it matters.\n"), []);
        assert_eq!(
            advised("Prose.\n\nBECAUSE: nothing.\n"),
            [(3, "orphan-because")]
        );
    }

    #[test]
    fn a_declared_parameter_is_a_known_when_key() {
        let text = "::param{name=tier}\n\n:::when{tier=\"gold\"}\nGold.\n:::\n";
        assert_eq!(advised(text), []);
    }

    #[test]
    fn an_override_of_a_rule_in_this_document_is_not_external() {
        let text = "SHOULD[brevity]: be short.\n\n:::must{overrides=\"should/brevity\"}\nx\n:::\n";
        assert_eq!(advised(text), []);
    }

    #[test]
    fn a_near_miss_shows_what_was_typed() {
        let near = NearMiss::new();
        let found = |l: &str| near.find(l);
        assert_eq!(
            found("- **Never**: push"),
            Some(("**Never**:".into(), "NEVER".into()))
        );
        assert_eq!(
            found("MUST verify twice."),
            Some(("MUST".into(), "MUST".into()))
        );
        assert_eq!(
            found("NEVER - push"),
            Some(("NEVER".into(), "NEVER".into()))
        );
        assert_eq!(
            found("MUST NOT verify"),
            Some(("MUST NOT".into(), "MUST NOT".into()))
        );
        assert_eq!(
            found("because: x"),
            Some(("because:".into(), "BECAUSE".into()))
        );
        assert_eq!(found("MUSTARD x"), None);
        assert_eq!(found("Must verify"), None);
    }

    #[test]
    fn the_near_miss_words_are_the_references() {
        // advise.ts:9 and :13, as the registry derives them.
        let near = NearMiss::new();
        let set = |v: &[String]| v.iter().map(|w| w.to_lowercase()).collect::<BTreeSet<_>>();
        let words = |s: &str| s.split('|').map(str::to_string).collect::<BTreeSet<_>>();
        assert_eq!(
            set(&near.any_case),
            words(
                "must not|should not|must|should|never|guardrail|may|always|avoid|because|\
                 note|info|tip|warning|caution|important|example"
            )
        );
        assert_eq!(
            set(&near.rules),
            words("must not|should not|must|should|never|guardrail|may|always|avoid")
        );
    }

    #[test]
    fn a_paragraph_starts_after_a_blank_a_heading_or_a_fence_or_at_a_list_item() {
        assert_eq!(advised("# H\nMust: x\n"), [(2, "near-miss-keyword")]);
        assert_eq!(
            advised(":::note\nhi\n:::\nMust: x\n"),
            [(4, "near-miss-keyword")]
        );
        assert_eq!(advised("Text\n- Must: x\n"), [(2, "near-miss-keyword")]);
        // Inside a paragraph it is a word of the paragraph.
        assert_eq!(advised("Text\nMust: x\n"), []);
    }

    #[test]
    fn the_near_miss_message_names_a_reason_or_a_rule() {
        let messages = |text: &str| {
            let doc = doc::parse(text).unwrap();
            advise(&doc)
                .into_iter()
                .map(|a| a.message)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            messages("because: x\n"),
            ["line 1: \"because:\" is prose — write \"BECAUSE:\" to make it a reason"]
        );
        assert_eq!(
            messages("Must: x\n"),
            ["line 1: \"Must:\" is prose — write \"MUST:\" to make it a rule"]
        );
    }

    #[test]
    fn only_a_reason_at_column_zero_is_an_orphan() {
        assert_eq!(advised("Prose.\n\n  BECAUSE: x\n"), []);
    }

    #[test]
    fn a_verbatim_flag_on_a_variant_is_no_context_key() {
        assert_eq!(advised(":::when{agent=\"a\" verbatim}\nx\n:::\n"), []);
    }

    #[test]
    fn an_otherwise_after_an_otherwise_is_an_orphan() {
        let text = ":::when{agent=\"a\"}\nA\n:::\n:::otherwise\nB\n:::\n:::otherwise\nC\n:::\n";
        assert_eq!(advised(text), [(7, "orphan-otherwise")]);
    }

    #[test]
    fn a_variant_of_blank_lines_is_empty() {
        assert_eq!(
            advised(":::when{agent=\"a\"}\n  \n:::\n"),
            [(1, "empty-variant")]
        );
    }

    #[test]
    fn a_note_in_a_container_still_joins_a_variant_group() {
        // A deliberate departure from `advise.ts`, which reports the
        // `otherwise` an orphan here (see the module docs): delivery keeps
        // the two as one group, and so do the advisories.
        let text =
            "::::note\n:::when{agent=\"a\"}\nA\n:::\n<!-- n -->\n:::otherwise\nB\n:::\n::::\n";
        assert_eq!(advised(text), []);
    }

    #[test]
    fn a_redeclared_parameter_is_judged_by_its_last_declaration() {
        let text = "---\nparameters:\n  - name: a\n---\n::param{name=a type=number default=x}\n";
        assert_eq!(
            advised(text),
            [(5, "param-default-type"), (5, "unused-parameter")]
        );
    }

    #[test]
    fn a_list_default_is_quoted_as_the_reference_renders_it() {
        let text = "---\nparameters: [{name: p, type: number, default: [1, 2]}]\n---\n${p}\n";
        let doc = doc::parse(text).unwrap();
        let messages: Vec<String> = advise(&doc).into_iter().map(|a| a.message).collect();
        assert_eq!(messages, ["line 2: default \"1,2\" is not a number"]);
    }

    /// Every line an advisory names is the whole text's, front matter
    /// included — the body's regions and reasons are placed past it.
    #[test]
    fn advisories_are_placed_past_the_front_matter() {
        assert_eq!(
            advised("---\nspec: 1\n---\n\nMust: x\n"),
            [(5, "near-miss-keyword")]
        );
        assert_eq!(
            advised("---\nspec: 1\n---\n[[oncall]]\n"),
            [(4, "unqualified-wikilink")]
        );
        assert_eq!(
            advised("---\nspec: 1\n---\n:::when{agent=\"a\"}\n:::\nprose\n:::otherwise\nO\n:::\n"),
            [(4, "empty-variant"), (7, "orphan-otherwise")]
        );
        assert_eq!(
            advised("---\nspec: 1\n---\n:::example\nMUST: q\n:::\n"),
            [(5, "keyword-in-example")]
        );
        assert_eq!(advised("---\nspec: 1\n---\nMUST: x\n\nBECAUSE: y\n"), []);
    }

    #[test]
    fn code_spans_hide_links_and_placeholders() {
        assert_eq!(advised("Write `[[oncall]]` and `${x}`.\n"), []);
        assert_eq!(without_code_spans("a `b` c `d"), "a  c `d");
    }

    #[test]
    fn the_pattern_scans_match_as_the_regexes_do() {
        assert_eq!(
            placeholders("${a ${b} ${} ${c}").collect::<Vec<_>>(),
            ["b", "c"]
        );
        assert_eq!(
            unqualified_wikilinks("[[a]] [[k/n]] [[b|B]] [[c"),
            ["a", "b"]
        );
        assert_eq!(
            sigil_links("[@x](server://a) [#y](https://b) [z](agent://c)"),
            [("@", "server"), ("#", "https")]
        );
        assert_eq!(strip_shown_tail("MUST - p"), "MUST");
        assert_eq!(strip_shown_tail("MUST NOT v"), "MUST NOT");
        assert_eq!(strip_shown_tail("Must:"), "Must:");
        assert!(is_open_fence(":::when{agent=\"x\"}"));
        assert!(is_open_fence("::::!skill[]"));
        assert!(!is_open_fence(":::note hello"));
        // `.` in the reference's `\{.*\}` matches no line terminator.
        for t in ['\r', '\u{2028}', '\u{2029}'] {
            assert!(!is_open_fence(&format!(":::note{{title=\"a{t}b\"}}")));
        }
        assert!(is_close_fence(":::  "));
        assert!(!is_close_fence("  :::"));
    }
}
