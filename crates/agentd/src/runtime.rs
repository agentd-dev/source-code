//! Single-entry-point driver.
//!
//! The binary has no subcommands. It resolves the workflow once,
//! infers its operating mode from the workflow's declarations, then
//! runs. Overrides flow through CLI flags (hand-parsed; no `clap`)
//! and environment variables, with the former winning.
//!
//! ```text
//! agentd [--config FILE]         (default: embedded, if built in)
//!        [--input FILE]           (one-shot mode; trigger payload)
//!        [--start NAME]           (default: only manual start node)
//!        [--bind HOST:PORT]       (server mode override)
//!        [--timeout-secs N]
//!        [--intel-unix PATH]
//!        [--mcp-stdio "CMD ARGS"]
//!        [--dry-run]
//!        [--validate-only]
//!        [--log-level LEVEL] [--log-format text|json] [--quiet]
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

use crate::engine::{
    Engine, ExecutionOutcome, HandlerRegistry, RunOptions, StubHandler, TriggerMeta,
};
use crate::workflow::{self, WorkflowDoc};

pub const EXIT_OK: u8 = 0;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_SEMANTIC: u8 = 5;
/// A run suspended at a `pause_for_approval` node — resumable, neither
/// success nor failure. Distinct so scripts can branch on it.
pub const EXIT_PAUSED: u8 = 7;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(argv: Vec<String>) -> ExitCode {
    // `agentd inspect RUN.json` — render a run record. A leading
    // subcommand, resolved before flag parsing.
    if argv.get(1).map(String::as_str) == Some("inspect") {
        return run_inspect(argv.get(2).map(String::as_str));
    }

    let (args, tracing_overrides) = match parse_args(&argv[1..]) {
        Ok((a, t)) => (a, t),
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

    // Mode 3 (RFC 0006 §3): the agent compiles its own workflow from a
    // natural-language instruction, then runs it. Resolved before the
    // declared-workflow path below — here `--config` is the *base
    // environment* (policy / backends / MCP / budgets) the compiled
    // plan runs inside, loaded by run_instruction_mode itself.
    if let Some(instruction) = resolve_instruction(&args) {
        return run_instruction_mode(&instruction, &args, &tracing_overrides);
    }

    // Resolve the workflow. CLI / env --config wins; then an embedded
    // config if the build baked one in; then nothing (usage error).
    // No tracing yet — pre-init errors go to stderr as plain text so
    // the log target declared in the workflow takes effect from the
    // first emitted event.
    let loaded = match load_workflow(&args) {
        Ok(l) => l,
        Err(msg) => {
            eprintln!("agentd: {msg}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let LoadedWorkflow {
        mut doc,
        raw_bytes,
        sig_path,
    } = loaded;

    // CLI / env overrides on the signing config. These win over the
    // TOML block so operators can harden a deploy without editing
    // the workflow file. `--signing-key-file` replaces whatever key
    // the TOML pinned, useful for key rotation without redeploying
    // the manifest.
    if let Some(path) = &args.signing_key_file {
        let cfg = doc.signing.get_or_insert_with(Default::default);
        cfg.public_key_file = Some(path.clone());
        cfg.public_key_pem = None;
    }

    // Merge logging config: workflow `[logging]` → env → CLI → default.
    // Now we know the full spec, install the subscriber.
    let resolved = resolve_logging(&doc, &tracing_overrides);
    if let Err(e) = crate::observability::apply(&resolved) {
        eprintln!("agentd: failed to install log subscriber: {e}");
        return ExitCode::from(EXIT_SEMANTIC);
    }

    // Signature verification (RFC 0002). Runs BEFORE validate() so a
    // tampered manifest with otherwise-valid DAG shape still fails.
    let sig_source = resolve_signature_source(&sig_path);
    match crate::signing::verify_or_skip(
        doc.signing.as_ref(),
        &raw_bytes,
        sig_source,
        args.signing_required,
    ) {
        crate::signing::Outcome::Ok => {}
        crate::signing::Outcome::Refused(reason) => {
            eprintln!("agentd: workflow signature verification failed: {reason}");
            return ExitCode::from(EXIT_SEMANTIC);
        }
    }

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

    // Apply process-wide resource budgets (RLIMIT_AS /
    // RLIMIT_CPU) before the engine builds — any subsequent
    // allocation or CPU burn is subject to the cap. `setrlimit`
    // failures emit a warn audit event but don't abort.
    let budget_cfg = doc.budget.clone().unwrap_or_default();
    crate::budget::apply_rlimits(&budget_cfg);

    // Build the engine.
    let engine = match build_engine(&doc, &args) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let effective_timeout = budget_cfg.clamp_run_time(args.timeout_secs.max(1));
    let options = RunOptions {
        timeout: Duration::from_secs(effective_timeout),
        dry_run: args.dry_run,
    };

    // `--resume RUN_ID` continues a paused run from its checkpoint
    // instead of starting fresh.
    if args.resume.is_some() {
        return run_resume_mode(doc, engine, options, &args);
    }

    match resolve_mode(&doc, args.mode.as_deref()) {
        Mode::Serve => run_serve_mode(doc, engine, options, &args),
        Mode::Once => run_once_mode(doc, engine, options, &args),
    }
}

// ---------------------------------------------------------------------------
// Arg parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
#[allow(dead_code)] // every field is read via a flag parser path
struct Args {
    config: Option<PathBuf>,
    input_file: Option<PathBuf>,
    start: Option<String>,
    mode: Option<String>,
    bind: Option<String>,
    timeout_secs: u64,
    intel_unix: Option<PathBuf>,
    instructions: Option<PathBuf>,
    /// `--instruction TEXT` / `--instruction @file` /
    /// `--instruction-file PATH` / `AGENTD_INSTRUCTION` — Mode 3
    /// (RFC 0006 §3): compile a workflow from this natural-language
    /// instruction and run it. `@file` (or `--instruction-file`)
    /// reads the instruction from a file.
    instruction: Option<String>,
    /// `--goal TEXT` / `--goal @file` — the original name for Mode 3;
    /// kept as an alias of `--instruction`.
    goal: Option<String>,
    plan_only: bool,
    auto_approve: bool,
    plan_out: Option<PathBuf>,
    /// `--promote PATH` — save the compiled plan as a durable, versioned
    /// workflow artifact (the static Mode-1 form of the dynamism).
    promote: Option<PathBuf>,
    max_replans: u32,
    intel_http: Option<String>,
    intel_http_bearer_file: Option<PathBuf>,
    mcp_stdio: Option<Vec<String>>,
    dry_run: bool,
    validate_only: bool,
    drain_timeout_secs: u64,
    /// `--signing-required` — hardens every deploy by refusing to
    /// start on an unsigned workflow, even if the TOML's
    /// `[signing].required` is false (or the block is absent).
    signing_required: bool,
    /// `--signing-key-file PATH` — overrides the TOML's pinned key.
    /// Lets operators rotate keys without editing the manifest.
    signing_key_file: Option<PathBuf>,
    /// `--reload-file PATH` / `AGENTD_RELOAD_FILE` — cross-platform
    /// reload trigger. Background thread polls the file's mtime
    /// every 250 ms; on any change, flips the same
    /// `RELOAD_REQUESTED` flag `SIGHUP` does. Acts as the Windows
    /// SIGHUP replacement (no console-signal equivalent there) and
    /// an additional reload channel on Unix (e.g. a k8s downward-
    /// API projection whose content changes when a ConfigMap
    /// rotates). The file's content is ignored — any mtime bump
    /// triggers a reload.
    reload_file: Option<PathBuf>,
    /// `--record PATH` / `AGENTD_RECORD` — write a structured run record
    /// (per-node output + timing, cost, outcome) to PATH after a
    /// one-shot run. Render it with `agentd inspect PATH`.
    record: Option<PathBuf>,
    /// `--state-dir DIR` / `AGENTD_STATE_DIR` — where `pause_for_approval`
    /// writes checkpoints and `--resume` reads them.
    state_dir: Option<PathBuf>,
    /// `--resume RUN_ID` — continue a paused run from its checkpoint in
    /// `--state-dir`, instead of starting fresh.
    resume: Option<String>,
}

/// CLI / env overrides for logging. `None` means "defer to workflow
/// `[logging]` or the built-in default". `Some(false)` on
/// `--quiet` wins over every other `enabled` source.
#[derive(Debug, Default)]
struct TracingOverrides {
    level: Option<String>,
    format: Option<crate::observability::Format>,
    target: Option<crate::observability::LogTarget>,
    /// `Some(false)` iff `--quiet` or the env equivalent set it.
    enabled_false: bool,
}

#[derive(Debug)]
enum ArgErr {
    Usage(String),
    ShowHelp,
    ShowVersion,
}

fn parse_args(argv: &[String]) -> Result<(Args, TracingOverrides), ArgErr> {
    let mut a = Args {
        timeout_secs: env_u64("AGENTD_TIMEOUT_SECS", 120),
        drain_timeout_secs: env_u64("AGENTD_DRAIN_TIMEOUT_SECS", 30),
        config: env_opt_path("AGENTD_CONFIG"),
        input_file: env_opt_path("AGENTD_INPUT"),
        start: std::env::var("AGENTD_START").ok(),
        mode: std::env::var("AGENTD_MODE").ok(),
        bind: std::env::var("AGENTD_HTTP_BIND").ok(),
        intel_unix: env_opt_path("AGENTD_INTEL_UNIX"),
        instructions: env_opt_path("AGENTD_INSTRUCTIONS"),
        instruction: std::env::var("AGENTD_INSTRUCTION")
            .ok()
            .filter(|s| !s.trim().is_empty()),
        goal: std::env::var("AGENTD_GOAL")
            .ok()
            .filter(|s| !s.trim().is_empty()),
        plan_only: env_bool("AGENTD_PLAN_ONLY"),
        promote: env_opt_path("AGENTD_PROMOTE"),
        auto_approve: env_bool("AGENTD_AUTO_APPROVE"),
        plan_out: env_opt_path("AGENTD_PLAN_OUT"),
        max_replans: env_u64("AGENTD_MAX_REPLANS", 2) as u32,
        intel_http: std::env::var("AGENTD_INTEL_HTTP")
            .ok()
            .filter(|s| !s.trim().is_empty()),
        intel_http_bearer_file: env_opt_path("AGENTD_INTEL_HTTP_BEARER_FILE"),
        mcp_stdio: std::env::var("AGENTD_MCP_STDIO").ok().and_then(|raw| {
            let parts: Vec<String> = raw.split_whitespace().map(String::from).collect();
            if parts.is_empty() { None } else { Some(parts) }
        }),
        dry_run: env_bool("AGENTD_DRY_RUN"),
        validate_only: env_bool("AGENTD_VALIDATE_ONLY"),
        signing_required: env_bool("AGENTD_SIGNING_REQUIRED"),
        signing_key_file: env_opt_path("AGENTD_SIGNING_KEY_FILE"),
        reload_file: env_opt_path("AGENTD_RELOAD_FILE"),
        record: env_opt_path("AGENTD_RECORD"),
        state_dir: env_opt_path("AGENTD_STATE_DIR"),
        resume: std::env::var("AGENTD_RESUME")
            .ok()
            .filter(|s| !s.trim().is_empty()),
    };
    let mut t = TracingOverrides {
        level: std::env::var("AGENTD_LOG")
            .ok()
            .filter(|s| !s.trim().is_empty()),
        format: std::env::var("AGENTD_LOG_FORMAT")
            .ok()
            .as_deref()
            .and_then(crate::observability::Format::parse),
        target: std::env::var("AGENTD_LOG_TARGET")
            .ok()
            .as_deref()
            .and_then(crate::observability::LogTarget::parse),
        enabled_false: env_bool("AGENTD_QUIET"),
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
            "--instruction" => {
                a.instruction = Some(require_value(argv, &mut i, arg)?.to_string());
            }
            "--instruction-file" => {
                // Sugar for `--instruction @PATH`; reuses the `@file`
                // expansion in run_instruction_mode.
                let p = require_value(argv, &mut i, arg)?;
                a.instruction = Some(format!("@{p}"));
            }
            "--goal" => {
                a.goal = Some(require_value(argv, &mut i, arg)?.to_string());
            }
            "--plan-only" => a.plan_only = true,
            "--promote" => {
                a.promote = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--auto-approve" => a.auto_approve = true,
            "--plan-out" => {
                a.plan_out = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--max-replans" => {
                let v = require_value(argv, &mut i, arg)?;
                a.max_replans = v.parse::<u32>().map_err(|_| {
                    ArgErr::Usage(format!("--max-replans expects an integer; got `{v}`"))
                })?;
            }
            "--instructions" => {
                a.instructions = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--intel-unix" => {
                a.intel_unix = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--intel-http" => {
                a.intel_http = Some(require_value(argv, &mut i, arg)?.to_string());
            }
            "--intel-http-bearer-file" => {
                a.intel_http_bearer_file = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--mcp-stdio" => {
                let raw = require_value(argv, &mut i, arg)?;
                let parts: Vec<String> = raw.split_whitespace().map(String::from).collect();
                if parts.is_empty() {
                    return Err(ArgErr::Usage(
                        "--mcp-stdio requires a non-empty command".into(),
                    ));
                }
                a.mcp_stdio = Some(parts);
            }
            "--dry-run" => a.dry_run = true,
            "--validate-only" => a.validate_only = true,
            "--signing-required" => a.signing_required = true,
            "--signing-key-file" => {
                a.signing_key_file = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--reload-file" => {
                a.reload_file = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--record" => {
                a.record = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--state-dir" => {
                a.state_dir = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
            }
            "--resume" => {
                a.resume = Some(require_value(argv, &mut i, arg)?.to_string());
            }
            "--log-level" => {
                t.level = Some(require_value(argv, &mut i, arg)?.to_string());
            }
            "--log-format" => {
                let raw = require_value(argv, &mut i, arg)?;
                t.format = Some(crate::observability::Format::parse(raw).ok_or_else(|| {
                    ArgErr::Usage(format!(
                        "--log-format expects `text` or `json`; got `{raw}`"
                    ))
                })?);
            }
            "--log-target" => {
                let raw = require_value(argv, &mut i, arg)?;
                t.target = Some(crate::observability::LogTarget::parse(raw).ok_or_else(|| {
                    ArgErr::Usage(format!(
                        "--log-target expects `stderr`, `stdout`, or `file:PATH`; got `{raw}`"
                    ))
                })?);
            }
            "--quiet" => t.enabled_false = true,
            other => {
                return Err(ArgErr::Usage(format!("unknown argument `{other}`")));
            }
        }
        i += 1;
    }
    Ok((a, t))
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
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
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
        Some("1" | "true" | "TRUE" | "True")
    )
}

/// Merge the three config sources into one [`ResolvedLogging`].
///
/// Precedence per field: **CLI override → env → workflow → default**.
/// `--quiet` is special: if set, `enabled = false` unconditionally.
fn resolve_logging(
    doc: &WorkflowDoc,
    overrides: &TracingOverrides,
) -> crate::observability::ResolvedLogging {
    let default = crate::observability::ResolvedLogging::default();
    let from_wf = doc.logging.clone().unwrap_or_default();

    let level = overrides
        .level
        .clone()
        .or(from_wf.level)
        .unwrap_or(default.level);
    let format = overrides
        .format
        .or(from_wf.format)
        .unwrap_or(default.format);
    let target = overrides
        .target
        .clone()
        .or(from_wf.target)
        .unwrap_or(default.target);
    let enabled = if overrides.enabled_false {
        false
    } else {
        from_wf.enabled.unwrap_or(default.enabled)
    };

    crate::observability::ResolvedLogging {
        level,
        format,
        target,
        enabled,
        audit: from_wf.audit,
        rotation: from_wf.rotation.unwrap_or_default(),
        otel: from_wf.otel,
    }
}

// ---------------------------------------------------------------------------
// Workflow loading
// ---------------------------------------------------------------------------

/// What `load_workflow` returns — the parsed doc plus everything
/// [`crate::signing::verify_or_skip`] needs to run against it.
struct LoadedWorkflow {
    doc: WorkflowDoc,
    /// Raw TOML bytes exactly as they sit on disk (or in the baked-in
    /// blob). Signature verification needs these unmodified; TOML
    /// parsing is lossy with respect to whitespace and comments.
    raw_bytes: Vec<u8>,
    /// Where to look for a detached signature — alongside the external
    /// file when `--config` is used, or the embedded blob when Mode B.
    sig_path: Option<PathBuf>,
}

fn load_workflow(args: &Args) -> Result<LoadedWorkflow, String> {
    if let Some(path) = &args.config {
        let src = fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let doc = WorkflowDoc::from_toml(&src)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
        // External signature convention: `<config>.toml.sig` next to
        // the TOML. Absent file is handled by the verifier based on
        // the `required` flag.
        let mut sig_path = path.clone();
        let mut ext = sig_path
            .extension()
            .map(|e| e.to_owned())
            .unwrap_or_default();
        ext.push(".sig");
        sig_path.set_extension(ext);
        return Ok(LoadedWorkflow {
            doc,
            raw_bytes: src.into_bytes(),
            sig_path: Some(sig_path),
        });
    }
    if let Some(src) = crate::embedded::EMBEDDED_CONFIG {
        let doc = WorkflowDoc::from_toml(src)
            .map_err(|e| format!("embedded workflow is malformed: {e}"))?;
        return Ok(LoadedWorkflow {
            doc,
            raw_bytes: src.as_bytes().to_vec(),
            sig_path: None,
        });
    }
    Err(
        "no workflow configured. Pass --config FILE / AGENTD_CONFIG, or rebuild \
         with `AGENTD_EMBED_CONFIG=path/to/wf.toml cargo build` to bake one in."
            .into(),
    )
}

/// Pick the right [`crate::signing::SignatureSource`] for the current
/// workflow source. External `--config` path → sibling `.sig` file.
/// Embedded mode → build-baked `EMBEDDED_CONFIG_SIG` bytes when
/// present; otherwise `None` and the verifier decides based on
/// `required`.
fn resolve_signature_source(
    sig_path: &Option<PathBuf>,
) -> crate::signing::SignatureSource<'static> {
    if let Some(path) = sig_path {
        return crate::signing::SignatureSource::FilePath(path.clone());
    }
    if let Some(bytes) = crate::embedded::EMBEDDED_CONFIG_SIG {
        return crate::signing::SignatureSource::RawBytes(bytes);
    }
    crate::signing::SignatureSource::None
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
        _ if has_long_lived_trigger(doc) => Mode::Serve,
        _ => Mode::Once,
    }
}

