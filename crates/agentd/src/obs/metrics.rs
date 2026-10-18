// SPDX-License-Identifier: AGPL-3.0-only
//! Process-local counters → Prometheus text. [feature: metrics]
//!
//! Off the default path: the public `record_*` fns are **no-ops unless built
//! with `--features metrics`**, so call sites stay clean and the default build
//! pays nothing (metrics are otherwise derivable from the JSON-lines event
//! stream — that is the default story). With the feature, a tiny dependency-free
//! atomic registry backs an opt-in HTTP `/metrics` scrape surface (`obs::serve`).
//!
//! The surface is the **supervisor's**: the process that serves `/metrics`.
//! Its children — turn workers and subagents, re-exec'd from the same binary —
//! have no surface of their own, so what they count (model calls and errors,
//! endpoint reachability, loop steps) goes up the control channel: a child
//! process calls [`report_upward`] at start, and every frame it sends is
//! preceded by an `AgentMsg::Metrics` carrying what it counted since the last
//! one ([`take_child_metrics`]), which the supervisor adds to its own
//! ([`record_child`]). An instance-tier child is a daemon with a surface of
//! its own, and reports nothing up.
//!
//! ## The metric contract
//!
//! The metric **names** and label **keys** below are a public API that a
//! control plane (agentctl) authors dashboards, alerts and scalers against.
//! Exposition is hand-written Prometheus 0.0.4 text — no `prometheus`/`metrics`
//! crate. Every series rendered with a value is one something writes, at the
//! event its help text names: a series nothing can write is not rendered, and
//! a label value nothing writes is not in its domain. A metric whose meaning
//! changes takes a new name.
//!
//! **Cardinality is binding:** `/metrics` is unauthenticated and may be bound on
//! all interfaces. Labels carry **bounded** values only (`status`, `model`,
//! `type`, `server`, `tool`, `reason`, `limit`, `signal`, `phase`, `ok`);
//! **never** `run_id` / `agent_id` / `agent_path` / `call_id` / a resource URI —
//! those are unbounded and live in logs and traces only. A control plane that
//! needs per-run granularity reads the run report or the event stream, never a
//! metric. This module therefore stores label-bearing series as small
//! **fixed-domain** atomic arrays (the closed label set is known at compile
//! time), so the cardinality is structurally bounded.
//!
//! Telemetry never crashes the agent: every fn here is a plain atomic add/store
//! that cannot fail; `render` only ever reads.

/// A supervised run began (the reactor's run-start path,
/// `runtime::steps::on_start_event`).
pub fn record_run_started() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.runs_started.fetch_add(1, Ordering::Relaxed);
}

/// A workflow run reached its terminal `status`
/// ([`crate::engine::RunStatus::as_str`]): one count in
/// `agent_runs_total{status}`, and one in whichever of the bare
/// `agent_runs_{completed,failed,killed}_total` it falls under. Called once
/// per run, where the run ends; a status outside the run vocabulary counts
/// under `other`.
pub fn record_run(status: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_run(status);
    #[cfg(not(feature = "metrics"))]
    let _ = status;
}

/// A start node fired (`start.fired`): a schedule, a loop, a subscription, a
/// webhook, an A2A message, a stream or a join, an event or a goal had its
/// event accepted into the inbox. The run it asks for may still wait there.
/// A `once` start and a `workflow.run` call are not reactions.
pub fn record_reaction() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.reactions.fetch_add(1, Ordering::Relaxed);
}

/// Tokens reported up by a direct child (`AgentMsg::Usage`).
///
/// Feeds both the bare `agent_tokens_{input,output}_total` and the frozen
/// `agent_tokens_total{type}`. The frozen schema also reserves a `model` label;
/// the `AgentMsg::Usage` control-channel message this call site rides does not
/// carry the model, so the label is left absent rather than faked — populating
/// it needs a call site at the intelligence boundary.
pub fn record_tokens(input: u64, output: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_tokens(input, output);
    #[cfg(not(feature = "metrics"))]
    let _ = (input, output);
}

/// One agent-loop step ran: a model round of a turn worker or a subagent.
/// Counted in the child the loop runs in and carried up
/// ([`take_child_metrics`]); drives `agent_loop_steps_total`.
pub fn record_loop_step() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.loop_steps.fetch_add(1, Ordering::Relaxed);
}

/// A guard refused something, by `reason` (`agent_refusals_total`):
/// `budget` (a token budget refused a turn, a step or a model call), `policy`
/// (a `security.policies` rule refused a tool call), `trifecta` (a subagent's
/// servers formed a lethal trifecta, or a read-back was withheld), and
/// `depth` / `breadth` / `total` / `rate` (a subagent spawn refused by that
/// `limits.subagents` cap). Any other value counts under `other`.
pub fn record_refusal(reason: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_refusal(reason);
    #[cfg(not(feature = "metrics"))]
    let _ = reason;
}

/// A hard bound stopped work, by `limit` (`agent_limit_exceeded_total`):
/// `steps` / `tokens` / `deadline` (see [`record_unit_end`]) and
/// `tokens_lifetime` (the instance's lifetime token budget was spent). Any
/// other value counts under `other`.
pub fn record_limit_exceeded(limit: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_limit_exceeded(limit);
    #[cfg(not(feature = "metrics"))]
    let _ = limit;
}

/// A turn worker or a subagent ended with terminal `status`. One that a bound
/// stopped counts under `agent_limit_exceeded_total`: `exhausted_steps` as
/// `steps`, `exhausted_tokens` as `tokens`, `deadline` as `deadline`.
pub fn record_unit_end(status: &str) {
    if let Some(limit) = bound_of(status) {
        record_limit_exceeded(limit);
    }
}

/// The bound a terminal status says stopped a turn or a subagent.
fn bound_of(status: &str) -> Option<&'static str> {
    match status {
        "exhausted_steps" => Some("steps"),
        "exhausted_tokens" => Some("tokens"),
        "deadline" => Some("deadline"),
        _ => None,
    }
}

/// A subagent was spawned (`subagent.spawn`).
pub fn record_subagent_spawned() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .subagents_spawned
        .fetch_add(1, Ordering::Relaxed);
}

/// A child process exited (`child.exit`). Drives
/// `agent_subagents_exited_total{status}`: `completed` (exit 0), `crashed`
/// (any other exit code), `cancelled` (a signal).
pub fn record_subagent_exited(status: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_subagent_exited(status);
    #[cfg(not(feature = "metrics"))]
    let _ = status;
}

/// A subagent was spawned again (`subagent.respawn`, `instance.respawn`). Drives
/// `agent_subagent_restarts_total{reason}`; `restored` is a subagent a
/// restarted supervisor found unfinished in its store.
pub fn record_subagent_restart(reason: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_subagent_restart(reason);
    #[cfg(not(feature = "metrics"))]
    let _ = reason;
}

/// The supervisor killed a child its liveness check gave up on
/// (`child.unhealthy`: stuck, or past its deadline). Drives
/// `agent_subagent_stuck_kills_total{signal}`; the kill is `SIGKILL` to the
/// child's process group, so `signal` is `kill`.
pub fn record_subagent_stuck_kill(signal: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_subagent_stuck_kill(signal);
    #[cfg(not(feature = "metrics"))]
    let _ = signal;
}

/// A model call began. Drives `agent_intel_calls_total`; a child's are
/// carried up ([`take_child_metrics`]).
pub fn record_intel_call() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.intel_calls.fetch_add(1, Ordering::Relaxed);
}

/// A model call's sweep of the endpoints failed, by reason. Drives
/// `agent_intel_errors_total`. `reason` ∈ `unreachable`\|`auth`\|`timeout`\|
/// `5xx` (an unknown value buckets `other`); a child's are carried up.
pub fn record_intel_error(reason: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_intel_error(reason);
    #[cfg(not(feature = "metrics"))]
    let _ = reason;
}

/// An MCP connect attempt failed for a declared `server` (`mcp.connect.fail`).
/// Drives `agent_mcp_connect_failures_total`. `server` is the declared server
/// name — bounded, because the declared set is small and fixed at config time;
/// an over-capacity name buckets under `other` so the series stays bounded.
pub fn record_mcp_connect_failure(server: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_mcp_connect_failure(server);
    #[cfg(not(feature = "metrics"))]
    let _ = server;
}

/// A drain phase transition. Drives `agent_drains_total`. `phase` ∈
/// `started`\|`completed`\|`forced` (an unknown value buckets `other`).
pub fn record_drain(phase: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_drain(phase);
    #[cfg(not(feature = "metrics"))]
    let _ = phase;
}

/// The supervisor loop went longer than the `/healthz` window without a tick:
/// counted by [`crate::obs::health::tick`] when it ticks again. Drives
/// `agent_reactor_stalls_total`. A loop that never comes back is not counted;
/// the `/healthz` 503 is what reports that one. Nor is a slow boot, before the
/// loop starts ([`crate::obs::health::loop_started`]).
pub fn record_reactor_stall() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.reactor_stalls.fetch_add(1, Ordering::Relaxed);
}

/// What the last model call found of its endpoint (`agent_intel_up`): up,
/// and not every endpoint down. A child's goes up with its next frame.
pub fn set_intel_up(up: bool) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.set_intel_up(up);
    #[cfg(not(feature = "metrics"))]
    let _ = up;
}

/// Point-in-time set of the tree-pause gauge (`agent_paused`) — 1 while the
/// `pause` operator tool has frozen the agentic loops, 0 after `resume`.
/// No-op-safe / metrics-gated, mirroring `set_intel_up`.
pub fn set_paused(on: bool) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.paused.store(u64::from(on), Ordering::Relaxed);
    #[cfg(not(feature = "metrics"))]
    let _ = on;
}

/// Point-in-time set of the subagent-tree shape gauges: subagents not yet
/// ended (`agent_active_subagents`), the deepest of them (`agent_tree_depth`,
/// 1 for one a turn or a step spawned), and the most of them one parent has
/// (`agent_tree_breadth`).
pub fn set_tree_shape(active: u64, depth: u64, breadth: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.set_tree_shape(active, depth, breadth);
    #[cfg(not(feature = "metrics"))]
    let _ = (active, depth, breadth);
}

/// Point-in-time set of the reactive gauges: the work executing now, runs
/// however they started and conversation turns (`agent_inflight_reactions`),
/// the age of the oldest event waiting in the inbox (`agent_reaction_lag_ms`,
/// 0 when none waits), and the MCP resource subscriptions the connections
/// hold (`agent_subscriptions_active{kind="resource"}`).
pub fn set_reactive_backlog(inflight: u64, lag_ms: u64, resource_subscriptions: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.set_reactive_backlog(inflight, lag_ms, resource_subscriptions);
    #[cfg(not(feature = "metrics"))]
    let _ = (inflight, lag_ms, resource_subscriptions);
}

