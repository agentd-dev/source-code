// SPDX-License-Identifier: AGPL-3.0-only
//! Capability scoping — the granted MCP subset, interpreted as a Rule-of-Two
//! trust budget.
//!
//! agentd has no policy engine; a subagent's authority *is* the subset of MCP
//! servers/tools its parent grants. Two invariants this module encodes:
//!
//! 1. **Monotonic narrowing.** A child's scope is the *intersection* with its
//!    parent's — a child can never widen beyond what its parent holds. Because
//!    narrowing only ever shrinks, a check made over the root grant bounds
//!    every descendant, which is what lets the trifecta gate run once at
//!    startup rather than at every spawn.
//! 2. **Rule of Two.** Servers and tools are tagged `untrusted_input` /
//!    `sensitive` / `egress`; granting one agent all three legs of the lethal
//!    trifecta is refused unless the operator overrides it
//!    (`security.allow_trifecta`).
//!
//! Everything here is pure logic with no I/O; callers apply it. Config
//! validation folds the whole root grant, so startup and `--validate-config`
//! can never reach different verdicts, and the `subagent.run` tool folds the
//! child's requested server subset before minting the child.

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
        ToolScope {
            servers: Scope::All,
            tools: Scope::All,
        }
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

/// The three legs of the "lethal trifecta". An agent holding all three — it
/// reads untrusted content, can touch sensitive data, and can send data out —
/// is the dangerous combination: one injected instruction in the content it
/// reads is enough to make it fetch a secret and forward it.
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
/// refusal to a loud warning.
pub fn evaluate(tags: Trifecta, allow_trifecta: bool) -> RuleOfTwo {
    if tags.legs() < 3 {
        RuleOfTwo::Ok
    } else if allow_trifecta {
        RuleOfTwo::Warn
    } else {
        RuleOfTwo::Refuse
    }
}

// ---------------------------------------------------------------------------
// Rule-of-Two tag check
// ---------------------------------------------------------------------------
//
// [`Trifecta`] above is the *accumulated* budget (the OR across a granted
// set). [`TrifectaTag`] below is the per-leg label an operator attaches to a
// server or tool; [`check_trifecta`] folds a tag stream into the budget and
// returns the verdict the caller acts on. The two layers share one source of
// truth — a tag is just a single-leg [`Trifecta`] — so "which combination is
// lethal" is defined in exactly one place.

/// One leg of the lethal trifecta — an operator-declared risk capability a
/// tool carries. Tags come from the operator's own config and nowhere else:
/// never from a tool description, a model's claim about itself, or anything
/// an MCP server advertises, because a server able to tag itself could simply
/// declare itself harmless and buy back the leg it was meant to be charged.
///
/// The three legs are access to private data, exposure to untrusted content,
/// and the ability to act or send data outward. Holding any two is fine;
/// holding all three is the shape where one injected instruction suffices to
/// read a secret and forward it, which is what the Rule of Two refuses to
/// co-locate in a single agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrifectaTag {
    /// Tool returns content from an uncontrolled source (web pages, inbound
    /// email, issue text, arbitrary files) — a possible injection carrier.
    UntrustedInput,
    /// Tool exposes private data or privileged systems (secrets store,
    /// internal DB, prod control plane).
    Sensitive,
    /// Tool can move data out of the trust boundary or change external state
    /// (HTTP POST, send mail, open PR, `exec`).
    Egress,
}

impl TrifectaTag {
    /// Parse an operator-declared tag string (`--mcp-tags name=…`). Snake-case,
    /// matching the serde wire form; unknown tags return `None`.
    pub fn parse(s: &str) -> Option<TrifectaTag> {
        match s {
            "untrusted_input" => Some(TrifectaTag::UntrustedInput),
            "sensitive" => Some(TrifectaTag::Sensitive),
            "egress" => Some(TrifectaTag::Egress),
            _ => None,
        }
    }

