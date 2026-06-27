//! Process-local counters → Prometheus text. RFC 0010 §metrics. [feature: metrics]
//!
//! Off the default path: the public `record_*` fns are **no-ops unless built
//! with `--features metrics`**, so call sites stay clean and the default build
//! pays nothing (metrics are otherwise derivable from the JSON-lines event
//! stream — that is the default story). With the feature, a tiny dependency-free
//! atomic registry backs an opt-in HTTP `/metrics` scrape surface (`obs::serve`).
//!
//! Counters are **per supervisor process**. The long-lived root daemon's surface
//! reflects the runs it supervises — every one-shot, reaction, and scheduled fire
//! flows through `supervise_once` — plus the tokens its *direct* children report
//! up the control channel. Nested subagents keep their own (process-local)
//! counters, still visible in their logs; cross-process metric rollup is a
//! deliberate non-goal for v1 (same boundary as the tree token ceiling).
//!
//! ## The frozen `metrics_schema` contract (RFC 0016 §4)
//!
//! RFC 0016 §4 freezes the metric **names** and label **keys** below into a
//! versioned public API ([`METRICS_SCHEMA`]) a control plane (agentctl) authors
//! dashboards/alerts/scalers against. The exposition mechanism is unchanged from
//! RFC 0010 §3.8 (hand-written Prometheus 0.0.4 text — no `prometheus`/`metrics`
//! crate). The §4.3 enumerated set is *the contract*; it is additive within the
//! major and a removal/rename bumps the major (§8.2).
//!
//! **Cardinality (RFC 0016 §4.2, binding):** `/metrics` is unauthenticated and
//! may be bound on all interfaces. Labels carry **bounded** values only
//! (`status`, `model`, `type`, `server`, `tool`, `reason`, `limit`, `signal`,
//! `phase`, `ok`); **never** `run_id` / `agent_id` / `agent_path` / `call_id` /
//! a resource URI — those are unbounded and live in logs/traces only. A control
//! plane that needs per-run granularity reads the run report (§6) or the event
//! stream (§7), never a metric. This module therefore stores label-bearing
//! series as small **fixed-domain** atomic arrays (the closed label set is known
//! at compile time), so the cardinality is structurally bounded.
//!
//! Telemetry never crashes the agent (RFC 0016 §8.4): every fn here is a plain
//! atomic add/store that cannot fail; `render` only ever reads.

/// Frozen metrics-schema version (RFC 0016 §4.1 / §8.1). Surfaced in the manifest
/// at `surfaces.metrics_schema`; the integrator wires the surface — this const is
/// the single source of truth this chunk owns. Additive series/label-values bump
/// the minor; a removed/renamed metric or label key bumps the major (§8.2).
pub const METRICS_SCHEMA: &str = "1.0";

/// Terminal disposition of one supervised run.
#[derive(Debug, Clone, Copy)]
pub enum RunOutcome {
    Completed,
    Failed,
    Killed,
}

/// A supervised run began (`supervise_once` entry).
pub fn record_run_started() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.runs_started.fetch_add(1, Ordering::Relaxed);
}

/// A supervised run reached a terminal disposition.
///
/// Also increments the frozen `agentd_runs_total{status}` (RFC 0016 §4.3) under a
/// **coarse** status projection of the three `RunOutcome` variants this call site
/// carries (`completed` / `crashed` / `cancelled`). The precise RFC 0007 §3.4
/// terminal-status string is available at the loop boundary but not at this
/// supervisor hook — see [`record_run_status`] and the integration caveat: an
/// integrator with the `TerminalStatus` in hand should call that instead for the
/// full closed-vocabulary label domain.
pub fn record_run(outcome: RunOutcome) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_run(outcome);
    #[cfg(not(feature = "metrics"))]
    let _ = outcome;
}

/// A supervised run reached a terminal status — the **frozen** §4.3 form.
///
/// `status` MUST be an RFC 0007 §3.4 closed-vocabulary string
/// ([`crate::agentloop::stop::TerminalStatus::as_str`]); an out-of-vocabulary
/// value is bucketed under `other` so the label domain stays closed (§4.2). This
/// is the precise driver for `agentd_runs_total{status}`; it is *not* wired from a
/// supervisor hook in this chunk (the supervisor only has the coarse `RunOutcome`)
/// — the integrator/loop chunk calls it where the `TerminalStatus` is known.
pub fn record_run_status(status: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_run_status(status);
    #[cfg(not(feature = "metrics"))]
    let _ = status;
}

/// A reactive trigger fired (one reaction).
pub fn record_reaction() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.reactions.fetch_add(1, Ordering::Relaxed);
}

/// Tokens reported up by a direct child (`AgentMsg::Usage`).
///
/// Feeds both the legacy bare `agentd_tokens_{input,output}_total` and the frozen
/// `agentd_tokens_total{type}` (RFC 0016 §4.3). The §4.3 schema also carries a
/// `model` label; the `AgentMsg::Usage` control-channel message this call site
/// rides does not carry the model, so the `model` label is absent here — wiring
/// the `model` label needs a new call site at the intelligence boundary (see the
/// integration caveat).
pub fn record_tokens(input: u64, output: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_tokens(input, output);
    #[cfg(not(feature = "metrics"))]
    let _ = (input, output);
}

/// The restart governor's circuit breaker tripped.
pub fn record_restart_tripped() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .restarts_tripped
        .fetch_add(1, Ordering::Relaxed);
}

/// One loop step executed (`loop.step`, RFC 0010 §3.3). Drives
/// `agentd_loop_steps_total` (RFC 0016 §4.3).
pub fn record_loop_step() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.loop_steps.fetch_add(1, Ordering::Relaxed);
}

