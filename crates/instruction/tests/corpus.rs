// SPDX-License-Identifier: AGPL-3.0-only
//! **The shared conformance fixtures** (E1-06): the same cases the TypeScript
//! twin (`@instruction-md/spec`) and the Go port run, so the implementations
//! cannot drift. Three suites, one directory per case:
//!
//! - `corpus/<case>/` — `doc.md`, `context.json`, and the artifacts
//!   `delivered.txt`, `tree.json`, `manifest.json`, `manifest.canonical.json`.
//!   `context.json` carries `{"params": {...}, "facts": {...}}`, and includes
//!   resolve by front-matter `id` among the corpus documents themselves.
//! - `refusals/<case>/` — `doc.md` and `refusals.json`.
//! - `advisories/<case>/` — `doc.md` and `advisories.json`.
//!
//! The fixtures are VENDORED at `tests/conformance/` (Apache-2.0, copied
//! byte-for-byte from the specification repo) for the same reason
//! `instruction.schema.json` is: a contract that only runs where someone
//! happens to have a sibling checkout does not run. It was skipping silently in
//! CI — reporting `ok` on every hosted runner — which is how a suite reports
//! health it never performed. `the_vendored_corpus_matches_upstream_when_present`
//! is the drift check, over the whole `conformance/` tree.
//!
//! **Every vendored artifact is accounted for.** An artifact either passes, or
//! is named in [`PENDING`] — the one list of what this implementation does not
//! meet yet. An unlisted failure fails; a listed entry that now passes fails
//! until it is removed; a listed entry naming no artifact fails; and a file the
//! runners do not know how to compare fails as an unknown artifact. So a
//! re-vendor can add cases but never silently pass one this crate skips.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use instruction_core::{Context, deliver, parse, tree_json};

/// The vendored fixtures — always present, so these tests always run.
const CONFORMANCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/conformance");

/// What `conformance/` holds besides case files: the suite directories this
/// runner reads and the two documents the drift check alone compares.
const SUITES: &[&str] = &["corpus", "refusals", "advisories"];
const ROOT_FILES: &[&str] = &["README.md", "LICENSE"];

/// The artifacts each suite's case may carry, beside its `doc.md` input and an
/// optional `context.json`. Anything else is an unknown artifact.
fn artifacts_of(suite: &str) -> &'static [&'static str] {
    match suite {
        "corpus" => &[
            "doc.md",
            "delivered.txt",
            "tree.json",
            "manifest.json",
            "manifest.canonical.json",
        ],
        "refusals" => &["refusals.json"],
        "advisories" => &["advisories.json"],
        _ => &[],
    }
}