/// True when the workflow declares any trigger type that requires
/// a long-running process — cron, interval, or fs_watch. Without
/// these the binary runs once and exits; with them it needs to
/// stay alive to fire the scheduled work.
fn has_long_lived_trigger(doc: &WorkflowDoc) -> bool {
    use crate::workflow::model::Trigger;
    doc.triggers.iter().any(|t| {
        matches!(
            t,
            Trigger::Cron { .. } | Trigger::Interval { .. } | Trigger::FsWatch { .. }
        )
    })
}

// ---------------------------------------------------------------------------
// Engine construction
// ---------------------------------------------------------------------------

fn build_engine(doc: &WorkflowDoc, args: &Args) -> Result<Engine, ExitCode> {
    let (reloadable_policy, mcp_allowlist) = build_policy(doc);
    // `Arc<ReloadablePolicy>` coerces to `Arc<dyn Policy>` at the
    // handler-registration boundary. Keep the typed handle around so
    // SIGHUP can reach back through `engine.reload.policy` to swap
    // the inner manifest without touching the registry.
    let policy_for_tools: crate::tools::policy::PolicyRef = reloadable_policy.clone();

    let budget = Arc::new(crate::budget::BudgetTracker::new(
        doc.budget
            .as_ref()
            .unwrap_or(&crate::budget::BudgetConfig::default()),
    ));

    let metrics = crate::observability::Metrics::new();
    let mut registry = HandlerRegistry::with_builtin_controls();
    crate::tools::register_default_tools(&mut registry, policy_for_tools.clone(), budget.clone());

    // Intelligence adapter (Unix or HTTP). Wrap whichever client
    // the operator selected in a `ReloadableIntelClient` so the
    // SIGHUP path can swap its inner (e.g. rotate the bearer token
    // from a Vault side-car).
    let intel_reload: Option<Arc<crate::intelligence::client::ReloadableIntelClient>> =
        if let Some(path) = &args.intel_unix {
            #[cfg(unix)]
            {
                let initial: Box<dyn crate::intelligence::client::IntelligenceClient> =
                    Box::new(crate::intelligence::client::UnixClient::new(
                        path.clone(),
                        Duration::from_secs(args.timeout_secs.max(1)),
                    ));
                let reloadable = Arc::new(crate::intelligence::client::ReloadableIntelClient::new(
                    initial,
                ));
                Some(reloadable)
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                eprintln!(
                    "agentd: --intel-unix is Unix-only; use --intel-http on Windows \
                     (rebuild with --features intel-http if needed)"
                );
                return Err(ExitCode::from(EXIT_USAGE));
            }
        } else {
            #[cfg(feature = "intel-http")]
            let h = match &args.intel_http {
                Some(url) => {
                    let bearer = read_intel_http_bearer(args)?;
                    match crate::intelligence::client::HttpClient::with_bearer(
                        url,
                        Duration::from_secs(args.timeout_secs.max(1)),
                        bearer,
                    ) {
                        Ok(client) => {
                            let initial: Box<dyn crate::intelligence::client::IntelligenceClient> =
                                Box::new(client);
                            let reloadable = Arc::new(
                                crate::intelligence::client::ReloadableIntelClient::new(initial),
                            );
                            Some(reloadable)
                        }
                        Err(e) => {
                            eprintln!("agentd: bad --intel-http URL: {e}");
                            return Err(ExitCode::from(EXIT_USAGE));
                        }
                    }
                }
                None => None,
            };
            #[cfg(not(feature = "intel-http"))]
            let h: Option<Arc<crate::intelligence::client::ReloadableIntelClient>> = {
                if args.intel_http.is_some() {
                    eprintln!(
                        "agentd: --intel-http requires the `intel-http` Cargo feature; \
                     rebuild with --features intel-http"
                    );
                    return Err(ExitCode::from(EXIT_USAGE));
                }
                None
            };
            h
        };

    // Named intelligence backends (RFC 0006 §3). Compose the CLI
    // `default` transport (when present) with every
    // `[[intelligence.backends]]` entry; the llm_infer handler
    // resolves nodes' `backend` names against this map.
    let named_defs: &[crate::intelligence::backends::BackendDef] = doc
        .intelligence
        .as_ref()
        .map(|i| i.backends.as_slice())
        .unwrap_or(&[]);
    if let Err(e) = crate::intelligence::backends::BackendDef::validate_list(named_defs) {
        eprintln!("agentd: {e}");
        return Err(ExitCode::from(EXIT_USAGE));
    }
    let mut backend_map: std::collections::HashMap<
        String,
        Arc<crate::intelligence::client::ReloadableIntelClient>,
    > = std::collections::HashMap::new();
    if let Some(default) = &intel_reload {
        backend_map.insert("default".to_string(), default.clone());
    }
    // `named_backends` is only mutated under `intel-remote`; the
    // feature-off build leaves it empty.
    #[allow(unused_mut)]
    let mut named_backends: std::collections::HashMap<
        String,
        Arc<crate::intelligence::client::ReloadableIntelClient>,
    > = std::collections::HashMap::new();
    #[cfg(feature = "intel-remote")]
    for def in named_defs {
        let client = match crate::intelligence::providers::RemoteClient::from_def(
            def,
            Duration::from_secs(args.timeout_secs.max(1)),
        ) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("agentd: {e}");
                return Err(ExitCode::from(EXIT_USAGE));
            }
        };
        let reloadable = Arc::new(crate::intelligence::client::ReloadableIntelClient::new(
            Box::new(client),
        ));
        named_backends.insert(def.name.clone(), reloadable.clone());
        backend_map.insert(def.name.clone(), reloadable);
    }
    #[cfg(not(feature = "intel-remote"))]
    if !named_defs.is_empty() {
        eprintln!(
            "agentd: workflow declares [[intelligence.backends]] but this build \
             lacks the `intel-remote` Cargo feature; rebuild with --features intel-remote"
        );
        return Err(ExitCode::from(EXIT_USAGE));
    }
    let backend_arc = Arc::new(backend_map.clone());
    if !backend_map.is_empty() {
        crate::intelligence::handler::register(
            &mut registry,
            backend_arc.clone(),
            budget.clone(),
            metrics.clone(),
        );
    }

    // MCP server registry. Composes every `[[mcp_servers]]` entry
    // plus (when set) the legacy `--mcp-stdio CMD ARG` as an
    // implicit `{name = "default", ..}` server. Each entry's child
    // process is wrapped in a `ReloadableMcpClient` and its
    // allowlist in a `ReloadableMcpAllowlist` so the SIGHUP reload path can
    // respawn or swap policy per-server without rebuilding the
    // handler registry.
    let mcp_registry = build_mcp_registry(doc, args, &mcp_allowlist)?;
    if !mcp_registry.is_empty() {
        crate::mcp::handler::register(&mut registry, mcp_registry.clone());
    }

    // agent_loop node (RFC 0006 §2) — bounded ReAct inside a node.
    // Registered only when at least one backend exists; its tool
    // broker reuses the same policy/budget/MCP gates as declared
    // node kinds. The system prompt comes from --instructions.
    if !backend_map.is_empty() {
        let mcp_opt = if mcp_registry.is_empty() {
            None
        } else {
            Some(mcp_registry.clone())
        };
        crate::agent::loop_node::register(
            &mut registry,
            backend_arc.clone(),
            policy_for_tools.clone(),
            budget.clone(),
            mcp_opt,
            args.instructions
                .as_ref()
                .and_then(|p| crate::agent::AgentInstructions::load(p).ok())
                .and_then(|i| i.system),
            metrics.clone(),
        );
    }

    registry.set_fallback(Box::new(StubHandler));
    Ok(Engine::with_metrics(registry, metrics)
        .with_state_dir(args.state_dir.clone())
        .with_reload_handles(crate::engine::ReloadHandles {
            policy: Some(reloadable_policy),
            intel: intel_reload,
            intel_backends: named_backends,
            mcp: Some(mcp_registry),
        }))
}