    /// This tag as a single-leg [`Trifecta`], so the accumulation logic lives
    /// in exactly one place (`Trifecta::merge`).
    pub fn as_trifecta(self) -> Trifecta {
        match self {
            TrifectaTag::UntrustedInput => Trifecta {
                untrusted_input: true,
                ..Default::default()
            },
            TrifectaTag::Sensitive => Trifecta {
                sensitive: true,
                ..Default::default()
            },
            TrifectaTag::Egress => Trifecta {
                egress: true,
                ..Default::default()
            },
        }
    }
}

/// The verdict on a grant's trifecta exposure. A refusal is never a crash —
/// each caller turns it into an outcome its layer can act on:
///
/// - [`TrifectaVerdict::Ok`] — two legs or fewer; the grant proceeds silently.
/// - [`TrifectaVerdict::RefusedTrifecta`] — all three legs and no override.
///   Config validation reports it as a startup error; the `subagent.run` tool
///   returns an error result the parent's model can act on by splitting the
///   work across two narrower children.
/// - [`TrifectaVerdict::AllowedWithWarning`] — all three legs, but the
///   operator set `security.allow_trifecta`, so the grant proceeds.
///
/// This mirrors [`RuleOfTwo`] (`Ok`/`Warn`/`Refuse`) with self-describing
/// names for the grant path; both sit on the same [`Trifecta`] budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrifectaVerdict {
    /// ≤2 legs — the Rule of Two holds; grant silently.
    Ok,
    /// All three legs and no `--allow-trifecta` — the grant is refused.
    RefusedTrifecta,
    /// All three legs but `--allow-trifecta` downgrades the refusal to a loud,
    /// auditable warning.
    AllowedWithWarning,
}

impl TrifectaVerdict {
    /// Whether the grant must be blocked at the chokepoint. Only
    /// [`TrifectaVerdict::RefusedTrifecta`] blocks; a warning still proceeds.
    pub fn is_refused(self) -> bool {
        matches!(self, TrifectaVerdict::RefusedTrifecta)
    }
}

