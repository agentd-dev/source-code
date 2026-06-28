//! agentd entry point.
//!
//! Dispatches between three roles of the one binary: the **supervisor** (the
//! normal CLI/daemon path), the **subagent** re-exec, and the early-exit
//! `--help`/`--version`. The supervisor parses + validates config, installs
//! signal handlers and the subreaper, sets up logging/tracing, optionally arms
//! health/metrics/cgroups, enforces the Rule-of-Two trifecta gate, then drives
//! the configured mode (once / loop / reactive / schedule), supervising a root
//! subagent.

use agentd::agentloop::stop::TerminalStatus;
use agentd::config::{Config, ConfigError, Mode};
use agentd::obs::log::{Comp, LogCtx, Logger};
use agentd::subagent::protocol::{IntelConfig, Limits, SpawnPayload, Telemetry};
use agentd::supervisor::reactor::{KillReason, SuperviseResult, supervise_once};
use agentd::triggers::mode::{run_reactive, run_scheduled};
use agentd::{exit, signals};
use serde_json::{Value, json};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let argv: Vec<String> = std::env::args().collect();

    // Hidden built-in mock MCP server (tests / dev):
    // `--internal-mock-mcp <uri> [--no-emit]`. Compiled only when the mock
    // modules are (debug builds, or release `--features internal-mocks`); the
    // production binary omits this dispatch entirely.
    #[cfg(any(feature = "internal-mocks", debug_assertions))]
    if argv.get(1).map(String::as_str) == Some("--internal-mock-mcp") {
        let uri = argv.get(2).map(String::as_str).unwrap_or("mock://resource");
        let emit = !argv.iter().any(|a| a == "--no-emit");
        return agentd::mcp::mock::run(uri, emit);
    }

    // Hidden built-in mock LLM (tests / observe-suite):
    // `--internal-mock-llm <socket> [final|read|schedule]`. Same gating as the
    // mock MCP dispatch above.
    #[cfg(any(feature = "internal-mocks", debug_assertions))]
    if argv.get(1).map(String::as_str) == Some("--internal-mock-llm") {
        let socket = argv
            .get(2)
            .map(String::as_str)
            .unwrap_or("/tmp/agentd-mock-llm.sock");
        let script = argv.get(3).map(String::as_str).unwrap_or("final");
        return agentd::intel::mock::run(socket, script);
    }

    // Subagent re-exec dispatch. The supervisor sets this in the child's
    // environment; the child reads its spawn payload over the control channel
    // (stdin) rather than from CLI/env config.
    if std::env::var_os(agentd::subagent::protocol::SUBAGENT_ENV).is_some() {
        return agentd::subagent::control::run();
    }

    let env: Vec<(String, String)> = std::env::vars().collect();
    let cfg = match Config::load(&argv[1..], &env) {
        Ok(c) => c,
        // `--capabilities` (RFC 0015 §5.2) joins `--help`/`--version` as a
        // side-effect-free early exit: the manifest JSON goes to stdout, exit 0.
        // It is resolved during config parse, before any MCP connect / LLM call /
        // socket bind below.
        Err(ConfigError::Help(s))
        | Err(ConfigError::Version(s))
        | Err(ConfigError::Capabilities(s)) => {
            print!("{s}");
            return exit::SUCCESS;
        }
        // `--config-schema` (RFC 0017 §4.2): the JSON Schema of the config file
        // to STDOUT, exit 0 — a side-effect-free schema export for agentctl.
        Err(ConfigError::Schema(s)) => {
            print!("{s}");
            return exit::SUCCESS;
        }
        // `--validate-config` (RFC 0017 §4.1): the admission verdict to STDERR.
        // Valid ⇒ one `config.valid` line, exit 0; invalid ⇒ N `config.invalid`
        // lines, exit 2 — before any MCP connect / LLM call / socket bind.
        Err(ConfigError::Validate(verdict)) => match verdict {
            Ok(line) => {
                eprintln!("{line}");
                return exit::SUCCESS;
            }
            Err(lines) => {
                eprintln!("{lines}");
                return exit::USAGE;
            }
        },
        Err(ConfigError::Usage(s)) => {
            eprintln!("{s}");
            return exit::USAGE;
        }
    };

    signals::install();
    // Adopt orphaned grandchildren into our reaping domain (RFC 0003).
    let subreaper = agentd::supervisor::reap::set_child_subreaper();

    // One trace id for the whole run (ingested from upstream or minted from the
    // run id) — stamped on every log line + propagated to children (RFC 0010).
    let trace_id = agentd::obs::trace::resolve(&cfg.run_id, cfg.traceparent.as_deref()).trace_id;
    let log = Logger::new(
        LogCtx {
            run_id: cfg.run_id.clone(),
            agent_id: "sup".into(),
            agent_path: "0".into(),
            comp: Comp::Supervisor,
            pid: std::process::id(),
            trace_id: Some(trace_id),
        },
        cfg.log_level,
    )
    .with_content(cfg.log_content);
    log.info(
        "proc.start",
        json!({
            "version": agentd::VERSION,
            "mode": cfg.mode.as_str(),
            "mcp_servers": cfg.mcp_servers.len(),
            "subscribe": cfg.subscribe.len(),
            "subreaper": subreaper,
        }),
    );
    // The validated config/policy this run operates under (RFC 0010 §2.9
    // closed vocabulary). Content-capture-off: lengths/schemes only, never the
    // instruction body or the intelligence credential.
    log.info(
        "config.loaded",
        json!({
            "max_steps": cfg.max_steps,
            "max_tokens": cfg.max_tokens,
            "deadline_ms": cfg.deadline.map(|d| d.as_millis() as u64),
            "max_depth": cfg.max_depth,
            "enable_exec": cfg.enable_exec,
            "log_content": cfg.log_content,
            "serve_mcp": cfg.serve_mcp.is_some(),
            "intel_scheme": cfg.intelligence.as_deref().and_then(|u| u.split(':').next()),
            "instruction_len": cfg.instruction.as_deref().map_or(0, str::len),
        }),
    );

    // Rule of Two (RFC 0012 §3.2): the lethal-trifecta REFUSAL is now enforced
    // inside `Config::validate()` — the single validation authority (RFC 0017 §7)
    // that `--validate-config` and startup both run — so a refused grant already
    // exited 2 during `Config::load` above and never reaches here. What remains is
    // the auditable WIDENING: when `--allow-trifecta` downgrades the refusal to a
    // warning, emit the `scope.trifecta_grant` event so the override is recorded.
    // Scope narrows monotonically (RFC 0009), so the root union bounds the tree.
    use agentd::sec::scope::{TrifectaVerdict, check_trifecta};
    if check_trifecta(cfg.trifecta_grant_tags(), cfg.allow_trifecta)
        == TrifectaVerdict::AllowedWithWarning
    {
        log.warn(
            "scope.trifecta_grant",
            json!({"allowed": true, "legs": ["untrusted_input", "sensitive", "egress"]}),
        );
    }

    // cgroup v2 memory awareness (best-effort): report the scheduler-imposed
    // budget so OOM risk is observable. Quiet when there is no cgroup. RFC 0010.
    let mem = agentd::supervisor::cgroup::snapshot();
    if mem.detected() {
        log.info(
            "cgroup.detected",
            json!({"memory_max": mem.max, "memory_current": mem.current, "memory_high": mem.high}),
        );
    }
    // Opt-in cgroup-v2 active enforcement (`--cgroup auto|<path>`): arm per-run
    // child cgroups for atomic `cgroup.kill` teardown, plus optional hard limits
    // (`--cgroup-memory-max`/`--cgroup-pids-max`). Best-effort — silently dormant
    // if the tree isn't writable (no delegation / off-cgroup).
    if let Some(spec) = cfg.cgroup.as_deref() {
        match agentd::supervisor::cgroup::configure(
            Some(spec),
            cfg.cgroup_memory_max.as_deref(),
            cfg.cgroup_pids_max.as_deref(),
        ) {
            Some(c) => {
                log.info(
                    "cgroup.enabled",
                    json!({"parent": c.parent.display().to_string(), "memory_max": c.limits.memory_max, "pids_max": c.limits.pids_max}),
                );
                if c.limits_unavailable {
                    // Limits were requested but the controllers couldn't be
                    // delegated (e.g. `auto` under a busy unit cgroup) — teardown
                    // still works, but the limits won't be enforced.
                    log.warn(
                        "cgroup.limits_unavailable",
                        json!({"parent": c.parent.display().to_string()}),
                    );
                }
            }
            None => log.warn("cgroup.unavailable", json!({"spec": spec})),
        }
    }

    // RFC 0016 §6.4: a reactive daemon has no single terminal outcome, so
    // `--report-file` is inert — warn (not a hard error) so the operator learns
    // the flag does nothing here. Its per-reaction outcomes live in metrics +
    // the event stream.
    if cfg.mode == Mode::Reactive && cfg.report_file.is_some() {
        log.warn(
            "config.inert",
            json!({"flag": "--report-file", "reason": "reactive daemons emit no run report (RFC 0016 §6.4)"}),
        );
    }
    // RFC 0016 §7.2 / §11: install the bounded `agentd://events` ring when the
    // served self-MCP is configured (the events surface is implied by
    // `--serve-mcp` + the `events` feature). A no-op without the feature; the ring
    // capture path is a single relaxed atomic load otherwise, so the default build
    // pays nothing. Telemetry never crashes the run (§8.4).
    install_event_ring(&cfg, &log);

    match cfg.mode {
        Mode::Once => run_once(&cfg, &log),
        // The long-lived modes all re-exec a root subagent, so they need our
        // own executable path.
        Mode::Reactive | Mode::Loop | Mode::Schedule => {
            let exe = match std::env::current_exe() {
                Ok(exe) => exe,
                Err(e) => {
                    log.error("proc.exit", json!({"err": format!("current_exe: {e}")}));
                    return exit::GENERIC;
                }
            };
            // Daemon liveness: write a supervisor-heartbeat health file (RFC 0010).
            if let Some(path) = &cfg.health_file {
                agentd::obs::health::spawn_writer(
                    path.into(),
                    cfg.run_id.clone(),
                    cfg.mode.as_str().into(),
                    std::time::Duration::from_millis(agentd::obs::health::LIVENESS_STALE_AFTER_MS),
                );
                log.info("health.armed", json!({"file": path}));
            }
            // Opt-in HTTP scrape/probe surface (RFC 0010). Built only with
            // `--features metrics`; without it, `--metrics-addr` warns and is inert.
            if let Some(addr) = &cfg.metrics_addr {
                serve_metrics(addr, &log);
            }
            // Opt-in served self-MCP for composability (RFC 0005) + the operator
            // profile (RFC 0015 §4). Built only with `--features serve-mcp`;
            // otherwise `--serve-mcp` warns + is inert. The operator tools share
            // the daemon's lifecycle state through process-global latches in
            // `signals` (no flag threading): `drain` flips the SAME one-way
            // DRAINING latch SIGTERM sets — so the metrics `/readyz` probe above,
            // the reactor's drain choreography, and the served inventory all read
            // one truth — and `lame-duck` flips the readiness override that both
            // `/readyz` and `agentd://inventory.ready` consult.
            let serve_wiring = cfg
                .serve_mcp
                .as_ref()
                .and_then(|spec| serve_self_mcp(spec, &exe, root_payload(&cfg), &cfg, &log));
            // Split the wiring: the shutdown handle (drained below) and the
            // live-config handle the reactive supervisor adopts so an applied hot
            // reload swaps the served `agentd://config/effective` view + pushes
            // `resources/updated` (RFC 0017 §4.2 / §5.6). Without serve-mcp the
            // wiring is `Option<()>` and there is no live-config handle to thread.
            #[cfg(feature = "serve-mcp")]
            let (serve_handle, live_config) = match serve_wiring {
                Some((h, lc)) => (Some(h), Some(lc)),
                None => (None, None),
            };
            let code = match cfg.mode {
                // The reactive driver re-reads the config FILE on SIGHUP (RFC 0017
                // §5), so it needs the process's original argv + env (the fixed
                // env/flag layers; only the FILE can change between loads). These
                // are inert without the `hot-reload` feature (the loop never
                // consults the reload latch), so the no-reload path is unchanged.
                Mode::Reactive => run_reactive(
                    exe,
                    root_payload(&cfg),
                    &cfg,
                    &argv[1..],
                    &env,
                    &log,
                    #[cfg(feature = "serve-mcp")]
                    live_config,
                ),
                _ => run_scheduled(exe, root_payload(&cfg), &cfg, &log), // Loop | Schedule
            };
            // On shutdown, let in-flight served runs drain before we exit (their
            // subtrees would otherwise be collapsed by PDEATHSIG at process exit).
            #[cfg(feature = "serve-mcp")]
            if let Some(h) = serve_handle {
                h.drain(cfg.drain_timeout);
                log.info("mcp.drained", json!({}));
            }
            #[cfg(not(feature = "serve-mcp"))]
            let _ = serve_wiring;
            code
        }
    }
}