/// Build the process-wide MCP server registry. Sources:
///   * every `[[mcp_servers]]` entry in the workflow TOML — each
///     with its own name, spawn command, and allowlist.
///   * `--mcp-stdio CMD ARG...` — legacy single-server path, mapped
///     to a `{name = "default"}` entry with the global `[policy.mcp]`
///     allowlist. Rejected when TOML also declares a `default`
///     entry (name collision).
fn build_mcp_registry(
    doc: &WorkflowDoc,
    args: &Args,
    fallback_allowlist: &crate::mcp::allowlist::McpAllowlist,
) -> std::result::Result<Arc<crate::mcp::McpRegistry>, ExitCode> {
    if let Err(e) = crate::mcp::config::McpServerDef::validate_list(&doc.mcp_servers) {
        eprintln!("agentd: {e}");
        return Err(ExitCode::from(EXIT_USAGE));
    }

    let mut handles: Vec<Arc<crate::mcp::McpServerHandle>> = Vec::new();
    for def in &doc.mcp_servers {
        let handle = spawn_mcp_handle(def)?;
        handles.push(handle);
    }

    if let Some(cmd) = &args.mcp_stdio {
        if doc.mcp_servers.iter().any(|d| d.name == "default") {
            eprintln!(
                "agentd: --mcp-stdio conflicts with an `[[mcp_servers]]` entry named `default`; \
                 use the TOML list or remove the CLI flag (not both)"
            );
            return Err(ExitCode::from(EXIT_USAGE));
        }
        // Legacy path: map CLI argv to a default-named server. Use
        // the global `[policy.mcp]` allowlist from the policy block
        // to preserve the pre-registry semantic (that allowlist was
        // the only one).
        let def = crate::mcp::config::from_cli_stdio(cmd.clone());
        let mut handle = spawn_mcp_handle(&def)?;
        // Override the allowlist with the existing `[policy.mcp]`
        // one so workflows that haven't migrated keep the same
        // tool-allow list they had before.
        Arc::get_mut(&mut handle)
            .expect("freshly-created Arc has no other clones")
            .allowlist = Arc::new(crate::mcp::allowlist::ReloadableMcpAllowlist::new(
            fallback_allowlist.clone(),
        ));
        handles.push(handle);
    }

    Ok(Arc::new(crate::mcp::McpRegistry::new(handles)))
}

/// Spawn one MCP stdio child and wrap it in the reloadable handles.
/// Failure to spawn is fatal at startup — operators should see it
/// immediately rather than discover it on the first request.
fn spawn_mcp_handle(
    def: &crate::mcp::config::McpServerDef,
) -> std::result::Result<Arc<crate::mcp::McpServerHandle>, ExitCode> {
    let (head, tail) = def
        .command
        .split_first()
        .expect("validate_list enforces non-empty command");
    match crate::mcp::client::StdioMcpClient::spawn(head.clone(), tail) {
        Ok(client) => {
            let initial: Box<dyn crate::mcp::client::McpClient> = Box::new(client);
            let allowlist = crate::mcp::allowlist::McpAllowlist {
                allowed_tools: def.allow_tools.clone(),
                allowed_resource_patterns: def.allow_resources.clone(),
            };
            Ok(Arc::new(crate::mcp::McpServerHandle {
                name: def.name.clone(),
                client: Arc::new(crate::mcp::client::ReloadableMcpClient::new(initial)),
                allowlist: Arc::new(crate::mcp::allowlist::ReloadableMcpAllowlist::new(
                    allowlist,
                )),
            }))
        }
        Err(e) => {
            eprintln!("agentd: failed to start MCP server `{}`: {e}", def.name);
            Err(ExitCode::from(EXIT_USAGE))
        }
    }
}