/// A refusal / guard trip by reason (RFC 0016 §4.3 `agentd_refusals_total`).
///
/// `reason` is the §4.3 closed domain (`trifecta`/`rate`/`budget`/`depth`/`mcp`);
/// an unknown value buckets under `other`.
pub fn record_refusal(reason: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_refusal(reason);
    #[cfg(not(feature = "metrics"))]
    let _ = reason;
}

/// A hard bound trip (`limit.exceeded`, RFC 0010 §3.3 / RFC 0016 §4.3).
///
/// `limit` is the §4.3 closed domain (`steps`/`tokens`/`deadline`/`depth`/
/// `tree_tokens`/`restart_storm`/`spawn_rate`); an unknown value buckets under
/// `other`.
pub fn record_limit_exceeded(limit: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_limit_exceeded(limit);
    #[cfg(not(feature = "metrics"))]
    let _ = limit;
}

/// A subagent was spawned (`subagent.spawn`, RFC 0016 §4.3).
pub fn record_subagent_spawned() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .subagents_spawned
        .fetch_add(1, Ordering::Relaxed);
}

/// A subagent exited with a terminal `status` (`subagent.exit`, RFC 0007 §3.4).
/// Drives `agentd_subagents_exited_total{status}` (RFC 0016 §4.3).
pub fn record_subagent_exited(status: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_subagent_exited(status);
    #[cfg(not(feature = "metrics"))]
    let _ = status;
}

/// A subagent was restarted by the governor (`subagent.restart`, RFC 0003 §3.7).
/// Drives `agentd_subagent_restarts_total{reason}` (RFC 0016 §4.3).
pub fn record_subagent_restart(reason: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_subagent_restart(reason);
    #[cfg(not(feature = "metrics"))]
    let _ = reason;
}

/// A wedged/stuck subagent was killed (`subagent.stuck`, RFC 0003 — the
/// reliability headline). Drives `agentd_subagent_stuck_kills_total{signal}`
/// (RFC 0016 §4.3); `signal` ∈ `term`\|`kill` (an unknown value buckets `other`).
pub fn record_subagent_stuck_kill(signal: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_subagent_stuck_kill(signal);
    #[cfg(not(feature = "metrics"))]
    let _ = signal;
}

/// An intelligence call was made (`intel.call`, RFC 0016 §4.3
/// `agentd_intel_calls_total`).
pub fn record_intel_call() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.intel_calls.fetch_add(1, Ordering::Relaxed);
}

/// An intelligence-endpoint error by reason (RFC 0016 §4.3
/// `agentd_intel_errors_total`). `reason` ∈ `unreachable`\|`auth`\|`timeout`\|
/// `5xx` (an unknown value buckets `other`).
pub fn record_intel_error(reason: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_intel_error(reason);
    #[cfg(not(feature = "metrics"))]
    let _ = reason;
}

/// An MCP connect attempt failed for a declared `server` (`mcp.connect.fail`,
/// RFC 0016 §4.3 `agentd_mcp_connect_failures_total`). `server` is the declared
/// server name (bounded — there is a fixed, small declared set, RFC 0004); an
/// over-capacity name buckets under `other` so the series stays bounded.
pub fn record_mcp_connect_failure(server: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_mcp_connect_failure(server);
    #[cfg(not(feature = "metrics"))]
    let _ = server;
}

/// A drain phase transition (RFC 0011 §4 / RFC 0016 §4.3 `agentd_drains_total`).
/// `phase` ∈ `started`\|`completed`\|`forced` (an unknown value buckets `other`).
pub fn record_drain(phase: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_drain(phase);
    #[cfg(not(feature = "metrics"))]
    let _ = phase;
}

/// A supervisor process restart was observed (rebuild+reconcile, RFC 0003 §3.11).
/// Drives `agentd_restarts_total` (RFC 0016 §4.3) — distinct from the breaker-trip
/// counter [`record_restart_tripped`].
pub fn record_supervisor_restart() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .supervisor_restarts
        .fetch_add(1, Ordering::Relaxed);
}

/// A wedged-reactor liveness trip (RFC 0003 / RFC 0016 §5 / §4.3
/// `agentd_reactor_stalls_total`).
pub fn record_reactor_stall() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.reactor_stalls.fetch_add(1, Ordering::Relaxed);
}

/// Point-in-time set of the intelligence-endpoint reachability gauge
/// (`agentd_intel_up`, RFC 0016 §4.3 — RFC 0006/0018).
pub fn set_intel_up(up: bool) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .intel_up
        .store(u64::from(up), Ordering::Relaxed);
    #[cfg(not(feature = "metrics"))]
    let _ = up;
}

/// Point-in-time set of the subagent-tree shape gauges (RFC 0016 §4.3:
/// `agentd_active_subagents` / `agentd_tree_depth` / `agentd_tree_breadth`).
pub fn set_tree_shape(active: u64, depth: u64, breadth: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.set_tree_shape(active, depth, breadth);
    #[cfg(not(feature = "metrics"))]
    let _ = (active, depth, breadth);
}

/// Point-in-time set of the reactive backlog gauges — the RFC 0019 scaling signal
/// set (RFC 0016 §4.3: `agentd_pending_events` / `agentd_inflight_reactions` /
/// `agentd_subscriptions_active` / `agentd_reaction_lag_ms`).
pub fn set_reactive_backlog(pending: u64, inflight: u64, subscriptions: u64, lag_ms: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.set_reactive_backlog(pending, inflight, subscriptions, lag_ms);
    #[cfg(not(feature = "metrics"))]
    let _ = (pending, inflight, subscriptions, lag_ms);
}

/// An item was dropped as out-of-shard (RFC 0019 §4.1 / §5.1
/// `agentd_shard_skipped_total`). The shard gate is the cheap pre-filter applied
/// at routing intake before any spawn; this counts the items this replica rejects.
pub fn record_shard_skipped() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.shard_skipped.fetch_add(1, Ordering::Relaxed);
}

