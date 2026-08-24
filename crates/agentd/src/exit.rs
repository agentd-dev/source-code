// SPDX-License-Identifier: AGPL-3.0-only
//! The public exit-code contract: a stable, machine-actionable API (e.g. for a
//! Kubernetes `podFailurePolicy`). Schedulers branch on these numbers, so treat
//! any change to a code's meaning as breaking.
//!
//! | Code | Meaning                                             | Scheduler hint |
//! |------|-----------------------------------------------------|----------------|
//! | 0    | success (one-shot completed / clean SIGTERM drain)  | Complete       |
//! | 1    | generic/unspecified failure                         | retriable      |
//! | 2    | config / usage error (validation)                   | non-retriable  |
//! | 3    | partial result                                      | policy         |
//! | 4    | intelligence unreachable / auth after retries       | retriable      |
//! | 5    | semantic — task cannot be done / refused            | non-retriable  |
//! | 6    | required MCP server failed to connect/handshake/die | retriable      |
//! | 7    | budget exceeded (steps/tokens/deadline/tree)        | policy         |
//! | 124  | hard wall-clock deadline (mnemonic to `timeout(1)`) | —              |
//! | 137  | killed by SIGKILL (128+9, OS-set) — often OOM       | raise memory   |
//! | 143  | killed by SIGTERM (128+15, OS-set) — ungraceful     | —              |
//!
//! A clean SIGTERM drain returns **0, not 143**: draining on request is a
//! success, not a kill. 137/143 only ever appear because the OS sets them when
//! the kernel kills the process; agentd never calls `exit(137)` itself.
//!
//! Around the table this module freezes two things a control plane depends on:
//! a contract version ([`EXIT_CODES`], surfaced at `surfaces.exit_codes`) and a
//! per-code `podFailurePolicy` *intent* ([`pod_failure_intent`]) that agentctl
//! compiles into `onExitCodes` rules. agentd emits codes and intents only; the
//! policy decision itself belongs to agentctl.

use crate::agentloop::stop::TerminalStatus;

/// The exit-code *contract* version (major.minor), surfaced in the manifest at
/// `surfaces.exit_codes`. It freezes the code->meaning table plus the
/// [`pod_failure_intent`] mapping as a versioned public API that a control plane
/// authors `podFailurePolicy` rules against. New codes may be added within a
/// major; **any** change to an existing code's meaning or intent is breaking and
/// bumps the major, because a reader compiled against the old major would
/// otherwise silently author the wrong policy. agentctl refuses to compile rules
/// for an `exit_codes` major it does not understand.
pub const EXIT_CODES: &str = "1.0";

pub const SUCCESS: i32 = 0;
pub const GENERIC: i32 = 1;
pub const USAGE: i32 = 2;
pub const PARTIAL: i32 = 3;
pub const INTEL_UNAVAILABLE: i32 = 4;
pub const REFUSED: i32 = 5;
pub const MCP_REQUIRED_DOWN: i32 = 6;
pub const BUDGET: i32 = 7;
pub const DEADLINE: i32 = 124;

/// Map a one-shot root subagent's outcome to an exit code.
/// `partial` is the result-body property, not a status: a `Completed` run
/// that only partially satisfied the objective exits `3`. A budget-bounded
/// run that nonetheless produced usable output is still reported under its
/// budget code (`7`) with the partial flag carried in the result JSON.
pub fn once_exit(status: TerminalStatus, partial: bool) -> i32 {
    use TerminalStatus::*;
    match status {
        Completed => {
            if partial {
                PARTIAL
            } else {
                SUCCESS
            }
        }
        Refused => REFUSED,
        ExhaustedSteps | ExhaustedTokens | Deadline => BUDGET,
        Stalled | LoopDetected => PARTIAL,
        Cancelled => GENERIC,
        Crashed => GENERIC,
    }
}

/// The OS-set codes (`128 + signo`). agentd never returns these itself
/// ([`once_exit`] tops out at `DEADLINE` = 124); the kernel sets them when it
/// kills the process. They are named here so [`pod_failure_intent`] can classify
/// the kernel-set code a reader actually observes.
pub const SIGKILL_EXIT: i32 = 137; // 128 + 9 — OOM / kubelet hard-kill
pub const SIGTERM_EXIT: i32 = 143; // 128 + 15 — ungraceful SIGTERM (drain forced past budget)