/// Pure Rule-of-Two check. Folds the tags of a granted tool set (`OR` across
/// legs) and judges the accumulated budget:
///
/// - fewer than three legs → [`TrifectaVerdict::Ok`] (any *two* is fine);
/// - all three legs → [`TrifectaVerdict::RefusedTrifecta`], unless
///   `allow_trifecta` downgrades it to [`TrifectaVerdict::AllowedWithWarning`].
///
/// Structural only — it never inspects tool *content* and never asks the
/// model to judge; it is a budget on co-located capability. Call it over the
/// tags of the already-narrowed grant and *before* the grant is handed out:
/// refusing once a child already holds the capability is too late.
pub fn check_trifecta<I>(tags: I, allow_trifecta: bool) -> TrifectaVerdict
where
    I: IntoIterator<Item = TrifectaTag>,
{
    let budget = tags
        .into_iter()
        .fold(Trifecta::default(), |acc, t| acc.merge(t.as_trifecta()));
    match evaluate(budget, allow_trifecta) {
        RuleOfTwo::Ok => TrifectaVerdict::Ok,
        RuleOfTwo::Warn => TrifectaVerdict::AllowedWithWarning,
        RuleOfTwo::Refuse => TrifectaVerdict::RefusedTrifecta,
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
        let scope = ToolScope {
            servers: Scope::only(["fs"]),
            tools: Scope::only(["read_file"]),
        };
        assert!(scope.allows("fs", "read_file"));
        assert!(!scope.allows("github", "read_file")); // wrong server
        assert!(!scope.allows("fs", "write_file")); // wrong tool
    }

    #[test]
    fn tool_scope_narrow_intersects_both() {
        let parent = ToolScope {
            servers: Scope::only(["fs", "db"]),
            tools: Scope::All,
        };
        let child = ToolScope {
            servers: Scope::only(["fs", "net"]),
            tools: Scope::only(["read"]),
        };
        let n = parent.narrow(&child);
        assert_eq!(n.servers, Scope::only(["fs"]));
        assert_eq!(n.tools, Scope::only(["read"]));
    }

    #[test]
    fn rule_of_two() {
        let two = Trifecta {
            untrusted_input: true,
            sensitive: true,
            egress: false,
        };
        assert_eq!(evaluate(two, false), RuleOfTwo::Ok);
        let three = Trifecta {
            untrusted_input: true,
            sensitive: true,
            egress: true,
        };
        assert_eq!(evaluate(three, false), RuleOfTwo::Refuse);
        assert_eq!(evaluate(three, true), RuleOfTwo::Warn);
        assert_eq!(three.legs(), 3);
    }

    #[test]
    fn trifecta_merge_accumulates() {
        let a = Trifecta {
            untrusted_input: true,
            ..Default::default()
        };
        let b = Trifecta {
            egress: true,
            ..Default::default()
        };
        assert_eq!(a.merge(b).legs(), 2);
    }

    // -----------------------------------------------------------------
    // Rule-of-Two tag check
    // -----------------------------------------------------------------

    use TrifectaTag::{Egress, Sensitive, UntrustedInput};

    #[test]
    fn tag_maps_to_single_leg() {
        assert_eq!(UntrustedInput.as_trifecta().legs(), 1);
        assert_eq!(Sensitive.as_trifecta().legs(), 1);
        assert_eq!(Egress.as_trifecta().legs(), 1);
        assert!(UntrustedInput.as_trifecta().untrusted_input);
        assert!(Sensitive.as_trifecta().sensitive);
        assert!(Egress.as_trifecta().egress);
    }

    #[test]
    fn empty_grant_is_ok() {
        assert_eq!(check_trifecta([], false), TrifectaVerdict::Ok);
    }

    #[test]
    fn each_single_leg_is_ok() {
        for tag in [UntrustedInput, Sensitive, Egress] {
            assert_eq!(check_trifecta([tag], false), TrifectaVerdict::Ok);
        }
    }

    #[test]
    fn every_pair_is_allowed() {
        // The three two-leg combinations — each is fine under the Rule of Two.
        let pairs = [
            [UntrustedInput, Sensitive],
            [UntrustedInput, Egress],
            [Sensitive, Egress],
        ];
        for pair in pairs {
            assert_eq!(
                check_trifecta(pair, false),
                TrifectaVerdict::Ok,
                "pair {pair:?} should be allowed"
            );
            // The override never *tightens* a verdict — a pair stays Ok.
            assert_eq!(check_trifecta(pair, true), TrifectaVerdict::Ok);
        }
    }

    #[test]
    fn all_three_refused_without_override() {
        assert_eq!(
            check_trifecta([UntrustedInput, Sensitive, Egress], false),
            TrifectaVerdict::RefusedTrifecta
        );
    }

    #[test]
    fn all_three_warns_with_override() {
        assert_eq!(
            check_trifecta([UntrustedInput, Sensitive, Egress], true),
            TrifectaVerdict::AllowedWithWarning
        );
    }

    #[test]
    fn duplicate_tags_do_not_inflate_legs() {
        // OR-fold, not a count: repeating a leg never crosses into trifecta.
        assert_eq!(
            check_trifecta([Egress, Egress, Egress], false),
            TrifectaVerdict::Ok
        );
        // Two distinct legs, each repeated, is still a pair.
        assert_eq!(
            check_trifecta([Sensitive, Sensitive, Egress, Egress], false),
            TrifectaVerdict::Ok
        );
    }

    #[test]
    fn only_refused_blocks_the_chokepoint() {
        assert!(TrifectaVerdict::RefusedTrifecta.is_refused());
        assert!(!TrifectaVerdict::Ok.is_refused());
        assert!(!TrifectaVerdict::AllowedWithWarning.is_refused());
    }

    #[test]
    fn tag_serde_roundtrips_snake_case() {
        // Tags arrive from MCP server config as snake_case.
        let json = serde_json::to_string(&UntrustedInput).unwrap();
        assert_eq!(json, "\"untrusted_input\"");
        let back: TrifectaTag = serde_json::from_str("\"egress\"").unwrap();
        assert_eq!(back, Egress);
    }
}
