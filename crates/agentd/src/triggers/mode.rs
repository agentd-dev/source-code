//! Execution-mode drivers. RFC 0008 §modes.
//!
//! `once` is `main::run_once` (spawn + supervise one root). This module hosts
//! the long-lived **`reactive`** driver — the signature "listen for MCP
//! resources, act when they appear" mode — and the `loop`/`schedule` driver
//! (`run_scheduled`).
//!
//! The reactive driver: the *supervisor* connects the configured MCP servers
//! and owns the long-lived **subscriptions**; on a `notifications/resources/
//! updated{uri}` it does **notify-then-read** (`resources/read` the current
//! state, RFC 0004) and, per the [`Router`] disposition, either `Spawn`s a
//! fresh root subagent templated from the event or `Continue`s a daemon-held
//! warm session (`warm.rs`). Events are processed serially on a single thread;
//! warm sessions are supervised non-blocking.

use crate::agentloop::stop::{Outcome, ScheduleRequest, SubscriptionAction, TerminalStatus};
use crate::config::Config;
use crate::exit;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::report::{Refusals, RunReport, Usage};
use crate::signals;
use crate::subagent::protocol::{SeedMessage, SpawnPayload};
use crate::supervisor::reactor::{SuperviseResult, supervise_once};
use crate::supervisor::restart::{RestartAction, RestartConfig, RestartGovernor};
use crate::triggers::router::{Disposition, Route, Router};
use crate::triggers::warm::WarmRegistry;
use crate::wire::mcp::method;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Poll cadence for draining MCP notifications + firing due deliveries.
const TICK: Duration = Duration::from_millis(200);
/// Default per-URI debounce (RFC 0008).
const DEBOUNCE: Duration = Duration::from_millis(250);

/// A lease this replica currently holds for a claim route (RFC 0019 §3.2).
/// Held in a registry keyed by the route URI; carried so the post-react ack/
/// release and the drain step-1.5 release have the lease id + the dedupe key.
#[cfg(feature = "cluster")]
struct HeldClaim {
    /// Index of the coordination server in the connected `servers` vec.
    server_idx: usize,
    /// The opaque lease id `work.claim` granted (for renew/ack/release).
    lease_id: String,
    /// The item-derived claim key (== the spawned reaction's RUN_ID), carried on
    /// `work.ack._meta.agentd/claim_key` so the server collapses the ack.
    claim_key: String,
}

/// Build the frozen `work.*` `_meta` for a claim call (RFC 0015 §5.6). The ONLY
/// keys are `agentd/claim_key`, `agentd/instance`, `agentd/shard` (omitted when
/// unsharded), and `traceparent` (omitted when absent). **No secret, no URL** —
/// the item URI is a `work.claim` argument, never a `_meta` value.
#[cfg(feature = "cluster")]
fn claim_meta(cfg: &Config, claim_key: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert(
        "agentd/claim_key".into(),
        Value::String(claim_key.to_string()),
    );
    if let Some(instance) = crate::identity::Identity::from_env(&cfg.run_id).instance {
        m.insert("agentd/instance".into(), Value::String(instance));
    }
    if let Some(shard) = cfg.shard.label() {
        m.insert("agentd/shard".into(), Value::String(shard));
    }
    if let Some(tp) = &cfg.traceparent {
        m.insert("traceparent".into(), Value::String(tp.clone()));
    }
    Value::Object(m)
}