/// Resolve the intel bearer token from `--intel-http-bearer-file` or
/// the `AGENTD_INTEL_HTTP_BEARER` env var. Extracted so the reload
/// path can share the exact same source-of-truth.
#[cfg(feature = "intel-http")]
fn read_intel_http_bearer(args: &Args) -> std::result::Result<Option<String>, ExitCode> {
    match &args.intel_http_bearer_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) => {
                eprintln!(
                    "agentd: failed to read intel bearer from {}: {e}",
                    path.display()
                );
                Err(ExitCode::from(EXIT_USAGE))
            }
        },
        None => Ok(std::env::var("AGENTD_INTEL_HTTP_BEARER")
            .ok()
            .filter(|s| !s.trim().is_empty())),
    }
}

/// Build a [`ReloadablePolicy`] from the workflow's `[policy]` block
/// and extract the MCP allowlist. The returned `Arc<ReloadablePolicy>`
/// is both handed to the tool handlers (via `Arc<dyn Policy>`
/// coercion) AND kept as the reload handle in `engine.reload.policy`.
fn build_policy(
    doc: &WorkflowDoc,
) -> (
    Arc<crate::tools::policy::ReloadablePolicy>,
    crate::mcp::allowlist::McpAllowlist,
) {
    let (inner, allowlist) = build_inner_policy(doc);
    (
        Arc::new(crate::tools::policy::ReloadablePolicy::new(inner)),
        allowlist,
    )
}

/// Construct the concrete `Box<dyn Policy>` that backs the reloadable
/// wrapper. Called once at startup and again on every SIGHUP, so the
/// same error-handling contract applies both times: a Rego compile
/// failure / feature gate / file read error exits the process on
/// startup and surfaces as a reload-failed audit event on reload
/// (the process stays alive on the old policy — see `run_reload`).
fn build_inner_policy(
    doc: &WorkflowDoc,
) -> (
    Box<dyn crate::tools::policy::Policy>,
    crate::mcp::allowlist::McpAllowlist,
) {
    match &doc.policy {
        Some(m) => {
            let policy = match crate::policy::ManifestPolicy::new(m.clone()) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("agentd: policy load failed: {e}");
                    std::process::exit(EXIT_SEMANTIC as i32);
                }
            };
            (
                Box::new(policy) as Box<dyn crate::tools::policy::Policy>,
                m.mcp_allowlist(),
            )
        }
        None => (
            Box::new(crate::tools::policy::AllowAll),
            crate::mcp::allowlist::McpAllowlist::allow_all(),
        ),
    }
}

/// Try to build a new `Box<dyn Policy>` from a fresh workflow doc,
/// reporting failures as `Err(String)` instead of exiting. Used by
/// the SIGHUP reload path so a bad policy block keeps the old
/// policy live (fail-forward semantics) rather than killing the
/// process mid-reload.
#[cfg(feature = "trigger-http")]
fn try_build_inner_policy(
    doc: &WorkflowDoc,
) -> std::result::Result<
    (
        Box<dyn crate::tools::policy::Policy>,
        crate::mcp::allowlist::McpAllowlist,
    ),
    String,
