// SPDX-License-Identifier: AGPL-3.0-only
//! Black-box conformance suite for the agentd runtime.
//!
//! The suite is a flat list of named [`Check`]s grouped into [`Category`]
//! families. Each check drives the real `agentd` binary through a [`Harness`]
//! and returns an [`Outcome`] — pass, or fail with a diagnostic. The same checks
//! back both the `#[test]` integration tests (so `cargo test` enforces
//! conformance) and the `agentd-conformance` runner binary (which renders a
//! PASS/FAIL report). Nothing here links the agentd library: conformance is
//! judged against the A2A / JSON-RPC spec and the documented exit-code table,
//! not against agentd's own types.

pub mod checks;
pub mod harness;
pub mod report;

pub use harness::Harness;
pub use report::Report;

/// The conformance families. Each names one externally-observable contract, so
/// a failure points at the promise that broke rather than at an implementation
/// detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// The supervisor contract: the exit-code table, drain, fail-fast.
    Supervisor,
    /// Security posture: trifecta refusal, secret redaction, tool scoping.
    Security,
    /// The durable-store contract: boot against a store, persist the
    /// manifest/runs, and resume a completed `once` start after restart.
    Store,
    /// The crash-durability contract: a SIGKILL at a kill point is recovered —
    /// the pending inbox event and running step replay.
    Durability,
    /// The tool registry: internal tools round-trip to the supervisor,
    /// an unknown tool is answered as an error, and the introspected surface.
    Tools,
    /// A2A conversations: the core JSON-RPC surface — the version gate and
    /// method vocabulary, reads that answer without a task, natural-language
    /// turns landing as task artifacts with their history, the
    /// `input-required` gate, push delivery, and the card's promises.
    A2aConversation,
    /// The observation feed the events extension declares: off unless the
    /// operator enables it, strict about its params, hello + ring replay.
    Events,
    /// The extension contract: activation by the `A2A-Extensions` header is
    /// the only way in, a result is a Task or a Message, and agentd's facts
    /// travel only under a declared extension URI.
    Extensions,
    /// Authentication honesty: the card declares exactly what the listener
    /// enforces, the extended card needs a declared credential, and a browser
    /// reaches the agent only from a listed origin.
    Auth,
}

impl Category {
    /// Every family, in report order: the one list the docs are held to.
    pub const ALL: [Category; 9] = [
        Category::Supervisor,
        Category::Security,
        Category::Store,
        Category::Durability,
        Category::Tools,
        Category::A2aConversation,
        Category::Events,
        Category::Extensions,
        Category::Auth,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Supervisor => "supervisor",
            Category::Security => "security",
            Category::Store => "store",
            Category::Durability => "durability",
            Category::Tools => "tools",
            Category::A2aConversation => "a2a-conversation",
            Category::Events => "events",
            Category::Extensions => "extensions",
            Category::Auth => "auth",
        }
    }
}

/// The result of one conformance check.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub passed: bool,
    /// On failure, why; on pass, an optional one-line note.
    pub detail: String,
}

impl Outcome {
    pub fn pass() -> Outcome {
        Outcome {
            passed: true,
            detail: String::new(),
        }
    }

    pub fn note(detail: impl Into<String>) -> Outcome {
        Outcome {
            passed: true,
            detail: detail.into(),
        }
    }

    pub fn fail(detail: impl Into<String>) -> Outcome {
        Outcome {
            passed: false,
            detail: detail.into(),
        }
    }

    /// Assert `cond`, failing with `detail` otherwise. Lets a check read as a
    /// sequence of `require(...)?`-style guards via [`Outcome::and`].
    pub fn require(cond: bool, detail: impl Into<String>) -> Outcome {
        if cond {
            Outcome::pass()
        } else {
            Outcome::fail(detail)
        }
    }

    /// Chain: if `self` passed, evaluate `next`; else keep the first failure.
    pub fn and(self, next: impl FnOnce() -> Outcome) -> Outcome {
        if self.passed { next() } else { self }
    }
}

