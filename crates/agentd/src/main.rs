//! agentd entry point.
//!
//! Dispatches between three roles of the one binary: the **supervisor** (the
//! normal CLI/daemon path), the **subagent** re-exec (M2), and the early-exit
//! `--help`/`--version`. The supervisor parses + validates config, installs
//! signal handlers, sets up logging, then drives the configured mode.
//!
//! M1 status: config/exit/logging/signals foundation is live; the supervisor
//! reactor, MCP client, intelligence client, and the agentic loop land across
//! M1–M3 (see docs/design/PLAN.md). Until then a validated run logs and exits
//! with a clear "scaffold only" notice — but `--help`, `--version`, and config
//! validation (exit 2) already behave per the contract.

use agentd::config::{Config, ConfigError, Mode};
use agentd::obs::log::{Comp, LogCtx, Logger};
use agentd::subagent::protocol::{IntelConfig, Limits, SpawnPayload, Telemetry};
use agentd::supervisor::reactor::{supervise_once, KillReason, SuperviseResult};
use agentd::triggers::mode::{run_reactive, run_scheduled};
use agentd::{exit, signals};
use serde_json::{json, Value};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let argv: Vec<String> = std::env::args().collect();

    // Hidden built-in mock MCP server (tests / dev):
    // `--internal-mock-mcp <uri> [--no-emit]`.
    if argv.get(1).map(String::as_str) == Some("--internal-mock-mcp") {
        let uri = argv.get(2).map(String::as_str).unwrap_or("mock://resource");
        let emit = !argv.iter().any(|a| a == "--no-emit");
        return agentd::mcp::mock::run(uri, emit);
    }

    // Hidden built-in mock LLM (M7 observe-suite):
    // `--internal-mock-llm <socket> [final|read|schedule]`.
    if argv.get(1).map(String::as_str) == Some("--internal-mock-llm") {
        let socket = argv.get(2).map(String::as_str).unwrap_or("/tmp/agentd-mock-llm.sock");
        let script = argv.get(3).map(String::as_str).unwrap_or("final");
        return agentd::intel::mock::run(socket, script);
    }

    // Subagent re-exec dispatch (M2). The supervisor sets this in the child's
    // environment; the child reads its spawn payload over the control channel
    // (stdin) rather than from CLI/env config.
    if std::env::var_os(agentd::subagent::protocol::SUBAGENT_ENV).is_some() {
        return agentd::subagent::control::run();
    }

    let env: Vec<(String, String)> = std::env::vars().collect();
    let cfg = match Config::load(&argv[1..], &env) {
        Ok(c) => c,
        Err(ConfigError::Help(s)) | Err(ConfigError::Version(s)) => {
            print!("{s}");
            return exit::SUCCESS;
        }
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

    // Rule of Two (RFC 0012 §3.2): refuse a grant that co-locates all three
    // lethal capability legs (untrusted input + sensitive data + egress) in one
    // agent, unless --allow-trifecta. Scope narrows monotonically (RFC 0009), so
    // enforcing on the root grant bounds the entire subagent tree.
    use agentd::sec::scope::{check_trifecta, TrifectaVerdict};
    match check_trifecta(cfg.trifecta_grant_tags(), cfg.allow_trifecta) {
        TrifectaVerdict::Ok => {}
        TrifectaVerdict::AllowedWithWarning => {
            log.warn("scope.trifecta_grant", json!({"allowed": true, "legs": ["untrusted_input", "sensitive", "egress"]}));
        }
        TrifectaVerdict::RefusedTrifecta => {
            log.error("scope.trifecta_refused", json!({"legs": ["untrusted_input", "sensitive", "egress"]}));
            eprintln!(
                "agentd: refused — this grant gives one agent all three lethal-trifecta legs \
                 (untrusted input + sensitive data + egress). Split the capabilities across \
                 subagents, or relaunch with --allow-trifecta."
            );
            return exit::USAGE;
        }
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
                    std::time::Duration::from_secs(5),
                );
                log.info("health.armed", json!({"file": path}));
            }
            // Opt-in HTTP scrape/probe surface (RFC 0010). Built only with
            // `--features metrics`; without it, `--metrics-addr` warns and is inert.
            if let Some(addr) = &cfg.metrics_addr {
                serve_metrics(addr, &log);
            }
            // Opt-in served self-MCP for composability (RFC 0005). Built only
            // with `--features serve-mcp`; otherwise `--serve-mcp` warns + is inert.
            let serve_handle = cfg.serve_mcp.as_ref().and_then(|spec| serve_self_mcp(spec, &exe, root_payload(&cfg), &cfg, &log));
            let code = match cfg.mode {
                Mode::Reactive => run_reactive(exe, root_payload(&cfg), &cfg, &log),
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
            let _ = serve_handle;
            code
        }
    }
}