> {
    match &doc.policy {
        Some(m) => {
            let policy = crate::policy::ManifestPolicy::new(m.clone())?;
            Ok((
                Box::new(policy) as Box<dyn crate::tools::policy::Policy>,
                m.mcp_allowlist(),
            ))
        }
        None => Ok((
            Box::new(crate::tools::policy::AllowAll),
            crate::mcp::allowlist::McpAllowlist::allow_all(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Once mode — run a manual start node, emit outcome JSON, exit
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

    let started = std::time::Instant::now();
    let result = engine.run_with_trace(&doc, &start, TriggerMeta::manual(input), options);
    let wall_ms = started.elapsed().as_millis() as u64;
    emit_run_result(&doc.name, &start, &engine, result, wall_ms, args)
}

/// `--resume RUN_ID`: load a paused run's checkpoint and continue it.
fn run_resume_mode(doc: WorkflowDoc, engine: Engine, options: RunOptions, args: &Args) -> ExitCode {
    let run_id = args.resume.as_deref().unwrap_or_default();
    let Some(dir) = &args.state_dir else {
        eprintln!("agentd: --resume needs --state-dir (where the checkpoint lives)");
        return ExitCode::from(EXIT_USAGE);
    };
    let checkpoint = match crate::engine::Checkpoint::load(dir, run_id) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agentd: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let start = checkpoint.start_node.clone();
    let started = std::time::Instant::now();
    let result = engine.resume(&doc, checkpoint, options);
    let wall_ms = started.elapsed().as_millis() as u64;
    emit_run_result(&doc.name, &start, &engine, result, wall_ms, args)
}

/// Shared tail for a one-shot or resumed run: write the optional run
/// record, print the outcome JSON, and map it to an exit code.
fn emit_run_result(
    workflow: &str,
    start: &str,
    engine: &Engine,
    result: Result<(ExecutionOutcome, crate::engine::ExecutionTrace), crate::Error>,
    wall_ms: u64,
    args: &Args,
) -> ExitCode {
    if let Some(path) = &args.record {
        let cost = engine.metrics().snapshot();
        let record = match &result {
            Ok((outcome, trace)) => crate::engine::RunRecord::from_outcome(
                workflow,
                start,
                wall_ms,
                cost,
                outcome,
                trace.clone(),
            ),
            Err(e) => {
                crate::engine::RunRecord::errored(workflow, start, wall_ms, cost, e.to_string())
            }
        };
        match fs::write(path, record.to_json_pretty()) {
            Ok(()) => eprintln!("agentd: run record written to {}", path.display()),
            Err(e) => eprintln!(
                "agentd: failed to write run record to {}: {e}",
                path.display()
            ),
        }
    }

    match result {
        Ok((outcome, _trace)) => {
            let code = match &outcome {
                ExecutionOutcome::Completed { .. } => EXIT_OK,
                ExecutionOutcome::Paused { .. } => EXIT_PAUSED,
                _ => EXIT_SEMANTIC,
            };
            println!("{}", serde_json::to_string_pretty(&outcome).unwrap());
            ExitCode::from(code)
        }
        Err(e) => {
            eprintln!("agentd: {e}");
            ExitCode::from(EXIT_SEMANTIC)
        }
    }
}

/// `agentd inspect RUN.json` — read a run record and render its
/// timeline. Parses to a generic JSON value so it can render records
/// written by any version.
fn run_inspect(path: Option<&str>) -> ExitCode {
    let Some(path) = path else {
        eprintln!("agentd inspect: usage: agentd inspect RUN.json");
        return ExitCode::from(EXIT_USAGE);
    };
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("agentd inspect: read {path}: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let value: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("agentd inspect: {path}: invalid run record JSON: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    print!("{}", crate::engine::record::render(&value));
    ExitCode::from(EXIT_OK)
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
// Serve mode — bind HTTP, block on SIGTERM
// ---------------------------------------------------------------------------

#[cfg(feature = "trigger-http")]
fn run_serve_mode(doc: WorkflowDoc, engine: Engine, options: RunOptions, args: &Args) -> ExitCode {
    use std::net::SocketAddr;

    // Serve mode accepts any long-lived trigger: HTTP, cron,
    // interval, fs_watch. Reject only when the workflow declared
    // none of them.
    if doc.http_routes.is_empty() && !has_long_lived_trigger(&doc) {
        eprintln!(
            "agentd: serve mode requires at least one [[http_routes]] \
             entry or a long-lived trigger (cron / interval / fs_watch)"
        );
        return ExitCode::from(EXIT_USAGE);
    }

    let doc_arc = Arc::new(doc.clone());
    let engine_arc = Arc::new(engine);

    // Spawn the HTTP server when routes are configured. Shutdown +
    // hot reload still flow through its handle. Cron / fs_watch
    // triggers run independently on their own threads.
    let http_handle = if !doc.http_routes.is_empty() {
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
        let server = crate::triggers::http::HttpServer::new(
            addr,
            doc_arc.clone(),
            engine_arc.clone(),
            options.clone(),
        )
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
        eprintln!("       GET /healthz is always live");
        Some(handle)
    } else {
        None
    };

    // Spawn cron / interval / fs_watch trigger threads. All share
    // the same shutdown flag as the main serve loop; they return
    // their `JoinHandle`s so the shutdown path can park until each
    // loop actually exits.
    let trigger_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let trigger_threads = match spawn_scheduled_triggers(
        &doc,
        doc_arc.clone(),
        engine_arc.clone(),
        options.clone(),
        trigger_shutdown.clone(),
    ) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("agentd: {e}");
            return ExitCode::from(EXIT_SEMANTIC);
        }
    };

    // Install SIGTERM/SIGINT/SIGHUP handlers and block until a
    // shutdown signal flips the flag. `SIGHUP` triggers an
    // in-place reload of TLS + auth (JWKS) state from the original
    // `--config` file; embedded-config builds refuse the reload
    // with an audit event since the baked-in TOML isn't rereadable.
    crate::signals::install_shutdown_handlers();

    // Optional cross-platform file-based reload trigger. Primary
    // SIGHUP replacement on Windows (no console-signal equivalent)
    // and an additional reload channel on Unix. The watcher thread
    // joins the same shutdown flag as the scheduled triggers.
    let reload_watcher = args
        .reload_file
        .clone()
        .map(|p| crate::signals::spawn_reload_file_watcher(p, trigger_shutdown.clone()));

    while !crate::signals::shutdown_requested() {
        if crate::signals::reload_requested() {
            if let Some(h) = &http_handle {
                run_reload(&args.config, h, &engine_arc, args);
            }
            crate::signals::clear_reload();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("agentd: shutdown signal received; draining…");

    // Signal trigger threads to stop. We don't bound-wait on them
    // here; each checks the flag at its own poll cadence (<= 200ms)
    // so join completes quickly under normal loads.
    trigger_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    for t in trigger_threads {
        let _ = t.join();
    }
    if let Some(w) = reload_watcher {
        let _ = w.join();
    }

    let clean = match http_handle {
        Some(h) => h.shutdown_and_drain(),
        None => true,
    };

    // Flush any pending OTLP spans and shut down the exporter
    // runtime. No-op when `otel` is disabled or `[otel]` wasn't
    // declared. Runs AFTER the HTTP drain so any spans emitted
    // during drain still get exported.
    crate::observability::otel::shutdown_otel();
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

/// Handle a `SIGHUP`-driven reload in serve mode. Re-reads the
/// workflow from `--config`, re-validates it, and asks the server
/// handle to swap TLS + prepared auth atomically. Failures keep
/// the old state running and surface as audit events — SIGHUP is a
/// "try this" not a "commit" operation.
///
/// Scope: TLS cert rotation + JWKS rotation. What is
/// NOT reloaded today: route table, rate-limit buckets, workflow
/// policy / tools, engine handler registry, intelligence / MCP
/// clients. Workflow-structural changes still require a restart.
/// See `docs/operations.md §5.4` for the full scope matrix.
#[cfg(feature = "trigger-http")]
/// Instantiate and spawn every cron / interval / fs_watch trigger
/// declared in the workflow. Returns the thread handles so the
/// serve loop can join them during shutdown. Feature-gated trigger
/// variants fail here with a rebuild hint if the workflow declares
/// a shape the binary wasn't compiled for.
fn spawn_scheduled_triggers(
    doc: &WorkflowDoc,
    workflow: Arc<WorkflowDoc>,
    engine: Arc<crate::engine::Engine>,
    options: RunOptions,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> std::result::Result<Vec<std::thread::JoinHandle<()>>, String> {
    use crate::workflow::model::Trigger;
    // `mut` is used under `trigger-cron` / `trigger-fs-watch`; the
    // feature-off build falls through every arm without pushing,
    // so rustc would warn without the allow.
    #[allow(unused_mut)]
    let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for trig in &doc.triggers {
        match trig {
            Trigger::Cron { .. } | Trigger::Interval { .. } => {
                #[cfg(feature = "trigger-cron")]
                {
                    if let Some(prepped) = crate::triggers::cron::CronTrigger::from_trigger(trig)
                        .map_err(|e| format!("{e}"))?
                    {
                        threads.push(prepped.spawn(
                            workflow.clone(),
                            engine.clone(),
                            options.clone(),
                            shutdown.clone(),
                        ));
                    }
                }
                #[cfg(not(feature = "trigger-cron"))]
                {
                    let _ = (&workflow, &engine, &options, &shutdown);
                    return Err("workflow declares a cron/interval trigger but this build \
                         lacks the `trigger-cron` Cargo feature; rebuild with \
                         --features trigger-cron"
                        .into());
                }
            }
            Trigger::FsWatch { .. } => {
                #[cfg(feature = "trigger-fs-watch")]
                {
                    if let Some(prepped) =
                        crate::triggers::fs_watch::FsWatchTrigger::from_trigger(trig)
                            .map_err(|e| format!("{e}"))?
                    {
                        threads.push(prepped.spawn(
                            workflow.clone(),
                            engine.clone(),
                            options.clone(),
                            shutdown.clone(),
                        ));
                    }
                }
                #[cfg(not(feature = "trigger-fs-watch"))]
                {
                    let _ = (&workflow, &engine, &options, &shutdown);
                    return Err("workflow declares an fs_watch trigger but this build \
                         lacks the `trigger-fs-watch` Cargo feature; rebuild with \
                         --features trigger-fs-watch"
                        .into());
                }
            }
            // MCP-subscription triggers route through the MCP client;
            // internal.event triggers are fired from inside workflow
            // nodes, not scheduled externally. Neither needs a thread.
            Trigger::McpResourceUpdated { .. }
            | Trigger::McpResourceCreated { .. }
            | Trigger::InternalEvent { .. } => {}
        }
    }
    Ok(threads)
}

#[cfg(feature = "trigger-http")]
fn run_reload(
    config_path: &Option<PathBuf>,
    handle: &crate::triggers::http::ServerHandle,
    engine: &Arc<Engine>,
    args: &Args,
) {
    let Some(path) = config_path else {
        tracing::warn!(
            target: "agentd::audit",
            event = "reload.skipped",
            reason = "embedded workflow has no source path to re-read",
        );
        return;
    };

    tracing::info!(target: "agentd::audit", event = "reload.started", path = %path.display());

    let src = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                target: "agentd::audit",
                event = "reload.failed",
                stage = "read",
                reason = %format!("{e}"),
            );
            return;
        }
    };
    let doc = match WorkflowDoc::from_toml(&src) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(
                target: "agentd::audit",
                event = "reload.failed",
                stage = "parse",
                reason = %format!("{e}"),
            );
            return;
        }
    };
    let report = workflow::validate(&doc);
    if !report.ok() {
        tracing::error!(
            target: "agentd::audit",
            event = "reload.failed",
            stage = "validate",
            issues = report.issues.len() as u64,
        );
        return;
    }

    // Reload TLS — pulls cert/key off disk fresh, rebuilds the
    // rustls::ServerConfig, swaps atomically.
    let tls_cfg = doc.server.as_ref().and_then(|s| s.tls.as_ref());
    if let Err(e) = handle.reload_tls(tls_cfg) {
        tracing::error!(
            target: "agentd::audit",
            event = "reload.failed",
            stage = "tls",
            reason = %format!("{e}"),
        );
        return;
    }

    // Reload prepared auth — re-parses every JWKS in [auth.oidc.*]
    // alongside bearer/HMAC bindings.
    #[cfg(feature = "auth")]
    {
        let empty = crate::auth::AuthConfig::default();
        let cfg = doc.auth.as_ref().unwrap_or(&empty);
        if let Err(e) = handle.reload_auth(cfg) {
            tracing::error!(
                target: "agentd::audit",
                event = "reload.failed",
                stage = "auth",
                reason = %format!("{e}"),
            );
            return;
        }
    }

    // Reload policy — rebuild `ManifestPolicy` (re-compiles Rego,
    // re-reads inline data) and swap the inner of the process-wide
    // `ReloadablePolicy`. Tool handlers (fs/env/http/shell) keep the
    // same `Arc<dyn Policy>` they captured at startup — this just
    // updates what that Arc dereferences to. Rego thread-local
    // engines self-invalidate via the new `RegoSpec.id` fingerprint
    // on first use after the swap.
    //
    // Fail-forward: a bad policy (Rego syntax error, missing data
    // file, feature-gated block on a feature-off build) keeps the
    // old policy live rather than leaving the process unprotected.
    if let Some(policy_handle) = &engine.reload.policy {
        match try_build_inner_policy(&doc) {
            Ok((new_inner, _new_mcp_allowlist)) => {
                policy_handle.swap(new_inner);
                tracing::info!(
                    target: "agentd::audit",
                    event = "reload.policy",
                );
            }
            Err(e) => {
                tracing::error!(
                    target: "agentd::audit",
                    event = "reload.failed",
                    stage = "policy",
                    reason = %e,
                );
                return;
            }
        }
    }

    // Reload MCP servers — for every registered server, rotate its
    // allowlist from the new config AND respawn the stdio child.
    // Fail-forward per server: a spawn failure for one server
    // keeps that server's old child running and logs the
    // `reload.mcp_respawn_failed` event; the rest of the reload
    // continues.
    if let Some(mcp_registry) = &engine.reload.mcp {
        let global_mcp_allowlist = doc
            .policy
            .as_ref()
            .map(|m| m.mcp_allowlist())
            .unwrap_or_else(crate::mcp::allowlist::McpAllowlist::allow_all);
        for handle in mcp_registry.iter() {
            // Look up the new server def (if any) so we can rotate
            // the allowlist + respawn with the new command.
            let def = doc.mcp_servers.iter().find(|d| d.name == handle.name);
            let (cmd, allowlist) = match def {
                Some(d) => (
                    d.command.as_slice(),
                    crate::mcp::allowlist::McpAllowlist {
                        allowed_tools: d.allow_tools.clone(),
                        allowed_resource_patterns: d.allow_resources.clone(),
                    },
                ),
                None if handle.name == "default" => {
                    // Legacy `--mcp-stdio` default server — command
                    // comes from CLI, allowlist comes from `[policy.mcp]`.
                    match args.mcp_stdio.as_deref() {
                        Some(c) => (c, global_mcp_allowlist.clone()),
                        None => {
                            // Server was registered at startup but the
                            // CLI arg vanished — can't respawn. Skip.
                            continue;
                        }
                    }
                }
                None => {
                    tracing::warn!(
                        target: "agentd::audit",
                        event = "reload.mcp_dropped_from_config",
                        server = %handle.name,
                    );
                    continue;
                }
            };

            handle.allowlist.swap(allowlist);
            tracing::info!(
                target: "agentd::audit",
                event = "reload.mcp_allowlist",
                server = %handle.name,
            );

            let (head, tail) = cmd
                .split_first()
                .expect("config validator enforces non-empty command");
            match crate::mcp::client::StdioMcpClient::spawn(head.clone(), tail) {
                Ok(new_client) => {
                    let boxed: Box<dyn crate::mcp::client::McpClient> = Box::new(new_client);
                    handle.client.swap(boxed);
                    tracing::info!(
                        target: "agentd::audit",
                        event = "reload.mcp_respawn",
                        server = %handle.name,
                        command = %head,
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "agentd::audit",
                        event = "reload.mcp_respawn_failed",
                        server = %handle.name,
                        command = %head,
                        reason = %format!("{e}"),
                    );
                }
            }
        }
    }

    // Reload intelligence client — rebuild from the current CLI
    // args (same endpoint; bearer-file and env are re-read so
    // rotation picks up). Endpoint changes still need a restart,
    // noted in operations.md.
    if let Some(intel_handle) = &engine.reload.intel {
        match rebuild_intel_client(args) {
            Ok(Some(new_inner)) => {
                intel_handle.swap(new_inner);
                tracing::info!(
                    target: "agentd::audit",
                    event = "reload.intel",
                );
            }
            Ok(None) => {
                // Operator changed CLI args between runs? Shouldn't
                // happen — args are captured at process start and
                // the reload sees the same set. No-op.
            }
            Err(e) => {
                tracing::error!(
                    target: "agentd::audit",
                    event = "reload.failed",
                    stage = "intel",
                    reason = %e,
                );
                return;
            }
        }
    }

    // Reload named intelligence backends — re-validate the new
    // doc's defs and rebuild matching clients (re-reads api_key_env
    // so key rotation lands). Unknown-to-the-process names are
    // ignored with an audit note: adding whole backends still
    // requires a restart (the handler map is registry-bound).
    #[cfg(feature = "intel-remote")]
    if !engine.reload.intel_backends.is_empty() {
        let defs: &[crate::intelligence::backends::BackendDef] = doc
            .intelligence
            .as_ref()
            .map(|i| i.backends.as_slice())
            .unwrap_or(&[]);
        if let Err(e) = crate::intelligence::backends::BackendDef::validate_list(defs) {
            tracing::error!(
                target: "agentd::audit",
                event = "reload.failed",
                stage = "intel_backends",
                reason = %e,
            );
            return;
        }
        for (name, handle) in &engine.reload.intel_backends {
            let Some(def) = defs.iter().find(|d| &d.name == name) else {
                tracing::warn!(
                    target: "agentd::audit",
                    event = "reload.intel_backend_dropped_from_config",
                    backend = %name,
                );
                continue;
            };
            match crate::intelligence::providers::RemoteClient::from_def(
                def,
                Duration::from_secs(args.timeout_secs.max(1)),
            ) {
                Ok(client) => {
                    handle.swap(Box::new(client));
                    tracing::info!(
                        target: "agentd::audit",
                        event = "reload.intel_backend",
                        backend = %name,
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "agentd::audit",
                        event = "reload.intel_backend_failed",
                        backend = %name,
                        reason = %format!("{e}"),
                    );
                }
            }
        }
    }

    // Reload routes + rate-limit buckets — rebuild the whole slice
    // from the new `[[http_routes]]` list and swap atomically.
    // Rate-limit counters reset to full capacity on the swap, by
    // design (see `HttpReloadable::build` doc-comment).
    if let Err(e) = handle.reload_http_state(&doc.http_routes) {
        tracing::error!(
            target: "agentd::audit",
            event = "reload.failed",
            stage = "routes",
            reason = %format!("{e}"),
        );
        return;
    }

    tracing::info!(target: "agentd::audit", event = "reload.succeeded");
}

/// Rebuild the intelligence client's inner implementation from the
/// current CLI args. Called from `run_reload` to refresh bearer
/// tokens / re-dial the endpoint. Returns `Ok(None)` only when the
/// CLI args no longer describe an intel client — rare, since args
/// don't actually change during a run; mostly a safety-net shape.
#[cfg(feature = "trigger-http")]
fn rebuild_intel_client(
    args: &Args,
) -> std::result::Result<Option<Box<dyn crate::intelligence::client::IntelligenceClient>>, String> {
    #[cfg(unix)]
    if let Some(path) = &args.intel_unix {
        return Ok(Some(Box::new(
            crate::intelligence::client::UnixClient::new(
                path.clone(),
                Duration::from_secs(args.timeout_secs.max(1)),
            ),
        )));
    }
    #[cfg(not(unix))]
    if args.intel_unix.is_some() {
        return Err("--intel-unix is Unix-only".into());
    }
    #[cfg(feature = "intel-http")]
    if let Some(url) = &args.intel_http {
        let bearer = match &args.intel_http_bearer_file {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|e| format!("read intel bearer {}: {e}", path.display()))?
                    .trim()
                    .to_string(),
            ),
            None => std::env::var("AGENTD_INTEL_HTTP_BEARER")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        };
        let client = crate::intelligence::client::HttpClient::with_bearer(
            url,
            Duration::from_secs(args.timeout_secs.max(1)),
            bearer,
        )
        .map_err(|e| format!("--intel-http URL: {e}"))?;
        return Ok(Some(Box::new(client)));
    }
    Ok(None)
}

#[cfg(not(feature = "trigger-http"))]
fn run_serve_mode(
    _doc: WorkflowDoc,
    _engine: Engine,
    _options: RunOptions,
    _args: &Args,
) -> ExitCode {
    eprintln!(
        "agentd: this build was compiled without the `trigger-http` feature; \
         rebuild with `--features trigger-http` to serve HTTP, or mark the \
         workflow as `--mode once`."
    );
    ExitCode::from(EXIT_USAGE)
}

// ---------------------------------------------------------------------------
// Instruction mode (Mode 3, RFC 0006 §3): compile a workflow from a
// natural-language instruction, then run it.
// ---------------------------------------------------------------------------

/// The instruction that drives Mode 3, resolved in precedence order:
/// `--instruction TEXT|@file` (or its `--instruction-file` sugar /
/// `AGENTD_INSTRUCTION`), then the legacy `--goal`, then the `task`
/// field of the `--instructions` file. Returns the raw argument — an
/// `@file` reference is expanded later in [`run_instruction_mode`].
/// `None` means no instruction was given and the agent runs a declared
/// workflow instead.
fn resolve_instruction(args: &Args) -> Option<String> {
    if let Some(s) = args.instruction.clone().filter(|s| !s.trim().is_empty()) {
        return Some(s);
    }
    if let Some(s) = args.goal.clone().filter(|s| !s.trim().is_empty()) {
        return Some(s);
    }
    // A standing `task` in the instructions file makes `--instructions
    // agent.toml` a complete, self-contained agent: identity + the work.
    // Peeked here only to decide the mode; run_instruction_mode reloads
    // the file for the rest of the identity.
    if let Some(p) = &args.instructions {
        if let Ok(ins) = crate::agent::AgentInstructions::load(p) {
            if let Some(task) = ins.task.filter(|s| !s.trim().is_empty()) {
                return Some(task);
            }
        }
    }
    None
}

fn run_instruction_mode(
    instruction_arg: &str,
    args: &Args,
    tracing_overrides: &TracingOverrides,
) -> ExitCode {
    // Optional base config (`--config`): the *environment* the agent
    // operates in — its policy, intelligence backends, MCP servers,
    // budgets, auth, logging. The agent compiles a workflow that runs
    // INSIDE this environment; it never widens its own policy.
    let base: Option<WorkflowDoc> = match &args.config {
        Some(path) => match fs::read_to_string(path)
            .map_err(|e| format!("read {}: {e}", path.display()))
            .and_then(|s| WorkflowDoc::from_toml(&s).map_err(|e| format!("{e}")))
        {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("agentd: base config {e}");
                return ExitCode::from(EXIT_USAGE);
            }
        },
        None => None,
    };

    // Logging from the base config's `[logging]` (if any), under CLI/env.
    let log_src = base.clone().unwrap_or_default();
    let resolved = resolve_logging(&log_src, tracing_overrides);
    let _ = crate::observability::apply(&resolved);

    // `--instruction @file` reads the instruction from a file.
    let instruction = match instruction_arg.strip_prefix('@') {
        Some(path) => match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("agentd: failed to read instruction file {path}: {e}");
                return ExitCode::from(EXIT_USAGE);
            }
        },
        None => instruction_arg.to_string(),
    };

    let instructions = match &args.instructions {
        Some(p) => match crate::agent::AgentInstructions::load(p) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("agentd: {e}");
                return ExitCode::from(EXIT_USAGE);
            }
        },
        None => crate::agent::AgentInstructions::default(),
    };

    // Resolve the backend names the planner may reference, and a
    // planner client to talk to. Prefer the base config's named
    // backends; fall back to CLI transports / AGENTD_GOAL_BACKEND.
    let backend_names = resolve_backend_names(base.as_ref(), args);
    let planner_client = match build_goal_planner_client(args, &instructions, base.as_ref()) {
        Ok(c) => c,
        Err(code) => return code,
    };

    // Inject the agent's ACTUAL capabilities into the planner prompt:
    // executable node kinds, configured backends, MCP servers+tools,
    // and the active policy (RFC 0006 §3).
    let catalog = crate::agent::catalog::CapabilityCatalog::from_base(base.as_ref(), backend_names);
    let ctx = crate::agent::planner::PlanContext {
        system: instructions.system.as_deref(),
        capabilities: catalog.render(),
    };

    let plan = match crate::agent::planner::generate(
        planner_client.as_ref(),
        &instruction,
        &ctx,
        args.max_replans,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agentd: {e}");
            return ExitCode::from(EXIT_SEMANTIC);
        }
    };

    // Graft the agent's plan onto the base environment: the generated
    // graph (name / nodes / edges / start nodes / triggers / routes),
    // everything else (policy, backends, MCP, budgets, auth, logging)
    // from the base config. The agent cannot grant itself capabilities.
    let doc = graft_environment(plan.doc, base);

    // Capability-altitude review for the approval gate: what the plan
    // does to the world + the policy it runs under — not raw TOML. The
    // full TOML is one flag away (--plan-only / --plan-out).
    eprintln!(
        "agentd: compiled workflow `{}` from the instruction in {} attempt(s).\n",
        doc.name, plan.attempts
    );
    eprint!(
        "{}",
        crate::agent::review::summarize_plan(&doc, &catalog.policy_summary)
    );

    if let Some(out) = &args.plan_out {
        if let Err(e) = fs::write(out, &plan.source) {
            eprintln!("agentd: failed to write plan to {}: {e}", out.display());
            return ExitCode::from(EXIT_SEMANTIC);
        }
        eprintln!("agentd: plan written to {}", out.display());
    }

    // `--promote PATH`: save the approved plan as a durable, versioned
    // workflow artifact — the static Mode-1 form of this dynamism, so
    // it stops being re-generated each run.
    if let Some(path) = &args.promote {
        let body = format!(
            "{}{}",
            promote_header(&instruction, plan.attempts),
            plan.source
        );
        if let Err(e) = fs::write(path, body) {
            eprintln!("agentd: failed to promote plan to {}: {e}", path.display());
            return ExitCode::from(EXIT_SEMANTIC);
        }
        eprintln!(
            "agentd: promoted to {} — add/confirm its [policy] and \
             [[intelligence.backends]] (see the summary above), sign it, then \
             run it directly with --config {}",
            path.display(),
            path.display(),
        );
    }

    // Raw TOML only when explicitly requested, so the default approval
    // view stays at the capability altitude.
    if args.plan_only || args.plan_out.is_some() {
        println!("{}", plan.source);
    }

    if args.plan_only {
        return ExitCode::from(EXIT_OK);
    }

    // Governance gate (RFC 0006 §2): a compiled plan does NOT run
    // without approval. The operator opts in per-invocation
    // (`--auto-approve`) or once in the instructions file
    // (`auto_approve = true` — they authored the spec).
    let approved = args.auto_approve || instructions.auto_approve;
    if !approved {
        eprintln!(
            "\nagentd: refusing to run a compiled plan without approval. \
             Re-run with --auto-approve (or set auto_approve = true in the \
             instructions file), or --plan-only to stop here."
        );
        tracing::warn!(target: "agentd::audit", event = "plan.rejected", reason = "not approved");
        return ExitCode::from(EXIT_USAGE);
    }
    tracing::info!(target: "agentd::audit", event = "plan.approved", workflow = %doc.name);

    // Execute on the normal engine, under the base config's policy /
    // budgets / backends / MCP — same validator, same gates, same audit.
    let engine = match build_engine(&doc, args) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let options = RunOptions {
        timeout: Duration::from_secs(args.timeout_secs.max(1)),
        dry_run: args.dry_run,
    };
    run_once_mode(doc, engine, options, args)
}

