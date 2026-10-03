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
//! `proseBlockLines`, `deliverFragment`), with one departure: `${}` is
//! substituted where a source line becomes a delivered one, after its inline
//! references are degraded, rather than over the whole text at the end. A
//! value is still never re-read — nothing reads a delivered line again — and
//! two things the whole-text pass got wrong come right. Whether a line is
//! fenced code is read from the source, before a label is glued onto it
//! (`**MUST:** ```sh` opens no fence the old pass could see). And an included
//! document's lines arrive substituted with its own parameters, so the
//! includer neither fills the placeholders the include left nor substitutes a
//! value the include inserted a second time (§5.2 include rule 1; §3.5: a
//! value is never re-parsed).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::doc::{
    self, Block, BodyKind, Disposition, Document, Extraction, Form, IncludeResolver, Node, Piece,
    registry,
};
use crate::{Authored, Fact, Include, Limits, Manifest, ParameterUse, Variants};

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
/// the include recursion (shared down the include tree), what the document
/// being walked declares (its own label style, parameters and facts), and
/// what the whole delivery accounts for (the overrides, the bytes inlined,
/// and the rest of its manifest). Private, so a later rule adds a field here
/// instead of threading another argument through every renderer.
pub(crate) struct Walk<'a> {
    pub(crate) granted: &'a BTreeSet<String>,
    /// The values `${name}` resolves to in this document (S18): each
    /// declared or given parameter whose value fits its type.
    pub(crate) params: BTreeMap<String, String>,
    /// What `when` and `unless` match against: the parameters plus the
    /// runtime facts, the facts winning a collision (they are
    /// runtime-authoritative). On a collision a variant selects on the fact
    /// while `${key}` still substitutes the parameter; the reference lets the
    /// parameter win both. An open question upstream, pinned by a test.
    pub(crate) facts: BTreeMap<String, String>,
    /// The runtime facts the delivery was given, apart from any parameter:
    /// all an included document receives from the delivery that includes it,
    /// beside its own parameters. An includer's parameter values never reach
    /// an include, as facts or as values (§5.2 include rule 1: "resolved with
    /// its own parameters").
    pub(crate) runtime: BTreeMap<String, String>,
    pub(crate) resolver: IncludeResolver<'a>,
    /// How many includes deep this document is (0 = the delivered document).
    pub(crate) depth: usize,
    /// The include targets on the path to this document, for cycles.
    pub(crate) seen: BTreeSet<String>,
    pub(crate) style: Style,
    /// The parameter declarations, which a `form` lists.
    pub(crate) decls: BTreeMap<String, BTreeMap<String, String>>,
    /// The overrides in force here (S24): those this document declares and,
    /// before them, those of every document on the include path above it.
    pub(crate) overrides: Vec<Override>,
    /// The rules not delivered because an override named them, `kind/name`,
    /// in the order delivery met them.
    pub(crate) overridden: Vec<String>,
    /// The bytes inlined from includes so far, over the whole delivery.
    pub(crate) include_bytes: usize,
    /// What the whole delivery's manifest accounts for (S7), handed down to
    /// each include's walk and back, so it is one account however deep the
    /// includes go.
    tally: Tally,
    /// The variants this document kept and dropped, by their per-kind
    /// counters (S7): an included document counts its own from 1 and
    /// contributes none of them to the delivery's manifest.
    variants: Variants,
    counters: BTreeMap<String, u64>,
    /// How many bodies deep the renderer is in this document, and whether a
    /// body went past [`NESTING_CAP`]: the fold refuses the document then.
    nesting: usize,
    pub(crate) too_deep: bool,
}

/// How many bodies deep delivery renders, each inside the last: a rule in a
/// `context` in a `when` is three. Twice what parse lets blocks nest
/// ([`doc::NESTING_CAP`]), so no nesting of containers reaches it; what does
/// is a chain of bodies read again — an alert quoted inside an alert, a
/// keyword's text opening with a keyword — which parse never sees, and each
/// of which re-reads everything inside it. Past the cap the document is
/// refused rather than delivered in part. Counted per document: an include
/// is its own.
pub(crate) const NESTING_CAP: usize = 2 * doc::NESTING_CAP;

/// What a delivery's manifest accounts for beyond its variants (S7), as the
/// walk delivers it. Only the delivered text is accounted for: a dropped
/// variant is never walked, a `form` lists its inputs without substituting
/// its body, and a skill's catalogue body is walked with the tally set aside
/// ([`skill_body`]).
#[derive(Default)]
struct Tally {
    /// The facts a `when` or `unless` compared, kept or dropped, with the
    /// value compared — across nested variants and included documents. A key
    /// compared again records its latest value, as the reference's shared
    /// map does.
    facts: BTreeMap<String, String>,
    /// Each parameter substituted, by name: its declared source and the
    /// value used. The first use wins — one name, one entry.
    parameters: BTreeMap<String, (String, String)>,
    /// The placeholders a delivered line left as written.
    unresolved: BTreeSet<String>,
    /// Each transclusion, in pre-order as delivery met it.
    includes: Vec<Include>,
    /// The deepest include level reached; 0 when nothing was inlined.
    include_depth: usize,
}

/// One `overrides` target a rule declares (S24), and where.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Override {
    /// `kind/name`, as written.
    target: String,
    /// The overriding rule's strength: a target stronger than it is kept.
    strength: u32,
    /// The include depth of the declaring document.
    depth: usize,
    /// Whether the declaring document has the target itself. A local target
    /// is answered only there; any other only by the documents it includes.
    local: bool,
    /// Whether delivery has met the target — silenced or, a guardrail or a
    /// stronger rule, kept. A found override answers nothing more.
    found: bool,
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
            runtime: facts.clone(),
            resolver,
            depth: 0,
            seen: BTreeSet::new(),
            style: Style::Bold,
            decls: BTreeMap::new(),
            overrides: Vec::new(),
            overridden: Vec::new(),
            include_bytes: 0,
            tally: Tally::default(),
            variants: Variants::default(),
            counters: BTreeMap::new(),
            nesting: 0,
            too_deep: false,
        };
        w.enter(doc, params, facts);
        w
    }

    /// The walk of a document included as `id` from this one: one level
    /// deeper, `id` on the path, delivered with its OWN declarations and
    /// style. It receives no caller parameters, and only the runtime facts:
    /// its variants select on its own parameter values, never on the
    /// includer's, which were checked against no declaration of its. The
    /// overrides pending here go down with it; its own are added after them
    /// and end with it. The delivery's tally goes down with it too, and its
    /// variant counters start again. Hand it back with [`Walk::leave`].
    pub(crate) fn include(&mut self, id: &str, doc: &Document) -> Walk<'a> {
        let mut seen = self.seen.clone();
        seen.insert(id.to_string());
        let mut w = Walk {
            granted: self.granted,
            params: BTreeMap::new(),
            facts: BTreeMap::new(),
            runtime: self.runtime.clone(),
            resolver: self.resolver,
            depth: self.depth + 1,
            seen,
            style: Style::Bold,
            decls: BTreeMap::new(),
            overrides: self.overrides.clone(),
            overridden: Vec::new(),
            include_bytes: self.include_bytes,
            tally: std::mem::take(&mut self.tally),
            variants: Variants::default(),
            counters: BTreeMap::new(),
            nesting: 0,
            too_deep: false,
        };
        w.enter(doc, &BTreeMap::new(), &self.runtime);
        w
    }

    /// Take back what an include's walk accounted for: which of the
    /// overrides pending here it found, the rules it overrode, the bytes it
    /// inlined, the tally. Its own overrides and variants end with it.
    pub(crate) fn leave(&mut self, sub: Walk) {
        let n = self.overrides.len();
        self.overrides = sub.overrides.into_iter().take(n).collect();
        self.overridden.extend(sub.overridden);
        self.include_bytes = sub.include_bytes;
        self.tally = sub.tally;
    }

    /// Account for an include's text being inlined (S7): after the resolver
    /// handed it over and the cycle, depth and byte caps let it through,
    /// before it is delivered — its target as written, the digest of its
    /// authored text, and the level it reaches.
    pub(crate) fn inlined(&mut self, target: &str, text: &str) {
        self.tally.includes.push(Include {
            target: target.to_string(),
            digest: crate::digest(text.as_bytes()),
        });
        self.tally.include_depth = self.tally.include_depth.max(self.depth + 1);
    }

    /// Account for a variant delivery met (S7): its per-kind counter in this
    /// document, kept or dropped, and every fact its conditions compared.
    fn variant(&mut self, b: &Block, kept: bool) {
        let n = self.counters.entry(b.kind.clone()).or_default();
        *n += 1;
        let id = format!("{}#{n}", b.kind);
        if kept {
            self.variants.kept.push(id);
        } else {
            self.variants.dropped.push(id);
        }
        for (key, _) in doc::conditions(b) {
            if let Some(value) = self.facts.get(key) {
                self.tally.facts.insert(key.clone(), value.clone());
            }
        }
    }

    /// The delivery's §7.4 manifest, once the walk of `doc` — the delivered
    /// document — is done and folded into `ex`.
    pub(crate) fn manifest(self, doc: &Document, ex: &Extraction) -> Manifest {
        let Tally {
            facts,
            parameters,
            mut unresolved,
            includes,
            include_depth,
        } = self.tally;
        // An override target found nowhere is unresolved too (S24).
        unresolved.extend(ex.unfound_overrides.iter().cloned());
        let mut overridden = ex.overridden.clone();
        overridden.sort();
        Manifest {
            // A file reader knows no version; a registry sets it.
            authored: Authored {
                digest: crate::author_digest(doc.raw.as_bytes()),
                version: None,
            },
            parameters: parameters
                .into_iter()
                .map(|(name, (source, value))| ParameterUse {
                    name,
                    source,
                    value_digest: crate::digest(value.as_bytes()),
                })
                .collect(),
            facts: facts
                .into_iter()
                .map(|(key, value)| Fact {
                    key,
                    value_digest: crate::digest(value.as_bytes()),
                })
                .collect(),
            variants: self.variants,
            includes,
            limits: Limits {
                include_depth: include_depth as u64,
                include_bytes: self.include_bytes as u64,
            },
            unresolved: unresolved.into_iter().collect(),
            overridden,
        }
    }

    /// The overrides the document being walked declares that found their
    /// target nowhere — in it, or in any document it includes (S24). Read
    /// once its walk is done.
    pub(crate) fn unfound_overrides(&self) -> Vec<String> {
        self.overrides
            .iter()
            .filter(|o| o.depth == self.depth && !o.found)
            .map(|o| o.target.clone())
            .collect()
    }

    /// `${name}` substituted in one delivered line (§3.5 step 7): see
    /// [`doc::substitute_line`]. Each placeholder is accounted for as it is
    /// met (S7): a value substituted under its declared source, `static`
    /// when undeclared; a placeholder left as written, unresolved.
    fn sub(&mut self, line: &str) -> String {
        let Walk {
            params,
            decls,
            tally,
            ..
        } = self;
        doc::substitute_line(line, params, &mut |name, value| match value {
            Some(value) => {
                let source = decls
                    .get(name)
                    .and_then(|d| d.get("source"))
                    .map_or("static", String::as_str);
                tally
                    .parameters
                    .entry(name.to_string())
                    .or_insert_with(|| (source.to_string(), value.to_string()));
            }
            None => {
                tally.unresolved.insert(name.to_string());
            }
        })
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
        // After the facts: an override declared inside a variant this
        // reader does not receive does not apply.
        let own = declared_overrides(doc, self);
        self.overrides.extend(own);
    }
}

