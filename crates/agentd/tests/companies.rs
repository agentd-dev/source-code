// SPDX-License-Identifier: AGPL-3.0-only
//! The Personal Agent Protocol draft agentd is pinned to (rfcs/0047 §9, P0):
//! the copy under `tests/companies/vendor/` is the draft at the commit its
//! `PIN` names, and every rule its JSON Schemas state is one agentd's
//! validator enforces.
//!
//! [`agentd::jsonschema`] has no regex engine and resolves `$ref` within one
//! document, and the draft's schemas use regex `pattern`s and references
//! from one file into another. The fake Company holds its own documents to
//! those schemas through the kit (`companies::mock::kit`), which supplies a
//! hand-written check per pattern and the other files. The forcing tests here
//! keep that true across a re-pin: a pattern with no check, a vendored file
//! the kit does not load, a keyword the validator would skip, or a
//! reference that leads nowhere fails until it is handled.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

const VENDOR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/companies/vendor");

/// The `PIN` file, read: its keys and its file mappings.
struct Pin {
    commit: String,
    /// `(vendored, upstream, is_dir)`.
    maps: Vec<(String, String, bool)>,
}

fn pin() -> Pin {
    let text = std::fs::read_to_string(format!("{VENDOR}/PIN")).expect("the PIN file");
    let mut commit = None;
    let mut maps = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["commit", c] => commit = Some(c.to_string()),
            ["file", v, u] => maps.push((v.to_string(), u.to_string(), false)),
            ["dir", v, u] => maps.push((v.to_string(), u.to_string(), true)),
            ["repository" | "date", _] => {}
            other => panic!("PIN: a line this test does not read: {other:?}"),
        }
    }
    Pin {
        commit: commit.expect("PIN names a commit"),
        maps,
    }
}

/// Every file under `root`, by its path relative to it.
fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let (Ok(bytes), Ok(rel)) = (std::fs::read(&p), p.strip_prefix(root)) {
                out.insert(rel.to_string_lossy().into_owned(), bytes);
            }
        }
    }
    out
}

/// The vendored schemas, parsed, by file name.
fn schemas() -> BTreeMap<String, Value> {
    files(&Path::new(VENDOR).join("schemas"))
        .into_iter()
        .map(|(name, bytes)| {
            let v = serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("schemas/{name} is not JSON: {e}"));
            (name, v)
        })
        .collect()
}

/// The PIN names one commit — the one CI checks the drift test out against —
/// and maps every vendored file, each to one upstream path.
#[test]
fn the_pin_names_one_commit_and_maps_every_vendored_file() {
    let pin = pin();
    assert!(
        pin.commit.len() == 40 && pin.commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "PIN: the commit is a full 40-hex sha: {:?}",
        pin.commit
    );
    let mut covered = BTreeSet::new();
    for (v, _, is_dir) in &pin.maps {
        let path = Path::new(VENDOR).join(v);
        if *is_dir {
            assert!(
                path.is_dir(),
                "PIN maps the directory {v}, which is not vendored"
            );
            for f in files(&path).into_keys() {
                covered.insert(format!("{v}/{f}"));
            }
        } else {
            assert!(
                path.is_file(),
                "PIN maps the file {v}, which is not vendored"
            );
            covered.insert(v.clone());
        }
    }
    let unmapped: Vec<String> = files(Path::new(VENDOR))
        .into_keys()
        .filter(|f| f != "PIN" && !covered.contains(f))
        .collect();
    assert!(
        unmapped.is_empty(),
        "vendored files the PIN does not say the origin of: {unmapped:?}"
    );
    // CI checks the upstream out at the pin for the drift test below; a
    // re-pin that moves only one of the two would compare against the
    // wrong revision.
    let ci = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows/ci.yml"
    ))
    .expect("ci.yml");
    let checkout = ci
        .split("repository: personal-agent-protocol/personal-agent-protocol")
        .nth(1)
        .expect("ci.yml checks the draft's repository out");
    let ci_ref = checkout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ref: "))
        .expect("the checkout pins a ref");
    assert_eq!(
        ci_ref.trim(),
        pin.commit,
        "ci.yml checks out another revision than the PIN"
    );
}