/// The `podFailurePolicy` *intent* a control plane compiles each exit code into.
/// agentd emits the **code**; agentctl owns the actual `FailJob`/`Ignore`/`Count`
/// choice and any operator override — this is the frozen hint it branches on,
/// not a policy.
///
/// The five intents:
/// - `complete`  — `0`: not a failure; never retry.
/// - `terminal`  — config/semantic error; a retry never helps ⇒ `FailJob`.
/// - `retriable` — usually transient ⇒ left to `backoffLimit` (`Count`).
/// - `policy`    — default `Count`, but the operator's `--budget-exit-code`
///   remap is honoured when present.
/// - `infra`     — kernel-set kill (OOM / ungraceful SIGTERM); the fix is a
///   resource or config change (memory, grace period), so it is never authored
///   as a retry rule — retrying reproduces the same kill.
///
/// An unrecognised code defaults to `retriable` — the conservative posture: an
/// unknown failure is treated like a generic one and left to the backoff limit,
/// never silently `FailJob`'d. (A code outside the contract should not occur at
/// the frozen `EXIT_CODES` major; this is belt-and-suspenders for a future
/// additive code an older agentctl has not learned.)
pub fn pod_failure_intent(code: i32) -> &'static str {
    match code {
        SUCCESS => "complete",
        USAGE | REFUSED => "terminal",
        PARTIAL | BUDGET | DEADLINE => "policy",
        GENERIC | INTEL_UNAVAILABLE | MCP_REQUIRED_DOWN => "retriable",
        SIGKILL_EXIT | SIGTERM_EXIT => "infra",
        _ => "retriable",
    }
}