/// A document's delivered lines (§3.5): its body walked once, then runs of
/// blank lines collapsed and the outer ones trimmed. Front matter and end
/// matter (S27) are not in the body the walk reads, so neither is ever
/// delivered. `${}` is not yet substituted.
pub(crate) fn document(doc: &Document, w: &mut Walk) -> Vec<String> {
    let lines: Vec<&str> = doc.source.split('\n').collect();
    let spans = node_spans(&doc.nodes);
    finalize(render(&lines, (0, lines.len()), &spans, w, true))
}

/// A skill's catalogue body, rendered as its prose would be delivered — in
/// the document's style, notes stripped, rules labelled and never named — so
/// the catalogue never shows a `MUST[name]:` the reader was never meant to
/// see. `lines` is the document body the skill's region indexes.
pub(crate) fn skill_body(b: &Block, lines: &[&str], w: &mut Walk) -> String {
    // The catalogue is not the delivery, and accounts for nothing the
    // delivery does. Overrides (S24) are accounted for over the delivered
    // text, as the reference delivers it: a rule met here first would be
    // marked found and leave its delivered twin — or a target found nowhere
    // else — wrongly accounted. An include here is inlined, but its bytes
    // are not the delivery's to cap (S7 `limits`), and the rules its own
    // overrides silence are not rules the delivered text left out. And a
    // catalogue body substitutes no parameter, as it never has: a skill's
    // text is the skill's, read when it is used.
    // Nor is the catalogue the delivery's to account for in its manifest
    // (S7): its variants, facts and includes are not the delivered text's.
    let overrides = std::mem::take(&mut w.overrides);
    let params = std::mem::take(&mut w.params);
    let (include_bytes, overridden) = (w.include_bytes, w.overridden.len());
    let tally = std::mem::take(&mut w.tally);
    let variants = std::mem::take(&mut w.variants);
    let counters = std::mem::take(&mut w.counters);
    let body = if b.set_group.is_some() {
        // A set member's body is its entry's, which has no region of its own.
        let src: Vec<String> = b.body.split('\n').map(str::to_string).collect();
        nested(w, |w| reread(&src, w))
    } else {
        body(b, lines, w)
    };
    let body = finalize(body).join("\n");
    w.overrides = overrides;
    w.params = params;
    w.include_bytes = include_bytes;
    w.overridden.truncate(overridden);
    w.tally = tally;
    w.variants = variants;
    w.counters = counters;
    body
}

/// A block's prose body delivered by the same rules, as lines (TS
/// `deliverFragment`). A body is not a document: it has no front or end
/// matter, and it is not finalized — its blank lines are the enclosing
/// document's to collapse.
///
/// A container's body is rendered from what parse read it as — its children
/// at their regions in these same lines, its notes, its inert blocks — and
/// is never read again: reading a body reads everything nested in it, so
/// reading each again at every level made delivery cost the nesting depth
/// times the size. Any other body has no lines of its own to index — a
/// keyword's text starts after its label, an alert's lines are unquoted —
/// so it is read again, as the reference reads every body.
fn body(b: &Block, lines: &[&str], w: &mut Walk) -> Vec<String> {
    nested(w, |w| match container_body(b, lines) {
        Some(range) => render(lines, range, &layout_spans(b), w, false),
        None => reread(&body_source(b, lines), w),
    })
}

/// A body read again on its own and rendered.
fn reread(body: &[String], w: &mut Walk) -> Vec<String> {
    let lines: Vec<&str> = body.iter().map(String::as_str).collect();
    let nodes = read_again(&lines);
    render(&lines, (0, lines.len()), &node_spans(&nodes), w, false)
}

/// The nodes of a body read again. The document parsed whole before delivery
/// began, so reading a piece of it again finds nothing to refuse that parse
/// did not.
fn read_again(lines: &[&str]) -> Vec<Node> {
    #[cfg(test)]
    READS.with(|r| r.set(r.get() + 1));
    doc::walk_nodes(lines, 0, &mut Vec::new()).0
}

#[cfg(test)]
thread_local! {
    /// How many bodies delivery has read again, on this thread.
    static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Render one more body deep, or note that the document nests past
/// [`NESTING_CAP`] and render nothing — the fold refuses it.
fn nested(w: &mut Walk, f: impl FnOnce(&mut Walk) -> Vec<String>) -> Vec<String> {
    if w.nesting >= NESTING_CAP {
        w.too_deep = true;
        return Vec::new();
    }
    w.nesting += 1;
    let out = f(w);
    w.nesting -= 1;
    out
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

/// Where a block, a note or an inert block lies, from a document's nodes or
/// a container's layout.
enum At<'n> {
    Block(&'n Block),
    Note(usize, usize),
    Inert(usize, usize),
}

/// What occupies each region delivery renders: `(first line, last line,
/// item)`, in order.
type Spans<'n> = Vec<(usize, usize, Item<'n>)>;

/// The regions `at` occupy, in order, a set's members as one.
fn spans<'n>(at: impl IntoIterator<Item = At<'n>>) -> Spans<'n> {
    let mut spans: Spans<'n> = Vec::new();
    for a in at {
        match a {
            At::Note(start, end) => spans.push((start, end, Item::Note)),
            At::Inert(start, end) => spans.push((start, end, Item::Inert)),
            At::Block(b) if b.set_group.is_some() => match spans.last_mut() {
                Some((_, _, Item::Set(members))) if members[0].set_group == b.set_group => {
                    members.push(b)
                }
                _ => spans.push((b.region.0, b.region.1, Item::Set(vec![b]))),
            },
            At::Block(b) => spans.push((b.region.0, b.region.1, Item::Block(b))),
        }
    }
    spans
}

/// The regions a document's (or a body's, read again) nodes occupy.
fn node_spans(nodes: &[Node]) -> Spans<'_> {
    spans(nodes.iter().filter_map(|n| match n {
        Node::Text(_) => None,
        Node::Note { start, end } => Some(At::Note(*start, *end)),
        Node::Inert { region, .. } => Some(At::Inert(region.0, region.1)),
        Node::Block(b) => Some(At::Block(b)),
    }))
}

/// The regions a container's body occupies, as parse laid it out.
fn layout_spans(b: &Block) -> Spans<'_> {
    spans(b.layout.iter().map(|p| match p {
        Piece::Child(k) => At::Block(&b.children[*k]),
        Piece::Note { start, end } => At::Note(*start, *end),
        Piece::Inert { start, end } => At::Inert(*start, *end),
    }))
}

/// Variant selection over one parent's spans (S10): whether each is
/// delivered. A `when` or an `unless` is kept by its conditions. An
/// `otherwise` is kept when no member of its group was — the run of `when`
/// and `unless` siblings immediately before it, which only blank lines and
/// author notes may separate; any other content between them ends the run,
/// and an `otherwise` with no run is kept. Anything not a variant is
/// delivered. Selection is per parent: a variant body walked again selects
/// its own nested variants, and only a kept one is walked (§5.2 rule 4).
fn select(
    lines: &[&str],
    from: usize,
    spans: &[(usize, usize, Item)],
    facts: &BTreeMap<String, String>,
) -> Vec<bool> {
    // Whether any member of the current run was kept; `None` when there is
    // no run for an `otherwise` to belong to.
    let mut group: Option<bool> = None;
    let mut next = from;
    let mut keep = Vec::with_capacity(spans.len());
    for (start, end, item) in spans {
        if lines
            .get(next..*start)
            .unwrap_or_default()
            .iter()
            .any(|l| !l.trim().is_empty())
        {
            group = None;
        }
        next = end + 1;
        keep.push(match item {
            Item::Note => true,
            Item::Block(b) if matches!(b.kind.as_str(), "when" | "unless") => {
                let kept = doc::variant_kept(b, facts);
                group = Some(group.unwrap_or(false) || kept);
                kept
            }
            Item::Block(b) if b.kind == "otherwise" => !group.take().unwrap_or(false),
            Item::Block(_) | Item::Set(_) | Item::Inert => {
                group = None;
                true
            }
        });
    }
    keep
}

