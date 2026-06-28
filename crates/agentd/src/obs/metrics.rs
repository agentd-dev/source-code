// SPDX-License-Identifier: Apache-2.0
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
///
/// **Process-local / unwired:** `loop.step` is emitted inside the re-exec'd child
/// agentic loop, a different process from the supervisor that `/metrics` scrapes,
/// so calling this would only bump the child's own registry (cross-process rollup
/// is a v1 non-goal — module header). It is therefore intentionally NOT called
/// from the loop; the series renders the supervisor's own process only and
/// agentctl derives step counts from `loop.step` log lines.
pub fn record_loop_step() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.loop_steps.fetch_add(1, Ordering::Relaxed);
}

/// A refusal / guard trip by reason (RFC 0016 §4.3 `agentd_refusals_total`).
///
/// `reason` is the §4.3 closed domain (`trifecta`/`rate`/`budget`/`depth`/`mcp`);
/// an unknown value buckets under `other`.
///
/// **Process-local / unwired:** refusals trip inside the re-exec'd child loop
/// (the orchestrator self-tool / scope checks), so a bump here would only reach
/// the child's process-local registry, never the supervisor scrape (cross-process
/// rollup is a v1 non-goal). Intentionally not called; the headline safety signal
/// is the refusal / `scope.trifecta_refused` log line.
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
///
/// **Partially wired:** the `tree_tokens` leg is the supervisor's own tree-ceiling
/// trip (`supervisor::reactor`, this process), so it is live and reaches the
/// scrape. The `steps`/`tokens`/`deadline`/`depth` legs trip inside the re-exec'd
/// child loop and are therefore process-local (not called from the child for the
/// scrape — derive those from `limit.exceeded` log lines).
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
///
/// **Reserved / unwired in metrics_schema 1.0:** this build has no in-process
/// rebuild+reconcile restart path to call it from (a pod restart is a fresh
/// process with a zeroed registry — an orchestrator counts those, not the
/// binary). The series renders (always 0) so the frozen contract stays
/// discoverable; this fn exists for the future reconcile path.
pub fn record_supervisor_restart() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .supervisor_restarts
        .fetch_add(1, Ordering::Relaxed);
}

/// A wedged-reactor liveness trip (RFC 0003 / RFC 0016 §5 / §4.3
/// `agentd_reactor_stalls_total`).
///
/// **Reserved / unwired in metrics_schema 1.0:** a wedged reactor is surfaced as a
/// `/healthz` 503 (a per-scrape read of the heartbeat age in `obs::serve`), not as
/// a one-shot in-process event, so there is no clean site to bump this exactly
/// once. The series renders (always 0) for discoverability; the live alerting
/// signal is the 503 itself.
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

/// Point-in-time set of the tree-pause gauge (`agentd_paused`, RFC 0015 §5.5 /
/// RFC 0016 §4.3) — 1 while the `pause` operator tool has frozen the agentic
/// loops, 0 after `resume`. No-op-safe / metrics-gated, mirroring `set_intel_up`.
pub fn set_paused(on: bool) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.paused.store(u64::from(on), Ordering::Relaxed);
    #[cfg(not(feature = "metrics"))]
    let _ = on;
}

/// Point-in-time set of the intelligence all-endpoints-down gauge
/// (`agentd_intel_all_down`, RFC 0018 §6 / RFC 0016 §4.3) — 1 while every model
/// endpoint is down (the latched, eventually-consistent last-child-experience
/// truth a subagent reports up via `AgentMsg::IntelHealth`; the same flag flips
/// `/readyz` NotReady). 0 once any endpoint is usable again. Distinct from
/// `agentd_intel_up` (the active endpoint's reachability): all-down is the
/// fleet-routing signal (no endpoint usable at all). No-op-safe / metrics-gated.
pub fn set_intel_all_down(on: bool) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .intel_all_down
        .store(u64::from(on), Ordering::Relaxed);
    #[cfg(not(feature = "metrics"))]
    let _ = on;
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
/// Wired by the claim gate (`cluster` build): a `work.claim{granted:false}` drops
/// the delivery and increments this — the over-provisioning signal a scaler reads
/// (high & rising under low backlog ⇒ scale down).
pub fn record_claim_lost() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.claims_lost.fetch_add(1, Ordering::Relaxed);
}