/// Apply the operator's `--budget-exit-code` remap. ONLY the two
/// operator-tunable `policy`-intent budget codes are remappable — `EXIT_PARTIAL`
/// (3) and `EXIT_BUDGET` (7); every other code (a clean `0`, a terminal refusal
/// `5`, the `policy` deadline `124`, a kernel `137`) is returned UNCHANGED. With
/// no remap configured (`None`) the canonical table applies verbatim.
///
/// This is applied ONLY to the final *process* exit code a Job's
/// `podFailurePolicy` observes — the run report keeps the canonical 3/7
/// projection (and the precise terminal `status`), so the durable record stays
/// truthful and `report.schema`-valid regardless of the remap.
pub fn apply_budget_remap(code: i32, budget_exit_code: Option<i32>) -> i32 {
    match (code, budget_exit_code) {
        (PARTIAL | BUDGET, Some(remapped)) => remapped,
        _ => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentloop::stop::TerminalStatus::*;

    #[test]
    fn budget_remap_touches_only_partial_and_budget() {
        // The two operator-tunable `policy` budget codes remap…
        assert_eq!(apply_budget_remap(PARTIAL, Some(0)), 0);
        assert_eq!(apply_budget_remap(BUDGET, Some(0)), 0);
        assert_eq!(apply_budget_remap(BUDGET, Some(1)), 1);
        // …and NOTHING else does, even though some share the `policy` intent.
        for code in [
            SUCCESS,
            GENERIC,
            USAGE,
            INTEL_UNAVAILABLE,
            REFUSED,
            MCP_REQUIRED_DOWN,
            DEADLINE,
        ] {
            assert_eq!(
                apply_budget_remap(code, Some(0)),
                code,
                "code {code} must never be remapped by --budget-exit-code"
            );
        }
        // No remap configured ⇒ the canonical table is verbatim.
        assert_eq!(apply_budget_remap(PARTIAL, None), PARTIAL);
        assert_eq!(apply_budget_remap(BUDGET, None), BUDGET);
    }

    #[test]
    fn mapping_matches_table() {
        assert_eq!(once_exit(Completed, false), SUCCESS);
        assert_eq!(once_exit(Completed, true), PARTIAL);
        assert_eq!(once_exit(Refused, false), REFUSED);
        assert_eq!(once_exit(ExhaustedSteps, false), BUDGET);
        assert_eq!(once_exit(ExhaustedTokens, false), BUDGET);
        assert_eq!(once_exit(Deadline, false), BUDGET);
        assert_eq!(once_exit(Stalled, false), PARTIAL);
        assert_eq!(once_exit(LoopDetected, false), PARTIAL);
        assert_eq!(once_exit(Cancelled, false), GENERIC);
        assert_eq!(once_exit(Crashed, false), GENERIC);
    }

    #[test]
    fn codes_are_distinct_and_in_documented_bands() {
        let table = [
            SUCCESS,
            GENERIC,
            USAGE,
            PARTIAL,
            INTEL_UNAVAILABLE,
            REFUSED,
            MCP_REQUIRED_DOWN,
            BUDGET,
            DEADLINE,
        ];
        // pairwise distinct — a collision would make a podFailurePolicy ambiguous
        for (i, a) in table.iter().enumerate() {
            for b in &table[i + 1..] {
                assert_ne!(a, b, "exit codes must be distinct");
            }
        }
        // every code is POSIX-portable (0..=125) except the OS-mnemonic 124
        assert!(table.iter().all(|&c| (0..=124).contains(&c)));
    }

    #[test]
    fn pod_failure_intent_matches_the_contract_table() {
        // The exact code->intent mapping agentctl compiles.
        assert_eq!(pod_failure_intent(SUCCESS), "complete");
        assert_eq!(pod_failure_intent(GENERIC), "retriable");
        assert_eq!(pod_failure_intent(USAGE), "terminal");
        assert_eq!(pod_failure_intent(PARTIAL), "policy");
        assert_eq!(pod_failure_intent(INTEL_UNAVAILABLE), "retriable");
        assert_eq!(pod_failure_intent(REFUSED), "terminal");
        assert_eq!(pod_failure_intent(MCP_REQUIRED_DOWN), "retriable");
        assert_eq!(pod_failure_intent(BUDGET), "policy");
        assert_eq!(pod_failure_intent(DEADLINE), "policy");
        // Kernel-set codes are infra fixes, never retry rules.
        assert_eq!(pod_failure_intent(SIGKILL_EXIT), "infra");
        assert_eq!(pod_failure_intent(SIGTERM_EXIT), "infra");
    }

    #[test]
    fn pod_failure_intent_is_total_over_the_contract_and_defaults_safely() {
        // Every code the table defines maps to one of the five intents.
        let intents = ["complete", "terminal", "retriable", "policy", "infra"];
        for code in [
            SUCCESS,
            GENERIC,
            USAGE,
            PARTIAL,
            INTEL_UNAVAILABLE,
            REFUSED,
            MCP_REQUIRED_DOWN,
            BUDGET,
            DEADLINE,
            SIGKILL_EXIT,
            SIGTERM_EXIT,
        ] {
            assert!(
                intents.contains(&pod_failure_intent(code)),
                "code {code} mapped outside the documented intent set"
            );
        }
        // An unknown code is treated conservatively — retriable, never a silent
        // FailJob (a terminal verdict on an unrecognised code would be unsafe).
        assert_eq!(pod_failure_intent(99), "retriable");
        assert_eq!(pod_failure_intent(-1), "retriable");
    }

    #[test]
    fn intent_never_authors_a_retry_rule_for_a_terminal_or_infra_code() {
        // The control-plane invariant: a `terminal` config/semantic error and an
        // `infra` kernel-kill must never be classified `retriable` — retrying
        // either burns the backoff limit and reproduces the same failure.
        for code in [USAGE, REFUSED, SIGKILL_EXIT, SIGTERM_EXIT] {
            assert_ne!(
                pod_failure_intent(code),
                "retriable",
                "code {code} must not be authored as a retry rule"
            );
        }
    }

    #[test]
    fn exit_codes_contract_version_is_frozen_at_one_zero() {
        // The manifest's surfaces.exit_codes value.
        assert_eq!(EXIT_CODES, "1.0");
    }

    #[test]
    fn once_exit_never_returns_success_for_a_non_completed_status() {
        for s in [
            Refused,
            ExhaustedSteps,
            ExhaustedTokens,
            Deadline,
            Stalled,
            LoopDetected,
            Cancelled,
            Crashed,
        ] {
            assert_ne!(
                once_exit(s, false),
                SUCCESS,
                "{s:?} must not look like success"
            );
        }
    }
}