/// Whether delivery walks a block's body again as a fragment of the
/// document — so a rule in it is a rule — rather than quoting it, listing
/// it or acknowledging it. One answer for the renderer and for the
/// override collector, so an `overrides` is collected from exactly the
/// bodies whose rules are delivered as rules.
fn body_is_fragment(b: &Block) -> bool {
    match b.disposition {
        Disposition::Machinery => false,
        // A kept variant's body, and that of any structural kind with one:
        // a catch-all that delivered nothing would silently drop the
        // guidance inside. Keyed on the schema's `x-body`, so a structural
        // kind a later registry adds keeps its text too.
        Disposition::Structural => {
            b.kind != "include" && doc::lookup(&b.kind).is_some_and(|k| k.body != BodyKind::None)
        }
        // An example is quoted (S16), a form lists its inputs, a glossary its
        // entries, and a `verbatim` body is quoted whole.
        Disposition::Prose => {
            !b.attrs.contains_key("verbatim")
                && match b.kind.as_str() {
                    "example" | "form" | "glossary" => false,
                    "output" | "context" | "tool" => true,
                    k => registry().label(k, false).is_some(),
                }
        }
    }
}

/// The overrides a document declares (S24), at the depth it is delivered:
/// every `overrides` target of a rule in a region this reader receives. A
/// rule inside a dropped variant declares nothing — §3.5 removes variants
/// (step 3) before it applies overrides (step 4) — so the walk selects
/// variants as delivery does and descends only into what delivery renders.
/// The same order holds on the other side: a target is local only when the
/// reader receives it here, so one this document has only inside a dropped
/// variant is looked for in the documents it includes.
fn declared_overrides(doc: &Document, w: &Walk) -> Vec<Override> {
    let reg = registry();
    let mut local = BTreeSet::new();
    let mut declared = Vec::new();
    let mut visit = |b: &Block| {
        if let Some((kind, name)) = doc::identity_of(b) {
            local.insert(format!("{kind}/{name}"));
        }
        let (Some(strength), Some(list)) = (reg.strength(&b.kind), b.attrs.get("overrides")) else {
            return;
        };
        for target in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            declared.push((target.to_string(), strength));
        }
    };
    let lines: Vec<&str> = doc.source.split('\n').collect();
    let spans = node_spans(&doc.nodes);
    received_blocks(&lines, (0, lines.len()), &spans, &w.facts, 0, &mut visit);
    declared
        .into_iter()
        .map(|(target, strength)| Override {
            local: local.contains(&target),
            target,
            strength,
            depth: w.depth,
            found: false,
        })
        .collect()
}

/// Visit every block in `spans` that delivery renders, in delivery order:
/// variants selected as [`select`] selects them, and a body descended into
/// only where [`body_is_fragment`] says the renderer walks it, read as
/// [`body`] reads it — and no deeper than it renders one (`nesting` bodies
/// down already).
fn received_blocks(
    lines: &[&str],
    range: (usize, usize),
    spans: &Spans,
    facts: &BTreeMap<String, String>,
    nesting: usize,
    visit: &mut dyn FnMut(&Block),
) {
    let keep = select(lines, range.0, spans, facts);
    for ((_, _, item), kept) in spans.iter().zip(keep) {
        let Item::Block(b) = item else { continue };
        if !kept {
            continue;
        }
        visit(b);
        if !body_is_fragment(b) || nesting >= NESTING_CAP {
            continue;
        }
        match container_body(b, lines) {
            Some(range) => {
                received_blocks(lines, range, &layout_spans(b), facts, nesting + 1, visit)
            }
            None => {
                let body = body_source(b, lines);
                let body: Vec<&str> = body.iter().map(String::as_str).collect();
                let nodes = read_again(&body);
                let spans = node_spans(&nodes);
                received_blocks(&body, (0, body.len()), &spans, facts, nesting + 1, visit);
            }
        }
    }
}

/// Whether delivery silences this rule (S24), marking the override that
/// names it found. A document's own rules answer its local overrides; an
/// included document's answer the overrides of the documents above it that
/// do not have the target. A guardrail, or a rule stronger than its
/// overrider, is found and kept — only a parse of the declaring document
/// could refuse it, and an included target is not in that document.
fn overridden(b: &Block, w: &mut Walk) -> bool {
    let reg = registry();
    if !reg.is_rule(&b.kind) {
        return false;
    }
    let Some((kind, name)) = doc::identity_of(b) else {
        return false;
    };
    let id = format!("{kind}/{name}");
    let depth = w.depth;
    for o in w.overrides.iter_mut() {
        if o.found || o.target != id {
            continue;
        }
        let answers = if o.depth == depth {
            o.local
        } else {
            o.depth < depth && !o.local
        };
        if !answers {
            continue;
        }
        o.found = true;
        if kind == "guardrail" || reg.strength(&kind).unwrap_or(0) > o.strength {
            // Kept; another override naming it may still be the one to
            // silence it, as the reference reads them.
            continue;
        }
        w.overridden.push(id);
        return true;
    }
    false
}

/// Render `lines[from..to]`, whose regions `spans` lists. `top` is a
/// document's own body, as opposed to a body inside one. The fold refuses
/// machinery anywhere but a document's top level, so a machinery block met
/// in a body is one only a body read again found — in an alert's unquoted
/// lines, which parse reads as text. It is delivered as the text parse says
/// it is: acknowledging it would claim a configuration nothing folded, and
/// dropping it would lose the author's words.
fn render(
    lines: &[&str],
    (from, to): (usize, usize),
    spans: &Spans,
    w: &mut Walk,
    top: bool,
) -> Vec<String> {
    let keep = select(lines, from, spans, &w.facts);

    let mut out = Vec::new();
    let mut prose = Prose::default();
    let mut li = from;
    let mut si = 0;
    while li < to {
        while spans.get(si).is_some_and(|s| s.0 < li) {
            si += 1;
        }
        let Some((start, end, item)) = spans.get(si).filter(|s| s.0 == li) else {
            out.push(prose.line(lines[li], w, true));
            li += 1;
            continue;
        };
        // A variant is accounted for where delivery meets it (S7), in
        // pre-order: a kept one before the variants its body holds, and a
        // dropped one's never, as its body is never walked.
        if let Item::Block(b) = item
            && matches!(b.kind.as_str(), "when" | "unless" | "otherwise")
        {
            w.variant(b, keep[si]);
        }
        match item {
            Item::Note => {}
            Item::Inert => {
                // Its body, raw — nothing in it is lifted — with the fences
                // removed, and its author notes, which are never delivered
                // (S9), with them.
                let body = lines.get(start + 1..*end).unwrap_or_default();
                for l in without_notes(body) {
                    out.push(prose.line(l, w, true));
                }
            }
            // A dropped variant leaves nothing.
            Item::Block(_) if !keep[si] => {}
            Item::Set(_) | Item::Block(_) if !top && machinery(item) => {
                for l in &lines[*start..=*end] {
                    out.push(prose.line(l, w, true));
                }
            }
            Item::Set(members) => {
                out.extend(doc::deliver_set_lines(members).iter().map(|l| w.sub(l)))
            }
            // An overridden rule is not delivered, nor its reason, which its
            // region covers.
            Item::Block(b) if b.disposition == Disposition::Prose && overridden(b, w) => {}
            Item::Block(b) => out.extend(block_lines(b, lines, w)),
        }
        li = end + 1;
        si += 1;
    }
    out
}

/// Source lines becoming delivered ones, in order. Fenced code is delivered
/// as written: nothing in it is a reference or a placeholder (§3.4 rule 4),
/// and a fence closes on a delimiter of its own length. Any other line has
/// its inline references degraded (prose; quoted material keeps them) and
/// then `${}` substituted, so a value is never read as a reference. Whether
/// a line is code is decided here, from the source, before any label is
/// glued onto it.
#[derive(Default)]
struct Prose {
    in_code: Option<usize>,
}

impl Prose {
    fn line(&mut self, l: &str, w: &mut Walk, degrade: bool) -> String {
        match (self.in_code, doc::code_fence_len(l)) {
            (Some(open), Some(n)) if n == open => self.in_code = None,
            (None, Some(n)) => self.in_code = Some(n),
            (None, None) if degrade => return w.sub(&degrade_inline(l)),
            (None, None) => return w.sub(l),
            _ => {}
        }
        l.to_string()
    }

    /// Quoted lines (an example, a `verbatim` body): no reference degraded,
    /// `${}` substituted outside their fenced code.
    fn quoted(lines: Vec<String>, w: &mut Walk) -> Vec<String> {
        let mut p = Prose::default();
        lines.iter().map(|l| p.line(l, w, false)).collect()
    }
}

/// `lines` without their author notes (S9): a column-0 `<!--` outside fenced
/// code through the first line holding `-->`, as parse reads one. For the
/// bodies parse keeps no notes for — an inert block's, a glossary's.
fn without_notes<'l>(lines: &[&'l str]) -> Vec<&'l str> {
    let mut out = Vec::with_capacity(lines.len());
    let mut in_code = None::<usize>;
    let mut k = 0;
    while k < lines.len() {
        let l = lines[k];
        match (in_code, doc::code_fence_len(l)) {
            (Some(open), Some(n)) if n == open => in_code = None,
            (None, Some(n)) => in_code = Some(n),
            (None, None) if doc::note_opens(l) => {
                k = doc::note_end(lines, k, lines.len()).unwrap_or(lines.len() - 1) + 1;
                continue;
            }
            _ => {}
        }
        out.push(l);
        k += 1;
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
        Disposition::Machinery => doc::machinery_ack(b).iter().map(|l| w.sub(l)).collect(),
        Disposition::Structural if b.kind == "include" => doc::deliver_include(b, w),
        // A kept variant (dropped ones never get here) delivers its body
        // unwrapped, with no fence and no label; so does any other
        // structural kind with a body. A `param` delivers nothing.
        Disposition::Structural if body_is_fragment(b) => body(b, lines, w),
        Disposition::Structural => Vec::new(),
        Disposition::Prose => prose_lines(b, lines, w),
    }
}