/// Reactive mode: subscribe to the configured resources and act on updates
/// until SIGTERM. `base` is the root payload whose `instruction` is the
/// standing task; each reaction adds the changed resource as context.
pub fn run_reactive(exe: PathBuf, base: SpawnPayload, cfg: &Config, log: &Logger) -> i32 {
    // The supervisor owns the MCP connections used for subscriptions + reads.
    // (Each spawned reaction connects its own MCP for tool use, via the payload.)
    let mut servers = Vec::new();
    for spec in &cfg.mcp_servers {
        match McpClient::spawn(&spec.name, &spec.command, Duration::from_secs(60))
            .and_then(|mut c| c.initialize().map(|()| c))
        {
            Ok(c) => {
                log.info(
                    "mcp.connect",
                    json!({"server": spec.name, "subscribe": c.capabilities().supports_subscribe()}),
                );
                servers.push(c);
            }
            Err(e) => {
                log.error(
                    "mcp.connect.fail",
                    json!({"server": spec.name, "err": e.to_string()}),
                );
                eprintln!("agentd: MCP server '{}' failed: {e}", spec.name);
                return exit::MCP_REQUIRED_DOWN;
            }
        }
    }

    // Work-claim live validation + wiring (RFC 0019 §3 / RFC 0015 §5.6),
    // `cluster`-gated. The connect loop above already exited 6 if a server is
    // down; here we check each distinct coordination server is *up and advertises*
    // `work.claim`+`work.ack` (exit 2 if not), then build `claim_by_uri` resolving
    // each route's server NAME → its connected index. The config layer already
    // guaranteed the server is a declared `--mcp` server (exit 2) and the URI is
    // in the subscribe set (routed as a Spawn).
    #[cfg(feature = "cluster")]
    let claim_by_uri: HashMap<String, crate::cluster::ClaimSpec> = {
        use std::collections::HashSet;
        let mut map: HashMap<String, crate::cluster::ClaimSpec> = HashMap::new();
        let mut validated: HashSet<usize> = HashSet::new();
        for route in &cfg.claim_routes {
            let Some(idx) = servers.iter().position(|s| s.name() == route.server) else {
                // Unreachable in practice (config validated it), but never panic.
                log.error(
                    "claim.server_missing",
                    json!({"uri": route.uri, "server": route.server}),
                );
                return exit::USAGE;
            };
            if validated.insert(idx) {
                // Live, post-handshake predicate (one list_tools per distinct
                // coordination server). A transport failure here is a down server
                // → exit 6 (retriable); up-but-missing-the-tools → exit 2.
                match servers[idx].list_tools() {
                    Ok(tools) if crate::cluster::advertises_work_tools(&tools) => {
                        log.info(
                            "claim.coord_ready",
                            json!({"server": route.server, "tools": tools.len()}),
                        );
                    }
                    Ok(_) => {
                        log.error(
                            "claim.coord_missing_tools",
                            json!({"server": route.server, "want": ["work.claim", "work.ack"]}),
                        );
                        eprintln!(
                            "agentd: claim coordination server '{}' is up but does not advertise work.claim/work.ack",
                            route.server
                        );
                        return exit::USAGE;
                    }
                    Err(e) => {
                        log.error(
                            "claim.coord_unreachable",
                            json!({"server": route.server, "err": e.to_string()}),
                        );
                        eprintln!(
                            "agentd: claim coordination server '{}' is unreachable: {e}",
                            route.server
                        );
                        return exit::MCP_REQUIRED_DOWN;
                    }
                }
            }
            map.insert(
                route.uri.clone(),
                crate::cluster::ClaimSpec {
                    server_idx: idx,
                    ttl: cfg.claim_ttl,
                    renew_fraction: cfg.claim_renew_fraction,
                    style: route.style,
                    route_id: route.uri.clone(),
                },
            );
        }
        if !map.is_empty() {
            log.info(
                "claim.armed",
                json!({"routes": map.len(), "ttl_ms": cfg.claim_ttl.as_millis() as u64}),
            );
        }
        map
    };

    // The held-claim registry (keyed by route URI — claim routes are exact-URI in
    // v1, so the synchronous spawn model holds at most one lease per URI at a
    // time). Drain step 1.5 (RFC 0019 §6) releases whatever is still held.
    #[cfg(feature = "cluster")]
    let mut held_claims: HashMap<String, HeldClaim> = HashMap::new();

    // `--subscribe` URIs route to a fresh Spawn per event; `--continue` URIs
    // route to one warm session (Disposition::Continue, session_id = the URI),
    // RFC 0008 §spawn-vs-continue.
    let mut routes: Vec<Route> = cfg
        .subscribe
        .iter()
        .map(|u| Route::new(u, Disposition::Spawn, DEBOUNCE))
        .collect();
    routes.extend(
        cfg.continue_subscribe
            .iter()
            .map(|u| Route::new(u, Disposition::Continue(u.clone()), DEBOUNCE)),
    );
    let mut router = Router::new(routes);
    let mut warm = WarmRegistry::default();

    // Shard gate (RFC 0019 §4.1): an instance with shard K/N considers only the
    // URIs it owns; out-of-shard URIs are dropped at routing intake (before the
    // debounce queue + before spawn) at near-zero cost. `shard_key` defaults to
    // the resource URI. Only active under the `cluster` feature with N>1 — a
    // default build (or N==1) owns everything, exactly as RFC 0008.
    #[cfg(feature = "cluster")]
    let shard = crate::cluster::Shard {
        k: cfg.shard.k,
        n: cfg.shard.n,
    };
    if cfg.shard.n > 1 {
        log.info(
            "shard.armed",
            json!({"k": cfg.shard.k, "n": cfg.shard.n, "shard": cfg.shard.label()}),
        );
    }
    // `in_shard(uri)`: true when this replica owns the URI (always true without
    // the feature / when unsharded). Drops increment `agentd_shard_skipped_total`.
    let in_shard = |uri: &str| -> bool {
        #[cfg(feature = "cluster")]
        {
            if shard.owns(uri) {
                true
            } else {
                crate::obs::metrics::record_shard_skipped();
                false
            }
        }
        #[cfg(not(feature = "cluster"))]
        {
            let _ = uri;
            true
        }
    };

    // Subscribe each URI (spawn + continue alike) on the first connected server
    // that supports it; track which server owns each URI so we read it back from
    // the same place.
    let mut owner: HashMap<String, usize> = HashMap::new();
    for uri in cfg.subscribe.iter().chain(&cfg.continue_subscribe) {
        let mut armed = false;
        for (i, s) in servers.iter().enumerate() {
            if s.capabilities().supports_subscribe() {
                match s.subscribe(uri) {
                    Ok(()) => {
                        owner.insert(uri.clone(), i);
                        log.info("subscribe", json!({"uri": uri, "server": s.name()}));
                        armed = true;
                        break;
                    }
                    Err(e) => log.warn("subscribe.fail", json!({"uri": uri, "err": e.to_string()})),
                }
            }
        }
        if !armed {
            log.warn("subscribe.unsupported", json!({"uri": uri}));
        }
    }
    log.info(
        "trigger.armed",
        json!({"kind": "reactive", "subscriptions": owner.len(), "servers": servers.len()}),
    );

    // Read-after-subscribe (mandatory, RFC 0008 / assessment §2.8): treat every
    // watched resource as possibly-changed at startup so a change that happened
    // before (or during) subscribing isn't missed. Converts the edge-triggered
    // `updated` notification into level-triggered "act on current state", and
    // recovers updates missed across a restart. The reactive model acts on what
    // the resource *is* now, so this is safe and idempotent.
    let t0 = Instant::now();
    for uri in owner.keys() {
        // Shard gate precedes routing: an out-of-shard URI never enters `pending`,
        // even on the startup read-after-subscribe sweep (RFC 0019 §4.1).
        if in_shard(uri) && router.on_updated(uri, t0) {
            log.info("reactive.initial_read", json!({"uri": uri}));
        }
    }

    // Triggers armed, subscriptions live: the supervisor is now ready to react.
    log.info("proc.ready", json!({"mode": "reactive"}));

    // Self-scheduled wake-ups (RFC 0008 §self-scheduling): (fire-at, instruction)
    // an agent requested for itself via the `schedule` self-tool. The daemon owns
    // them — a reaction can set its own next tick.
    let mut wakes: Vec<(Instant, String)> = Vec::new();

    loop {
        crate::obs::health::tick();
        if signals::draining() {
            // Drain step 1.5 (RFC 0019 §6): release every held claim BEFORE
            // winding down, so a surviving replica re-claims immediately rather
            // than waiting out the lease TTL. Best-effort under a hard sub-budget
            // (`min(2s, drain_timeout/4)` total) — never blocks drain past it; a
            // failed release is logged + counted, never fatal (the TTL backstops).
            #[cfg(feature = "cluster")]
            if !held_claims.is_empty() {
                let budget = std::cmp::min(Duration::from_secs(2), cfg.drain_timeout / 4);
                let deadline = Instant::now() + budget;
                let total = held_claims.len();
                let mut released = 0usize;
                for (uri, held) in held_claims.drain() {
                    if Instant::now() >= deadline {
                        log.warn(
                            "drain.claim_release_budget",
                            json!({"released": released, "total": total}),
                        );
                        break;
                    }
                    crate::obs::metrics::record_claim_released();
                    match crate::cluster::claim::release(
                        &servers[held.server_idx],
                        &held.lease_id,
                        "draining",
                    ) {
                        Ok(()) => {
                            released += 1;
                            log.info("claim.released", json!({"uri": uri, "reason": "draining"}));
                        }
                        Err(e) => log.warn(
                            "drain.claim_release_failed",
                            json!({"uri": uri, "lease": held.lease_id, "err": e}),
                        ),
                    }
                }
            }

            // Wind down warm sessions gracefully (cancel → let them emit a
            // terminal Result + exit, bounded by the drain timeout), then drop
            // any stragglers (kill + reap).
            warm.cancel_all(log);
            let deadline = Instant::now() + cfg.drain_timeout;
            while !warm.is_empty() && Instant::now() < deadline {
                let _ = warm.drain(log);
                std::thread::sleep(Duration::from_millis(50));
            }
            warm.clear();
            for (uri, &i) in &owner {
                let _ = servers[i].unsubscribe(uri); // best-effort
            }
            log.info("proc.exit", json!({"reason": "drain", "mode": "reactive"}));
            return exit::SUCCESS;
        }
        let now = Instant::now();

        // Drain inbound notifications → feed the router. (list_changed
        // re-enumeration for templated subscriptions lands later.)
        for s in &servers {
            for n in s.drain_notifications() {
                if n.method == method::NOTIFY_RESOURCES_UPDATED
                    && let Some(uri) = updated_uri(&n.params)
                    && in_shard(&uri)
                    && router.on_updated(&uri, now)
                {
                    log.info("resource.updated", json!({"uri": uri}));
                }
            }
        }

        // Fire due (debounced) deliveries: notify-then-read, then react.
        for delivery in router.due(now) {
            let content = read_current(&servers, &owner, &delivery.uri).unwrap_or_default();
            crate::obs::metrics::record_reaction();
            match delivery.disposition {
                Disposition::Spawn => {
                    // A fresh, independent reaction per event (synchronous v1).
                    // `mut` is needed only in a `cluster` build (the RUN_ID
                    // narrowing override below the claim gate).
                    #[cfg_attr(not(feature = "cluster"), allow(unused_mut))]
                    let mut payload = reactive_payload(&base, &delivery.uri, &content);

                    // CLAIM GATE (RFC 0019 §3.4), `cluster`-gated: for a claim
                    // route, claim the item BEFORE spawning and proceed only on a
                    // granted lease. The spawned reaction then runs with the
                    // item-derived RUN_ID so every downstream side-effect dedupes
                    // on the same key (RFC 0019 §3.5 / RFC 0011 §6.2).
                    #[cfg(feature = "cluster")]
                    if let Some(spec) = claim_by_uri.get(&delivery.uri) {
                        let claim_key =
                            crate::cluster::derive_claim_key(&delivery.uri, &spec.route_id);
                        let meta = claim_meta(cfg, &claim_key);
                        let coord = &servers[spec.server_idx];
                        match crate::cluster::claim(coord, &delivery.uri, spec.ttl, meta) {
                            crate::cluster::ClaimOutcome::Lost { held_by } => {
                                crate::obs::metrics::record_claim_lost();
                                log.info(
                                    "claim.lost",
                                    json!({"uri": delivery.uri, "held_by": held_by}),
                                );
                                continue; // another replica owns it — skip.
                            }
                            crate::cluster::ClaimOutcome::Error(e) => {
                                // A failed reaction never kills the daemon (RFC
                                // 0019 §8 row 6): skip this delivery, keep serving.
                                log.error("claim.error", json!({"uri": delivery.uri, "err": e}));
                                continue;
                            }
                            crate::cluster::ClaimOutcome::Granted {
                                lease_id,
                                expires_in_ms,
                            } => {
                                crate::obs::metrics::record_claim_granted();
                                log.info(
                                    "claim.granted",
                                    json!({"uri": delivery.uri, "expires_in_ms": expires_in_ms}),
                                );
                                held_claims.insert(
                                    delivery.uri.clone(),
                                    HeldClaim {
                                        server_idx: spec.server_idx,
                                        lease_id,
                                        claim_key: claim_key.clone(),
                                    },
                                );
                                // RUN_ID narrowing (RFC 0019 §3.5): the child
                                // stamps `_meta.agentd/run_id` from this field
                                // (subagent/control.rs), so overriding it routes
                                // every side-effect dedupe onto the claim key.
                                payload.telemetry.run_id = claim_key;
                            }
                        }
                    }

                    log.info(
                        "trigger.fired",
                        json!({"uri": delivery.uri, "bytes": content.len()}),
                    );
                    let outcome = react(&exe, &payload, cfg.drain_timeout, log);

                    // Settle the claim (RFC 0019 §3.4): a terminal `completed`
                    // run acks (the side effect is committed + deduped on the
                    // claim key); anything else releases (the item is immediately
                    // re-claimable). The synchronous spawn model means the claim
                    // is claimed→settled within this one deliver iteration.
                    #[cfg(feature = "cluster")]
                    if let Some(held) = held_claims.remove(&delivery.uri) {
                        settle_claim(&servers, &held, outcome.as_ref(), log);
                    }

                    if let Some(o) = outcome {
                        apply_effects(o, &mut wakes, &mut router, &mut owner, &servers, log);
                    }
                }
                Disposition::Continue(session_id) => {
                    // Deliver into the one warm session for this route (spawn it
                    // on the first event, inject thereafter). Non-blocking — the
                    // session's turn outcomes are drained below.
                    let payload = reactive_payload(&base, &delivery.uri, &content);
                    let event = changed_message(&delivery.uri, &content);
                    log.info(
                        "trigger.fired",
                        json!({"uri": delivery.uri, "bytes": content.len(), "session": session_id}),
                    );
                    if let Err(e) = warm.deliver(&exe, &session_id, payload, &event, log) {
                        log.error(
                            "warm.spawn_fail",
                            json!({"session": session_id, "err": e.to_string()}),
                        );
                    }
                }
            }
        }

        // Drain any warm continue-sessions: each completed turn may itself
        // self-schedule / self-subscribe, applied like a Spawn reaction's.
        for (_session, outcome) in warm.drain(log) {
            apply_effects(outcome, &mut wakes, &mut router, &mut owner, &servers, log);
        }

        // Fire due self-scheduled wake-ups: each runs its own instruction as a
        // fresh reaction, and may schedule further wake-ups (a self-sustaining
        // agent, bounded by the daemon lifetime + per-run budgets).
        for instruction in drain_due_wakes(&mut wakes, now) {
            let payload = scheduled_payload(&base, &instruction);
            log.info(
                "trigger.fired",
                json!({"kind": "self_schedule", "instruction_len": instruction.len()}),
            );
            crate::obs::metrics::record_reaction();
            if let Some(o) = react(&exe, &payload, cfg.drain_timeout, log) {
                apply_effects(o, &mut wakes, &mut router, &mut owner, &servers, log);
            }
        }

        // Publish the reactive-backlog scaling signals (RFC 0019 §5.1) each tick:
        // `pending` distinct queued URIs, `inflight` warm active sessions,
        // `subscriptions` reconciled live, and the lag of the oldest pending item
        // (how overdue it is past its debounce). No-op without the `metrics`
        // feature — call it unconditionally.
        let now = Instant::now();
        let lag_ms = router
            .oldest_pending()
            .map(|at| now.saturating_duration_since(at).as_millis() as u64)
            .unwrap_or(0);
        crate::obs::metrics::set_reactive_backlog(
            router.pending_count() as u64,
            warm.len() as u64,
            owner.len() as u64,
            lag_ms,
        );

        std::thread::sleep(TICK);
    }
}