/// Install the bounded `agentd://events` ring when the served self-MCP is
/// configured (RFC 0016 §7.2). The events surface is implied by `--serve-mcp` +
/// the `events` feature; without either, no ring is installed and capture stays
/// a no-op. `--events-ring N` sizes it (default 1024).
#[cfg(feature = "events")]
fn install_event_ring(cfg: &Config, log: &Logger) {
    if cfg.serve_mcp.is_some() {
        agentd::obs::log::install_event_ring(cfg.events_ring);
        log.info(
            "events.armed",
            json!({"ring": cfg.events_ring, "uri": agentd::agentd_uri::EVENTS_URI}),
        );
    }
}

/// No-op without the `events` feature: this build serves no `agentd://events`
/// resource, so no ring is installed (capability-absence-not-error). Warns when
/// the surface was implied (`--serve-mcp` set) so the operator knows it is inert.
#[cfg(not(feature = "events"))]
fn install_event_ring(cfg: &Config, log: &Logger) {
    if cfg.serve_mcp.is_some() {
        log.warn(
            "events.unavailable",
            json!({"reason": "built without --features events"}),
        );
    }
}

/// Start the opt-in HTTP probe/scrape surface, or warn that this build can't.
/// Gated so the default build links no listener. RFC 0010.
#[cfg(feature = "metrics")]
fn serve_metrics(addr: &str, log: &Logger) {
    if let Err(e) = agentd::obs::serve::spawn(addr, log.clone()) {
        log.error(
            "metrics.bind_fail",
            json!({"addr": addr, "err": e.to_string()}),
        );
    }
}

