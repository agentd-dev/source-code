//! Capability scoping — the granted MCP subset, interpreted as a Rule-of-Two
//! trust budget. RFC 0012 §capability-scoping.
//!
//! agentd has no policy engine; a subagent's authority *is* the subset of MCP
//! servers/tools its parent grants. Two invariants this module enforces:
//!
//! 1. **Monotonic narrowing.** A child's scope is the *intersection* with its
//!    parent's — a child can never widen beyond what its parent holds.
//! 2. **Rule of Two.** Tools are tagged `untrusted_input` / `sensitive` /
//!    `egress`; granting one subagent all three legs of the lethal trifecta is
//!    refused unless explicitly overridden (`--allow-trifecta`).
//!
//! This is pure logic; it is wired into the spawn chokepoint (with
//! `supervisor/tree.rs`) and the self-MCP grant path in later M2 steps.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A whitelist over names: everything, or an explicit set. `BTreeSet` for
/// deterministic ordering (stable logs/serialization).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    All,
    Only(BTreeSet<String>),
}

impl Scope {
    pub fn only<I, S>(names: I) -> Scope
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Scope::Only(names.into_iter().map(Into::into).collect())
    }

    pub fn allows(&self, name: &str) -> bool {
        match self {
            Scope::All => true,
            Scope::Only(set) => set.contains(name),
        }
    }

    /// Intersect a child's requested scope with this (the parent's). The
    /// result never exceeds the parent — `All ∩ x = x`, `Only(p) ∩ All =
    /// Only(p)`, `Only(p) ∩ Only(r) = Only(p ∩ r)` (names the parent lacks are
    /// silently dropped — a clamp, not an error).
    pub fn narrow(&self, requested: &Scope) -> Scope {
        match (self, requested) {
            (Scope::All, r) => r.clone(),
            (p @ Scope::Only(_), Scope::All) => p.clone(),
            (Scope::Only(p), Scope::Only(r)) => Scope::Only(p.intersection(r).cloned().collect()),
        }
    }
}

/// A subagent's tool scope: which MCP servers it may reach, and (optionally)
/// which tools within them. Both must pass for a call to be allowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolScope {
    pub servers: Scope,
    pub tools: Scope,
}

impl ToolScope {
    /// The root agent's scope — everything the operator configured.
    pub fn all() -> ToolScope {
        ToolScope { servers: Scope::All, tools: Scope::All }
    }

    pub fn allows_server(&self, server: &str) -> bool {
        self.servers.allows(server)
    }

    /// A tool call is allowed only if both its server and its tool name are in
    /// scope.
    pub fn allows(&self, server: &str, tool: &str) -> bool {
        self.servers.allows(server) && self.tools.allows(tool)
    }

    /// Narrow a child's request against this parent scope (both dimensions).
    pub fn narrow(&self, requested: &ToolScope) -> ToolScope {
        ToolScope {
            servers: self.servers.narrow(&requested.servers),
            tools: self.tools.narrow(&requested.tools),
        }
    }
}

/// The three legs of the "lethal trifecta". A subagent holding all three —
/// it reads untrusted content, can touch sensitive data, and can exfiltrate —
/// is the dangerous combination (RFC 0012).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Trifecta {
    pub untrusted_input: bool,
    pub sensitive: bool,
    pub egress: bool,
}

impl Trifecta {
    pub fn legs(self) -> u8 {
        self.untrusted_input as u8 + self.sensitive as u8 + self.egress as u8
    }

    /// Fold a tool's tags into the running total for a grant.
    pub fn merge(self, other: Trifecta) -> Trifecta {
        Trifecta {
            untrusted_input: self.untrusted_input || other.untrusted_input,
            sensitive: self.sensitive || other.sensitive,
            egress: self.egress || other.egress,
        }
    }
}

/// The verdict on a grant. `Ok` ≤ 2 legs; all 3 legs → `Refuse` (or `Warn`
/// with `--allow-trifecta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleOfTwo {
    Ok,
    Warn,
    Refuse,
}

/// Evaluate a grant's trifecta exposure. The Rule of Two is satisfied at ≤2
/// legs; 3 legs violates it — refused unless `allow_trifecta` downgrades the
/// refusal to a loud warning (RFC 0012).
pub fn evaluate(tags: Trifecta, allow_trifecta: bool) -> RuleOfTwo {
    if tags.legs() < 3 {
        RuleOfTwo::Ok
    } else if allow_trifecta {
        RuleOfTwo::Warn
    } else {
        RuleOfTwo::Refuse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_all_allows_everything() {
        assert!(Scope::All.allows("anything"));
    }

    #[test]
    fn scope_only_is_a_whitelist() {
        let s = Scope::only(["read_file", "list_dir"]);
        assert!(s.allows("read_file"));
        assert!(!s.allows("write_file"));
    }

    #[test]
    fn narrow_never_widens() {
        let parent = Scope::only(["a", "b"]);
        // child asks for everything -> clamped to parent
        assert_eq!(parent.narrow(&Scope::All), parent);
        // child asks for a superset -> clamped to the intersection
        let child = Scope::only(["a", "c"]);
        assert_eq!(parent.narrow(&child), Scope::only(["a"]));
        // parent All -> child gets exactly what it asked
        assert_eq!(Scope::All.narrow(&child), child);
    }

    #[test]
    fn tool_scope_requires_both_dimensions() {
        let scope = ToolScope { servers: Scope::only(["fs"]), tools: Scope::only(["read_file"]) };
        assert!(scope.allows("fs", "read_file"));
        assert!(!scope.allows("github", "read_file")); // wrong server
        assert!(!scope.allows("fs", "write_file")); // wrong tool
    }

    #[test]
    fn tool_scope_narrow_intersects_both() {
        let parent = ToolScope { servers: Scope::only(["fs", "db"]), tools: Scope::All };
        let child = ToolScope { servers: Scope::only(["fs", "net"]), tools: Scope::only(["read"]) };
        let n = parent.narrow(&child);
        assert_eq!(n.servers, Scope::only(["fs"]));
        assert_eq!(n.tools, Scope::only(["read"]));
    }

    #[test]
    fn rule_of_two() {
        let two = Trifecta { untrusted_input: true, sensitive: true, egress: false };
        assert_eq!(evaluate(two, false), RuleOfTwo::Ok);
        let three = Trifecta { untrusted_input: true, sensitive: true, egress: true };
        assert_eq!(evaluate(three, false), RuleOfTwo::Refuse);
        assert_eq!(evaluate(three, true), RuleOfTwo::Warn);
        assert_eq!(three.legs(), 3);
    }

    #[test]
    fn trifecta_merge_accumulates() {
        let a = Trifecta { untrusted_input: true, ..Default::default() };
        let b = Trifecta { egress: true, ..Default::default() };
        assert_eq!(a.merge(b).legs(), 2);
    }
}