/// `loop`/`schedule` driver — re-run the standing instruction on a timer until
/// SIGTERM. RFC 0008 §modes: a clock is just another trigger; each fire is an
/// independent supervised run (`once` semantics). `loop` (interval default 0)
/// re-enters back-to-back; `schedule` fires on its `--interval`. The optional
/// 5-field-cron source is the `cron` feature (later); v1 is interval-based.
///
/// Daemon exit predicate = signal only; a per-fire run carries its own
/// deadline, and the orchestrator bounds the daemon (Job deadline). Failed
/// fires are governed by the [`RestartGovernor`] (RFC 0003 §3.7): exponential
/// backoff + capped jitter keeps it from hot-spinning, and a crash-loop trips
/// the circuit breaker (assessment §4 M2) — at which point the daemon exits
/// rather than respawn into a known-bad loop.
pub fn run_scheduled(exe: PathBuf, base: SpawnPayload, cfg: &Config, log: &Logger) -> i32 {
    let interval = cfg.interval.unwrap_or(Duration::ZERO);
    // This driver supervises whole re-runs, not the session-backing children the
    // §3.7 default profile is tuned for: each fire spawns, reaches `ready`, runs
    // its loop, and exits. So a transient-dependency failure (e.g. intelligence
    // momentarily unreachable) reaches the model call before failing and must be
    // an *ordinary* governed failure, not the fork-bomb fast-fail. We keep the
    // §3.7 backoff/breaker but set `spawn_ready` to a sliver, so only a run that
    // dies near-instantly (couldn't do any work — the genuine crash-on-spawn,
    // §3.6) is weighted heavier here. RFC 0003 §3.7.
    let mut governor = RestartGovernor::new(RestartConfig {
        spawn_ready: Duration::from_millis(50),
        ..RestartConfig::default()
    });
    // Parse the optional cron schedule (feature-gated; a bad expr fails fast).
    // Without the feature `--cron` is inert — warned once, falls back to interval.
    #[cfg(feature = "cron")]
    let cron: Option<crate::triggers::timer::CronExpr> = match &cfg.cron {
        Some(expr) => match crate::triggers::timer::CronExpr::parse(expr) {
            Ok(c) => Some(c),
            Err(e) => {
                log.error("config.invalid", json!({"cron": expr, "err": e}));
                return exit::USAGE;
            }
        },
        None => None,
    };
    #[cfg(not(feature = "cron"))]
    if cfg.cron.is_some() {
        log.warn(
            "cron.unavailable",
            json!({"reason": "built without --features cron"}),
        );
    }

    let mut iteration: u64 = 0;
    // The bounded daemon's report bookends (RFC 0016 §6): a `loop`/`schedule`
    // daemon reports its TERMINAL disposition (a clean drain → `cancelled`/exit 0;
    // a tripped restart breaker → `crashed`/exit 1). `last_status` carries the most
    // recent fire's terminal status into a drain report (the "schedule-tick"
    // outcome, §6). Reactive emits nothing — it never reaches this driver (§6.4).
    let daemon_started = SystemTime::now();
    let mut last_status = TerminalStatus::Completed;
    log.info(
        "trigger.armed",
        json!({"kind": cfg.mode.as_str(), "interval_ms": interval.as_millis() as u64, "cron": cfg.cron}),
    );
    log.info("proc.ready", json!({"mode": cfg.mode.as_str()}));

    // Timer-shard gate (RFC 0019 §4.1): a sharded `schedule`/`loop` fleet must not
    // have every replica fire the same tick. In `shard0` mode (the default) only
    // shard 0 fires; the others have no work — so this instance idles until SIGTERM
    // and exits 0 cleanly rather than running the ticker. (`keyed` mode fires on
    // every replica; the per-tick key gate is applied elsewhere / deferred.)
    if !fires_timers(cfg) {
        log.info(
            "shard.idle",
            json!({"k": cfg.shard.k, "n": cfg.shard.n, "timer": cfg.shard.timer.as_str(), "reason": "non-firing timer shard"}),
        );
        while !signals::draining() {
            crate::obs::health::tick();
            std::thread::sleep(Duration::from_millis(100));
        }
        log.info(
            "proc.exit",
            json!({"reason": "drain", "mode": cfg.mode.as_str()}),
        );
        write_daemon_report(cfg, last_status, exit::SUCCESS, daemon_started, log);
        return exit::SUCCESS;
    }

    loop {
        crate::obs::health::tick();
        if signals::draining() {
            log.info(
                "proc.exit",
                json!({"reason": "drain", "mode": cfg.mode.as_str()}),
            );
            write_daemon_report(cfg, last_status, exit::SUCCESS, daemon_started, log);
            return exit::SUCCESS;
        }

        // cron fires *at* its instants, so the wait precedes the run (vs interval,
        // whose spacing is applied after the run below).
        #[cfg(feature = "cron")]
        let cron_active = cron.is_some();
        #[cfg(not(feature = "cron"))]
        let cron_active = false;
        #[cfg(feature = "cron")]
        if let Some(c) = &cron {
            let now = now_unix_secs();
            let wait = c
                .next_after(now)
                .map(|t| Duration::from_secs(t.saturating_sub(now)))
                .unwrap_or(Duration::from_secs(60));
            sleep_interruptible(wait);
            if signals::draining() {
                log.info(
                    "proc.exit",
                    json!({"reason": "drain", "mode": cfg.mode.as_str()}),
                );
                write_daemon_report(cfg, last_status, exit::SUCCESS, daemon_started, log);
                return exit::SUCCESS;
            }
        }

        iteration += 1;
        log.info("schedule.fired", json!({"iteration": iteration}));

        // Time the run so the governor can spot a crash-on-spawn (RFC 0003 §3.7
        // — a run that dies faster than the ready threshold counts heavier).
        let started = Instant::now();
        let ok = match supervise_once(exe.clone(), &base, cfg.drain_timeout, log.clone()) {
            Ok(SuperviseResult::Completed(o)) => {
                log.info("run.completed", json!({"status": o.status.as_str()}));
                // Carry the fire's terminal status into a later drain report (§6).
                last_status = o.status;
                true
            }
            Ok(SuperviseResult::Failed(e)) => {
                log.warn("run.failed", json!({"err": e}));
                last_status = TerminalStatus::Crashed;
                false
            }
            Ok(SuperviseResult::Killed(r)) => {
                log.warn("run.killed", json!({"reason": format!("{r:?}")}));
                last_status = TerminalStatus::Crashed;
                false
            }
            Err(e) => {
                log.error("run.spawn_fail", json!({"err": e.to_string()}));
                last_status = TerminalStatus::Crashed;
                false
            }
        };

        // Consult the restart governor. A successful run resets it and waits
        // the configured interval — 0 for `loop`, the `--interval` for
        // `schedule` (interval semantics preserved). A failed/killed run either
        // backs off (capped + jittered, never below the interval) or, on a
        // tripped breaker, ends the daemon rather than respawn into a known-bad
        // loop. RFC 0003 §3.7 / assessment §4 M2 "crash-loop trips breaker".
        // cron's spacing is the pre-run wait above, so a successful cron fire has
        // no post-wait; interval mode waits its interval here. A failed run still
        // backs off (capped + jittered) regardless of the schedule source.
        let post_wait = if cron_active {
            Duration::ZERO
        } else {
            interval
        };
        let now = Instant::now();
        match governor.on_outcome(ok, now.duration_since(started), now) {
            _ if ok => sleep_interruptible(post_wait),
            RestartAction::Backoff(d) => sleep_interruptible(d.max(post_wait)),
            RestartAction::Tripped => {
                crate::obs::metrics::record_restart_tripped();
                log.warn(
                    "proc.exit",
                    json!({"reason": "restart_breaker", "iteration": iteration}),
                );
                // The daemon ends on a known-bad loop → a `crashed`/exit-1 report.
                write_daemon_report(
                    cfg,
                    TerminalStatus::Crashed,
                    exit::GENERIC,
                    daemon_started,
                    log,
                );
                return exit::GENERIC;
            }
        }
    }
}

