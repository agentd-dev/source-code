// SPDX-License-Identifier: Apache-2.0
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
pub mod report;

pub use harness::Harness;
pub use report::Report;

/// The conformance families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// agentd as an MCP **server** (`--serve-mcp`): the JSON-RPC 2.0 + MCP
    /// protocol it must speak to peers.
    McpServer,
    /// agentd as an MCP **client**: the requests it sends a backing server.
    McpClient,
    /// The supervisor contract: the exit-code table, drain, fail-fast.
    Supervisor,
    /// The agentic ReAct loop end-to-end: tool calls → execution → final answer.
    AgentLoop,
    /// Security posture: trifecta refusal, secret redaction, tool scoping.
    Security,
    /// The work-claim / lease convention (RFC 0019 §3, the frozen `work.*`
    /// contract RFC 0015 §5.6): atomic single-grant + the claim→ack lifecycle a
    /// `cluster` agentd drives against a coordination server.
    WorkClaim,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::McpServer => "mcp-server",
            Category::McpClient => "mcp-client",
            Category::Supervisor => "supervisor",
            Category::AgentLoop => "agent-loop",
            Category::Security => "security",
            Category::WorkClaim => "work-claim",
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
    v.extend(checks::mcp_server::checks());
    v.extend(checks::mcp_client::checks());
    v.extend(checks::supervisor::checks());
    v.extend(checks::agent_loop::checks());
    v.extend(checks::security::checks());
    v.extend(checks::work_claim::checks());
    v
}
