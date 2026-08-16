// SPDX-License-Identifier: AGPL-3.0-only
//! Black-box conformance suite for the agentd runtime.
//!
//! The suite is a flat list of named [`Check`]s grouped into [`Category`]
//! families. Each check drives the real `agentd` binary through a [`Harness`]
//! and returns an [`Outcome`] — pass, or fail with a diagnostic. The same checks
//! back both the `#[test]` integration tests (so `cargo test` enforces
//! conformance) and the `agentd-conformance` runner binary (which renders a
//! PASS/FAIL report). Nothing here links the agentd library: conformance is
//! judged against the MCP / JSON-RPC spec and the documented exit-code table,
//! not against agentd's own types.

pub mod checks;
pub mod harness;
pub mod mcp_http_server;
pub mod report;

pub use harness::Harness;
pub use report::Report;

/// The conformance families. agentd 2.0: the v1-only families (`mcp-server`,
/// `mcp-client`, `work-claim`) were retired with the mode cut-over and rebuilt as
/// the v2 families below (P7): the durable-store contract, the crash/restore
/// durability contract, the internal tool registry, and A2A conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// The supervisor contract: the exit-code table, drain, fail-fast.
    Supervisor,
    /// Security posture: trifecta refusal, secret redaction, tool scoping.
    Security,
    /// The durable-store contract (RFC 0025): boot against a store, persist the
    /// manifest/runs, and resume a completed `once` start after restart.
    Store,
    /// The crash-durability contract (RFC 0025/0026 §4.4): a SIGKILL at a kill
    /// point is recovered — the pending inbox event and running step replay.
    Durability,
    /// The tool registry (RFC 0028): internal tools round-trip to the supervisor,
    /// an unknown tool is answered as an error, and the introspected surface.
    Tools,
    /// A2A conversations (RFC 0029): the JSON-RPC surface — command DataParts,
    /// natural-language turns landing as task artifacts, GetTask/ListTasks, card.
    A2aConversation,
    /// The display-client interface (RFC 0032): the default-OFF gate, the
    /// SubscribeToEvents feed (hello + ring replay), and the human-in-the-loop
    /// gate round-trip.
    Interface,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Supervisor => "supervisor",
            Category::Security => "security",
            Category::Store => "store",
            Category::Durability => "durability",
            Category::Tools => "tools",
            Category::A2aConversation => "a2a-conversation",
            Category::Interface => "interface",
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

/// Every conformance check across all families, in a stable order.
pub fn all_checks() -> Vec<Check> {
    let mut v = Vec::new();
    v.extend(checks::supervisor::checks());
    v.extend(checks::security::checks());
    v.extend(checks::store::checks());
    v.extend(checks::durability::checks());
    v.extend(checks::tools::checks());
    v.extend(checks::a2a_conversation::checks());
    v.extend(checks::interface::checks());
    v
}
