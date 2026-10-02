// SPDX-License-Identifier: MIT OR Apache-2.0
//! **Refusals** (S20, Appendix B): what a reader says when it will not load a
//! document. A refusal carries the line it names, the stable **code** of its
//! condition, and the message.
//!
//! The code is the contract. Conformance compares a refusal's line, code and
//! message; the message shapes are only RECOMMENDED upstream, so a reader may
//! translate them, but the code is what a tool or a test can rely on.

use serde::Serialize;

/// One refusal: the line it names (when it names one), its Appendix B code,
/// and the message — the construct, and what to write instead.
///
/// `message` never carries the `line N: ` prefix: the line travels as data,
/// as it does in the fixture corpus's `refusals.json`. [`Display`] puts the
/// prefix back, so the text agentd has always printed is unchanged.
///
/// [`Display`]: std::fmt::Display
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub line: Option<u32>,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    /// A refusal that names `line` (1-based).
    pub fn at(line: usize, code: &'static str, message: impl Into<String>) -> Refusal {
        Refusal {
            // A document with more than u32::MAX lines is never read, so the
            // saturation is unreachable; it keeps the type the wire uses.
            line: Some(u32::try_from(line).unwrap_or(u32::MAX)),
            ..Refusal::new(code, message)
        }
    }

    /// A refusal of the document as a whole (the `spec` version it pins) — no
    /// line.
    pub fn new(code: &'static str, message: impl Into<String>) -> Refusal {
        // A code outside the list is a condition no reader could match: catch
        // it where it is minted, in every debug build and test run.
        debug_assert!(
            CODES.contains(&code),
            "refusal code {code:?} is not in CODES"
        );
        Refusal {
            line: None,
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Byte-identical to the strings the parser produced before refusals
        // were structured, so logs, `config.invalid` and every test that
        // matches the text read the same.
        match self.line {
            Some(n) => write!(f, "line {n}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Every refusal code: the code column of the specification's Appendix B, in
/// its order, then the codes the conformance fixtures pin that Appendix B has
/// no row for.
pub const CODES: &[&str] = &[
    "unknown-machinery-kind",
    "sigiled-prose-kind",
    "bare-machinery-kind",
    "unclosed-fence",
    "text-after-close-fence",
    "malformed-attributes",
    "repeated-attribute",
    "id-class-shorthand",
    "duplicate-identity",
    "dangling-reference",
    "reference-cycle",
    "body-required",
    "body-forbidden",
    "set-body-mixed",
    "set-row-without-name",
    "unknown-column",
    "redundant-set",
    "subblock-out-of-place",
    "section-without-fence",
    "ungranted-family",
    "self-grant",
    "mutable-image-tag",
    "unpinned-remote-asset",
    "literal-credential",
    "widening-override",
    "unsupported-isolation",
    "unimplemented-version",
    "non-integer-version",
    "signature-typ-mismatch",
    "audience-mismatch",
    "digest-mismatch",
    "manifest-dropped-missing",
    "unpinned-publisher",
    "delivery-ceiling-exceeded",
    "update-dangling-reference",
    "update-trifecta-widening",
    "subblock-reference",
    "unknown-attribute",
    "missing-attribute",
    "attribute-value",
    "form-not-accepted",
    "invalid-yaml-body",
    "front-matter-yaml",
    "end-matter-yaml",
    "wire-floor",
    "network-any",
    "include-unavailable",
    "because-repeated",
    "override-guardrail",
    "override-stronger",
    // Not in Appendix B: refusals/non-integer-version pins it as its second
    // entry (the TypeScript port's schema-validation path). Raised upstream.
    "schema",
];

/// The Appendix B codes this crate never emits, each with the reason. Every
/// other code in [`CODES`] has a site that constructs it; a unit test holds
/// the two lists to that, so a code cannot go unimplemented without a word.
pub const UNDETECTED: &[(&str, &str)] = &[
    (
        "text-after-close-fence",
        "unreachable: the grammar's closeFence is `^:{3,}\\s*$`, so a line with \
         trailing text is never a close fence",
    ),
    (
        "body-forbidden",
        "unreachable: every kind with no body takes only the leaf or set form, so \
         form-not-accepted refuses a body first",
    ),
    (
        "include-unavailable",
        "an include that does not resolve degrades to the not-available note (§5.2); \
         it is not refused",
    ),
    (
        "unsupported-isolation",
        "a runtime condition of the consuming reader (agentd), not of a document",
    ),
    (
        "update-dangling-reference",
        "a live-update condition of the consuming reader (agentd), not of a document",
    ),
    (
        "update-trifecta-widening",
        "a live-update condition of the consuming reader (agentd), not of a document",
    ),
    (
        "unknown-attribute",
        "deferred: attributes are not yet refused from the schema's attribute lists",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Appendix B codes a later unit of the 1.1 re-vendor constructs, with that
    /// unit. Shrink-only: the unit that builds a code removes it here, and the
    /// accounting test fails until it does.
    const PENDING_CODES: &[(&str, &str)] = &[
        ("because-repeated", "V3"),
        ("end-matter-yaml", "V4"),
        ("override-guardrail", "V4"),
        ("override-stronger", "V4"),
        ("signature-typ-mismatch", "V8"),
        ("audience-mismatch", "V8"),
        ("digest-mismatch", "V8"),
        ("manifest-dropped-missing", "V8"),
        ("unpinned-publisher", "V8"),
        ("delivery-ceiling-exceeded", "V8"),
        ("wire-floor", "V8"),
    ];

    /// Every `.rs` file under `dir`, at any depth, sorted. Recursive, so a
    /// module that moves into a directory of its own keeps its constructors
    /// in view of the accounting.
    fn rs_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for p in std::fs::read_dir(&d)
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
            {
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files.push(p);
                }
            }
        }
        files.sort();
        files
    }

    /// The crate's non-test source, every `.rs` file under `src/` except this
    /// one (whose lists name every code), each cut at its test module.
    fn crate_source() -> String {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let own = std::path::Path::new(dir).join("refusal.rs");
        let files: Vec<_> = rs_files(std::path::Path::new(dir))
            .into_iter()
            .filter(|p| *p != own)
            .collect();
        assert!(files.len() > 1, "no crate source under {dir}");
        files
            .iter()
            .map(|p| {
                let text = std::fs::read_to_string(p).unwrap();
                match text.find("#[cfg(test)]\nmod tests") {
                    Some(at) => text[..at].to_string(),
                    None => text,
                }
            })
            .collect()
    }

    /// The code ARGUMENT of every constructor call in `src`: the second
    /// argument of `Refusal::at(line, code, …)`, the first of
    /// `Refusal::new(code, …)`. Scoped to the calls, so a code that merely
    /// appears as a word elsewhere (`"schema"` is also a workflow-step key)
    /// never counts as built. A code that is not a string literal is an
    /// error, not a guess: a code chosen through a variable is one this scan
    /// could not hold to [`CODES`], so every site must name its own.
    fn codes_in(src: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for (call, skip) in [("Refusal::at(", 1), ("Refusal::new(", 0)] {
            for (at, _) in src.match_indices(call) {
                let mut rest = src[at + call.len()..].trim_start();
                // Skip the leading arguments: up to the first comma outside
                // any bracket or string.
                for _ in 0..skip {
                    let (mut depth, mut in_str, mut escaped) = (0i32, false, false);
                    let mut end = None;
                    for (i, c) in rest.char_indices() {
                        match c {
                            _ if escaped => escaped = false,
                            '\\' if in_str => escaped = true,
                            '"' => in_str = !in_str,
                            '(' | '[' | '{' if !in_str => depth += 1,
                            ')' | ']' | '}' if !in_str => depth -= 1,
                            ',' if !in_str && depth == 0 => {
                                end = Some(i);
                                break;
                            }
                            _ => {}
                        }
                    }
                    let end = end.ok_or_else(|| format!("{call}: no code argument"))?;
                    rest = rest[end + 1..].trim_start();
                }
                let lit = rest
                    .strip_prefix('"')
                    .and_then(|r| r.find('"').map(|len| &r[..len]))
                    .ok_or_else(|| {
                        let head: String = rest.chars().take(40).collect();
                        format!("{call}: the code is not a string literal: {head:?}")
                    })?;
                out.push(lit.to_string());
            }
        }
        Ok(out)
    }

    fn constructed_codes() -> Vec<String> {
        codes_in(&crate_source()).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn the_scan_reads_the_code_argument_and_refuses_a_non_literal() {
        let src = r#"
            Refusal::at(b.line + f("x", y), "duplicate-identity", format!("x"));
            Refusal::at(
                line_no,
                "unclosed-fence",
                "never closed",
            );
            Refusal::new("schema", "x");
        "#;
        assert_eq!(
            codes_in(src).unwrap(),
            ["duplicate-identity", "unclosed-fence", "schema"]
        );
        // A code through a variable is refused rather than read as whatever
        // literal happens to come next.
        let err =
            codes_in("Refusal::at(1, code, \"x\"); Refusal::new(\"schema\", \"y\");").unwrap_err();
        assert!(err.contains("not a string literal"), "{err}");
    }

    #[test]
    fn the_scan_reaches_a_module_in_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("manifest")).unwrap();
        std::fs::write(dir.path().join("lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("manifest/mod.rs"), "").unwrap();
        std::fs::write(dir.path().join("manifest/notes.txt"), "").unwrap();
        let found: Vec<_> = rs_files(dir.path())
            .iter()
            .map(|p| p.strip_prefix(dir.path()).unwrap().to_path_buf())
            .collect();
        assert_eq!(
            found,
            [
                std::path::PathBuf::from("lib.rs"),
                std::path::PathBuf::from("manifest/mod.rs")
            ]
        );
    }

    /// Two [`UNDETECTED`] entries are unreachable only because of what the
    /// vendored schema says. Read those premises from it, so a re-vendor that
    /// makes either condition reachable fails here, naming the entry to
    /// replace with a detection site.
    #[test]
    fn the_unreachable_codes_are_still_unreachable_under_the_schema() {
        use crate::doc::{BodyKind, Form};
        let schema: serde_json::Value = serde_json::from_str(crate::doc::schema_json()).unwrap();
        let close = schema["x-grammar"]["closeFence"].as_str().unwrap();
        assert_eq!(
            close, "^:{3,}\\s*$",
            "closeFence changed — a close fence may now carry text, so \
             `text-after-close-fence` needs a detection site: remove it from UNDETECTED"
        );
        let names: Vec<String> = schema["$defs"]["kinds"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for name in &names {
            let k = crate::doc::lookup(name).unwrap();
            assert!(
                k.body != BodyKind::None
                    || k.forms.iter().all(|f| matches!(f, Form::Leaf | Form::Set)),
                "{name} has no body yet takes a form that has one ({:?}) — \
                 `body-forbidden` needs a detection site: remove it from UNDETECTED",
                k.forms
            );
        }
        assert!(
            UNDETECTED
                .iter()
                .any(|(c, _)| *c == "text-after-close-fence")
                && UNDETECTED.iter().any(|(c, _)| *c == "body-forbidden")
        );
    }

    #[test]
    fn every_code_is_built_undetected_or_pending() {
        let constructed = constructed_codes();
        let built = |code: &str| constructed.iter().any(|c| c == code);
        let undetected: Vec<&str> = UNDETECTED.iter().map(|(c, _)| *c).collect();
        let pending: Vec<&str> = PENDING_CODES.iter().map(|(c, _)| *c).collect();
        let mut problems = Vec::new();
        for code in CODES {
            match (
                built(code),
                undetected.contains(code),
                pending.contains(code),
            ) {
                (true, false, false) | (false, true, false) | (false, false, true) => {}
                (false, false, false) => problems.push(format!(
                    "{code}: no site constructs it, and it is neither UNDETECTED nor pending"
                )),
                (true, _, true) => problems.push(format!(
                    "{code}: now constructed — remove it from PENDING_CODES"
                )),
                (true, true, _) => {
                    problems.push(format!("{code}: constructed, yet listed as UNDETECTED"))
                }
                (false, true, true) => {
                    problems.push(format!("{code}: both UNDETECTED and pending"))
                }
            }
        }
        for code in undetected.iter().chain(&pending) {
            if !CODES.contains(code) {
                problems.push(format!("{code}: listed, but not in CODES"));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// Every code a constructor is handed is in [`CODES`] — the debug assert
    /// catches it only on a path a test happens to run; this reads them all.
    #[test]
    fn every_constructed_code_is_in_codes() {
        let constructed = constructed_codes();
        assert!(
            constructed.len() > 30,
            "only {} constructor calls found",
            constructed.len()
        );
        for code in &constructed {
            assert!(
                CODES.contains(&code.as_str()),
                "a constructor uses {code:?}, which is not in CODES"
            );
        }
    }

    #[test]
    fn codes_are_unique() {
        let mut sorted = CODES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), CODES.len(), "a code is listed twice");
    }

    #[test]
    fn display_carries_the_line_and_serialize_the_fixture_shape() {
        let r = Refusal::at(3, "unclosed-fence", ":::!skill is never closed");
        assert_eq!(r.to_string(), "line 3: :::!skill is never closed");
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({
                "line": 3, "code": "unclosed-fence", "message": ":::!skill is never closed",
            })
        );
        let whole = Refusal::new(
            "unimplemented-version",
            "front matter: spec \"2\" is not implemented",
        );
        assert_eq!(
            whole.to_string(),
            "front matter: spec \"2\" is not implemented"
        );
        assert_eq!(
            serde_json::to_value(&whole).unwrap()["line"],
            serde_json::Value::Null
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "is not in CODES")]
    fn a_code_outside_the_list_is_caught_where_it_is_minted() {
        let _ = Refusal::at(1, "no-such-code", "x");
    }
}
