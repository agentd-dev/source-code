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

// The case reading the fixture dumper shares, so `dump` reproduces what this
// runner compares.
#[path = "support/corpus_case.rs"]
mod corpus_case;

/// The vendored fixtures — always present, so these tests always run.
const CONFORMANCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/conformance");

/// What `conformance/` holds besides case files: the suite directories this
/// runner reads and the two documents the drift check alone compares.
const SUITES: &[&str] = &["corpus", "refusals", "advisories"];
const ROOT_FILES: &[&str] = &["README.md", "LICENSE"];

/// The artifacts each suite's case may carry, beside its `doc.md` input (and,
/// for the corpus, an optional `context.json` — see [`inputs_of`]). Anything
/// else is an unknown artifact.
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

/// The input files a suite's case may carry. Only the corpus runner reads a
/// `context.json` (params, facts); the refusal runner validates under every
/// grant and the advisory runner reads nothing. So a `context.json` anywhere
/// else is an unknown artifact — a refusal case that meant limited grants
/// would otherwise be judged under all of them without a word.
fn inputs_of(suite: &str) -> &'static [&'static str] {
    match suite {
        "corpus" => &["doc.md", "context.json"],
        _ => &["doc.md"],
    }
}

/// The vendored artifacts this implementation does not meet yet, keyed
/// `"<suite>/<case>/<file>"`, grouped by the unit of the 1.1 re-vendor that
/// closes them. Shrink-only: an entry that starts passing must be deleted
/// here, and the test says so.
const PENDING: &[&str] = &[
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
        let total = self.outcomes.len();
        let verdict = self.judge(suite, PENDING, Path::new(CONFORMANCE));
        eprintln!(
            "{suite}: {total} artifacts — {} pass, {} pending, {} failing",
            verdict.passed,
            verdict.pending_failed,
            verdict.failures.len()
        );
        assert!(
            verdict.failures.is_empty(),
            "{} {suite} failures:\n{}",
            verdict.failures.len(),
            verdict.failures.join("\n")
        );
    }

    /// The judgment `finish` asserts on, against a given pending list and
    /// fixture root — apart from the live run, so each of its rules has a
    /// test of its own (see `the_ledger_*` below).
    fn judge(self, suite: &str, pending_list: &[&str], root: &Path) -> Verdict {
        let prefix = format!("{suite}/");
        let pending: BTreeSet<&str> = pending_list
            .iter()
            .copied()
            .filter(|k| k.starts_with(&prefix))
            .collect();
        let mut failures = self.structural;
        // A PENDING key outside every suite is checked by every run, so a
        // typo'd suite cannot hide in a list no runner reads.
        for key in pending_list {
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
                let exists = root.join(key).is_file();
                failures.push(if exists {
                    format!("PENDING {key:?} is not an artifact this runner compares")
                } else {
                    format!("PENDING {key:?} names a nonexistent file")
                });
            }
        }
        Verdict {
            failures,
            passed,
            pending_failed,
        }
    }
}

/// What [`Ledger::judge`] found: every failure, and the counts it printed.
struct Verdict {
    failures: Vec<String>,
    passed: usize,
    pending_failed: usize,
}

/// The suite's cases, in order, with every file in each case directory
/// checked against what the suite knows ([`inputs_of`] and
/// [`artifacts_of`]). A stray file at the suite root, a case without
/// `doc.md`, or an unknown file is recorded as structural.
fn cases(suite: &str, ledger: &mut Ledger) -> Vec<PathBuf> {
    cases_in(Path::new(CONFORMANCE), suite, ledger)
}

