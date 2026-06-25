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
use agentd::{exit, signals};
use serde_json::json;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let argv: Vec<String> = std::env::args().collect();

    // Subagent re-exec dispatch (M2). The supervisor sets this in the child's
    // environment; the child reads its spawn payload over the control channel
    // rather than from CLI/env config.
    if std::env::var_os("AGENTD_SUBAGENT").is_some() {
        eprintln!("agentd: subagent mode is not yet implemented (M2 — docs/design/PLAN.md)");
        return exit::GENERIC;
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
        }),
    );

    match cfg.mode {
        Mode::Once | Mode::Loop | Mode::Reactive | Mode::Schedule => {
            log.warn(
                "proc.exit",
                json!({
                    "reason": "not_implemented",
                    "detail": "supervisor + agentic loop land in M1-M3",
                }),
            );
            eprintln!(
                "agentd {}: scaffold only — '{}' mode runs once M1-M3 land (docs/design/PLAN.md)",
                agentd::VERSION,
                cfg.mode.as_str()
            );
            exit::GENERIC
        }
    }
}
