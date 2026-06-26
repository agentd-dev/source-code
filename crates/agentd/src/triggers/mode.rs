//! Execution-mode drivers. RFC 0008 §modes.
//!
//! `once` is `main::run_once` (spawn + supervise one root). This module hosts
//! the long-lived **`reactive`** driver — the signature "listen for MCP
//! resources, act when they appear" mode. `loop`/`schedule` follow in M4.
//!
//! The reactive driver: the *supervisor* connects the configured MCP servers
//! and owns the long-lived **subscriptions**; on a `notifications/resources/
//! updated{uri}` it does **notify-then-read** (`resources/read` the current
//! state, RFC 0004) and, per the [`Router`] disposition, spawns a fresh root
//! subagent templated from the event. v1 reacts **synchronously** (one event
//! at a time) and treats every route as `Spawn`; warm `Continue` sessions and
//! concurrent reactions land later this milestone.

use crate::agentloop::stop::ScheduleRequest;
use crate::config::Config;
use crate::exit;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::signals;
use crate::subagent::protocol::{SeedMessage, SpawnPayload};
use crate::supervisor::reactor::{supervise_once, SuperviseResult};
use crate::supervisor::restart::{RestartAction, RestartConfig, RestartGovernor};
use crate::triggers::router::{Disposition, Route, Router};
use crate::wire::mcp::method;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Poll cadence for draining MCP notifications + firing due deliveries.
const TICK: Duration = Duration::from_millis(200);
/// Default per-URI debounce (RFC 0008).
const DEBOUNCE: Duration = Duration::from_millis(250);

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
                log.error("mcp.connect.fail", json!({"server": spec.name, "err": e.to_string()}));
                eprintln!("agentd: MCP server '{}' failed: {e}", spec.name);
                return exit::MCP_REQUIRED_DOWN;
            }
        }
    }

    // v1: every subscription routes to a fresh Spawn. Continue/warm sessions later.
    let routes: Vec<Route> =
        cfg.subscribe.iter().map(|u| Route::new(u, Disposition::Spawn, DEBOUNCE)).collect();
    let mut router = Router::new(routes);

    // Subscribe each URI on the first connected server that supports it; track
    // which server owns each URI so we read it back from the same place.
    let mut owner: HashMap<String, usize> = HashMap::new();
    for uri in &cfg.subscribe {
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
        if router.on_updated(uri, t0) {
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
                    && router.on_updated(&uri, now)
                {
                    log.info("resource.updated", json!({"uri": uri}));
                }
            }
        }

        // Fire due (debounced) deliveries: notify-then-read, then react.
        for delivery in router.due(now) {
            let content = read_current(&servers, &owner, &delivery.uri).unwrap_or_default();
            match delivery.disposition {
                Disposition::Spawn | Disposition::Continue(_) => {
                    let payload = reactive_payload(&base, &delivery.uri, &content);
                    log.info("trigger.fired", json!({"uri": delivery.uri, "bytes": content.len()}));
                    crate::obs::metrics::record_reaction();
                    let scheduled = react(&exe, &payload, cfg.drain_timeout, log);
                    arm_wakes(&mut wakes, scheduled, Instant::now(), log);
                }
            }
        }

        // Fire due self-scheduled wake-ups: each runs its own instruction as a
        // fresh reaction, and may schedule further wake-ups (a self-sustaining
        // agent, bounded by the daemon lifetime + per-run budgets).
        for instruction in drain_due_wakes(&mut wakes, now) {
            let payload = scheduled_payload(&base, &instruction);
            log.info("trigger.fired", json!({"kind": "self_schedule", "instruction_len": instruction.len()}));
            crate::obs::metrics::record_reaction();
            let scheduled = react(&exe, &payload, cfg.drain_timeout, log);
            arm_wakes(&mut wakes, scheduled, Instant::now(), log);
        }

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
    let mut iteration: u64 = 0;
    log.info(
        "trigger.armed",
        json!({"kind": cfg.mode.as_str(), "interval_ms": interval.as_millis() as u64}),
    );
    log.info("proc.ready", json!({"mode": cfg.mode.as_str()}));

    loop {
        crate::obs::health::tick();
        if signals::draining() {
            log.info("proc.exit", json!({"reason": "drain", "mode": cfg.mode.as_str()}));
            return exit::SUCCESS;
        }
        iteration += 1;
        log.info("schedule.fired", json!({"iteration": iteration}));

        // Time the run so the governor can spot a crash-on-spawn (RFC 0003 §3.7
        // — a run that dies faster than the ready threshold counts heavier).
        let started = Instant::now();
        let ok = match supervise_once(exe.clone(), &base, cfg.drain_timeout, log.clone()) {
            Ok(SuperviseResult::Completed(o)) => {
                log.info("run.completed", json!({"status": o.status.as_str()}));
                true
            }
            Ok(SuperviseResult::Failed(e)) => {
                log.warn("run.failed", json!({"err": e}));
                false
            }
            Ok(SuperviseResult::Killed(r)) => {
                log.warn("run.killed", json!({"reason": format!("{r:?}")}));
                false
            }
            Err(e) => {
                log.error("run.spawn_fail", json!({"err": e.to_string()}));
                false
            }
        };

        // Consult the restart governor. A successful run resets it and waits
        // the configured interval — 0 for `loop`, the `--interval` for
        // `schedule` (interval semantics preserved). A failed/killed run either
        // backs off (capped + jittered, never below the interval) or, on a
        // tripped breaker, ends the daemon rather than respawn into a known-bad
        // loop. RFC 0003 §3.7 / assessment §4 M2 "crash-loop trips breaker".
        let now = Instant::now();
        match governor.on_outcome(ok, now.duration_since(started), now) {
            _ if ok => sleep_interruptible(interval),
            RestartAction::Backoff(d) => sleep_interruptible(d.max(interval)),
            RestartAction::Tripped => {
                crate::obs::metrics::record_restart_tripped();
                log.warn("proc.exit", json!({"reason": "restart_breaker", "iteration": iteration}));
                return exit::GENERIC;
            }
        }
    }
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