/// Write the bounded `loop`/`schedule` daemon's run-outcome report at its
/// terminal transition (RFC 0016 §6). Off for a bare run (no `--report-file`);
/// reactive never calls this (§6.4). `status` is the daemon's terminal
/// disposition (the last fire's status on a drain, `crashed` on a breaker trip);
/// `exit_code` is its coarse projection. Usage is best-effort `0` (the daemon
/// does not aggregate per-fire totals here) — honest absence, never an estimate
/// (RFC 0010 §3.9). Best-effort-but-loud: a failed write logs `report.write.fail`
/// and never gates the exit (§8.4).
fn write_daemon_report(
    cfg: &Config,
    status: TerminalStatus,
    exit_code: i32,
    started: SystemTime,
    log: &Logger,
) {
    let Some(path) = cfg.report_file.as_deref() else {
        return;
    };
    let identity = crate::identity::Identity::from_env(&cfg.run_id);
    let trace_id =
        Some(crate::obs::trace::resolve(&cfg.run_id, cfg.traceparent.as_deref()).trace_id);
    let report = RunReport::new(
        cfg.run_id.clone(),
        identity.instance,
        cfg.mode.as_str().to_string(),
        status,
        exit_code,
        false,
        Usage::default(),
        Refusals::default(),
        trace_id,
        started,
        SystemTime::now(),
    );
    report.write_to_file(path, log);
}