/// Names the planner may reference for `llm_infer` / `agent_loop`:
/// the base config's `[[intelligence.backends]]` plus `default` when a
/// CLI socket transport is configured.
fn resolve_backend_names(base: Option<&WorkflowDoc>, args: &Args) -> Vec<String> {
    let mut names: Vec<String> = base
        .and_then(|d| d.intelligence.as_ref())
        .map(|i| i.backends.iter().map(|b| b.name.clone()).collect())
        .unwrap_or_default();
    let has_socket = args.intel_unix.is_some() || args.intel_http.is_some();
    if (has_socket || std::env::var("AGENTD_GOAL_BACKEND").is_ok())
        && !names.iter().any(|n| n == "default")
    {
        names.insert(0, "default".to_string());
    }
    names
}

/// Graft a compiled plan onto the base environment. Generated graph
/// shape wins for `name`/`description`/start nodes/triggers/routes/
/// nodes/edges; the operator-provided environment (policy, backends,
/// MCP, budget, auth, logging, signing, server) is preserved so the
/// agent runs inside it and cannot widen its own authority.
fn graft_environment(generated: WorkflowDoc, base: Option<WorkflowDoc>) -> WorkflowDoc {
    match base {
        None => generated,
        Some(mut merged) => {
            merged.name = generated.name;
            merged.description = generated.description;
            merged.start_nodes = generated.start_nodes;
            merged.triggers = generated.triggers;
            merged.http_routes = generated.http_routes;
            merged.nodes = generated.nodes;
            merged.edges = generated.edges;
            merged
        }
    }
}