/// A config hot reload reached a terminal disposition. Drives
/// `agent_config_reload_total{result}` with the closed domain
/// `applied`\|`rejected` (an unknown value buckets `other`). A `rejected` reload
/// is a clean no-op (the running config is unchanged); `applied` bumps the
/// generation gauge via [`set_config_generation`].
pub fn record_config_reload(result: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_config_reload(result);
    #[cfg(not(feature = "metrics"))]
    let _ = result;
}

/// A turn worker was started, by `kind` (`agent_turns_total{kind}`): `root`
/// (a conversation turn), `step` (a workflow `agent`/`think`/`decide` step),
/// `think` and `decide` (those tools), `preflight`, `compaction`.
pub fn record_turn(kind: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_turn(kind);
    #[cfg(not(feature = "metrics"))]
    let _ = kind;
}

/// An attempt at a workflow step ended (`agent_steps_total{status}`):
/// `done`, `failed`, or `other` (timed out). A step that never ran is not
/// one.
pub fn record_step(status: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_step(status);
    #[cfg(not(feature = "metrics"))]
    let _ = status;
}

/// A remote-store op completed (`agent_store_ops_total{result}` + latency sum).
pub fn record_store_op(result: &str, latency_ms: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_store_op(result, latency_ms);
    #[cfg(not(feature = "metrics"))]
    let _ = (result, latency_ms);
}

/// Point-in-time set of the durable inbox backlog (`agent_inbox_pending`).
pub fn set_inbox_pending(n: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.set_inbox_pending(n);
    #[cfg(not(feature = "metrics"))]
    let _ = n;
}

/// Point-in-time set of the largest live context's token estimate
/// (`agent_context_tokens`).
pub fn set_context_tokens(n: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.set_context_tokens(n);
    #[cfg(not(feature = "metrics"))]
    let _ = n;
}

/// Point-in-time set of the config-generation gauge
/// (`agent_config_generation`): the count of successfully-applied reloads, so a
/// scraper can detect "this instance has picked up generation N" against
/// agentctl's desired generation. Monotonic in practice — the reload loop only
/// ever increments it.
pub fn set_config_generation(generation: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .config_generation
        .store(generation, Ordering::Relaxed);
    #[cfg(not(feature = "metrics"))]
    let _ = generation;
}

/// Point-in-time set of the lifetime-budget balance gauge
/// (`agent_budget_tokens_remaining`): tokens left under
/// `intelligence.budget.lifetime_tokens`. `None` = no lifetime budget — the
/// gauge is then not emitted, since a 0 would read as a spent one.
pub fn set_budget_tokens_remaining(remaining: Option<u64>) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY
        .budget_tokens_remaining
        .store(remaining.unwrap_or(u64::MAX), Ordering::Relaxed);
    #[cfg(not(feature = "metrics"))]
    let _ = remaining;
}

/// Point-in-time set of the resource-pressure gauges (`agent_pressure_level`,
/// `agent_disk_free_bytes`): the shed/drain state the admission gates act on
/// and the disk headroom that (usually) drives it. `disk_free: None` = no file
/// store on this instance — the byte gauge is then not emitted at all, because
/// exporting the supervisor's local free space when durability lives elsewhere
/// would invite alerts on the wrong disk.
pub fn set_pressure(level: u64, disk_free: Option<u64>) {
    #[cfg(feature = "metrics")]
    {
        imp::REGISTRY.pressure_level.store(level, Ordering::Relaxed);
        imp::REGISTRY
            .disk_free_bytes
            .store(disk_free.unwrap_or(u64::MAX), Ordering::Relaxed);
    }
    #[cfg(not(feature = "metrics"))]
    let _ = (level, disk_free);
}

/// Point-in-time set of the work-in-progress gauges (`agent_runs_active`,
/// `agent_turns_queued`): non-terminal workflow runs, and conversation turns
/// waiting for a dispatch slot (parallelism, pause, drain, or shed — the gauge
/// does not say which; the event stream does).
pub fn set_work_backlog(runs_active: u64, turns_queued: u64) {
    #[cfg(feature = "metrics")]
    {
        imp::REGISTRY
            .runs_active
            .store(runs_active, Ordering::Relaxed);
        imp::REGISTRY
            .turns_queued
            .store(turns_queued, Ordering::Relaxed);
    }
    #[cfg(not(feature = "metrics"))]
    let _ = (runs_active, turns_queued);
}

/// Point-in-time `agent_stream_lag{stream,consumer}` for every consumer of
/// one start `kind` (`stream`, `correlate`): how many events on its stream it
/// has not consumed yet. Each call REPLACES that kind's set, so a consumer a
/// reload removed stops being exported instead of freezing at its last value.
/// `consumer` is `<workflow>/<node>` — both declared names, so the label set
/// is bounded by the configuration; past [`STREAM_LAG_SLOTS`] consumers of a
/// kind the rest fold into one `other` series carrying their largest lag.
pub fn set_stream_lag(kind: &'static str, lags: Vec<(String, String, u64)>) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.set_stream_lag(kind, lags);
    #[cfg(not(feature = "metrics"))]
    let _ = (kind, lags);
}

/// How many `{stream,consumer}` series one start kind exports before the rest
/// fold into `other`.
pub const STREAM_LAG_SLOTS: usize = 32;

/// How many series one MCP Events family exports before the rest fold into
/// one whose labels all read `other`.
pub const EVENT_SERIES_SLOTS: usize = 512;

/// What became of one MCP Events occurrence
/// (`agent_mcp_events_total{server,event,mode,outcome}`). `outcome` ∈
/// `appended`\|`duplicate`\|`rejected`\|`schema_mismatch`\|`refused_pressure`\|
/// `followed` (taken for the instruction to read again, kept in no
/// stream)\|`unauthenticated` (a webhook delivery that failed its check); an
/// unknown value buckets `other`. `server` and `event` come from the
/// operator's bindings, at most 256 of them.
pub fn record_mcp_event(server: &str, event: &str, mode: &str, outcome: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_add(
        "agent_mcp_events_total",
        vec![
            server.into(),
            event.into(),
            mode.into(),
            imp::event_outcome(outcome).into(),
        ],
        1,
    );
    #[cfg(not(feature = "metrics"))]
    let _ = (server, event, mode, outcome);
}

/// One `events/*` request agentd sent
/// (`agent_mcp_events_requests_total{server,method,outcome}`): `outcome` ∈
/// `ok`\|`refused` (a JSON-RPC error answer)\|`failed` (no usable answer).
pub fn record_mcp_events_request(server: &str, method: &str, outcome: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_add(
        "agent_mcp_events_requests_total",
        vec![server.into(), method.into(), outcome.into()],
        1,
    );
    #[cfg(not(feature = "metrics"))]
    let _ = (server, method, outcome);
}

/// A gap a binding saw (`agent_mcp_event_gaps_total{server,event,cause}`):
/// `cause` ∈ `truncated`\|`no_replay`.
pub fn record_mcp_event_gap(server: &str, event: &str, cause: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_add(
        "agent_mcp_event_gaps_total",
        vec![server.into(), event.into(), cause.into()],
        1,
    );
    #[cfg(not(feature = "metrics"))]
    let _ = (server, event, cause);
}

/// Dedup entries a binding dropped to stay under its cap
/// (`agent_mcp_events_dedup_evicted_total{server,event}`).
pub fn record_mcp_events_dedup_evicted(server: &str, event: &str, n: u64) {
    #[cfg(feature = "metrics")]
    if n > 0 {
        imp::REGISTRY.event_add(
            "agent_mcp_events_dedup_evicted_total",
            vec![server.into(), event.into()],
            n,
        );
    }
    #[cfg(not(feature = "metrics"))]
    let _ = (server, event, n);
}

/// Append time minus the occurrence's server-reported timestamp, clamped at 0
/// (`agent_mcp_event_lag_ms{server,event}`): server-reported, so only as true
/// as the server's clock.
pub fn set_mcp_event_lag(server: &str, event: &str, ms: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_set(
        "agent_mcp_event_lag_ms",
        vec![server.into(), event.into()],
        ms,
    );
    #[cfg(not(feature = "metrics"))]
    let _ = (server, event, ms);
}

/// One call agentd made to a Company (RFC 0047 §7,
/// `agent_company_requests_total{company,interface,outcome}`): `interface` ∈
/// `mcp`; `outcome` ∈ `ok`\|`error` (the Company's own answer, a tool's
/// `isError` or a refusal it sent)\|`unsent` (refused before it was sent)\|
/// `unknown` (it may have run: no answer, or a `5xx`, after it was sent). An
/// unknown value buckets `other`. `company` is a company entry's name.
pub fn record_company_request(company: &str, interface: &str, outcome: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_add(
        "agent_company_requests_total",
        vec![
            company.into(),
            interface.into(),
            imp::closed_label(outcome, &["ok", "error", "unsent", "unknown"]).into(),
        ],
        1,
    );
    #[cfg(not(feature = "metrics"))]
    let _ = (company, interface, outcome);
}

/// One request agentd sent a Company's token endpoint
/// (`agent_company_token_requests_total{company,grant,outcome}`): `grant` ∈
/// `start`\|`renew`\|`bearer`; `outcome` ∈ `ok`\|`refused` (an OAuth
/// error)\|`failed` (no usable answer).
pub fn record_company_token_request(company: &str, grant: &str, outcome: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_add(
        "agent_company_token_requests_total",
        vec![
            company.into(),
            imp::closed_label(grant, &["start", "renew", "bearer"]).into(),
            imp::closed_label(outcome, &["ok", "refused", "failed"]).into(),
        ],
        1,
    );
    #[cfg(not(feature = "metrics"))]
    let _ = (company, grant, outcome);
}

/// Whether agentd's Session at a Company is signed in
/// (`agent_company_signed_in{company}`, 0 or 1).
pub fn set_company_signed_in(company: &str, signed_in: bool) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_set(
        "agent_company_signed_in",
        vec![company.into()],
        u64::from(signed_in),
    );
    #[cfg(not(feature = "metrics"))]
    let _ = (company, signed_in);
}

/// The instant before which nothing is sent to a Company, in seconds since
/// the Unix epoch (`agent_company_rate_limited_until_seconds{company}`); a
/// past instant holds nothing back.
pub fn set_company_rate_limited_until(company: &str, unix_s: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_set(
        "agent_company_rate_limited_until_seconds",
        vec![company.into()],
        unix_s,
    );
    #[cfg(not(feature = "metrics"))]
    let _ = (company, unix_s);
}