/// A claim was lost to another replica (RFC 0019 §5.1 `agentd_claims_lost_total`).
/// **Frozen-but-unfed**: claim/lease (RFC 0019 §3) is DEFERRED (§12), so nothing
/// increments this yet — the name is reserved now so the schema is stable when the
/// claim mechanism lands (exactly like the other pre-frozen RFC 0016 series).
pub fn record_claim_lost() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.claims_lost.fetch_add(1, Ordering::Relaxed);
}

/// Set the saturation gauge — `in_flight / capacity` in `[0.0, 1.0]` (RFC 0019
/// §5.1), the HPA "utilization" target. Stored as basis points (0..=10000) in a
/// u64 atomic and rendered as `value/10000.0`, so the gauge stays a plain atomic
/// (telemetry never allocates / never fails). `numerator`/`denominator` are the
/// live in-flight count and the capacity cap; a zero denominator reads 0.0.
pub fn set_saturation(numerator: u64, denominator: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.set_saturation(numerator, denominator);
    #[cfg(not(feature = "metrics"))]
    let _ = (numerator, denominator);
}

/// Render the current counters (+ live cgroup memory gauges) as Prometheus text.
#[cfg(feature = "metrics")]
pub fn render_prometheus() -> String {
    let mut s = imp::REGISTRY.render();
    s.push_str(&imp::memory_gauges(crate::supervisor::cgroup::snapshot()));
    s
}

#[cfg(feature = "metrics")]
use std::sync::atomic::Ordering;