#[cfg(not(feature = "metrics"))]
fn serve_metrics(addr: &str, log: &Logger) {
    log.warn(
        "metrics.unavailable",
        json!({"addr": addr, "reason": "built without --features metrics"}),
    );
}

/// Start the served self-MCP (composability, RFC 0005), or warn this build can't.
/// The `status` tool reports `cfg`; `subagent.spawn` runs fresh agents from the
/// daemon's root payload template (`base`).
/// The served-MCP wiring handed back to `main`: the shutdown [`ServeHandle`] plus
/// the [`LiveConfig`](agentd::mcp::server::LiveConfig) handle the reactive
/// supervisor adopts so an applied hot reload swaps the served
/// `agentd://config/effective` view and pushes `resources/updated` (RFC 0017
/// §4.2 / §5.6). The SAME registry backs both — one subscription set.
#[cfg(feature = "serve-mcp")]
type ServeWiring = (
    agentd::mcp::server::ServeHandle,
    std::sync::Arc<agentd::mcp::server::LiveConfig>,
);

#[cfg(feature = "serve-mcp")]
fn serve_self_mcp(
    spec: &str,
    exe: &std::path::Path,
    base: SpawnPayload,
    cfg: &Config,
    log: &Logger,
) -> Option<ServeWiring> {
    use agentd::config::ServeTarget;
    // The target is already validated at config load (exit 2 on a bad scheme/port),
    // so a parse failure here is unexpected — surface it and stay inert rather than
    // panic.
    let target = match ServeTarget::parse(spec) {
        Ok(t) => t,
        Err(e) => {
            log.error(
                "mcp.serve_fail",
                json!({"spec": spec, "err": e.to_string()}),
            );
            return None;
        }
    };
    let new_ctx = || {
        agentd::mcp::server::ServeCtx::new(
            cfg.run_id.clone(),
            cfg.mode.as_str().to_string(),
            exe.to_path_buf(),
            base.clone(),
            cfg.drain_timeout,
            std::sync::Arc::new(cfg.clone()),
        )
    };
    match target {
        ServeTarget::Unix(path) => {
            let path = path.to_string_lossy().into_owned();
            // Capture the live-config handle BEFORE the ctx is moved into `serve`
            // (its `subs` is the same registry `serve` shares into the ServeHandle).
            let ctx = new_ctx();
            let live_config = ctx.live_config();
            match agentd::mcp::server::serve(&path, ctx, log.clone()) {
                Ok(handle) => Some((handle, live_config)),
                Err(e) => {
                    log.error(
                        "mcp.serve_fail",
                        json!({"path": path, "err": e.to_string()}),
                    );
                    None
                }
            }
        }
        ServeTarget::Vsock { cid, port } => serve_self_mcp_vsock(cid, port, new_ctx(), log),
    }
}