/// The upstream checkout's commit, when git can say.
fn head_of(checkout: &Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "a revision git cannot name".to_string())
}

/// Drift check against a checkout of the draft's repository, when one is
/// named: every vendored file must equal its upstream original, byte for
/// byte, and a vendored directory must hold exactly the upstream one's
/// files. Set `COMPANIES_DRAFT_REPO` to a checkout of the PIN's commit; CI
/// does, and `scripts/ci-gate.sh` refuses a run that skipped. A path that is
/// named but is not a checkout fails rather than skips: a drift check that
/// skips on a bad path reports health it never performed.
#[test]
fn the_vendored_draft_matches_upstream_when_present() {
    let Ok(upstream) = std::env::var("COMPANIES_DRAFT_REPO") else {
        eprintln!("COMPANIES_DRAFT_REPO is not set; drift check skipped");
        return;
    };
    let up = PathBuf::from(&upstream);
    assert!(
        up.join("kit").is_dir(),
        "COMPANIES_DRAFT_REPO={upstream:?} has no kit/: not a checkout of the draft's repository — fail, not skip"
    );
    let pin = pin();
    let mut differences = Vec::new();
    for (v, u, is_dir) in &pin.maps {
        let (mine, theirs) = (Path::new(VENDOR).join(v), up.join(u));
        if *is_dir {
            let (vf, uf) = (files(&mine), files(&theirs));
            for (name, bytes) in &uf {
                match vf.get(name) {
                    None => differences.push(format!("{u}/{name}: upstream file is not vendored")),
                    Some(b) if b != bytes => {
                        differences.push(format!("{v}/{name} differs from {u}/{name}"))
                    }
                    _ => {}
                }
            }
            for name in vf.keys().filter(|n| !uf.contains_key(*n)) {
                differences.push(format!("{v}/{name}: vendored file is not upstream"));
            }
        } else {
            match (std::fs::read(&mine), std::fs::read(&theirs)) {
                (Ok(a), Ok(b)) if a == b => {}
                (Ok(_), Ok(_)) => differences.push(format!("{v} differs from {u}")),
                (_, Err(e)) => differences.push(format!("{u}: {e}")),
                (Err(e), _) => differences.push(format!("{v}: {e}")),
            }
        }
    }
    assert!(
        differences.is_empty(),
        "the vendored draft has drifted from {upstream}, at {}, pinned at {}:\n  {}\n\
         If the checkout is at the pin, a vendored file was edited: restore it. \
         Otherwise upstream has moved, and a re-pin is a deliberate change (see PIN).",
        head_of(&up),
        pin.commit,
        differences.join("\n  ")
    );
    eprintln!(
        "draft drift check: every vendored file identical to {upstream} ({})",
        head_of(&up)
    );
}

/// The keywords [`agentd::jsonschema`] enforces, and those it reads as
/// annotations; `pattern` is enforced through the kit's checks.
const ENFORCED: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "patternProperties",
    "enum",
    "const",
    "items",
    "prefixItems",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "minProperties",
    "maxProperties",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "$ref",
    "contains",
    "dependentRequired",
    "pattern",
];
const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "$defs",
    "definitions",
    "title",
    "description",
    "examples",
    "default",
];

/// Every keyword at a schema position under `schema`, and every `$ref`, with
/// the pointer it is at.
fn walk(
    schema: &Value,
    at: &str,
    keywords: &mut Vec<(String, String)>,
    refs: &mut Vec<(String, String)>,
) {
    let Value::Object(m) = schema else { return };
    for (k, v) in m {
        keywords.push((at.to_string(), k.clone()));
        let here = format!("{at}/{k}");
        match k.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" => {
                for (name, sub) in v.as_object().into_iter().flatten() {
                    walk(sub, &format!("{here}/{name}"), keywords, refs);
                }
            }
            "items" | "additionalProperties" | "not" | "if" | "then" | "else" | "contains" => {
                walk(v, &here, keywords, refs)
            }
            "prefixItems" | "allOf" | "anyOf" | "oneOf" => {
                for (i, sub) in v.as_array().into_iter().flatten().enumerate() {
                    walk(sub, &format!("{here}/{i}"), keywords, refs);
                }
            }
            "$ref" => refs.push((here, v.as_str().unwrap_or_default().to_string())),
            _ => {}
        }
    }
}