/// Provenance header prepended to a `--promote`d plan, so a saved
/// workflow records where it came from.
fn promote_header(instruction: &str, attempts: u32) -> String {
    let snippet: String = instruction
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(100)
        .collect();
    format!(
        "# Promoted agentd workflow — compiled from an instruction, then approved.\n\
         # Instruction: {snippet}\n\
         # Planner attempts: {attempts}.\n\
         # Before production: confirm the [policy] and [[intelligence.backends]] this\n\
         # plan needs (see the capability summary printed at promotion), then sign it.\n\n"
    )
}

/// Build the client the planner uses to compile the workflow.
/// Precedence: explicit CLI transports (`--intel-unix` / `--intel-http`)
/// win; otherwise the `--config` base environment supplies the brain —
/// the backend named by the instructions file's `default_backend`
/// (or `default`), resolved against `[[intelligence.backends]]` — so
/// `agentd --config prod.toml --instruction "…"` needs no extra flags;
/// finally `AGENTD_GOAL_BACKEND=provider:model` for a zero-TOML run.
fn build_goal_planner_client(
    args: &Args,
    instructions: &crate::agent::AgentInstructions,
    base: Option<&WorkflowDoc>,
) -> std::result::Result<Box<dyn crate::intelligence::client::IntelligenceClient>, ExitCode> {
    // `timeout` feeds the per-transport constructors below; each is
    // behind its own cfg, so on a build with none of them compiled
    // in (e.g. Windows without intel-remote) it goes unread.
    #[allow(unused_variables)]
    let timeout = Duration::from_secs(args.timeout_secs.max(1));

    #[cfg(unix)]
    if let Some(path) = &args.intel_unix {
        return Ok(Box::new(crate::intelligence::client::UnixClient::new(
            path.clone(),
            timeout,
        )));
    }

    #[cfg(feature = "intel-http")]
    if let Some(url) = &args.intel_http {
        let bearer = read_intel_http_bearer(args)?;
        return match crate::intelligence::client::HttpClient::with_bearer(url, timeout, bearer) {
            Ok(c) => Ok(Box::new(c)),
            Err(e) => {
                eprintln!("agentd: bad --intel-http URL: {e}");
                Err(ExitCode::from(EXIT_USAGE))
            }
        };
    }

    // The `--config` base environment supplies the planner's brain when
    // no CLI transport is given: resolve the backend named by the
    // instructions file's `default_backend` (or `default`) against the
    // base config's `[[intelligence.backends]]` and talk to it directly.
    // This is the zero-flag path for `agentd --config … --instruction …`.
    #[cfg(feature = "intel-remote")]
    if let Some(def) = base.and_then(|d| d.intelligence.as_ref()).and_then(|i| {
        let want = instructions.effective_backend();
        i.backends.iter().find(|b| b.name == want)
    }) {
        return match crate::intelligence::providers::RemoteClient::from_def(def, timeout) {
            Ok(c) => Ok(Box::new(c)),
            Err(e) => {
                eprintln!("agentd: planner backend `{}`: {e}", def.name);
                Err(ExitCode::from(EXIT_USAGE))
            }
        };
    }

    // Last resort: a named remote provider via env-only config —
    // `AGENTD_GOAL_BACKEND=provider:model` lets Mode 3 run with zero
    // TOML. Keys come from the provider's standard env var.
    #[cfg(feature = "intel-remote")]
    if let Some(spec) = std::env::var("AGENTD_GOAL_BACKEND")
        .ok()
        .filter(|s| !s.is_empty())
    {
        match goal_backend_from_spec(&spec, instructions, timeout) {
            Ok(c) => return Ok(c),
            Err(e) => {
                eprintln!("agentd: AGENTD_GOAL_BACKEND: {e}");
                return Err(ExitCode::from(EXIT_USAGE));
            }
        }
    }
    let _ = (instructions, base);

    eprintln!(
        "agentd: instruction mode needs a model to plan with. Either pass \
         --config CONFIG.toml whose [[intelligence.backends]] includes the \
         instructions' default_backend, or --intel-unix PATH / --intel-http URL, \
         or (with --features intel-remote) set AGENTD_GOAL_BACKEND=provider:model \
         plus the provider's API-key env var."
    );
    Err(ExitCode::from(EXIT_USAGE))
}