/// Bind the served self-MCP over vsock when this build has the `vsock` feature;
/// otherwise stay inert (config validation already rejects `vsock:` on a
/// non-vsock build, so this arm is only reached on a `vsock` build).
#[cfg(all(feature = "serve-mcp", feature = "vsock"))]
fn serve_self_mcp_vsock(
    cid: u32,
    port: u32,
    ctx: agentd::mcp::server::ServeCtx,
    log: &Logger,
) -> Option<ServeWiring> {
    // Capture the live-config handle before the ctx moves into `serve_vsock`.
    let live_config = ctx.live_config();
    match agentd::mcp::server::serve_vsock(cid, port, ctx, log.clone()) {
        Ok(handle) => Some((handle, live_config)),
        Err(e) => {
            log.error(
                "mcp.serve_fail",
                json!({"transport": "vsock", "cid": cid, "port": port, "err": e.to_string()}),
            );
            None
        }
    }
}

#[cfg(all(feature = "serve-mcp", not(feature = "vsock")))]
fn serve_self_mcp_vsock(
    cid: u32,
    port: u32,
    _ctx: agentd::mcp::server::ServeCtx,
    log: &Logger,
) -> Option<ServeWiring> {
    log.warn(
        "mcp.serve_unavailable",
        json!({"transport": "vsock", "cid": cid, "port": port, "reason": "built without --features vsock"}),
    );
    None
}

