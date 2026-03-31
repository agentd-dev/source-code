//! Single-entry-point driver.
//!
//! The binary has no subcommands. It resolves the workflow once,
//! validates it, then runs it one-shot from a chosen start node and
//! prints the outcome as JSON. Overrides flow through CLI flags
//! (hand-parsed; no `clap`) and environment variables, with the
//! former winning.
//!
//! ```text
//! agentd [--config FILE]
//!        [--input FILE]           (trigger payload)
//!        [--start NAME]           (default: only manual start node)
//!        [--timeout-secs N]
//!        [--dry-run]
//!        [--validate-only]
//!        [--version] [--help]
//! ```
//!
//! All CLI flags have `AGENTD_*` env-var twins. Every workflow that
//! compiles today still runs; new knobs are optional.

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

    run_once_mode(doc, engine, options, &args)
}

// ---------------------------------------------------------------------------
// Arg parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    input_file: Option<PathBuf>,
    start: Option<String>,
    timeout_secs: u64,
    dry_run: bool,
    validate_only: bool,
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
        config: env_opt_path("AGENTD_CONFIG"),
        input_file: env_opt_path("AGENTD_INPUT"),
        start: std::env::var("AGENTD_START").ok(),
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
            "--timeout-secs" => {
                let v = require_value(argv, &mut i, arg)?;
                a.timeout_secs = v.parse::<u64>().map_err(|_| {
                    ArgErr::Usage(format!("--timeout-secs expects an integer; got `{v}`"))
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
         [--timeout-secs N]         AGENTD_TIMEOUT_SECS    per-run deadline (default 120)
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
