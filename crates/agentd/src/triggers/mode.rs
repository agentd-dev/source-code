// SPDX-License-Identifier: Apache-2.0
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
#[cfg(feature = "hot-reload")]
use crate::subagent::protocol::SwapIntel;
use crate::subagent::protocol::{SeedMessage, SpawnPayload};
use crate::supervisor::reactor::{SuperviseResult, supervise_once};
use crate::supervisor::restart::{RestartAction, RestartConfig, RestartGovernor};
use crate::triggers::router::{Condition, Disposition, Route, Router};
use crate::triggers::warm::WarmRegistry;
use crate::wire::mcp::method;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Poll cadence for draining MCP notifications + firing due deliveries.
const TICK: Duration = Duration::from_millis(200);
/// Default per-URI debounce (RFC 0008).
const DEBOUNCE: Duration = Duration::from_millis(250);

/// A lease this replica currently holds for a claim route (RFC 0019 §3.2).
/// Carried so the post-react ack/release, the renew heartbeat, and the drain
/// step-1.5 release have the lease id + the dedupe key + the TTL cadence,
/// regardless of whether the claim is spawn-style or continue-style.
///
/// The `held_claims` registry mixes two key spaces (reconciled cleanly — the
/// struct carries everything needed to ack/release either way):
///   * **spawn-claim** entries are keyed by the route **URI** and live for at
///     most one deliver iteration (claimed→settled inline). Their heartbeat
///     fields are never consulted (settled before the tick's renew pass).
///   * **continue-claim** entries are keyed by the warm **session id** and live
///     for the session's whole life (many deliveries); the renew heartbeat
///     keeps the lease (`last_renew`), and they settle when the session ENDS.
#[cfg(feature = "cluster")]
struct HeldClaim {
    /// NAME of the coordination server in the name-keyed `servers` map. A name is
    /// stable across a hot reload (RFC 0017 §5.3); a positional index would shift
    /// when an earlier server is removed, silently mis-routing the ack/release.
    server: String,
    /// The opaque lease id `work.claim` granted (for renew/ack/release).
    lease_id: String,
    /// The item-derived claim key (== the spawned reaction's RUN_ID), carried on
    /// `work.ack._meta.agent/claim_key` so the server collapses the ack.
    claim_key: String,
    /// The requested lease TTL — the heartbeat renews at `ttl * renew_fraction`.
    ttl: Duration,
    /// The heartbeat cadence fraction of the TTL (RFC 0019 §3.6, default 0.33).
    renew_fraction: f64,
    /// When the lease was last claimed/renewed — the heartbeat compares
    /// `now - last_renew >= ttl * renew_fraction` each tick (continue-claims).
    last_renew: Instant,
}

/// Build the frozen `work.*` `_meta` for a claim call (RFC 0015 §5.6). The ONLY
/// keys are `agent/claim_key`, `agent/instance`, `agent/shard` (omitted when
/// unsharded), and `traceparent` (omitted when absent). **No secret, no URL** —
/// the item URI is a `work.claim` argument, never a `_meta` value.
#[cfg(feature = "cluster")]
fn claim_meta(cfg: &Config, claim_key: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert(
        "agent/claim_key".into(),
        Value::String(claim_key.to_string()),
    );
    if let Some(instance) = crate::identity::Identity::from_env(&cfg.run_id).instance {
        m.insert("agent/instance".into(), Value::String(instance));
    }
    if let Some(shard) = cfg.shard.label() {
        m.insert("agent/shard".into(), Value::String(shard));
    }
    if let Some(tp) = &cfg.traceparent {
        m.insert("traceparent".into(), Value::String(tp.clone()));
    }
    Value::Object(m)
}