/// The vendored artifacts this implementation does not meet yet, keyed
/// `"<suite>/<case>/<file>"`, grouped by the unit of the 1.1 re-vendor that
/// closes them. Shrink-only: an entry that starts passing must be deleted
/// here, and the test says so.
const PENDING: &[&str] = &[
    // Cases new in revision 1.1 (S8-S26): the forms, notes, end matter,
    // variants, typed parameters and overrides they exercise land in
    // units V3-V6.
    "corpus/author-notes/delivered.txt",
    "corpus/author-notes/tree.json",
    "corpus/conditions/delivered.txt",
    "corpus/conditions/tree.json",
    "corpus/end-matter/delivered.txt",
    "corpus/end-matter/tree.json",
    "corpus/eval/delivered.txt",
    "corpus/eval/doc.md",
    "corpus/eval/tree.json",
    "corpus/examples-avoid/delivered.txt",
    "corpus/examples-avoid/tree.json",
    "corpus/labels-plain/delivered.txt",
    "corpus/labels-plain/tree.json",
    "corpus/labels-tags/delivered.txt",
    "corpus/labels-tags/tree.json",
    "corpus/named-rules/delivered.txt",
    "corpus/named-rules/doc.md",
    "corpus/named-rules/tree.json",
    "corpus/output/delivered.txt",
    "corpus/output/tree.json",
    "corpus/overrides-house/delivered.txt",
    "corpus/overrides-house/tree.json",
    "corpus/overrides/delivered.txt",
    "corpus/overrides/tree.json",
    "corpus/parameter-types/delivered.txt",
    "corpus/parameter-types/tree.json",
    "corpus/permissions/delivered.txt",
    "corpus/permissions/tree.json",
    "corpus/reasons/delivered.txt",
    "corpus/reasons/tree.json",
    "corpus/should-not/delivered.txt",
    "corpus/should-not/tree.json",
    "corpus/skill-trigger/delivered.txt",
    "corpus/skill-trigger/tree.json",
    "corpus/variants/delivered.txt",
    "corpus/variants/tree.json",
    "corpus/verbatim/tree.json",
    // Delivered text upstream changed for 1.1 (the skill-trigger
    // acknowledgement; research-agent's enum-typed parameter) — units V5-V6.
    "corpus/coding-agent/delivered.txt",
    "corpus/research-agent/delivered.txt",
    "corpus/spec-example/delivered.txt",
    "corpus/support-agent/delivered.txt",
    // The S7 resolution manifest — unit V7.
    "corpus/author-notes/manifest.canonical.json",
    "corpus/author-notes/manifest.json",
    "corpus/coding-agent/manifest.canonical.json",
    "corpus/coding-agent/manifest.json",
    "corpus/conditions/manifest.canonical.json",
    "corpus/conditions/manifest.json",
    "corpus/deploy-runbook/manifest.canonical.json",
    "corpus/deploy-runbook/manifest.json",
    "corpus/end-matter/manifest.canonical.json",
    "corpus/end-matter/manifest.json",
    "corpus/eval/manifest.canonical.json",
    "corpus/eval/manifest.json",
    "corpus/examples-avoid/manifest.canonical.json",
    "corpus/examples-avoid/manifest.json",
    "corpus/house-style/manifest.canonical.json",
    "corpus/house-style/manifest.json",
    "corpus/labels-plain/manifest.canonical.json",
    "corpus/labels-plain/manifest.json",
    "corpus/labels-tags/manifest.canonical.json",
    "corpus/labels-tags/manifest.json",
    "corpus/named-rules/manifest.canonical.json",
    "corpus/named-rules/manifest.json",
    "corpus/nested-keyword/manifest.canonical.json",
    "corpus/nested-keyword/manifest.json",
    "corpus/orchestrator/manifest.canonical.json",
    "corpus/orchestrator/manifest.json",
    "corpus/output/manifest.canonical.json",
    "corpus/output/manifest.json",
    "corpus/overrides-house/manifest.canonical.json",
    "corpus/overrides-house/manifest.json",
    "corpus/overrides/manifest.canonical.json",
    "corpus/overrides/manifest.json",
    "corpus/param-flags/manifest.canonical.json",
    "corpus/param-flags/manifest.json",
    "corpus/parameter-types/manifest.canonical.json",
    "corpus/parameter-types/manifest.json",
    "corpus/permissions/manifest.canonical.json",
    "corpus/permissions/manifest.json",
    "corpus/reasons/manifest.canonical.json",
    "corpus/reasons/manifest.json",
    "corpus/research-agent/manifest.canonical.json",
    "corpus/research-agent/manifest.json",
    "corpus/should-not/manifest.canonical.json",
    "corpus/should-not/manifest.json",
    "corpus/skill-trigger/manifest.canonical.json",
    "corpus/skill-trigger/manifest.json",
    "corpus/spec-example/manifest.canonical.json",
    "corpus/spec-example/manifest.json",
    "corpus/support-agent/manifest.canonical.json",
    "corpus/support-agent/manifest.json",
    "corpus/variants/manifest.canonical.json",
    "corpus/variants/manifest.json",
    "corpus/verbatim/manifest.canonical.json",
    "corpus/verbatim/manifest.json",
    // Refusals new in revision 1.1 — units V3-V4.
    "refusals/because-repeated/refusals.json",
    "refusals/duplicate-named-keyword/refusals.json",
    "refusals/output-schema-not-a-reference/refusals.json",
    "refusals/override-guardrail/refusals.json",
    "refusals/override-stronger/refusals.json",
    // The S19 advisories — unit V9, which deletes this list.
    "advisories/empty-variant/advisories.json",
    "advisories/keyword-in-example/advisories.json",
    "advisories/near-miss-keyword/advisories.json",
    "advisories/nothing-inside-notes-or-code/advisories.json",
    "advisories/orphan-because/advisories.json",
    "advisories/otherwise-without-a-group/advisories.json",
    "advisories/overrides-external/advisories.json",
    "advisories/parameter-default-type/advisories.json",
    "advisories/sigil-scheme-mismatch/advisories.json",
    "advisories/skill-when-alias/advisories.json",
    "advisories/undeclared-and-unused-parameters/advisories.json",
    "advisories/unqualified-wikilink/advisories.json",
    "advisories/when-unknown-key/advisories.json",
];

