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

    let mut registry = HandlerRegistry::with_builtin_controls();
    crate::tools::register_default_tools(&mut registry, policy_for_tools, budget);

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
    if !backend_map.is_empty() {
        crate::intelligence::handler::register(&mut registry, Arc::new(backend_map.clone()));
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

    registry.set_fallback(Box::new(StubHandler));
    Ok(
        Engine::new(registry).with_reload_handles(crate::engine::ReloadHandles {
            policy: Some(reloadable_policy),
            intel: intel_reload,
            intel_backends: named_backends,
            mcp: Some(mcp_registry),
        }),
    )
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
        [--reload-file PATH]       AGENTD_RELOAD_FILE         touch-to-reload (Windows SIGHUP replacement)
        [--version] [--help]

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
}