/// One User gate a Company call opened, and how it ended
/// (`agent_company_gates_total{company,kind,answer}`): `kind` ∈
/// `cross_company`; `answer` ∈ `approve`\|`deny`\|`unanswered` (it timed
/// out or was cancelled)\|`unreachable` (nobody could be asked, so the call
/// was refused).
pub fn record_company_gate(company: &str, kind: &str, answer: &str) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_add(
        "agent_company_gates_total",
        vec![
            company.into(),
            imp::closed_label(kind, &["cross_company"]).into(),
            imp::closed_label(answer, &["approve", "deny", "unanswered", "unreachable"]).into(),
        ],
        1,
    );
    #[cfg(not(feature = "metrics"))]
    let _ = (company, kind, answer);
}

/// The active MCP Events subscriptions, by delivery mode — the `kind="event"`
/// series of `agent_subscriptions_active{kind,mode}`, replaced whole.
pub fn set_event_subscriptions(by_mode: Vec<(String, u64)>) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.event_replace(
        "agent_subscriptions_active",
        by_mode
            .into_iter()
            .map(|(mode, n)| (vec!["event".to_string(), mode], n))
            .collect(),
    );
    #[cfg(not(feature = "metrics"))]
    let _ = by_mode;
}

/// What a child process counted since its last frame, carried up the control
/// channel as `AgentMsg::Metrics` so the supervisor's surface counts the work
/// its children do. Each count is a delta; `intel_up` is what the child's
/// last model call found, when it made one since.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildMetrics {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub intel_calls: u64,
    /// By `agent_intel_errors_total` reason.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub intel_errors: std::collections::BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intel_up: Option<bool>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub loop_steps: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Whether this process is a child that reports its counts upward.
#[cfg(feature = "metrics")]
static REPORTS_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark this process a child whose counts go up to its supervisor
/// ([`take_child_metrics`]). Called once, as the child starts.
pub fn report_upward() {
    #[cfg(feature = "metrics")]
    REPORTS_UP.store(true, Ordering::Relaxed);
}

/// What this process counted since the last call, taken: the counts it
/// carries start again from zero. `None` when nothing was counted, and in a
/// process that does not report upward — the supervisor keeps its own.
pub fn take_child_metrics() -> Option<ChildMetrics> {
    #[cfg(feature = "metrics")]
    if REPORTS_UP.load(Ordering::Relaxed) {
        return imp::REGISTRY.take_child();
    }
    None
}