/// Start the opt-in HTTP probe/scrape surface, or warn that this build can't.
/// Gated so the default build links no listener. RFC 0010.
#[cfg(feature = "metrics")]
fn serve_metrics(addr: &str, log: &Logger) {
    if let Err(e) = agentd::obs::serve::spawn(addr, log.clone()) {
        log.error("metrics.bind_fail", json!({"addr": addr, "err": e.to_string()}));
    }
}

#[cfg(not(feature = "metrics"))]
fn serve_metrics(addr: &str, log: &Logger) {
    log.warn("metrics.unavailable", json!({"addr": addr, "reason": "built without --features metrics"}));
}

/// Start the served self-MCP (composability, RFC 0005), or warn this build can't.
/// The `status` tool reports `cfg`; `subagent.spawn` runs fresh agents from the
/// daemon's root payload template (`base`).
#[cfg(feature = "serve-mcp")]
fn serve_self_mcp(
    spec: &str,
    exe: &std::path::Path,
    base: SpawnPayload,
    cfg: &Config,
    log: &Logger,
) -> Option<agentd::mcp::server::ServeHandle> {
    let path = spec.strip_prefix("unix:").unwrap_or(spec);
    let ctx = agentd::mcp::server::ServeCtx::new(
        cfg.run_id.clone(),
        cfg.mode.as_str().to_string(),
        exe.to_path_buf(),
        base,
        cfg.drain_timeout,
    );
    match agentd::mcp::server::serve(path, ctx, log.clone()) {
        Ok(handle) => Some(handle),
        Err(e) => {
            log.error("mcp.serve_fail", json!({"path": path, "err": e.to_string()}));
            None
        }
    }
}

#[cfg(not(feature = "serve-mcp"))]
fn serve_self_mcp(spec: &str, _exe: &std::path::Path, _base: SpawnPayload, _cfg: &Config, log: &Logger) -> Option<()> {
    log.warn("mcp.serve_unavailable", json!({"spec": spec, "reason": "built without --features serve-mcp"}));
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

    match supervise_once(exe, &payload, cfg.drain_timeout, log.clone()) {
        Ok(SuperviseResult::Completed(outcome)) => {
            print_result(&outcome.result);
            log.info("proc.exit", json!({"status": outcome.status.as_str(), "partial": outcome.partial}));
            exit::once_exit(outcome.status, outcome.partial)
        }
        Ok(SuperviseResult::Failed(err)) => {
            log.error("proc.exit", json!({"err": err}));
            eprintln!("agentd: {err}");
            failed_exit(&err)
        }
        Ok(SuperviseResult::Killed(reason)) => {
            log.warn("proc.exit", json!({"killed": format!("{reason:?}")}));
            eprintln!("agentd: run terminated ({reason:?})");
            match reason {
                KillReason::Deadline | KillReason::Stuck => exit::DEADLINE,
                KillReason::TreeBudget => exit::BUDGET,
                // A SIGTERM-initiated drain is a graceful shutdown → exit 0, never
                // 143 (RFC 0011 §5.1: we self-exit 0; 143 is OS-set when the
                // kernel kills us). A drain that overran its budget still exits 0
                // but logged `drain.timeout` + the SIGKILL ladder, so the
                // ungraceful teardown is auditable.
                KillReason::Drain => exit::SUCCESS,
            }
        }
        Err(e) => {
            log.error("proc.exit", json!({"err": format!("spawn: {e}")}));
            eprintln!("agentd: failed to spawn root subagent: {e}");
            exit::GENERIC
        }
    }
}

/// Build the root subagent's spawn payload from CLI config. The root gets the
/// full configured MCP set (scope narrows only for *child* subagents).
fn root_payload(cfg: &Config) -> SpawnPayload {
    // ~10 years if no deadline, so the child's `Instant + ms` never overflows.
    let deadline_ms = cfg.deadline.map(|d| d.as_millis() as u64).unwrap_or(315_360_000_000);
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
            trace_id: Some(agentd::obs::trace::resolve(&cfg.run_id, cfg.traceparent.as_deref()).trace_id),
            log_level: cfg.log_level.as_str().into(),
            log_content: cfg.log_content,
        },
        depth: 0,
        enable_exec: cfg.enable_exec,
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
        other => println!("{}", serde_json::to_string_pretty(other).unwrap_or_default()),
    }
}
