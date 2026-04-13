//! Single-entry-point driver.
//!
//! The binary has no subcommands. It resolves the workflow once,
//! infers its operating mode from the workflow's declarations, then
//! runs. Overrides flow through CLI flags (hand-parsed; no `clap`)
//! and environment variables, with the former winning.
//!
//! ```text
//! agentd [--config FILE]
//!        [--input FILE]           (one-shot mode; trigger payload)
//!        [--start NAME]           (default: only manual start node)
//!        [--mode once|serve]
//!        [--bind HOST:PORT]       (server mode override)
//!        [--timeout-secs N]
//!        [--dry-run]
//!        [--validate-only]
//!        [--version] [--help]
//! ```
//!
//! All CLI flags have `AGENTD_*` env-var twins. Every workflow that
//! compiles today still runs; new knobs are optional.
//!
//! **Mode inference.** A workflow with at least one `[[http_routes]]`
//! entry switches the binary into server mode; otherwise the binary
//! runs once and exits. Override with `--mode serve|once` if the
//! default is wrong.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::engine::{Engine, HandlerRegistry, RunOptions, TriggerMeta};
use crate::workflow::{self, WorkflowDoc};

pub const EXIT_OK: u8 = 0;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_SEMANTIC: u8 = 5;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(argv: Vec<String>) -> ExitCode {
    let args = match parse_args(&argv[1..]) {
        Ok(a) => a,
        Err(ArgErr::Usage(msg)) => {
            eprintln!("agentd: {msg}");
            print_help();
            return ExitCode::from(EXIT_USAGE);
        }
        Err(ArgErr::ShowHelp) => {
            print_help();
            return ExitCode::from(EXIT_OK);
        }
        Err(ArgErr::ShowVersion) => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            return ExitCode::from(EXIT_OK);
        }
    };

    // Resolve the workflow.
    let doc = match load_workflow(&args) {
        Ok(d) => d,
        Err(msg) => {
            eprintln!("agentd: {msg}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    // Pre-validate — saves the operator a confusing engine error.
    let report = workflow::validate(&doc);
    if !report.ok() {
        emit_validation_report(&doc.name, &report);
        return ExitCode::from(EXIT_SEMANTIC);
    }
    if args.validate_only {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "workflow": doc.name,
                "ok": true,
            }))
            .unwrap()
        );
        return ExitCode::from(EXIT_OK);
    }

    // Build the engine.
    let engine = build_engine(&doc);
    let options = RunOptions {
        timeout: Duration::from_secs(args.timeout_secs.max(1)),
        dry_run: args.dry_run,
    };

    match resolve_mode(&doc, args.mode.as_deref()) {
        Mode::Serve => run_serve_mode(doc, engine, options, &args),
        Mode::Once => run_once_mode(doc, engine, options, &args),
    }
}

// ---------------------------------------------------------------------------
// Mode resolution
// ---------------------------------------------------------------------------

enum Mode {
    Serve,
    Once,
}

fn resolve_mode(doc: &WorkflowDoc, override_: Option<&str>) -> Mode {
    match override_ {
        Some("serve") => Mode::Serve,
        Some("once") => Mode::Once,
        _ if !doc.http_routes.is_empty() => Mode::Serve,
        _ => Mode::Once,
    }
}

// ---------------------------------------------------------------------------
// Arg parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    input_file: Option<PathBuf>,
    start: Option<String>,
    mode: Option<String>,
    bind: Option<String>,
    timeout_secs: u64,
    dry_run: bool,
    validate_only: bool,
    drain_timeout_secs: u64,
}

#[derive(Debug)]
enum ArgErr {
    Usage(String),
    ShowHelp,
    ShowVersion,
}