/// Parse `provider:model` (e.g. `anthropic:claude-sonnet-4-6`) into a
/// remote client, resolving the provider's conventional key env var.
#[cfg(feature = "intel-remote")]
fn goal_backend_from_spec(
    spec: &str,
    _instructions: &crate::agent::AgentInstructions,
    timeout: Duration,
) -> std::result::Result<Box<dyn crate::intelligence::client::IntelligenceClient>, String> {
    use crate::intelligence::backends::{BackendDef, ProviderKind};
    let (provider, model) = spec
        .split_once(':')
        .ok_or("expected provider:model, e.g. anthropic:claude-sonnet-4-6")?;
    let (kind, key_env) = match provider {
        "anthropic" => (ProviderKind::Anthropic, Some("ANTHROPIC_API_KEY")),
        "openai" => (ProviderKind::Openai, Some("OPENAI_API_KEY")),
        "gemini" => (ProviderKind::Gemini, Some("GEMINI_API_KEY")),
        other => return Err(format!("unknown provider `{other}`")),
    };
    let def = BackendDef {
        name: "goal".into(),
        provider: kind,
        model: Some(model.to_string()),
        api_key_env: key_env.map(String::from),
        base_url: None,
        max_tokens: None,
    };
    let client = crate::intelligence::providers::RemoteClient::from_def(&def, timeout)
        .map_err(|e| format!("{e}"))?;
    Ok(Box::new(client))
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

subcommands:
  agentd inspect RUN.json          render a run record written by --record

usage:
  agentd [--config FILE]            AGENTD_CONFIG          (required unless embedded)
        [--input FILE]             AGENTD_INPUT           one-shot trigger payload
        [--start NAME]             AGENTD_START           explicit start node
        [--mode once|serve]        AGENTD_MODE            override auto-inferred mode
        [--bind HOST:PORT]         AGENTD_HTTP_BIND       server-mode bind (default 127.0.0.1:8080)
        [--timeout-secs N]         AGENTD_TIMEOUT_SECS    per-run deadline (default 120)
        [--drain-timeout-secs N]   AGENTD_DRAIN_TIMEOUT_SECS  graceful shutdown grace (default 30)
        [--intel-unix PATH]        AGENTD_INTEL_UNIX
        [--intel-http URL]         AGENTD_INTEL_HTTP              http://host:port/path
        [--intel-http-bearer-file P] AGENTD_INTEL_HTTP_BEARER_FILE  or AGENTD_INTEL_HTTP_BEARER
        [--mcp-stdio \"CMD ARGS\"]   AGENTD_MCP_STDIO       legacy single-server; prefer [[mcp_servers]] TOML
        [--dry-run]                AGENTD_DRY_RUN=1
        [--record PATH]            AGENTD_RECORD          write a run record; inspect with `agentd inspect PATH`
        [--state-dir DIR]          AGENTD_STATE_DIR       where pause_for_approval checkpoints live
        [--resume RUN_ID]          AGENTD_RESUME          continue a paused run from its checkpoint
        [--validate-only]          AGENTD_VALIDATE_ONLY=1
        [--log-level LEVEL]        AGENTD_LOG             (default warn)
        [--log-format text|json]   AGENTD_LOG_FORMAT      (default text)
        [--log-target TARGET]      AGENTD_LOG_TARGET      stderr | stdout | file:PATH
        [--quiet]                  AGENTD_QUIET=1
        [--signing-required]       AGENTD_SIGNING_REQUIRED=1  fail-closed on unsigned
        [--signing-key-file PATH]  AGENTD_SIGNING_KEY_FILE    override pinned pubkey
        [--reload-file PATH]       AGENTD_RELOAD_FILE         touch-to-reload (Windows SIGHUP replacement)
        [--version] [--help]

instruction mode (RFC 0006 §3) — compile a workflow from an instruction, then run it:
        [--instruction TEXT|@FILE] AGENTD_INSTRUCTION    the task; `@FILE` reads it from a file
        [--instruction-file PATH]                        sugar for --instruction @PATH
        [--goal TEXT|@FILE]        AGENTD_GOAL           alias of --instruction
        [--instructions FILE]      AGENTD_INSTRUCTIONS   agent identity (system, default_backend, task)
        [--auto-approve]           AGENTD_AUTO_APPROVE=1 run the compiled plan (else stops, fail-closed)
        [--plan-only]              AGENTD_PLAN_ONLY=1    print the compiled workflow and exit
        [--plan-out PATH]          AGENTD_PLAN_OUT       also write the compiled workflow here
        [--promote PATH]           AGENTD_PROMOTE        save the approved plan as a durable workflow
        [--max-replans N]          AGENTD_MAX_REPLANS    bounded validation-repair rounds (default 2)
  The planner's model comes from --config's [[intelligence.backends]] (by default_backend),
  else --intel-unix / --intel-http, else AGENTD_GOAL_BACKEND=provider:model. The compiled plan
  runs inside --config's policy / budgets / MCP — the agent never widens its own authority.

Design: rfcs/0001-bounded-workflow-runtime.md
Docs:   docs/README.md\
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
                retry: None,
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
        let (a, t) = parse_args(&[
            "--config".into(),
            "/tmp/wf.toml".into(),
            "--start".into(),
            "main".into(),
            "--dry-run".into(),
            "--log-level".into(),
            "debug".into(),
            "--log-format".into(),
            "json".into(),
        ])
        .unwrap();
        assert_eq!(a.config, Some(PathBuf::from("/tmp/wf.toml")));
        assert_eq!(a.start.as_deref(), Some("main"));
        assert!(a.dry_run);
        assert_eq!(t.level.as_deref(), Some("debug"));
        assert_eq!(t.format, Some(crate::observability::Format::Json));
    }

    #[test]
    fn resolve_logging_merges_by_precedence() {
        use crate::observability::{Format, LogTarget, LoggingConfig};

        // 1. Workflow provides level + target.
        let mut doc = wf_with(vec![manual("m")]);
        doc.logging = Some(LoggingConfig {
            level: Some("info".into()),
            format: Some(Format::Text),
            target: Some(LogTarget::File("/tmp/a.log".into())),
            enabled: Some(true),
            audit: None,
            rotation: None,
            otel: None,
        });
        let overrides = TracingOverrides::default();
        let resolved = resolve_logging(&doc, &overrides);
        assert_eq!(resolved.level, "info");
        assert_eq!(resolved.format, Format::Text);
        assert_eq!(resolved.target, LogTarget::File("/tmp/a.log".into()));
        assert!(resolved.enabled);

        // 2. CLI override beats workflow.
        let overrides = TracingOverrides {
            level: Some("debug".into()),
            format: Some(Format::Json),
            target: Some(LogTarget::Stdout),
            enabled_false: false,
        };
        let resolved = resolve_logging(&doc, &overrides);
        assert_eq!(resolved.level, "debug");
        assert_eq!(resolved.format, Format::Json);
        assert_eq!(resolved.target, LogTarget::Stdout);

        // 3. --quiet forces enabled=false regardless of workflow.
        let overrides = TracingOverrides {
            enabled_false: true,
            ..TracingOverrides::default()
        };
        let resolved = resolve_logging(&doc, &overrides);
        assert!(!resolved.enabled);

        // 4. No sources → built-in defaults.
        let bare = wf_with(vec![manual("m")]);
        let resolved = resolve_logging(&bare, &TracingOverrides::default());
        assert_eq!(resolved.level, "warn");
        assert_eq!(resolved.format, Format::Text);
        assert_eq!(resolved.target, LogTarget::Stderr);
        assert!(resolved.enabled);
    }

    #[test]
    fn parse_args_rejects_bad_log_format() {
        let err = parse_args(&["--log-format".into(), "xml".into()]).unwrap_err();
        assert!(matches!(err, ArgErr::Usage(ref m) if m.contains("text") && m.contains("json")));
    }

    #[test]
    fn parse_args_rejects_bad_log_target() {
        let err = parse_args(&["--log-target".into(), "telegram".into()]).unwrap_err();
        assert!(matches!(err, ArgErr::Usage(ref m) if m.contains("file:")));
    }

    #[test]
    fn parse_args_accepts_file_log_target() {
        let (_, t) = parse_args(&["--log-target".into(), "file:/tmp/x.log".into()]).unwrap();
        assert_eq!(
            t.target,
            Some(crate::observability::LogTarget::File("/tmp/x.log".into()))
        );
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

    #[test]
    fn parse_args_instruction_forms() {
        // `--instruction TEXT` lands verbatim.
        let (a, _) = parse_args(&["--instruction".into(), "do the thing".into()]).unwrap();
        assert_eq!(a.instruction.as_deref(), Some("do the thing"));

        // `--instruction-file PATH` is sugar for `--instruction @PATH`.
        let (a, _) = parse_args(&["--instruction-file".into(), "/etc/task.txt".into()]).unwrap();
        assert_eq!(a.instruction.as_deref(), Some("@/etc/task.txt"));

        // `--goal` is the legacy alias, stored separately.
        let (a, _) = parse_args(&["--goal".into(), "legacy".into()]).unwrap();
        assert_eq!(a.goal.as_deref(), Some("legacy"));
    }

    #[test]
    fn resolve_instruction_precedence() {
        // --instruction wins over --goal.
        let a = Args {
            instruction: Some("primary".into()),
            goal: Some("secondary".into()),
            ..Default::default()
        };
        assert_eq!(resolve_instruction(&a).as_deref(), Some("primary"));

        // --goal is used when --instruction is absent.
        let a = Args {
            goal: Some("from goal".into()),
            ..Default::default()
        };
        assert_eq!(resolve_instruction(&a).as_deref(), Some("from goal"));

        // Blank values are ignored, not treated as an instruction.
        let a = Args {
            instruction: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(resolve_instruction(&a), None);

        // Nothing set → no instruction mode.
        assert_eq!(resolve_instruction(&Args::default()), None);
    }

    #[test]
    fn resolve_instruction_reads_instructions_file_task() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("agent.toml");
        std::fs::write(
            &p,
            "[agent]\nname = \"a\"\ntask = \"summarise the newest log\"\n",
        )
        .unwrap();
        let a = Args {
            instructions: Some(p),
            ..Default::default()
        };
        assert_eq!(
            resolve_instruction(&a).as_deref(),
            Some("summarise the newest log")
        );
    }

    #[test]
    fn graft_environment_keeps_base_authority_takes_generated_graph() {
        use crate::intelligence::backends::{BackendDef, IntelligenceConfig, ProviderKind};
        use crate::policy::{FsPolicy, PolicyManifest};

        let mut base = wf_with(vec![manual("old")]);
        base.name = "base-env".into();
        base.policy = Some(PolicyManifest {
            fs: FsPolicy {
                write: vec!["/tmp/out/**".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        base.intelligence = Some(IntelligenceConfig {
            backends: vec![BackendDef {
                name: "claude".into(),
                provider: ProviderKind::Anthropic,
                model: Some("claude-sonnet-4-6".into()),
                api_key_env: Some("ANTHROPIC_API_KEY".into()),
                base_url: None,
                max_tokens: None,
            }],
        });

        let mut generated = wf_with(vec![manual("entry")]);
        generated.name = "compiled-plan".into();

        let out = graft_environment(generated.clone(), Some(base));
        // Generated graph shape wins.
        assert_eq!(out.name, "compiled-plan");
        assert_eq!(out.start_nodes, generated.start_nodes);
        assert_eq!(out.nodes, generated.nodes);
        // Base authority is preserved — the agent can't widen it.
        assert!(out.policy.is_some());
        assert_eq!(
            out.policy.unwrap().fs.write,
            vec!["/tmp/out/**".to_string()]
        );
        assert_eq!(out.intelligence.unwrap().backends[0].name, "claude");

        // No base → the generated doc passes through unchanged.
        let passthrough = graft_environment(generated.clone(), None);
        assert_eq!(passthrough.name, generated.name);
    }

    #[test]
    fn resolve_backend_names_lists_base_and_default_socket() {
        use crate::intelligence::backends::{BackendDef, IntelligenceConfig, ProviderKind};

        let mut base = wf_with(vec![manual("m")]);
        base.intelligence = Some(IntelligenceConfig {
            backends: vec![BackendDef {
                name: "claude".into(),
                provider: ProviderKind::Anthropic,
                model: Some("m".into()),
                api_key_env: Some("K".into()),
                base_url: None,
                max_tokens: None,
            }],
        });

        // Base backends are always offered to the planner.
        let names = resolve_backend_names(Some(&base), &Args::default());
        assert!(names.contains(&"claude".to_string()));

        // A CLI socket transport adds the reserved `default` name, first.
        let with_socket = Args {
            intel_unix: Some(PathBuf::from("/run/intel.sock")),
            ..Default::default()
        };
        let names = resolve_backend_names(Some(&base), &with_socket);
        assert_eq!(names.first().map(String::as_str), Some("default"));
        assert!(names.contains(&"claude".to_string()));
    }
}