/// One suite run's outcome per artifact key, judged against [`PENDING`].
#[derive(Default)]
struct Ledger {
    outcomes: BTreeMap<String, Result<(), String>>,
    /// Problems with the fixture tree itself (unknown files, a case with no
    /// `doc.md`) — never pending-able.
    structural: Vec<String>,
}

impl Ledger {
    fn record(&mut self, key: String, outcome: Result<(), String>) {
        self.outcomes.insert(key, outcome);
    }

    /// Fail on every unlisted failure, every listed pass, and every listed
    /// key of `suite` this run never recorded; print the counts.
    fn finish(self, suite: &str) {
        let prefix = format!("{suite}/");
        let pending: BTreeSet<&str> = PENDING
            .iter()
            .copied()
            .filter(|k| k.starts_with(&prefix))
            .collect();
        let mut failures = self.structural;
        // A PENDING key outside every suite is checked by every run, so a
        // typo'd suite cannot hide in a list no runner reads.
        for key in PENDING {
            if !SUITES.iter().any(|s| key.starts_with(&format!("{s}/"))) {
                failures.push(format!("PENDING {key:?} names no suite"));
            }
        }
        let (mut passed, mut pending_failed) = (0, 0);
        for (key, outcome) in &self.outcomes {
            match (outcome, pending.contains(key.as_str())) {
                (Ok(()), false) => passed += 1,
                (Err(_), true) => pending_failed += 1,
                (Ok(()), true) => {
                    failures.push(format!("{key}: now passes — remove it from PENDING"))
                }
                (Err(why), false) => failures.push(format!("{key}: {why}")),
            }
        }
        for key in &pending {
            if !self.outcomes.contains_key(*key) {
                let exists = Path::new(CONFORMANCE).join(key).is_file();
                failures.push(if exists {
                    format!("PENDING {key:?} is not an artifact this runner compares")
                } else {
                    format!("PENDING {key:?} names a nonexistent file")
                });
            }
        }
        eprintln!(
            "{suite}: {} artifacts — {passed} pass, {pending_failed} pending, {} failing",
            self.outcomes.len(),
            failures.len()
        );
        assert!(
            failures.is_empty(),
            "{} {suite} failures:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

/// The suite's cases, in order, with every file in each case directory
/// checked against what the suite knows (`doc.md`, `context.json`, and
/// [`artifacts_of`]). A stray file at the suite root, a case without
/// `doc.md`, or an unknown file is recorded as structural.
fn cases(suite: &str, ledger: &mut Ledger) -> Vec<PathBuf> {
    let root = Path::new(CONFORMANCE).join(suite);
    assert!(
        root.is_dir(),
        "{root:?} missing — the fixtures are vendored in-tree and must be present; \
         a skipped conformance run is not a passing one"
    );
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root).unwrap().filter_map(|e| e.ok()) {
        let dir = entry.path();
        let case = entry.file_name().to_string_lossy().into_owned();
        if !dir.is_dir() {
            ledger
                .structural
                .push(format!("{suite}/{case}: unknown artifact (not a case)"));
            continue;
        }
        if !dir.join("doc.md").is_file() {
            ledger
                .structural
                .push(format!("{suite}/{case}: case has no doc.md"));
        }
        for f in std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()) {
            let file = f.file_name().to_string_lossy().into_owned();
            if file != "doc.md" && file != "context.json" && !artifacts_of(suite).contains(&&*file)
            {
                ledger
                    .structural
                    .push(format!("{suite}/{case}/{file}: unknown artifact"));
            }
        }
        out.push(dir);
    }
    out.sort();
    assert!(!out.is_empty(), "{root:?} present but empty");
    out
}

fn case_name(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

fn refused(errs: &[instruction_core::Refusal]) -> String {
    errs.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[test]
fn the_shared_corpus_delivers_byte_exactly() {
    let mut ledger = Ledger::default();
    // `conformance/` itself holds the suites and the two root documents; a
    // file or directory beyond those is an artifact no runner compares.
    for entry in std::fs::read_dir(CONFORMANCE)
        .unwrap()
        .filter_map(|e| e.ok())
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let known = if entry.path().is_dir() {
            SUITES.contains(&&*name)
        } else {
            ROOT_FILES.contains(&&*name)
        };
        if !known {
            ledger
                .structural
                .push(format!("{name}: unknown artifact in conformance/"));
        }
    }
    let cases = cases("corpus", &mut ledger);

    // Build the include resolver: front-matter `id` → document text, over the
    // whole corpus.
    let mut by_id: BTreeMap<String, String> = BTreeMap::new();
    for dir in &cases {
        let Ok(text) = std::fs::read_to_string(dir.join("doc.md")) else {
            continue;
        };
        if let Ok(d) = parse(&text)
            && let Some(id) = d.front.get("id").and_then(|v| v.as_str())
        {
            by_id.insert(id.to_string(), text.clone());
            // Also index by the short id (`ins_x` and bare `x`).
            if let Some(short) = id.rsplit('/').next() {
                by_id.insert(short.to_string(), text.clone());
                if let Some(bare) = short.strip_prefix("ins_") {
                    by_id.insert(bare.to_string(), text);
                }
            }
        }
    }

    let resolver = |id: &str| by_id.get(id).cloned();
    for dir in &cases {
        let name = case_name(dir);
        let key = |file: &str| format!("corpus/{name}/{file}");
        let Ok(text) = std::fs::read_to_string(dir.join("doc.md")) else {
            continue;
        };
        let ctx_path = dir.join("context.json");
        let ctx_v: serde_json::Value = if ctx_path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&ctx_path).unwrap()).unwrap()
        } else {
            serde_json::json!({})
        };
        let map_of = |key: &str| -> BTreeMap<String, String> {
            ctx_v[key]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        };
        let ctx = Context {
            grants: instruction_core::doc::all_families(),
            params: map_of("params"),
            facts: map_of("facts"),
            resolve_include: Some(&resolver),
        };

