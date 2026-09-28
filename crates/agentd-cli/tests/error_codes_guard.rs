// SPDX-License-Identifier: AGPL-3.0-only
//! **A spec error code means what the spec says, and nothing else.**
//!
//! A2A gives `-32003` one meaning — push notifications are not supported — and
//! `-32007` one meaning — this agent has no extended card. agentd once answered
//! "not authorized" with the first and "who are you?" with the second, so a
//! client that branched on the spec's code told its user to turn on push
//! notifications when the real answer was "sign in". Identity refusals now have
//! codes of their own (`a2a::errors::{UNAUTHENTICATED, PERMISSION_DENIED}`), and
//! this file keeps the two spec codes where they belong:
//!
//! * `-32003` is raised only by `Runtime::push_enabled`, the one check that
//!   knows whether push is on;
//! * `-32007` is never raised by the listener or the runtime binding at all —
//!   it lives in `a2a/errors.rs`, and an agent with no scheme answers the
//!   extended card with `-32004`.
//!
//! Both the number and every name for it are matched — agentd's constant, the
//! SDK's constant and the SDK's error variant — because a guard that only saw
//! `-32003` would wave through `errors::PUSH_NOTIFICATION_NOT_SUPPORTED` in an
//! auth path. Comments, string literals and `#[cfg(test)]` modules are not
//! code that answers anyone, so they are blanked before matching.

use std::path::{Path, PathBuf};

/// The trees that answer A2A callers: the listener and the runtime binding.
const SCANNED: &[&str] = &["src/a2a/serve", "src/runtime/a2a_server"];

/// One spec code and the single place it may be raised.
struct Rule {
    /// The magnitude; a sign or `_` separators do not hide it.
    code: u64,
    /// Every identifier that stands for it.
    names: &'static [&'static str],
    /// `(impl self type, fn)` allowed to raise it; `None` = nowhere scanned.
    allowed: Option<(&'static str, &'static str)>,
}

const RULES: &[Rule] = &[
    Rule {
        code: 32003,
        names: &[
            "PUSH_NOTIFICATION_NOT_SUPPORTED",
            "PushNotificationNotSupported",
        ],
        allowed: Some(("Runtime", "push_enabled")),
    },
    Rule {
        code: 32007,
        names: &[
            "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
            "AUTHENTICATED_EXTENDED_CARD_NOT_CONFIGURED",
            "AuthenticatedExtendedCardNotConfigured",
        ],
        allowed: None,
    },
];

#[test]
fn spec_codes_are_not_reused_for_auth() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agentd");
    let mut files = Vec::new();
    for dir in SCANNED {
        let before = files.len();
        collect_rs(&crate_dir.join(dir), &mut files);
        // A moved tree must fail here, not make the guard vacuously green.
        assert!(files.len() > before, "{dir} has no .rs files to scan");
    }
    let mut found = Vec::new();
    for file in &files {
        let src = std::fs::read_to_string(file).expect("read source");
        let shown = file.strip_prefix(&crate_dir).unwrap_or(file).display();
        for v in violations(&src) {
            found.push(format!("{shown}:{v}"));
        }
    }
    assert!(
        found.is_empty(),
        "a spec error code is raised outside the one place it means what the spec says \
         (identity refusals use a2a::errors::UNAUTHENTICATED / PERMISSION_DENIED):\n  {}",
        found.join("\n  ")
    );
}

