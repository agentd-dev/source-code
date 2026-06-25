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
use agentd::triggers::mode::run_reactive;
use agentd::{exit, signals};
use serde_json::{json, Value};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let argv: Vec<String> = std::env::args().collect();

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

    let log = Logger::new(
        LogCtx {
            run_id: cfg.run_id.clone(),
            agent_id: "sup".into(),
            agent_path: "0".into(),
            comp: Comp::Supervisor,
            pid: std::process::id(),
            trace_id: None,
        },
        cfg.log_level,
    );
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

    match cfg.mode {
        Mode::Once => run_once(&cfg, &log),
        Mode::Reactive => match std::env::current_exe() {
            Ok(exe) => run_reactive(exe, root_payload(&cfg), &cfg, &log),
            Err(e) => {
                log.error("proc.exit", json!({"err": format!("current_exe: {e}")}));
                exit::GENERIC
            }
        },
        other => {
            log.warn(
                "proc.exit",
                json!({"reason": "not_implemented", "mode": other.as_str()}),
            );
            eprintln!(
                "agentd {}: '{}' mode lands in M4 (docs/design/PLAN.md); once + reactive work now",
                agentd::VERSION,
                other.as_str()
            );
            exit::GENERIC
        }
    }
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
                KillReason::Drain => exit::SUCCESS, // clean drain (M5 refines 0 vs 143)
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
            trace_id: None,
            log_level: cfg.log_level.as_str().into(),
        },
        depth: 0,
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