/// Every keyword the vendored schemas use is one the validator enforces or
/// reads as an annotation. A re-pin that brings `format`, say, would
/// otherwise be validated as if the keyword were not there.
#[test]
fn every_vendored_keyword_is_enforced() {
    let mut unknown = Vec::new();
    for (name, schema) in schemas() {
        let (mut keywords, mut refs) = (Vec::new(), Vec::new());
        walk(&schema, "", &mut keywords, &mut refs);
        for (at, k) in keywords {
            if !ENFORCED.contains(&k.as_str()) && !ANNOTATIONS.contains(&k.as_str()) {
                unknown.push(format!("{name}#{at}: {k}"));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "keywords the validator would skip:\n  {}",
        unknown.join("\n  ")
    );
}

/// The kit: the vendored schemas as the fake Company loads them, the checks
/// for their patterns, and the resolver of their references.
#[cfg(all(
    feature = "oauth",
    feature = "tls",
    any(debug_assertions, feature = "internal-mocks")
))]
mod kit {
    use super::*;
    use agentd::companies::mock::kit::{self, PATTERNS};
    use serde_json::json;

    /// Every regex `pattern` in the vendored schemas, with where it is.
    fn patterns() -> BTreeMap<String, Vec<String>> {
        fn collect(v: &Value, at: &str, out: &mut BTreeMap<String, Vec<String>>) {
            match v {
                Value::Object(m) => {
                    for (k, sub) in m {
                        let here = format!("{at}/{k}");
                        if let ("pattern", Value::String(p)) = (k.as_str(), sub) {
                            out.entry(p.clone()).or_default().push(at.to_string());
                        } else {
                            collect(sub, &here, out);
                        }
                    }
                }
                Value::Array(a) => {
                    for (i, sub) in a.iter().enumerate() {
                        collect(sub, &format!("{at}/{i}"), out);
                    }
                }
                _ => {}
            }
        }
        let mut out = BTreeMap::new();
        for (name, schema) in schemas() {
            collect(&schema, &name, &mut out);
        }
        out
    }