/// Whether this instance fires timer events for its shard identity (RFC 0019
/// §4.1). An unsharded instance (`n == 1`, the default / non-cluster build) always
/// fires. In `shard0` mode only shard 0 fires (one fleet-wide ticker); in `keyed`
/// mode every replica fires. Pure (reads `cfg.shard`).
fn fires_timers(cfg: &Config) -> bool {
    use crate::config::TimerShardMode;
    if cfg.shard.n == 1 {
        return true;
    }
    match cfg.shard.timer {
        TimerShardMode::Shard0 => cfg.shard.k == 0,
        TimerShardMode::Keyed => true,
    }
}

/// Current UTC unix seconds — the clock the cron source matches against.
#[cfg(feature = "cron")]
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Sleep up to `dur`, returning early if a drain is requested (so SIGTERM
/// during a long interval wakes the daemon promptly).
fn sleep_interruptible(dur: Duration) {
    let deadline = Instant::now() + dur;
    loop {
        crate::obs::health::tick(); // stay alive across a long inter-run wait
        if signals::draining() {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
}

/// Settle a held claim after its reaction returns (RFC 0019 §3.4). A terminal
/// `completed` outcome acks (`work.ack`, carrying `agentd/claim_key` so the server
/// collapses a redelivered-but-already-acked item); any non-terminal / failed
/// outcome releases (`work.release{reason:"wind-down"}`) so the item is immediately
/// re-claimable. Best-effort: a failed ack/release is logged + counted, never
/// fatal — the lease TTL is the backstop.
#[cfg(feature = "cluster")]
fn settle_claim(servers: &[McpClient], held: &HeldClaim, outcome: Option<&Outcome>, log: &Logger) {
    let coord = &servers[held.server_idx];
    let completed = outcome.is_some_and(|o| o.status == TerminalStatus::Completed);
    if completed {
        match crate::cluster::claim::ack(coord, &held.lease_id, &held.claim_key) {
            Ok(()) => log.info("claim.acked", json!({"lease": held.lease_id})),
            Err(e) => log.warn(
                "claim.ack_failed",
                json!({"lease": held.lease_id, "err": e}),
            ),
        }
    } else {
        crate::obs::metrics::record_claim_released();
        match crate::cluster::claim::release(coord, &held.lease_id, "wind-down") {
            Ok(()) => log.info("claim.released", json!({"lease": held.lease_id})),
            Err(e) => log.warn(
                "claim.release_failed",
                json!({"lease": held.lease_id, "err": e}),
            ),
        }
    }
}

/// Spawn + supervise one reaction synchronously, logging the outcome and
/// returning its `Outcome` (only when it completed) so the daemon can apply the
/// agent's self-scheduling / self-subscription requests (RFC 0008).
fn react(exe: &Path, payload: &SpawnPayload, drain: Duration, log: &Logger) -> Option<Outcome> {
    match supervise_once(exe.to_path_buf(), payload, drain, log.clone()) {
        Ok(SuperviseResult::Completed(o)) => {
            log.info("reactive.handled", json!({"status": o.status.as_str()}));
            Some(o)
        }
        Ok(SuperviseResult::Failed(e)) => {
            log.error("reactive.failed", json!({"err": e}));
            None
        }
        Ok(SuperviseResult::Killed(r)) => {
            log.warn("reactive.killed", json!({"reason": format!("{r:?}")}));
            None
        }
        Err(e) => {
            log.error("reactive.spawn_fail", json!({"err": e.to_string()}));
            None
        }
    }
}

/// Apply a completed reaction's self-requests: arm its scheduled wake-ups and
/// add/remove its resource subscriptions on the live router + servers (RFC 0008).
fn apply_effects(
    o: Outcome,
    wakes: &mut Vec<(Instant, String)>,
    router: &mut Router,
    owner: &mut HashMap<String, usize>,
    servers: &[McpClient],
    log: &Logger,
) {
    arm_wakes(wakes, o.scheduled, Instant::now(), log);
    for req in o.subscriptions {
        match req.action {
            SubscriptionAction::Subscribe => {
                if router.has_exact(&req.uri) {
                    continue; // already watched
                }
                let armed = servers.iter().enumerate().any(|(i, s)| {
                    if s.capabilities().supports_subscribe() && s.subscribe(&req.uri).is_ok() {
                        owner.insert(req.uri.clone(), i);
                        // Self-subscribe = self-scheduling into a WARM session
                        // (RFC 0008 §self-subscribe): the agent re-enters one live
                        // continue-session per event (session keyed by the URI),
                        // rather than a fresh spawn each time.
                        router.add_route(Route::new(&req.uri, Disposition::Continue(req.uri.clone()), DEBOUNCE));
                        log.info("trigger.armed", json!({"kind": "self_subscribe", "uri": req.uri, "server": s.name(), "disposition": "continue"}));
                        true
                    } else {
                        false
                    }
                });
                if !armed {
                    log.warn(
                        "subscribe.unsupported",
                        json!({"uri": req.uri, "kind": "self_subscribe"}),
                    );
                }
            }
            SubscriptionAction::Unsubscribe => {
                if let Some(i) = owner.remove(&req.uri) {
                    let _ = servers[i].unsubscribe(&req.uri);
                }
                if router.remove_exact(&req.uri) > 0 {
                    log.info(
                        "unsubscribe",
                        json!({"uri": req.uri, "kind": "self_subscribe"}),
                    );
                }
            }
        }
    }
}

/// Arm self-scheduled wake-ups relative to `base_time`, logging each (RFC 0008).
fn arm_wakes(
    wakes: &mut Vec<(Instant, String)>,
    reqs: Vec<ScheduleRequest>,
    base_time: Instant,
    log: &Logger,
) {
    for r in reqs {
        let at = base_time + Duration::from_millis(r.after_ms);
        log.info(
            "trigger.armed",
            json!({"kind": "self_schedule", "after_ms": r.after_ms}),
        );
        wakes.push((at, r.instruction));
    }
}

/// Remove and return the instructions of every wake-up now due (fire-at ≤ now),
/// retaining the rest. Pure (drains `wakes` in place).
fn drain_due_wakes(wakes: &mut Vec<(Instant, String)>, now: Instant) -> Vec<String> {
    let mut due = Vec::new();
    wakes.retain(|(at, instruction)| {
        if *at <= now {
            due.push(instruction.clone());
            false
        } else {
            true
        }
    });
    due
}

/// The "resource changed" event message — the user turn a reaction acts on. Used
/// as a fresh spawn's seed and as a warm session's inject body, so both
/// dispositions react to the identical event framing. Pure.
pub fn changed_message(uri: &str, content: &str) -> String {
    format!("The resource {uri} changed. Its current content is:\n\n{content}")
}

/// Build the payload for one reaction: the standing instruction plus the
/// changed resource's current state as context. Pure.
pub fn reactive_payload(base: &SpawnPayload, uri: &str, content: &str) -> SpawnPayload {
    let mut p = base.clone();
    p.context_seed = vec![SeedMessage {
        role: "user".into(),
        content: changed_message(uri, content),
    }];
    p
}

/// Build the payload for a self-scheduled wake-up: the agent's own deferred
/// `instruction` replaces the standing one; no resource context. Pure.
fn scheduled_payload(base: &SpawnPayload, instruction: &str) -> SpawnPayload {
    let mut p = base.clone();
    p.instruction = instruction.to_string();
    p.context_seed = Vec::new();
    p
}

fn updated_uri(params: &Option<Value>) -> Option<String> {
    params.as_ref()?.get("uri")?.as_str().map(str::to_string)
}

fn read_current(
    servers: &[McpClient],
    owner: &HashMap<String, usize>,
    uri: &str,
) -> Option<String> {
    let idx = *owner.get(uri)?;
    servers.get(idx)?.read_resource(uri).ok().map(|r| r.text())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::protocol::{IntelConfig, Limits, Telemetry};

    fn base() -> SpawnPayload {
        SpawnPayload {
            instruction: "triage the change".into(),
            output_contract: None,
            context_seed: Vec::new(),
            intelligence: IntelConfig {
                uri: "unix:/x".into(),
                token: None,
                model: None,
            },
            mcp_servers: Vec::new(),
            a2a_peers: Vec::new(),
            limits: Limits {
                max_steps: 10,
                max_tokens: 1000,
                deadline_ms: 1000,
                max_depth: 4,
            },
            telemetry: Telemetry {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                trace_id: None,
                log_level: "error".into(),
                log_content: false,
            },
            depth: 0,
            enable_exec: false,
            warm: false,
        }
    }

    #[test]
    fn reactive_payload_keeps_instruction_and_adds_context() {
        let p = reactive_payload(&base(), "file:///in.json", "{\"n\":1}");
        assert_eq!(p.instruction, "triage the change"); // standing instruction preserved
        assert_eq!(p.context_seed.len(), 1);
        assert!(p.context_seed[0].content.contains("file:///in.json"));
        assert!(p.context_seed[0].content.contains("{\"n\":1}"));
    }

    #[test]
    fn fires_timers_gates_non_firing_shards() {
        use crate::config::{Mode, ShardCfg, TimerShardMode};
        let mut cfg = Config {
            mode: Mode::Schedule,
            ..Config::default()
        };
        // Unsharded (default): always fires.
        assert!(fires_timers(&cfg));
        // shard0 mode: only shard 0 of a real fleet fires; others idle.
        cfg.shard = ShardCfg {
            k: 0,
            n: 4,
            timer: TimerShardMode::Shard0,
        };
        assert!(fires_timers(&cfg));
        cfg.shard.k = 2;
        assert!(!fires_timers(&cfg));
        // keyed mode: every replica fires (the per-key gate is elsewhere).
        cfg.shard.timer = TimerShardMode::Keyed;
        assert!(fires_timers(&cfg));
    }

    #[cfg(feature = "cluster")]
    #[test]
    fn claim_meta_carries_only_the_frozen_keys_and_no_secret_or_url() {
        use crate::config::{Config, ShardCfg, TimerShardMode};
        // The downward-API instance var is process-global; set + clear around use.
        unsafe { std::env::set_var("AGENTD_POD_NAME", "pod-abc") };
        let cfg = Config {
            run_id: "run-1".into(),
            traceparent: Some("00-trace-span-01".into()),
            shard: ShardCfg {
                k: 3,
                n: 8,
                timer: TimerShardMode::Shard0,
            },
            // A token IS set; it must never reach the claim _meta.
            intelligence_token: Some("super-secret".into()),
            intelligence: Some("https://user:cred@api.example/v1".into()),
            ..Config::default()
        };
        let m = claim_meta(&cfg, "deadbeef");
        // Exactly the frozen set (RFC 0015 §5.6).
        assert_eq!(m["agentd/claim_key"], json!("deadbeef"));
        assert_eq!(m["agentd/instance"], json!("pod-abc"));
        assert_eq!(m["agentd/shard"], json!("3/8"));
        assert_eq!(m["traceparent"], json!("00-trace-span-01"));
        let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys.len(), 4, "only the 4 frozen keys: {keys:?}");
        // No secret, no URL/host anywhere in the serialized meta.
        let blob = serde_json::to_string(&m).unwrap();
        assert!(!blob.contains("super-secret"), "token leaked into _meta");
        assert!(!blob.contains("api.example"), "endpoint leaked into _meta");
        assert!(!blob.contains("cred"), "credential leaked into _meta");
        unsafe { std::env::remove_var("AGENTD_POD_NAME") };

        // Unsharded ⇒ the shard key is OMITTED (not null).
        let cfg2 = Config {
            run_id: "run-2".into(),
            ..Config::default()
        };
        let m2 = claim_meta(&cfg2, "k");
        assert!(
            m2.get("agentd/shard").is_none(),
            "shard must be omitted when unsharded"
        );
        assert!(
            m2.get("traceparent").is_none(),
            "traceparent omitted when absent"
        );
        assert_eq!(m2["agentd/claim_key"], json!("k"));
    }

    #[test]
    fn updated_uri_parses() {
        assert_eq!(
            updated_uri(&Some(json!({"uri": "file://a"}))),
            Some("file://a".into())
        );
        assert_eq!(updated_uri(&Some(json!({"title": "x"}))), None);
        assert_eq!(updated_uri(&None), None);
    }

    fn test_logger() -> Logger {
        use crate::obs::log::{Comp, Level, LogCtx};
        Logger::new(
            LogCtx {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: 0,
                trace_id: None,
            },
            Level::Error,
        )
    }

    #[test]
    fn scheduled_payload_replaces_instruction_and_clears_seed() {
        let mut b = base();
        b.context_seed = vec![SeedMessage {
            role: "user".into(),
            content: "stale".into(),
        }];
        let p = scheduled_payload(&b, "do the deferred thing");
        assert_eq!(p.instruction, "do the deferred thing"); // the agent's own follow-up
        assert!(p.context_seed.is_empty()); // no resource context on a time wake
    }

    #[test]
    fn arm_and_drain_wakes_fire_past_due_keep_future() {
        let now = Instant::now();
        let mut wakes: Vec<(Instant, String)> = Vec::new();
        arm_wakes(
            &mut wakes,
            vec![
                ScheduleRequest {
                    after_ms: 0,
                    instruction: "now".into(),
                },
                ScheduleRequest {
                    after_ms: 60_000,
                    instruction: "later".into(),
                },
            ],
            now,
            &test_logger(),
        );
        assert_eq!(wakes.len(), 2);
        // Slightly after `now`: the 0ms wake is due, the 60s one is not.
        let due = drain_due_wakes(&mut wakes, now + Duration::from_millis(1));
        assert_eq!(due, vec!["now".to_string()]);
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].1, "later");
    }
}