/// Add what a child reported (`AgentMsg::Metrics`) to this process's counts.
pub fn record_child(m: &ChildMetrics) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_child(m);
    #[cfg(not(feature = "metrics"))]
    let _ = m;
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
    use std::fmt::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) static REGISTRY: Registry = Registry::new();

    // --- closed label domains -----------------------------------------------
    // Each is a fixed array of `(label_value, AtomicU64)`; an out-of-vocabulary
    // value lands in the trailing `other` slot so the series stays bounded. The
    // arrays ARE the cardinality bound — there is no map, no allocation, no
    // unbounded label key path.

    /// `agent_runs_total{status}` label domain: the terminal run statuses
    /// (verbatim from `RunStatus::as_str`), plus `other`.
    const RUN_STATUS: &[&str] = &[
        "completed",
        "failed",
        "refused",
        "cancelled",
        "stalled",
        "other",
    ];

    /// `agent_subagents_exited_total{status}` label domain: how a child
    /// process ended — exit 0, another exit code, a signal — plus `other`.
    const EXIT_STATUS: &[&str] = &["completed", "crashed", "cancelled", "other"];

    /// `agent_refusals_total{reason}` label domain (see `record_refusal`).
    const REFUSAL_REASONS: &[&str] = &[
        "budget", "policy", "trifecta", "depth", "breadth", "total", "rate", "other",
    ];

    /// `agent_limit_exceeded_total{limit}` label domain (see
    /// `record_limit_exceeded`).
    const LIMIT_LABELS: &[&str] = &["steps", "tokens", "deadline", "tokens_lifetime", "other"];

    /// `agent_subagent_restarts_total{reason}` label domain.
    const RESTART_REASONS: &[&str] = &["restored", "other"];

    /// `agent_subagent_stuck_kills_total{signal}` label domain.
    const SIGNAL_LABELS: &[&str] = &["kill", "other"];

    /// `agent_intel_errors_total{reason}` label domain.
    const INTEL_ERROR_REASONS: &[&str] = &["unreachable", "auth", "timeout", "5xx", "other"];

    /// `agent_drains_total{phase}` label domain.
    const DRAIN_PHASES: &[&str] = &["started", "completed", "forced", "other"];

    /// `agent_tokens_total{type}` direction label domain.
    const TOKEN_TYPES: &[&str] = &["in", "out"];

    /// `agent_config_reload_total{result}` label domain. A hot reload either
    /// `applied` (the reloadable diff took effect) or was `rejected` (invalid /
    /// restart-only / inconsistent → a clean no-op); `other` is the catch-all
    /// that keeps the series bounded.
    const RELOAD_RESULTS: &[&str] = &["applied", "rejected", "other"];

    /// `agent_turns_total{kind}` label domain: what a turn worker was started
    /// for (see `record_turn`), plus `other`.
    const TURN_KINDS: &[&str] = &[
        "root",
        "step",
        "think",
        "decide",
        "preflight",
        "compaction",
        "other",
    ];

    /// `agent_mcp_events_total{outcome}` label domain: what became of an MCP
    /// Events occurrence ([`super::record_mcp_event`]).
    const EVENT_OUTCOMES: &[&str] = &[
        "appended",
        "duplicate",
        "rejected",
        "schema_mismatch",
        "refused_pressure",
        "followed",
        "unauthenticated",
    ];

    /// `value` when it is one of `domain`, else `other`: a label from a
    /// closed set stays closed whatever a caller passes.
    pub(super) fn closed_label<'a>(value: &'a str, domain: &[&str]) -> &'a str {
        if domain.contains(&value) {
            value
        } else {
            "other"
        }
    }

    /// An MCP Events outcome as its label: itself when in the domain, else
    /// `other`.
    pub(super) fn event_outcome(outcome: &str) -> &str {
        if EVENT_OUTCOMES.contains(&outcome) {
            outcome
        } else {
            "other"
        }
    }

    /// `agent_steps_total{status}` label domain: how a step that ran ended —
    /// `done` / `failed` — plus `other` (timed out). A step that never ran
    /// (skipped, or on a branch not taken) is not counted.
    const STEP_STATUS: &[&str] = &["done", "failed", "other"];

    /// `agent_store_ops_total{result}` label domain: a remote-store op outcome —
    /// `ok` / `conflict` (a CAS mismatch) / `error` — plus `other`.
    const STORE_RESULTS: &[&str] = &["ok", "conflict", "error", "other"];

    /// A fixed-domain labelled counter family: one atomic per known label value.
    /// `N` matches the backing domain slice length; the trailing slot is the
    /// `other` catch-all that keeps the cardinality bounded.
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
            self.add(domain, value, 1);
        }

        /// Add `n` to the slot for `value`, as [`Self::inc`] does one.
        fn add(&self, domain: &[&str], value: &str, n: u64) {
            let idx = domain.iter().position(|&l| l == value).unwrap_or(N - 1);
            self.slots[idx].fetch_add(n, Ordering::Relaxed);
        }
    }

    pub(super) struct Registry {
        // --- bare, unlabelled series (kept alongside the labelled families) --
        pub(super) runs_started: AtomicU64,
        runs_completed: AtomicU64,
        runs_failed: AtomicU64,
        runs_killed: AtomicU64,
        pub(super) reactions: AtomicU64,
        tokens_input: AtomicU64,
        tokens_output: AtomicU64,

        // --- run lifecycle & terminal-status ---------------------------------
        runs_total: LabelCounter<{ RUN_STATUS.len() }>,
        pub(super) loop_steps: AtomicU64,

        // --- refusal / bound counters ----------------------------------------
        refusals: LabelCounter<{ REFUSAL_REASONS.len() }>,
        limit_exceeded: LabelCounter<{ LIMIT_LABELS.len() }>,

        // --- subagent-tree gauges + counters ---------------------------------
        active_subagents: AtomicU64,
        tree_depth: AtomicU64,
        tree_breadth: AtomicU64,
        pub(super) subagents_spawned: AtomicU64,
        subagents_exited: LabelCounter<{ EXIT_STATUS.len() }>,
        subagent_restarts: LabelCounter<{ RESTART_REASONS.len() }>,
        subagent_stuck_kills: LabelCounter<{ SIGNAL_LABELS.len() }>,

        // --- intelligence health ---------------------------------------------
        // `agent_intel_all_down` has no field: it is rendered from the
        // `signals` latch `/readyz` reads, so the two never disagree.
        pub(super) intel_calls: AtomicU64,
        pub(super) intel_up: AtomicU64,
        /// What `set_intel_up` stored that no child report carried up yet:
        /// 0 nothing, 1 down, 2 up.
        intel_up_unsent: AtomicU64,
        intel_errors: LabelCounter<{ INTEL_ERROR_REASONS.len() }>,

        // --- tree-pause gauge (0/1) ------------------------------------------
        // Set by the `pause`/`resume` operator tools; 1 while the tree is paused.
        pub(super) paused: AtomicU64,

        // --- frozen: MCP server health ---------------------------------------
        mcp_connect_failures: LabelCounter<{ MCP_SERVER_SLOTS }>,

        // --- frozen: lifecycle events ----------------------------------------
        drains: LabelCounter<{ DRAIN_PHASES.len() }>,
        pub(super) reactor_stalls: AtomicU64,

        // --- frozen: token accounting (typed) --------------------------------
        tokens_typed: LabelCounter<{ TOKEN_TYPES.len() }>,

        // --- reactive gauges (the autoscaler's signal set) -------------------
        inflight_reactions: AtomicU64,
        reaction_lag_ms: AtomicU64,
        resource_subscriptions: AtomicU64,

        // --- hot-reload outcome counter + generation gauge -------------------
        config_reloads: LabelCounter<{ RELOAD_RESULTS.len() }>,
        pub(super) config_generation: AtomicU64,

        // --- per-instance lifetime-budget balance gauge ----------------------
        /// `u64::MAX` = no lifetime budget; then the gauge is not emitted.
        pub(super) budget_tokens_remaining: AtomicU64,
        pub(super) pressure_level: AtomicU64,
        /// `u64::MAX` = unknown/no file store; then the gauge is not emitted.
        pub(super) disk_free_bytes: AtomicU64,
        pub(super) runs_active: AtomicU64,
        pub(super) turns_queued: AtomicU64,

        // --- runtime series: turns, workflow steps, store, inbox, context ----
        turns_total: LabelCounter<{ TURN_KINDS.len() }>,
        steps_total: LabelCounter<{ STEP_STATUS.len() }>,
        store_ops: LabelCounter<{ STORE_RESULTS.len() }>,
        store_latency_ms_sum: AtomicU64,
        pub(super) inbox_pending: AtomicU64,
        pub(super) context_tokens: AtomicU64,

        // --- stream consumer lag, by start kind ------------------------------
        // Replaced whole per kind on every consumer pass; see `set_stream_lag`.
        stream_lag: std::sync::Mutex<std::collections::BTreeMap<&'static str, StreamLag>>,

        // --- MCP Events, by binding ------------------------------------------
        // Each family by name, each series by its label values. Bounded:
        // `server` and `event` come from the operator's bindings (at most 256),
        // the rest from closed domains, and a family past
        // `EVENT_SERIES_SLOTS` folds the rest into one `other` series.
        events: std::sync::Mutex<EventFamilies>,
    }

    type EventFamilies =
        std::collections::BTreeMap<&'static str, std::collections::BTreeMap<Vec<String>, u64>>;

    /// The labeled families — MCP Events' and the Companies' — name, type,
    /// help, label names.
    const EVENT_FAMILIES: &[(&str, &str, &str, &[&str])] = &[
        (
            "agent_mcp_events_total",
            "counter",
            "MCP Events occurrences, by what became of them.",
            &["server", "event", "mode", "outcome"],
        ),
        (
            "agent_mcp_events_requests_total",
            "counter",
            "events/* requests agentd sent, by outcome.",
            &["server", "method", "outcome"],
        ),
        (
            "agent_mcp_event_gaps_total",
            "counter",
            "Gaps a binding saw: a truncated answer, or a re-arm with no replay.",
            &["server", "event", "cause"],
        ),
        (
            "agent_mcp_events_dedup_evicted_total",
            "counter",
            "Dedup entries a binding dropped to stay under its cap.",
            &["server", "event"],
        ),
        (
            "agent_mcp_event_lag_ms",
            "gauge",
            "Append time minus the occurrence's server-reported timestamp (ms).",
            &["server", "event"],
        ),
        // --- Companies (RFC 0047 §7): `company` is a company entry's name,
        // and no label carries a User ID, a session or a conversation.
        (
            "agent_company_requests_total",
            "counter",
            "Calls agentd made to a Company, by interface and outcome.",
            &["company", "interface", "outcome"],
        ),
        (
            "agent_company_token_requests_total",
            "counter",
            "Requests agentd sent a Company's token endpoint, by grant and outcome.",
            &["company", "grant", "outcome"],
        ),
        (
            "agent_company_signed_in",
            "gauge",
            "Whether agentd's Session at the Company is signed in (0 or 1).",
            &["company"],
        ),
        (
            "agent_company_rate_limited_until_seconds",
            "gauge",
            "The instant before which nothing is sent to the Company (Unix seconds).",
            &["company"],
        ),
        (
            "agent_company_gates_total",
            "counter",
            "User gates a Company call opened, by kind and answer.",
            &["company", "kind", "answer"],
        ),
    ];

    /// A Prometheus label value, escaped.
    fn escape_label(v: &str) -> String {
        v.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }

    /// One series line: `name{l1="v1",…} value`.
    fn series_line(s: &mut String, name: &str, labels: &[&str], values: &[String], value: u64) {
        let pairs: Vec<String> = labels
            .iter()
            .zip(values)
            .map(|(l, v)| format!("{l}=\"{}\"", escape_label(v)))
            .collect();
        let _ = writeln!(s, "{name}{{{}}} {value}", pairs.join(","));
    }

    /// One start kind's consumers as of their last pass: the named series, and
    /// the largest lag among those past the slot limit (`None` when none were).
    #[derive(Default)]
    struct StreamLag {
        named: Vec<(String, String, u64)>,
        other: Option<u64>,
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
                intel_up_unsent: AtomicU64::new(0),
                intel_errors: LabelCounter::new(),
                paused: AtomicU64::new(0),
                mcp_connect_failures: LabelCounter::new(),
                drains: LabelCounter::new(),
                reactor_stalls: AtomicU64::new(0),
                tokens_typed: LabelCounter::new(),
                inflight_reactions: AtomicU64::new(0),
                reaction_lag_ms: AtomicU64::new(0),
                resource_subscriptions: AtomicU64::new(0),
                config_reloads: LabelCounter::new(),
                config_generation: AtomicU64::new(0),
                budget_tokens_remaining: AtomicU64::new(u64::MAX),
                pressure_level: AtomicU64::new(0),
                disk_free_bytes: AtomicU64::new(u64::MAX),
                runs_active: AtomicU64::new(0),
                turns_queued: AtomicU64::new(0),
                turns_total: LabelCounter::new(),
                steps_total: LabelCounter::new(),
                store_ops: LabelCounter::new(),
                store_latency_ms_sum: AtomicU64::new(0),
                inbox_pending: AtomicU64::new(0),
                context_tokens: AtomicU64::new(0),
                stream_lag: std::sync::Mutex::new(std::collections::BTreeMap::new()),
                events: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            }
        }

        /// The series of `family` a write lands in: its own, or `other` once
        /// the family holds its slots.
        fn event_series(
            fam: &std::collections::BTreeMap<Vec<String>, u64>,
            labels: Vec<String>,
        ) -> Vec<String> {
            if fam.len() >= super::EVENT_SERIES_SLOTS && !fam.contains_key(&labels) {
                vec!["other".to_string(); labels.len()]
            } else {
                labels
            }
        }

        pub(super) fn event_add(&self, family: &'static str, labels: Vec<String>, by: u64) {
            let mut t = self.events.lock().unwrap_or_else(|p| p.into_inner());
            let fam = t.entry(family).or_default();
            let key = Self::event_series(fam, labels);
            *fam.entry(key).or_default() += by;
        }

        pub(super) fn event_set(&self, family: &'static str, labels: Vec<String>, value: u64) {
            let mut t = self.events.lock().unwrap_or_else(|p| p.into_inner());
            let fam = t.entry(family).or_default();
            let key = Self::event_series(fam, labels);
            fam.insert(key, value);
        }

        pub(super) fn event_replace(&self, family: &'static str, series: Vec<(Vec<String>, u64)>) {
            let mut t = self.events.lock().unwrap_or_else(|p| p.into_inner());
            t.insert(family, series.into_iter().collect());
        }

        /// The series of `family`, as written.
        fn event_family(&self, family: &str) -> Vec<(Vec<String>, u64)> {
            let t = self.events.lock().unwrap_or_else(|p| p.into_inner());
            t.get(family)
                .map(|f| f.iter().map(|(k, v)| (k.clone(), *v)).collect())
                .unwrap_or_default()
        }

        pub(super) fn set_stream_lag(
            &self,
            kind: &'static str,
            mut lags: Vec<(String, String, u64)>,
        ) {
            let other = lags
                .get(super::STREAM_LAG_SLOTS..)
                .and_then(|rest| rest.iter().map(|(_, _, lag)| *lag).max());
            lags.truncate(super::STREAM_LAG_SLOTS);
            // Poisoning is ignored: telemetry must never crash the agent.
            let mut table = match self.stream_lag.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            table.insert(kind, StreamLag { named: lags, other });
        }

        pub(super) fn record_run(&self, status: &str) {
            let bare = match status {
                "completed" => &self.runs_completed,
                "cancelled" => &self.runs_killed,
                _ => &self.runs_failed,
            };
            bare.fetch_add(1, Ordering::Relaxed);
            self.runs_total.inc(RUN_STATUS, status);
        }

        pub(super) fn record_tokens(&self, input: u64, output: u64) {
            self.tokens_input.fetch_add(input, Ordering::Relaxed);
            self.tokens_output.fetch_add(output, Ordering::Relaxed);
            // Frozen `agent_tokens_total{type}` (the `model` label is not
            // available at the `AgentMsg::Usage` hook, so it stays absent).
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
            self.subagents_exited.inc(EXIT_STATUS, status);
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

        pub(super) fn set_intel_up(&self, up: bool) {
            self.intel_up.store(u64::from(up), Ordering::Relaxed);
            self.intel_up_unsent
                .store(if up { 2 } else { 1 }, Ordering::Relaxed);
        }

        /// Take what a child counted since the last take: each count swapped
        /// for zero, so a count lands in exactly one report however the
        /// frames that carry them interleave.
        pub(super) fn take_child(&self) -> Option<super::ChildMetrics> {
            let mut m = super::ChildMetrics {
                intel_calls: self.intel_calls.swap(0, Ordering::Relaxed),
                intel_up: match self.intel_up_unsent.swap(0, Ordering::Relaxed) {
                    1 => Some(false),
                    2 => Some(true),
                    _ => None,
                },
                loop_steps: self.loop_steps.swap(0, Ordering::Relaxed),
                ..Default::default()
            };
            for (slot, reason) in self.intel_errors.slots.iter().zip(INTEL_ERROR_REASONS) {
                let n = slot.swap(0, Ordering::Relaxed);
                if n > 0 {
                    m.intel_errors.insert(reason.to_string(), n);
                }
            }
            (m != super::ChildMetrics::default()).then_some(m)
        }

        pub(super) fn record_child(&self, m: &super::ChildMetrics) {
            self.intel_calls.fetch_add(m.intel_calls, Ordering::Relaxed);
            self.loop_steps.fetch_add(m.loop_steps, Ordering::Relaxed);
            for (reason, n) in &m.intel_errors {
                self.intel_errors.add(INTEL_ERROR_REASONS, reason, *n);
            }
            if let Some(up) = m.intel_up {
                self.intel_up.store(u64::from(up), Ordering::Relaxed);
            }
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

        pub(super) fn record_turn(&self, kind: &str) {
            self.turns_total.inc(TURN_KINDS, kind);
        }

        pub(super) fn record_step(&self, status: &str) {
            self.steps_total.inc(STEP_STATUS, status);
        }

        pub(super) fn record_store_op(&self, result: &str, latency_ms: u64) {
            self.store_ops.inc(STORE_RESULTS, result);
            self.store_latency_ms_sum
                .fetch_add(latency_ms, Ordering::Relaxed);
        }

        pub(super) fn set_inbox_pending(&self, n: u64) {
            self.inbox_pending.store(n, Ordering::Relaxed);
        }

        pub(super) fn set_context_tokens(&self, n: u64) {
            self.context_tokens.store(n, Ordering::Relaxed);
        }

        pub(super) fn set_tree_shape(&self, active: u64, depth: u64, breadth: u64) {
            self.active_subagents.store(active, Ordering::Relaxed);
            self.tree_depth.store(depth, Ordering::Relaxed);
            self.tree_breadth.store(breadth, Ordering::Relaxed);
        }

        pub(super) fn set_reactive_backlog(&self, inflight: u64, lag_ms: u64, resources: u64) {
            self.inflight_reactions.store(inflight, Ordering::Relaxed);
            self.reaction_lag_ms.store(lag_ms, Ordering::Relaxed);
            self.resource_subscriptions
                .store(resources, Ordering::Relaxed);
        }

        pub(super) fn render(&self) -> String {
            let mut s = String::new();
            let g = |a: &AtomicU64| a.load(Ordering::Relaxed);

            // --- liveness / readiness gauges ---------------------------------
            // `agent_up` is always 1 while we can render. `agent_ready` reads the
            // same process-wide drain/lame-duck state `/readyz` reports —
            // read-only, no extra call site.
            gauge(&mut s, "agent_up", "1 while the process is alive", 1);
            // `agent_ready` mirrors `/readyz` exactly: NotReady when draining,
            // lame-ducked, OR all intelligence endpoints are down — the same three
            // conditions the readiness probe consults.
            let ready = u64::from(
                !crate::signals::draining()
                    && !crate::signals::lame_duck()
                    && !crate::signals::intel_all_down(),
            );
            gauge(
                &mut s,
                "agent_ready",
                "1 when ready to accept work (not draining / lame-ducked / intel-all-down)",
                ready,
            );
            // `agent_paused`: 1 while the tree is paused at turn boundaries.
            // Pause is NOT readiness — a paused instance can still be ready (the
            // `ready` gauge above ignores pause, only drain/lame-duck).
            gauge(
                &mut s,
                "agent_paused",
                "1 while the agentic tree is paused at turn boundaries",
                g(&self.paused),
            );

            // --- run lifecycle & terminal-status -----------------------------
            labelled_counter(
                &mut s,
                "agent_runs_total",
                "Workflow runs by terminal status.",
                "status",
                RUN_STATUS,
                &self.runs_total,
            );
            // Counted where the loop runs — a turn worker or a subagent, never
            // this process — and carried up with the child's frames.
            counter(
                &mut s,
                "agent_loop_steps_total",
                "Agent loop steps: model rounds of turn workers and subagents.",
                g(&self.loop_steps),
            );

            // --- token / cost accounting -------------------------------------
            // `agent_tokens_total{type}`: the frozen `model` label is DEFERRED —
            // the only call site (`record_tokens`, fed by
            // `AgentMsg::Usage` up the control channel) does not carry the model
            // identifier, and adding it needs an emit site at the intelligence
            // boundary. The label key stays reserved and absent rather than
            // faked. agentctl gets per-model token splits from
            // `intel.result.usage` log lines.
            labelled_counter(
                &mut s,
                "agent_tokens_total",
                "Model tokens by direction (the frozen `model` label is deferred — the AgentMsg::Usage hook carries no model id; never faked).",
                "type",
                TOKEN_TYPES,
                &self.tokens_typed,
            );
            // Model calls: this process's own (a decision, a judge) and those
            // its children report up. No `model` label, as for tokens.
            counter(
                &mut s,
                "agent_intel_calls_total",
                "Model calls, by this process and the children it supervises.",
                g(&self.intel_calls),
            );

            // --- refusal / bound counters ------------------------------------
            labelled_counter(
                &mut s,
                "agent_refusals_total",
                "Refusals by the guard that refused: a budget, a policy, the trifecta, a subagent cap.",
                "reason",
                REFUSAL_REASONS,
                &self.refusals,
            );
            labelled_counter(
                &mut s,
                "agent_limit_exceeded_total",
                "Work a hard bound stopped: a turn or subagent out of steps, tokens or time, or the lifetime budget spent.",
                "limit",
                LIMIT_LABELS,
                &self.limit_exceeded,
            );

            // --- subagent-tree gauges + counters -----------------------------
            gauge(
                &mut s,
                "agent_active_subagents",
                "Subagents not yet ended.",
                g(&self.active_subagents),
            );
            gauge(
                &mut s,
                "agent_tree_depth",
                "Depth of the deepest subagent not yet ended (1 for one a turn or step spawned).",
                g(&self.tree_depth),
            );
            gauge(
                &mut s,
                "agent_tree_breadth",
                "Most subagents not yet ended that one parent spawned.",
                g(&self.tree_breadth),
            );
            counter(
                &mut s,
                "agent_subagents_spawned_total",
                "Child processes started: turn workers and subagents.",
                g(&self.subagents_spawned),
            );
            labelled_counter(
                &mut s,
                "agent_subagents_exited_total",
                "Child processes exited: exit 0, another exit code, a signal.",
                "status",
                EXIT_STATUS,
                &self.subagents_exited,
            );
            labelled_counter(
                &mut s,
                "agent_subagent_restarts_total",
                "Subagents spawned again, by reason.",
                "reason",
                RESTART_REASONS,
                &self.subagent_restarts,
            );
            labelled_counter(
                &mut s,
                "agent_subagent_stuck_kills_total",
                "Children killed by the liveness check (stuck, or past their deadline), by signal.",
                "signal",
                SIGNAL_LABELS,
                &self.subagent_stuck_kills,
            );

            // --- intelligence health -----------------------------------------
            gauge(
                &mut s,
                "agent_intel_up",
                "1 when the last model call found its endpoint up.",
                g(&self.intel_up),
            );
            // `agent_intel_all_down`: 1 while EVERY model endpoint is down — the
            // fleet-routing signal, read from the latch that flips /readyz.
            gauge(
                &mut s,
                "agent_intel_all_down",
                "1 while all intelligence endpoints are down.",
                u64::from(crate::signals::intel_all_down()),
            );
            labelled_counter(
                &mut s,
                "agent_intel_errors_total",
                "Failed sweeps of the model endpoints, by reason; a call that backs off and sweeps again counts each.",
                "reason",
                INTEL_ERROR_REASONS,
                &self.intel_errors,
            );

            // --- MCP server health -------------------------------------------
            // `agent_mcp_up{server}` is gauge-per-declared-server; there is no
            // declared-server registration hook here, so it is RESERVED and not
            // emitted at all — the honest-absence shape the rest of this module
            // follows. The connect-failure counter below IS wired — the
            // daemon's supervisor-process connect path (initial + hot-reload add,
            // `triggers::mode`) calls `record_mcp_connect_failure(server)`, so a
            // failing declared server shows up here labelled by `server`. (A
            // child dials connections of its own; one that fails fails the
            // child, and is not counted here.)
            mcp_servers().render_connect_failures(&mut s, &self.mcp_connect_failures);

            // --- tool-call accounting — RESERVED ------------------------------
            // `agent_tool_calls_total{server,tool,ok}` is keyed off `tool.result`,
            // whose boundary (`McpClient::call_tool`) runs predominantly INSIDE the
            // children (a turn's or a subagent's tool use). Nothing counts it
            // yet, so the series is RESERVED here — rendered as a HELP/TYPE
            // marker, no fabricated 0 — and agentctl reads tool calls from
            // `tool.result` log lines. This mirrors the `agent_mcp_up`
            // honest-absence shape.
            reserved(
                &mut s,
                "agent_tool_calls_total",
                "counter",
                "Tool calls by server/tool/ok — reserved; nothing counts them yet (derive from tool.result log lines).",
            );
            // `agent_tool_call_duration_ms` / `agent_intel_call_duration_ms` /
            // `agent_run_duration_ms` are frozen HISTOGRAMS. This crate has no
            // histogram exposition machinery (no bucket/sum/count emission, by
            // design — the surface is hand-written counter/gauge text), so they are
            // RESERVED: rendered as HELP/TYPE markers only, no fabricated buckets.
            // A half-built histogram would be worse than an honest marker. Latency
            // lives in the `dur_ms` field of the matching log lines.
            reserved(
                &mut s,
                "agent_tool_call_duration_ms",
                "histogram",
                "Tool-call latency — reserved; histogram exposition not implemented (use the tool.result dur_ms field).",
            );
            reserved(
                &mut s,
                "agent_intel_call_duration_ms",
                "histogram",
                "Intelligence-call latency — reserved; histogram exposition not implemented (use the intel.result dur_ms field).",
            );
            reserved(
                &mut s,
                "agent_run_duration_ms",
                "histogram",
                "Run latency by terminal status — reserved; histogram exposition not implemented (derive from run start→terminal log lines).",
            );

            // --- lifecycle events ---------------------------------------------
            // `agent_drains_total{phase}`: the drain (`runtime::drain`) counts
            // `started` as it begins, and `completed` or `forced` as it ends.
            labelled_counter(
                &mut s,
                "agent_drains_total",
                "Drain phase transitions.",
                "phase",
                DRAIN_PHASES,
                &self.drains,
            );
            // Counted when the loop ticks again after a gap past the `/healthz`
            // window (`obs::health::tick`); one that never comes back is the
            // 503's to report.
            counter(
                &mut s,
                "agent_reactor_stalls_total",
                "Times the supervisor loop went longer than the /healthz window without a tick, counted when it came back.",
                g(&self.reactor_stalls),
            );

            // --- hot reload ---------------------------------------------------
            // `agent_config_reload_total{result}` over the closed applied/rejected
            // domain, plus `agent_config_generation` (applied-reload count) so a
            // scraper detects "generation N is effective" against the desired one.
            labelled_counter(
                &mut s,
                "agent_config_reload_total",
                "Hot reloads by result.",
                "result",
                RELOAD_RESULTS,
                &self.config_reloads,
            );
            gauge(
                &mut s,
                "agent_config_generation",
                "Successfully-applied config reloads (the live generation).",
                g(&self.config_generation),
            );

            // --- lifetime budget balance --------------------------------------
            // Absent without a lifetime budget: a 0 there would read as spent.
            let remaining = g(&self.budget_tokens_remaining);
            if remaining != u64::MAX {
                gauge(
                    &mut s,
                    "agent_budget_tokens_remaining",
                    "Tokens left under intelligence.budget.lifetime_tokens (absent without one).",
                    remaining,
                );
            }

            // --- resource pressure + work in progress -------------------------
            gauge(
                &mut s,
                "agent_pressure_level",
                "Resource-pressure level: 0 ok, 1 warn, 2 shedding (admission stopped, in-flight drains).",
                g(&self.pressure_level),
            );
            let free = g(&self.disk_free_bytes);
            if free != u64::MAX {
                gauge(
                    &mut s,
                    "agent_disk_free_bytes",
                    "Free bytes on the file store's filesystem (absent without a file store).",
                    free,
                );
            }
            gauge(
                &mut s,
                "agent_runs_active",
                "Workflow runs in a non-terminal state.",
                g(&self.runs_active),
            );
            gauge(
                &mut s,
                "agent_turns_queued",
                "Conversation turns queued for a dispatch slot.",
                g(&self.turns_queued),
            );

            // --- reactive gauges — the autoscaler's signal set ----------------
            gauge(
                &mut s,
                "agent_inflight_reactions",
                "Work executing now: runs in `running`, however they started, and conversation turns in flight.",
                g(&self.inflight_reactions),
            );
            gauge(
                &mut s,
                "agent_reaction_lag_ms",
                "Age of the oldest event waiting in the inbox (ms); 0 when none waits.",
                g(&self.reaction_lag_ms),
            );
            // One family, two kinds: the MCP resource subscriptions the
            // connections hold, and the MCP Events subscriptions by delivery
            // mode (RFC 0045 §5.12).
            let _ = writeln!(
                s,
                "# HELP agent_subscriptions_active Active subscriptions: MCP resources (kind=\"resource\") and MCP Events bindings by mode (kind=\"event\")."
            );
            let _ = writeln!(s, "# TYPE agent_subscriptions_active gauge");
            series_line(
                &mut s,
                "agent_subscriptions_active",
                &["kind"],
                &["resource".to_string()],
                g(&self.resource_subscriptions),
            );
            for (values, n) in self.event_family("agent_subscriptions_active") {
                series_line(
                    &mut s,
                    "agent_subscriptions_active",
                    &["kind", "mode"],
                    &values,
                    n,
                );
            }

            // --- bare, unlabelled series --------------------------------------
            counter(
                &mut s,
                "agent_runs_started_total",
                "Supervised runs started",
                g(&self.runs_started),
            );
            counter(
                &mut s,
                "agent_runs_completed_total",
                "Supervised runs that completed",
                g(&self.runs_completed),
            );
            counter(
                &mut s,
                "agent_runs_failed_total",
                "Supervised runs that ended in any status but completed or cancelled",
                g(&self.runs_failed),
            );
            counter(
                &mut s,
                "agent_runs_killed_total",
                "Supervised runs that ended cancelled",
                g(&self.runs_killed),
            );
            counter(
                &mut s,
                "agent_reactions_total",
                "Start nodes fired: a schedule, loop, subscription, webhook, A2A message, stream, join, event or goal had its event accepted into the inbox",
                g(&self.reactions),
            );
            counter(
                &mut s,
                "agent_tokens_input_total",
                "Input tokens reported by direct children",
                g(&self.tokens_input),
            );
            counter(
                &mut s,
                "agent_tokens_output_total",
                "Output tokens reported by direct children",
                g(&self.tokens_output),
            );

            // --- runtime series: turns, workflow steps, store, inbox, context -
            labelled_counter(
                &mut s,
                "agent_turns_total",
                "Turn workers started, by kind.",
                "kind",
                TURN_KINDS,
                &self.turns_total,
            );
            labelled_counter(
                &mut s,
                "agent_steps_total",
                "Workflow steps that ran, by how each attempt ended.",
                "status",
                STEP_STATUS,
                &self.steps_total,
            );
            labelled_counter(
                &mut s,
                "agent_store_ops_total",
                "Remote-store ops by result.",
                "result",
                STORE_RESULTS,
                &self.store_ops,
            );
            counter(
                &mut s,
                "agent_store_latency_ms_sum",
                "Cumulative remote-store op latency (ms); divide by agent_store_ops_total for the mean.",
                g(&self.store_latency_ms_sum),
            );
            gauge(
                &mut s,
                "agent_inbox_pending",
                "Durable inbox events awaiting processing.",
                g(&self.inbox_pending),
            );
            gauge(
                &mut s,
                "agent_context_tokens",
                "Estimated token size of the largest live conversation context.",
                g(&self.context_tokens),
            );

            // --- stream consumers ----------------------------------------------
            // One series per armed `stream`/`correlate` consumer, so a consumer
            // that falls behind is visible before retention trims past it (the
            // trim itself is the `stream.consumer.skipped` line). No consumers,
            // no samples: a 0 would claim a consumer that does not exist.
            {
                let table = match self.stream_lag.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                let name = "agent_stream_lag";
                let _ = writeln!(
                    s,
                    "# HELP {name} Events on a stream that its consumer has not consumed yet."
                );
                let _ = writeln!(s, "# TYPE {name} gauge");
                for (stream, consumer, lag) in table.values().flat_map(|k| &k.named) {
                    let _ = writeln!(
                        s,
                        "{name}{{stream={:?},consumer={:?}}} {lag}",
                        stream.as_str(),
                        consumer.as_str()
                    );
                }
                if let Some(other) = table.values().filter_map(|k| k.other).max() {
                    let _ = writeln!(s, "{name}{{stream=\"other\",consumer=\"other\"}} {other}");
                }
            }
            // --- MCP Events (RFC 0045 §5.12) and Companies (RFC 0047 §7) --------
            for (name, kind, help, labels) in EVENT_FAMILIES {
                let _ = writeln!(s, "# HELP {name} {help}");
                let _ = writeln!(s, "# TYPE {name} {kind}");
                for (values, n) in self.event_family(name) {
                    series_line(&mut s, name, labels, &values, n);
                }
            }
            s
        }
    }

    // --- `agent_mcp_connect_failures_total{server}` -------------------------
    // The `server` label is bounded — the declared set is small and fixed — but
    // its *values* are config-time strings, not a compile-time enum. We bound it
    // structurally with a fixed slot table that interns server names on first use;
    // once full, further names fold into `other` so the series can never grow
    // unbounded. The table is a process-global behind a Mutex —
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
        /// (`MCP_SERVER_SLOTS - 1`) once the table is full. Poisoning is ignored,
        /// because telemetry must never crash the agent.
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

        /// Emit one `agent_mcp_connect_failures_total{server="…"}` line per
        /// interned server with a non-zero count, plus the `other` overflow slot.
        fn render_connect_failures(&self, s: &mut String, ctr: &LabelCounter<MCP_SERVER_SLOTS>) {
            let names = match self.names.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let name = "agent_mcp_connect_failures_total";
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

    /// A frozen series whose machinery is not implemented: render the
    /// `# HELP`/`# TYPE` headers — so the contract stays discoverable from the
    /// scrape and a silent drop is catchable — WITHOUT a fabricated always-0
    /// sample line. This is the same honest-absence shape as `agent_mcp_up`: a
    /// marker, not a value. `kind` is the Prometheus type the series will carry
    /// once implemented (`counter`/`histogram`); `help` MUST say it is reserved
    /// and why (cross-process boundary, or no histogram exposition).
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

    /// One labelled counter family: a single HELP/TYPE header, then one series
    /// line per closed-domain label value — the domain *is* the cardinality
    /// bound. `domain` and the `LabelCounter` slots are the same length.
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
                "agent_memory_max_bytes",
                "cgroup v2 memory.max hard limit (bytes)",
                v,
            );
        }
        if let Some(v) = mem.current {
            gauge(
                &mut s,
                "agent_memory_current_bytes",
                "cgroup v2 memory.current usage (bytes)",
                v,
            );
        }
        s
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Whether `out` has exactly the sample `line`. A gauge whose help
        /// text starts with "1 while…" has `name 1` inside its `# HELP` line
        /// too, so a substring check passes whatever the value.
        fn has(out: &str, line: &str) -> bool {
            out.lines().any(|l| l == line)
        }

        #[test]
        fn render_is_valid_prometheus_text() {
            let r = Registry::new();
            r.runs_started.fetch_add(3, Ordering::Relaxed);
            r.record_run("completed");
            r.record_run("failed");
            r.record_tokens(100, 50);
            let out = r.render();
            assert!(out.contains("# TYPE agent_runs_started_total counter"));
            assert!(out.contains("agent_runs_started_total 3"));
            assert!(out.contains("agent_runs_completed_total 1"));
            assert!(out.contains("agent_runs_failed_total 1"));
            assert!(out.contains("agent_tokens_input_total 100"));
            assert!(out.contains("agent_tokens_output_total 50"));
        }

        #[test]
        fn pressure_gauges_emit_and_disk_free_is_absent_until_known() {
            let r = Registry::new();
            let out = r.render();
            assert!(out.contains("# TYPE agent_pressure_level gauge"));
            assert!(out.contains("agent_pressure_level 0"));
            assert!(out.contains("agent_runs_active 0"));
            assert!(out.contains("agent_turns_queued 0"));
            // No file store → no byte reading → the gauge is NOT emitted (an
            // exported 0 would read as "disk full" to an alert).
            assert!(!out.contains("agent_disk_free_bytes"));
            r.pressure_level.store(2, Ordering::Relaxed);
            r.disk_free_bytes.store(123_456, Ordering::Relaxed);
            r.runs_active.store(3, Ordering::Relaxed);
            r.turns_queued.store(7, Ordering::Relaxed);
            let out = r.render();
            assert!(out.contains("agent_pressure_level 2"));
            assert!(out.contains("agent_disk_free_bytes 123456"));
            assert!(out.contains("agent_runs_active 3"));
            assert!(out.contains("agent_turns_queued 7"));
        }

        #[test]
        fn frozen_schema_emits_up_and_ready_gauges() {
            let r = Registry::new();
            let out = r.render();
            // The liveness/readiness gauges are label-free.
            assert!(out.contains("# TYPE agent_up gauge"));
            assert!(has(&out, "agent_up 1"));
            assert!(out.contains("# TYPE agent_ready gauge"));
            // ready is 0/1; in a bare test process (no drain) it is 1.
            assert!(out.contains("agent_ready "));
        }

        #[test]
        fn paused_gauge_renders_zero_then_one() {
            // `agent_paused` is a 0/1 gauge, default 0.
            let r = Registry::new();
            let out = r.render();
            assert!(out.contains("# TYPE agent_paused gauge"));
            assert!(has(&out, "agent_paused 0"));
            // Set via the same atomic `set_paused` writes; renders 1.
            r.paused.store(1, Ordering::Relaxed);
            assert!(has(&r.render(), "agent_paused 1"));
        }

        #[test]
        fn intel_all_down_gauge_reads_the_readiness_latch() {
            // `agent_intel_all_down` is the latch a child's `IntelHealth`
            // report sets and `/readyz` reads, rendered as 0/1: the gauge and
            // the probe cannot disagree.
            let _g = crate::signals::test_guard();
            let r = Registry::new();
            crate::signals::set_intel_all_down(false);
            assert!(has(&r.render(), "agent_intel_all_down 0"));
            crate::signals::set_intel_all_down(true);
            let out = r.render();
            crate::signals::set_intel_all_down(false);
            assert!(out.contains("# TYPE agent_intel_all_down gauge"));
            assert!(has(&out, "agent_intel_all_down 1"), "{out}");
        }

        #[test]
        fn a_finished_run_counts_once() {
            // The run's end is one count in the labelled family and one in the
            // bare counter it falls under; nothing else is counted for it.
            let r = Registry::new();
            r.record_run("completed");
            let out = r.render();
            assert!(
                out.contains("agent_runs_total{status=\"completed\"} 1"),
                "{out}"
            );
            assert!(out.contains("agent_runs_completed_total 1"));
            let total: u64 = out
                .lines()
                .filter(|l| l.starts_with("agent_runs_total{"))
                .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
                .sum();
            assert_eq!(total, 1, "{out}");
        }

        #[test]
        fn runs_total_uses_the_run_status_domain() {
            let r = Registry::new();
            r.record_run("completed");
            r.record_run("failed");
            r.record_run("refused");
            r.record_run("refused");
            r.record_run("cancelled");
            // an out-of-vocabulary status buckets under `other`, never a new label
            r.record_run("totally_made_up");
            let out = r.render();
            assert!(out.contains("agent_runs_total{status=\"completed\"} 1"));
            assert!(out.contains("agent_runs_total{status=\"failed\"} 1"));
            assert!(out.contains("agent_runs_total{status=\"refused\"} 2"));
            assert!(out.contains("agent_runs_total{status=\"other\"} 1"));
            // every closed-domain value is present (zero-valued series included)
            assert!(out.contains("agent_runs_total{status=\"stalled\"} 0"));
            // a loop's statuses are not a run's
            assert!(!out.contains("status=\"loop_detected\""));
            // the bare counters split the same ends three ways
            assert!(out.contains("agent_runs_completed_total 1"));
            assert!(out.contains("agent_runs_killed_total 1"));
            assert!(out.contains("agent_runs_failed_total 4"));
            // exactly one HELP/TYPE header for the family
            assert_eq!(out.matches("# TYPE agent_runs_total counter").count(), 1);
        }

        #[test]
        fn typed_tokens_track_direction() {
            let r = Registry::new();
            r.record_tokens(880, 40);
            r.record_tokens(120, 10);
            let out = r.render();
            assert!(out.contains("agent_tokens_total{type=\"in\"} 1000"));
            assert!(out.contains("agent_tokens_total{type=\"out\"} 50"));
        }

        #[test]
        fn refusals_and_limits_use_closed_domains() {
            let r = Registry::new();
            r.record_refusal("trifecta");
            r.record_refusal("depth");
            r.record_refusal("depth");
            r.record_refusal("policy");
            r.record_limit_exceeded("tokens_lifetime");
            let out = r.render();
            assert!(out.contains("agent_refusals_total{reason=\"trifecta\"} 1"));
            assert!(out.contains("agent_refusals_total{reason=\"depth\"} 2"));
            assert!(out.contains("agent_refusals_total{reason=\"policy\"} 1"));
            assert!(out.contains("agent_limit_exceeded_total{limit=\"tokens_lifetime\"} 1"));
            // closed domains: a stray reason never widens the label set
            r.record_refusal("nope");
            assert!(
                r.render()
                    .contains("agent_refusals_total{reason=\"other\"} 1")
            );
        }

        #[test]
        fn a_units_end_names_the_bound_that_stopped_it() {
            // The terminal statuses a turn or a subagent ends on name the bound
            // that stopped it; any other end is no bound's.
            use super::super::bound_of;
            assert_eq!(bound_of("exhausted_steps"), Some("steps"));
            assert_eq!(bound_of("exhausted_tokens"), Some("tokens"));
            assert_eq!(bound_of("deadline"), Some("deadline"));
            for other in [
                "completed",
                "failed",
                "cancelled",
                "loop_detected",
                "refused",
            ] {
                assert_eq!(bound_of(other), None, "{other}");
            }
            // Every bound it names is in the family's domain.
            for b in ["steps", "tokens", "deadline"] {
                assert!(LIMIT_LABELS.contains(&b));
            }
        }

        #[test]
        fn tree_and_backlog_gauges_are_settable() {
            let r = Registry::new();
            r.set_tree_shape(4, 2, 3);
            r.set_reactive_backlog(1, 250, 9);
            let out = r.render();
            assert!(out.contains("agent_active_subagents 4"));
            assert!(out.contains("agent_tree_depth 2"));
            assert!(out.contains("agent_tree_breadth 3"));
            assert!(out.contains("agent_inflight_reactions 1"));
            assert!(out.contains("agent_subscriptions_active{kind=\"resource\"} 9"));
            assert!(out.contains("agent_reaction_lag_ms 250"));
            // The inbox backlog is `agent_inbox_pending`'s, and only its.
            assert!(!out.contains("agent_pending_events"));
        }

        #[test]
        fn a_childs_counts_reach_the_supervisor_once() {
            // A child counts into its own registry; what it took goes up as one
            // report, and the supervisor adds it to its own counts. A second
            // take carries only what was counted since.
            let child = Registry::new();
            let sup = Registry::new();
            assert_eq!(child.take_child(), None, "nothing counted, nothing sent");
            child.intel_calls.fetch_add(2, Ordering::Relaxed);
            child.record_intel_error("timeout");
            child.set_intel_up(true);
            child.loop_steps.fetch_add(3, Ordering::Relaxed);
            let report = child.take_child().expect("a report");
            assert_eq!(report.intel_calls, 2);
            assert_eq!(report.intel_errors.get("timeout"), Some(&1));
            assert_eq!(report.intel_up, Some(true));
            assert_eq!(report.loop_steps, 3);
            assert_eq!(child.take_child(), None, "taken counts are not sent twice");
            sup.record_child(&report);
            // It survives the wire as it was written.
            let wire: super::super::ChildMetrics =
                serde_json::from_value(serde_json::to_value(&report).unwrap()).unwrap();
            sup.record_child(&wire);
            let out = sup.render();
            assert!(out.contains("agent_intel_calls_total 4"), "{out}");
            assert!(out.contains("agent_intel_errors_total{reason=\"timeout\"} 2"));
            assert!(has(&out, "agent_intel_up 1"), "{out}");
            assert!(out.contains("agent_loop_steps_total 6"));
            // The child's last word on reachability is the gauge's.
            child.set_intel_up(false);
            sup.record_child(&child.take_child().expect("a report"));
            assert!(has(&sup.render(), "agent_intel_up 0"));
        }

        #[test]
        fn steps_count_how_an_attempt_ended() {
            // Only a step that ran ends through the counter: one skipped, or
            // on a branch not taken, never reaches it, so no `skipped` series
            // reads as "nothing was skipped".
            let r = Registry::new();
            r.record_step("done");
            r.record_step("failed");
            r.record_step("other");
            let out = r.render();
            for status in ["done", "failed", "other"] {
                assert!(
                    has(&out, &format!("agent_steps_total{{status=\"{status}\"}} 1")),
                    "{out}"
                );
            }
            assert!(
                !out.contains("agent_steps_total{status=\"skipped\"}"),
                "{out}"
            );
        }

        #[test]
        fn mcp_event_outcomes_keep_their_names() {
            // A webhook delivery that failed its check, and an occurrence only
            // the instruction follows, count by name, not as `other`.
            assert_eq!(event_outcome("unauthenticated"), "unauthenticated");
            assert_eq!(event_outcome("followed"), "followed");
            for o in EVENT_OUTCOMES {
                assert_eq!(event_outcome(o), *o);
            }
            assert_eq!(event_outcome("anything else"), "other");
        }

        #[test]
        fn mcp_events_families_label_by_binding_escape_and_fold_overflow() {
            let r = Registry::new();
            let out = r.render();
            assert!(out.contains("# TYPE agent_mcp_events_total counter"));
            assert!(out.contains("# TYPE agent_mcp_event_lag_ms gauge"));
            r.event_add(
                "agent_mcp_events_total",
                vec![
                    "pd".into(),
                    "incident.created".into(),
                    "poll".into(),
                    "appended".into(),
                ],
                2,
            );
            r.event_add(
                "agent_mcp_events_total",
                vec![
                    "pd".into(),
                    "incident.created".into(),
                    "poll".into(),
                    "appended".into(),
                ],
                1,
            );
            r.event_set(
                "agent_mcp_event_lag_ms",
                vec!["pd".into(), "a\"b".into()],
                40,
            );
            r.event_replace(
                "agent_subscriptions_active",
                vec![(vec!["event".into(), "poll".into()], 3)],
            );
            let out = r.render();
            assert!(out.contains(
                "agent_mcp_events_total{server=\"pd\",event=\"incident.created\",mode=\"poll\",outcome=\"appended\"} 3"
            ), "{out}");
            assert!(
                out.contains("agent_mcp_event_lag_ms{server=\"pd\",event=\"a\\\"b\"} 40"),
                "{out}"
            );
            assert!(
                out.contains("agent_subscriptions_active{kind=\"event\",mode=\"poll\"} 3"),
                "{out}"
            );
            assert_eq!(
                out.matches("# TYPE agent_subscriptions_active gauge")
                    .count(),
                1
            );
            for i in 0..super::super::EVENT_SERIES_SLOTS + 2 {
                r.event_add(
                    "agent_mcp_event_gaps_total",
                    vec![format!("s{i}"), "e".into(), "truncated".into()],
                    1,
                );
            }
            let out = r.render();
            assert!(
                out.contains(
                    "agent_mcp_event_gaps_total{server=\"other\",event=\"other\",cause=\"other\"} 2"
                ),
                "{out}"
            );
        }

        #[test]
        fn stream_lag_is_one_series_per_consumer_and_replaced_per_kind() {
            let r = Registry::new();
            // No consumer, no sample: the family is discoverable, but a 0 would
            // claim a consumer that does not exist.
            let out = r.render();
            assert!(out.contains("# TYPE agent_stream_lag gauge"));
            assert!(!out.contains("agent_stream_lag{"));
            r.set_stream_lag(
                "stream",
                vec![
                    ("orders".into(), "fulfil/take".into(), 3),
                    ("ticks".into(), "bulk/take".into(), 0),
                ],
            );
            r.set_stream_lag("correlate", vec![("orders".into(), "join/both".into(), 7)]);
            let out = r.render();
            assert!(out.contains("agent_stream_lag{stream=\"orders\",consumer=\"fulfil/take\"} 3"));
            assert!(out.contains("agent_stream_lag{stream=\"ticks\",consumer=\"bulk/take\"} 0"));
            assert!(out.contains("agent_stream_lag{stream=\"orders\",consumer=\"join/both\"} 7"));
            assert_eq!(out.matches("# TYPE agent_stream_lag gauge").count(), 1);
            // A pass replaces its kind's set whole: a consumer a reload removed
            // stops being exported rather than freezing at its last value, and
            // the other kind is untouched.
            r.set_stream_lag("stream", vec![("orders".into(), "fulfil/take".into(), 1)]);
            let out = r.render();
            assert!(out.contains("consumer=\"fulfil/take\"} 1"));
            assert!(!out.contains("bulk/take"));
            assert!(out.contains("consumer=\"join/both\"} 7"));
            // Past the slot limit the rest fold into ONE `other` series that
            // carries the worst of them, so the cardinality stays bounded and
            // a lagging consumer is never invisible.
            let many = (0..super::super::STREAM_LAG_SLOTS + 3)
                .map(|i| ("s".to_string(), format!("w{i}/n"), i as u64))
                .collect();
            r.set_stream_lag("stream", many);
            let out = r.render();
            let worst = super::super::STREAM_LAG_SLOTS + 2;
            assert!(out.contains(&format!(
                "agent_stream_lag{{stream=\"other\",consumer=\"other\"}} {worst}"
            )));
            assert!(!out.contains(&format!("consumer=\"w{worst}/n\"")));
            assert_eq!(
                out.lines()
                    .filter(|l| l.starts_with("agent_stream_lag{"))
                    .count(),
                super::super::STREAM_LAG_SLOTS + 2,
                "the named slots, join/both, and other"
            );
        }

        #[test]
        fn mcp_connect_failures_label_by_server_and_fold_overflow() {
            let r = Registry::new();
            r.record_mcp_connect_failure("github");
            r.record_mcp_connect_failure("github");
            r.record_mcp_connect_failure("filesystem");
            let out = r.render();
            assert!(out.contains("agent_mcp_connect_failures_total{server=\"github\"} 2"));
            assert!(out.contains("agent_mcp_connect_failures_total{server=\"filesystem\"} 1"));
        }

        #[test]
        fn drains_phase_distinguishes_clean_from_forced() {
            let r = Registry::new();
            r.record_drain("started");
            r.record_drain("completed");
            r.record_drain("forced");
            let out = r.render();
            assert!(out.contains("agent_drains_total{phase=\"completed\"} 1"));
            assert!(out.contains("agent_drains_total{phase=\"forced\"} 1"));
        }

        #[test]
        fn config_reload_total_renders_both_label_values_and_generation() {
            // The reload counter has the closed applied/rejected domain (every
            // value rendered, zero-valued included), and the generation gauge
            // tracks applied reloads.
            let r = Registry::new();
            let out = r.render();
            // Both closed-domain series are present even at zero.
            assert!(out.contains("# TYPE agent_config_reload_total counter"));
            assert!(out.contains("agent_config_reload_total{result=\"applied\"} 0"));
            assert!(out.contains("agent_config_reload_total{result=\"rejected\"} 0"));
            assert!(out.contains("# TYPE agent_config_generation gauge"));
            assert!(out.contains("agent_config_generation 0"));
            // They increment over the closed domain; an unknown buckets `other`.
            r.record_config_reload("applied");
            r.record_config_reload("rejected");
            r.record_config_reload("rejected");
            r.record_config_reload("totally_made_up");
            r.config_generation.store(1, Ordering::Relaxed);
            let out = r.render();
            assert!(out.contains("agent_config_reload_total{result=\"applied\"} 1"));
            assert!(out.contains("agent_config_reload_total{result=\"rejected\"} 2"));
            assert!(out.contains("agent_config_reload_total{result=\"other\"} 1"));
            assert!(out.contains("agent_config_generation 1"));
            // Exactly one HELP/TYPE header for the counter family.
            assert_eq!(
                out.matches("# TYPE agent_config_reload_total counter")
                    .count(),
                1
            );
        }

        #[test]
        fn budget_gauge_is_absent_without_a_lifetime_budget() {
            // No lifetime budget, no gauge: a 0 would read as a spent one.
            let r = Registry::new();
            assert!(!r.render().contains("agent_budget_tokens_remaining"));
            r.budget_tokens_remaining.store(1500, Ordering::Relaxed);
            let out = r.render();
            assert!(out.contains("# TYPE agent_budget_tokens_remaining gauge"));
            assert!(out.contains("agent_budget_tokens_remaining 1500"));
            r.budget_tokens_remaining.store(0, Ordering::Relaxed);
            assert!(r.render().contains("agent_budget_tokens_remaining 0"));
        }

        #[test]
        fn no_unbounded_identifier_labels_leak() {
            // Cardinality: render must never contain a run_id/agent_path-style
            // label key. We assert the only label keys present are the bounded set.
            let r = Registry::new();
            r.record_run("completed");
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
            assert!(g.contains("# TYPE agent_memory_max_bytes gauge"));
            assert!(g.contains("agent_memory_max_bytes 1024"));
            assert!(g.contains("agent_memory_current_bytes 512"));
            assert_eq!(g.matches(" gauge\n").count(), 2);
            // no cgroup → no gauge lines (keeps /metrics clean off-cgroup)
            assert!(memory_gauges(MemorySnapshot::default()).is_empty());
        }

        #[test]
        fn frozen_schema_4_3_series_all_present_emitted_or_reserved() {
            // Honesty gate: every frozen series MUST be discoverable from the
            // render — either as a live counter/gauge or as a reserved HELP/TYPE
            // marker. This catches a silent drop of a frozen series (which is a
            // major-bump-only change) at test time.
            let r = Registry::new();
            let out = r.render();
            // The full metric-name set (the names are the frozen contract).
            for name in [
                // liveness/readiness gauges
                "agent_up",
                "agent_ready",
                // run lifecycle + tokens + intel
                "agent_runs_total",
                "agent_run_duration_ms", // reserved (histogram)
                "agent_loop_steps_total",
                "agent_tokens_total",
                "agent_intel_calls_total",
                "agent_intel_call_duration_ms", // reserved (histogram)
                // refusal / bound
                "agent_refusals_total",
                "agent_limit_exceeded_total",
                // subagent tree
                "agent_active_subagents",
                "agent_tree_depth",
                "agent_tree_breadth",
                "agent_subagents_spawned_total",
                "agent_subagents_exited_total",
                "agent_subagent_restarts_total",
                "agent_subagent_stuck_kills_total",
                // intelligence health
                "agent_intel_up",
                "agent_intel_errors_total",
                // MCP server health
                "agent_mcp_connect_failures_total",
                // tool-call accounting (reserved)
                "agent_tool_calls_total",
                "agent_tool_call_duration_ms", // reserved (histogram)
                // lifecycle events
                "agent_drains_total",
                "agent_reactor_stalls_total",
                // reactive gauges
                "agent_inflight_reactions",
                "agent_subscriptions_active",
                "agent_reaction_lag_ms",
                // bare series
                "agent_reactions_total",
                "agent_turns_total",
            ] {
                assert!(
                    out.contains(&format!("# TYPE {name} ")),
                    "frozen series missing from render: {name}"
                );
            }
            // What nothing in agentd can write is not on the surface: no
            // restart governor, no in-process supervisor restart, and the inbox
            // backlog is `agent_inbox_pending`'s alone.
            for gone in [
                "agent_restarts_total",
                "agent_restarts_tripped_total",
                "agent_pending_events",
            ] {
                assert!(
                    !out.contains(&format!("# TYPE {gone} ")),
                    "{gone} is rendered"
                );
            }
            // The three histograms + the deferred tool-call counter are RESERVED:
            // a HELP/TYPE marker, NO fabricated sample line (the honest-absence
            // shape — no `name <value>` and no `name{...} <value>`).
            for reserved in [
                "agent_run_duration_ms",
                "agent_intel_call_duration_ms",
                "agent_tool_call_duration_ms",
                "agent_tool_calls_total",
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
            assert!(out.contains("reserved"));
        }

        #[test]
        fn wired_supervisor_counters_increment() {
            // The supervisor-process counters increment via the same registry
            // methods the emit sites call. (The emit sites live in
            // `supervisor::reactor` / `triggers::mode`; here we exercise the
            // registry contract those call sites depend on.)
            let r = Registry::new();
            // child spawn/exit (`runtime::children`).
            r.subagents_spawned.fetch_add(1, Ordering::Relaxed);
            r.record_subagent_exited("completed");
            r.record_subagent_exited("cancelled");
            // the liveness check's kill (`Runtime::on_unhealthy_child`).
            r.record_subagent_stuck_kill("kill");
            // drain phases (`Runtime::begin_drain`, `Runtime::drain_step`).
            r.record_drain("started");
            r.record_drain("completed");
            r.record_drain("forced");
            // a restored subagent spawned again (`respawn_restored_subagents`).
            r.record_subagent_restart("restored");
            // mcp connect failure (mode.rs connect + hot-reload add).
            r.record_mcp_connect_failure("github");
            // the reactor coming back from a stall (`obs::health::tick`).
            r.reactor_stalls.fetch_add(1, Ordering::Relaxed);
            let out = r.render();
            assert!(out.contains("agent_subagents_spawned_total 1"));
            assert!(out.contains("agent_subagents_exited_total{status=\"completed\"} 1"));
            assert!(out.contains("agent_subagents_exited_total{status=\"cancelled\"} 1"));
            assert!(out.contains("agent_subagent_stuck_kills_total{signal=\"kill\"} 1"));
            assert!(out.contains("agent_drains_total{phase=\"started\"} 1"));
            assert!(out.contains("agent_drains_total{phase=\"completed\"} 1"));
            assert!(out.contains("agent_drains_total{phase=\"forced\"} 1"));
            assert!(out.contains("agent_subagent_restarts_total{reason=\"restored\"} 1"));
            assert!(out.contains("agent_mcp_connect_failures_total{server=\"github\"} 1"));
            assert!(out.contains("agent_reactor_stalls_total 1"));
            // The label domains hold what is written: a child's exit is not a
            // run's status, and nothing sends a stuck child SIGTERM.
            assert!(!out.contains("agent_subagents_exited_total{status=\"exhausted_steps\"}"));
            assert!(!out.contains("signal=\"term\""));
        }
    }
}