#[cfg(not(feature = "serve-mcp"))]
fn serve_self_mcp(
    spec: &str,
    _exe: &std::path::Path,
    _base: SpawnPayload,
    _cfg: &Config,
    log: &Logger,
) -> Option<()> {
    log.warn(
        "mcp.serve_unavailable",
        json!({"spec": spec, "reason": "built without --features serve-mcp"}),
    );
    None
}

/// One-shot mode: spawn + supervise a root subagent that runs the agentic
/// loop, then map its result to an exit code (RFC 0011 §5). stdout is the
/// result; stderr is telemetry. The loop itself runs in the child process; the
/// supervisor here owns lifecycle, liveness, and teardown.
fn run_once(cfg: &Config, log: &Logger) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log.error("proc.exit", json!({"err": format!("current_exe: {e}")}));
            eprintln!("agentd: cannot locate own executable: {e}");
            return exit::GENERIC;
        }
    };
    let payload = root_payload(cfg);

    // Bookend the run for the report's duration / timestamps (RFC 0016 §6.2).
    let started = std::time::SystemTime::now();
    let result = supervise_once(exe, &payload, cfg.drain_timeout, log.clone());
    let ended = std::time::SystemTime::now();

    // Derive (terminal-status, has_usable_partial, exit_code) ONCE, so the report
    // and the process exit code agree (the exit code is the report's coarse
    // projection, §6.2). `status`/`partial` come from the outcome where present;
    // a fatal-infra Failed / a supervisor Kill have no TerminalStatus enum value
    // (RFC 0007 §3.4 aborts), so they project to the nearest report status.
    let (status, partial, code) = match &result {
        Ok(SuperviseResult::Completed(o)) => {
            (o.status, o.partial, exit::once_exit(o.status, o.partial))
        }
        Ok(SuperviseResult::Failed(err)) => (TerminalStatus::Crashed, false, failed_exit(err)),
        Ok(SuperviseResult::Killed(reason)) => kill_report_status(*reason),
        Err(_) => (TerminalStatus::Crashed, false, exit::GENERIC),
    };

    // Write the run-outcome report (RFC 0016 §6.3) BEFORE the proc.exit line and
    // BEFORE returning the code. Best-effort-but-loud (§8.4): a failed write logs
    // `report.write.fail` and the run still exits with `code` — the exit code is
    // the floor contract, never gated on the report landing.
    write_run_report(cfg, status, code, partial, started, ended, log);

    // The PROCESS exit a Job's podFailurePolicy observes — the operator's
    // `--budget-exit-code` remaps ONLY the two policy budget codes (3/7) here
    // (RFC 0011 §5.2); the report above kept the canonical projection.
    let proc_code = exit::apply_budget_remap(code, cfg.budget_exit_code);

    match result {
        Ok(SuperviseResult::Completed(outcome)) => {
            print_result(&outcome.result);
            log.info(
                "proc.exit",
                json!({"status": outcome.status.as_str(), "partial": outcome.partial}),
            );
            proc_code
        }
        Ok(SuperviseResult::Failed(err)) => {
            log.error("proc.exit", json!({"err": err}));
            eprintln!("agentd: {err}");
            proc_code
        }
        Ok(SuperviseResult::Killed(reason)) => {
            log.warn("proc.exit", json!({"killed": format!("{reason:?}")}));
            eprintln!("agentd: run terminated ({reason:?})");
            proc_code
        }
        Err(e) => {
            log.error("proc.exit", json!({"err": format!("spawn: {e}")}));
            eprintln!("agentd: failed to spawn root subagent: {e}");
            proc_code
        }
    }
}

