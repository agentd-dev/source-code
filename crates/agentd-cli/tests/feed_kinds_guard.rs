// SPDX-License-Identifier: AGPL-3.0-only
//! **Every kind the feed pushes is one events/v1 declares.**
//!
//! `FeedKind::ALL` is the published vocabulary: the extended card lists it,
//! the schema bundle is built from it, a client switches on it. A push is a
//! plain string, so nothing in the type system stops a call site from pushing
//! a kind the vocabulary never had — the `message` and `pairing` kinds lived
//! on for a release after their meaning had moved. The debug push assertion
//! catches such a push when a test happens to drive it; this scan catches it
//! whether anything drives it or not.
//!
//! It reads every place a kind is named: the first argument of `feed_push(…)`
//! and `feed.push(…)`, the `kind:` of each `StatusItem` the section diff
//! pushes, and the arms of `removed_kind`. A kind that is not a literal is
//! refused too, outside the one file that forwards kinds it was handed
//! (`runtime/a2a_server/feed.rs`), because a computed kind is one this scan
//! cannot read. And the other direction: every declared kind is pushed
//! somewhere, so the vocabulary carries no kind a client waits for in vain.
//!
//! Comments and `#[cfg(test)]` modules are not pushes, so they are skipped.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agentd::runtime::surface::events::FeedKind;

/// The trees that push onto the feed.
const SCANNED: &[&str] = &["../agentd/src", "src"];

/// The file that forwards a kind it was handed rather than naming one.
const FORWARDS: &str = "runtime/a2a_server/feed.rs";