/// The source lines of a block's body, its reason excluded (TS
/// `regionBodyLines`): a container's between its fences, a section's under
/// its heading, an alert's quoted lines unquoted, a keyword's text after its
/// label and the lines of its paragraph. A leaf has none.
fn body_source(b: &Block, lines: &[&str]) -> Vec<String> {
    let start = b.region.0;
    let own_end = own_end(b, lines);
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

/// The last line of a block's own region: the region covers the reason that
/// follows a rule, and only blank lines separate the two, so it is the last
/// non-blank line before the reason.
fn own_end(b: &Block, lines: &[&str]) -> usize {
    let (start, end) = b.region;
    match b.reason {
        Some((reason, _)) => {
            let mut j = reason;
            while j > start + 1 && lines[j - 1].trim().is_empty() {
                j -= 1;
            }
            j - 1
        }
        None => end,
    }
}

/// A container's body lines, `[from, to)`, between its fences — where parse
/// laid out what is in it. `None` for any other form.
fn container_body(b: &Block, lines: &[&str]) -> Option<(usize, usize)> {
    (b.form == Form::Container).then(|| (b.region.0 + 1, own_end(b, lines)))
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
    // A `verbatim` body is quoted whole; any other body is a fragment. The
    // source is copied only where it is delivered as written.
    let src = || body_source(b, lines);
    let inner = |w: &mut Walk| {
        if body_is_fragment(b) {
            body(b, lines, w)
        } else {
            Prose::quoted(src(), w)
        }
    };
    // An attribute the label shows is delivered text like any other, so its
    // `${}` is substituted — before it is escaped into a tag.
    let title = b.attrs.get("title").map(|t| w.sub(t));
    let title = title.as_ref();
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
            wrap(
                w.style,
                &head,
                Prose::quoted(note_free(b, src()), w),
                "",
                true,
            )
        }
        // S17: the body is prose (a rule in it is a rule); the schema is
        // the host's and never delivered.
        "output" => {
            let mut suffix = String::new();
            let mut attrs = String::new();
            if let Some(f) = b.attrs.get("format").map(|f| w.sub(f)) {
                suffix.push_str(&format!(" ({f})"));
                attrs.push_str(&format!(" format=\"{}\"", xml_attr(&f)));
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
            let body = inner(w);
            wrap(w.style, &head, body, "", true)
        }
        // context, form, tool and glossary deliver the same in every style.
        "context" => {
            let t = title.map(|t| format!(" title=\"{t}\"")).unwrap_or_default();
            let mut out = vec![format!("<reference{t}>")];
            out.extend(inner(w));
            out.push("</reference>".into());
            out
        }
        "form" => doc::form_lines(b, &w.decls)
            .iter()
            .map(|l| w.sub(l))
            .collect(),
        "tool" => {
            let mut out = vec![w.sub(&doc::tool_head(b))];
            out.extend(inner(w));
            out
        }
        // One line per term, from the parsed body less its author notes
        // (S9), which the definition-list reader would take for a term.
        "glossary" => {
            let body: Vec<&str> = b.body.split('\n').collect();
            doc::deflist_entries(&without_notes(&body).join("\n"))
                .into_iter()
                .map(|(term, def)| w.sub(&format!("**{term}** — {}", degrade_inline(&def))))
                .collect()
        }
        k if registry().label(k, false).is_some() => {
            // A rule, or an admonition: its label (the negated one for
            // `SHOULD NOT`, the canonical one for an alias — S8), its
            // condition (S15), never its name (S12).
            let label = registry().label(k, flag(b, "not")).unwrap_or(k).to_string();
            let (suffix, attrs) = match b.attrs.get("if").map(|c| w.sub(c)) {
                Some(c) => (format!(" (if {c})"), format!(" if=\"{}\"", xml_attr(&c))),
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
            let body = inner(w);
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
                let text: Vec<String> = w
                    .sub(&degrade_inline(because))
                    .split('\n')
                    .map(str::to_string)
                    .collect();
                out.extend(wrap(w.style, &head, text, lead, false));
            }
            out
        }
        // A prose kind the registry gives no delivery shape delivers its
        // body as written.
        _ => Prose::quoted(src(), w),
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

    /// The folded document with `params`, `facts` and `includes` (id → text).
    fn fold_ctx(
        text: &str,
        params: &[(&str, &str)],
        facts: &[(&str, &str)],
        includes: &[(&str, &str)],
    ) -> doc::Extraction {
        let map = |kv: &[(&str, &str)]| -> BTreeMap<String, String> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let includes = map(includes);
        let resolve = |id: &str| includes.get(id).cloned();
        fold_full(
            &parse(text).unwrap(),
            &all_families(),
            &map(params),
            &map(facts),
            &resolve,
        )
        .unwrap()
    }

    /// The delivered text under `facts`.
    fn with_facts(text: &str, facts: &[(&str, &str)]) -> String {
        fold_ctx(text, &[], facts, &[]).cleaned
    }

    /// The delivered text with `params` given.
    fn with_params(text: &str, params: &[(&str, &str)]) -> String {
        fold_ctx(text, params, &[], &[]).cleaned
    }

    /// S10, §5.2 rule 3: an unknown key keeps a `when` and an `unless`
    /// alike; an `unless` is dropped only when every key is known and
    /// matches; `verbatim` is no condition.
    #[test]
    fn when_and_unless_keep_on_an_unknown_key() {
        let doc = ":::when{env=\"prod\" tier=\"pro\"}\nW.\n:::\n\nx\n\n\
                   :::unless{env=\"prod\" tier=\"pro\"}\nU.\n:::\n";
        assert_eq!(with_facts(doc, &[("env", "prod")]), "W.\n\nx\n\nU.\n");
        assert_eq!(
            with_facts(doc, &[("env", "prod"), ("tier", "pro")]),
            "W.\n\nx\n"
        );
        assert_eq!(
            with_facts(doc, &[("env", "dev"), ("tier", "pro")]),
            "x\n\nU.\n"
        );
        assert_eq!(
            with_facts(
                ":::unless{env=\"prod\" verbatim}\nU.\n:::\n\nx\n",
                &[("env", "prod")]
            ),
            "x\n"
        );
    }

    /// S10: an `otherwise` belongs to the `when`/`unless` run right before
    /// it; blank lines and author notes do not break the run, prose does,
    /// and an `otherwise` with no run is kept whatever its attributes say.
    #[test]
    fn otherwise_follows_its_group() {
        let noted = ":::when{env=\"prod\"}\nP.\n:::\n\n<!-- aside -->\n\n\
                     :::otherwise\nO.\n:::\n";
        assert_eq!(with_facts(noted, &[("env", "prod")]), "P.\n");
        assert_eq!(with_facts(noted, &[("env", "dev")]), "O.\n");
        // A dropped `when` and a kept `unless` are one group: kept.
        assert_eq!(
            with_facts(
                ":::when{env=\"prod\"}\nP.\n:::\n:::unless{env=\"prod\"}\nU.\n:::\n\n\
                 :::otherwise\nO.\n:::\n",
                &[("env", "dev")]
            ),
            "U.\n"
        );
        // Prose between them ends the run: this `otherwise` is an orphan.
        assert_eq!(
            with_facts(
                ":::when{env=\"prod\"}\nP.\n:::\n\nBetween.\n\n:::otherwise\nO.\n:::\n",
                &[("env", "prod")]
            ),
            "P.\n\nBetween.\n\nO.\n"
        );
        // So does any other block.
        assert_eq!(
            with_facts(
                ":::when{env=\"prod\"}\nP.\n:::\n\nMUST: m.\n\n:::otherwise\nO.\n:::\n",
                &[("env", "prod")]
            ),
            "P.\n\n**MUST:** m.\n\nO.\n"
        );
    }

    /// S10: an orphan `otherwise` is always kept, its attributes ignored,
    /// and an `otherwise` closes its group, so a second one is an orphan.
    #[test]
    fn an_orphan_otherwise_is_kept() {
        assert_eq!(
            with_facts(
                "Intro.\n\n:::otherwise{env=\"prod\"}\nO.\n:::\n",
                &[("env", "dev")]
            ),
            "Intro.\n\nO.\n"
        );
        assert_eq!(
            with_facts(
                ":::when{env=\"prod\"}\nP.\n:::\n:::otherwise\nO1.\n:::\n:::otherwise\nO2.\n:::\n",
                &[("env", "prod")]
            ),
            "P.\nO2.\n"
        );
    }

    /// §5.2 rule 4: a variant nested in a dropped parent is never
    /// evaluated, and a group is tracked per parent — inside a kept body
    /// too, apart from the parent's own siblings.
    #[test]
    fn nested_variants_select_inside_a_kept_parent_only() {
        let nested = "::::when{a=\"1\"}\n:::when{b=\"2\"}\nB.\n:::\n:::otherwise\nNot b.\n:::\n::::\n\
                      :::otherwise\nNot a.\n:::\n";
        assert_eq!(with_facts(nested, &[("a", "1"), ("b", "3")]), "Not b.\n");
        assert_eq!(with_facts(nested, &[("a", "1"), ("b", "2")]), "B.\n");
        // The parent dropped, its orphan `otherwise` is never reached.
        assert_eq!(with_facts(nested, &[("a", "2")]), "Not a.\n");
        assert_eq!(
            with_facts(
                "::::when{a=\"1\"}\n:::otherwise\nInner.\n:::\n::::\n",
                &[("a", "2")]
            ),
            ""
        );
    }

    /// S18: each declared type admits exactly its values; a string, a list
    /// and an untyped parameter admit anything.
    #[test]
    fn each_parameter_type_admits_its_values() {
        let typed = |ty: &str, values: Option<&str>| {
            let mut d = BTreeMap::from([("type".to_string(), ty.to_string())]);
            if let Some(v) = values {
                d.insert("values".into(), v.into());
            }
            d
        };
        // A type, its `values`, what it admits and what it refuses.
        type Case<'a> = (&'a str, Option<&'a str>, &'a [&'a str], &'a [&'a str]);
        let cases: &[Case] = &[
            (
                "number",
                None,
                &["5", "-5", "1.5", "-0.25"],
                &["1.", "+1", "1e3", "", ".5", "lots", "-"],
            ),
            ("boolean", None, &["true", "false"], &["yes", "True", ""]),
            (
                "enum",
                Some("free, pro"),
                &["free", "pro"],
                &["enterprise", "", " free", "free, pro"],
            ),
            (
                "url",
                None,
                &["https://x.test/a?b", "http://x"],
                &["ftp://x", "https://", "https://a b", "x"],
            ),
            (
                "duration",
                None,
                &["15m", "250ms", "2w", "1d"],
                &["m", "15", "15min", "1.5h", "-1s"],
            ),
        ];
        for (ty, values, fit, fail) in cases {
            let d = typed(ty, *values);
            for v in *fit {
                assert!(doc::value_fits(Some(&d), v), "{ty} admits {v:?}");
            }
            for v in *fail {
                assert!(!doc::value_fits(Some(&d), v), "{ty} refuses {v:?}");
            }
        }
        // `enum` with no `values` admits nothing.
        assert!(!doc::value_fits(Some(&typed("enum", None)), "x"));
        for ty in ["string", "list"] {
            assert!(doc::value_fits(Some(&typed(ty, None)), "any thing, at all"));
        }
        assert!(doc::value_fits(None, "undeclared"));
        assert!(doc::value_fits(Some(&BTreeMap::new()), ""));
    }

    /// S18: a given value that fits is used; one that fails is unresolved
    /// and never replaced by the default; with none given, a default that
    /// fails is no value either; `example` is never a value, and a form
    /// never shows it.
    #[test]
    fn a_parameter_value_resolves_only_when_it_fits() {
        let doc = ":::param[]\n\
                   | name | type   | default | example |\n\
                   |------|--------|---------|---------|\n\
                   | n    | number | 5       | 7       |\n\
                   | late | number | soon    |         |\n\
                   | e    | string |         | sample  |\n\
                   :::\n\n\
                   :::form\nn: ${n}, e: ${e}\n:::\n\n\
                   n=${n} late=${late} e=${e}\n";
        assert_eq!(
            with_params(doc, &[]),
            "**Inputs to collect**\n- **n** — default: 5\n- **e**\n\nn=5 late=${late} e=${e}\n"
        );
        assert!(with_params(doc, &[("n", "12.5")]).ends_with("n=12.5 late=${late} e=${e}\n"));
        // A given value that fails its type does not fall back to the
        // default the caller did not ask for.
        assert!(with_params(doc, &[("n", "lots")]).ends_with("n=${n} late=${late} e=${e}\n"));
        // A YAML default is its text, so a number checks as one.
        assert_eq!(
            with_params(
                "---\nparameters:\n  - {name: n, type: number, default: 500}\n---\n${n}\n",
                &[]
            ),
            "500\n"
        );
    }

    /// §5.2: only a value that fits is a fact — an enum value outside its
    /// `values` leaves the key unknown, so both of its `when`s are kept.
    #[test]
    fn only_a_fitting_value_is_a_when_fact() {
        let doc = "::param{name=tier type=enum values=\"free, pro\"}\n\n\
                   :::when{tier=\"free\"}\nFree.\n:::\n\n:::when{tier=\"pro\"}\nPro.\n:::\n";
        assert_eq!(with_params(doc, &[("tier", "pro")]), "Pro.\n");
        assert_eq!(
            with_params(doc, &[("tier", "enterprise")]),
            "Free.\n\nPro.\n"
        );
    }

    /// A front-matter `values` list is listed the way a `param` attribute's
    /// comma-separated value is, and checked the same.
    #[test]
    fn a_yaml_values_list_reads_as_a_comma_separated_value() {
        let doc = "---\nparameters:\n  - {name: d, type: enum, values: [quick, thorough]}\n---\n\
                   :::form\n${d}\n:::\n\nDepth ${d}.\n";
        assert_eq!(
            with_params(doc, &[("d", "thorough")]),
            "**Inputs to collect**\n- **d** — one of: quick, thorough\n\nDepth thorough.\n"
        );
    }

    /// §3.4 rule 4: `${x}` in fenced code is the code's own syntax and is
    /// not substituted — a fence closes on its own length — while an inline
    /// code span is (the corpus pins `` `${default_branch}` ``).
    #[test]
    fn placeholders_in_fenced_code_are_untouched() {
        let doc = "::param{name=x default=1}\n\nUse ${x}.\n\n```sh\necho ${x}\n```\n\n\
                   ````md\n```\n${x}\n```\n````\n\nRun `${x}` now.\n";
        assert_eq!(
            with_params(doc, &[]),
            "Use 1.\n\n```sh\necho ${x}\n```\n\n````md\n```\n${x}\n```\n````\n\nRun `1` now.\n"
        );
    }

    /// S24: a local target is not delivered, nor its reason, and is
    /// recorded; a target found nowhere is recorded as unfound.
    #[test]
    fn an_override_silences_its_local_target_and_its_reason() {
        let ex = fold_ctx(
            "SHOULD[brevity]: be brief.\nBECAUSE: people skim.\n\nMUST: answer.\n\n\
             :::must{name=full overrides=\"should/brevity, may/nowhere\"}\nGive every step.\n:::\n",
            &[],
            &[],
            &[],
        );
        assert_eq!(
            ex.cleaned,
            "**MUST:** answer.\n\n**MUST:** Give every step.\n"
        );
        assert_eq!(ex.overridden, ["should/brevity"]);
        assert_eq!(ex.unfound_overrides, ["may/nowhere"]);
    }

    /// S24: in an included document a guardrail, or a rule stronger than
    /// its overrider, is found and kept; a weaker one is silenced.
    #[test]
    fn an_included_guardrail_or_stronger_rule_is_found_and_kept() {
        let house = "GUARDRAIL[no-pii]: never leak.\n\nMUST[tone]: be warm.\n\nSHOULD[brevity]: be brief.\n";
        let ex = fold_ctx(
            "::include{id=\"house\"}\n\n\
             :::should{name=s overrides=\"guardrail/no-pii, must/tone, should/brevity\"}\nKeep it short.\n:::\n",
            &[],
            &[],
            &[("house", house)],
        );
        assert_eq!(
            ex.cleaned,
            "**GUARDRAIL:** never leak.\n\n**MUST:** be warm.\n\n**SHOULD:** Keep it short.\n"
        );
        assert_eq!(ex.overridden, ["should/brevity"]);
        assert!(
            ex.unfound_overrides.is_empty(),
            "{:?}",
            ex.unfound_overrides
        );
    }

    /// S24: a target is looked up locally first — a local target is answered
    /// only locally — then in the includes in include order, depth first,
    /// and one found answers nothing more.
    #[test]
    fn an_override_looks_locally_then_in_include_order_depth_first() {
        let target = ":::must{name=m overrides=\"should/b\"}\nM.\n:::\n";
        let ex = fold_ctx(
            &format!("::include{{id=\"a\"}}\n\nSHOULD[b]: local.\n\n{target}"),
            &[],
            &[],
            &[("a", "SHOULD[b]: included.\n")],
        );
        assert_eq!(ex.cleaned, "**SHOULD:** included.\n\n**MUST:** M.\n");
        // `a` includes `c`; both `c` and `b` have the target: `c`, met first,
        // is silenced, and the override comes back up found.
        let ex = fold_ctx(
            &format!("::include{{id=\"a\"}}\n\n::include{{id=\"b\"}}\n\n{target}"),
            &[],
            &[],
            &[
                ("a", "A.\n\n::include{id=\"c\"}\n"),
                ("c", "SHOULD[b]: in c.\n"),
                ("b", "SHOULD[b]: in b.\n"),
            ],
        );
        assert_eq!(ex.cleaned, "A.\n\n**SHOULD:** in b.\n\n**MUST:** M.\n");
        assert_eq!(ex.overridden, ["should/b"]);
        // An include's own overrides apply to it and its includes only.
        let ex = fold_ctx(
            "::include{id=\"a\"}\n\n::include{id=\"b\"}\n",
            &[],
            &[],
            &[
                (
                    "a",
                    "::include{id=\"c\"}\n\n:::must{name=m overrides=\"should/b\"}\nM.\n:::\n",
                ),
                ("c", "SHOULD[b]: in c.\n"),
                ("b", "SHOULD[b]: in b.\n"),
            ],
        );
        assert_eq!(ex.cleaned, "**MUST:** M.\n\n**SHOULD:** in b.\n");
        // ...even when it found nothing there: it does not reach what a
        // later sibling includes.
        let ex = fold_ctx(
            "::include{id=\"a\"}\n\n::include{id=\"b\"}\n",
            &[],
            &[],
            &[
                ("a", ":::must{name=m overrides=\"should/b\"}\nM.\n:::\n"),
                ("b", "::include{id=\"d\"}\n"),
                ("d", "SHOULD[b]: in d.\n"),
            ],
        );
        assert_eq!(ex.cleaned, "**MUST:** M.\n\n**SHOULD:** in d.\n");
        assert!(ex.overridden.is_empty());
    }

    /// §3.5 removes variants before it applies overrides: an `overrides`
    /// declared inside a dropped variant — or in a variant nested in one —
    /// does not apply.
    #[test]
    fn an_override_in_a_dropped_variant_does_not_apply() {
        let doc = "SHOULD[b]: brief.\n\n::::when{env=\"prod\"}\n\
                   :::must{name=m overrides=\"should/b\"}\nM.\n:::\n::::\n";
        let ex = fold_ctx(doc, &[], &[("env", "dev")], &[]);
        assert_eq!(ex.cleaned, "**SHOULD:** brief.\n");
        assert!(ex.overridden.is_empty() && ex.unfound_overrides.is_empty());
        assert_eq!(with_facts(doc, &[("env", "prod")]), "**MUST:** M.\n");
        let nested = "SHOULD[b]: brief.\n\n:::::when{a=\"1\"}\n::::otherwise\n\
                      :::must{name=m overrides=\"should/b\"}\nM.\n:::\n::::\n:::::\n";
        assert_eq!(with_facts(nested, &[("a", "2")]), "**SHOULD:** brief.\n");
        assert_eq!(with_facts(nested, &[("a", "1")]), "**MUST:** M.\n");
    }

    /// The skill catalogue is not the delivery: rendering a skill's body
    /// neither applies an override nor marks one found, so the accounting
    /// is the delivered text's.
    #[test]
    fn a_skill_body_leaves_overrides_alone() {
        let ex = fold_ctx(
            ":::!skill{name=tone}\nSHOULD[b]: be brief.\n:::\n\n\
             :::must{name=m overrides=\"should/b\"}\nM.\n:::\n",
            &[],
            &[],
            &[],
        );
        assert_eq!(ex.skills[0].body, "**SHOULD:** be brief.");
        assert!(ex.overridden.is_empty());
        assert_eq!(ex.unfound_overrides, ["should/b"]);
    }

    /// Nor does a catalogue body spend the delivery's include budget, or
    /// record as overridden a rule an include in it silenced: an include in
    /// a skill is inlined there, and accounted for nowhere.
    #[test]
    fn a_skill_body_accounts_for_nothing_it_includes() {
        let sized = |n: usize| format!("{}\n", "x".repeat(n - 1));
        let ex = fold_ctx(
            ":::!skill{name=s}\n::include{id=\"big\"}\n:::\n\n::include{id=\"one\"}\n",
            &[],
            &[],
            &[("big", &sized(1 << 20)), ("one", "MUST: one.\n")],
        );
        assert!(
            ex.skills[0].body.starts_with("xxx"),
            "inlined in the catalogue"
        );
        assert!(
            ex.cleaned.ends_with("\n\n**MUST:** one.\n"),
            "the delivery's include is not starved: {:?}",
            &ex.cleaned[ex.cleaned.len().saturating_sub(80)..]
        );
        let ex = fold_ctx(
            ":::!skill{name=s}\nA.\n\n::include{id=\"i\"}\n:::\n",
            &[],
            &[],
            &[(
                "i",
                "SHOULD[b]: x.\n\n:::must{name=m overrides=\"should/b\"}\nM.\n:::\n",
            )],
        );
        assert_eq!(ex.skills[0].body, "A.\n\n**MUST:** M.");
        assert!(ex.overridden.is_empty(), "{:?}", ex.overridden);
    }

    /// §3.5 removes variants before overrides on both sides: a target this
    /// document has only inside a dropped variant is not local, so the
    /// override finds it in what the document includes.
    #[test]
    fn a_target_only_in_a_dropped_variant_is_looked_for_in_includes() {
        let ex = fold_ctx(
            ":::when{env=\"x\"}\nSHOULD[b]: x.\n:::\n\n::include{id=\"a\"}\n\n\
             :::must{name=m overrides=\"should/b\"}\nM.\n:::\n",
            &[],
            &[("env", "y")],
            &[("a", "SHOULD[b]: inc.\n")],
        );
        assert_eq!(ex.cleaned, "**MUST:** M.\n");
        assert_eq!(ex.overridden, ["should/b"]);
        assert!(
            ex.unfound_overrides.is_empty(),
            "{:?}",
            ex.unfound_overrides
        );
    }

    /// S24: a guardrail in an included document is found and kept, even by
    /// an overrider as strong as it — a guardrail is never overridden.
    #[test]
    fn an_included_guardrail_is_kept_by_an_equal_overrider() {
        let ex = fold_ctx(
            "::include{id=\"h\"}\n\n:::guardrail{name=g overrides=\"guardrail/x\"}\nG.\n:::\n",
            &[],
            &[],
            &[("h", "GUARDRAIL[x]: keep.\n")],
        );
        assert_eq!(ex.cleaned, "**GUARDRAIL:** keep.\n\n**GUARDRAIL:** G.\n");
        assert!(ex.overridden.is_empty(), "{:?}", ex.overridden);
        assert!(
            ex.unfound_overrides.is_empty(),
            "{:?}",
            ex.unfound_overrides
        );
    }

    /// Includes are capped at 1 MiB inlined over the whole delivery: one
    /// include at the cap is inlined, the next is not, and neither is one
    /// over it on its own.
    #[test]
    fn includes_are_capped_at_one_mebibyte_together() {
        let sized = |n: usize| format!("{}\n", "x".repeat(n - 1));
        let note = "> _(included instruction not available)_";
        let ex = fold_ctx(
            "::include{id=\"big\"}\n\n::include{id=\"one\"}\n",
            &[],
            &[],
            &[("big", &sized(1 << 20)), ("one", "y\n")],
        );
        assert!(
            ex.cleaned.starts_with("xxx"),
            "the include at the cap is inlined"
        );
        assert!(
            ex.cleaned.ends_with(&format!("\n\n{note}\n")),
            "the next is not"
        );
        // The bytes a nested include inlines count toward the same cap.
        let outer = "::include{id=\"big\"}\n";
        let ex = fold_ctx(
            "::include{id=\"outer\"}\n\n::include{id=\"one\"}\n",
            &[],
            &[],
            &[
                ("outer", outer),
                ("big", &sized((1 << 20) - outer.len())),
                ("one", "y\n"),
            ],
        );
        assert!(
            ex.cleaned.starts_with("xxx"),
            "the nested include is inlined"
        );
        assert!(
            ex.cleaned.ends_with(&format!("\n\n{note}\n")),
            "the next is not"
        );
        let ex = fold_ctx(
            "::include{id=\"over\"}\n",
            &[],
            &[],
            &[("over", &sized((1 << 20) + 1))],
        );
        assert_eq!(ex.cleaned, format!("{note}\n"));
    }

    /// An included document that does not parse — malformed end matter
    /// included — degrades to the not-available note.
    #[test]
    fn an_include_that_does_not_parse_is_not_available() {
        assert_eq!(
            delivered_with(
                "A.\n\n::include{id=\"house\"}\n",
                "house",
                "MUST: x.\n\n---\nowners: [\n---\n"
            ),
            "A.\n\n> _(included instruction not available)_\n"
        );
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

    /// Notes (S9) are stripped from a glossary and from an inert body too,
    /// though parse keeps none for either — but a `<!--` in fenced code is
    /// code.
    #[test]
    fn notes_are_stripped_from_glossaries_and_inert_bodies() {
        assert_eq!(
            delivered(
                ":::glossary\nTerm\n: Def one.\n<!-- secret glossary note -->\n\nOther\n: Def two.\n:::\n\n\
                 :::aside\nA.\n<!-- secret inert note -->\n```html\n<!-- markup -->\n```\n:::\n"
            ),
            "**Term** — Def one.\n**Other** — Def two.\n\nA.\n```html\n<!-- markup -->\n```\n"
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

    /// The refusals of a fold, as `(line, code)`.
    fn refused(
        text: &str,
        facts: &[(&str, &str)],
        includes: &[(&str, &str)],
    ) -> Vec<(u32, String)> {
        let map = |kv: &[(&str, &str)]| -> BTreeMap<String, String> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let includes = map(includes);
        let resolve = |id: &str| includes.get(id).cloned();
        fold_full(
            &parse(text).unwrap(),
            &all_families(),
            &BTreeMap::new(),
            &map(facts),
            &resolve,
        )
        .unwrap_err()
        .into_iter()
        .map(|r| (r.line.unwrap_or(0), r.code.to_string()))
        .collect()
    }

    /// Configuration folds from the top level only, so machinery anywhere
    /// else is refused rather than loaded as nothing: in a kept `when` or a
    /// dropped one, a `context`, a skill — a set once. At the top level it
    /// folds and is acknowledged.
    #[test]
    fn machinery_folds_from_the_top_level_only() {
        let wf = ":::!workflow{name=w}\nsteps: { s: { kind: manual } }\n:::";
        let in_when = format!("::::when{{env=\"prod\"}}\nKept.\n{wf}\n::::\n");
        let nested = vec![(3, "nested-machinery".to_string())];
        assert_eq!(refused(&in_when, &[("env", "prod")], &[]), nested);
        assert_eq!(refused(&in_when, &[("env", "dev")], &[]), nested);
        assert_eq!(
            refused(
                ":::context\nRead this.\n::!mcp{name=x url=\"https://x.test/mcp\"}\n:::\n",
                &[],
                &[]
            ),
            nested
        );
        assert_eq!(
            refused(
                ":::!skill{name=s}\nUse it.\n::!human{name=h}\n:::\n",
                &[],
                &[]
            ),
            nested
        );
        assert_eq!(
            refused(
                "::::when{env=\"prod\"}\nX.\n:::!human[]\n| name |\n|------|\n| a |\n| b |\n:::\n::::\n",
                &[],
                &[]
            ),
            vec![(3, "nested-machinery".to_string())],
            "a set is one refusal"
        );
        let ex = fold(&parse(wf).unwrap(), &all_families()).unwrap();
        assert_eq!(ex.workflows.len(), 1);
        assert_eq!(
            ex.cleaned,
            "[workflow \"w\" is loaded and runs autonomously]\n"
        );
    }

    /// An included document folds no configuration, so one carrying
    /// machinery is not available: the model is never told of a workflow
    /// that was not loaded. Prose alone is inlined.
    #[test]
    fn an_included_document_with_machinery_is_not_available() {
        let ex = fold_ctx(
            "Top.\n\n::include{id=\"i\"}\n",
            &[],
            &[],
            &[(
                "i",
                ":::!workflow{name=w}\nsteps: { s: { kind: manual } }\n:::\n",
            )],
        );
        assert_eq!(
            ex.cleaned,
            "Top.\n\n> _(included instruction not available)_\n"
        );
        assert!(ex.workflows.is_empty() && ex.config.is_empty());
        assert_eq!(
            delivered_with("::include{id=\"i\"}\n", "i", "MUST: be kind.\n"),
            "**MUST:** be kind.\n"
        );
    }

    /// A machinery opener only delivery's second reading of a body finds —
    /// in an alert's unquoted lines, which parse reads as text — is
    /// delivered as the text it is: no acknowledgement of what never folded,
    /// and none of the author's words lost.
    #[test]
    fn machinery_only_a_body_reading_finds_is_text() {
        assert_eq!(
            delivered("> [!NOTE]\n> Ask first.\n> ::!human{name=h}\n"),
            "**NOTE:** Ask first.\n::!human{name=h}\n"
        );
    }

    /// §5.2 include rule 1: an include is resolved with its OWN parameters.
    /// The includer's value — a default it declares, or one it was given —
    /// is no fact of the include's, so its variant and its `${}` agree.
    #[test]
    fn an_include_selects_and_substitutes_with_its_own_parameters() {
        let inc = "::param{name=tier type=enum values=\"free, pro\" default=free}\n\n\
                   :::when{tier=\"free\"}\nFree ${tier}.\n:::\n\n:::when{tier=\"pro\"}\nPro ${tier}.\n:::\n";
        let declared = fold_ctx(
            "::param{name=tier default=pro}\n\n::include{id=\"i\"}\n",
            &[],
            &[],
            &[("i", inc)],
        );
        assert_eq!(declared.cleaned, "Free free.\n");
        let given = fold_ctx(
            "::include{id=\"i\"}\n",
            &[("tier", "enterprise")],
            &[],
            &[("i", inc)],
        );
        assert_eq!(given.cleaned, "Free free.\n");
        // Nor through an include in between, whatever either declares.
        let nested = fold_ctx(
            "::param{name=tier default=pro}\n\n::include{id=\"a\"}\n",
            &[],
            &[],
            &[
                (
                    "a",
                    "::param{name=tier default=pro}\n\n::include{id=\"i\"}\n",
                ),
                ("i", inc),
            ],
        );
        assert_eq!(nested.cleaned, "Free free.\n");
        // A runtime fact still reaches it.
        let ex = fold_ctx(
            "::include{id=\"i\"}\n",
            &[],
            &[("env", "prod")],
            &[(
                "i",
                ":::when{env=\"prod\"}\nProd.\n:::\n:::otherwise\nElse.\n:::\n",
            )],
        );
        assert_eq!(ex.cleaned, "Prod.\n");
    }

    /// An include's lines arrive substituted with its own parameters and
    /// are touched by nothing of the includer's: a placeholder it left stays
    /// (the caller's value never checked against its declaration), and a
    /// value it inserted is not substituted again (§3.5: never re-parsed).
    #[test]
    fn the_includer_never_substitutes_an_includes_lines() {
        let ex = fold_ctx(
            "::include{id=\"i\"}\n",
            &[("n", "lots")],
            &[],
            &[("i", "::param{name=n type=number}\n\nN=${n}\n")],
        );
        assert_eq!(ex.cleaned, "N=${n}\n");
        let ex = fold_ctx(
            "::param{name=b default=SECRET}\n\n::include{id=\"i\"}\n\nB=${b}\n",
            &[],
            &[],
            &[("i", "::param{name=v default=\"${b}\"}\n\nV=${v} and ${b}\n")],
        );
        assert_eq!(ex.cleaned, "V=${b} and ${b}\n\nB=SECRET\n");
    }

    /// Whether a line is code is read from the source: a rule whose body
    /// opens with a fence has its label glued onto that line, and the fence
    /// is still a fence — its `${}` untouched, the prose after it
    /// substituted. In every label style.
    #[test]
    fn a_label_glued_onto_a_fence_keeps_it_a_fence() {
        let body =
            "::param{name=x default=VAL}\n\n:::must\n```sh\necho ${x}\n```\n:::\n\nUse ${x}.\n";
        assert_eq!(
            delivered(body),
            "**MUST:** ```sh\necho ${x}\n```\n\nUse VAL.\n"
        );
        assert_eq!(
            delivered(&format!("---\ndelivery: {{labels: tags}}\n---\n{body}")),
            "<must>```sh\necho ${x}\n```</must>\n\nUse VAL.\n"
        );
    }

    /// §3.5: a substituted value is size-capped — 2000 bytes, cut on a
    /// character boundary — however often its placeholder is written.
    #[test]
    fn a_substituted_value_is_capped() {
        let long = "A".repeat(5000);
        let out = with_params("${x}\n${x}\n", &[("x", &long)]);
        let cap = "A".repeat(doc::PARAM_VALUE_BYTES_CAP);
        assert_eq!(out, format!("{cap}\n{cap}\n"));
        // Two-byte characters: the cut falls between them, not inside one.
        let wide = "é".repeat(1001);
        let out = with_params("${x}\n", &[("x", &wide)]);
        assert_eq!(out, format!("{}\n", "é".repeat(1000)));
    }

    /// `n` `when`s, each inside the last, around one line.
    fn whens(n: usize, inner: &str) -> String {
        let mut doc = String::new();
        for k in 0..n {
            doc.push_str(&format!("{}when{{a=\"1\"}}\n", ":".repeat(n + 3 - k)));
        }
        doc.push_str(inner);
        for k in (0..n).rev() {
            doc.push_str(&format!("{}\n", ":".repeat(n + 3 - k)));
        }
        doc
    }

    /// A container's body is rendered from what parse read it as, never read
    /// again: delivering containers nested as deep as parse admits reads no
    /// body twice, so the work is the document's size, not its size times
    /// its depth.
    #[test]
    fn a_container_body_is_never_read_again() {
        let doc = parse(&whens(doc::NESTING_CAP + 1, "Deep.\n")).unwrap();
        READS.with(|r| r.set(0));
        let ex = fold(&doc, &all_families()).unwrap();
        assert_eq!(ex.cleaned, "Deep.\n");
        assert_eq!(READS.with(|r| r.get()), 0);
        // A keyword's text starts after its label, so it has no lines of its
        // own to index and is read again — once for the overrides it might
        // declare, once for the delivery — however deep it sits.
        let doc = parse(&whens(3, "MUST: deep.\n")).unwrap();
        READS.with(|r| r.set(0));
        assert_eq!(
            fold(&doc, &all_families()).unwrap().cleaned,
            "**MUST:** deep.\n"
        );
        assert_eq!(READS.with(|r| r.get()), 2);
    }

    /// Bodies render at most NESTING_CAP deep — twice what parse lets
    /// containers nest, so only bodies read again reach it: a blockquote in a
    /// blockquote, which parse never sees. Past it the document is refused,
    /// never delivered in part.
    #[test]
    fn bodies_read_again_nest_at_most_the_cap() {
        let alerts = |n: usize| {
            (1..=n)
                .map(|k| format!("{} [!NOTE]\n", ">".repeat(k).replace('>', "> ").trim_end()))
                .collect::<String>()
                + &format!("{} Deep.\n", "> ".repeat(n).trim_end())
        };
        assert!(delivered(&alerts(NESTING_CAP)).ends_with("Deep.\n"));
        assert_eq!(
            refused(&alerts(NESTING_CAP + 1), &[], &[]),
            vec![(0, "nesting-depth".to_string())]
        );
        // So does a keyword whose text opens with a keyword, read again for
        // every label.
        let chain = |n: usize| format!("{}x\n", "MUST: ".repeat(n));
        assert!(delivered(&chain(NESTING_CAP)).ends_with("x\n"));
        assert_eq!(
            refused(&chain(NESTING_CAP + 1), &[], &[]),
            vec![(0, "nesting-depth".to_string())]
        );
    }

    /// §5.2: a `param` declares wherever it is written — in a `when` body,
    /// as a leaf or a table, as at the top level.
    #[test]
    fn a_param_in_a_body_declares() {
        assert_eq!(
            with_facts(
                "::::when{a=\"1\"}\n:::param[]\n| name | default |\n|---|---|\n| p | v |\n:::\nUse ${p}.\n::::\n",
                &[("a", "1")]
            ),
            "Use v.\n"
        );
        assert_eq!(
            with_facts(
                ":::when{agent=\"agentd\"}\n::param{name=y default=1}\n:::\n\nY=${y}\n",
                &[("agent", "agentd")]
            ),
            "Y=1\n"
        );
    }

    /// When a runtime fact and a parameter share a key, the variant selects
    /// on the fact (runtime-authoritative) and `${key}` substitutes the
    /// parameter. The reference lets the parameter win both; which is right
    /// is open upstream, and this pins what the crate does until it is
    /// settled.
    #[test]
    fn a_fact_selects_and_a_parameter_substitutes_on_a_shared_key() {
        assert_eq!(
            with_facts(
                "::param{name=env default=dev}\n\n:::when{env=\"prod\"}\nProd ${env}.\n:::\n",
                &[("env", "prod")]
            ),
            "Prod dev.\n"
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

    /// Appendix A, `!human`: a role says what it may be asked for when
    /// `may` is set.
    #[test]
    fn a_human_role_says_what_it_may_be_asked() {
        assert_eq!(
            delivered(
                "::!human{name=ops channel=\"slack://x\" may=\"approve, deny\"}\n\n::!human{name=lead}\n"
            ),
            "[human role \"ops\" may be asked (may: approve, deny)]\n\n[human role \"lead\" may be asked]\n"
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

    // ── The S7 resolution manifest, accounted for by the delivery walk ──

    /// A delivery's manifest with `params`, `facts` and `includes` (id →
    /// text), every family granted.
    fn manifest_of(
        text: &str,
        params: &[(&str, &str)],
        facts: &[(&str, &str)],
        includes: &[(&str, &str)],
    ) -> Manifest {
        let map = |kv: &[(&str, &str)]| -> BTreeMap<String, String> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let includes = map(includes);
        let resolve = |id: &str| includes.get(id).cloned();
        let ctx = crate::Context {
            grants: all_families(),
            params: map(params),
            facts: map(facts),
            resolve_include: Some(&resolve),
        };
        crate::deliver(&parse(text).unwrap(), &ctx)
            .unwrap()
            .manifest
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Variants are counted per kind in document order as delivery meets
    /// them, kept and dropped each in encounter order: a kept variant before
    /// those its body holds, a dropped one's never — nor the facts they
    /// would have compared. An included document counts its own and adds
    /// none, though the facts it compared are the delivery's.
    #[test]
    fn variants_are_counted_where_delivery_meets_them() {
        let doc = "::::when{env=\"prod\"}\nProd.\n\n:::unless{tier=\"gold\"}\nNot gold.\n:::\n::::\n\n\
                   ::::when{env=\"dev\"}\nDev.\n\n:::when{region=\"eu\"}\nNever met.\n:::\n::::\n\n\
                   :::otherwise\nNeither.\n:::\n\n:::unless{tier=\"gold\" verbatim}\nAgain.\n:::\n\n\
                   ::include{id=\"i\"}\n";
        let m = manifest_of(
            doc,
            &[],
            &[
                ("env", "prod"),
                ("tier", "gold"),
                ("region", "eu"),
                ("agent", "x"),
                ("verbatim", "true"),
            ],
            &[("i", ":::when{agent=\"x\"}\nIncluded.\n:::\n")],
        );
        assert_eq!(m.variants.kept, strings(&["when#1"]));
        assert_eq!(
            m.variants.dropped,
            strings(&["unless#1", "when#2", "otherwise#1", "unless#2"])
        );
        // `region` was compared only in a dropped subtree, `verbatim` is no
        // condition; `agent` was compared by the include.
        let keys: Vec<&str> = m.facts.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["agent", "env", "tier"]);
        assert_eq!(m.facts[1].value_digest, crate::digest(b"prod"));
    }

    /// A fact is recorded only when the condition compared a known value: a
    /// parameter whose value does not fit its type is no fact, and a key no
    /// fact supplies was not compared.
    #[test]
    fn only_a_compared_value_is_a_recorded_fact() {
        let m = manifest_of(
            "::param{name=n type=number}\n\n:::when{n=\"1\"}\nA.\n:::\n\n:::when{unknown=\"x\"}\nB.\n:::\n",
            &[("n", "lots")],
            &[],
            &[],
        );
        assert!(m.facts.is_empty(), "{:?}", m.facts);
        assert_eq!(m.variants.kept, strings(&["when#1", "when#2"]));
    }

    /// Includes are recorded as they are inlined: pre-order, depth first, a
    /// repeat twice, the target as written; bytes in UTF-8 over every
    /// inlined text; the depth the deepest reached. A cycle, or a target
    /// nothing resolves, inlines nothing and is not recorded.
    #[test]
    fn includes_are_accounted_as_they_are_inlined() {
        let a = "A.\n\n::include{id=\"b\"}\n";
        let b = "Café — ü.\n";
        let c = "C.\n\n::include{id=\"c\"}\n";
        let m = manifest_of(
            "::include{id=\"a\"}\n\n::include{uri=\"instruction://ins_b\"}\n\n::include{id=\"a\"}\n\n\
             ::include{id=\"c\"}\n\n::include{id=\"gone\"}\n",
            &[],
            &[],
            &[("a", a), ("b", b), ("instruction://ins_b", b), ("c", c)],
        );
        let got: Vec<(&str, &str)> = m
            .includes
            .iter()
            .map(|i| (i.target.as_str(), i.digest.as_str()))
            .collect();
        let (da, db, dc) = (
            crate::digest(a.as_bytes()),
            crate::digest(b.as_bytes()),
            crate::digest(c.as_bytes()),
        );
        assert_eq!(
            got,
            [
                ("a", da.as_str()),
                ("b", db.as_str()),
                ("instruction://ins_b", db.as_str()),
                ("a", da.as_str()),
                ("b", db.as_str()),
                ("c", dc.as_str()),
            ]
        );
        assert!(b.len() > b.chars().count(), "the sample is not ASCII");
        assert_eq!(
            m.limits,
            Limits {
                include_depth: 2,
                include_bytes: (2 * a.len() + 3 * b.len() + c.len()) as u64,
            }
        );
    }

    /// Each parameter substituted in the delivered text is recorded once,
    /// sorted, the first use winning, with its declared source — front
    /// matter or `param` — or `static`, and the digest of the value used,
    /// as capped. Text never delivered records nothing: a dropped variant, a
    /// form's body, a skill's catalogue body.
    #[test]
    fn parameters_are_recorded_where_they_are_substituted() {
        let long = "x".repeat(doc::PARAM_VALUE_BYTES_CAP + 10);
        let doc = format!(
            "---\nspec: \"1\"\nparameters:\n  - name: region\n    source: workspace\n    default: eu\n---\n\
             ::param{{name=ticket source=prompt}}\n::param{{name=env default=prod}}\n::param{{name=big default=\"{long}\"}}\n\n\
             Ship ${{env}} in ${{region}} for ${{ticket}}, ${{env}} again, ${{big}}.\n\n\
             :::when{{env=\"dev\"}}\nOnly ${{secret}}.\n:::\n\n\
             :::form{{title=\"Intake\"}}\nFill ${{hidden}}.\n:::\n\n\
             :::!skill{{name=s}}\nUse ${{skillonly}} and ${{nothing}}.\n:::\n"
        );
        let m = manifest_of(
            &doc,
            &[
                ("ticket", "T-1"),
                ("secret", "s"),
                ("hidden", "h"),
                ("skillonly", "k"),
            ],
            &[],
            &[],
        );
        let got: Vec<(&str, &str, String)> = m
            .parameters
            .iter()
            .map(|p| (p.name.as_str(), p.source.as_str(), p.value_digest.clone()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "big",
                    "static",
                    crate::digest(&long.as_bytes()[..doc::PARAM_VALUE_BYTES_CAP])
                ),
                ("env", "static", crate::digest(b"prod")),
                ("region", "workspace", crate::digest(b"eu")),
                ("ticket", "prompt", crate::digest(b"T-1")),
            ]
        );
        assert!(m.unresolved.is_empty(), "{:?}", m.unresolved);
    }

    /// Decided where the spec leaves it open (a): what an included document
    /// substitutes, and leaves unresolved, is the one delivery's — under its
    /// own declarations, after the includer's first use of a name.
    #[test]
    fn an_includes_parameters_are_the_deliverys() {
        let m = manifest_of(
            "::param{name=env default=prod}\n\nIn ${env}.\n\n::include{id=\"i\"}\n",
            &[],
            &[],
            &[(
                "i",
                "::param{name=env default=staging source=workspace}\n::param{name=own default=x source=prompt}\n\n\
                 ${env}, ${own}, ${gone}.\n",
            )],
        );
        let got: Vec<(&str, &str, String)> = m
            .parameters
            .iter()
            .map(|p| (p.name.as_str(), p.source.as_str(), p.value_digest.clone()))
            .collect();
        assert_eq!(
            got,
            [
                ("env", "static", crate::digest(b"prod")),
                ("own", "prompt", crate::digest(b"x")),
            ]
        );
        assert_eq!(m.unresolved, strings(&["gone"]));
    }

    /// Decided (b): a placeholder in fenced code is neither substituted nor
    /// recorded, resolvable or not.
    #[test]
    fn a_placeholder_in_fenced_code_is_not_accounted_for() {
        let m = manifest_of(
            "::param{name=env default=prod}\n\n```\n${env} ${nope}\n```\n",
            &[],
            &[],
            &[],
        );
        assert!(m.parameters.is_empty(), "{:?}", m.parameters);
        assert!(m.unresolved.is_empty(), "{:?}", m.unresolved);
    }

    /// Placeholders left as written are unresolved, sorted and once each; a
    /// `${…}` the placeholder grammar does not admit is no placeholder.
    #[test]
    fn unresolved_placeholders_are_sorted_once_each() {
        let m = manifest_of("Use ${b} ${a} ${b} ${} ${a b} ${_c.d-e}.\n", &[], &[], &[]);
        assert_eq!(m.unresolved, strings(&["_c.d-e", "a", "b"]));
    }

    /// Decided (c): an `overrides` in a dropped variant adds nothing to
    /// `overridden` or `unresolved`; delivered, it silences its target and
    /// reports the one found nowhere.
    #[test]
    fn an_override_in_a_dropped_variant_is_not_accounted_for() {
        let doc = "SHOULD[brevity]: be brief.\n\n::::when{env=\"dev\"}\n\
                   :::must{name=m overrides=\"should/brevity, should/nowhere\"}\nM.\n:::\n::::\n";
        let m = manifest_of(doc, &[], &[("env", "prod")], &[]);
        assert!(m.overridden.is_empty(), "{:?}", m.overridden);
        assert!(m.unresolved.is_empty(), "{:?}", m.unresolved);
        let m = manifest_of(doc, &[], &[("env", "dev")], &[]);
        assert_eq!(m.overridden, strings(&["should/brevity"]));
        assert_eq!(m.unresolved, strings(&["should/nowhere"]));
    }

    /// `overridden` is sorted, and holds the rules silenced inside includes;
    /// an unfound override target sorts among the unresolved placeholders.
    #[test]
    fn overridden_is_sorted_and_reaches_into_includes() {
        let m = manifest_of(
            "SHOULD[z]: z.\n\n::include{id=\"i\"}\n\n\
             :::must{name=m overrides=\"should/z, should/a, should/gone\"}\nM ${p}.\n:::\n",
            &[],
            &[],
            &[("i", "SHOULD[a]: a.\n")],
        );
        assert_eq!(m.overridden, strings(&["should/a", "should/z"]));
        assert_eq!(m.unresolved, strings(&["p", "should/gone"]));
    }

    /// A skill's catalogue body is not the delivery: its variants, facts,
    /// placeholders and includes are not in the manifest.
    #[test]
    fn a_skill_catalogue_body_is_not_accounted_for() {
        let m = manifest_of(
            "::::!skill{name=s}\nUse ${p} and ${q}.\n\n:::when{env=\"prod\"}\nP.\n:::\n\n\
             ::include{id=\"one\"}\n::::\n",
            &[("p", "1")],
            &[("env", "prod")],
            &[("one", "One.\n")],
        );
        assert_eq!(
            m,
            Manifest {
                authored: m.authored.clone(),
                ..Manifest::default()
            }
        );
    }

    /// The authored digest is the author digest of the raw document: its
    /// front-matter signature line excluded, its end matter — never
    /// delivered — included.
    #[test]
    fn the_authored_digest_covers_the_document_as_stored() {
        let doc = "---\nspec: \"1\"\n---\nMUST: x.\n\n---\nowners: [ana]\n---\n";
        let m = manifest_of(doc, &[], &[], &[]);
        assert_eq!(m.authored.digest, crate::author_digest(doc.as_bytes()));
        assert_eq!(m.authored.version, None);
        let signed = doc.replacen("---\nMUST", "signature: a.b.c\n---\nMUST", 1);
        assert_eq!(manifest_of(&signed, &[], &[], &[]).authored, m.authored);
        let other_owner = doc.replace("ana", "bo");
        assert_ne!(
            manifest_of(&other_owner, &[], &[], &[]).authored,
            m.authored
        );
    }
}