    /// Each regex pattern the vendored schemas carry has its named,
    /// hand-written check, and each check is for a pattern they still carry:
    /// a re-pin that adds a pattern fails here until it is checked, and
    /// one that drops a pattern leaves no check behind.
    #[test]
    fn every_vendored_pattern_has_a_check() {
        let found = patterns();
        let unchecked: Vec<String> = found
            .iter()
            .filter(|(p, _)| kit::check_for(p).is_none())
            .map(|(p, at)| format!("{p:?} at {}", at.join(", ")))
            .collect();
        assert!(
            unchecked.is_empty(),
            "patterns with no check in companies::mock::kit::PATTERNS:\n  {}",
            unchecked.join("\n  ")
        );
        let stale: Vec<&str> = PATTERNS
            .iter()
            .filter(|c| !found.contains_key(c.pattern))
            .map(|c| c.name)
            .collect();
        assert!(
            stale.is_empty(),
            "checks for patterns no vendored schema has: {stale:?}"
        );
        let mut names: Vec<&str> = PATTERNS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), PATTERNS.len(), "two checks share a name");
        for c in PATTERNS {
            assert!(
                !c.accepts.is_empty() && !c.rejects.is_empty(),
                "{}: a check with no examples either way is not shown to be its pattern's",
                c.name
            );
        }
    }

    /// The kit loads every vendored schema, as it is on disk.
    #[test]
    fn the_kit_loads_every_vendored_schema() {
        let on_disk = schemas();
        let loaded: BTreeSet<&str> = kit::SCHEMAS.iter().map(|(n, _)| *n).collect();
        let vendored: BTreeSet<&str> = on_disk.keys().map(String::as_str).collect();
        assert_eq!(
            loaded, vendored,
            "the kit and the vendored directory list different schemas"
        );
        for (name, schema) in &on_disk {
            assert_eq!(
                kit::kit().schema(name),
                Some(schema),
                "{name}: the kit does not hold the vendored schema"
            );
            assert_eq!(
                schema["$id"],
                json!(name),
                "{name}: its $id is not its file name"
            );
        }
    }

    /// Every `$ref` leads somewhere — a local pointer into its own document,
    /// or another vendored file and a pointer into that one — and the files
    /// referenced across are loaded where they are referenced: content that
    /// breaks only the referenced file's rules is refused through the
    /// reference.
    #[test]
    fn cross_document_references_resolve() {
        let all = schemas();
        let mut across = BTreeSet::new();
        for (name, schema) in &all {
            let (mut keywords, mut refs) = (Vec::new(), Vec::new());
            walk(schema, "", &mut keywords, &mut refs);
            for (at, r) in refs {
                let (doc, frag) = r.split_once('#').unwrap_or((r.as_str(), ""));
                let target = if doc.is_empty() {
                    schema
                } else {
                    across.insert(doc.to_string());
                    all.get(doc)
                        .unwrap_or_else(|| panic!("{name}#{at}: $ref {r:?} names no vendored file"))
                };
                assert!(
                    frag.is_empty() || target.pointer(frag).is_some(),
                    "{name}#{at}: $ref {r:?} points at nothing"
                );
            }
        }
        assert_eq!(
            across,
            [
                "message.schema.json",
                "token-response.schema.json",
                "event.schema.json"
            ]
            .map(String::from)
            .into(),
            "the files referenced across documents"
        );
        let k = kit::kit();
        let message = json!({"id": "msg_1", "sender": "agent", "text": "hello"});
        // event → message: the message's own `#/$defs/id` and its `sender`.
        let event = |message: Value| json!({"id": "evt_1", "type": "message", "created_at": "2026-10-16T17:08:46Z", "message": message});
        let mut with_role = message.clone();
        with_role["role"] = json!("company");
        assert!(
            k.validate("event.schema.json", &event(with_role.clone()))
                .is_ok()
        );
        assert!(
            k.validate("event.schema.json", &event(message.clone()))
                .is_err(),
            "an event's message needs a role"
        );
        let mut bad = with_role.clone();
        bad["id"] = json!("msg 1");
        assert!(k.validate("event.schema.json", &event(bad)).is_err());
        // events-response → event → message.
        let read = |events: Value| json!({"events": events, "cursor": null, "has_more": false, "status": "idle", "responder": "agent"});
        assert!(
            k.validate(
                "events-response.schema.json",
                &read(json!([event(with_role.clone())]))
            )
            .is_ok()
        );
        let mut no_sender = with_role.clone();
        no_sender.as_object_mut().unwrap().remove("sender");
        assert!(
            k.validate(
                "events-response.schema.json",
                &read(json!([event(no_sender)]))
            )
            .is_err()
        );
        // conversation-request → message.
        assert!(
            k.validate(
                "conversation-request.schema.json",
                &json!({"message": message})
            )
            .is_ok()
        );
        assert!(
            k.validate(
                "conversation-request.schema.json",
                &json!({"message": {"id": "m", "sender": "agent"}})
            )
            .is_err(),
            "a message carries text, data or context"
        );
        // mediated-sign-in-response → token-response.
        let complete = json!({"status": "complete", "access_token": "a", "token_type": "DPoP", "expires_in": 60,
            "scope": "", "session_id": "s", "signed_in": true, "refresh_token": "r"});
        assert!(
            k.validate("mediated-sign-in-response.schema.json", &complete)
                .is_ok()
        );
        let mut bad = complete.clone();
        bad["token_type"] = json!("mac");
        assert!(
            k.validate("mediated-sign-in-response.schema.json", &bad)
                .is_err()
        );
    }
}