/// Reactive mode: subscribe to the configured resources and act on updates
/// until SIGTERM. `base` is the root payload whose `instruction` is the
/// standing task; each reaction adds the changed resource as context.
///
/// `args`/`env` are the process's original argv (sans program name) + env — the
/// FIXED env/flag layers a hot reload (RFC 0017 §5) re-merges the new FILE over.
/// They are only ever consulted under the `hot-reload` feature; without it the
/// reload latch is never set and this loop is byte-for-byte the pre-reload path.
///
/// `live_config` (present only with `serve-mcp`) is the served
/// `agentd://config/effective` view's live handle (RFC 0017 §4.2 / §5.6): on an
/// APPLIED hot reload this loop swaps the new config into it (so a served read
/// reflects the reload) and pushes `resources/updated` to subscribers. It is
/// `None` when `--serve-mcp` is not configured, and is only acted on with the
/// `hot-reload` feature — so without serve-mcp or without hot-reload it is inert.
// `not(hot-reload)` already covers the serve-mcp-without-hot-reload case (where
// `live_config` is the unused param), so a single attr suffices — a second
// `all(serve-mcp, not(hot-reload))` attr would be a clippy::duplicated-attributes
// in the `a2a` (serve-mcp, no hot-reload) combo.
#[cfg_attr(not(feature = "hot-reload"), allow(unused_variables))]
pub fn run_reactive(
    exe: PathBuf,
    base: SpawnPayload,
    cfg: &Config,
    args: &[String],
    env: &[(String, String)],
    log: &Logger,
    #[cfg(feature = "serve-mcp")] live_config: Option<
        std::sync::Arc<crate::mcp::server::LiveConfig>,
    >,
) -> i32 {
    // The supervisor owns the MCP connections used for subscriptions + reads.
    // (Each spawned reaction connects its own MCP for tool use, via the payload.)
    //
    // NAME-KEYED (RFC 0017 §5.3): `servers` maps a server's NAME → its connected
    // `McpClient`, and `owner` maps a subscription URI → the owning server's NAME.
    // A name is stable across an add/remove/edit hot reload, where a positional
    // `Vec` index would shift the moment an earlier server is removed — silently
    // corrupting `owner` and the claim wiring. Lookups are by-name throughout.
    // `server_order` preserves the config declaration order so the "first server
    // that supports subscribe" choice (subscription arming) is byte-for-byte the
    // pre-refactor insertion-order choice, not the BTreeMap's name-sorted order.
    let mut servers: BTreeMap<String, McpClient> = BTreeMap::new();
    // Mutable so a hot reload can append an added server / drop a removed one
    // while keeping the declaration-order "first supporting server" choice stable.
    #[cfg_attr(not(feature = "hot-reload"), allow(unused_mut))]
    let mut server_order: Vec<String> = Vec::new();
    for spec in &cfg.mcp_servers {
        match crate::mcp::from_spec(spec, Duration::from_secs(60))
            .and_then(|mut c| c.initialize().map(|()| c))
        {
            Ok(c) => {
                log.info(
                    "mcp.connect",
                    json!({"server": spec.name, "subscribe": c.capabilities().supports_subscribe()}),
                );
                servers.insert(spec.name.clone(), c);
                server_order.push(spec.name.clone());
            }
            Err(e) => {
                // Frozen §4.3 `agentd_mcp_connect_failures_total{server}` — the
                // daemon's startup connect runs in THIS (supervisor) process, so
                // the bump reaches the scrape (the child-side connect is process-
                // local; see the metrics.rs caveat).
                crate::obs::metrics::record_mcp_connect_failure(&spec.name);
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
    #[cfg_attr(not(feature = "hot-reload"), allow(unused_mut))]
    let mut claim_by_uri: HashMap<String, crate::cluster::ClaimSpec> = {
        use std::collections::HashSet;
        let mut map: HashMap<String, crate::cluster::ClaimSpec> = HashMap::new();
        let mut validated: HashSet<String> = HashSet::new();
        for route in &cfg.claim_routes {
            let Some(coord) = servers.get(&route.server) else {
                // Unreachable in practice (config validated it), but never panic.
                log.error(
                    "claim.server_missing",
                    json!({"uri": route.uri, "server": route.server}),
                );
                return exit::USAGE;
            };
            if validated.insert(route.server.clone()) {
                // Live, post-handshake predicate (one list_tools per distinct
                // coordination server). A transport failure here is a down server
                // → exit 6 (retriable); up-but-missing-the-tools → exit 2.
                match coord.list_tools() {
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
                    server: route.server.clone(),
                    ttl: cfg.claim_ttl,
                    renew_fraction: cfg.claim_renew_fraction,
                    style: route.style,
                    route_id: route.uri.clone(),
                    continue_session: route.continue_session,
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
    // (in config declaration order) that supports it; track which server NAME
    // owns each URI so we read it back from the same place. Name-keyed `owner`
    // (URI → server name) is stable across a reload (RFC 0017 §5.3).
    let mut owner: HashMap<String, String> = HashMap::new();
    for uri in cfg.subscribe.iter().chain(&cfg.continue_subscribe) {
        let mut armed = false;
        for name in &server_order {
            let s = &servers[name];
            if s.capabilities().supports_subscribe() {
                match s.subscribe(uri) {
                    Ok(()) => {
                        owner.insert(uri.clone(), name.clone());
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

    // Hot-reload working state (RFC 0017 §5). `base` carries the live reloadable
    // values stamped into each reaction's payload (model/limits/log_level); a
    // reload swaps them so the NEXT `reactive_payload` uses them. `running` is the
    // diff baseline (the config currently in effect). Both are `mut` only under
    // the `hot-reload` feature — without it the loop never touches them, so the
    // no-reload path is unchanged. `generation` counts applied reloads (the
    // `agentd_config_generation` gauge, RFC 0017 §5.6).
    #[cfg(feature = "hot-reload")]
    let mut base = base;
    #[cfg(feature = "hot-reload")]
    let mut running: Config = cfg.clone();
    #[cfg(feature = "hot-reload")]
    let mut generation: u64 = 0;

    // Arm the inotify file-watch reload trigger (RFC 0017 §5.2) when
    // `--watch-config` is set on a `config-watch` build. The watcher runs on its
    // own thread for the process's life and sets the SAME RELOAD latch SIGHUP
    // does (attributed `trigger:"watch"`), so it funnels into the identical reload
    // routine below — one code path. Config validation already guaranteed a config
    // file is present when `watch_config` is set (exit 2 otherwise), so the path
    // resolves here.
    #[cfg(all(unix, feature = "config-watch"))]
    if cfg.watch_config
        && let Some(path) = config_path_of(args, env)
    {
        crate::config_watch::spawn_config_watcher(Path::new(&path), log);
    }

    // ── Reactive WORKFLOW (pivot Phase 7 follow-up) ──────────────────────────
    // The base payload may carry a workflow with REACTIVE semantics: drive it
    // now; a Wait SUSPENDS the child, whose serialized slice we hold while the
    // watch is armed through the normal subscription machinery. The daemon's
    // lifetime becomes the workflow's lifetime: terminal workflow → exit with
    // its projected code (an event-loop workflow — a back-edge into a Wait —
    // runs indefinitely).
    #[cfg(feature = "workflow")]
    let mut wf_suspended: Option<(crate::graph::GraphState, String, Instant)> = None;
    #[cfg(feature = "workflow")]
    if base.workflow.is_some() && base.workflow_reactive {
        log.info("workflow.reactive.start", json!({}));
        crate::graph::live::publish(json!({"status": "driving"}));
        let o = react(&exe, &base, cfg.drain_timeout, log);
        if let Some(code) = handle_workflow_outcome(
            o,
            &mut wf_suspended,
            &mut router,
            &mut owner,
            &servers,
            &server_order,
            log,
        ) {
            return code;
        }
    }

    loop {
        crate::obs::health::tick();

        // Hot-reload routine (RFC 0017 §5.3): on a tick where a SIGHUP-set RELOAD
        // is pending AND we are not draining (drain wins, §5.2), run the bounded
        // validate-first/quiesce/apply choreography, then clear the latch. A
        // rejected reload is a clean no-op (the running config is byte-for-byte
        // unchanged). Gated on `hot-reload`; off-feature `reload_requested()` is a
        // const `false`, so this whole block is dead-code-eliminated and the
        // no-reload path is identical to before.
        #[cfg(feature = "hot-reload")]
        if signals::reload_requested() && !signals::draining() {
            // Belt-and-suspenders for liveness (RFC 0016 §10): the reload apply is a
            // multi-call reactor-thread section (re-handshake list_tools, connect,
            // subscribe, claim re-validate). Each call is now SHORT-bounded, but a
            // run of several back-to-back could still approach the staleness window —
            // so refresh the heartbeat immediately before entering it. Cheap (one
            // relaxed store); the short per-call timeout is the real fix.
            crate::obs::health::tick();
            if let Some(new_cfg) = apply_reload(
                args,
                env,
                &running,
                &mut base,
                &mut router,
                &mut owner,
                &mut servers,
                &mut server_order,
                #[cfg(feature = "cluster")]
                &mut claim_by_uri,
                &mut generation,
                log,
            ) {
                // RFC 0018 §5.2: an APPLIED reload whose diff touches the
                // intelligence endpoint list / model / swap policy must REACH
                // in-flight work — `apply_value_swaps` already repointed the spawn
                // template (NEW spawns), but live children (warm `--continue`
                // sessions + served runs) need the swap fanned to them. Build the
                // swap frame from the new config and fan it BEFORE `running` is
                // reassigned (we diff `running` → `new_cfg`).
                let swap_needed = intel_swap_needed(&running, &new_cfg);
                if swap_needed {
                    let swap = SwapIntel {
                        uri: new_cfg.intelligence.clone().unwrap_or_default(),
                        token: new_cfg.intelligence_token.clone(),
                        model: new_cfg.model.clone(),
                        policy: new_cfg.model_swap,
                    };
                    // The reactive daemon's own warm `--continue` sessions.
                    let warm_reached = warm.fan_swap_intel(&swap, log);
                    // The served runs (warm + async) held by the serve-mcp ctx —
                    // reached through the shared `LiveConfig` (serve-mcp only).
                    #[cfg(feature = "serve-mcp")]
                    let served_reached = live_config
                        .as_ref()
                        .map(|lc| lc.fan_swap_intel(swap.clone()))
                        .unwrap_or(0);
                    #[cfg(not(feature = "serve-mcp"))]
                    let served_reached = 0u64;
                    // The `intel.swap` event (RFC 0018 §8 + the §4.4 notify): the
                    // SUPERVISOR-side audit anchor that feeds the events ring (RFC
                    // 0016). Transport+index only — NO secret, NO URL (the `token`/
                    // endpoint URL never appear in this event, only the model names
                    // + policy + whether the endpoint list changed, §7).
                    let endpoint_change = running.intelligence != new_cfg.intelligence
                        || running.intelligence_token != new_cfg.intelligence_token;
                    let from_model = running.model.clone().unwrap_or_default();
                    let to_model = new_cfg.model.clone().unwrap_or_default();
                    log.info(
                        "intel.swap",
                        json!({
                            "kind": if from_model != to_model { "model" } else { "endpoint" },
                            "model_from": from_model,
                            "model_to": to_model,
                            "endpoint_change": endpoint_change,
                            "policy": new_cfg.model_swap.as_str(),
                            "warm_reached": warm_reached,
                            "served_reached": served_reached,
                        }),
                    );
                }
                // RFC 0017 §5.3 step 5: an `mcp_servers` change DEFINITELY changes
                // the available tool set, so the self-MCP must emit
                // `notifications/tools/list_changed` so the model / agentctl re-reads
                // the tool catalogue. Computed BEFORE `running` is reassigned.
                #[cfg(feature = "serve-mcp")]
                let tools_changed = running.mcp_servers != new_cfg.mcp_servers;
                // APPLIED reload (RFC 0017 §5.6): publish the new config onto the
                // served `agentd://config/effective` view and push
                // `resources/updated` so a subscribed agentctl learns push-style.
                // A REJECTED reload returns `None` and never reaches here, so it
                // fires nothing (the served view stays the prior config). Gated on
                // `serve-mcp` (the served handle only exists then) — without it the
                // local working-config swap below is the sole effect.
                #[cfg(feature = "serve-mcp")]
                if let Some(lc) = &live_config {
                    lc.swap(std::sync::Arc::new(new_cfg.clone()));
                    lc.notify_config_effective_updated();
                    // RFC 0018 §4.4: after a swap, notify `agentd://intelligence`
                    // subscribers to re-read the new endpoint topology / model /
                    // swap policy (notify-then-read; no payload, no secret/URL).
                    if swap_needed {
                        lc.notify_intelligence_updated();
                    }
                    // RFC 0017 §5.3 step 5: the tool catalogue changed → fan
                    // `notifications/tools/list_changed` so peers re-list.
                    if tools_changed {
                        lc.notify_tools_list_changed();
                    }
                }
                running = new_cfg;
            }
            signals::clear_reload();
        }
        // A SIGHUP that arrived while draining is consumed without acting (drain
        // supersedes it — the process is exiting). Clearing keeps the latch tidy.
        #[cfg(feature = "hot-reload")]
        if signals::reload_requested() && signals::draining() {
            signals::clear_reload();
        }

        if signals::draining() {
            // Drain step 1.5 (RFC 0019 §6): release every held claim BEFORE
            // winding down, so a surviving replica re-claims immediately rather
            // than waiting out the lease TTL. Best-effort under a hard WALL-TIME
            // budget (`min(2s, drain_timeout/4)` total): each release is given the
            // REMAINING budget as its per-call timeout, so the budget bounds wall
            // time — not iteration count — even against a hung coordination server
            // (a single release can no longer block the full ~60s per-request
            // timeout; audit Finding 2). A failed/timed-out release is logged +
            // counted, never fatal (the lease TTL is the backstop).
            #[cfg(feature = "cluster")]
            if !held_claims.is_empty() {
                // Belt-and-suspenders for liveness (RFC 0016 §10): the release loop
                // below is a multi-call reactor-thread section; refresh the heartbeat
                // before entering it so a run of short-bounded releases still keeps
                // the daemon live while it drains (the writer thread runs until the
                // final `draining` record). The per-call budget cap is the real fix.
                crate::obs::health::tick();
                let budget = std::cmp::min(Duration::from_secs(2), cfg.drain_timeout / 4);
                let deadline = Instant::now() + budget;
                let total = held_claims.len();
                let mut released = 0usize;
                // The registry mixes key spaces — a spawn-claim is keyed by URI, a
                // continue-claim by session id — but a `HeldClaim` carries the
                // lease id + server regardless, so the release is identical.
                for (key, held) in held_claims.drain() {
                    let now = Instant::now();
                    if now >= deadline {
                        log.warn(
                            "drain.claim_release_budget",
                            json!({"released": released, "total": total}),
                        );
                        break;
                    }
                    crate::obs::metrics::record_claim_released();
                    let Some(coord) = servers.get(&held.server) else {
                        // The coordination server was removed by a prior reload —
                        // its lease is already gone with the process; nothing to
                        // release. Count it as handled and move on (never panic).
                        log.warn(
                            "drain.claim_release_skipped",
                            json!({"key": key, "server": held.server, "reason": "server_removed"}),
                        );
                        continue;
                    };
                    // Cap this release at the REMAINING budget (never longer than
                    // the management timeout), so one hung release cannot blow the
                    // whole drain budget — the deadline now bounds WALL TIME.
                    let per_call =
                        std::cmp::min(crate::obs::health::management_timeout(), deadline - now);
                    match crate::cluster::claim::release_within(
                        coord,
                        &held.lease_id,
                        "draining",
                        per_call,
                    ) {
                        Ok(()) => {
                            released += 1;
                            log.info("claim.released", json!({"key": key, "reason": "draining"}));
                        }
                        Err(e) => log.warn(
                            "drain.claim_release_failed",
                            json!({"key": key, "lease": held.lease_id, "err": e}),
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
            for (uri, name) in &owner {
                if let Some(s) = servers.get(name) {
                    // Best-effort, SHORT-bounded (RFC 0016 §10): a slow server must
                    // not stall the drain (the unsubscribe is on the reactor thread).
                    let _ = s.unsubscribe_within(uri, crate::obs::health::management_timeout());
                }
            }
            // Frozen §4.3 `agentd_drains_total{phase="completed"}` — the reactive
            // daemon finished its graceful wind-down (warm sessions cancelled,
            // claims released, subscriptions dropped) in THIS (supervisor) process.
            // The block runs once before returning, so this counts each drain once.
            crate::obs::metrics::record_drain("completed");
            log.info("proc.exit", json!({"reason": "drain", "mode": "reactive"}));
            return exit::SUCCESS;
        }
        let now = Instant::now();

        // Drain inbound notifications → feed the router. A `resources/updated`
        // arms its route (notify-then-read fires the reaction below). A backing
        // server's `tools/list_changed` is OBSERVED (pivot Phase 5.2): every fresh
        // SPAWN reaction already re-enumerates tools per run (`Session::prepare`
        // re-lists), so a changed tool set is picked up by the next reaction with no
        // extra wiring — the log makes the re-enumeration auditable. (Live refresh of
        // an already-running WARM continue-session's catalogue crosses the process
        // boundary — it reuses the same daemon→child inject the workflow Wait uses,
        // Phase 7 — and is a documented follow-up, not a silent drop.)
        for s in servers.values() {
            for n in s.drain_notifications() {
                if n.method == method::NOTIFY_RESOURCES_UPDATED
                    && let Some(uri) = updated_uri(&n.params)
                    && in_shard(&uri)
                    && router.on_updated(&uri, now)
                {
                    log.info("resource.updated", json!({"uri": uri}));
                } else if n.method == method::NOTIFY_TOOLS_LIST_CHANGED {
                    // Next reaction re-lists; warm-session live refresh is Phase 7.
                    log.info("mcp.tools_list_changed", json!({}));
                }
            }
        }

        // Fire due (debounced) deliveries: notify-then-read, then react.
        for delivery in router.due(now) {
            let content = read_current(&servers, &owner, &delivery.uri).unwrap_or_default();
            // A suspended reactive WORKFLOW's wait resolving: consume this
            // delivery as the resume trigger (never a normal reaction).
            #[cfg(feature = "workflow")]
            if wf_suspended
                .as_ref()
                .is_some_and(|(_, uri, _)| *uri == delivery.uri)
            {
                let (state, uri, _) = wf_suspended.take().expect("checked");
                disarm_workflow_wait(&uri, &mut router, &mut owner, &servers, log);
                let value = serde_json::from_str::<serde_json::Value>(&content)
                    .unwrap_or(serde_json::Value::String(content.clone()));
                let o = resume_workflow(
                    &exe,
                    &base,
                    state,
                    false,
                    Some(value),
                    cfg.drain_timeout,
                    log,
                );
                match handle_workflow_outcome(
                    o,
                    &mut wf_suspended,
                    &mut router,
                    &mut owner,
                    &servers,
                    &server_order,
                    log,
                ) {
                    Some(code) => return code,
                    None => continue,
                }
            }
            // Condition predicate (pivot Phase 5.2): a conditional route/subscription
            // fires ONLY when the freshly-read content satisfies the predicate.
            // Notify-then-read means the content is authoritative here (not the stale
            // notification). An unmet condition drops THIS delivery but leaves the
            // subscription armed for the next update. This is the daemon half of
            // `subagent.await_resource`: the agent parks on a resource state and only
            // wakes when it is reached.
            if !condition_met(&delivery.condition, &content) {
                log.debug(
                    "trigger.condition_unmet",
                    json!({"uri": delivery.uri, "bytes": content.len()}),
                );
                continue;
            }
            crate::obs::metrics::record_reaction();
            match delivery.disposition {
                Disposition::Spawn => {
                    // A fresh, independent reaction per event (synchronous v1).
                    // `mut` is needed only in a `cluster` build (the RUN_ID
                    // narrowing override below the claim gate).
                    #[cfg_attr(not(feature = "cluster"), allow(unused_mut))]
                    let mut payload = reactive_payload(&base, &delivery.uri, &content);

                    // CLAIM GATE (RFC 0019 §3.4), `cluster`-gated: for a SPAWN
                    // claim route, claim the item BEFORE spawning and proceed only
                    // on a granted lease. The spawned reaction then runs with the
                    // item-derived RUN_ID so every downstream side-effect dedupes
                    // on the same key (RFC 0019 §3.5 / RFC 0011 §6.2). A
                    // continue-claim route is handled in the Continue arm (its URI
                    // routes there, never here) — guard it anyway so the spawn
                    // path stays exclusively spawn-claims.
                    #[cfg(feature = "cluster")]
                    if let Some(spec) = claim_by_uri.get(&delivery.uri)
                        && !spec.continue_session
                    {
                        let claim_key =
                            crate::cluster::derive_claim_key(&delivery.uri, &spec.route_id);
                        let meta = claim_meta(cfg, &claim_key);
                        // Resolve the coordination server by NAME (stable across a
                        // reload). Absent ⇒ a reload removed it; skip the delivery
                        // (the claim route is unsatisfiable), never panic.
                        let Some(coord) = servers.get(&spec.server) else {
                            log.error(
                                "claim.server_missing",
                                json!({"uri": delivery.uri, "server": spec.server}),
                            );
                            continue;
                        };
                        match crate::cluster::claim_styled(
                            coord,
                            spec.style,
                            &delivery.uri,
                            spec.ttl,
                            meta,
                        ) {
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
                                        server: spec.server.clone(),
                                        lease_id,
                                        claim_key: claim_key.clone(),
                                        ttl: spec.ttl,
                                        renew_fraction: spec.renew_fraction,
                                        last_renew: Instant::now(),
                                    },
                                );
                                // RUN_ID narrowing (RFC 0019 §3.5): the child
                                // stamps `_meta.agent/run_id` from this field
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
                        apply_effects(
                            o,
                            &mut wakes,
                            &mut router,
                            &mut owner,
                            &servers,
                            &server_order,
                            log,
                        );
                    }
                }
                Disposition::Continue(session_id) => {
                    // Deliver into the one warm session for this route (spawn it
                    // on the first event, inject thereafter). Non-blocking — the
                    // session's turn outcomes are drained below.
                    #[cfg_attr(not(feature = "cluster"), allow(unused_mut))]
                    let mut payload = reactive_payload(&base, &delivery.uri, &content);
                    let event = changed_message(&delivery.uri, &content);

                    // CONTINUE-CLAIM GATE (RFC 0019 §3.4), `cluster`-gated. A
                    // continue-claim route claims the item BEFORE delivering into
                    // the warm session, and HOLDS the lease for the session's life
                    // (keyed by the session id, not the URI — a warm session spans
                    // many deliveries). Mirrors the Spawn-arm gate, but the held
                    // claim is settled when the session ENDS (terminal/drain), not
                    // per delivery; the renew heartbeat (below) keeps a long
                    // session's lease alive. A session that is already live holds
                    // its lease → deliver directly (no re-claim).
                    #[cfg(feature = "cluster")]
                    if let Some(spec) = claim_by_uri.get(&delivery.uri)
                        && spec.continue_session
                        && !held_claims.contains_key(&session_id)
                    {
                        let claim_key =
                            crate::cluster::derive_claim_key(&delivery.uri, &spec.route_id);
                        let meta = claim_meta(cfg, &claim_key);
                        // Resolve the coordination server by NAME (stable across a
                        // reload). Absent ⇒ a reload removed it; skip the delivery.
                        let Some(coord) = servers.get(&spec.server) else {
                            log.error(
                                "claim.server_missing",
                                json!({"uri": delivery.uri, "server": spec.server, "session": session_id}),
                            );
                            continue;
                        };
                        match crate::cluster::claim_styled(
                            coord,
                            spec.style,
                            &delivery.uri,
                            spec.ttl,
                            meta,
                        ) {
                            crate::cluster::ClaimOutcome::Lost { held_by } => {
                                crate::obs::metrics::record_claim_lost();
                                log.info(
                                    "claim.lost",
                                    json!({"uri": delivery.uri, "held_by": held_by, "session": session_id}),
                                );
                                continue; // another replica owns it — skip.
                            }
                            crate::cluster::ClaimOutcome::Error(e) => {
                                // A failed claim never kills the daemon (RFC 0019
                                // §8 row 6): skip this delivery, keep serving.
                                log.error(
                                    "claim.error",
                                    json!({"uri": delivery.uri, "err": e, "session": session_id}),
                                );
                                continue;
                            }
                            crate::cluster::ClaimOutcome::Granted {
                                lease_id,
                                expires_in_ms,
                            } => {
                                crate::obs::metrics::record_claim_granted();
                                log.info(
                                    "claim.granted",
                                    json!({"uri": delivery.uri, "expires_in_ms": expires_in_ms, "session": session_id}),
                                );
                                // Key the held claim by the SESSION id (it outlives
                                // this delivery), carrying the TTL cadence so the
                                // heartbeat can renew it while the session is live.
                                held_claims.insert(
                                    session_id.clone(),
                                    HeldClaim {
                                        server: spec.server.clone(),
                                        lease_id,
                                        claim_key: claim_key.clone(),
                                        ttl: spec.ttl,
                                        renew_fraction: spec.renew_fraction,
                                        last_renew: Instant::now(),
                                    },
                                );
                                // RUN_ID narrowing (RFC 0019 §3.5): the warm
                                // session stamps every side-effect `_meta.agent/
                                // run_id` from this field, so a redelivered item
                                // dedupes on the same item-derived claim key.
                                payload.telemetry.run_id = claim_key;
                            }
                        }
                    }

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
        // self-schedule / self-subscribe, applied like a Spawn reaction's. A
        // session that ENDED this pass settles its continue-held claim (RFC 0019
        // §3.4): a terminal `completed` acks, anything else releases.
        let warm_drained = warm.drain(log);
        for (_session, outcome) in warm_drained.turns {
            apply_effects(
                outcome,
                &mut wakes,
                &mut router,
                &mut owner,
                &servers,
                &server_order,
                log,
            );
        }
        #[cfg(feature = "cluster")]
        for (session_id, terminal) in warm_drained.ended {
            if let Some(held) = held_claims.remove(&session_id) {
                settle_session_claim(&servers, &held, terminal, &session_id, log);
            }
        }
        #[cfg(not(feature = "cluster"))]
        let _ = warm_drained.ended;

        // Renew heartbeat (RFC 0019 §3.3 / §8 row 7): keep every still-held
        // (continue) claim's lease alive while its warm session runs. Cheap — a
        // timestamp compare per held claim, a `work.renew` only on the cadence
        // boundary (`ttl * renew_fraction`). A spawn-claim is settled inline
        // before this pass, so only live continue-holds are ever renewed.
        #[cfg(feature = "cluster")]
        if !held_claims.is_empty() {
            renew_held_claims(&servers, &mut held_claims, log);
        }

        // A suspended reactive WORKFLOW whose wait timed out: resume it on the
        // `timeout` edge (the wait's own clock, independent of any update).
        #[cfg(feature = "workflow")]
        if wf_suspended
            .as_ref()
            .is_some_and(|(_, _, deadline)| now >= *deadline)
        {
            let (state, uri, _) = wf_suspended.take().expect("checked");
            disarm_workflow_wait(&uri, &mut router, &mut owner, &servers, log);
            log.info("workflow.wait.timeout", json!({"on_uri": uri}));
            let o = resume_workflow(&exe, &base, state, true, None, cfg.drain_timeout, log);
            if let Some(code) = handle_workflow_outcome(
                o,
                &mut wf_suspended,
                &mut router,
                &mut owner,
                &servers,
                &server_order,
                log,
            ) {
                return code;
            }
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
                apply_effects(
                    o,
                    &mut wakes,
                    &mut router,
                    &mut owner,
                    &servers,
                    &server_order,
                    log,
                );
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

/// The hot-reload choreography (RFC 0017 §5.3), run on the reactor thread when a
/// SIGHUP-set RELOAD is pending (and not draining). Validate-first, quiesce,
/// all-or-nothing. Returns `Some(new_running)` when the reloadable diff was
/// applied (the caller adopts it as the new baseline), or `None` when the reload
/// was rejected — a clean, byte-for-byte no-op (RFC 0017 §7): the running config,
/// `base`, `router`, `owner`, and `servers` are all left exactly as they were.
///
/// `mcp_servers` is now **reloadable** (RFC 0017 §5.1 / §5.3 step 4): a validated
/// reload re-handshakes the MCP server set at this quiesce boundary — removed
/// servers are stopped+reaped (their `McpClient` Drop runs the stdio shutdown
/// ladder), added servers are spawned+handshaked+subscribed (with the MANDATORY
/// read-after-subscribe), changed servers are remove-then-add. The name-keyed
/// `servers`/`owner` map (and name-keyed `claim_by_uri`) makes this safe: a
/// remove/add never shifts another server's key, so `owner` and the claim wiring
/// stay coherent. A failed *add* is contained (logged `mcp.connect.fail`, the
/// server is simply absent — RFC 0007 tool-domain absence) and never aborts the
/// daemon or rolls back already-applied sub-steps (§5.3 contained-failure).
#[cfg(feature = "hot-reload")]
#[allow(clippy::too_many_arguments)]
fn apply_reload(
    args: &[String],
    env: &[(String, String)],
    running: &Config,
    base: &mut SpawnPayload,
    router: &mut Router,
    owner: &mut HashMap<String, String>,
    servers: &mut BTreeMap<String, McpClient>,
    server_order: &mut Vec<String>,
    #[cfg(feature = "cluster")] claim_by_uri: &mut HashMap<String, crate::cluster::ClaimSpec>,
    generation: &mut u64,
    log: &Logger,
) -> Option<Config> {
    let started = Instant::now();
    // Trigger attribution (RFC 0017 §5.6 — `{trigger:"sighup"|"watch"}`). The
    // file-watch thread sets a watch-attribution flag alongside the RELOAD latch;
    // take-and-clear it here to label this reload. Defaults to "sighup" (the
    // SIGHUP handler / a programmatic `request_reload` never set the flag).
    let trigger = if signals::take_reload_was_watch() {
        "watch"
    } else {
        "sighup"
    };
    log.info("config.reload_requested", json!({"trigger": trigger}));

    // STEP 1 — re-load + re-validate (pure-CPU, no side effect). Re-read ONLY the
    // FILE, re-merge built-in<file<env<flag. `Config::reload` runs the FULL
    // `validate()` pipeline, so a now-invalid file is a `Usage` error here.
    //
    // The lethal-trifecta gate now lives in `validate()` (RFC 0012 §3.2 / RFC 0017
    // §7 — the one validation authority), so a reload whose NEW config forms a
    // complete trifecta WITHOUT `--allow-trifecta` is refused right here. Because
    // `--allow-trifecta` is process-global (flag/env, never file-settable) the
    // running daemon could NOT have started with an un-allowed trifecta, so any
    // such refusal on reload is, by construction, a NEWLY-formed trifecta (e.g. a
    // reload that adds an egress/sensitive-tagged `mcp_servers` entry). Relabel it
    // from the generic "invalid" to `reason="trifecta_required"` — mirroring the
    // `restart_required` path — so agentctl can route it (the live widening of a
    // security-critical capability is refused without a restart + the override).
    let mut new_cfg = match Config::reload(args, env) {
        Ok(c) => c,
        Err(e) => {
            let msg = e.to_string();
            reject(log, generation, reload_reject_reason(&msg), None, &msg);
            return None; // no-op — running config kept verbatim
        }
    };
    // Process identity is restart-only and MUST be stable for the process's life
    // (RFC 0017 §5.1 / RFC 0011 §6). When no explicit `--run-id`/`AGENTD_RUN_ID`
    // is set, `load` MINTS a fresh run id each call (time+pid) — that is not a
    // config change, it is the same auto-identity re-rolled. Pin the candidate's
    // run id to the running one when there is no explicit source, so a reload
    // never spuriously trips the restart-only `run_id` diff. An EXPLICIT run id
    // that genuinely changed is still (correctly) a restart-only reject.
    if !run_id_explicit(args, env) {
        new_cfg.run_id = running.run_id.clone();
    }
    // The reload-coherence check (RFC 0017 §5.4): internal consistency of the
    // reloadable subset. `file_present` is implied (a reload only matters with a
    // file), but pass it honestly so the restart-only-in-file warnings surface.
    let file_present = config_file_present(args, env);
    if let Err(diags) = Config::reload_coherence_check(&new_cfg, Some(running), file_present) {
        // STEP 2's restart-only diff and the §5.4 consistency errors both land
        // here. Name the first error's field + reason so agentctl can route it
        // (a restart-only diff → roll a restart; an inconsistency → fix the file).
        let first = diags.iter().find(|d| d.is_error());
        let (reason, field, msg) = match first {
            Some(d) if d.msg.contains("restart-only") => {
                ("restart_required", Some(d.field.clone()), d.msg.clone())
            }
            Some(d) => ("invalid", Some(d.field.clone()), d.msg.clone()),
            None => ("invalid", None, "reload rejected".to_string()),
        };
        reject(log, generation, reason, field, &msg);
        return None; // no-op
    }

    // Trifecta widening audit (RFC 0012 §3.2): with `--allow-trifecta` set, a
    // reload CAN legitimately land a complete trifecta that the running config did
    // not hold (e.g. it adds the egress leg via a tagged `mcp_servers` entry).
    // `validate()` (run inside `Config::reload` above) already PERMITTED it
    // (downgraded the refusal to a warning), so it reaches here — but the live
    // widening must still be recorded, mirroring the startup `scope.trifecta_grant`
    // event the supervisor emits for an allowed root trifecta. Emit it only on the
    // running→new TRANSITION into a complete trifecta, so a steady-state reload of
    // an already-allowed trifecta config does not re-log.
    {
        use crate::sec::scope::{TrifectaVerdict, check_trifecta};
        let now = check_trifecta(new_cfg.trifecta_grant_tags(), new_cfg.allow_trifecta);
        let before = check_trifecta(running.trifecta_grant_tags(), running.allow_trifecta);
        if now == TrifectaVerdict::AllowedWithWarning
            && before != TrifectaVerdict::AllowedWithWarning
        {
            log.warn(
                "scope.trifecta_grant",
                json!({
                    "allowed": true,
                    "legs": ["untrusted_input", "sensitive", "egress"],
                    "via": "reload",
                }),
            );
        }
    }

    // Both pure-CPU gates passed. Compute the reloadable diff (what changed) for
    // the success event's `changed` list. Restart-only fields cannot differ here
    // (the coherence check would have rejected), so only reloadable fields remain.
    let changed = reloadable_changes(running, &new_cfg);
    if changed.is_empty() {
        // A reload with no reloadable change is still a successful no-op apply
        // (the file may have been touched without a material change). Report it as
        // applied so the generation advances and agentctl sees the push landed.
        log.info(
            "config.reloaded",
            json!({"changed": [], "applied_ms": started.elapsed().as_millis() as u64}),
        );
        *generation += 1;
        crate::obs::metrics::record_config_reload("applied");
        crate::obs::metrics::set_config_generation(*generation);
        return Some(new_cfg);
    }

    // STEP 3 — QUIESCE. In reactive mode the quiesce point IS this idle moment
    // between routed deliveries (we run at the top of the tick, before any
    // delivery is dispatched). Set the tree-wide `reloading` guard so the served
    // `subagent.spawn` chokepoint transiently refuses NEW spawns (cleared in step
    // 6). We do NOT cancel in-flight work — a synchronous reaction, if one were
    // mid-flight, would already have returned before this tick boundary.
    signals::set_reloading(true);

    // STEP 4 — APPLY the reloadable diff (idempotent, ordered, all-or-nothing on
    // what validated; contained runtime failure). value-swaps first (lowest
    // risk), then the MCP server re-handshake (so an added server is connected
    // BEFORE the subscription reconcile can bind a new URI to it / before a claim
    // route re-resolves to it), then the subscription reconcile (read-after-
    // subscribe on adds, RFC 0017 §5.3), then (cluster) the claim re-resolution.
    apply_value_swaps(base, &new_cfg, log);
    apply_mcp_server_diff(running, &new_cfg, servers, server_order, owner, router, log);
    apply_subscription_diff(running, &new_cfg, router, owner, servers, server_order, log);
    // Under `cluster`, the claim wiring (`claim_by_uri`) binds a route URI →
    // coordination server NAME → its live client. After the server set changed,
    // re-resolve it against the post-reload `servers` and re-validate each claim
    // coordination server still advertises `work.*` (RFC 0019 §3). A now-absent or
    // no-longer-advertising claim server does NOT exit the daemon on a reload (a
    // reload never kills the daemon, RFC 0017 §5.3); the route is dropped from the
    // wiring and logged, so its deliveries fall through unclaimed (the gentlest
    // correct degradation — the work simply isn't claimed by this replica).
    #[cfg(feature = "cluster")]
    reresolve_claim_routes(&new_cfg, servers, claim_by_uri, log);

    // STEP 5 — self-MCP surface refresh. An `mcp_servers` change DEFINITELY
    // changes the available tool set, so the caller (`run_reactive`) emits
    // `notifications/tools/list_changed` when the server set differs (it holds the
    // served self-MCP handle). The SUBSCRIBABLE served resource
    // `agentd://config/effective` (RFC 0017 §4.2 / §5.6) is likewise refreshed
    // there: the caller swaps the live config + fires `resources/updated` on this
    // applied return, so a subscribed agentctl re-reads the post-reload view.

    // STEP 6 — clear the guard, emit success, bump the generation + metric.
    signals::set_reloading(false);
    *generation += 1;
    log.info(
        "config.reloaded",
        json!({"changed": changed, "applied_ms": started.elapsed().as_millis() as u64}),
    );
    crate::obs::metrics::record_config_reload("applied");
    crate::obs::metrics::set_config_generation(*generation);
    Some(new_cfg)
}

/// Classify a candidate-config `validate()` failure into a reload-reject
/// `reason` (RFC 0017 §5.6). A newly-formed lethal trifecta (refused by the gate
/// now living in `validate()`, RFC 0012 §3.2) gets the dedicated
/// `trifecta_required` reason — mirroring `restart_required` — so agentctl can
/// route it (a security widening needs a restart + `--allow-trifecta`, not a
/// file fix). Everything else is the generic `invalid`. Pure.
#[cfg(feature = "hot-reload")]
fn reload_reject_reason(msg: &str) -> &'static str {
    if msg.contains("lethal-trifecta") {
        "trifecta_required"
    } else {
        "invalid"
    }
}

/// Emit the `config.reload_rejected` event + the `rejected` metric (RFC 0017
/// §5.6 / §7). A rejected reload is a clean no-op; the generation does NOT
/// advance. `field` names the offending field when known.
#[cfg(feature = "hot-reload")]
fn reject(log: &Logger, _generation: &mut u64, reason: &str, field: Option<String>, msg: &str) {
    log.warn(
        "config.reload_rejected",
        json!({"reason": reason, "field": field, "diagnostics": [msg]}),
    );
    crate::obs::metrics::record_config_reload("rejected");
}

/// The reloadable field groups (RFC 0017 §5.1) that differ between `running` and
/// `new` — the success event's `changed` list. Restart-only fields are excluded
/// (the coherence check already proved they are unchanged). Pure.
#[cfg(feature = "hot-reload")]
fn reloadable_changes(running: &Config, new: &Config) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if running.model != new.model
        || running.max_tokens != new.max_tokens
        || running.intelligence_headers != new.intelligence_headers
    {
        changed.push("model");
    }
    if running.max_steps != new.max_steps
        || running.max_depth != new.max_depth
        || running.deadline != new.deadline
    {
        changed.push("limits");
    }
    if running.log_level != new.log_level {
        changed.push("log_level");
    }
    if running.subscribe != new.subscribe {
        changed.push("subscribe");
    }
    // RFC 0017 §5.1: the MCP server inventory is reloadable — a re-handshake at
    // the quiesce boundary (add/remove/edit a server). Diffed by name+command+tags.
    if running.mcp_servers != new.mcp_servers {
        changed.push("mcp_servers");
    }
    // RFC 0018 §5.1: the intelligence endpoint list + swap policy are reloadable.
    // A change repoints NEW spawns (via `apply_value_swaps`) and is fanned to
    // in-flight children as `ctrl/swap_intel` (the caller, `intel_swap_needed`).
    if running.intelligence != new.intelligence
        || running.intelligence_token != new.intelligence_token
        || running.model_swap != new.model_swap
    {
        changed.push("intelligence");
    }
    changed
}

/// Whether a reload's diff touches the hot-swappable intelligence config (RFC
/// 0018 §5.1): the endpoint list, the resolved default credential, the model, or
/// the swap policy. When true, the caller fans `ctrl/swap_intel` to every
/// in-flight child + warm/served run and notifies `agentd://intelligence`. Pure.
#[cfg(feature = "hot-reload")]
fn intel_swap_needed(running: &Config, new: &Config) -> bool {
    running.intelligence != new.intelligence
        || running.intelligence_token != new.intelligence_token
        || running.model != new.model
        || running.model_swap != new.model_swap
}

/// Apply the low-risk value-swaps (RFC 0017 §5.3 step 4) into the working spawn
/// template `base`, so the NEXT reaction's payload uses them. model/max_tokens →
/// the intel + limits blocks; limits.* → the new spawn template; log_level → the
/// child telemetry level (and logged immediately). In-flight children keep their
/// already-minted budgets (§5.5) — only NEW spawns see the new template.
#[cfg(feature = "hot-reload")]
fn apply_value_swaps(base: &mut SpawnPayload, new: &Config, log: &Logger) {
    base.intelligence.model = new.model.clone();
    // RFC 0018 §5.1: repoint the endpoint list + default credential so a NEW spawn
    // (or a warm session re-spawned after this reload) dials the new endpoints. An
    // in-flight child is reached separately by the `ctrl/swap_intel` fan-out — this
    // only updates the working template. The token is swapped but never logged.
    base.intelligence.uri = new.intelligence.clone().unwrap_or_default();
    base.intelligence.token = new.intelligence_token.clone();
    base.limits.max_tokens = new.max_tokens;
    base.limits.max_steps = new.max_steps;
    base.limits.max_depth = new.max_depth;
    base.limits.deadline_ms = new
        .deadline
        .map(|d| d.as_millis() as u64)
        .unwrap_or(315_360_000_000);
    base.telemetry.log_level = new.log_level.as_str().to_string();
    log.info(
        "config.reload.values",
        json!({
            "model": new.model,
            "max_tokens": new.max_tokens,
            "max_steps": new.max_steps,
            "max_depth": new.max_depth,
            "log_level": new.log_level.as_str(),
        }),
    );
}

/// Reconcile the declared `subscribe` set across the reload boundary (RFC 0017
/// §5.3 step 4 / §5.5): unsubscribe REMOVED URIs (drop their route + owner);
/// for ADDED URIs subscribe on a supporting server AND read-after-subscribe
/// (MANDATORY — synthesize the initial read so edge→level holds across the
/// reload), adding their route. Unchanged URIs are left untouched. This reuses
/// the already-proven restart reconcile machinery, run at a reload boundary.
#[cfg(feature = "hot-reload")]
fn apply_subscription_diff(
    running: &Config,
    new: &Config,
    router: &mut Router,
    owner: &mut HashMap<String, String>,
    servers: &BTreeMap<String, McpClient>,
    server_order: &[String],
    log: &Logger,
) {
    use std::collections::HashSet;
    let old_set: HashSet<&str> = running.subscribe.iter().map(String::as_str).collect();
    let new_set: HashSet<&str> = new.subscribe.iter().map(String::as_str).collect();

    // REMOVED: unsubscribe + drop the route + owner (and any pending delivery).
    for uri in running.subscribe.iter() {
        if !new_set.contains(uri.as_str()) {
            if let Some(name) = owner.remove(uri)
                && let Some(s) = servers.get(&name)
            {
                // Best-effort, SHORT-bounded (RFC 0016 §10) — on the reactor thread.
                let _ = s.unsubscribe_within(uri, crate::obs::health::management_timeout());
            }
            router.remove_exact(uri);
            log.info("unsubscribe", json!({"uri": uri, "kind": "reload"}));
        }
    }

    // ADDED: subscribe on the first supporting server (config declaration order),
    // add the route, then read-after-subscribe so a change that predates the
    // subscribe isn't missed.
    let now = Instant::now();
    for uri in new.subscribe.iter() {
        if old_set.contains(uri.as_str()) || router.has_exact(uri) {
            continue; // unchanged (or already routed) — leave it
        }
        arm_added_subscription(uri, servers, server_order, owner, router, now, log);
    }
}

/// Arm one newly-added subscription URI: subscribe on the first supporting server
/// (declaration order), add its Spawn route, and do the MANDATORY read-after-
/// subscribe (RFC 0017 §5.3 — convert edge→level across the reload boundary,
/// exactly like startup). Shared by `apply_subscription_diff` (new declared subs)
/// and the orphan re-home path in `apply_mcp_server_diff` (a sub whose owning
/// server was removed but the URI is still wanted). Logs `subscribe.unsupported`
/// when no connected server can serve it.
#[cfg(feature = "hot-reload")]
fn arm_added_subscription(
    uri: &str,
    servers: &BTreeMap<String, McpClient>,
    server_order: &[String],
    owner: &mut HashMap<String, String>,
    router: &mut Router,
    now: Instant,
    log: &Logger,
) {
    match arm_uri_on_first_supporting(servers, server_order, owner, uri) {
        Some(server) => {
            if !router.has_exact(uri) {
                router.add_route(Route::new(uri, Disposition::Spawn, DEBOUNCE));
            }
            log.info(
                "subscribe",
                json!({"uri": uri, "server": server, "kind": "reload"}),
            );
            if router.on_updated(uri, now) {
                log.info(
                    "reactive.initial_read",
                    json!({"uri": uri, "kind": "reload"}),
                );
            }
        }
        None => log.warn(
            "subscribe.unsupported",
            json!({"uri": uri, "kind": "reload"}),
        ),
    }
}

/// Re-handshake the MCP server set across the reload boundary (RFC 0017 §5.3
/// step 4). Diffs `running.mcp_servers` vs `new.mcp_servers` BY NAME (+ command/
/// argv/tags), then applies, ordered, with contained runtime failure:
///
///  * **removed** (in running, not in new): unsubscribe its owned URIs (best-
///    effort), drop the `McpClient` (its Drop runs the stdio shutdown ladder /
///    kill+reap), remove from `servers`/`server_order`. Any URI it owned that is
///    STILL a wanted subscription is RE-HOMED onto a surviving server below.
///  * **changed** (same name, different endpoint/headers/tags): remove-then-add —
///    an endpoint change is a new connection.
///  * **added** (in new, not in running): `McpClient::from_spec` + `initialize` +
///    re-stamp the run-id/traceparent tool `_meta` (matching the connect path);
///    append to `servers`/`server_order`. A failed add is CONTAINED — logged
///    `mcp.connect.fail`, the server simply absent (RFC 0007 tool-domain absence)
///    — it never rolls back applied sub-steps and never aborts the daemon.
///
/// After the set changed, orphaned subscriptions (a still-wanted URI whose owning
/// server was removed) are re-subscribed on a surviving server WITH the mandatory
/// read-after-subscribe, so edge→level holds across the reload. The added
/// servers' OWN subscriptions are armed by the subsequent `apply_subscription_diff`
/// (a newly-declared URI) or by this re-home (an existing URI moving servers).
/// `servers`/`owner` are never left in a state that did not validate (§5.3).
#[cfg(feature = "hot-reload")]
fn apply_mcp_server_diff(
    running: &Config,
    new: &Config,
    servers: &mut BTreeMap<String, McpClient>,
    server_order: &mut Vec<String>,
    owner: &mut HashMap<String, String>,
    router: &mut Router,
    log: &Logger,
) {
    use crate::config::McpServerSpec;
    let by_name = |specs: &[McpServerSpec], name: &str| -> Option<McpServerSpec> {
        specs.iter().find(|s| s.name == name).cloned()
    };
    // REMOVED + CHANGED: a server in `running` that is gone, or whose command/argv/
    // tags differ in `new`, is stopped+reaped (drop runs the shutdown ladder). A
    // changed server is removed here and re-added in the ADD pass below.
    let mut removed_uris: Vec<String> = Vec::new();
    for spec in &running.mcp_servers {
        let want = by_name(&new.mcp_servers, &spec.name);
        let drop_it = match &want {
            None => true,         // removed outright
            Some(n) => n != spec, // changed (command/argv/tags differ)
        };
        if drop_it {
            // Unsubscribe + collect every URI this server owned, so we can re-home
            // the ones still wanted after the set settles. Best-effort unsubscribe.
            let owned: Vec<String> = owner
                .iter()
                .filter(|(_, n)| n.as_str() == spec.name)
                .map(|(u, _)| u.clone())
                .collect();
            for uri in &owned {
                if let Some(s) = servers.get(&spec.name) {
                    // Best-effort, SHORT-bounded (RFC 0016 §10) — on the reactor thread.
                    let _ = s.unsubscribe_within(uri, crate::obs::health::management_timeout());
                }
                owner.remove(uri);
                removed_uris.push(uri.clone());
            }
            // Drop the client (Drop = stdio shutdown ladder / kill+reap, RFC 0004).
            servers.remove(&spec.name);
            server_order.retain(|n| n != &spec.name);
            log.info(
                "mcp.disconnect",
                json!({"server": spec.name, "kind": "reload", "reason": if want.is_none() {"removed"} else {"changed"}}),
            );
        }
    }

    // ADDED + CHANGED(re-add): a server in `new` not currently connected is
    // spawned + handshaked. A failed add is contained (logged + absent), never
    // fatal and never a rollback (RFC 0017 §5.3 contained-failure).
    for spec in &new.mcp_servers {
        if servers.contains_key(&spec.name) {
            continue; // unchanged — leave the live connection untouched
        }
        // The handshake runs on the REACTOR thread mid-reload, so it uses the
        // SHORT management timeout (RFC 0016 §10): a slow-but-alive added server
        // must not block the reactor (and starve the liveness heartbeat) for the
        // full ~60s. A timeout is a contained `mcp.connect.fail` — the server is
        // simply absent (RFC 0007 / RFC 0017 §5.3 contained-failure), never fatal.
        match crate::mcp::from_spec(spec, Duration::from_secs(60)).and_then(|mut c| {
            c.initialize_within(crate::obs::health::management_timeout())
                .map(|()| c)
        }) {
            Ok(mut c) => {
                // Re-stamp the run-id (retry dedup, RFC 0011) + traceparent
                // (tracing, RFC 0010) tool `_meta`, exactly like the connect path.
                // run_id is restart-only (stable for the process's life), so `new`
                // carries the same id the supervisor started with.
                let mut meta = json!({"agent/run_id": new.run_id});
                if let Some(tp) = &new.traceparent {
                    meta["traceparent"] = Value::String(tp.clone());
                }
                c.set_tool_meta(meta);
                log.info(
                    "mcp.connect",
                    json!({"server": spec.name, "kind": "reload", "subscribe": c.capabilities().supports_subscribe()}),
                );
                servers.insert(spec.name.clone(), c);
                server_order.push(spec.name.clone());
            }
            Err(e) => {
                // CONTAINED FAILURE: a failed add is a tool-domain absence (RFC
                // 0007), logged, NOT a rollback, NOT a daemon abort (RFC 0017 §5.3).
                // Frozen §4.3 `agentd_mcp_connect_failures_total{server}` — the
                // hot-reload add runs on the reactor (supervisor) thread.
                crate::obs::metrics::record_mcp_connect_failure(&spec.name);
                log.error(
                    "mcp.connect.fail",
                    json!({"server": spec.name, "kind": "reload", "err": e.to_string()}),
                );
            }
        }
    }

    // RE-HOME orphaned subscriptions: a URI whose owning server was removed but
    // that is STILL declared (or routed) gets re-subscribed on a surviving server
    // WITH read-after-subscribe. A URI that was only on the removed server and is
    // no longer wanted is left dropped (apply_subscription_diff handles declared
    // removals; this covers the unchanged-URI-but-removed-server case).
    let now = Instant::now();
    for uri in removed_uris {
        let still_wanted = new.subscribe.iter().any(|u| u == &uri)
            || new.continue_subscribe.iter().any(|u| u == &uri);
        if !still_wanted {
            // Not declared anymore AND its server is gone — drop its route too, so
            // a stale Spawn route can't fire against a vanished owner.
            router.remove_exact(&uri);
            continue;
        }
        if owner.contains_key(&uri) {
            continue; // already re-homed (e.g. a still-present server also owns it)
        }
        arm_added_subscription(&uri, servers, server_order, owner, router, now, log);
    }
}

/// Re-resolve the claim wiring after an `mcp_servers` reload (RFC 0019 §3, RFC
/// 0017 §5.3): each claim route's coordination server is resolved BY NAME against
/// the post-reload `servers`. A route whose coordination server is now absent — or
/// is present but no longer advertises `work.*` — is DROPPED from `claim_by_uri`
/// and logged. A reload never exits the daemon (unlike startup, where a missing
/// claim server is a hard exit); the gentlest correct degradation is to leave the
/// route unsatisfiable (its deliveries fall through unclaimed by THIS replica —
/// at-least-once + another replica's claim still cover the work). Best-effort.
#[cfg(all(feature = "hot-reload", feature = "cluster"))]
fn reresolve_claim_routes(
    new: &Config,
    servers: &BTreeMap<String, McpClient>,
    claim_by_uri: &mut HashMap<String, crate::cluster::ClaimSpec>,
    log: &Logger,
) {
    use std::collections::HashSet;
    claim_by_uri.clear();
    let mut validated: HashSet<String> = HashSet::new();
    let mut unsatisfiable: HashSet<String> = HashSet::new();
    for route in &new.claim_routes {
        if unsatisfiable.contains(&route.server) {
            continue; // already found gone/incoherent this pass
        }
        let Some(coord) = servers.get(&route.server) else {
            log.warn(
                "claim.coord_missing",
                json!({"uri": route.uri, "server": route.server, "kind": "reload", "degradation": "route_unsatisfiable"}),
            );
            unsatisfiable.insert(route.server.clone());
            continue;
        };
        // Re-validate the coordination predicate once per distinct server. A
        // transport failure or a missing `work.*` advert drops the route (logged),
        // never exits — a reload must not kill the daemon (RFC 0017 §5.3). This
        // runs on the REACTOR thread mid-loop, so it uses the SHORT management
        // timeout (RFC 0016 §10): a slow-but-alive coordination server here must
        // not block the reactor past the liveness window — a timeout just drops the
        // route as unsatisfiable (the gentlest degradation), exactly like an
        // unreachable server.
        if validated.insert(route.server.clone()) {
            match coord.list_tools_within(crate::obs::health::management_timeout()) {
                Ok(tools) if crate::cluster::advertises_work_tools(&tools) => {
                    log.info(
                        "claim.coord_ready",
                        json!({"server": route.server, "kind": "reload", "tools": tools.len()}),
                    );
                }
                Ok(_) => {
                    log.warn(
                        "claim.coord_missing_tools",
                        json!({"server": route.server, "kind": "reload", "degradation": "route_unsatisfiable"}),
                    );
                    unsatisfiable.insert(route.server.clone());
                    continue;
                }
                Err(e) => {
                    log.warn(
                        "claim.coord_unreachable",
                        json!({"server": route.server, "kind": "reload", "err": e.to_string(), "degradation": "route_unsatisfiable"}),
                    );
                    unsatisfiable.insert(route.server.clone());
                    continue;
                }
            }
        }
        claim_by_uri.insert(
            route.uri.clone(),
            crate::cluster::ClaimSpec {
                server: route.server.clone(),
                ttl: new.claim_ttl,
                renew_fraction: new.claim_renew_fraction,
                style: route.style,
                route_id: route.uri.clone(),
                continue_session: route.continue_session,
            },
        );
    }
    if !claim_by_uri.is_empty() {
        log.info(
            "claim.rearmed",
            json!({"routes": claim_by_uri.len(), "kind": "reload"}),
        );
    }
}

/// Whether a config FILE is in play (`--config` / `AGENTD_CONFIG`) — a reload
/// only matters when the FILE (the one mutable layer) can change. Used to scope
/// the restart-only-in-file advisory warnings (RFC 0017 §5.4 check 1). Pure.
#[cfg(feature = "hot-reload")]
fn config_file_present(args: &[String], env: &[(String, String)]) -> bool {
    args.iter().any(|a| a == "--config") || env.iter().any(|(k, _)| k == "AGENTD_CONFIG")
}

/// Resolve the config file path the same way `Config::load` does (`--config`
/// flag wins over `AGENTD_CONFIG`), for arming the inotify watcher (RFC 0017
/// §5.2). Returns `None` when no file is in play. Pure.
#[cfg(all(unix, feature = "config-watch"))]
fn config_path_of(args: &[String], env: &[(String, String)]) -> Option<String> {
    // `--config <PATH>`: the value follows the flag.
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--config" {
            return it.next().cloned();
        }
    }
    env.iter()
        .find(|(k, _)| k == "AGENTD_CONFIG")
        .map(|(_, v)| v.clone())
}

/// Whether the run id was EXPLICITLY set (`--run-id` / `AGENTD_RUN_ID`) rather
/// than auto-minted by `load` (RFC 0011 §6 / RFC 0017 §5). A reload re-runs
/// `load`, which re-mints an auto run id each call; an auto id is therefore not a
/// real config change and is pinned to the running one (see `apply_reload`). An
/// explicit id that genuinely changed remains a (correct) restart-only reject.
#[cfg(feature = "hot-reload")]
fn run_id_explicit(args: &[String], env: &[(String, String)]) -> bool {
    args.iter().any(|a| a == "--run-id") || env.iter().any(|(k, _)| k == "AGENTD_RUN_ID")
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
        // Frozen §4.3 `agentd_drains_total{phase="completed"}` (supervisor process;
        // each drain-exit path is mutually exclusive, so a daemon drains once).
        crate::obs::metrics::record_drain("completed");
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
            // Frozen §4.3 `agentd_drains_total{phase="completed"}` (supervisor).
            crate::obs::metrics::record_drain("completed");
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
                // Frozen §4.3 `agentd_drains_total{phase="completed"}` (supervisor).
                crate::obs::metrics::record_drain("completed");
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
            RestartAction::Backoff(d) => {
                // Frozen §4.3 `agentd_subagent_restarts_total{reason}` — the
                // `loop`/`schedule` governor is about to respawn a FAILED run
                // (the `ok` arm above handles a clean run, which is not a restart).
                // The governor exposes no finer cause at this site, so the coarse
                // `crashed` reason is used (the closed domain also carries
                // `stuck`/`rate`, driven where that detail is known). This runs in
                // the daemon (supervisor) process, so it reaches the scrape.
                crate::obs::metrics::record_subagent_restart("crashed");
                sleep_interruptible(d.max(post_wait));
            }
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
/// `completed` outcome acks (`work.ack`, carrying `agent/claim_key` so the server
/// collapses a redelivered-but-already-acked item); any non-terminal / failed
/// outcome releases (`work.release{reason:"wind-down"}`) so the item is immediately
/// re-claimable. Best-effort: a failed ack/release is logged + counted, never
/// fatal — the lease TTL is the backstop.
#[cfg(feature = "cluster")]
fn settle_claim(
    servers: &BTreeMap<String, McpClient>,
    held: &HeldClaim,
    outcome: Option<&Outcome>,
    log: &Logger,
) {
    let Some(coord) = servers.get(&held.server) else {
        // The coordination server was removed by a reload — its lease died with
        // the process; nothing to ack/release. Never panic (RFC 0019 §8 row 6).
        log.warn(
            "claim.settle_skipped",
            json!({"lease": held.lease_id, "server": held.server, "reason": "server_removed"}),
        );
        return;
    };
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

/// Settle a continue-claim when its WARM SESSION ends (RFC 0019 §3.4). Unlike
/// the spawn-claim (settled inline within one deliver iteration), a continue
/// session holds its lease across many deliveries; this runs when the session
/// reaches its END. `terminal` is the session's terminal disposition:
/// `Some(Completed)` acks (the side effect is committed + collapses on a
/// redelivered-but-already-acked item); any other terminal status — or a
/// `None` (the session failed / its process died, no clean completion) —
/// releases so the item is immediately re-claimable. Best-effort: a failed
/// ack/release is logged + counted, never fatal (the lease TTL is the backstop).
#[cfg(feature = "cluster")]
fn settle_session_claim(
    servers: &BTreeMap<String, McpClient>,
    held: &HeldClaim,
    terminal: Option<TerminalStatus>,
    session_id: &str,
    log: &Logger,
) {
    let Some(coord) = servers.get(&held.server) else {
        log.warn(
            "claim.settle_skipped",
            json!({"lease": held.lease_id, "session": session_id, "server": held.server, "reason": "server_removed"}),
        );
        return;
    };
    if terminal == Some(TerminalStatus::Completed) {
        match crate::cluster::claim::ack(coord, &held.lease_id, &held.claim_key) {
            Ok(()) => log.info(
                "claim.acked",
                json!({"lease": held.lease_id, "session": session_id}),
            ),
            Err(e) => log.warn(
                "claim.ack_failed",
                json!({"lease": held.lease_id, "session": session_id, "err": e}),
            ),
        }
    } else {
        crate::obs::metrics::record_claim_released();
        match crate::cluster::claim::release(coord, &held.lease_id, "wind-down") {
            Ok(()) => log.info(
                "claim.released",
                json!({"lease": held.lease_id, "session": session_id}),
            ),
            Err(e) => log.warn(
                "claim.release_failed",
                json!({"lease": held.lease_id, "session": session_id, "err": e}),
            ),
        }
    }
}

/// The renew heartbeat (RFC 0019 §3.3 / §3.6 / §8 row 7): each tick, for every
/// continue-held claim whose work is still in flight (a live warm session),
/// renew the lease when `now - last_renew >= ttl * renew_fraction` so a long
/// session does not lose its lease to TTL expiry. Best-effort: a failed renew is
/// logged + counted, never fatal — the item may redeliver if the lease expires
/// (at-least-once + item-derived idempotency, §3.5, holds). Cheap: a timestamp
/// compare per held claim per tick, a `work.renew` round-trip only on the
/// cadence boundary. Spawn-claims never reach here (settled inline within a
/// tick, before this pass).
#[cfg(feature = "cluster")]
fn renew_held_claims(
    servers: &BTreeMap<String, McpClient>,
    held_claims: &mut HashMap<String, HeldClaim>,
    log: &Logger,
) {
    let now = Instant::now();
    for (key, held) in held_claims.iter_mut() {
        let cadence = held.ttl.mul_f64(held.renew_fraction);
        if now.duration_since(held.last_renew) < cadence {
            continue; // not yet due — the cheap path most ticks take.
        }
        let Some(coord) = servers.get(&held.server) else {
            // The coordination server was removed by a reload — stop renewing a
            // lease whose server is gone (the lease died with the process). Skip;
            // the TTL backstop + at-least-once redelivery hold (RFC 0019 §3.5).
            held.last_renew = now;
            log.warn(
                "claim.renew_skipped",
                json!({"lease": held.lease_id, "key": key, "server": held.server, "reason": "server_removed"}),
            );
            continue;
        };
        match crate::cluster::claim::renew(coord, &held.lease_id, held.ttl) {
            Ok(()) => {
                held.last_renew = now;
                log.info("claim.renewed", json!({"lease": held.lease_id, "key": key}));
            }
            Err(e) => {
                // Best-effort: log, but DO advance `last_renew` so we don't
                // hammer a flapping coordination server every tick — the next
                // cadence window retries. The lease TTL is the backstop, and a
                // lease that expires merely redelivers the item (at-least-once +
                // item-derived idempotency, §3.5). No NEW metric (the RFC 0016
                // schema is frozen) — the `claim.renew_failed` log line counts it.
                held.last_renew = now;
                log.warn(
                    "claim.renew_failed",
                    json!({"lease": held.lease_id, "key": key, "err": e}),
                );
            }
        }
    }
}

/// Spawn + supervise one reaction synchronously, logging the outcome and
/// Spawn a RESUME child for a suspended reactive workflow: the base payload
/// (which carries the graph) plus the persisted slice + how the wait resolved.
#[cfg(feature = "workflow")]
fn resume_workflow(
    exe: &Path,
    base: &SpawnPayload,
    state: crate::graph::GraphState,
    timed_out: bool,
    content: Option<serde_json::Value>,
    drain: Duration,
    log: &Logger,
) -> Option<Outcome> {
    let mut payload = base.clone();
    payload.workflow_resume = Some(crate::subagent::protocol::WorkflowResume {
        state,
        timed_out,
        content,
    });
    crate::graph::live::publish(json!({"status": "driving"}));
    react(exe, &payload, drain, log)
}

/// Digest one reactive-workflow child's outcome: a SUSPENSION arms the watch
/// (subscription + exact route + the timeout clock) and parks the slice;
/// anything else is the workflow's terminal — `Some(exit code)` ends the daemon
/// (its lifetime IS the workflow's). A failed/killed child (no outcome) is a
/// broken workflow run → exit generic.
#[cfg(feature = "workflow")]
#[allow(clippy::too_many_arguments)]
fn handle_workflow_outcome(
    o: Option<Outcome>,
    wf_suspended: &mut Option<(crate::graph::GraphState, String, Instant)>,
    router: &mut Router,
    owner: &mut HashMap<String, String>,
    servers: &BTreeMap<String, McpClient>,
    server_order: &[String],
    log: &Logger,
) -> Option<i32> {
    let Some(o) = o else {
        crate::graph::live::publish(json!({"status": "failed"}));
        return Some(crate::exit::GENERIC);
    };
    let suspended = o
        .result
        .get("$workflow")
        .and_then(|w| w.get("suspended"))
        .cloned();
    if let Some(susp) = suspended {
        let on_uri = susp
            .get("on_uri")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let timeout_ms = susp
            .get("timeout_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);
        let state: crate::graph::GraphState =
            match serde_json::from_value(susp.get("state").cloned().unwrap_or_default()) {
                Ok(s) => s,
                Err(e) => {
                    log.error("workflow.suspend.bad_state", json!({"err": e.to_string()}));
                    crate::graph::live::publish(json!({"status": "failed"}));
                    return Some(crate::exit::GENERIC);
                }
            };
        // A HUMAN GATE suspension (RFC 0021 §7) rides the same machinery: its
        // `reply_uri` (may be empty = timeout-only in the daemon shape) is the
        // watch, and the gate's payload is surfaced on `agent://workflow` for
        // the operator/peer to inspect. The A2A `SendMessage` reply path serves
        // ASYNC served runs; the reactive daemon's gates resolve by
        // reply_uri/timeout (documented in workflows.md).
        let gate = susp.get("gate").filter(|g| !g.is_null()).cloned();
        // Arm the watch through the NORMAL subscription machinery; a uri no
        // server can watch still resumes via the timeout (fail open, like the
        // in-process wait). A gate with NO reply_uri arms nothing — timeout only.
        if on_uri.is_empty() {
            if gate.is_none() {
                log.warn("workflow.wait.unwatchable", json!({"uri": ""}));
            }
        } else if arm_uri_on_first_supporting(servers, server_order, owner, &on_uri).is_some() {
            router.add_route(Route::new(&on_uri, Disposition::Spawn, DEBOUNCE));
        } else {
            log.warn("workflow.wait.unwatchable", json!({"uri": on_uri}));
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        log.info(
            "workflow.suspended",
            json!({"on_uri": on_uri, "timeout_ms": timeout_ms, "steps": state.steps(), "tokens": state.tokens(), "gate": gate.as_ref().and_then(|g| g.get("node"))}),
        );
        let mut live = json!({
            "status": "suspended",
            "on_uri": on_uri,
            "timeout_ms": timeout_ms,
            "steps": state.steps(),
            "tokens": state.tokens(),
        });
        if let Some(g) = &gate {
            live["gate"] = g.clone();
        }
        crate::graph::live::publish(live);
        *wf_suspended = Some((state, on_uri, deadline));
        return None;
    }
    // Terminal: the child already mapped the graph outcome onto TerminalStatus;
    // project it exactly like a one-shot (the daemon's lifetime = the workflow's).
    let code = crate::exit::once_exit(o.status, o.partial);
    log.info(
        "workflow.reactive.exit",
        json!({"status": o.status.as_str(), "code": code}),
    );
    crate::graph::live::publish(json!({
        "status": "terminal",
        "terminal": o.status.as_str(),
        "result": o.result,
    }));
    Some(code)
}

/// Tear down a suspended workflow's watch (subscription + exact route) before a
/// resume — whichever of update/timeout fires first wins, the other is disarmed.
#[cfg(feature = "workflow")]
fn disarm_workflow_wait(
    uri: &str,
    router: &mut Router,
    owner: &mut HashMap<String, String>,
    servers: &BTreeMap<String, McpClient>,
    _log: &Logger,
) {
    if let Some(name) = owner.remove(uri)
        && let Some(s) = servers.get(&name)
    {
        let _ = s.unsubscribe_within(uri, crate::obs::health::management_timeout());
    }
    let _ = router.remove_exact(uri);
}

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

/// Subscribe `uri` on the first connected server (in config declaration order
/// `server_order`) that supports `resources/subscribe` and accepts it, recording
/// the owning server NAME in `owner`. Returns the owning server name on success,
/// `None` if no server armed it. The declaration-order walk reproduces the
/// pre-refactor positional "first supporting server" choice exactly (now keyed by
/// name, not index — RFC 0017 §5.3). Pure routing helper.
fn arm_uri_on_first_supporting(
    servers: &BTreeMap<String, McpClient>,
    server_order: &[String],
    owner: &mut HashMap<String, String>,
    uri: &str,
) -> Option<String> {
    for name in server_order {
        let Some(s) = servers.get(name) else { continue };
        // SHORT management timeout (RFC 0016 §10): this is only ever called on the
        // reactor thread (the reload re-handshake / orphan re-home / a completed
        // reaction's self-subscribe), so a slow-but-alive server arming the
        // subscription must not block the reactor past the liveness window. The
        // startup subscription sweep arms directly with the default timeout.
        if s.capabilities().supports_subscribe()
            && s.subscribe_within(uri, crate::obs::health::management_timeout())
                .is_ok()
        {
            owner.insert(uri.to_string(), name.clone());
            return Some(name.clone());
        }
    }
    None
}

/// Apply a completed reaction's self-requests: arm its scheduled wake-ups and
/// add/remove its resource subscriptions on the live router + servers (RFC 0008).
fn apply_effects(
    o: Outcome,
    wakes: &mut Vec<(Instant, String)>,
    router: &mut Router,
    owner: &mut HashMap<String, String>,
    servers: &BTreeMap<String, McpClient>,
    server_order: &[String],
    log: &Logger,
) {
    arm_wakes(wakes, o.scheduled, Instant::now(), log);
    for req in o.subscriptions {
        match req.action {
            SubscriptionAction::Subscribe => {
                if router.has_exact(&req.uri) {
                    continue; // already watched
                }
                match arm_uri_on_first_supporting(servers, server_order, owner, &req.uri) {
                    Some(server) => {
                        // A conditional subscribe (`await_resource`, pivot Phase 5.2)
                        // carries a content predicate; re-parse it here (it was
                        // validated at tool-call time). A predicate that somehow
                        // fails to parse arms the route UNCONDITIONALLY would be
                        // wrong (it would fire on any update, defeating the wait), so
                        // skip arming and log — the agent's wait simply never arms
                        // rather than misfiring.
                        let condition = match &req.condition {
                            None => None,
                            Some(raw) => match crate::triggers::router::Condition::from_json(raw) {
                                Ok(c) => Some(c),
                                Err(e) => {
                                    log.warn(
                                        "await_resource.bad_condition",
                                        json!({"uri": req.uri, "err": e}),
                                    );
                                    continue;
                                }
                            },
                        };
                        let kind = if condition.is_some() {
                            "await_resource"
                        } else {
                            "self_subscribe"
                        };
                        // Self-subscribe = self-scheduling into a WARM session
                        // (RFC 0008 §self-subscribe): the agent re-enters one live
                        // continue-session per event (session keyed by the URI),
                        // rather than a fresh spawn each time. A condition makes the
                        // re-entry fire only when the resource reaches the wanted
                        // state (the in-turn wait).
                        router.add_route(
                            Route::new(&req.uri, Disposition::Continue(req.uri.clone()), DEBOUNCE)
                                .with_condition(condition),
                        );
                        log.info("trigger.armed", json!({"kind": kind, "uri": req.uri, "server": server, "disposition": "continue"}));
                    }
                    None => log.warn(
                        "subscribe.unsupported",
                        json!({"uri": req.uri, "kind": "self_subscribe"}),
                    ),
                }
            }
            SubscriptionAction::Unsubscribe => {
                if let Some(name) = owner.remove(&req.uri)
                    && let Some(s) = servers.get(&name)
                {
                    // Best-effort, SHORT-bounded (RFC 0016 §10) — on the reactor thread.
                    let _ =
                        s.unsubscribe_within(&req.uri, crate::obs::health::management_timeout());
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

/// Whether a delivery's optional content predicate is satisfied by the freshly
/// read resource content (pivot Phase 5.2 — the reactor's condition gate). No
/// condition → always fire (the v1 fire-on-any behaviour). A condition against
/// content that does not parse as JSON → `false` (a structured predicate cannot
/// match non-JSON), so a conditional wait simply does not fire rather than
/// misfiring. Pure + total, so the fire/skip decision is unit-testable in
/// isolation from the reactor loop.
fn condition_met(condition: &Option<Condition>, content: &str) -> bool {
    match condition {
        None => true,
        Some(cond) => serde_json::from_str::<Value>(content)
            .map(|v| cond.eval(&v))
            .unwrap_or(false),
    }
}

fn read_current(
    servers: &BTreeMap<String, McpClient>,
    owner: &HashMap<String, String>,
    uri: &str,
) -> Option<String> {
    let name = owner.get(uri)?;
    // SHORT management timeout (RFC 0016 §10): the notify-then-read runs on the
    // single reactor thread, so a slow-but-alive resource server must not block it
    // past the liveness staleness window (Finding 1). A timed-out read is treated
    // exactly like any read failure — the caller `unwrap_or_default()`s to empty
    // and the level-triggered model re-reads on the next `updated`, so the
    // edge→level recovery semantics are unchanged from today's read-failure path.
    servers
        .get(name)?
        .read_resource_within(uri, crate::obs::health::management_timeout())
        .ok()
        .map(|r| r.text())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::protocol::{IntelConfig, Limits, Telemetry};

    #[test]
    fn condition_gate_fires_only_on_a_content_match() {
        use serde_json::json;
        // The reactor's decision point (pivot Phase 5.2): a conditional delivery
        // fires ONLY when the freshly-read content satisfies the predicate — the
        // "daemon fires only on match" behaviour behind `await_resource`.
        let cond =
            Condition::from_json(&json!({"pointer": "/status", "op": "eq", "value": "ready"}))
                .unwrap();
        let some = Some(cond);
        // Match → fire.
        assert!(condition_met(&some, r#"{"status":"ready"}"#));
        // No match → skip (the wait stays armed for the next update).
        assert!(!condition_met(&some, r#"{"status":"working"}"#));
        // Non-JSON content can never satisfy a structured predicate → skip.
        assert!(!condition_met(&some, "not json at all"));
        // No condition → always fire (the v1 fire-on-any behaviour is preserved).
        assert!(condition_met(&None, r#"{"anything":true}"#));
        assert!(condition_met(
            &None,
            "even non-json fires when unconditional"
        ));
    }

    /// The mcp_servers reload diff/apply DECISION logic (RFC 0017 §5.3 step 4),
    /// unit-tested in isolation from the spawn I/O (the live re-handshake itself is
    /// proven by the reload_e2e suite). A server is classified by NAME, then by
    /// whether its command/argv/tags differ: removed / added / changed / unchanged.
    #[cfg(feature = "hot-reload")]
    #[test]
    fn mcp_server_reload_is_diffed_by_name_and_endpoint_and_tags() {
        use crate::config::{Config, McpServerSpec, Mode};
        let srv = |name: &str, ep: &str| McpServerSpec {
            name: name.into(),
            endpoint: ep.into(),
            ..Default::default()
        };
        let running = Config {
            mode: Mode::Reactive,
            mcp_servers: vec![srv("a", "https://a.example"), srv("b", "https://b.example")],
            ..Config::default()
        };
        // ADD c, REMOVE b, CHANGE a's endpoint, KEEP nothing identical to a.
        let mut new = running.clone();
        new.mcp_servers = vec![
            srv("a", "https://a-v2.example"), // changed (endpoint differs)
            srv("c", "https://c.example"),    // added
                                              // b removed
        ];
        // reloadable_changes must flag mcp_servers as changed.
        assert!(reloadable_changes(&running, &new).contains(&"mcp_servers"));

        // The per-server classification the apply step uses: by name, then equality.
        let by_name =
            |specs: &[McpServerSpec], n: &str| specs.iter().find(|s| s.name == n).cloned();
        // a: present in both, but specs differ → CHANGED (remove-then-add).
        let a_old = by_name(&running.mcp_servers, "a").unwrap();
        let a_new = by_name(&new.mcp_servers, "a").unwrap();
        assert_ne!(a_old, a_new, "a is changed");
        // b: in running, gone in new → REMOVED.
        assert!(by_name(&new.mcp_servers, "b").is_none(), "b is removed");
        // c: in new, not running → ADDED.
        assert!(by_name(&running.mcp_servers, "c").is_none(), "c is added");

        // An identical server set (same name+endpoint+tags) is NO change.
        assert!(!reloadable_changes(&running, &running).contains(&"mcp_servers"));

        // A tag-only change still counts (the apply remove-then-adds it).
        let mut tagged = running.clone();
        tagged.mcp_servers[0].tags = vec![crate::sec::scope::TrifectaTag::Sensitive];
        assert!(reloadable_changes(&running, &tagged).contains(&"mcp_servers"));
    }

    /// A reload whose NEW config forms a complete lethal trifecta (no
    /// `--allow-trifecta`) is refused by the trifecta gate now in `validate()`
    /// (RFC 0012 §3.2 / RFC 0017 §7), and that refusal is classified as
    /// `reason="trifecta_required"` so a SIGHUP/ConfigMap swap can never widen the
    /// trifecta live. The candidate is rebuilt from a config file, so we drive the
    /// real `Config::reload` path (file<env<flag).
    #[cfg(feature = "hot-reload")]
    #[test]
    fn reload_forming_a_trifecta_is_refused_as_trifecta_required() {
        use std::io::Write as _;
        // A config file that grants all three legs on one server (no override).
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            r#"{{"mcp_servers":[{{"name":"s","endpoint":"https://s.example","tags":{{"*":["untrusted_input","sensitive","egress"]}}}}]}}"#
        )
        .unwrap();
        f.flush().unwrap();
        let args = vec![
            "--config".to_string(),
            f.path().display().to_string(),
            "--mode".to_string(),
            "reactive".to_string(),
            "--subscribe".to_string(),
            "x:res".to_string(),
        ];
        let env = vec![
            ("INSTRUCTION".to_string(), "x".to_string()),
            (
                "AGENTD_INTELLIGENCE".to_string(),
                "https://intel.example".to_string(),
            ),
        ];
        // The candidate fails to load (the trifecta gate in validate() refuses it).
        let err = Config::reload(&args, &env).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("lethal-trifecta"), "got: {msg}");
        // …and the reload reason classifier maps that to trifecta_required.
        assert_eq!(reload_reject_reason(&msg), "trifecta_required");
        // A generic validation failure is still plain "invalid".
        assert_eq!(
            reload_reject_reason("mcp server 's' has empty name"),
            "invalid"
        );
        // With --allow-trifecta the SAME file loads cleanly (the override permits it).
        let mut allowed = args.clone();
        allowed.push("--allow-trifecta".to_string());
        assert!(Config::reload(&allowed, &env).is_ok());
    }

    fn base() -> SpawnPayload {
        SpawnPayload {
            instruction: "triage the change".into(),
            output_contract: None,
            context_seed: Vec::new(),
            intelligence: IntelConfig {
                uri: "https://intel.example".into(),
                token: None,
                model: None,
            },
            mcp_servers: Vec::new(),
            a2a_peers: Vec::new(),
            tls_ca: None,
            aauth: None,
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
            warm: false,
            #[cfg(feature = "workflow")]
            workflow: None,
            #[cfg(feature = "workflow")]
            workflow_reactive: false,
            #[cfg(feature = "workflow")]
            workflow_resume: None,
            #[cfg(feature = "workflow")]
            workflow_resume_ref: None,
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
        assert_eq!(m["agent/claim_key"], json!("deadbeef"));
        assert_eq!(m["agent/instance"], json!("pod-abc"));
        assert_eq!(m["agent/shard"], json!("3/8"));
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
            m2.get("agent/shard").is_none(),
            "shard must be omitted when unsharded"
        );
        assert!(
            m2.get("traceparent").is_none(),
            "traceparent omitted when absent"
        );
        assert_eq!(m2["agent/claim_key"], json!("k"));
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