fn cases_in(conformance: &Path, suite: &str, ledger: &mut Ledger) -> Vec<PathBuf> {
    let root = conformance.join(suite);
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
            if !inputs_of(suite).contains(&&*file) && !artifacts_of(suite).contains(&&*file) {
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

/// The refusal-matching rule (the Go port's
/// TestRefusalCasesYieldThePinnedMessages): a pinned refusal is matched by one
/// we produce with the same line — a `null` line matches only a refusal that
/// names none — the same code, and the same message. The code is the
/// contract (S20), so the right line and message under the wrong code is a
/// miss.
fn refusal_matches(want: &serde_json::Value, got: &instruction_core::Refusal) -> bool {
    got.line.map(u64::from) == want["line"].as_u64()
        && Some(got.code) == want["code"].as_str()
        && Some(got.message.as_str()) == want["message"].as_str()
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

    // Includes resolve by front-matter `id` among the corpus documents.
    let by_id = corpus_case::documents_by_id(&Path::new(CONFORMANCE).join("corpus"));
    let resolver = |id: &str| by_id.get(id).cloned();
    for dir in &cases {
        let name = case_name(dir);
        let key = |file: &str| format!("corpus/{name}/{file}");
        let Ok(text) = std::fs::read_to_string(dir.join("doc.md")) else {
            continue;
        };
        let (params, facts) = corpus_case::context_of(&dir.join("context.json"));
        let ctx = Context {
            grants: instruction_core::doc::all_families(),
            params,
            facts,
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

        let delivery = deliver(&doc, &ctx);
        if dir.join("delivered.txt").exists() {
            let want = std::fs::read_to_string(dir.join("delivered.txt")).unwrap();
            ledger.record(
                key("delivered.txt"),
                match &delivery {
                    Ok(out) if out.text == want => Ok(()),
                    Ok(out) => Err(format!(
                        "delivered text differs (got {} bytes, want {})\n--- first diff ---\n{}",
                        out.text.len(),
                        want.len(),
                        first_diff(&want, &out.text)
                    )),
                    Err(errs) => Err(format!("delivery refused: {}", refused(errs))),
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
        // The §7.4 resolution manifest (S7): `manifest.json` structure-exact,
        // and byte-exact as the pretty form the fixtures are exported in;
        // `manifest.canonical.json` byte-exact, the signed form.
        if dir.join("manifest.json").exists() {
            let want = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
            ledger.record(
                key("manifest.json"),
                match &delivery {
                    Ok(out) => manifest_matches(&out.manifest, &want),
                    Err(errs) => Err(format!("delivery refused: {}", refused(errs))),
                },
            );
        }
        if dir.join("manifest.canonical.json").exists() {
            let want = std::fs::read(dir.join("manifest.canonical.json")).unwrap();
            ledger.record(
                key("manifest.canonical.json"),
                match &delivery {
                    Ok(out) if out.manifest.canonical().as_bytes() == want => Ok(()),
                    Ok(out) => Err(format!(
                        "canonical manifest differs\nwant: {}\n got: {}",
                        String::from_utf8_lossy(&want),
                        out.manifest.canonical()
                    )),
                    Err(errs) => Err(format!("delivery refused: {}", refused(errs))),
                },
            );
        }
    }
    eprintln!("shared corpus: {} cases", cases.len());
    ledger.finish("corpus");
}

/// A delivery's manifest against a `manifest.json`: the same structure, and
/// — the fixtures being exported as pretty JSON with a closing newline —
/// the same bytes.
fn manifest_matches(got: &instruction_core::Manifest, want: &str) -> Result<(), String> {
    let want_v: serde_json::Value = serde_json::from_str(want).unwrap();
    let got_v = serde_json::to_value(got).unwrap();
    if got_v != want_v {
        return Err(format!("manifest differs\nwant: {want_v}\n got: {got_v}"));
    }
    let pretty = serde_json::to_string_pretty(got).unwrap() + "\n";
    if pretty != want {
        return Err(format!(
            "manifest bytes differ\n--- first diff ---\n{}",
            first_diff(want, &pretty)
        ));
    }
    Ok(())
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
        // Every pinned refusal is matched by one we produce
        // ([`refusal_matches`]). Extra refusals are allowed: a reader may say
        // more.
        let mut msgs = Vec::new();
        for w in &want {
            let wline = w["line"].as_u64();
            let wcode = w["code"].as_str().unwrap_or("");
            let wmsg = w["message"].as_str().unwrap_or("");
            // A code no reader can produce is a fixture problem, not a
            // pending one: a re-vendor that adds a condition must add its
            // code to CODES first.
            if !instruction_core::CODES.contains(&wcode) {
                ledger.structural.push(format!(
                    "refusals/{name}: pinned code {wcode:?} is not in CODES"
                ));
            }
            let hit = got.iter().any(|g| refusal_matches(w, g));
            if !hit {
                msgs.push(format!(
                    "  want [{wline:?}] {wcode} {wmsg:?}; got: {}",
                    got.iter()
                        .map(|g| format!("[{:?}] {} {:?}", g.line, g.code, g.message))
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

/// `CODES` is the specification's Appendix B code column, when a checkout of
/// it is at hand: every code the table gives a condition is one this crate
/// knows, and every code this crate knows is in the table or is one of the
/// non-catalogue codes the fixtures pin. The README is not vendored, so like
/// the drift checks this runs against the pinned checkout CI makes, and fails
/// rather than skips when `INSTRUCTION_SPEC_REPO` names one without it.
#[test]
fn the_refusal_codes_are_appendix_b_when_present() {
    let explicit = std::env::var("INSTRUCTION_SPEC_REPO");
    let upstream = explicit
        .clone()
        .unwrap_or_else(|_| "/root/instruction-md/specification".into());
    let Ok(readme) = std::fs::read_to_string(Path::new(&upstream).join("README.md")) else {
        assert!(
            explicit.is_err(),
            "INSTRUCTION_SPEC_REPO={upstream:?} was set but has no README.md — fail, not skip"
        );
        eprintln!("no upstream checkout; Appendix B check skipped");
        return;
    };
    let start = readme
        .find("\n## Appendix B")
        .expect("the README has an Appendix B");
    let end = readme[start..]
        .find("\n## Appendix C")
        .map_or(readme.len(), |e| start + e);
    // The code is the second cell of a table row, written in backticks; the
    // row for a condition that is not a refusal has `—` there instead.
    let table: BTreeSet<&str> = readme[start..end]
        .lines()
        .filter_map(|row| row.strip_prefix('|')?.split('|').nth(1))
        .filter_map(|cell| cell.trim().strip_prefix('`')?.strip_suffix('`'))
        .collect();
    assert!(
        table.len() > 40,
        "only {} codes read from Appendix B",
        table.len()
    );
    let ours: BTreeSet<&str> = instruction_core::CODES.iter().copied().collect();
    // The codes with no row in Appendix B, as the crate lists them.
    let non_catalogue: BTreeSet<&str> = instruction_core::NON_CATALOGUE.iter().copied().collect();
    let missing: Vec<_> = table.difference(&ours).collect();
    let extra: Vec<_> = ours
        .difference(&table)
        .filter(|c| !non_catalogue.contains(*c))
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "CODES differs from Appendix B in {upstream}: missing {missing:?}, not in the table {extra:?}"
    );
}

// ── The runner's own rules, judged on synthetic outcomes ────────────────────
// The live fixtures exercise none of these conditions today (no listed entry
// passes, none is missing, no stray file is vendored), so each rule is held
// here instead — a reverted rule fails its test, not just a future re-vendor.

/// A ledger with these outcomes, judged against `pending` over `root`.
fn judged(outcomes: &[(&str, Result<(), String>)], pending: &[&str], root: &Path) -> Vec<String> {
    let mut ledger = Ledger::default();
    for (key, outcome) in outcomes {
        ledger.record(key.to_string(), outcome.clone());
    }
    ledger.judge("corpus", pending, root).failures
}

#[test]
fn the_ledger_fails_a_listed_entry_that_now_passes() {
    let root = Path::new(CONFORMANCE);
    let key = "corpus/coding-agent/tree.json";
    let failures = judged(&[(key, Ok(()))], &[key], root);
    assert_eq!(
        failures,
        [format!("{key}: now passes — remove it from PENDING")]
    );
    // Still failing, it is pending and nothing else.
    assert!(judged(&[(key, Err("x".into()))], &[key], root).is_empty());
}

#[test]
fn the_ledger_fails_an_unlisted_failure_and_passes_an_unlisted_pass() {
    let root = Path::new(CONFORMANCE);
    let failures = judged(
        &[
            ("corpus/a/tree.json", Err("tree.json differs".into())),
            ("corpus/b/tree.json", Ok(())),
        ],
        &[],
        root,
    );
    assert_eq!(failures, ["corpus/a/tree.json: tree.json differs"]);
}

#[test]
fn the_ledger_fails_a_listed_entry_no_run_recorded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("corpus/a")).unwrap();
    std::fs::write(dir.path().join("corpus/a/notes.txt"), "").unwrap();
    let failures = judged(
        &[],
        &[
            "corpus/a/notes.txt",
            "corpus/gone/tree.json",
            "corpsu/a/tree.json",
        ],
        dir.path(),
    );
    assert_eq!(
        failures,
        [
            "PENDING \"corpsu/a/tree.json\" names no suite",
            "PENDING \"corpus/a/notes.txt\" is not an artifact this runner compares",
            "PENDING \"corpus/gone/tree.json\" names a nonexistent file",
        ]
    );
}

#[test]
fn a_case_file_the_suite_does_not_know_is_an_unknown_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str| {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    };
    for f in [
        "corpus/a/doc.md",
        "corpus/a/context.json",
        "corpus/a/delivered.txt",
        "corpus/a/notes.txt",
        "corpus/b/tree.json",
        "corpus/stray.md",
        "refusals/r/doc.md",
        "refusals/r/refusals.json",
        "refusals/r/context.json",
    ] {
        write(f);
    }
    let mut corpus = Ledger::default();
    let found = cases_in(dir.path(), "corpus", &mut corpus);
    assert_eq!(found.len(), 2);
    let mut structural = corpus.structural;
    structural.sort();
    assert_eq!(
        structural,
        [
            "corpus/a/notes.txt: unknown artifact",
            "corpus/b: case has no doc.md",
            "corpus/stray.md: unknown artifact (not a case)",
        ]
    );
    // `context.json` is an input only the corpus runner reads.
    let mut refusals = Ledger::default();
    cases_in(dir.path(), "refusals", &mut refusals);
    assert_eq!(
        refusals.structural,
        ["refusals/r/context.json: unknown artifact"]
    );
}

#[test]
fn a_refusal_matches_on_line_code_and_message() {
    let want = serde_json::json!({"line": 3, "code": "unclosed-fence", "message": "m"});
    let got = |line: Option<u32>, code: &'static str, message: &str| instruction_core::Refusal {
        line,
        code,
        message: message.into(),
    };
    assert!(refusal_matches(&want, &got(Some(3), "unclosed-fence", "m")));
    assert!(
        !refusal_matches(&want, &got(Some(3), "malformed-attributes", "m")),
        "wrong code"
    );
    assert!(
        !refusal_matches(&want, &got(Some(4), "unclosed-fence", "m")),
        "wrong line"
    );
    assert!(
        !refusal_matches(&want, &got(None, "unclosed-fence", "m")),
        "no line"
    );
    assert!(
        !refusal_matches(&want, &got(Some(3), "unclosed-fence", "n")),
        "wrong message"
    );
    let whole = serde_json::json!({"line": null, "code": "schema", "message": "m"});
    assert!(refusal_matches(&whole, &got(None, "schema", "m")));
    assert!(
        !refusal_matches(&whole, &got(Some(1), "schema", "m")),
        "a null line names none"
    );
}