        // doc.md: a corpus document parses and validates under every family
        // without one refusal (the Go port's
        // TestCorpusParsesAndValidatesWithoutRefusals). A document that does
        // not parse has no other artifact to compare, so each is recorded as
        // failing for that reason rather than vanishing from the count.
        let doc = match parse(&text) {
            Ok(d) => d,
            Err(errs) => {
                let why = format!("refused: {}", refused(&errs));
                ledger.record(key("doc.md"), Err(why.clone()));
                for artifact in artifacts_of("corpus") {
                    if *artifact != "doc.md" && dir.join(artifact).exists() {
                        ledger.record(key(artifact), Err(format!("doc.md {why}")));
                    }
                }
                continue;
            }
        };
        let errs = instruction_core::validate(&doc, &ctx);
        ledger.record(
            key("doc.md"),
            if errs.is_empty() {
                Ok(())
            } else {
                Err(format!("validation refused: {}", refused(&errs)))
            },
        );

        if dir.join("delivered.txt").exists() {
            let want = std::fs::read_to_string(dir.join("delivered.txt")).unwrap();
            ledger.record(
                key("delivered.txt"),
                match deliver(&doc, &ctx) {
                    Ok(out) if out.text == want => Ok(()),
                    Ok(out) => Err(format!(
                        "delivered text differs (got {} bytes, want {})\n--- first diff ---\n{}",
                        out.text.len(),
                        want.len(),
                        first_diff(&want, &out.text)
                    )),
                    Err(errs) => Err(format!("delivery refused: {}", refused(&errs))),
                },
            );
        }
        if dir.join("tree.json").exists() {
            let want: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("tree.json")).unwrap())
                    .unwrap();
            ledger.record(
                key("tree.json"),
                if tree_json(&doc) == want {
                    Ok(())
                } else {
                    Err("tree.json differs".into())
                },
            );
        }
        // The §7 resolution manifest (S7) has no comparator in this crate yet;
        // recording the artifact as failing keeps it on the PENDING ledger
        // instead of out of sight.
        for manifest in ["manifest.json", "manifest.canonical.json"] {
            if dir.join(manifest).exists() {
                ledger.record(key(manifest), Err("not compared yet — S7 (unit V7)".into()));
            }
        }
    }
    eprintln!("shared corpus: {} cases", cases.len());
    ledger.finish("corpus");
}

fn first_diff(want: &str, got: &str) -> String {
    for (i, (w, g)) in want.lines().zip(got.lines()).enumerate() {
        if w != g {
            return format!("line {}:\nwant: {w}\n got: {g}", i + 1);
        }
    }
    format!(
        "line count differs: want {} got {}",
        want.lines().count(),
        got.lines().count()
    )
}

