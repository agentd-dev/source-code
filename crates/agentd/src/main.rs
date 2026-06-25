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

use agentd::agentloop::runner::{run_root, LoopAbort};
use agentd::config::{Config, ConfigError, Mode};
use agentd::intel::client::IntelClient;
use agentd::mcp::client::McpClient;
use agentd::obs::log::{Comp, LogCtx, Logger};
use agentd::{exit, signals};
use serde_json::{json, Value};
use std::time::Duration;

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
        Mode::Once => run_once(&cfg, &log),
        other => {
            log.warn(
                "proc.exit",
                json!({"reason": "not_implemented", "mode": other.as_str()}),
            );
            eprintln!(
                "agentd {}: '{}' mode lands in M3-M4 (docs/design/PLAN.md); 'once' works now",
                agentd::VERSION,
                other.as_str()
            );
            exit::GENERIC
        }
    }
}

/// One-shot mode: build the intelligence client, connect the MCP servers, run
/// the root agent loop, print the result to stdout, and map the terminal
/// status to an exit code (RFC 0011 §5). stdout is the result; stderr is
/// telemetry.
fn run_once(cfg: &Config, log: &Logger) -> i32 {
    let intel = match IntelClient::from_config(cfg) {
        Ok(c) => c,
        Err(e) => {
            log.error("intel.config.fail", json!({"err": e.to_string()}));
            eprintln!("agentd: {e}");
            // An unsupported transport (e.g. https without --features tls) is a
            // build/config mismatch, not a runtime outage.
            return exit::USAGE;
        }
    };

    let mut servers = Vec::new();
    for spec in &cfg.mcp_servers {
        match McpClient::spawn(&spec.name, &spec.command, Duration::from_secs(60)) {
            Ok(mut client) => match client.initialize() {
                Ok(()) => {
                    log.info(
                        "mcp.connect",
                        json!({"server": spec.name, "tools": client.capabilities().supports_tools()}),
                    );
                    servers.push(client);
                }
                Err(e) => {
                    log.error("mcp.connect.fail", json!({"server": spec.name, "err": e.to_string()}));
                    eprintln!("agentd: MCP server '{}' handshake failed: {e}", spec.name);
                    return exit::MCP_REQUIRED_DOWN;
                }
            },
            Err(e) => {
                log.error("mcp.connect.fail", json!({"server": spec.name, "err": e.to_string()}));
                eprintln!("agentd: MCP server '{}' failed to spawn: {e}", spec.name);
                return exit::MCP_REQUIRED_DOWN;
            }
        }
    }

    match run_root(&intel, &servers, cfg, log) {
        Ok(outcome) => {
            print_result(&outcome.result);
            log.info("proc.exit", json!({"status": outcome.status.as_str(), "partial": outcome.partial}));
            exit::once_exit(outcome.status, outcome.partial)
        }
        Err(LoopAbort::Intel(m)) => {
            log.error("intel.error", json!({"err": m}));
            eprintln!("agentd: intelligence error: {m}");
            exit::INTEL_UNAVAILABLE
        }
        Err(LoopAbort::Mcp(m)) => {
            log.error("mcp.error", json!({"err": m}));
            eprintln!("agentd: mcp error: {m}");
            exit::MCP_REQUIRED_DOWN
        }
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