/// Project a supervisor [`KillReason`] to a `(report status, has_usable_partial,
/// exit code)` triple for the run report + exit (RFC 0016 §6.2 / RFC 0011 §5). A
/// kill has no `TerminalStatus` enum value, so it maps to the nearest report
/// status; the exit code matches `run_once`'s prior mapping exactly (a clean
/// drain is `0`, never `143` — RFC 0011 §5.1).
fn kill_report_status(reason: KillReason) -> (TerminalStatus, bool, i32) {
    match reason {
        KillReason::Deadline | KillReason::Stuck => {
            (TerminalStatus::Deadline, false, exit::DEADLINE)
        }
        KillReason::TreeBudget => (TerminalStatus::ExhaustedTokens, false, exit::BUDGET),
        KillReason::Drain => (TerminalStatus::Cancelled, false, exit::SUCCESS),
    }
}

/// Build + write the run-outcome report (RFC 0016 §6) when `--report-file` is
/// configured. Off for a bare CLI run (no file) and inert for reactive (§6.4 —
/// reactive never calls this). Usage is best-effort: the supervisor does not
/// aggregate per-run token/step totals into the `once` outcome path, so they are
/// reported as `0` — honest absence, never an estimate (RFC 0010 §3.9 / §4.3).
/// A control plane that needs exact usage reads the metrics / event stream.
fn write_run_report(
    cfg: &Config,
    status: TerminalStatus,
    exit_code: i32,
    has_usable_partial: bool,
    started: std::time::SystemTime,
    ended: std::time::SystemTime,
    log: &Logger,
) {
    let Some(path) = cfg.report_file.as_deref() else {
        return; // off for a bare run
    };
    let identity = agentd::identity::Identity::from_env(&cfg.run_id);
    let trace_id =
        Some(agentd::obs::trace::resolve(&cfg.run_id, cfg.traceparent.as_deref()).trace_id);
    let report = agentd::report::RunReport::new(
        cfg.run_id.clone(),
        identity.instance,
        cfg.mode.as_str().to_string(),
        status,
        exit_code,
        has_usable_partial,
        agentd::report::Usage::default(),
        agentd::report::Refusals::default(),
        trace_id,
        started,
        ended,
    );
    report.write_to_file(path, log);
}