#[test]
fn the_shared_refusal_corpus_matches() {
    let mut ledger = Ledger::default();
    let cases = cases("refusals", &mut ledger);
    for dir in &cases {
        let name = case_name(dir);
        let (Ok(text), Ok(want)) = (
            std::fs::read_to_string(dir.join("doc.md")),
            std::fs::read_to_string(dir.join("refusals.json")),
        ) else {
            ledger.structural.push(format!(
                "refusals/{name}: a refusal case needs doc.md and refusals.json"
            ));
            continue;
        };
        let want: Vec<serde_json::Value> = serde_json::from_str(&want).unwrap();
        let got: Vec<instruction_core::Refusal> = match parse(&text) {
            Err(errs) => errs,
            Ok(d) => instruction_core::validate(
                &d,
                &Context {
                    grants: instruction_core::doc::all_families(),
                    ..Context::default()
                },
            ),
        };
        // The matching rule both runners use: message equality, and line
        // equality when the fixture's line is not null.
        let mut msgs = Vec::new();
        for w in &want {
            let wmsg = w["message"].as_str().unwrap_or("");
            let wline = w["line"].as_u64();
            let hit = got.iter().any(|g| {
                g.message_body() == wmsg && (wline.is_none() || g.line.map(u64::from) == wline)
            });
            if !hit {
                msgs.push(format!(
                    "  want {:?} (line {:?}); got: {}",
                    wmsg,
                    wline,
                    got.iter()
                        .map(|g| format!("[{:?}] {:?}", g.line, g.message_body()))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
        }
        ledger.record(
            format!("refusals/{name}/refusals.json"),
            if msgs.is_empty() {
                Ok(())
            } else {
                Err(format!("refusals differ:\n{}", msgs.join("\n")))
            },
        );
    }
    eprintln!("refusal corpus: {} cases", cases.len());
    ledger.finish("refusals");
}

/// The §19 advisories (S19): diagnostics that never block delivery. This
/// crate emits none yet, so every case is recorded as not compared — on the
/// PENDING ledger, where a re-vendor that adds one cannot pass unseen.
#[test]
fn the_shared_advisory_corpus_matches() {
    let mut ledger = Ledger::default();
    let cases = cases("advisories", &mut ledger);
    for dir in &cases {
        let name = case_name(dir);
        if dir.join("advisories.json").is_file() {
            ledger.record(
                format!("advisories/{name}/advisories.json"),
                Err("not compared yet — S19 (unit V9)".into()),
            );
        } else {
            ledger.structural.push(format!(
                "advisories/{name}: an advisory case needs advisories.json"
            ));
        }
    }
    eprintln!("advisory corpus: {} cases", cases.len());
    ledger.finish("advisories");
}

/// The vendored fixtures must equal the specification's, when a checkout of
/// it is at hand: the WHOLE `conformance/` tree, every file byte for byte —
/// the suites, `README.md` and `LICENSE` alike. Same contract as the vendored
/// schema: vendoring buys "it always runs", and this buys "it is still the
/// spec's corpus and not a local fork". CI checks out the pinned spec revision
/// and `scripts/ci-gate.sh` refuses a run of this test that skipped.
#[test]
fn the_vendored_corpus_matches_upstream_when_present() {
    let explicit = std::env::var("INSTRUCTION_SPEC_REPO");
    let upstream = explicit
        .clone()
        .unwrap_or_else(|_| "/root/instruction-md/specification".into());
    let up = Path::new(&upstream).join("conformance");
    if !up.exists() {
        assert!(
            explicit.is_err(),
            "INSTRUCTION_SPEC_REPO={upstream:?} was set but has no conformance/ — \
             fail, not skip"
        );
        eprintln!("no upstream checkout; drift check skipped");
        return;
    }
    let list = |root: &Path| -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(bytes) = std::fs::read(&p)
                    && let Ok(rel) = p.strip_prefix(root)
                {
                    out.insert(rel.to_string_lossy().into_owned(), bytes);
                }
            }
        }
        out
    };
    let (uf, vf) = (list(&up), list(Path::new(CONFORMANCE)));
    let mut differences = Vec::new();
    for (name, bytes) in &uf {
        match vf.get(name) {
            None => differences.push(format!("{name}: upstream file is not vendored")),
            Some(mine) if mine != bytes => {
                differences.push(format!("{name}: vendored copy differs from upstream"))
            }
            _ => {}
        }
    }
    for name in vf.keys() {
        if !uf.contains_key(name) {
            differences.push(format!("{name}: vendored file is not upstream"));
        }
    }
    assert!(
        differences.is_empty(),
        "the vendored conformance fixtures have drifted from {upstream}:\n  {}\n\
         re-vendor with: rm -rf crates/instruction/tests/conformance && \
         mkdir crates/instruction/tests/conformance && \
         cp -r {upstream}/conformance/. crates/instruction/tests/conformance/",
        differences.join("\n  ")
    );
    eprintln!(
        "conformance drift check: {} files identical to {upstream}",
        uf.len()
    );
}
