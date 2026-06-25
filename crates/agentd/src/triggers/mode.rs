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

use crate::config::Config;
use crate::exit;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::signals;
use crate::subagent::protocol::{SeedMessage, SpawnPayload};
use crate::supervisor::reactor::{supervise_once, SuperviseResult};
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
                    react(&exe, &payload, cfg.drain_timeout, log);
                }
            }
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
/// deadline, and the orchestrator bounds the daemon (Job deadline). A
/// fast-failing run backs off (capped) so it can't hot-spin — a lightweight
/// stand-in for the restart governor (RFC 0003).
pub fn run_scheduled(exe: PathBuf, base: SpawnPayload, cfg: &Config, log: &Logger) -> i32 {
    let interval = cfg.interval.unwrap_or(Duration::ZERO);
    let mut backoff = Duration::from_millis(500);
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

        let wait = if ok {
            backoff = Duration::from_millis(500);
            interval
        } else {
            let w = backoff.max(interval);
            backoff = (backoff * 2).min(Duration::from_secs(30));
            w
        };
        sleep_interruptible(wait);
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

/// Spawn + supervise one reaction synchronously, logging the outcome.
fn react(exe: &Path, payload: &SpawnPayload, drain: Duration, log: &Logger) {
    match supervise_once(exe.to_path_buf(), payload, drain, log.clone()) {
        Ok(SuperviseResult::Completed(o)) => {
            log.info("reactive.handled", json!({"status": o.status.as_str()}))
        }
        Ok(SuperviseResult::Failed(e)) => log.error("reactive.failed", json!({"err": e})),
        Ok(SuperviseResult::Killed(r)) => log.warn("reactive.killed", json!({"reason": format!("{r:?}")})),
        Err(e) => log.error("reactive.spawn_fail", json!({"err": e.to_string()})),
    }
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
}