/// Build the root subagent's spawn payload from CLI config. The root gets the
/// full configured MCP set (scope narrows only for *child* subagents).
fn root_payload(cfg: &Config) -> SpawnPayload {
    // ~10 years if no deadline, so the child's `Instant + ms` never overflows.
    let deadline_ms = cfg
        .deadline
        .map(|d| d.as_millis() as u64)
        .unwrap_or(315_360_000_000);
    SpawnPayload {
        instruction: cfg.instruction.clone().unwrap_or_default(),
        output_contract: None,
        context_seed: Vec::new(),
        intelligence: IntelConfig {
            uri: cfg.intelligence.clone().unwrap_or_default(),
            token: cfg.intelligence_token.clone(),
            model: cfg.model.clone(),
        },
        mcp_servers: cfg.mcp_servers.clone(),
        a2a_peers: cfg.a2a_peers.clone(),
        limits: Limits {
            max_steps: cfg.max_steps,
            max_tokens: cfg.max_tokens,
            deadline_ms,
            max_depth: cfg.max_depth,
        },
        telemetry: Telemetry {
            run_id: cfg.run_id.clone(),
            agent_id: "0".into(),
            agent_path: "0".into(),
            // Same trace id as the supervisor's logs → the whole tree correlates
            // (resolve is deterministic for a given run id; RFC 0010).
            trace_id: Some(
                agentd::obs::trace::resolve(&cfg.run_id, cfg.traceparent.as_deref()).trace_id,
            ),
            log_level: cfg.log_level.as_str().into(),
            log_content: cfg.log_content,
        },
        depth: 0,
        exec_allow: cfg
            .exec_allow
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        warm: false, // root runs are one-shot; warm continue-sessions are daemon-minted
    }
}

/// Map a fatal subagent failure to an exit code. The control layer prefixes
/// errors with `intel:` / `mcp:` (RFC 0011 §5).
fn failed_exit(err: &str) -> i32 {
    if err.contains("intel") {
        exit::INTEL_UNAVAILABLE
    } else if err.contains("mcp") {
        exit::MCP_REQUIRED_DOWN
    } else {
        exit::GENERIC
    }
}

/// The agent's result goes to stdout (so a caller can capture it); a string
/// result prints verbatim, anything else as pretty JSON.
fn print_result(v: &Value) {
    match v {
        Value::String(s) => println!("{s}"),
        other => println!(
            "{}",
            serde_json::to_string_pretty(other).unwrap_or_default()
        ),
    }
}