#[test]
fn every_pushed_kind_is_declared() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in SCANNED {
        let before = files.len();
        collect_rs(&root.join(dir), &mut files);
        // A moved tree must fail here, not make the guard vacuously green.
        assert!(files.len() > before, "{dir} has no .rs files to scan");
    }
    let declared: BTreeSet<&str> = FeedKind::ALL.iter().map(|k| k.as_str()).collect();
    let mut pushed = BTreeSet::new();
    let mut bad = Vec::new();
    for file in &files {
        let src = std::fs::read_to_string(file).expect("read source");
        let shown = file.display().to_string();
        for site in sites(&src) {
            match site.kind {
                Kind::Literal(k) if declared.contains(k.as_str()) => {
                    pushed.insert(k);
                }
                Kind::Literal(k) => bad.push(format!(
                    "{shown}:{}: {k:?} is not in FeedKind::ALL",
                    site.line
                )),
                Kind::Computed(_) if shown.ends_with(FORWARDS) => {}
                Kind::Computed(expr) => bad.push(format!(
                    "{shown}:{}: a kind computed from `{expr}` — name the kind as a literal",
                    site.line
                )),
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a feed push outside the events/v1 vocabulary (surface::events::FeedKind):\n  {}",
        bad.join("\n  ")
    );
    let never: Vec<&&str> = declared.iter().filter(|k| !pushed.contains(**k)).collect();
    assert!(
        never.is_empty(),
        "declared kinds nothing pushes: {never:?} (pushed: {pushed:?})"
    );
}

/// The scanner on a source written to trip it: it reads each form a kind is
/// named in, and nothing in comments, strings elsewhere or test modules.
#[test]
fn the_scanner_reads_every_form_and_only_code() {
    let src = r##"
fn a(&self) {
    self.feed_push("task", FeedVis::All, json!({})); // line 3
    self.feed_push(
        "message",                                    // line 5
        FeedVis::All,
        json!({}),
    );
    // self.feed_push("commented", …)
    /* feed.push("blocked", …) */
    let _ = "feed.push(\"quoted\")";
    feed.push("auth", FeedVis::Operator, event);      // line 12
    self.feed_push(kind, vis, data);                  // line 13
    items.push(StatusItem { key: k, kind: "run", vis, data }); // line 14
    items.push(StatusItem { key: k, kind, vis, data });        // line 15
    other.push("not a feed");
}
pub(crate) struct StatusItem { pub kind: &'static str }
pub(crate) fn feed_push(&self, kind: &str) {}
fn removed_kind(kind: &str) -> &'static str {
    match kind {
        "run" => "run.removed",                       // line 22
        _ => "child.removed",                         // line 23
    }
}
#[cfg(test)]
mod tests {
    fn t() { feed.push("pairing", FeedVis::All, json!({})); }
}
"##;
    let got: Vec<(usize, Kind)> = sites(src).into_iter().map(|s| (s.line, s.kind)).collect();
    let lit = |s: &str| Kind::Literal(s.to_string());
    let expr = |s: &str| Kind::Computed(s.to_string());
    assert_eq!(
        got,
        vec![
            (3, lit("task")),
            (5, lit("message")),
            (12, lit("auth")),
            (13, expr("kind")),
            (14, lit("run")),
            (15, expr("kind")),
            (22, lit("run.removed")),
            (23, lit("child.removed")),
        ]
    );
}

#[derive(Debug, PartialEq)]
enum Kind {
    Literal(String),
    Computed(String),
}

struct Site {
    line: usize,
    kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(char),
}

/// Every place `src` names a feed kind.
fn sites(src: &str) -> Vec<Site> {
    let toks = strip_test_modules(lex(src));
    let t = |i: usize| toks.get(i).map(|(_, t)| t);
    let ident = |i: usize, s: &str| matches!(t(i), Some(Tok::Ident(x)) if x == s);
    let punct = |i: usize, c: char| t(i) == Some(&Tok::Punct(c));
    // The kind argument at `i`: a literal, or the expression's first token.
    let kind_at = |i: usize| match t(i) {
        Some(Tok::Str(s)) => Kind::Literal(s.clone()),
        Some(Tok::Ident(x)) => Kind::Computed(x.clone()),
        other => Kind::Computed(format!("{other:?}")),
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let line = toks[i].0;
        let after_fn = i > 0 && ident(i - 1, "fn");
        if ident(i, "feed_push") && punct(i + 1, '(') && !after_fn {
            out.push(Site {
                line: toks.get(i + 2).map_or(line, |t| t.0),
                kind: kind_at(i + 2),
            });
        } else if ident(i, "feed") && punct(i + 1, '.') && ident(i + 2, "push") && punct(i + 3, '(')
        {
            out.push(Site {
                line: toks.get(i + 4).map_or(line, |t| t.0),
                kind: kind_at(i + 4),
            });
        } else if ident(i, "StatusItem") && punct(i + 1, '{') && !(i > 0 && ident(i - 1, "struct"))
        {
            let close = matching(&toks, i + 1);
            // `kind` as a field of this literal, not of a nested one.
            let mut depth = 0usize;
            for (j, (at, tok)) in toks.iter().enumerate().take(close).skip(i + 2) {
                match tok {
                    Tok::Punct('{' | '(' | '[') => depth += 1,
                    Tok::Punct('}' | ')' | ']') => depth = depth.saturating_sub(1),
                    Tok::Ident(x)
                        if x == "kind"
                            && depth == 0
                            && (punct(j - 1, '{') || punct(j - 1, ',')) =>
                    {
                        let kind = if punct(j + 1, ':') {
                            kind_at(j + 2)
                        } else {
                            Kind::Computed("kind".into())
                        };
                        out.push(Site { line: *at, kind });
                    }
                    _ => {}
                }
            }
        } else if after_fn && ident(i, "removed_kind") {
            let open = (i..toks.len())
                .find(|&j| punct(j, '{'))
                .unwrap_or(toks.len());
            let close = matching(&toks, open);
            for j in open..close {
                if punct(j, '=')
                    && punct(j + 1, '>')
                    && let Some(Tok::Str(s)) = t(j + 2)
                {
                    out.push(Site {
                        line: toks[j + 2].0,
                        kind: Kind::Literal(s.clone()),
                    });
                }
            }
            i = close;
        }
        i += 1;
    }
    out
}

/// The index of the brace closing the one at `open`.
fn matching(toks: &[(usize, Tok)], open: usize) -> usize {
    let mut depth = 0usize;
    for (j, (_, tok)) in toks.iter().enumerate().skip(open) {
        match tok {
            Tok::Punct('{') => depth += 1,
            Tok::Punct('}') => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

/// `toks` without any `#[cfg(test)] mod … { … }`.
fn strip_test_modules(toks: Vec<(usize, Tok)>) -> Vec<(usize, Tok)> {
    let is = |i: usize, want: &Tok| toks.get(i).map(|(_, t)| t) == Some(want);
    let id = |s: &str| Tok::Ident(s.into());
    let attr = [
        Tok::Punct('#'),
        Tok::Punct('['),
        id("cfg"),
        Tok::Punct('('),
        id("test"),
        Tok::Punct(')'),
        Tok::Punct(']'),
    ];
    let mut keep = vec![true; toks.len()];
    let mut i = 0;
    while i < toks.len() {
        if attr.iter().enumerate().all(|(k, t)| is(i + k, t)) {
            let mut j = i + attr.len();
            // Further attributes may sit between it and the `mod`.
            while is(j, &Tok::Punct('#')) && is(j + 1, &Tok::Punct('[')) {
                while j < toks.len() && !is(j, &Tok::Punct(']')) {
                    j += 1;
                }
                j += 1;
            }
            if is(j, &id("mod")) && is(j + 2, &Tok::Punct('{')) {
                let close = matching(&toks, j + 2);
                keep[i..=close.min(toks.len() - 1)].fill(false);
                i = close;
            }
        }
        i += 1;
    }
    toks.into_iter()
        .zip(keep)
        .filter_map(|(t, k)| k.then_some(t))
        .collect()
}

/// Identifiers, string literals (their value) and punctuation, each with its
/// line; comments, char literals, lifetimes and numbers dropped.
fn lex(src: &str) -> Vec<(usize, Tok)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut line = 1;
    let mut i = 0;
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line += 1;
            i += 1;
        } else if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            let mut depth = 0;
            while i < b.len() {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    line += usize::from(b[i] == b'\n');
                    i += 1;
                }
            }
        } else if c == b'"' {
            let at = line;
            let mut s = String::new();
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    line += usize::from(b[i + 1] == b'\n');
                    s.push(b[i + 1] as char);
                    i += 2;
                } else {
                    line += usize::from(b[i] == b'\n');
                    let ch = src[i..].chars().next().unwrap_or(' ');
                    s.push(ch);
                    i += ch.len_utf8();
                }
            }
            i += 1;
            out.push((at, Tok::Str(s)));
        } else if c == b'\'' {
            // A char literal is '\…' or one char then a quote; anything else
            // is a lifetime or a label.
            let close = if b.get(i + 1) == Some(&b'\\') {
                b.get(i + 3..)
                    .and_then(|r| r.iter().position(|&c| c == b'\''))
                    .map(|p| i + 3 + p)
            } else {
                let width = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                (b.get(i + 1 + width) == Some(&b'\'')).then_some(i + 1 + width)
            };
            i = close.map_or(i + 1, |c| c + 1);
        } else if ident(c) {
            let start = i;
            while i < b.len() && ident(b[i]) {
                i += 1;
            }
            let word = &src[start..i];
            // r"…", r#"…"#, br"…": a raw string runs to a quote followed by
            // as many `#` as opened it.
            let hashes = b[i..].iter().take_while(|&&h| h == b'#').count();
            if matches!(word, "r" | "br") && b.get(i + hashes) == Some(&b'"') {
                let open = i + hashes + 1;
                let closing = format!("\"{}", "#".repeat(hashes));
                let end = src[open..].find(&closing).map_or(b.len(), |p| open + p);
                out.push((line, Tok::Str(src[open..end].to_string())));
                line += src[open..end].matches('\n').count();
                i = (end + closing.len()).min(b.len());
            } else if !c.is_ascii_digit() {
                // A number is not an identifier; nothing here matches one.
                out.push((line, Tok::Ident(word.to_string())));
            }
        } else if c.is_ascii_whitespace() {
            i += 1;
        } else {
            let ch = src[i..].chars().next().unwrap();
            if ch.is_ascii() {
                out.push((line, Tok::Punct(ch)));
            }
            i += ch.len_utf8();
        }
    }
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}