fn parse_args(argv: &[String]) -> Result<Args, ArgErr> {
    let mut a = Args {
        timeout_secs: env_u64("AGENTD_TIMEOUT_SECS", 120),
        drain_timeout_secs: env_u64("AGENTD_DRAIN_TIMEOUT_SECS", 30),
        config: env_opt_path("AGENTD_CONFIG"),
        input_file: env_opt_path("AGENTD_INPUT"),
        start: std::env::var("AGENTD_START").ok(),
        mode: std::env::var("AGENTD_MODE").ok(),
        bind: std::env::var("AGENTD_HTTP_BIND").ok(),
        dry_run: env_bool("AGENTD_DRY_RUN"),
        validate_only: env_bool("AGENTD_VALIDATE_ONLY"),
    };

    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--help" | "-h" => return Err(ArgErr::ShowHelp),
            "--version" | "-V" => return Err(ArgErr::ShowVersion),
            "--config" | "-c" => {
                a.config = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--input" | "--input-file" | "-i" => {
                a.input_file = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--start" | "-s" => {
                a.start = Some(require_value(argv, &mut i, arg)?.to_string());
            }
            "--mode" => {
                a.mode = Some(require_value(argv, &mut i, arg)?.to_string());
            }
            "--bind" | "-b" => {
                a.bind = Some(require_value(argv, &mut i, arg)?.to_string());
            }
            "--timeout-secs" => {
                let v = require_value(argv, &mut i, arg)?;
                a.timeout_secs = v.parse::<u64>().map_err(|_| {
                    ArgErr::Usage(format!("--timeout-secs expects an integer; got `{v}`"))
                })?;
            }
            "--drain-timeout-secs" => {
                let v = require_value(argv, &mut i, arg)?;
                a.drain_timeout_secs = v.parse::<u64>().map_err(|_| {
                    ArgErr::Usage(format!(
                        "--drain-timeout-secs expects an integer; got `{v}`"
                    ))
                })?;
            }
            "--dry-run" => a.dry_run = true,
            "--validate-only" => a.validate_only = true,
            other => {
                return Err(ArgErr::Usage(format!("unknown argument `{other}`")));
            }
        }
        i += 1;
    }
    Ok(a)
}

fn require_value<'a>(argv: &'a [String], idx: &mut usize, flag: &str) -> Result<&'a str, ArgErr> {
    *idx += 1;
    argv.get(*idx)
        .map(String::as_str)
        .ok_or_else(|| ArgErr::Usage(format!("{flag} requires a value")))
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_opt_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