/// One conformance check: a stable id, its family, what contract it proves, and
/// the function that drives the harness to verify it.
pub struct Check {
    pub id: &'static str,
    pub category: Category,
    pub desc: &'static str,
    pub run: fn(&Harness) -> Outcome,
}

/// Run one check, converting a panic (a failed harness `expect`, a spawn error)
/// into a check failure rather than aborting the whole suite.
pub fn run_check(h: &Harness, check: &Check) -> Outcome {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    match catch_unwind(AssertUnwindSafe(|| (check.run)(h))) {
        Ok(o) => o,
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panicked".to_string());
            Outcome::fail(format!("panicked: {msg}"))
        }
    }
}

/// The checks of one family. The match is exhaustive, so a new family cannot
/// be declared without saying where its checks live.
pub fn family(category: Category) -> Vec<Check> {
    match category {
        Category::Supervisor => checks::supervisor::checks(),
        Category::Security => checks::security::checks(),
        Category::Store => checks::store::checks(),
        Category::Durability => checks::durability::checks(),
        Category::Tools => checks::tools::checks(),
        Category::A2aConversation => checks::a2a_conversation::checks(),
        Category::Events => checks::events::checks(),
        Category::Extensions => checks::extensions::checks(),
        Category::Auth => checks::auth::checks(),
    }
}

/// Every conformance check across all families, in a stable order.
pub fn all_checks() -> Vec<Check> {
    Category::ALL.into_iter().flat_map(family).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The section of `CONFORMANCE.md` that lists the checks: per family, in
    /// report order, a row per check naming it and what it proves — written
    /// from the checks themselves, so the page cannot claim a check that does
    /// not exist or describe one differently from the report.
    fn checks_section() -> String {
        let mut out = String::new();
        for c in Category::ALL {
            out.push_str(&format!(
                "### `{}`\n\n| Check | What it proves |\n|---|---|\n",
                c.as_str()
            ));
            for check in family(c) {
                out.push_str(&format!("| `{}` | {} |\n", check.id, check.desc));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn conformance_md_lists_exactly_the_checks() {
        let doc = include_str!("../../../CONFORMANCE.md");
        let section = checks_section();
        assert!(
            doc.contains(&section),
            "CONFORMANCE.md must list the checks exactly as they are registered:\n\n{section}"
        );
        // …and nothing else: every table row in the page is one of those.
        let rows = |s: &str| s.lines().filter(|l| l.starts_with("| `")).count();
        assert_eq!(
            rows(doc),
            rows(&section),
            "CONFORMANCE.md lists a check that does not exist"
        );
    }

    /// The crate README's family table names the families there are, in
    /// report order.
    #[test]
    fn the_readme_names_every_family() {
        let readme = include_str!("../README.md");
        let named: Vec<&str> = readme
            .lines()
            .filter_map(|l| l.strip_prefix("| `"))
            .filter_map(|l| l.split_once('`').map(|(name, _)| name))
            .collect();
        let families: Vec<&str> = Category::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(named, families);
    }

    /// A check's id names its family, ids are unique, and a description can
    /// sit in a table cell.
    #[test]
    fn check_ids_are_unique_and_name_their_family() {
        let checks = all_checks();
        let mut ids: Vec<&str> = checks.iter().map(|c| c.id).collect();
        for c in &checks {
            assert!(
                c.id.starts_with(&format!("{}/", c.category.as_str())),
                "{} is not in the {} family's namespace",
                c.id,
                c.category.as_str()
            );
            assert!(
                !c.desc.contains('|'),
                "{}: `|` would split its table row",
                c.id
            );
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), checks.len(), "two checks share an id");
        for c in Category::ALL {
            assert!(
                family(c).iter().all(|check| check.category == c),
                "{}: a check registered under another family",
                c.as_str()
            );
            assert!(
                !family(c).is_empty(),
                "{}: a family with no checks",
                c.as_str()
            );
        }
    }
}