/// The scanner itself, on sources written to trip it, so the guard above is
/// known to see what it must and nothing else.
#[test]
fn the_scanner_sees_code_and_only_code() {
    let src = r##"
impl Runtime {
    fn push_enabled(&self) -> Result<(), Value> {
        if on { Ok(()) } else { Err(err_obj(-32003, "push is off")) }
    }
    fn a2a_push_set(&mut self) -> Value {
        err_obj(-32_003, "not authorized") // line 7
    }
}
fn card(p: &Principal) -> Value {
    // -32007 in a comment is prose, and so is "-32007" in a string,
    /* and -32003 in /* a nested */ block comment, */
    let _ = r#"raw -32007 "quoted""#;
    let _ = '"'; let _ = '\'';
    if p.anon { return err(id, errors::EXTENDED_AGENT_CARD_NOT_CONFIGURED, "anon"); } // line 15
    let _ = A2AError::PushNotificationNotSupported; // line 16
    let _ = 132003 + 320030 + PUSH_NOTIFICATION_NOT_SUPPORTED_X; // near misses
    err(id, -32007i64, "{ unbalanced in a string") // line 18
}
impl Other {
    fn push_enabled(&self) -> Value { err_obj(-32003, "wrong impl") } // line 21
}
#[cfg(test)]
mod tests {
    #[test]
    fn push_off_is_32003() { assert_eq!(code, -32003); }
}
"##;
    let got = violations(src);
    let lines: Vec<usize> = got
        .iter()
        .map(|v| v.split(':').next().unwrap().parse().unwrap())
        .collect();
    assert_eq!(lines, vec![7, 15, 16, 18, 21], "{got:#?}");
}