fn env_bool(key: &str) -> bool {
    matches!(
        std::env::var(key).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

// ---------------------------------------------------------------------------
// Workflow loading
// ---------------------------------------------------------------------------

fn load_workflow(args: &Args) -> Result<WorkflowDoc, String> {
    if let Some(path) = &args.config {
        let src = fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let doc = WorkflowDoc::from_toml(&src)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
        return Ok(doc);
    }
    Err("no workflow configured. Pass --config FILE or set AGENTD_CONFIG.".into())
}

// ---------------------------------------------------------------------------
// Engine construction
// ---------------------------------------------------------------------------

fn build_engine(doc: &WorkflowDoc) -> Engine {
    let mut registry = HandlerRegistry::with_builtin_controls();

    // Policy: the workflow's [policy] block when present, AllowAll
    // otherwise. Fail-closed enforcement is the manifest's job;
    // absence of the block is an explicit operator choice.
    let policy: crate::tools::policy::PolicyRef = match &doc.policy {
        Some(manifest) => Arc::new(crate::policy::ManifestPolicy::new(manifest.clone())),
        None => crate::tools::policy::allow_all(),
    };

    crate::tools::register_default_tools(&mut registry, policy);
    Engine::new(registry)
}

// ---------------------------------------------------------------------------
// One-shot mode
// ---------------------------------------------------------------------------

fn run_once_mode(doc: WorkflowDoc, engine: Engine, options: RunOptions, args: &Args) -> ExitCode {
    let start = match pick_once_start(&doc, args.start.as_deref()) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("agentd: {msg}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let input = match &args.input_file {
        Some(path) => match fs::read_to_string(path) {
            Ok(s) => serde_json::from_str::<Value>(&s).unwrap_or_else(|_| json!({ "raw": s })),
            Err(e) => {
                eprintln!("agentd: failed to read {}: {e}", path.display());
                return ExitCode::from(EXIT_USAGE);
            }
        },
        None => Value::Null,
    };

    match engine.run(&doc, &start, TriggerMeta::manual(input), options) {
        Ok(outcome) => {
            let success = outcome.is_success();
            println!("{}", serde_json::to_string_pretty(&outcome).unwrap());
            ExitCode::from(if success { EXIT_OK } else { EXIT_SEMANTIC })
        }
        Err(e) => {
            eprintln!("agentd: {e}");
            ExitCode::from(EXIT_SEMANTIC)
        }
    }
}

/// Choose a start node for one-shot mode.
/// 1. `--start NAME` wins.
/// 2. If exactly one manual start node exists, use it.
/// 3. If exactly one start node exists (any source), use it.
/// 4. Else error — operator must disambiguate.
fn pick_once_start(doc: &WorkflowDoc, override_: Option<&str>) -> Result<String, String> {
    use crate::workflow::model::StartSource;
    if let Some(name) = override_ {
        return Ok(name.to_string());
    }
    let manual: Vec<&str> = doc
        .start_nodes
        .iter()
        .filter(|s| s.source == StartSource::Manual)
        .map(|s| s.name.as_str())
        .collect();
    if manual.len() == 1 {
        return Ok(manual[0].to_string());
    }
    if doc.start_nodes.len() == 1 {
        return Ok(doc.start_nodes[0].name.clone());
    }
    Err(format!(
        "cannot pick a start node automatically ({} manual / {} total); pass --start NAME",
        manual.len(),
        doc.start_nodes.len()
    ))
}

// ---------------------------------------------------------------------------
// Serve mode
// ---------------------------------------------------------------------------

#[cfg(feature = "trigger-http")]
fn run_serve_mode(doc: WorkflowDoc, engine: Engine, options: RunOptions, args: &Args) -> ExitCode {
    use std::net::SocketAddr;

    if doc.http_routes.is_empty() {
        eprintln!("agentd: serve mode requires at least one [[http_routes]] entry");
        return ExitCode::from(EXIT_USAGE);
    }

    let bind = args
        .bind
        .clone()
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = match bind.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("agentd: invalid bind address `{bind}`: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let doc_arc = Arc::new(doc.clone());
    let server = crate::triggers::http::HttpServer::new(addr, doc_arc, Arc::new(engine), options)
        .with_drain_timeout(Duration::from_secs(args.drain_timeout_secs.max(1)));
    let handle = match server.spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("agentd: {e}");
            return ExitCode::from(EXIT_SEMANTIC);
        }
    };
    eprintln!(
        "agentd: workflow `{}` listening on http://{}/ ({} routes; drain_timeout={}s)",
        doc.name,
        handle.local_addr(),
        doc.http_routes.len(),
        args.drain_timeout_secs,
    );
    for r in &doc.http_routes {
        eprintln!("       {} {} → {}", r.method, r.path, r.start_node);
    }

    // Install SIGTERM/SIGINT handlers and block until a shutdown
    // signal flips the flag.
    crate::signals::install_shutdown_handlers();
    while !crate::signals::shutdown_requested() {
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("agentd: shutdown signal received; draining…");

    let clean = handle.shutdown_and_drain();
    eprintln!(
        "agentd: drain {}",
        if clean {
            "complete"
        } else {
            "timed out (forced exit)"
        }
    );
    if clean {
        ExitCode::from(EXIT_OK)
    } else {
        ExitCode::from(EXIT_SEMANTIC)
    }
}

/// Serve-mode stub for builds without the HTTP trigger. The mode
/// resolver can still pick Serve (workflow declares routes); the
/// binary refuses with a rebuild hint instead of silently running
/// once.
#[cfg(not(feature = "trigger-http"))]
fn run_serve_mode(
    doc: WorkflowDoc,
    _engine: Engine,
    _options: RunOptions,
    _args: &Args,
) -> ExitCode {
    eprintln!(
        "agentd: workflow `{}` wants serve mode but this build lacks the \
         `trigger-http` Cargo feature; rebuild with --features trigger-http",
        doc.name
    );
    ExitCode::from(EXIT_SEMANTIC)
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn emit_validation_report(workflow: &str, report: &workflow::ValidationReport) {
    let payload = json!({
        "workflow": workflow,
        "ok": report.ok(),
        "issues": report
            .issues
            .iter()
            .map(|i| json!({ "code": i.code, "message": i.message }))
            .collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&payload).unwrap());
}

fn print_help() {
    eprintln!(
        "\
agentd {version} — bounded workflow runtime

usage:
  agentd [--config FILE]            AGENTD_CONFIG          (required)
         [--input FILE]             AGENTD_INPUT           one-shot trigger payload
         [--start NAME]             AGENTD_START           explicit start node
         [--mode once|serve]        AGENTD_MODE            override auto-inferred mode
         [--bind HOST:PORT]         AGENTD_HTTP_BIND       server-mode bind (default 127.0.0.1:8080)
         [--timeout-secs N]         AGENTD_TIMEOUT_SECS    per-run deadline (default 120)
         [--drain-timeout-secs N]   AGENTD_DRAIN_TIMEOUT_SECS  graceful shutdown grace (default 30)
         [--dry-run]                AGENTD_DRY_RUN=1
         [--validate-only]          AGENTD_VALIDATE_ONLY=1
         [--version] [--help]

Design: rfcs/0001-bounded-workflow-runtime.md\
",
        version = env!("CARGO_PKG_VERSION")
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{Edge, Node, NodeKind, StartNode, StartSource};

    fn wf_with(starts: Vec<StartNode>) -> WorkflowDoc {
        WorkflowDoc {
            name: "t".into(),
            start_nodes: starts,
            nodes: vec![Node {
                id: "a".into(),
                kind: NodeKind::Terminate,
            }],
            edges: vec![] as Vec<Edge>,
            ..Default::default()
        }
    }

    fn manual(name: &str) -> StartNode {
        StartNode {
            name: name.into(),
            source: StartSource::Manual,
            entry_node: Some("a".into()),
        }
    }
    fn http_start(name: &str) -> StartNode {
        StartNode {
            name: name.into(),
            source: StartSource::Http,
            entry_node: Some("a".into()),
        }
    }

    #[test]
    fn pick_start_prefers_override() {
        let doc = wf_with(vec![manual("a"), manual("b")]);
        assert_eq!(pick_once_start(&doc, Some("b")).unwrap(), "b");
    }

    #[test]
    fn pick_start_uses_the_only_manual() {
        let doc = wf_with(vec![manual("only"), http_start("other")]);
        assert_eq!(pick_once_start(&doc, None).unwrap(), "only");
    }

    #[test]
    fn pick_start_uses_the_only_start_when_no_manual() {
        let doc = wf_with(vec![http_start("only")]);
        assert_eq!(pick_once_start(&doc, None).unwrap(), "only");
    }

    #[test]
    fn pick_start_errors_on_ambiguity() {
        let doc = wf_with(vec![manual("a"), manual("b")]);
        let err = pick_once_start(&doc, None).unwrap_err();
        assert!(err.contains("cannot pick a start node"));
    }

    #[test]
    fn mode_inferred_from_http_routes() {
        let mut doc = wf_with(vec![http_start("on_http")]);
        assert!(matches!(resolve_mode(&doc, None), Mode::Once));
        doc.http_routes.push(crate::workflow::model::HttpRoute {
            method: "POST".into(),
            path: "/x".into(),
            start_node: "on_http".into(),
            input_schema: None,
            auth: None,
            rate_limit: None,
        });
        assert!(matches!(resolve_mode(&doc, None), Mode::Serve));
    }

    #[test]
    fn mode_override_wins() {
        let doc = wf_with(vec![manual("a")]);
        assert!(matches!(resolve_mode(&doc, Some("serve")), Mode::Serve));
        assert!(matches!(resolve_mode(&doc, Some("once")), Mode::Once));
    }

    #[test]
    fn parse_args_flag_values() {
        let a = parse_args(&[
            "--config".into(),
            "/tmp/wf.toml".into(),
            "--start".into(),
            "main".into(),
            "--dry-run".into(),
        ])
        .unwrap();
        assert_eq!(a.config, Some(PathBuf::from("/tmp/wf.toml")));
        assert_eq!(a.start.as_deref(), Some("main"));
        assert!(a.dry_run);
    }

    #[test]
    fn parse_args_missing_value() {
        let err = parse_args(&["--config".into()]).unwrap_err();
        assert!(matches!(err, ArgErr::Usage(ref m) if m.contains("requires a value")));
    }

    #[test]
    fn parse_args_unknown_flag() {
        let err = parse_args(&["--mystery".into()]).unwrap_err();
        assert!(matches!(err, ArgErr::Usage(_)));
    }

    #[test]
    fn parse_args_help_is_its_own_error() {
        let err = parse_args(&["--help".into()]).unwrap_err();
        assert!(matches!(err, ArgErr::ShowHelp));
    }
}