#[cfg(feature = "metrics")]
mod imp {
    use super::RunOutcome;
    use std::fmt::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) static REGISTRY: Registry = Registry::new();

    // --- closed label domains (RFC 0016 §4.2/§4.3) ---------------------------
    // Each is a fixed array of `(label_value, AtomicU64)`; an out-of-vocabulary
    // value lands in the trailing `other` slot so the series stays bounded. The
    // arrays ARE the cardinality bound — there is no map, no allocation, no
    // unbounded label key path.

    /// `agentd_runs_total{status}` / `agentd_subagents_exited_total{status}`
    /// label domain: the RFC 0007 §3.4 closed terminal-status vocabulary
    /// (verbatim from `TerminalStatus::as_str`), plus `other`.
    const STATUS_LABELS: &[&str] = &[
        "completed",
        "refused",
        "exhausted_steps",
        "exhausted_tokens",
        "deadline",
        "stalled",
        "loop_detected",
        "cancelled",
        "crashed",
        "other",
    ];

    /// `agentd_refusals_total{reason}` label domain (RFC 0016 §4.3).
    const REFUSAL_REASONS: &[&str] = &["trifecta", "rate", "budget", "depth", "mcp", "other"];

    /// `agentd_limit_exceeded_total{limit}` label domain (mirrors the
    /// `limit.exceeded` event's `limit` field, RFC 0016 §4.3).
    const LIMIT_LABELS: &[&str] = &[
        "steps",
        "tokens",
        "deadline",
        "depth",
        "tree_tokens",
        "restart_storm",
        "spawn_rate",
        "other",
    ];

    /// `agentd_subagent_restarts_total{reason}` label domain (RFC 0003 §3.7).
    const RESTART_REASONS: &[&str] = &["crashed", "stuck", "rate", "other"];

    /// `agentd_subagent_stuck_kills_total{signal}` label domain (RFC 0016 §4.3).
    const SIGNAL_LABELS: &[&str] = &["term", "kill", "other"];

    /// `agentd_intel_errors_total{reason}` label domain (RFC 0016 §4.3).
    const INTEL_ERROR_REASONS: &[&str] = &["unreachable", "auth", "timeout", "5xx", "other"];

    /// `agentd_drains_total{phase}` label domain (RFC 0011 §4 / RFC 0016 §4.3).
    const DRAIN_PHASES: &[&str] = &["started", "completed", "forced", "other"];

    /// `agentd_tokens_total{type}` direction label domain (RFC 0016 §4.3).
    const TOKEN_TYPES: &[&str] = &["in", "out"];

    /// A fixed-domain labelled counter family: one atomic per known label value.
    /// `N` matches the backing domain slice length; the trailing slot is the
    /// `other` catch-all that keeps the cardinality bound (RFC 0016 §4.2).
    struct LabelCounter<const N: usize> {
        slots: [AtomicU64; N],
    }

    impl<const N: usize> LabelCounter<N> {
        const fn new() -> Self {
            LabelCounter {
                slots: [const { AtomicU64::new(0) }; N],
            }
        }

        /// Increment the slot for `value`; an unknown value lands in the last
        /// (`other`) slot. `domain` MUST have length `N`.
        fn inc(&self, domain: &[&str], value: &str) {
            let idx = domain.iter().position(|&l| l == value).unwrap_or(N - 1);
            self.slots[idx].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) struct Registry {
        // --- legacy bare series (RFC 0010 §3.8; kept — additive within major) -
        pub(super) runs_started: AtomicU64,
        runs_completed: AtomicU64,
        runs_failed: AtomicU64,
        runs_killed: AtomicU64,
        pub(super) reactions: AtomicU64,
        tokens_input: AtomicU64,
        tokens_output: AtomicU64,
        pub(super) restarts_tripped: AtomicU64,

        // --- frozen §4.3: run lifecycle & terminal-status --------------------
        runs_total: LabelCounter<{ STATUS_LABELS.len() }>,
        pub(super) loop_steps: AtomicU64,

        // --- frozen §4.3: refusal / bound counters ---------------------------
        refusals: LabelCounter<{ REFUSAL_REASONS.len() }>,
        limit_exceeded: LabelCounter<{ LIMIT_LABELS.len() }>,

        // --- frozen §4.3: subagent-tree gauges + counters --------------------
        active_subagents: AtomicU64,
        tree_depth: AtomicU64,
        tree_breadth: AtomicU64,
        pub(super) subagents_spawned: AtomicU64,
        subagents_exited: LabelCounter<{ STATUS_LABELS.len() }>,
        subagent_restarts: LabelCounter<{ RESTART_REASONS.len() }>,
        subagent_stuck_kills: LabelCounter<{ SIGNAL_LABELS.len() }>,

        // --- frozen §4.3: intelligence health --------------------------------
        pub(super) intel_calls: AtomicU64,
        pub(super) intel_up: AtomicU64,
        intel_errors: LabelCounter<{ INTEL_ERROR_REASONS.len() }>,

        // --- frozen §4.3: MCP server health ----------------------------------
        mcp_connect_failures: LabelCounter<{ MCP_SERVER_SLOTS }>,

        // --- frozen §4.3: lifecycle events -----------------------------------
        drains: LabelCounter<{ DRAIN_PHASES.len() }>,
        pub(super) supervisor_restarts: AtomicU64,
        pub(super) reactor_stalls: AtomicU64,

        // --- frozen §4.3: token accounting (typed) ---------------------------
        tokens_typed: LabelCounter<{ TOKEN_TYPES.len() }>,

        // --- frozen §4.3: reactive backlog (the RFC 0019 scaling signal set) --
        pending_events: AtomicU64,
        inflight_reactions: AtomicU64,
        subscriptions_active: AtomicU64,
        reaction_lag_ms: AtomicU64,

        // --- RFC 0019 §5.1: horizontal-scaling signals ----------------------
        // `agentd_saturation` is stored as basis points (0..=10000) and rendered
        // as a float in [0,1]. `agentd_shard_skipped_total` counts out-of-shard
        // drops; `agentd_claims_lost_total` is frozen-but-unfed (claim is deferred).
        saturation_bp: AtomicU64,
        pub(super) shard_skipped: AtomicU64,
        pub(super) claims_lost: AtomicU64,
    }

    impl Registry {
        const fn new() -> Registry {
            Registry {
                runs_started: AtomicU64::new(0),
                runs_completed: AtomicU64::new(0),
                runs_failed: AtomicU64::new(0),
                runs_killed: AtomicU64::new(0),
                reactions: AtomicU64::new(0),
                tokens_input: AtomicU64::new(0),
                tokens_output: AtomicU64::new(0),
                restarts_tripped: AtomicU64::new(0),
                runs_total: LabelCounter::new(),
                loop_steps: AtomicU64::new(0),
                refusals: LabelCounter::new(),
                limit_exceeded: LabelCounter::new(),
                active_subagents: AtomicU64::new(0),
                tree_depth: AtomicU64::new(0),
                tree_breadth: AtomicU64::new(0),
                subagents_spawned: AtomicU64::new(0),
                subagents_exited: LabelCounter::new(),
                subagent_restarts: LabelCounter::new(),
                subagent_stuck_kills: LabelCounter::new(),
                intel_calls: AtomicU64::new(0),
                intel_up: AtomicU64::new(0),
                intel_errors: LabelCounter::new(),
                mcp_connect_failures: LabelCounter::new(),
                drains: LabelCounter::new(),
                supervisor_restarts: AtomicU64::new(0),
                reactor_stalls: AtomicU64::new(0),
                tokens_typed: LabelCounter::new(),
                pending_events: AtomicU64::new(0),
                inflight_reactions: AtomicU64::new(0),
                subscriptions_active: AtomicU64::new(0),
                reaction_lag_ms: AtomicU64::new(0),
                saturation_bp: AtomicU64::new(0),
                shard_skipped: AtomicU64::new(0),
                claims_lost: AtomicU64::new(0),
            }
        }

        pub(super) fn record_run(&self, outcome: RunOutcome) {
            // Legacy bare counters.
            let c = match outcome {
                RunOutcome::Completed => &self.runs_completed,
                RunOutcome::Failed => &self.runs_failed,
                RunOutcome::Killed => &self.runs_killed,
            };
            c.fetch_add(1, Ordering::Relaxed);
            // Frozen §4.3 `agentd_runs_total{status}` — a COARSE projection of the
            // three `RunOutcome` variants this hook carries onto the RFC 0007 §3.4
            // domain (a precise status needs `record_run_status`, see the caveat).
            let status = match outcome {
                RunOutcome::Completed => "completed",
                RunOutcome::Failed => "crashed",
                RunOutcome::Killed => "cancelled",
            };
            self.runs_total.inc(STATUS_LABELS, status);
        }

        pub(super) fn record_run_status(&self, status: &str) {
            self.runs_total.inc(STATUS_LABELS, status);
        }

        pub(super) fn record_tokens(&self, input: u64, output: u64) {
            self.tokens_input.fetch_add(input, Ordering::Relaxed);
            self.tokens_output.fetch_add(output, Ordering::Relaxed);
            // Frozen §4.3 `agentd_tokens_total{type}` (the `model` label is not
            // available at the `AgentMsg::Usage` hook — see the caveat).
            self.tokens_typed.slots[0].fetch_add(input, Ordering::Relaxed);
            self.tokens_typed.slots[1].fetch_add(output, Ordering::Relaxed);
        }

        pub(super) fn record_refusal(&self, reason: &str) {
            self.refusals.inc(REFUSAL_REASONS, reason);
        }

        pub(super) fn record_limit_exceeded(&self, limit: &str) {
            self.limit_exceeded.inc(LIMIT_LABELS, limit);
        }

        pub(super) fn record_subagent_exited(&self, status: &str) {
            self.subagents_exited.inc(STATUS_LABELS, status);
        }

        pub(super) fn record_subagent_restart(&self, reason: &str) {
            self.subagent_restarts.inc(RESTART_REASONS, reason);
        }

        pub(super) fn record_subagent_stuck_kill(&self, signal: &str) {
            self.subagent_stuck_kills.inc(SIGNAL_LABELS, signal);
        }

        pub(super) fn record_intel_error(&self, reason: &str) {
            self.intel_errors.inc(INTEL_ERROR_REASONS, reason);
        }

        pub(super) fn record_mcp_connect_failure(&self, server: &str) {
            mcp_servers().record_failure(&self.mcp_connect_failures, server);
        }

        pub(super) fn record_drain(&self, phase: &str) {
            self.drains.inc(DRAIN_PHASES, phase);
        }

        pub(super) fn set_tree_shape(&self, active: u64, depth: u64, breadth: u64) {
            self.active_subagents.store(active, Ordering::Relaxed);
            self.tree_depth.store(depth, Ordering::Relaxed);
            self.tree_breadth.store(breadth, Ordering::Relaxed);
        }

        pub(super) fn set_reactive_backlog(
            &self,
            pending: u64,
            inflight: u64,
            subscriptions: u64,
            lag_ms: u64,
        ) {
            self.pending_events.store(pending, Ordering::Relaxed);
            self.inflight_reactions.store(inflight, Ordering::Relaxed);
            self.subscriptions_active
                .store(subscriptions, Ordering::Relaxed);
            self.reaction_lag_ms.store(lag_ms, Ordering::Relaxed);
        }

        pub(super) fn set_saturation(&self, numerator: u64, denominator: u64) {
            // Store as basis points (0..=10000) so the gauge stays a plain atomic;
            // `render` divides by 10000.0 to emit the [0,1] float. Clamp to the cap
            // so a transient over-cap in-flight never reports > 1.0. A zero capacity
            // is reported as 0 (no work possible ⇒ no saturation), never a div-by-0.
            let bp = numerator
                .saturating_mul(10_000)
                .checked_div(denominator)
                .unwrap_or(0)
                .min(10_000);
            self.saturation_bp.store(bp, Ordering::Relaxed);
        }

        pub(super) fn render(&self) -> String {
            let mut s = String::new();
            let g = |a: &AtomicU64| a.load(Ordering::Relaxed);

            // --- liveness / readiness gauges (RFC 0016 §4.3) -----------------
            // `agentd_up` is always 1 while we can render. `agentd_ready` is
            // derived from the same process-wide drain/lame-duck state `/readyz`
            // reports (RFC 0010 §3.7) — read-only, no extra call site.
            gauge(&mut s, "agentd_up", "1 while the process is alive", 1);
            let ready = u64::from(!crate::signals::draining() && !crate::signals::lame_duck());
            gauge(
                &mut s,
                "agentd_ready",
                "1 when ready to accept work (not draining / lame-ducked)",
                ready,
            );

            // --- run lifecycle & terminal-status (RFC 0016 §4.3) -------------
            labelled_counter(
                &mut s,
                "agentd_runs_total",
                "Runs by terminal status (RFC 0007 §3.4).",
                "status",
                STATUS_LABELS,
                &self.runs_total,
            );
            counter(
                &mut s,
                "agentd_loop_steps_total",
                "Agentic loop steps executed.",
                g(&self.loop_steps),
            );

            // --- token / cost accounting (RFC 0016 §4.3) ---------------------
            // `agentd_tokens_total{type}` — `model` label deferred (see caveat).
            labelled_counter(
                &mut s,
                "agentd_tokens_total",
                "Model tokens by direction.",
                "type",
                TOKEN_TYPES,
                &self.tokens_typed,
            );
            counter(
                &mut s,
                "agentd_intel_calls_total",
                "Intelligence calls made.",
                g(&self.intel_calls),
            );

            // --- refusal / bound counters (RFC 0016 §4.3) --------------------
            labelled_counter(
                &mut s,
                "agentd_refusals_total",
                "Refusals/guard trips by reason.",
                "reason",
                REFUSAL_REASONS,
                &self.refusals,
            );
            labelled_counter(
                &mut s,
                "agentd_limit_exceeded_total",
                "Hard-bound trips by limit.",
                "limit",
                LIMIT_LABELS,
                &self.limit_exceeded,
            );

            // --- subagent-tree gauges + counters (RFC 0016 §4.3) -------------
            gauge(
                &mut s,
                "agentd_active_subagents",
                "Subagents currently alive in the tree.",
                g(&self.active_subagents),
            );
            gauge(
                &mut s,
                "agentd_tree_depth",
                "Current max subagent-tree depth.",
                g(&self.tree_depth),
            );
            gauge(
                &mut s,
                "agentd_tree_breadth",
                "Current max siblings at any tree node.",
                g(&self.tree_breadth),
            );
            counter(
                &mut s,
                "agentd_subagents_spawned_total",
                "Subagents spawned.",
                g(&self.subagents_spawned),
            );
            labelled_counter(
                &mut s,
                "agentd_subagents_exited_total",
                "Subagents exited by terminal status (RFC 0007 §3.4).",
                "status",
                STATUS_LABELS,
                &self.subagents_exited,
            );
            labelled_counter(
                &mut s,
                "agentd_subagent_restarts_total",
                "Subagent restarts by reason (RFC 0003 §3.7).",
                "reason",
                RESTART_REASONS,
                &self.subagent_restarts,
            );
            labelled_counter(
                &mut s,
                "agentd_subagent_stuck_kills_total",
                "Wedged-subagent kills by signal (RFC 0003).",
                "signal",
                SIGNAL_LABELS,
                &self.subagent_stuck_kills,
            );

            // --- intelligence health (RFC 0016 §4.3) -------------------------
            gauge(
                &mut s,
                "agentd_intel_up",
                "1 when the intelligence endpoint is reachable.",
                g(&self.intel_up),
            );
            labelled_counter(
                &mut s,
                "agentd_intel_errors_total",
                "Intelligence-endpoint errors by reason.",
                "reason",
                INTEL_ERROR_REASONS,
                &self.intel_errors,
            );

            // --- MCP server health (RFC 0016 §4.3) ---------------------------
            // `agentd_mcp_up{server}` is gauge-per-declared-server; this chunk
            // has no declared-server registration hook, so it is deferred (see
            // caveat). The connect-failure counter is wired by `server`.
            mcp_servers().render_connect_failures(&mut s, &self.mcp_connect_failures);

            // --- lifecycle events (RFC 0016 §4.3) ----------------------------
            labelled_counter(
                &mut s,
                "agentd_drains_total",
                "Drain phase transitions (RFC 0011 §4).",
                "phase",
                DRAIN_PHASES,
                &self.drains,
            );
            counter(
                &mut s,
                "agentd_restarts_total",
                "Supervisor process restarts observed (RFC 0003 §3.11).",
                g(&self.supervisor_restarts),
            );
            counter(
                &mut s,
                "agentd_reactor_stalls_total",
                "Wedged-reactor liveness trips (RFC 0003).",
                g(&self.reactor_stalls),
            );

            // --- reactive backlog — the RFC 0019 scaling signal set (§4.3) ---
            gauge(
                &mut s,
                "agentd_pending_events",
                "Reactive events received but not yet routed.",
                g(&self.pending_events),
            );
            gauge(
                &mut s,
                "agentd_inflight_reactions",
                "Reactions currently executing.",
                g(&self.inflight_reactions),
            );
            gauge(
                &mut s,
                "agentd_subscriptions_active",
                "Reconciled declared subscriptions.",
                g(&self.subscriptions_active),
            );
            gauge(
                &mut s,
                "agentd_reaction_lag_ms",
                "Age of the oldest un-routed pending event (ms).",
                g(&self.reaction_lag_ms),
            );

            // --- horizontal-scaling signals (RFC 0019 §5.1) ------------------
            // `agentd_saturation` is in_flight/capacity in [0,1] — the HPA target.
            // Stored as basis points; rendered as the float.
            let sat = g(&self.saturation_bp) as f64 / 10_000.0;
            gauge_f64(
                &mut s,
                "agentd_saturation",
                "In-flight / capacity utilization in [0,1] (RFC 0019 §5.1).",
                sat,
            );
            counter(
                &mut s,
                "agentd_shard_skipped_total",
                "Items dropped as out-of-shard (RFC 0019 §4.1).",
                g(&self.shard_skipped),
            );
            // Frozen-but-unfed: claim/lease is deferred (RFC 0019 §3/§12), so this
            // reads 0; the name is reserved now so the schema is stable on landing.
            counter(
                &mut s,
                "agentd_claims_lost_total",
                "Work claims lost to another replica (RFC 0019 §5.1; claim deferred).",
                g(&self.claims_lost),
            );

            // --- legacy bare series (RFC 0010 §3.8; retained, additive) ------
            counter(
                &mut s,
                "agentd_runs_started_total",
                "Supervised runs started",
                g(&self.runs_started),
            );
            counter(
                &mut s,
                "agentd_runs_completed_total",
                "Supervised runs that completed",
                g(&self.runs_completed),
            );
            counter(
                &mut s,
                "agentd_runs_failed_total",
                "Supervised runs that failed on infra",
                g(&self.runs_failed),
            );
            counter(
                &mut s,
                "agentd_runs_killed_total",
                "Supervised runs torn down by the supervisor",
                g(&self.runs_killed),
            );
            counter(
                &mut s,
                "agentd_reactions_total",
                "Reactive triggers fired",
                g(&self.reactions),
            );
            counter(
                &mut s,
                "agentd_tokens_input_total",
                "Input tokens reported by direct children",
                g(&self.tokens_input),
            );
            counter(
                &mut s,
                "agentd_tokens_output_total",
                "Output tokens reported by direct children",
                g(&self.tokens_output),
            );
            counter(
                &mut s,
                "agentd_restarts_tripped_total",
                "Restart-governor breaker trips",
                g(&self.restarts_tripped),
            );
            s
        }
    }

    // --- `agentd_mcp_connect_failures_total{server}` -------------------------
    // The `server` label is bounded (RFC 0004: a small, fixed declared set) but
    // its *values* are config-time strings, not a compile-time enum. We bound it
    // structurally with a fixed slot table that interns server names on first use;
    // once full, further names fold into `other` so the series can never grow
    // unbounded (RFC 0016 §4.2). The table is a process-global behind a Mutex —
    // a slow path touched only on a connect failure, never on the render hot path
    // beyond a read snapshot.

    /// Max distinct `server` label values held before folding into `other`.
    const MCP_SERVER_SLOTS: usize = 16;

    struct McpServerTable {
        names: std::sync::Mutex<Vec<String>>,
    }

    impl McpServerTable {
        const fn new() -> Self {
            McpServerTable {
                names: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// Index for `server`; interns on first use, or the `other` slot
        /// (`MCP_SERVER_SLOTS - 1`) once the table is full. Poisoning is ignored
        /// (telemetry never crashes the agent — RFC 0016 §8.4).
        fn index(&self, server: &str) -> usize {
            let mut names = match self.names.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(i) = names.iter().position(|n| n == server) {
                return i;
            }
            if names.len() < MCP_SERVER_SLOTS - 1 {
                names.push(server.to_string());
                return names.len() - 1;
            }
            MCP_SERVER_SLOTS - 1
        }

        fn record_failure(&self, ctr: &LabelCounter<MCP_SERVER_SLOTS>, server: &str) {
            let idx = self.index(server);
            ctr.slots[idx].fetch_add(1, Ordering::Relaxed);
        }

        /// Emit one `agentd_mcp_connect_failures_total{server="…"}` line per
        /// interned server with a non-zero count, plus the `other` overflow slot.
        fn render_connect_failures(&self, s: &mut String, ctr: &LabelCounter<MCP_SERVER_SLOTS>) {
            let names = match self.names.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let name = "agentd_mcp_connect_failures_total";
            let _ = writeln!(s, "# HELP {name} MCP connect failures by server.");
            let _ = writeln!(s, "# TYPE {name} counter");
            for (i, server) in names.iter().enumerate() {
                let v = ctr.slots[i].load(Ordering::Relaxed);
                let _ = writeln!(s, "{name}{{server={:?}}} {v}", server.as_str());
            }
            let other = ctr.slots[MCP_SERVER_SLOTS - 1].load(Ordering::Relaxed);
            if other != 0 {
                let _ = writeln!(s, "{name}{{server=\"other\"}} {other}");
            }
        }
    }

    fn mcp_servers() -> &'static McpServerTable {
        static TABLE: McpServerTable = McpServerTable::new();
        &TABLE
    }

    /// One counter family in Prometheus text exposition format.
    fn counter(s: &mut String, name: &str, help: &str, value: u64) {
        let _ = writeln!(s, "# HELP {name} {help}");
        let _ = writeln!(s, "# TYPE {name} counter");
        let _ = writeln!(s, "{name} {value}");
    }

    /// One gauge family (point-in-time value) in Prometheus text format.
    fn gauge(s: &mut String, name: &str, help: &str, value: u64) {
        let _ = writeln!(s, "# HELP {name} {help}");
        let _ = writeln!(s, "# TYPE {name} gauge");
        let _ = writeln!(s, "{name} {value}");
    }

    /// One float-valued gauge family (e.g. a [0,1] ratio) in Prometheus text.
    fn gauge_f64(s: &mut String, name: &str, help: &str, value: f64) {
        let _ = writeln!(s, "# HELP {name} {help}");
        let _ = writeln!(s, "# TYPE {name} gauge");
        let _ = writeln!(s, "{name} {value}");
    }

    /// One labelled counter family: a single HELP/TYPE header, then one series
    /// line per closed-domain label value (RFC 0016 §4.2 — the domain is the
    /// bound). `domain` and the `LabelCounter` slots are the same length.
    fn labelled_counter<const N: usize>(
        s: &mut String,
        name: &str,
        help: &str,
        label: &str,
        domain: &[&str],
        ctr: &LabelCounter<N>,
    ) {
        let _ = writeln!(s, "# HELP {name} {help}");
        let _ = writeln!(s, "# TYPE {name} counter");
        for (i, value) in domain.iter().enumerate() {
            let v = ctr.slots[i].load(Ordering::Relaxed);
            let _ = writeln!(s, "{name}{{{label}={value:?}}} {v}");
        }
    }

    /// Live cgroup v2 memory gauges, emitted only for fields the kernel exposes
    /// (kept out of `Registry::render` so the counter set stays deterministic).
    pub(super) fn memory_gauges(mem: crate::supervisor::cgroup::MemorySnapshot) -> String {
        let mut s = String::new();
        if let Some(v) = mem.max {
            gauge(
                &mut s,
                "agentd_memory_max_bytes",
                "cgroup v2 memory.max hard limit (bytes)",
                v,
            );
        }
        if let Some(v) = mem.current {
            gauge(
                &mut s,
                "agentd_memory_current_bytes",
                "cgroup v2 memory.current usage (bytes)",
                v,
            );
        }
        s
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn render_is_valid_prometheus_text() {
            let r = Registry::new();
            r.runs_started.fetch_add(3, Ordering::Relaxed);
            r.record_run(RunOutcome::Completed);
            r.record_run(RunOutcome::Failed);
            r.record_tokens(100, 50);
            let out = r.render();
            assert!(out.contains("# TYPE agentd_runs_started_total counter"));
            assert!(out.contains("agentd_runs_started_total 3"));
            assert!(out.contains("agentd_runs_completed_total 1"));
            assert!(out.contains("agentd_runs_failed_total 1"));
            assert!(out.contains("agentd_tokens_input_total 100"));
            assert!(out.contains("agentd_tokens_output_total 50"));
        }

        #[test]
        fn frozen_schema_emits_up_and_ready_gauges() {
            let r = Registry::new();
            let out = r.render();
            // RFC 0016 §4.3 liveness/readiness gauges, label-free.
            assert!(out.contains("# TYPE agentd_up gauge"));
            assert!(out.contains("agentd_up 1"));
            assert!(out.contains("# TYPE agentd_ready gauge"));
            // ready is 0/1; in a bare test process (no drain) it is 1.
            assert!(out.contains("agentd_ready "));
        }

        #[test]
        fn runs_total_uses_the_closed_status_domain() {
            let r = Registry::new();
            r.record_run_status("completed");
            r.record_run_status("refused");
            r.record_run_status("refused");
            // an out-of-vocabulary status buckets under `other`, never a new label
            r.record_run_status("totally_made_up");
            let out = r.render();
            assert!(out.contains("agentd_runs_total{status=\"completed\"} 1"));
            assert!(out.contains("agentd_runs_total{status=\"refused\"} 2"));
            assert!(out.contains("agentd_runs_total{status=\"other\"} 1"));
            // every closed-domain value is present (zero-valued series included)
            assert!(out.contains("agentd_runs_total{status=\"loop_detected\"} 0"));
            // exactly one HELP/TYPE header for the family
            assert_eq!(out.matches("# TYPE agentd_runs_total counter").count(), 1);
        }

        #[test]
        fn typed_tokens_track_direction() {
            let r = Registry::new();
            r.record_tokens(880, 40);
            r.record_tokens(120, 10);
            let out = r.render();
            assert!(out.contains("agentd_tokens_total{type=\"in\"} 1000"));
            assert!(out.contains("agentd_tokens_total{type=\"out\"} 50"));
        }

        #[test]
        fn refusals_and_limits_use_closed_domains() {
            let r = Registry::new();
            r.record_refusal("trifecta");
            r.record_refusal("depth");
            r.record_refusal("depth");
            r.record_limit_exceeded("spawn_rate");
            let out = r.render();
            assert!(out.contains("agentd_refusals_total{reason=\"trifecta\"} 1"));
            assert!(out.contains("agentd_refusals_total{reason=\"depth\"} 2"));
            assert!(out.contains("agentd_limit_exceeded_total{limit=\"spawn_rate\"} 1"));
            // closed domains: a stray reason never widens the label set
            r.record_refusal("nope");
            assert!(
                r.render()
                    .contains("agentd_refusals_total{reason=\"other\"} 1")
            );
        }

        #[test]
        fn tree_and_backlog_gauges_are_settable() {
            let r = Registry::new();
            r.set_tree_shape(4, 2, 3);
            r.set_reactive_backlog(7, 1, 9, 250);
            let out = r.render();
            assert!(out.contains("agentd_active_subagents 4"));
            assert!(out.contains("agentd_tree_depth 2"));
            assert!(out.contains("agentd_tree_breadth 3"));
            assert!(out.contains("agentd_pending_events 7"));
            assert!(out.contains("agentd_inflight_reactions 1"));
            assert!(out.contains("agentd_subscriptions_active 9"));
            assert!(out.contains("agentd_reaction_lag_ms 250"));
        }

        #[test]
        fn horizontal_scaling_signals_render() {
            // RFC 0019 §5.1: saturation (float [0,1]), shard-skip counter, and the
            // frozen-but-unfed claims-lost counter.
            let r = Registry::new();
            // 35/64 in-flight → 5468 bp → 0.5468 (basis-point granularity).
            r.set_saturation(35, 64);
            r.shard_skipped.fetch_add(3, Ordering::Relaxed);
            let out = r.render();
            assert!(out.contains("# TYPE agentd_saturation gauge"));
            assert!(out.contains("agentd_saturation 0.5468"));
            assert!(out.contains("# TYPE agentd_shard_skipped_total counter"));
            assert!(out.contains("agentd_shard_skipped_total 3"));
            // claims-lost is frozen-but-unfed: present, reads 0.
            assert!(out.contains("# TYPE agentd_claims_lost_total counter"));
            assert!(out.contains("agentd_claims_lost_total 0"));
        }

        #[test]
        fn saturation_clamps_and_guards_zero_capacity() {
            let r = Registry::new();
            // over-cap in-flight clamps to 1.0
            r.set_saturation(100, 64);
            assert!(r.render().contains("agentd_saturation 1"));
            // zero capacity → 0.0 (never a div-by-zero)
            r.set_saturation(5, 0);
            assert!(r.render().contains("agentd_saturation 0"));
        }

        #[test]
        fn mcp_connect_failures_label_by_server_and_fold_overflow() {
            let r = Registry::new();
            r.record_mcp_connect_failure("github");
            r.record_mcp_connect_failure("github");
            r.record_mcp_connect_failure("filesystem");
            let out = r.render();
            assert!(out.contains("agentd_mcp_connect_failures_total{server=\"github\"} 2"));
            assert!(out.contains("agentd_mcp_connect_failures_total{server=\"filesystem\"} 1"));
        }

        #[test]
        fn drains_phase_distinguishes_clean_from_forced() {
            let r = Registry::new();
            r.record_drain("started");
            r.record_drain("completed");
            r.record_drain("forced");
            let out = r.render();
            assert!(out.contains("agentd_drains_total{phase=\"completed\"} 1"));
            assert!(out.contains("agentd_drains_total{phase=\"forced\"} 1"));
        }

        #[test]
        fn no_unbounded_identifier_labels_leak() {
            // §4.2 cardinality: render must never contain a run_id/agent_path-style
            // label key. We assert the only label keys present are the bounded set.
            let r = Registry::new();
            r.record_run_status("completed");
            r.record_tokens(1, 1);
            r.record_refusal("trifecta");
            r.record_mcp_connect_failure("github");
            let out = r.render();
            for forbidden in [
                "run_id=",
                "agent_id=",
                "agent_path=",
                "call_id=",
                "session_id=",
                "uri=",
            ] {
                assert!(
                    !out.contains(forbidden),
                    "leaked unbounded label: {forbidden}"
                );
            }
        }

        #[test]
        fn memory_gauges_emit_only_present_fields() {
            use crate::supervisor::cgroup::MemorySnapshot;
            // a limited cgroup → two gauge families
            let g = memory_gauges(MemorySnapshot {
                max: Some(1024),
                current: Some(512),
                high: None,
            });
            assert!(g.contains("# TYPE agentd_memory_max_bytes gauge"));
            assert!(g.contains("agentd_memory_max_bytes 1024"));
            assert!(g.contains("agentd_memory_current_bytes 512"));
            assert_eq!(g.matches(" gauge\n").count(), 2);
            // no cgroup → no gauge lines (keeps /metrics clean off-cgroup)
            assert!(memory_gauges(MemorySnapshot::default()).is_empty());
        }
    }
}