/// Spawn + supervise one reaction synchronously, logging the outcome and
/// returning any future wake-ups the reaction scheduled for itself (RFC 0008).
fn react(exe: &Path, payload: &SpawnPayload, drain: Duration, log: &Logger) -> Vec<ScheduleRequest> {
    match supervise_once(exe.to_path_buf(), payload, drain, log.clone()) {
        Ok(SuperviseResult::Completed(o)) => {
            log.info("reactive.handled", json!({"status": o.status.as_str()}));
            o.scheduled
        }
        Ok(SuperviseResult::Failed(e)) => {
            log.error("reactive.failed", json!({"err": e}));
            Vec::new()
        }
        Ok(SuperviseResult::Killed(r)) => {
            log.warn("reactive.killed", json!({"reason": format!("{r:?}")}));
            Vec::new()
        }
        Err(e) => {
            log.error("reactive.spawn_fail", json!({"err": e.to_string()}));
            Vec::new()
        }
    }
}

/// Arm self-scheduled wake-ups relative to `base_time`, logging each (RFC 0008).
fn arm_wakes(wakes: &mut Vec<(Instant, String)>, reqs: Vec<ScheduleRequest>, base_time: Instant, log: &Logger) {
    for r in reqs {
        let at = base_time + Duration::from_millis(r.after_ms);
        log.info("trigger.armed", json!({"kind": "self_schedule", "after_ms": r.after_ms}));
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

/// Build the payload for one reaction: the standing instruction plus the
/// changed resource's current state as context. Pure.
pub fn reactive_payload(base: &SpawnPayload, uri: &str, content: &str) -> SpawnPayload {
    let mut p = base.clone();
    p.context_seed = vec![SeedMessage {
        role: "user".into(),
        content: format!("The resource {uri} changed. Its current content is:\n\n{content}"),
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

fn read_current(servers: &[McpClient], owner: &HashMap<String, usize>, uri: &str) -> Option<String> {
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
            intelligence: IntelConfig { uri: "unix:/x".into(), token: None, model: None },
            mcp_servers: Vec::new(),
            limits: Limits { max_steps: 10, max_tokens: 1000, deadline_ms: 1000, max_depth: 4 },
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
    fn updated_uri_parses() {
        assert_eq!(updated_uri(&Some(json!({"uri": "file://a"}))), Some("file://a".into()));
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
        b.context_seed = vec![SeedMessage { role: "user".into(), content: "stale".into() }];
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
                ScheduleRequest { after_ms: 0, instruction: "now".into() },
                ScheduleRequest { after_ms: 60_000, instruction: "later".into() },
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