/// `line: what` for every place a rule's code or name appears where it may not.
fn violations(src: &str) -> Vec<String> {
    let code = strip_test_modules(&sanitize(src));
    let fns = spans(&code, "fn");
    let impls = spans(&code, "impl");
    let mut out = Vec::new();
    for (at, token) in tokens(&code) {
        for rule in RULES {
            let hit = match token.bytes().next() {
                Some(b'0'..=b'9') => number(&token) == Some(rule.code),
                _ => rule.names.contains(&token.as_str()),
            };
            if !hit {
                continue;
            }
            let inside = |spans: &[Span]| {
                spans
                    .iter()
                    .filter(|s| s.open < at && at < s.close)
                    .max_by_key(|s| s.open)
                    .map(|s| s.name.clone())
            };
            let place = (inside(&impls), inside(&fns));
            let ok = rule.allowed.is_some_and(|(ty, f)| {
                place.0.as_deref() == Some(ty) && place.1.as_deref() == Some(f)
            });
            if !ok {
                let line = code[..at].matches('\n').count() + 1;
                let (ty, f) = place;
                out.push(format!(
                    "{line}: `{token}` (-{}) in {}::{}",
                    rule.code,
                    ty.as_deref().unwrap_or("_"),
                    f.as_deref().unwrap_or("_")
                ));
            }
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

// ---- a lexer just good enough to tell code from prose ------------------------

/// The source with every comment and every string and char literal replaced by
/// spaces. Byte offsets and newlines are kept, so positions and line numbers
/// still point into the original.
fn sanitize(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    while i < b.len() {
        let start = i;
        if b[i..].starts_with(b"//") {
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
                    i += 1;
                }
            }
        } else if b[i] == b'r'
            && (i == 0 || !ident(b[i - 1]) || (b[i - 1] == b'b' && (i < 2 || !ident(b[i - 2]))))
            && matches!(b.get(i + 1), Some(b'#' | b'"'))
        {
            // r"…" / r#"…"# (and br…): ends at a quote followed by as many #.
            let mut j = i + 1;
            while b.get(j) == Some(&b'#') {
                j += 1;
            }
            if b.get(j) != Some(&b'"') {
                i += 1;
                continue;
            }
            let hashes = j - i - 1;
            i = j + 1;
            while i < b.len() {
                if b[i] == b'"'
                    && b[i + 1..]
                        .iter()
                        .take(hashes)
                        .filter(|&&c| c == b'#')
                        .count()
                        == hashes
                {
                    i += 1 + hashes;
                    break;
                }
                i += 1;
            }
        } else if b[i] == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
        } else if b[i] == b'\'' {
            // A char literal is '\…' or one char then a quote; anything else
            // is a lifetime or a label and stays code.
            let close = if b.get(i + 1) == Some(&b'\\') {
                b.get(i + 3..)
                    .and_then(|r| r.iter().position(|&c| c == b'\''))
                    .map(|p| i + 3 + p)
            } else {
                let width = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                (b.get(i + 1 + width) == Some(&b'\'')).then_some(i + 1 + width)
            };
            match close {
                Some(c) => i = c + 1,
                None => {
                    i += 1;
                    continue;
                }
            }
        } else {
            i += 1;
            continue;
        }
        let end = i.min(b.len());
        blank(&mut out, start, end);
        i = end;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `code` with every `#[cfg(test)] mod … { … }` blanked: a test asserting that
/// push-off answers `-32003` raises nothing.
fn strip_test_modules(code: &str) -> String {
    let mut out = code.as_bytes().to_vec();
    let mut from = 0;
    while let Some(p) = code[from..].find("#[cfg(test)]") {
        let at = from + p;
        let mut rest = code[at + "#[cfg(test)]".len()..].trim_start();
        from = at + 1;
        // Further attributes (`#[allow(…)]`) may sit between it and the `mod`.
        while rest.starts_with("#[") {
            rest = rest[rest.find(']').map_or(rest.len(), |e| e + 1)..].trim_start();
        }
        if !rest.starts_with("mod ") {
            continue;
        }
        let open = code.len() - rest.len() + rest.find(['{', ';']).unwrap_or(0);
        if code.as_bytes()[open] != b'{' {
            continue;
        }
        let close = matching(code, open);
        for c in &mut out[at..=close] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A braced item: where its body opens and closes, and its name (the fn name,
/// or an impl's self type).
struct Span {
    name: String,
    open: usize,
    close: usize,
}

/// Every `fn` or `impl` item with a body, in sanitized code.
fn spans(code: &str, keyword: &str) -> Vec<Span> {
    let mut out = Vec::new();
    for (at, token) in tokens(code) {
        if token != keyword {
            continue;
        }
        // `impl Trait` in a signature is a type, not an item: an impl block
        // starts where an item may, after `}`, `;`, `{`, an attribute, or
        // `unsafe`.
        if keyword == "impl" {
            let before = code[..at].trim_end();
            let item_start = before.is_empty()
                || before.ends_with(['}', ';', '{', ']'])
                || before.ends_with("unsafe");
            if !item_start {
                continue;
            }
        }
        let after = &code[at + keyword.len()..];
        let Some(end) = after.find(['{', ';']) else {
            continue;
        };
        if after.as_bytes()[end] != b'{' {
            continue;
        }
        let header = &after[..end];
        let name = if keyword == "fn" {
            header
                .trim_start()
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default()
                .to_string()
        } else {
            // `impl<T> Trait for path::Type<T> where …` → `Type`.
            let header = header.split(" where ").next().unwrap_or(header);
            let ty = header.rsplit(" for ").next().unwrap_or(header).trim();
            let ty = ty.split('<').next().unwrap_or(ty);
            ty.rsplit("::").next().unwrap_or(ty).trim().to_string()
        };
        // A `fn(…)` pointer type in a signature names nothing; its "body"
        // would be the enclosing fn's.
        if name.is_empty() {
            continue;
        }
        let open = at + keyword.len() + end;
        out.push(Span {
            name,
            open,
            close: matching(code, open),
        });
    }
    out
}

/// The `}` that closes the `{` at `open` (end of file if unbalanced).
fn matching(code: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (i, c) in code.bytes().enumerate().skip(open) {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    code.len()
}

/// Every identifier and number token with its offset.
fn tokens(code: &str) -> Vec<(usize, String)> {
    let b = code.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if word(b[i]) {
            let start = i;
            while i < b.len() && word(b[i]) {
                i += 1;
            }
            out.push((start, code[start..i].to_string()));
        } else {
            i += 1;
        }
    }
    out
}

/// A decimal integer token's value: `32_003`, `32003i64` → 32003. A hex or
/// float token, or a digit-led identifier, is not one of ours.
fn number(token: &str) -> Option<u64> {
    let digits: String = token
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    let suffix = &token[token
        .find(|c: char| !(c.is_ascii_digit() || c == '_'))
        .unwrap_or(token.len())..];
    let integer_suffix = [
        "", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
    ];
    if !integer_suffix.contains(&suffix) {
        return None;
    }
    digits.parse().ok()
}
