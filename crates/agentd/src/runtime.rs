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

use crate::engine::{Engine, HandlerRegistry, RunOptions, StubHandler, TriggerMeta};
use crate::workflow::{self, WorkflowDoc};

pub const EXIT_OK: u8 = 0;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_SEMANTIC: u8 = 5;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(argv: Vec<String>) -> ExitCode {
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
    intel_unix: Option<PathBuf>,
    /// `--signing-required` — hardens every deploy by refusing to
    /// start on an unsigned workflow, even if the TOML's
    /// `[signing].required` is false (or the block is absent).
    signing_required: bool,
    /// `--signing-key-file PATH` — overrides the TOML's pinned key.
    /// Lets operators rotate keys without editing the manifest.
    signing_key_file: Option<PathBuf>,
    intel_http: Option<String>,
    intel_http_bearer_file: Option<PathBuf>,
    mcp_stdio: Option<Vec<String>>,
    dry_run: bool,
    validate_only: bool,
    drain_timeout_secs: u64,
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
        signing_required: env_bool("AGENTD_SIGNING_REQUIRED"),
        signing_key_file: env_opt_path("AGENTD_SIGNING_KEY_FILE"),
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
            "--signing-required" => a.signing_required = true,
            "--signing-key-file" => {
                a.signing_key_file = Some(PathBuf::from(require_value(argv, &mut i, arg)?));
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
// Engine construction
// ---------------------------------------------------------------------------

fn build_engine(doc: &WorkflowDoc, args: &Args) -> Result<Engine, ExitCode> {
    let (policy, mcp_allowlist) = build_policy(doc);

    let budget = Arc::new(crate::budget::BudgetTracker::new(
        doc.budget
            .as_ref()
            .unwrap_or(&crate::budget::BudgetConfig::default()),
    ));

    let mut registry = HandlerRegistry::with_builtin_controls();
    crate::tools::register_default_tools(&mut registry, policy, budget);

    // Intelligence adapter (Unix or HTTP). Wrap whichever client
    // the operator selected in a `ReloadableIntelClient` so a
    // future reload path can swap its inner (e.g. rotate the bearer
    // token from a Vault side-car).
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
            crate::intelligence::handler::register(&mut registry, reloadable.clone());
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
        if let Some(url) = &args.intel_http {
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
                    crate::intelligence::handler::register(&mut registry, reloadable.clone());
                }
                Err(e) => {
                    eprintln!("agentd: bad --intel-http URL: {e}");
                    return Err(ExitCode::from(EXIT_USAGE));
                }
            }
        }
        #[cfg(not(feature = "intel-http"))]
        if args.intel_http.is_some() {
            eprintln!(
                "agentd: --intel-http requires the `intel-http` Cargo feature; \
                 rebuild with --features intel-http"
            );
            return Err(ExitCode::from(EXIT_USAGE));
        }
    }

    // MCP server registry. Composes every `[[mcp_servers]]` entry
    // plus (when set) the legacy `--mcp-stdio CMD ARG` as an
    // implicit `{name = "default", ..}` server.
    let mcp_registry = build_mcp_registry(doc, args, &mcp_allowlist)?;
    if !mcp_registry.is_empty() {
        crate::mcp::handler::register(&mut registry, mcp_registry.clone());
    }

    registry.set_fallback(Box::new(StubHandler));
    Ok(Engine::new(registry))
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

/// Extract the policy + MCP allowlist from the workflow's `[policy]`
/// block. AllowAll / allow-everything when the block is absent —
/// fail-closed enforcement is the manifest's job; absence of the
/// block is an explicit operator choice.
fn build_policy(
    doc: &WorkflowDoc,
) -> (
    crate::tools::policy::PolicyRef,
    crate::mcp::allowlist::McpAllowlist,
) {
    match &doc.policy {
        Some(m) => (
            Arc::new(crate::policy::ManifestPolicy::new(m.clone())),
            m.mcp_allowlist(),
        ),
        None => (
            crate::tools::policy::allow_all(),
            crate::mcp::allowlist::McpAllowlist::allow_all(),
        ),
    }
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
         [--validate-only]          AGENTD_VALIDATE_ONLY=1
         [--log-level LEVEL]        AGENTD_LOG             (default warn)
         [--log-format text|json]   AGENTD_LOG_FORMAT      (default text)
         [--log-target TARGET]      AGENTD_LOG_TARGET      stderr | stdout | file:PATH
         [--quiet]                  AGENTD_QUIET=1
         [--signing-required]       AGENTD_SIGNING_REQUIRED=1  fail-closed on unsigned
         [--signing-key-file PATH]  AGENTD_SIGNING_KEY_FILE    override pinned pubkey
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
}