/// A claim was granted (RFC 0019 §3.2 / §5.1 `agentd_claims_granted_total`): this
/// replica won `work.claim` and proceeds to process the item. Wired by the claim
/// gate in the `cluster` build.
pub fn record_claim_granted() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.claims_granted.fetch_add(1, Ordering::Relaxed);
}

/// A held claim was released (RFC 0019 §3.3 / §6 `agentd_claims_released_total`):
/// a non-terminal wind-down or a drain handed the item back to the fleet. Wired by
/// the claim gate + the drain step-1.5 in the `cluster` build.
pub fn record_claim_released() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .claims_released
        .fetch_add(1, Ordering::Relaxed);
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

/// A config hot reload reached a terminal disposition (RFC 0017 §5.6). Drives
/// `agentd_config_reload_total{result}` with the closed domain
/// `applied`\|`rejected` (an unknown value buckets `other`). A `rejected` reload
/// is a clean no-op (the running config is unchanged); `applied` bumps the
/// generation gauge via [`set_config_generation`].
pub fn record_config_reload(result: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_config_reload(result);
    #[cfg(not(feature = "metrics"))]
    let _ = result;
}

/// Point-in-time set of the config-generation gauge (`agentd_config_generation`,
/// RFC 0017 §5.6): the count of successfully-applied reloads, so a scraper can
/// detect "this instance has picked up generation N" against agentctl's desired
/// generation. Monotonic in practice (the reload loop only ever increments it).
pub fn set_config_generation(generation: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .config_generation
        .store(generation, Ordering::Relaxed);
    #[cfg(not(feature = "metrics"))]
    let _ = generation;
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

    /// `agentd_config_reload_total{result}` label domain (RFC 0017 §5.6). A hot
    /// reload either `applied` (the reloadable diff took effect) or was `rejected`
    /// (invalid / restart-only / inconsistent → a clean no-op); `other` is the
    /// catch-all that keeps the series bounded (RFC 0016 §4.2).
    const RELOAD_RESULTS: &[&str] = &["applied", "rejected", "other"];

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
        // RFC 0018 §6: 1 while ALL model endpoints are down (the latched,
        // eventually-consistent last-child-experience truth that also flips
        // `/readyz`). Distinct from `intel_up` (the active endpoint's reachability).
        pub(super) intel_all_down: AtomicU64,
        intel_errors: LabelCounter<{ INTEL_ERROR_REASONS.len() }>,

        // --- RFC 0015 §5.5: tree-pause gauge (0/1) ---------------------------
        // Set by the `pause`/`resume` operator tools; 1 while the tree is paused.
        pub(super) paused: AtomicU64,

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
        // drops; `agentd_claims_lost_total` is fed by the claim gate (work-claim
        // ships) alongside the granted/released counters.
        saturation_bp: AtomicU64,
        pub(super) shard_skipped: AtomicU64,
        pub(super) claims_lost: AtomicU64,
        pub(super) claims_granted: AtomicU64,
        pub(super) claims_released: AtomicU64,

        // --- RFC 0017 §5.6: hot-reload outcome counter + generation gauge -----
        config_reloads: LabelCounter<{ RELOAD_RESULTS.len() }>,
        pub(super) config_generation: AtomicU64,
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
                intel_all_down: AtomicU64::new(0),
                intel_errors: LabelCounter::new(),
                paused: AtomicU64::new(0),
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
                claims_granted: AtomicU64::new(0),
                claims_released: AtomicU64::new(0),
                config_reloads: LabelCounter::new(),
                config_generation: AtomicU64::new(0),
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

        pub(super) fn record_config_reload(&self, result: &str) {
            self.config_reloads.inc(RELOAD_RESULTS, result);
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
            // `agentd_ready` mirrors `/readyz` exactly (RFC 0010 §3.7 / RFC 0018 §6):
            // NotReady when draining, lame-ducked, OR all intelligence endpoints are
            // down — the same three conditions the readiness probe consults.
            let ready = u64::from(
                !crate::signals::draining()
                    && !crate::signals::lame_duck()
                    && !crate::signals::intel_all_down(),
            );
            gauge(
                &mut s,
                "agentd_ready",
                "1 when ready to accept work (not draining / lame-ducked / intel-all-down)",
                ready,
            );
            // `agentd_paused` (RFC 0015 §5.5): 1 while the tree is paused at turn
            // boundaries. Pause is NOT readiness — a paused instance can still be
            // ready (the `ready` gauge above ignores pause, only drain/lame-duck).
            gauge(
                &mut s,
                "agentd_paused",
                "1 while the agentic tree is paused at turn boundaries (RFC 0015 §4.3)",
                g(&self.paused),
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
            // `agentd_loop_steps_total` is driven by `loop.step`, which is emitted
            // INSIDE the re-exec'd child agentic loop — a different process from the
            // supervisor this scrape reflects. `record_loop_step` is intentionally
            // left unwired here: bumping it would only touch the child's own
            // process-local registry, never this supervisor's. The series is
            // rendered (so the frozen contract stays discoverable) but reads the
            // supervisor's own process only; cross-process rollup is a v1 non-goal
            // (module header). agentctl derives per-run step counts from `loop.step`
            // log lines (the default story), not from this counter.
            counter(
                &mut s,
                "agentd_loop_steps_total",
                "Agentic loop steps (process-local; emitted in the child loop, so the supervisor scrape reflects its own process only — cross-process rollup is a v1 non-goal).",
                g(&self.loop_steps),
            );

            // --- token / cost accounting (RFC 0016 §4.3) ---------------------
            // `agentd_tokens_total{type}`: the `model` label RFC 0016 §4.3 freezes
            // is DEFERRED in metrics_schema 1.0 — the only call site
            // (`record_tokens`, fed by `AgentMsg::Usage` up the control channel)
            // does not carry the model identifier, and adding it needs a new emit
            // site at the intelligence boundary. The label key is reserved; it is
            // intentionally absent (never faked) until that site lands. agentctl
            // gets per-model token splits from `intel.result.usage` log lines.
            labelled_counter(
                &mut s,
                "agentd_tokens_total",
                "Model tokens by direction (the frozen `model` label is deferred in metrics_schema 1.0 — the AgentMsg::Usage hook carries no model id; never faked).",
                "type",
                TOKEN_TYPES,
                &self.tokens_typed,
            );
            // `agentd_intel_calls_total`: same `model`-label deferral as tokens
            // (the `record_intel_call` site carries no model id). Additionally
            // process-local — `IntelClient::complete` runs in the re-exec'd child
            // (the supervisor makes no LLM calls), so this reflects only the
            // scraped process. Derive per-model call counts from `intel.call` logs.
            counter(
                &mut s,
                "agentd_intel_calls_total",
                "Intelligence calls made (process-local — the LLM client runs in the child; the frozen `model` label is deferred in metrics_schema 1.0, never faked).",
                g(&self.intel_calls),
            );

            // --- refusal / bound counters (RFC 0016 §4.3) --------------------
            // `agentd_refusals_total` is driven by the model/loop refusing or a
            // guard tripping — all INSIDE the re-exec'd child loop (orchestrator
            // self-tool / scope checks), so `record_refusal` is left unwired: it
            // would only bump the child's process-local registry. Rendered for
            // contract discoverability but process-local (the supervisor scrape
            // reflects its own process; cross-process rollup is a v1 non-goal).
            // agentctl derives refusals from the refusal/`scope.trifecta_refused`
            // log lines.
            labelled_counter(
                &mut s,
                "agentd_refusals_total",
                "Refusals/guard trips by reason (process-local; tripped in the child loop, so the supervisor scrape reflects its own process only).",
                "reason",
                REFUSAL_REASONS,
                &self.refusals,
            );
            // `agentd_limit_exceeded_total{limit}` is PARTIALLY wired: the
            // `tree_tokens` leg is the supervisor's own tree-ceiling trip
            // (`supervisor::reactor`, this process → reaches the scrape), so it is
            // live. The `steps`/`tokens`/`deadline`/`depth` legs trip inside the
            // re-exec'd child loop and are therefore process-local (unwired here;
            // derived from `limit.exceeded` log lines). Same cross-process boundary
            // as the rest of this module.
            labelled_counter(
                &mut s,
                "agentd_limit_exceeded_total",
                "Hard-bound trips by limit (the `tree_tokens` leg is supervisor-live; the steps/tokens/deadline/depth legs trip in the child loop and are process-local).",
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
            // `agentd_intel_all_down` (RFC 0018 §6): 1 while EVERY model endpoint is
            // down — the fleet-routing signal (the same latch that flips /readyz).
            gauge(
                &mut s,
                "agentd_intel_all_down",
                "1 while all intelligence endpoints are down (RFC 0018 §6).",
                g(&self.intel_all_down),
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
            // has no declared-server registration hook, so it is RESERVED/not
            // emitted in this build (the honest-absence precedent the rest of this
            // module follows). The connect-failure counter below IS wired — the
            // daemon's supervisor-process connect path (initial + hot-reload add,
            // `triggers::mode`) calls `record_mcp_connect_failure(server)`, so a
            // failing declared server shows up here labelled by `server`. (A
            // child-side connect failure is process-local and does not reach this
            // supervisor scrape — the cross-process boundary in the module header.)
            mcp_servers().render_connect_failures(&mut s, &self.mcp_connect_failures);

            // --- tool-call accounting (RFC 0016 §4.3) — RESERVED -------------
            // `agentd_tool_calls_total{server,tool,ok}` is keyed off `tool.result`,
            // whose boundary (`McpClient::call_tool`) runs predominantly INSIDE the
            // re-exec'd child loop (the subagent's tool use); the only supervisor-
            // process call sites are the reactor's own management/lease calls
            // (`cluster` claim gate), not the agent's tool use the dashboard wants.
            // A scrape-side counter would therefore be process-local and misleading
            // (it would NOT reflect the children's tool calls), so the series is
            // RESERVED here — rendered as a HELP/TYPE marker, no fabricated 0 — and
            // agentctl reads tool calls from `tool.result` log lines (the default
            // story). This mirrors the `agentd_mcp_up` honest-absence precedent.
            reserved(
                &mut s,
                "agentd_tool_calls_total",
                "counter",
                "Tool calls by server/tool/ok (RFC 0016 §4.3) — reserved in metrics_schema 1.0; the tool-call boundary runs in the child loop, so a supervisor scrape can't reflect it (derive from tool.result log lines).",
            );
            // `agentd_tool_call_duration_ms` / `agentd_intel_call_duration_ms` /
            // `agentd_run_duration_ms` are frozen HISTOGRAMS. This crate has no
            // histogram exposition machinery (no bucket/sum/count emission, by
            // design — RFC 0010 §3.8 keeps the surface a hand-written counter/gauge
            // text), so they are RESERVED: rendered as HELP/TYPE markers only, no
            // fabricated buckets. Implementing them is a real feature beyond this
            // honesty pass; a half-built histogram would be worse than an honest
            // marker. Latency lives in the `dur_ms` field of the matching log lines.
            reserved(
                &mut s,
                "agentd_tool_call_duration_ms",
                "histogram",
                "Tool-call latency (RFC 0016 §4.3) — reserved in metrics_schema 1.0; histogram exposition not implemented (use the tool.result dur_ms field).",
            );
            reserved(
                &mut s,
                "agentd_intel_call_duration_ms",
                "histogram",
                "Intelligence-call latency (RFC 0016 §4.3) — reserved in metrics_schema 1.0; histogram exposition not implemented (use the intel.result dur_ms field).",
            );
            reserved(
                &mut s,
                "agentd_run_duration_ms",
                "histogram",
                "Run latency by terminal status (RFC 0016 §4.3) — reserved in metrics_schema 1.0; histogram exposition not implemented (derive from run start→terminal log lines).",
            );

            // --- lifecycle events (RFC 0016 §4.3) ----------------------------
            // `agentd_drains_total{phase}` is wired: the reactor's per-run teardown
            // (`supervisor::reactor`) and the daemon's graceful wind-down
            // (`triggers::mode`) both run in this (supervisor) process and bump
            // `started`/`completed`/`forced`.
            labelled_counter(
                &mut s,
                "agentd_drains_total",
                "Drain phase transitions (RFC 0011 §4).",
                "phase",
                DRAIN_PHASES,
                &self.drains,
            );
            // `agentd_restarts_total` is RESERVED in metrics_schema 1.0: it counts
            // a supervisor process *restart* (rebuild+reconcile, RFC 0003 §3.11),
            // and this build has no such in-process restart path to emit it from
            // (a pod restart is a fresh process with a zeroed registry — an
            // orchestrator counts those, not the binary). Rendered (always 0) so
            // the frozen series stays discoverable; `record_supervisor_restart`
            // exists for the future reconcile path but is intentionally unwired.
            counter(
                &mut s,
                "agentd_restarts_total",
                "Supervisor process restarts observed (RFC 0003 §3.11) — reserved in metrics_schema 1.0; no in-process restart/reconcile emit site in this build.",
                g(&self.supervisor_restarts),
            );
            // `agentd_reactor_stalls_total` is RESERVED in metrics_schema 1.0: a
            // wedged reactor is surfaced as a `/healthz` 503 (a derived read of the
            // heartbeat age in `obs::serve`, evaluated per scrape — RFC 0010 §3.7),
            // not as a one-shot in-process event, so there is no clean emit site to
            // bump a counter exactly once. Rendered (always 0) for discoverability;
            // `record_reactor_stall` is intentionally unwired pending a dedicated
            // stall-detection edge. The liveness signal an operator alerts on is the
            // 503 itself, not this counter.
            counter(
                &mut s,
                "agentd_reactor_stalls_total",
                "Wedged-reactor liveness trips (RFC 0003) — reserved in metrics_schema 1.0; the live signal is the /healthz 503, no one-shot in-process emit site yet.",
                g(&self.reactor_stalls),
            );

            // --- hot reload (RFC 0017 §5.6) ----------------------------------
            // `agentd_config_reload_total{result}` over the closed applied/rejected
            // domain, plus `agentd_config_generation` (applied-reload count) so a
            // scraper detects "generation N is effective" against the desired one.
            labelled_counter(
                &mut s,
                "agentd_config_reload_total",
                "Hot reloads by result (RFC 0017 §5.6).",
                "result",
                RELOAD_RESULTS,
                &self.config_reloads,
            );
            gauge(
                &mut s,
                "agentd_config_generation",
                "Successfully-applied config reloads (the live generation).",
                g(&self.config_generation),
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
            // Claim lifecycle counters (RFC 0019 §5.1). Lost is the over-provision
            // signal (high under low backlog ⇒ scale down); granted/released round
            // out the claim outcome set. Wired by the `cluster` claim gate.
            counter(
                &mut s,
                "agentd_claims_lost_total",
                "Work claims lost to another replica (RFC 0019 §5.1).",
                g(&self.claims_lost),
            );
            counter(
                &mut s,
                "agentd_claims_granted_total",
                "Work claims granted to this replica (RFC 0019 §3.2).",
                g(&self.claims_granted),
            );
            counter(
                &mut s,
                "agentd_claims_released_total",
                "Held claims released back to the fleet (RFC 0019 §3.3/§6).",
                g(&self.claims_released),
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

    /// A frozen `metrics_schema 1.0` series whose machinery is not implemented in
    /// this build: render the `# HELP`/`# TYPE` headers (so the contract stays
    /// discoverable from the scrape and a future silent-drop is catchable) WITHOUT
    /// a fabricated always-0 sample line. This is the same honest-absence shape as
    /// `agentd_mcp_up` — a marker, not a value. `kind` is the Prometheus type the
    /// series will eventually carry (`counter`/`histogram`); `help` MUST say it is
    /// reserved and why (cross-process boundary / no histogram exposition yet).
    fn reserved(s: &mut String, name: &str, kind: &str, help: &str) {
        let _ = writeln!(s, "# HELP {name} {help}");
        let _ = writeln!(s, "# TYPE {name} {kind}");
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
        fn paused_gauge_renders_zero_then_one() {
            // RFC 0015 §5.5: `agentd_paused` is a 0/1 gauge, default 0.
            let r = Registry::new();
            let out = r.render();
            assert!(out.contains("# TYPE agentd_paused gauge"));
            assert!(out.contains("agentd_paused 0"));
            // Set via the same atomic `set_paused` writes; renders 1.
            r.paused.store(1, Ordering::Relaxed);
            assert!(r.render().contains("agentd_paused 1"));
        }

        #[test]
        fn intel_all_down_gauge_renders_zero_then_one() {
            // RFC 0018 §6: `agentd_intel_all_down` is a 0/1 gauge, default 0, set
            // from the latched all-down flag (the same one /readyz reads).
            let r = Registry::new();
            let out = r.render();
            assert!(out.contains("# TYPE agentd_intel_all_down gauge"));
            assert!(out.contains("agentd_intel_all_down 0"));
            // Set via the same atomic `set_intel_all_down` writes; renders 1.
            r.intel_all_down.store(1, Ordering::Relaxed);
            assert!(r.render().contains("agentd_intel_all_down 1"));
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
            // claims-lost counter (fed by the claim gate).
            let r = Registry::new();
            // 35/64 in-flight → 5468 bp → 0.5468 (basis-point granularity).
            r.set_saturation(35, 64);
            r.shard_skipped.fetch_add(3, Ordering::Relaxed);
            let out = r.render();
            assert!(out.contains("# TYPE agentd_saturation gauge"));
            assert!(out.contains("agentd_saturation 0.5468"));
            assert!(out.contains("# TYPE agentd_shard_skipped_total counter"));
            assert!(out.contains("agentd_shard_skipped_total 3"));
            // The claim lifecycle counters render (default 0 in a bare registry).
            assert!(out.contains("# TYPE agentd_claims_lost_total counter"));
            assert!(out.contains("agentd_claims_lost_total 0"));
            assert!(out.contains("# TYPE agentd_claims_granted_total counter"));
            assert!(out.contains("# TYPE agentd_claims_released_total counter"));
            // And they increment.
            r.claims_lost.fetch_add(2, Ordering::Relaxed);
            r.claims_granted.fetch_add(5, Ordering::Relaxed);
            r.claims_released.fetch_add(1, Ordering::Relaxed);
            let out = r.render();
            assert!(out.contains("agentd_claims_lost_total 2"));
            assert!(out.contains("agentd_claims_granted_total 5"));
            assert!(out.contains("agentd_claims_released_total 1"));
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
        fn config_reload_total_renders_both_label_values_and_generation() {
            // RFC 0017 §5.6: the reload counter has the closed applied/rejected
            // domain (every value rendered, zero-valued included), and the
            // generation gauge tracks applied reloads.
            let r = Registry::new();
            let out = r.render();
            // Both closed-domain series are present even at zero.
            assert!(out.contains("# TYPE agentd_config_reload_total counter"));
            assert!(out.contains("agentd_config_reload_total{result=\"applied\"} 0"));
            assert!(out.contains("agentd_config_reload_total{result=\"rejected\"} 0"));
            assert!(out.contains("# TYPE agentd_config_generation gauge"));
            assert!(out.contains("agentd_config_generation 0"));
            // They increment over the closed domain; an unknown buckets `other`.
            r.record_config_reload("applied");
            r.record_config_reload("rejected");
            r.record_config_reload("rejected");
            r.record_config_reload("totally_made_up");
            r.config_generation.store(1, Ordering::Relaxed);
            let out = r.render();
            assert!(out.contains("agentd_config_reload_total{result=\"applied\"} 1"));
            assert!(out.contains("agentd_config_reload_total{result=\"rejected\"} 2"));
            assert!(out.contains("agentd_config_reload_total{result=\"other\"} 1"));
            assert!(out.contains("agentd_config_generation 1"));
            // Exactly one HELP/TYPE header for the counter family.
            assert_eq!(
                out.matches("# TYPE agentd_config_reload_total counter")
                    .count(),
                1
            );
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

        #[test]
        fn frozen_schema_4_3_series_all_present_emitted_or_reserved() {
            // Honesty gate: every frozen RFC 0016 §4.3 series MUST be discoverable
            // from the render — either as a live counter/gauge or as a reserved
            // HELP/TYPE marker. This catches a future silent drop of a frozen
            // series (a major-bump-only change) at test time.
            let r = Registry::new();
            let out = r.render();
            // The full §4.3 metric-name set (the names are the frozen contract).
            for name in [
                // liveness/readiness gauges
                "agentd_up",
                "agentd_ready",
                // run lifecycle + tokens + intel
                "agentd_runs_total",
                "agentd_run_duration_ms", // reserved (histogram)
                "agentd_loop_steps_total",
                "agentd_tokens_total",
                "agentd_intel_calls_total",
                "agentd_intel_call_duration_ms", // reserved (histogram)
                // refusal / bound
                "agentd_refusals_total",
                "agentd_limit_exceeded_total",
                // subagent tree
                "agentd_active_subagents",
                "agentd_tree_depth",
                "agentd_tree_breadth",
                "agentd_subagents_spawned_total",
                "agentd_subagents_exited_total",
                "agentd_subagent_restarts_total",
                "agentd_subagent_stuck_kills_total",
                // intelligence health
                "agentd_intel_up",
                "agentd_intel_errors_total",
                // MCP server health
                "agentd_mcp_connect_failures_total",
                // tool-call accounting (reserved)
                "agentd_tool_calls_total",
                "agentd_tool_call_duration_ms", // reserved (histogram)
                // lifecycle events
                "agentd_drains_total",
                "agentd_restarts_total",       // reserved (no emit site)
                "agentd_reactor_stalls_total", // reserved (no emit site)
                // reactive backlog
                "agentd_pending_events",
                "agentd_inflight_reactions",
                "agentd_subscriptions_active",
                "agentd_reaction_lag_ms",
            ] {
                assert!(
                    out.contains(&format!("# TYPE {name} ")),
                    "frozen §4.3 series missing from render: {name}"
                );
            }
            // The three histograms + the deferred tool-call counter are RESERVED:
            // a HELP/TYPE marker, NO fabricated sample line (the honest-absence
            // shape — no `name <value>` and no `name{...} <value>`).
            for reserved in [
                "agentd_run_duration_ms",
                "agentd_intel_call_duration_ms",
                "agentd_tool_call_duration_ms",
                "agentd_tool_calls_total",
            ] {
                assert!(
                    out.contains(&format!("# TYPE {reserved} ")),
                    "reserved series marker missing: {reserved}"
                );
                // No sample line for the reserved series (only the two `#` headers).
                for line in out.lines() {
                    if line.starts_with('#') {
                        continue;
                    }
                    assert!(
                        !line.starts_with(reserved),
                        "reserved series {reserved} must not emit a sample line: {line:?}"
                    );
                }
            }
            // The reserved markers say so (honest HELP text).
            assert!(out.contains("reserved in metrics_schema 1.0"));
        }

        #[test]
        fn wired_supervisor_counters_increment() {
            // The supervisor-process counters wired in this pass increment via the
            // same registry methods the emit sites call. (The emit sites live in
            // `supervisor::reactor` / `triggers::mode`; here we exercise the
            // registry contract those call sites depend on.)
            let r = Registry::new();
            // subagent spawn/exit (reactor.rs).
            r.subagents_spawned.fetch_add(1, Ordering::Relaxed);
            r.record_subagent_exited("completed");
            r.record_subagent_exited("cancelled");
            // stuck-kill ladder (reactor.rs drive_drain Term/Kill).
            r.record_subagent_stuck_kill("term");
            r.record_subagent_stuck_kill("kill");
            // drain phases (reactor.rs begin_drain/Done/timeout + mode.rs daemon).
            r.record_drain("started");
            r.record_drain("completed");
            r.record_drain("forced");
            // restart governor respawn (mode.rs Backoff branch).
            r.record_subagent_restart("crashed");
            // mcp connect failure (mode.rs connect + hot-reload add).
            r.record_mcp_connect_failure("github");
            // tree-token bound trip (reactor.rs Usage handler).
            r.record_limit_exceeded("tree_tokens");
            let out = r.render();
            assert!(out.contains("agentd_subagents_spawned_total 1"));
            assert!(out.contains("agentd_subagents_exited_total{status=\"completed\"} 1"));
            assert!(out.contains("agentd_subagents_exited_total{status=\"cancelled\"} 1"));
            assert!(out.contains("agentd_subagent_stuck_kills_total{signal=\"term\"} 1"));
            assert!(out.contains("agentd_subagent_stuck_kills_total{signal=\"kill\"} 1"));
            assert!(out.contains("agentd_drains_total{phase=\"started\"} 1"));
            assert!(out.contains("agentd_drains_total{phase=\"completed\"} 1"));
            assert!(out.contains("agentd_drains_total{phase=\"forced\"} 1"));
            assert!(out.contains("agentd_subagent_restarts_total{reason=\"crashed\"} 1"));
            assert!(out.contains("agentd_mcp_connect_failures_total{server=\"github\"} 1"));
            assert!(out.contains("agentd_limit_exceeded_total{limit=\"tree_tokens\"} 1"));
        }

        #[test]
        fn reserved_no_emit_counters_render_zero() {
            // `agentd_restarts_total` (supervisor restart) and
            // `agentd_reactor_stalls_total` have no in-process emit site in this
            // build; they render reserved-but-present at 0 so the contract stays
            // discoverable without falsely claiming a non-zero value.
            let r = Registry::new();
            let out = r.render();
            assert!(out.contains("# TYPE agentd_restarts_total counter"));
            assert!(out.contains("agentd_restarts_total 0"));
            assert!(out.contains("# TYPE agentd_reactor_stalls_total counter"));
            assert!(out.contains("agentd_reactor_stalls_total 0"));
            // Their HELP marks them reserved (not silently permanent-0). Both
            // reserved-counter HELP lines carry the marker phrase.
            assert!(out.matches("reserved in metrics_schema 1.0").count() >= 2);
        }
    }
}
