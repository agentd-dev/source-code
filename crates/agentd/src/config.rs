//! Configuration: precedence + validate-at-startup. RFC 0011 §2-§3.
//!
//! Precedence, top wins: `built-in default < env var < CLI flag`. Everything
//! is env-settable (12-factor). The whole config is validated **before any
//! side effect** — a bad config exits `2` in milliseconds, not after an LLM
//! round-trip.
//!
//! A config-file layer (which would slot between default and env) is
//! intentionally not built — env/flag are the complete, stable surface, and
//! secrets are env/flag only.

use crate::obs::log::Level;
use crate::sec::scope::TrifectaTag;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Execution mode — one supervisor loop, four exit predicates (RFC 0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Run the instruction once to a terminal status, then exit.
    Once,
    /// Keep working until a bound (iterations/deadline/tree-token) or signal.
    Loop,
    /// Idle; wake on MCP resource updates. Exits only on signal/fatal.
    Reactive,
    /// Per-fire identical to `once`, driven by an internal interval/cron.
    Schedule,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Once => "once",
            Mode::Loop => "loop",
            Mode::Reactive => "reactive",
            Mode::Schedule => "schedule",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "once" => Some(Mode::Once),
            "loop" => Some(Mode::Loop),
            "reactive" => Some(Mode::Reactive),
            "schedule" => Some(Mode::Schedule),
            _ => None,
        }
    }
}

/// Where `--serve-mcp` binds the served self-MCP (RFC 0015 §3.1). `Stdio` is the
/// implicit default (no `--serve-mcp`); the explicit targets are a unix socket or
/// — `--features vsock` — an AF_VSOCK port. The string forms are
/// `unix:PATH` | `vsock:PORT` | `vsock:CID:PORT`; `vsock:PORT` binds the wildcard
/// context id `VMADDR_CID_ANY`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeTarget {
    /// Bind a unix-domain socket at this path.
    Unix(std::path::PathBuf),
    /// Bind AF_VSOCK `(cid, port)`. `cid` is the wildcard `VMADDR_CID_ANY` for
    /// the bare `vsock:PORT` form.
    Vsock { cid: u32, port: u32 },
}

/// The wildcard listen context id (`VMADDR_CID_ANY`) for the bare `vsock:PORT`
/// form. Hard-coded (libc's value) so config parsing carries no `vsock`-feature
/// dependency — the binding itself is feature-gated in `mcp::server`.
pub const VMADDR_CID_ANY: u32 = 0xFFFF_FFFF;

impl ServeTarget {
    /// Parse a `--serve-mcp` value. Validates the scheme/port and, for `vsock:`,
    /// that this build has the `vsock` feature (mirroring the intelligence
    /// `https`-needs-`tls` scheme check). Returns a [`ConfigError::Usage`] (exit 2,
    /// before any side effect) on any problem.
    pub fn parse(spec: &str) -> Result<ServeTarget, ConfigError> {
        if let Some(path) = spec.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(usage("--serve-mcp: unix path is empty".into()));
            }
            return Ok(ServeTarget::Unix(path.into()));
        }
        if let Some(rest) = spec.strip_prefix("vsock:") {
            // The scheme is gated on the build, like https→tls — reject early so the
            // operator gets a crisp exit 2, not a silent inert listener.
            if !cfg!(feature = "vsock") {
                return Err(usage(
                    "--serve-mcp: scheme unsupported: vsock requires the 'vsock' build feature"
                        .into(),
                ));
            }
            let (cid, port_str) = match rest.split_once(':') {
                Some((c, p)) => {
                    let cid = c.parse::<u32>().map_err(|_| {
                        usage(format!(
                            "--serve-mcp: invalid vsock cid '{c}' (want a number)"
                        ))
                    })?;
                    (cid, p)
                }
                None => (VMADDR_CID_ANY, rest),
            };
            let port = port_str.parse::<u32>().map_err(|_| {
                usage(format!(
                    "--serve-mcp: invalid vsock port '{port_str}' (want a number)"
                ))
            })?;
            if port == 0 {
                return Err(usage("--serve-mcp: vsock port must be > 0".into()));
            }
            return Ok(ServeTarget::Vsock { cid, port });
        }
        Err(usage(format!(
            "--serve-mcp: scheme unsupported (want unix:PATH | vsock:PORT | vsock:CID:PORT): {spec}"
        )))
    }
}

/// A declared **A2A peer**: a name and a client transport endpoint to reach a
/// remote A2A agent (or the on-node gateway that forwards into the mesh). This
/// is the delegation-backend axis of RFC 0020 §3 — `a2a.delegate` looks a peer
/// up here and runs the A2A client against `endpoint`. The endpoint is one of
/// agentd's existing client transports: `unix:/path` or `vsock:CID:PORT`. No
/// secrets live here (the gateway is the PEP; the vsock peer is trusted, RFC
/// 0012 §3.8). Serializable so it travels in the spawn payload to subagents,
/// exactly like `mcp_servers` (RFC 0009 §spawn-payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aPeerSpec {
    pub name: String,
    pub endpoint: String,
}

impl A2aPeerSpec {
    /// Resolve this peer's endpoint string to a parsed [`A2aEndpoint`] for the
    /// A2A client to dial. Returns the validation message (without the `agentd:`
    /// prefix) on a bad scheme. The endpoint is validated at startup, so at run
    /// time this is expected to succeed; the `Result` keeps the call total.
    pub fn endpoint_of(&self) -> Result<A2aEndpoint, String> {
        A2aEndpoint::parse(&self.endpoint).map_err(|e| e.to_string())
    }
}

/// The client transport an [`A2aPeerSpec`] endpoint resolves to. Parsed once
/// (scheme-validated at startup), then the A2A client dials it. `vsock:CID:PORT`
/// requires both forms of a cid+port (no wildcard — a client dials a concrete
/// peer, unlike the `--serve-mcp` listen form which may wildcard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum A2aEndpoint {
    /// Connect to a unix-domain socket at this path.
    Unix(std::path::PathBuf),
    /// Connect to AF_VSOCK `(cid, port)`.
    Vsock { cid: u32, port: u32 },
}

impl A2aEndpoint {
    /// Parse an `--a2a-peer` endpoint. Validates the scheme/port and, for
    /// `vsock:`, that this build has the `vsock` feature — mirroring
    /// [`ServeTarget::parse`]. Returns a [`ConfigError::Usage`] (exit 2, before
    /// any side effect) on any problem.
    pub fn parse(spec: &str) -> Result<A2aEndpoint, ConfigError> {
        if let Some(path) = spec.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(usage("--a2a-peer: unix path is empty".into()));
            }
            return Ok(A2aEndpoint::Unix(path.into()));
        }
        if let Some(rest) = spec.strip_prefix("vsock:") {
            if !cfg!(feature = "vsock") {
                return Err(usage(
                    "--a2a-peer: scheme unsupported: vsock requires the 'vsock' build feature"
                        .into(),
                ));
            }
            // A client dials a concrete peer: CID:PORT is required (no wildcard).
            let (cid_str, port_str) = rest.split_once(':').ok_or_else(|| {
                usage(format!(
                    "--a2a-peer: vsock endpoint must be vsock:CID:PORT (got: vsock:{rest})"
                ))
            })?;
            let cid = cid_str.parse::<u32>().map_err(|_| {
                usage(format!(
                    "--a2a-peer: invalid vsock cid '{cid_str}' (want a number)"
                ))
            })?;
            let port = port_str.parse::<u32>().map_err(|_| {
                usage(format!(
                    "--a2a-peer: invalid vsock port '{port_str}' (want a number)"
                ))
            })?;
            if port == 0 {
                return Err(usage("--a2a-peer: vsock port must be > 0".into()));
            }
            return Ok(A2aEndpoint::Vsock { cid, port });
        }
        Err(usage(format!(
            "--a2a-peer: scheme unsupported (want unix:PATH | vsock:CID:PORT): {spec}"
        )))
    }
}

/// A declared MCP server: a name and the argv to spawn it (stdio transport).
/// Serializable because it travels in the subagent spawn payload as the
/// child's scoped server subset (RFC 0005, RFC 0009).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerSpec {
    pub name: String,
    pub command: Vec<String>,
    /// Operator-declared capability tags (`--mcp-tags`) for the Rule-of-Two
    /// trifecta check (RFC 0012 §3.1). Travels in the spawn payload so a child's
    /// narrowed grant carries the same tags. Empty = untagged (the check treats
    /// an untagged server conservatively as `untrusted_input`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<TrifectaTag>,
}

/// The fully-resolved, validated configuration.
#[derive(Clone, PartialEq)]
pub struct Config {
    pub instruction: Option<String>,
    pub intelligence: Option<String>,
    pub intelligence_token: Option<String>,
    /// Path to a mounted file holding the intelligence credential
    /// (`--intelligence-token-file` / `AGENTD_INTELLIGENCE_TOKEN_FILE`, RFC 0017
    /// §6.1). The token is read+trimmed from this file at load (and re-readable
    /// for rotation); the resolved value lands in `intelligence_token`, never in
    /// a log. The `--intelligence-token` flag/env stays as the inline source.
    pub intelligence_token_file: Option<String>,
    pub model: Option<String>,
    pub mcp_servers: Vec<McpServerSpec>,
    /// Declared remote-A2A delegation peers (`--a2a-peer name=endpoint`). The
    /// delegation-backend axis of RFC 0020 §3: `a2a.delegate` dials these. Only
    /// honoured in `--features a2a` builds (validated at startup).
    pub a2a_peers: Vec<A2aPeerSpec>,
    pub mode: Mode,
    pub subscribe: Vec<String>,
    /// Subscriptions routed to a **warm continue-session** rather than a fresh
    /// spawn per event: all events on the URI re-enter one live session, in
    /// order (RFC 0008 §spawn-vs-continue). Repeatable `--continue <uri>`.
    pub continue_subscribe: Vec<String>,
    pub interval: Option<Duration>,
    pub max_steps: u32,
    pub max_tokens: u64,
    pub deadline: Option<Duration>,
    pub max_depth: u32,
    pub run_id: String,
    pub log_level: Level,
    pub drain_timeout: Duration,
    pub enable_exec: bool,
    pub serve_mcp: Option<String>,
    pub health_file: Option<String>,
    /// Inbound W3C `traceparent` to continue (else a trace is minted from the
    /// run id). RFC 0010 §context-propagation.
    pub traceparent: Option<String>,
    /// Opt-in content capture (RFC 0010 §2.9). Off by default: telemetry logs
    /// hashes/lengths only; `--log-content` adds the actual tool args/results
    /// (truncated). Propagates to children via the telemetry block.
    pub log_content: bool,
    /// Opt-in HTTP probe/scrape surface (`/metrics` + `/healthz` + `/readyz`).
    /// Off unless set; only honoured in `--features metrics` builds. RFC 0010.
    pub metrics_addr: Option<String>,
    /// Opt-in cgroup-v2 active enforcement: `auto` (derive `<own-cgroup>/agentd`)
    /// or an absolute path under `/sys/fs/cgroup`. Each run gets a child cgroup
    /// for atomic `cgroup.kill` teardown. Best-effort — disabled if not writable;
    /// agentd stays cgroup-aware, never cgroup-requiring. RFC 0010, assessment §2.3.
    /// Note: if hard limits are requested and the path points at a shared/existing
    /// cgroup, delegating its controllers also enables them for its other children.
    pub cgroup: Option<String>,
    /// Optional hard `memory.max` for each run's cgroup (`max` or a size like
    /// `512M`/`2G`/bytes). Needs `--cgroup` + a parent that can delegate the
    /// `memory` controller; otherwise it no-ops (teardown still works).
    pub cgroup_memory_max: Option<String>,
    /// Optional hard `pids.max` for each run's cgroup (`max` or a count). Counts
    /// *threads*, not just processes, so set it generously (the root subagent is
    /// multi-threaded). Same delegation requirement as `cgroup_memory_max`.
    pub cgroup_pids_max: Option<String>,
    /// Allow a lethal-trifecta grant (all three capability legs in one agent)
    /// instead of refusing at startup (RFC 0012 §3.2). Process-global operator
    /// override — deliberately NOT carried in the spawn payload.
    pub allow_trifecta: bool,
    /// Optional 5-field UTC cron schedule for `--mode schedule` (RFC 0008).
    /// Only honoured in `--features cron` builds; the production path is an
    /// external CronJob → `--mode once`.
    pub cron: Option<String>,
    /// Where to write the run-outcome report at the terminal transition
    /// (`--report-file PATH` / `AGENTD_REPORT_FILE`, RFC 0016 §6.3). Atomic write
    /// (temp + rename). Off for a bare CLI run; inert for `--mode reactive`
    /// (warned at startup — a reactive daemon has no single terminal outcome,
    /// §6.4).
    pub report_file: Option<String>,
    /// Capacity of the bounded `agentd://events` ring (`--events-ring N` /
    /// `AGENTD_EVENTS_RING`, RFC 0016 §7.2/§11): the last N emitted lines held in
    /// memory for the live-tail resource. Default 1024. Only consumed when the
    /// `events` surface is served (`--serve-mcp` + the `events` feature).
    pub events_ring: usize,
    /// Declared intelligence HTTP headers (RFC 0006 §3, settable only via the
    /// config file's `intelligence_headers`, RFC 0017 §3.3). Values are
    /// **templates** that may carry `{{secret:NAME}}` / `{{secret-file:PATH}}`
    /// refs (§6) — the NAMES/refs are structural; the resolved secret is never
    /// stored here or logged. An inline secret-shaped value is rejected at
    /// validation (§3.1). A `BTreeMap` so the order is deterministic.
    pub intelligence_headers: std::collections::BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            instruction: None,
            intelligence: None,
            intelligence_token: None,
            intelligence_token_file: None,
            model: None,
            mcp_servers: Vec::new(),
            a2a_peers: Vec::new(),
            mode: Mode::Once,
            subscribe: Vec::new(),
            continue_subscribe: Vec::new(),
            interval: None,
            max_steps: 50,
            max_tokens: 200_000,
            deadline: Some(Duration::from_secs(600)),
            max_depth: 4,
            run_id: String::new(), // filled in load() if unset
            log_level: Level::Info,
            drain_timeout: Duration::from_secs(25),
            enable_exec: false,
            serve_mcp: None,
            health_file: None,
            traceparent: None,
            log_content: false,
            metrics_addr: None,
            cgroup: None,
            cgroup_memory_max: None,
            cgroup_pids_max: None,
            allow_trifecta: false,
            cron: None,
            report_file: None,
            events_ring: crate::obs::log::EVENTS_RING_DEFAULT,
            intelligence_headers: std::collections::BTreeMap::new(),
        }
    }
}

// Redact the credential — never let it reach a log or a panic message.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("instruction", &self.instruction.as_deref().map(|_| "<set>"))
            .field("intelligence", &self.intelligence)
            .field(
                "intelligence_token",
                &self.intelligence_token.as_ref().map(|_| "***"),
            )
            .field("intelligence_token_file", &self.intelligence_token_file)
            .field("model", &self.model)
            .field("mcp_servers", &self.mcp_servers)
            .field("a2a_peers", &self.a2a_peers)
            .field("mode", &self.mode)
            .field("subscribe", &self.subscribe)
            .field("continue_subscribe", &self.continue_subscribe)
            .field("interval", &self.interval)
            .field("max_steps", &self.max_steps)
            .field("max_tokens", &self.max_tokens)
            .field("deadline", &self.deadline)
            .field("max_depth", &self.max_depth)
            .field("run_id", &self.run_id)
            .field("log_level", &self.log_level)
            .field("drain_timeout", &self.drain_timeout)
            .field("enable_exec", &self.enable_exec)
            .field("serve_mcp", &self.serve_mcp)
            .field("health_file", &self.health_file)
            .field("traceparent", &self.traceparent)
            .field("log_content", &self.log_content)
            .field("metrics_addr", &self.metrics_addr)
            .field("cgroup", &self.cgroup)
            .field("cgroup_memory_max", &self.cgroup_memory_max)
            .field("cgroup_pids_max", &self.cgroup_pids_max)
            .field("allow_trifecta", &self.allow_trifecta)
            .field("cron", &self.cron)
            .field("report_file", &self.report_file)
            .field("events_ring", &self.events_ring)
            // Header NAMES only — a value may carry a {{secret:…}} ref, so redact
            // the values defensively (RFC 0012 §3.7: never log a secret).
            .field(
                "intelligence_headers",
                &self.intelligence_headers.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// What `load()` can short-circuit with. `Help`/`Version`/`Capabilities` are
/// *not* errors (exit 0); `Usage` is a validation/parse failure (exit 2,
/// RFC 0011 §5). `Capabilities` carries the pretty-printed manifest JSON — the
/// side-effect-free admission probe (`agentd --capabilities`, RFC 0015 §5.2),
/// short-circuited before run-required validation so it succeeds even with no
/// instruction (agentctl probes an image without a full run config).
#[derive(Debug)]
pub enum ConfigError {
    Help(String),
    Version(String),
    Capabilities(String),
    Usage(String),
    /// `--config-schema` (RFC 0017 §4.2): the JSON Schema of the config file,
    /// printed to **stdout**, exit 0 — a side-effect-free schema export so
    /// agentctl can validate a CR before applying it.
    Schema(String),
    /// `--validate-config` (RFC 0017 §4.1): the admission verdict. `Ok(line)` is
    /// a valid config (one `config.valid` line, exit 0); `Err(lines)` is one or
    /// more `config.invalid` diagnostics (exit 2). The caller prints to stderr.
    Validate(Result<String, String>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Help(s)
            | ConfigError::Version(s)
            | ConfigError::Capabilities(s)
            | ConfigError::Schema(s) => {
                write!(f, "{s}")
            }
            ConfigError::Usage(s) => write!(f, "{s}"),
            ConfigError::Validate(Ok(s)) | ConfigError::Validate(Err(s)) => write!(f, "{s}"),
        }
    }
}

impl Config {
    /// Resolve config from CLI args (excluding the leading program name) and the
    /// environment, applying precedence (`built-in default < FILE < env < flag`,
    /// RFC 0011 §2.1 / RFC 0017 §3.2) and validating.
    pub fn load(args: &[String], env: &[(String, String)]) -> Result<Config, ConfigError> {
        let envmap: HashMap<&str, &str> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        // `--config-schema` (RFC 0017 §4.2): a side-effect-free schema export.
        // The schema is static (generated from the `ConfigFile` types), so it
        // short-circuits BEFORE the file is even read — exit 0, JSON to stdout.
        if args.iter().any(|a| a == "--config-schema") {
            let schema = crate::config_file::config_schema();
            let json = serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string());
            return Err(ConfigError::Schema(format!("{json}\n")));
        }
        // `--validate-config` (RFC 0017 §4.1): captured here, acted on at the end.
        // It is the side-effect-free admission verdict — it validates whatever
        // config is given and never requires an --instruction to *validate*.
        let validate_config = args.iter().any(|a| a == "--validate-config");

        let mut c = Config::default();

        // --- FILE layer (RFC 0017 §3, precedence layer 1) ---
        // `--config <path>` / `AGENTD_CONFIG`. The file is the lowest non-default
        // layer: env and flags below override it; repeatable list flags ADD to
        // the file's lists (§3.2). A malformed/unreadable file is exit 2 BEFORE
        // any side effect (it is parsed before the env/flag layers touch `c`).
        let config_path = scan_flag_value(args, "--config")
            .or_else(|| envmap.get("AGENTD_CONFIG").map(|v| v.to_string()));
        if let Some(path) = &config_path {
            let cf = crate::config_file::ConfigFile::load(path).map_err(usage)?;
            apply_config_file(&mut c, cf)?;
        }

        // --- env layer ---
        if let Some(v) = envmap.get("INSTRUCTION") {
            c.instruction = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_INTELLIGENCE") {
            c.intelligence = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_INTELLIGENCE_TOKEN") {
            c.intelligence_token = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_INTELLIGENCE_TOKEN_FILE") {
            c.intelligence_token_file = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_MODEL") {
            c.model = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_MODE") {
            c.mode = Mode::parse(v).ok_or_else(|| usage(format!("invalid AGENTD_MODE: {v}")))?;
        }
        if let Some(v) = envmap.get("AGENTD_MAX_STEPS") {
            c.max_steps = v
                .parse()
                .map_err(|_| usage(format!("invalid AGENTD_MAX_STEPS: {v}")))?;
        }
        if let Some(v) = envmap.get("AGENTD_MAX_TOKENS") {
            c.max_tokens = v
                .parse()
                .map_err(|_| usage(format!("invalid AGENTD_MAX_TOKENS: {v}")))?;
        }
        if let Some(v) = envmap.get("AGENTD_DEADLINE") {
            c.deadline = Some(parse_duration(v).map_err(usage)?);
        }
        if let Some(v) = envmap.get("AGENTD_RUN_ID") {
            c.run_id = (*v).to_string();
        }
        if let Some(v) = envmap.get("AGENTD_LOG_LEVEL") {
            c.log_level =
                Level::parse(v).ok_or_else(|| usage(format!("invalid AGENTD_LOG_LEVEL: {v}")))?;
        }
        if let Some(v) = envmap.get("AGENTD_DRAIN_TIMEOUT") {
            c.drain_timeout = parse_duration(v).map_err(usage)?;
        }
        if let Some(v) = envmap.get("AGENTD_ENABLE_EXEC") {
            c.enable_exec = truthy(v);
        }
        if let Some(v) = envmap.get("AGENTD_LOG_CONTENT") {
            c.log_content = truthy(v);
        }
        if let Some(v) = envmap.get("AGENTD_METRICS_ADDR") {
            c.metrics_addr = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_CGROUP") {
            c.cgroup = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_CGROUP_MEMORY_MAX") {
            c.cgroup_memory_max = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_CGROUP_PIDS_MAX") {
            c.cgroup_pids_max = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_ALLOW_TRIFECTA") {
            c.allow_trifecta = truthy(v);
        }
        if let Some(v) = envmap.get("AGENTD_CRON") {
            c.cron = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_REPORT_FILE") {
            c.report_file = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_EVENTS_RING") {
            c.events_ring = v
                .parse()
                .map_err(|_| usage(format!("invalid AGENTD_EVENTS_RING: {v}")))?;
        }
        if let Some(v) = envmap.get("AGENTD_SERVE_MCP") {
            c.serve_mcp = Some((*v).to_string());
        }
        // A single `AGENTD_A2A_PEER` env declares one peer (the env channel is
        // one value; declare more with repeated `--a2a-peer` flags). RFC 0020 §3.
        if let Some(v) = envmap.get("AGENTD_A2A_PEER") {
            c.a2a_peers.push(parse_a2a_peer_spec(v)?);
        }
        if let Some(v) = envmap.get("AGENTD_TRACEPARENT") {
            c.traceparent = Some((*v).to_string());
        }

        // --- flag layer (overrides env) ---
        // `--mcp-tags` may precede or follow its `--mcp`; collect and apply once
        // every server is known.
        let mut mcp_tags: Vec<(String, Vec<TrifectaTag>)> = Vec::new();
        // `--capabilities` is the admission probe (RFC 0015 §5.2): captured here
        // and resolved after the whole config is parsed but BEFORE run-required
        // validation, so it reflects whatever config is present and succeeds with
        // no instruction.
        let mut capabilities = false;
        let mut it = args.iter().peekable();
        while let Some(arg) = it.next() {
            let mut take = |name: &str| -> Result<String, ConfigError> {
                it.next()
                    .cloned()
                    .ok_or_else(|| usage(format!("{name} requires a value")))
            };
            match arg.as_str() {
                "-h" | "--help" => return Err(ConfigError::Help(help_text())),
                "-V" | "--version" => {
                    return Err(ConfigError::Version(format!("agentd {}\n", crate::VERSION)));
                }
                "--capabilities" => capabilities = true,
                // Already resolved into the FILE layer above; consume its value
                // here so the arg-loop doesn't reject it as unknown.
                "--config" => {
                    let _ = take("--config")?;
                }
                // Flags acted on outside the arg loop (schema short-circuits at the
                // top of load; validate is acted on after full resolution). They
                // take no value — accept and ignore here.
                "--config-schema" | "--validate-config" => {}
                "--instruction" => c.instruction = Some(take("--instruction")?),
                "--intelligence-token-file" => {
                    c.intelligence_token_file = Some(take("--intelligence-token-file")?)
                }
                "--instruction-file" => {
                    let p = take("--instruction-file")?;
                    c.instruction = Some(read_file(&p)?);
                }
                "--intelligence" => c.intelligence = Some(take("--intelligence")?),
                "--intelligence-token" => {
                    c.intelligence_token = Some(take("--intelligence-token")?)
                }
                "--model" => c.model = Some(take("--model")?),
                "--mcp" => {
                    let spec = take("--mcp")?;
                    c.mcp_servers.push(parse_mcp_spec(&spec)?);
                }
                "--a2a-peer" => {
                    let spec = take("--a2a-peer")?;
                    c.a2a_peers.push(parse_a2a_peer_spec(&spec)?);
                }
                "--mode" => {
                    let v = take("--mode")?;
                    c.mode =
                        Mode::parse(&v).ok_or_else(|| usage(format!("invalid --mode: {v}")))?;
                }
                "--subscribe" => c.subscribe.push(take("--subscribe")?),
                "--continue" => c.continue_subscribe.push(take("--continue")?),
                "--interval" => {
                    c.interval = Some(parse_duration(&take("--interval")?).map_err(usage)?)
                }
                "--cron" => c.cron = Some(take("--cron")?),
                "--max-steps" => {
                    let v = take("--max-steps")?;
                    c.max_steps = v
                        .parse()
                        .map_err(|_| usage(format!("invalid --max-steps: {v}")))?;
                }
                "--max-tokens" => {
                    let v = take("--max-tokens")?;
                    c.max_tokens = v
                        .parse()
                        .map_err(|_| usage(format!("invalid --max-tokens: {v}")))?;
                }
                "--deadline" => {
                    c.deadline = Some(parse_duration(&take("--deadline")?).map_err(usage)?)
                }
                "--max-depth" => {
                    let v = take("--max-depth")?;
                    c.max_depth = v
                        .parse()
                        .map_err(|_| usage(format!("invalid --max-depth: {v}")))?;
                }
                "--run-id" => c.run_id = take("--run-id")?,
                "--log-level" => {
                    let v = take("--log-level")?;
                    c.log_level = Level::parse(&v)
                        .ok_or_else(|| usage(format!("invalid --log-level: {v}")))?;
                }
                "--drain-timeout" => {
                    c.drain_timeout = parse_duration(&take("--drain-timeout")?).map_err(usage)?
                }
                "--enable-exec" => c.enable_exec = true,
                "--log-content" => c.log_content = true,
                "--allow-trifecta" => c.allow_trifecta = true,
                "--mcp-tags" => mcp_tags.push(parse_mcp_tags(&take("--mcp-tags")?)?),
                "--metrics-addr" => c.metrics_addr = Some(take("--metrics-addr")?),
                "--cgroup" => c.cgroup = Some(take("--cgroup")?),
                "--cgroup-memory-max" => c.cgroup_memory_max = Some(take("--cgroup-memory-max")?),
                "--cgroup-pids-max" => c.cgroup_pids_max = Some(take("--cgroup-pids-max")?),
                "--serve-mcp" => c.serve_mcp = Some(take("--serve-mcp")?),
                "--health-file" => c.health_file = Some(take("--health-file")?),
                "--traceparent" => c.traceparent = Some(take("--traceparent")?),
                "--report-file" => c.report_file = Some(take("--report-file")?),
                "--events-ring" => {
                    let v = take("--events-ring")?;
                    c.events_ring = v
                        .parse()
                        .map_err(|_| usage(format!("invalid --events-ring: {v}")))?;
                }
                other => return Err(usage(format!("unknown argument: {other}"))),
            }
        }

        // Apply collected `--mcp-tags` to their servers (order-independent).
        for (name, tags) in mcp_tags {
            match c.mcp_servers.iter_mut().find(|s| s.name == name) {
                Some(s) => s.tags = tags,
                None => {
                    return Err(usage(format!(
                        "--mcp-tags references unknown server '{name}'"
                    )));
                }
            }
        }

        if c.run_id.is_empty() {
            c.run_id = generate_run_id();
        }

        // `--capabilities`: build the manifest from whatever config IS present +
        // the downward-API identity, and short-circuit BEFORE run-required
        // validation (RFC 0015 §5.2). This is the side-effect-free admission
        // probe — it must succeed with no --instruction, so it never reaches the
        // `validate()` below. The caller prints the JSON and exits 0.
        if capabilities {
            let identity = crate::identity::Identity::from_env(&c.run_id);
            let manifest = crate::capabilities::manifest(&c, &identity, false);
            let json = serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".to_string());
            return Err(ConfigError::Capabilities(format!("{json}\n")));
        }

        // Resolve `--intelligence-token-file` into the token (RFC 0017 §6.1). An
        // inline `--intelligence-token`/env wins (it is the higher-precedence
        // source); the file is the fallback. Read+trimmed here, but a missing
        // file is reported through `validate()` so `--validate-config` collects it
        // with the rest, and the resolved value never reaches a log.
        c.resolve_token_file()?;

        // `--validate-config` (RFC 0017 §4.1): the side-effect-free admission
        // verdict. Run the FULL validation pipeline, collecting EVERY diagnostic
        // (not fast-failing on the first, unlike startup) so an operator/CI sees
        // all problems in one pass, then short-circuit with the verdict. It does
        // NOT require an --instruction to *validate* — it validates whatever it is
        // given. The caller prints to stderr and maps the result to exit 0/2.
        if validate_config {
            return Err(ConfigError::Validate(c.validate_collect_all()));
        }

        c.validate()?;
        Ok(c)
    }

    /// Resolve `--intelligence-token-file` into `intelligence_token` when no
    /// inline token is set (RFC 0017 §6.1). A read failure is surfaced as a usage
    /// error (exit 2 at startup; collected by `--validate-config`). The token is
    /// never logged — the error carries only the path.
    fn resolve_token_file(&mut self) -> Result<(), ConfigError> {
        if self.intelligence_token.is_some() {
            return Ok(()); // inline source wins (higher precedence)
        }
        if let Some(path) = self.intelligence_token_file.clone() {
            let tok = crate::sec::secret::read_token_file(&path).map_err(usage)?;
            self.intelligence_token = Some(tok);
        }
        Ok(())
    }

    /// Run the full validation pipeline collecting EVERY diagnostic as one NDJSON
    /// `config.{valid,invalid}` line set (RFC 0017 §4.1). `Ok(line)` ⇒ valid
    /// (exit 0); `Err(lines)` ⇒ one-or-more `config.invalid` lines (exit 2).
    ///
    /// Each independent check is run and its message collected, so the operator
    /// sees all problems at once. The check SET is exactly `validate()`'s — there
    /// is one validation authority, so the admission gate and the startup path
    /// can never disagree (RFC 0017 §7).
    fn validate_collect_all(&self) -> Result<String, String> {
        let mut diags: Vec<String> = Vec::new();
        // The single validate() pipeline is fast-fail; to collect ALL problems we
        // re-run it after fixing each surfaced error would be O(n²) and brittle.
        // Instead we run the independent declarative checks directly and append
        // each failing one. The header/secret checks (this RFC) plus a final
        // `validate()` pass (which catches anything not separately enumerated)
        // give complete coverage with a single source of truth.
        self.collect_header_diags(&mut diags);
        // Run the authoritative validate() and, if it fails, record its message
        // (it is fast-fail, so this is the first non-header structural problem).
        // `validate()` also runs the header check, so skip a duplicate when the
        // failure is a header diag we already collected.
        if let Err(e) = self.validate() {
            let msg = e.to_string();
            if !diags.iter().any(|d| msg.ends_with(d.as_str())) {
                diags.push(msg);
            }
        }
        if diags.is_empty() {
            Ok(config_valid_line())
        } else {
            Err(diags
                .into_iter()
                .map(|d| config_invalid_line(&d))
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }

    /// Validate the declared `intelligence_headers` (RFC 0017 §3.1/§6): a value
    /// may be a plain scalar or carry `{{secret:NAME}}` / `{{secret-file:PATH}}`
    /// refs, but an **inline secret-shaped value** (a header named like a
    /// credential whose value is NOT a ref) is rejected — a secret must be a
    /// reference, never a literal in the file. Every ref must also resolve
    /// (the env var is set; the file exists), else exit 2 (§6.2).
    fn collect_header_diags(&self, diags: &mut Vec<String>) {
        let env = |k: &str| std::env::var(k).ok();
        for (name, value) in &self.intelligence_headers {
            // A credential-shaped header carrying a literal (non-ref) value is the
            // "inline secret in the file" footgun — reject it (RFC 0017 §3.1).
            if is_secret_shaped_key(name) && !crate::sec::secret::has_secret_ref(value) {
                diags.push(format!(
                    "intelligence_headers['{name}'] looks like a credential but has an inline value; \
                     use {{{{secret:NAME}}}} or {{{{secret-file:PATH}}}} (never an inline secret)"
                ));
                continue;
            }
            // Every secret ref must resolve at startup (§6.2): a missing env var
            // or an unreadable file is exit 2 before any side effect.
            if crate::sec::secret::has_secret_ref(value)
                && let Err(e) = crate::sec::secret::refs_resolvable(value, &env)
            {
                diags.push(format!("intelligence_headers['{name}']: {e}"));
            }
        }
    }

    /// The capability-tag union of the root agent's grant, for the Rule-of-Two
    /// trifecta check (RFC 0012 §3.1). An untagged MCP server contributes
    /// `untrusted_input` (the conservative default); `--enable-exec` contributes
    /// `egress` (exec moves data / changes external state). Because scope narrows
    /// monotonically (RFC 0009), enforcing on this root union bounds the whole
    /// subagent tree.
    pub fn trifecta_grant_tags(&self) -> Vec<TrifectaTag> {
        let mut tags = Vec::new();
        for s in &self.mcp_servers {
            if s.tags.is_empty() {
                tags.push(TrifectaTag::UntrustedInput);
            } else {
                tags.extend(s.tags.iter().copied());
            }
        }
        if self.enable_exec {
            tags.push(TrifectaTag::Egress);
        }
        tags
    }

    /// Reject inconsistent config before any side effect (RFC 0011 §2).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self
            .instruction
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            return Err(usage(
                "missing instruction (INSTRUCTION env or --instruction)".into(),
            ));
        }
        if self.intelligence.as_deref().unwrap_or("").is_empty() {
            return Err(usage(
                "missing intelligence endpoint (AGENTD_INTELLIGENCE or --intelligence)".into(),
            ));
        }
        validate_intelligence_uri(self.intelligence.as_deref().unwrap())?;
        for s in &self.mcp_servers {
            if s.name.is_empty() || s.command.is_empty() {
                return Err(usage(format!(
                    "mcp server '{}' has empty name or command",
                    s.name
                )));
            }
        }
        if self.max_steps == 0 {
            return Err(usage("--max-steps must be > 0".into()));
        }
        // A zero events ring would hold nothing (every push instantly evicts) —
        // reject it so an operator who wants the live-tail surface gets a usable
        // window (RFC 0016 §7.2). Off-by-default; only consumed when serving.
        if self.events_ring == 0 {
            return Err(usage("--events-ring must be > 0".into()));
        }
        if self.mode == Mode::Reactive
            && self.subscribe.is_empty()
            && self.continue_subscribe.is_empty()
        {
            return Err(usage(
                "--mode reactive requires at least one --subscribe or --continue <uri>".into(),
            ));
        }
        if !self.continue_subscribe.is_empty() && self.mode != Mode::Reactive {
            return Err(usage(
                "--continue is only valid with --mode reactive".into(),
            ));
        }
        if self.mode == Mode::Schedule && self.interval.is_none() && self.cron.is_none() {
            return Err(usage(
                "--mode schedule requires --interval <dur> or --cron <expr>".into(),
            ));
        }
        if self.cron.is_some() && self.mode != Mode::Schedule {
            return Err(usage("--cron is only valid with --mode schedule".into()));
        }
        // The per-run limits do nothing without a cgroup to apply them to, so a
        // limit set alone is a misconfiguration (the operator believes the run is
        // bounded when it isn't) — surface it, like --cron/--continue.
        if (self.cgroup_memory_max.is_some() || self.cgroup_pids_max.is_some())
            && self.cgroup.is_none()
        {
            return Err(usage(
                "--cgroup-memory-max/--cgroup-pids-max require --cgroup".into(),
            ));
        }
        // A zero limit can never let the agent run: pids.max=0 refuses placement
        // (the run loses both limits and the cgroup.kill backstop) and memory.max=0
        // OOM-kills instantly. Reject it outright (use a real value or `max`).
        if self.cgroup_pids_max.as_deref().map(str::trim) == Some("0") {
            return Err(usage(
                "--cgroup-pids-max must be > 0 (it counts threads, not just processes) or 'max'"
                    .into(),
            ));
        }
        if self.cgroup_memory_max.as_deref().map(str::trim) == Some("0") {
            return Err(usage("--cgroup-memory-max must be > 0 or 'max'".into()));
        }
        // Validate the served-MCP target up front (RFC 0015 §3.1): a bad scheme,
        // a vsock target on a non-vsock build, or a zero/non-numeric port exits 2
        // before any listener is bound — mirroring the intelligence-URI check.
        if let Some(spec) = &self.serve_mcp {
            ServeTarget::parse(spec)?;
        }
        // Declared A2A delegation peers (RFC 0020 §3) need the `a2a` build
        // feature, and each endpoint scheme is validated up front (exit 2 before
        // any side effect) — mirroring the served-MCP target check.
        if !self.a2a_peers.is_empty() && !cfg!(feature = "a2a") {
            return Err(usage("--a2a-peer requires the 'a2a' build feature".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for peer in &self.a2a_peers {
            if peer.name.is_empty() || peer.endpoint.is_empty() {
                return Err(usage(format!(
                    "--a2a-peer '{}' has an empty name or endpoint",
                    peer.name
                )));
            }
            if !seen.insert(peer.name.as_str()) {
                return Err(usage(format!(
                    "--a2a-peer name '{}' is declared more than once",
                    peer.name
                )));
            }
            A2aEndpoint::parse(&peer.endpoint)?;
        }
        // Declared intelligence headers (RFC 0017 §3.1/§6): reject an inline
        // secret-shaped value, and require every {{secret…}} ref to resolve. The
        // `--validate-config` path runs the same check via `collect_header_diags`
        // (collecting all), so the admission gate and startup never disagree.
        let mut header_diags = Vec::new();
        self.collect_header_diags(&mut header_diags);
        if let Some(first) = header_diags.into_iter().next() {
            return Err(usage(first));
        }
        Ok(())
    }
}

/// Heuristic: is this header name credential-shaped (RFC 0011 §3.2 / RFC 0017
/// §3.1)? A header so named must carry a `{{secret:…}}` *reference*, not an
/// inline literal — so a secret can never be smuggled into the config file.
fn is_secret_shaped_key(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "authorization"
        || n == "x-api-key"
        || n == "api-key"
        || n == "token"
        || n.ends_with("-token")
        || n.ends_with("_token")
        || n == "password"
        || n == "secret"
        || n.ends_with("-key")
        || n.ends_with("_key")
}

/// The single-line `config.valid` verdict (RFC 0017 §4.1), to stderr, exit 0.
fn config_valid_line() -> String {
    serde_json::json!({"event": "config.valid"}).to_string()
}

/// One machine-actionable `config.invalid` diagnostic line (RFC 0017 §4.1),
/// to stderr, exit 2. `msg` is the human-readable reason.
fn config_invalid_line(msg: &str) -> String {
    serde_json::json!({"event": "config.invalid", "msg": msg}).to_string()
}

fn validate_intelligence_uri(uri: &str) -> Result<(), ConfigError> {
    let ok = uri.starts_with("https://")
        || uri.starts_with("unix:")
        || uri.starts_with("vsock:")
        || uri.starts_with("http://"); // dev only; the client warns
    if ok {
        Ok(())
    } else {
        Err(usage(format!(
            "intelligence endpoint must be unix:/path, https://host/…, or vsock:cid:port (got: {uri})"
        )))
    }
}

/// Parse `--mcp name=cmd arg arg`. The command is whitespace-split into argv;
/// quoting/escaping is not supported (declare such servers via a wrapper
/// script).
fn parse_mcp_spec(spec: &str) -> Result<McpServerSpec, ConfigError> {
    let (name, cmd) = spec
        .split_once('=')
        .ok_or_else(|| usage(format!("--mcp must be name=command (got: {spec})")))?;
    let command: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
    if name.is_empty() || command.is_empty() {
        return Err(usage(format!("--mcp '{spec}' has empty name or command")));
    }
    Ok(McpServerSpec {
        name: name.to_string(),
        command,
        tags: Vec::new(),
    })
}

/// Parse `--a2a-peer name=endpoint` into an [`A2aPeerSpec`] (RFC 0020 §3). The
/// endpoint is the remainder after the FIRST `=` (so `unix:`/`vsock:` schemes —
/// which contain no `=` — pass through verbatim); the scheme itself is validated
/// later in [`Config::validate`] via [`A2aEndpoint::parse`].
fn parse_a2a_peer_spec(spec: &str) -> Result<A2aPeerSpec, ConfigError> {
    let (name, endpoint) = spec
        .split_once('=')
        .ok_or_else(|| usage(format!("--a2a-peer must be name=endpoint (got: {spec})")))?;
    if name.is_empty() || endpoint.is_empty() {
        return Err(usage(format!(
            "--a2a-peer '{spec}' has an empty name or endpoint"
        )));
    }
    Ok(A2aPeerSpec {
        name: name.to_string(),
        endpoint: endpoint.to_string(),
    })
}

/// Parse `--mcp-tags name=tag,tag` into (server-name, tags). Tags are the
/// snake-case capability legs (RFC 0012 §3.1).
fn parse_mcp_tags(spec: &str) -> Result<(String, Vec<TrifectaTag>), ConfigError> {
    let (name, list) = spec
        .split_once('=')
        .ok_or_else(|| usage(format!("--mcp-tags must be name=tag,tag (got: {spec})")))?;
    if name.is_empty() {
        return Err(usage(format!(
            "--mcp-tags '{spec}' has an empty server name"
        )));
    }
    let mut tags = Vec::new();
    for t in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let tag = TrifectaTag::parse(t).ok_or_else(|| {
            usage(format!(
                "unknown trifecta tag '{t}' (want: untrusted_input|sensitive|egress)"
            ))
        })?;
        tags.push(tag);
    }
    Ok((name.to_string(), tags))
}

fn read_file(path: &str) -> Result<String, ConfigError> {
    std::fs::read_to_string(path)
        .map_err(|e| usage(format!("cannot read instruction file {path}: {e}")))
}

/// Scan `args` for the value following the first occurrence of `flag` (a
/// `--flag VALUE` pair). Used to resolve `--config` BEFORE the main arg loop so
/// the file can seed the lowest layer. Returns `None` if absent or value-less.
fn scan_flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
    }
    None
}

/// Apply a parsed [`crate::config_file::ConfigFile`] onto `c` as the FILE layer
/// (RFC 0017 §3.2, precedence layer 1). Only keys the file actually sets are
/// written (field-wise); env/flags below override them. List-valued keys
/// (`mcp_servers`, `subscribe`, `a2a_peers`) **seed** the list — repeatable
/// flags ADD to them. Maps the file's `command`+`argv` into the runtime
/// `McpServerSpec` argv, and flattens the glob→tags map to the server's tag set.
fn apply_config_file(
    c: &mut Config,
    cf: crate::config_file::ConfigFile,
) -> Result<(), ConfigError> {
    if let Some(model) = cf.model {
        c.model = Some(model);
    }
    if let Some(mt) = cf.max_tokens {
        c.max_tokens = mt;
    }
    if let Some(limits) = cf.limits {
        if let Some(s) = limits.max_steps {
            c.max_steps = s;
        }
        if let Some(d) = limits.max_depth {
            c.max_depth = d;
        }
        if let Some(secs) = limits.deadline_secs {
            c.deadline = Some(Duration::from_secs(secs));
        }
    }
    if let Some(level) = cf.log_level {
        c.log_level = Level::parse(&level)
            .ok_or_else(|| usage(format!("config file: invalid log_level: {level}")))?;
    }
    // mcp_servers: each file object → one McpServerSpec (command + argv → argv;
    // the glob→tags map flattens to the union of declared tags). Seeds the list.
    for s in cf.mcp_servers {
        if s.name.is_empty() || s.command.is_empty() {
            return Err(usage(format!(
                "config file: mcp server '{}' has an empty name or command",
                s.name
            )));
        }
        if let Some(t) = &s.transport {
            // stdio is the only transport the client speaks today; reject an
            // unknown one at parse (exit 2) rather than silently ignoring it.
            if t != "stdio" && t != "unix" {
                return Err(usage(format!(
                    "config file: mcp server '{}' has unsupported transport '{t}' (want stdio)",
                    s.name
                )));
            }
        }
        let mut command = vec![s.command];
        command.extend(s.argv);
        let mut tags: Vec<TrifectaTag> = Vec::new();
        for tag_list in s.tags.values() {
            for t in tag_list {
                let tag = TrifectaTag::parse(t).ok_or_else(|| {
                    usage(format!(
                        "config file: mcp server '{}' has unknown trifecta tag '{t}' \
                         (want: untrusted_input|sensitive|egress)",
                        s.name
                    ))
                })?;
                if !tags.contains(&tag) {
                    tags.push(tag);
                }
            }
        }
        c.mcp_servers.push(McpServerSpec {
            name: s.name,
            command,
            tags,
        });
    }
    c.subscribe.extend(cf.subscribe);
    for p in cf.a2a_peers {
        if p.name.is_empty() || p.endpoint.is_empty() {
            return Err(usage(format!(
                "config file: a2a peer '{}' has an empty name or endpoint",
                p.name
            )));
        }
        c.a2a_peers.push(A2aPeerSpec {
            name: p.name,
            endpoint: p.endpoint,
        });
    }
    // Declared intelligence headers (templates; secret-shaped values validated).
    c.intelligence_headers.extend(cf.intelligence_headers);
    Ok(())
}

fn usage(msg: String) -> ConfigError {
    ConfigError::Usage(format!("agentd: {msg}"))
}

fn truthy(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Parse `600s`, `5m`, `2h`, `500ms`, or a bare integer (seconds).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num, unit): (&str, &str) = match s.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "s"),
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid duration: {s}"))?;
    let d = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        other => return Err(format!("unknown duration unit '{other}' in {s}")),
    };
    Ok(d)
}

/// A unique-enough run id for the default case (time + pid). The operator can
/// override with `--run-id`/`AGENTD_RUN_ID` for idempotent retries (RFC 0011
/// §idempotency). A proper ULID can replace this without changing the surface.
fn generate_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    format!("{millis:011x}{pid:04x}")
}

fn help_text() -> String {
    format!(
        "agentd {ver} — a minimal, MCP-native, reactive agent\n\
         \n\
         USAGE:\n\
         \x20 agentd --instruction <TEXT> --intelligence <URI> [--mcp name=cmd ...] [options]\n\
         \n\
         REQUIRED:\n\
         \x20 --instruction <TEXT>        the task (or INSTRUCTION env)\n\
         \x20 --instruction-file <PATH>   read the instruction from a file\n\
         \x20 --intelligence <URI>        unix:/path | https://host/... | vsock:cid:port\n\
         \n\
         INTELLIGENCE:\n\
         \x20 --intelligence-token <T>    bearer/key (or AGENTD_INTELLIGENCE_TOKEN)\n\
         \x20 --intelligence-token-file <PATH>  read the token from a mounted file (rotation; or AGENTD_INTELLIGENCE_TOKEN_FILE)\n\
         \x20 --model <NAME>              model id (or AGENTD_MODEL)\n\
         \n\
         TOOLS / MCP:\n\
         \x20 --mcp name=command          declare an MCP server (repeatable; stdio)\n\
         \x20 --serve-mcp <TARGET>        serve agentd's own MCP: unix:/path | vsock:PORT | vsock:CID:PORT (vsock needs --features vsock)\n\
         \x20 --a2a-peer name=<ENDPOINT>  declare a remote A2A delegation peer: unix:/path | vsock:CID:PORT (repeatable; needs --features a2a)\n\
         \x20 --enable-exec               expose the gated exec tool\n\
         \x20 --mcp-tags name=t,t         capability tags: untrusted_input|sensitive|egress\n\
         \x20 --allow-trifecta            permit all three capability legs in one agent\n\
         \n\
         MODE / TRIGGERS:\n\
         \x20 --mode once|loop|reactive|schedule   (default once)\n\
         \x20 --subscribe <uri>           subscribe to an MCP resource (repeatable)\n\
         \x20 --continue <uri>            subscribe, routed to one warm session (repeatable)\n\
         \x20 --interval <dur>            loop/schedule interval (e.g. 5m)\n\
         \x20 --cron <5-field>           schedule on a UTC cron expr (needs --features cron)\n\
         \n\
         LIMITS:\n\
         \x20 --max-steps <N>             per-run step cap (default 50)\n\
         \x20 --max-tokens <N>            token budget (default 200000)\n\
         \x20 --deadline <dur>            wall-clock deadline (default 600s)\n\
         \x20 --max-depth <N>             subagent tree depth cap (default 4)\n\
         \n\
         RUNTIME:\n\
         \x20 --run-id <ID>               idempotency key (or AGENTD_RUN_ID)\n\
         \x20 --log-level <L>             trace|debug|info|warn|error (default info)\n\
         \x20 --log-content               log tool args/results, not just lengths (opt-in)\n\
         \x20 --drain-timeout <dur>       graceful drain budget (default 25s; < pod grace)\n\
         \x20 --health-file <PATH>        liveness heartbeat file\n\
         \x20 --metrics-addr <host:port>  serve /metrics+/healthz+/readyz (`:port` = all IPv4 ifaces; needs --features metrics)\n\
         \x20 --cgroup <auto|PATH>        per-run cgroup for atomic cgroup.kill teardown (best-effort)\n\
         \x20 --cgroup-memory-max <SIZE>  per-run memory.max (max|512M|2G|bytes; needs --cgroup + delegation)\n\
         \x20 --cgroup-pids-max <N>       per-run pids.max (max|count of THREADS; needs --cgroup + delegation)\n\
         \x20 --traceparent <W3C>         continue an upstream trace (or AGENTD_TRACEPARENT)\n\
         \x20 --report-file <PATH>        write the run-outcome report at terminal (atomic; inert for reactive)\n\
         \x20 --events-ring <N>           agentd://events ring size (default 1024; needs --serve-mcp + --features events)\n\
         \x20 --capabilities             print the capabilities manifest (JSON) and exit\n\
         \n\
         CONFIG FILE (RFC 0017):\n\
         \x20 --config <PATH>             load a declarative JSON config file (or AGENTD_CONFIG)\n\
         \x20 --validate-config          load+validate (file+env+flags), print the verdict, exit 0/2\n\
         \x20 --config-schema            print the config-file JSON Schema and exit\n\
         \x20 -h, --help / -V, --version\n",
        ver = crate::VERSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_override_env() {
        let env = vec![
            ("AGENTD_INTELLIGENCE".into(), "unix:/run/intel.sock".into()),
            ("INSTRUCTION".into(), "from-env".into()),
        ];
        let c = Config::load(&args(&["--instruction", "from-flag"]), &env).unwrap();
        assert_eq!(c.instruction.as_deref(), Some("from-flag"));
        assert_eq!(c.intelligence.as_deref(), Some("unix:/run/intel.sock"));
    }

    fn base_env() -> Vec<(String, String)> {
        vec![
            ("INSTRUCTION".into(), "x".into()),
            ("AGENTD_INTELLIGENCE".into(), "unix:/x".into()),
        ]
    }

    #[test]
    fn report_file_and_events_ring_parse_from_flag_and_env() {
        // Default: off / the 1024 ring (RFC 0016 §11).
        let c = Config::load(&args(&[]), &base_env()).unwrap();
        assert_eq!(c.report_file, None);
        assert_eq!(c.events_ring, crate::obs::log::EVENTS_RING_DEFAULT);

        // Flags set both.
        let c = Config::load(
            &args(&["--report-file", "/out/report.json", "--events-ring", "256"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.report_file.as_deref(), Some("/out/report.json"));
        assert_eq!(c.events_ring, 256);

        // Env sets both; a flag overrides the ring (precedence: flag > env).
        let mut env = base_env();
        env.push(("AGENTD_REPORT_FILE".into(), "/env/report.json".into()));
        env.push(("AGENTD_EVENTS_RING".into(), "64".into()));
        let c = Config::load(&args(&["--events-ring", "512"]), &env).unwrap();
        assert_eq!(c.report_file.as_deref(), Some("/env/report.json"));
        assert_eq!(c.events_ring, 512);
    }

    #[test]
    fn events_ring_zero_and_bad_value_are_usage_errors() {
        let zero = Config::load(&args(&["--events-ring", "0"]), &base_env()).unwrap_err();
        assert!(matches!(zero, ConfigError::Usage(_)));
        let bad = Config::load(&args(&["--events-ring", "lots"]), &base_env()).unwrap_err();
        assert!(matches!(bad, ConfigError::Usage(_)));
    }

    #[test]
    fn mcp_tags_attach_to_their_server_order_independent() {
        // --mcp-tags before its --mcp still resolves.
        let c = Config::load(
            &args(&["--mcp-tags", "fs=sensitive,egress", "--mcp", "fs=mcp-fs"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(
            c.mcp_servers[0].tags,
            vec![TrifectaTag::Sensitive, TrifectaTag::Egress]
        );
    }

    #[test]
    fn mcp_tags_unknown_server_or_tag_is_usage_error() {
        let bad_server = Config::load(
            &args(&["--mcp", "fs=cmd", "--mcp-tags", "ghost=egress"]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(bad_server, ConfigError::Usage(_)));
        let bad_tag = Config::load(
            &args(&["--mcp", "fs=cmd", "--mcp-tags", "fs=bogus"]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(bad_tag, ConfigError::Usage(_)));
    }

    #[test]
    fn cgroup_limits_require_cgroup_and_reject_zero() {
        // A limit without --cgroup is a misconfiguration (silently unbounded run).
        let e = Config::load(&args(&["--cgroup-memory-max", "512M"]), &base_env()).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
        let e2 = Config::load(&args(&["--cgroup-pids-max", "64"]), &base_env()).unwrap_err();
        assert!(matches!(e2, ConfigError::Usage(_)));
        // With --cgroup, the limits validate.
        let c = Config::load(
            &args(&[
                "--cgroup",
                "auto",
                "--cgroup-memory-max",
                "512M",
                "--cgroup-pids-max",
                "64",
            ]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.cgroup_memory_max.as_deref(), Some("512M"));
        assert_eq!(c.cgroup_pids_max.as_deref(), Some("64"));
        // A zero limit can never let the agent run → rejected.
        let z = Config::load(
            &args(&["--cgroup", "auto", "--cgroup-pids-max", "0"]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(z, ConfigError::Usage(_)));
        let zm = Config::load(
            &args(&["--cgroup", "auto", "--cgroup-memory-max", "0"]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(zm, ConfigError::Usage(_)));
    }

    #[test]
    fn cron_requires_schedule_mode() {
        // --cron with the wrong mode → usage error
        let e = Config::load(
            &args(&[
                "--mode",
                "reactive",
                "--subscribe",
                "x://y",
                "--cron",
                "* * * * *",
            ]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
        // --mode schedule --cron validates (the expr itself is parsed by the cron feature)
        let c = Config::load(
            &args(&["--mode", "schedule", "--cron", "0 9 * * 1-5"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.cron.as_deref(), Some("0 9 * * 1-5"));
        // schedule mode with neither interval nor cron → usage error
        let e2 = Config::load(&args(&["--mode", "schedule"]), &base_env()).unwrap_err();
        assert!(matches!(e2, ConfigError::Usage(_)));
    }

    #[test]
    fn trifecta_grant_tags_defaults_untagged_to_untrusted_and_exec_to_egress() {
        let c = Config::load(&args(&["--mcp", "fs=cmd", "--enable-exec"]), &base_env()).unwrap();
        let tags = c.trifecta_grant_tags();
        assert!(tags.contains(&TrifectaTag::UntrustedInput)); // untagged server
        assert!(tags.contains(&TrifectaTag::Egress)); // --enable-exec
        assert!(!tags.contains(&TrifectaTag::Sensitive)); // two legs → not a trifecta
    }

    #[test]
    fn missing_instruction_is_usage_error() {
        let env = vec![("AGENTD_INTELLIGENCE".into(), "unix:/x".into())];
        let e = Config::load(&[], &env).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    #[test]
    fn help_short_circuits() {
        let e = Config::load(&args(&["--help"]), &[]).unwrap_err();
        assert!(matches!(e, ConfigError::Help(_)));
    }

    #[test]
    fn capabilities_emits_parseable_json_with_no_instruction() {
        // The admission probe must succeed with NO --instruction and NO
        // intelligence — it never reaches run-required validation.
        let e = Config::load(&args(&["--capabilities"]), &[]).unwrap_err();
        let json = match e {
            ConfigError::Capabilities(s) => s,
            other => panic!("expected Capabilities, got {other:?}"),
        };
        let v: serde_json::Value =
            serde_json::from_str(&json).expect("manifest must be valid JSON");
        // It reflects the resolved config (a minted run id is always present).
        assert_eq!(v["contract_version"], serde_json::json!("1.0"));
        assert!(
            v["identity"]["run_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert!(v.get("surfaces").is_some());
    }

    #[test]
    fn capabilities_reflects_present_config() {
        // With config present, the manifest reflects it (no validation needed).
        let c = Config::load(
            &args(&["--capabilities", "--mcp", "fs=cmd", "--enable-exec"]),
            &base_env(),
        );
        let json = match c.unwrap_err() {
            ConfigError::Capabilities(s) => s,
            other => panic!("expected Capabilities, got {other:?}"),
        };
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["exec_enabled"], serde_json::json!(true));
        assert_eq!(v["mcp_servers"][0]["name"], serde_json::json!("fs"));
    }

    #[test]
    fn reactive_requires_subscribe() {
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            ("AGENTD_INTELLIGENCE".into(), "unix:/x".into()),
        ];
        let e = Config::load(&args(&["--mode", "reactive"]), &env).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
        // with a subscription it validates
        let c = Config::load(
            &args(&["--mode", "reactive", "--subscribe", "file://a"]),
            &env,
        )
        .unwrap();
        assert_eq!(c.mode, Mode::Reactive);
    }

    #[test]
    fn mcp_spec_parsing() {
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            ("AGENTD_INTELLIGENCE".into(), "unix:/x".into()),
        ];
        let c = Config::load(&args(&["--mcp", "fs=mcp-server-fs --root /data"]), &env).unwrap();
        assert_eq!(c.mcp_servers.len(), 1);
        assert_eq!(c.mcp_servers[0].name, "fs");
        assert_eq!(
            c.mcp_servers[0].command,
            vec!["mcp-server-fs", "--root", "/data"]
        );
    }

    #[test]
    fn duration_units() {
        assert_eq!(parse_duration("600s").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert!(parse_duration("nope").is_err());
    }

    #[test]
    fn invalid_intelligence_uri_rejected() {
        let env = vec![("INSTRUCTION".into(), "x".into())];
        let e = Config::load(&args(&["--intelligence", "ftp://x"]), &env).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    #[test]
    fn serve_target_unix_parses() {
        assert_eq!(
            ServeTarget::parse("unix:/run/agentd.sock").unwrap(),
            ServeTarget::Unix("/run/agentd.sock".into())
        );
        // empty path → usage error
        assert!(matches!(
            ServeTarget::parse("unix:"),
            Err(ConfigError::Usage(_))
        ));
        // a bare/foreign scheme → usage error
        assert!(matches!(
            ServeTarget::parse("tcp:1234"),
            Err(ConfigError::Usage(_))
        ));
    }

    #[cfg(feature = "vsock")]
    #[test]
    fn serve_target_vsock_parses_on_vsock_build() {
        // vsock:PORT → wildcard cid (VMADDR_CID_ANY)
        assert_eq!(
            ServeTarget::parse("vsock:5005").unwrap(),
            ServeTarget::Vsock {
                cid: VMADDR_CID_ANY,
                port: 5005
            }
        );
        // vsock:CID:PORT → that cid
        assert_eq!(
            ServeTarget::parse("vsock:2:5005").unwrap(),
            ServeTarget::Vsock { cid: 2, port: 5005 }
        );
        // port 0 / non-numeric port / non-numeric cid → usage error
        for bad in ["vsock:0", "vsock:2:0", "vsock:abc", "vsock:x:5005"] {
            assert!(
                matches!(ServeTarget::parse(bad), Err(ConfigError::Usage(_))),
                "{bad} must be a usage error"
            );
        }
    }

    #[cfg(not(feature = "vsock"))]
    #[test]
    fn serve_target_vsock_rejected_without_feature() {
        let e = ServeTarget::parse("vsock:5005").unwrap_err();
        match e {
            ConfigError::Usage(msg) => assert!(
                msg.contains("vsock requires the 'vsock' build feature"),
                "got: {msg}"
            ),
            _ => panic!("expected a Usage error"),
        }
    }

    #[test]
    fn serve_mcp_validation_runs_at_load() {
        // unix: still parses through full load() exactly as before.
        let c = Config::load(&args(&["--serve-mcp", "unix:/tmp/a.sock"]), &base_env()).unwrap();
        assert_eq!(c.serve_mcp.as_deref(), Some("unix:/tmp/a.sock"));
        // a foreign scheme is rejected at load (exit 2) before any side effect.
        let e = Config::load(&args(&["--serve-mcp", "tcp:9000"]), &base_env()).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    #[test]
    fn a2a_peer_spec_parses_name_and_endpoint() {
        // The endpoint is the remainder after the first '=', so the unix:/vsock:
        // scheme passes through verbatim (no second '=' to confuse the split).
        let spec = parse_a2a_peer_spec("mesh=unix:/run/peer.sock").unwrap();
        assert_eq!(spec.name, "mesh");
        assert_eq!(spec.endpoint, "unix:/run/peer.sock");
        // Missing '=' / empty halves are usage errors.
        assert!(matches!(
            parse_a2a_peer_spec("noequals"),
            Err(ConfigError::Usage(_))
        ));
        assert!(matches!(
            parse_a2a_peer_spec("=unix:/x"),
            Err(ConfigError::Usage(_))
        ));
        assert!(matches!(
            parse_a2a_peer_spec("mesh="),
            Err(ConfigError::Usage(_))
        ));
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn a2a_peer_flag_parses_and_validates_on_a2a_build() {
        // A valid unix peer loads through full validation.
        let c = Config::load(
            &args(&["--a2a-peer", "mesh=unix:/run/peer.sock"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.a2a_peers.len(), 1);
        assert_eq!(c.a2a_peers[0].name, "mesh");
        assert_eq!(c.a2a_peers[0].endpoint, "unix:/run/peer.sock");

        // A bad endpoint scheme is rejected at load (exit 2) before any side effect.
        let bad = Config::load(&args(&["--a2a-peer", "mesh=tcp:9000"]), &base_env()).unwrap_err();
        assert!(matches!(bad, ConfigError::Usage(_)));

        // An empty unix path is a usage error too.
        let empty = Config::load(&args(&["--a2a-peer", "mesh=unix:"]), &base_env()).unwrap_err();
        assert!(matches!(empty, ConfigError::Usage(_)));

        // A duplicate peer name is rejected.
        let dup = Config::load(
            &args(&[
                "--a2a-peer",
                "mesh=unix:/a.sock",
                "--a2a-peer",
                "mesh=unix:/b.sock",
            ]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(dup, ConfigError::Usage(_)));
    }

    #[cfg(not(feature = "a2a"))]
    #[test]
    fn a2a_peer_requires_the_a2a_feature() {
        // The flag parses, but validation rejects it without the build feature.
        let e = Config::load(
            &args(&["--a2a-peer", "mesh=unix:/run/peer.sock"]),
            &base_env(),
        )
        .unwrap_err();
        match e {
            ConfigError::Usage(msg) => assert!(
                msg.contains("--a2a-peer requires the 'a2a' build feature"),
                "got: {msg}"
            ),
            other => panic!("expected a Usage error, got {other:?}"),
        }
    }

    #[cfg(all(feature = "a2a", feature = "vsock"))]
    #[test]
    fn a2a_peer_vsock_endpoint_requires_cid_and_port() {
        // vsock:CID:PORT parses; the wildcard/bare forms do not (a client dials a
        // concrete peer).
        let c = Config::load(&args(&["--a2a-peer", "g=vsock:2:5005"]), &base_env()).unwrap();
        assert_eq!(c.a2a_peers[0].endpoint, "vsock:2:5005");
        for bad in ["g=vsock:5005", "g=vsock:2:0", "g=vsock:x:5005"] {
            assert!(
                matches!(
                    Config::load(&args(&["--a2a-peer", bad]), &base_env()),
                    Err(ConfigError::Usage(_))
                ),
                "{bad} must be a usage error"
            );
        }
    }

    #[test]
    fn token_redacted_in_debug() {
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            (
                "AGENTD_INTELLIGENCE".into(),
                "https://api.example/v1".into(),
            ),
            ("AGENTD_INTELLIGENCE_TOKEN".into(), "super-secret".into()),
        ];
        let c = Config::load(&[], &env).unwrap();
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("super-secret"));
        assert!(dbg.contains("***"));
    }

    // ───────────────────────── RFC 0017 — config file ─────────────────────────

    use std::io::Write as _;

    fn write_tmp(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn config_file_loads_mcp_subscribe_a2a_and_limits() {
        let file = write_tmp(
            r#"{
                "model": "claude-from-file",
                "max_tokens": 1234567,
                "limits": { "max_steps": 77, "max_depth": 3, "deadline_secs": 120 },
                "mcp_servers": [
                    { "name": "web", "command": "mcp-fetch", "argv": ["--timeout", "30"],
                      "tags": { "*": ["untrusted_input"] } }
                ],
                "subscribe": ["fs:file:///watch/inbox"]
            }"#,
        );
        let c = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.model.as_deref(), Some("claude-from-file"));
        assert_eq!(c.max_tokens, 1_234_567);
        assert_eq!(c.max_steps, 77);
        assert_eq!(c.max_depth, 3);
        assert_eq!(c.deadline, Some(Duration::from_secs(120)));
        assert_eq!(c.mcp_servers.len(), 1);
        assert_eq!(c.mcp_servers[0].name, "web");
        assert_eq!(
            c.mcp_servers[0].command,
            vec!["mcp-fetch", "--timeout", "30"]
        );
        assert_eq!(c.mcp_servers[0].tags, vec![TrifectaTag::UntrustedInput]);
        assert_eq!(c.subscribe, vec!["fs:file:///watch/inbox"]);
    }

    #[test]
    fn env_and_flag_override_file_per_precedence() {
        // built-in < FILE < env < flag (RFC 0011 §2.1 / RFC 0017 §3.2).
        let file = write_tmp(r#"{ "model": "from-file", "max_tokens": 100 }"#);
        let mut env = base_env();
        env.push(("AGENTD_MODEL".into(), "from-env".into()));
        // env beats file; a flag beats env.
        let c = Config::load(
            &args(&[
                "--config",
                file.path().to_str().unwrap(),
                "--max-tokens",
                "999",
            ]),
            &env,
        )
        .unwrap();
        assert_eq!(c.model.as_deref(), Some("from-env")); // env > file
        assert_eq!(c.max_tokens, 999); // flag > file
        // Without the env/flag, the file value stands.
        let c2 = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c2.model.as_deref(), Some("from-file"));
        assert_eq!(c2.max_tokens, 100);
    }

    #[test]
    fn flag_mcp_and_subscribe_add_to_the_file_list() {
        // Repeatable list flags ADD to the file's lists (the one documented
        // deviation from pure last-writer-wins, RFC 0017 §3.2).
        let file = write_tmp(
            r#"{ "mcp_servers": [{ "name": "web", "command": "mcp-fetch" }],
                "subscribe": ["fs:file:///a"] }"#,
        );
        let c = Config::load(
            &args(&[
                "--config",
                file.path().to_str().unwrap(),
                "--mcp",
                "fs=mcp-fs",
                "--subscribe",
                "fs:file:///b",
            ]),
            &base_env(),
        )
        .unwrap();
        let names: Vec<&str> = c.mcp_servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["web", "fs"]); // file seeds, flag adds
        assert_eq!(c.subscribe, vec!["fs:file:///a", "fs:file:///b"]);
    }

    #[test]
    fn config_via_env_alias() {
        let file = write_tmp(r#"{ "model": "env-config" }"#);
        let mut env = base_env();
        env.push(("AGENTD_CONFIG".into(), file.path().to_str().unwrap().into()));
        let c = Config::load(&args(&[]), &env).unwrap();
        assert_eq!(c.model.as_deref(), Some("env-config"));
    }

    #[test]
    fn malformed_config_file_is_usage_error() {
        let file = write_tmp("{ this is not json ");
        let e = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    #[test]
    fn unreadable_config_file_is_usage_error() {
        let e =
            Config::load(&args(&["--config", "/no/such/config.json"]), &base_env()).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    #[test]
    fn config_file_unknown_key_is_usage_error() {
        // deny_unknown_fields: a typo'd key fails at parse (exit 2).
        let file = write_tmp(r#"{ "max_token": 5 }"#);
        let e = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    // ───────────────────────── RFC 0017 — --validate-config ───────────────────

    fn validate_verdict(args_: &[&str], env: &[(String, String)]) -> Result<String, String> {
        match Config::load(&args(args_), env).unwrap_err() {
            ConfigError::Validate(v) => v,
            other => panic!("expected Validate, got {other:?}"),
        }
    }

    #[test]
    fn validate_config_valid_returns_ok_with_no_instruction_needed() {
        // It validates whatever is given; a complete config returns the
        // config.valid verdict. (Here instruction+intelligence are present.)
        let v = validate_verdict(&["--validate-config"], &base_env());
        let line = v.expect("a complete config validates");
        assert!(line.contains("config.valid"));
        let _: serde_json::Value = serde_json::from_str(&line).unwrap();
    }

    #[test]
    fn validate_config_invalid_returns_err_exit2_shape() {
        // reactive with no subscribe → invalid (RFC 0011 §3.3). Verdict is Err.
        let v = validate_verdict(&["--validate-config", "--mode", "reactive"], &base_env());
        let lines = v.unwrap_err();
        assert!(lines.contains("config.invalid"));
        // Each line is parseable NDJSON.
        for line in lines.lines() {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn validate_config_runs_without_an_instruction() {
        // No INSTRUCTION at all: --validate-config still produces a verdict (it
        // does not need an instruction to *run*); the missing-instruction shows
        // up as an invalid diagnostic, not a crash.
        let env = vec![("AGENTD_INTELLIGENCE".into(), "unix:/x".into())];
        let v = match Config::load(&args(&["--validate-config"]), &env).unwrap_err() {
            ConfigError::Validate(v) => v,
            other => panic!("expected Validate, got {other:?}"),
        };
        let lines = v.unwrap_err();
        assert!(lines.contains("config.invalid"));
        assert!(lines.contains("instruction"));
    }

    #[test]
    fn validate_config_rejects_bad_intelligence_scheme() {
        let mut env = base_env();
        env.retain(|(k, _)| k != "AGENTD_INTELLIGENCE");
        let v = validate_verdict(&["--validate-config", "--intelligence", "ftp://nope"], &env);
        assert!(v.unwrap_err().contains("config.invalid"));
    }

    // ───────────────────────── RFC 0017 — --config-schema ─────────────────────

    #[test]
    fn config_schema_emits_parseable_json_schema() {
        let s = match Config::load(&args(&["--config-schema"]), &[]).unwrap_err() {
            ConfigError::Schema(s) => s,
            other => panic!("expected Schema, got {other:?}"),
        };
        let v: serde_json::Value = serde_json::from_str(&s).expect("schema is valid JSON");
        assert_eq!(
            v["$schema"],
            serde_json::json!("https://json-schema.org/draft/2020-12/schema")
        );
        assert!(v["properties"].is_object());
        // It short-circuits with NO instruction and NO config (static export).
    }

    // ───────────────────────── RFC 0017 — secret refs (§6) ────────────────────

    #[test]
    fn intelligence_token_file_reads_and_trims() {
        let tok = write_tmp("file-token\n");
        let mut env = base_env();
        env.push((
            "AGENTD_INTELLIGENCE_TOKEN_FILE".into(),
            tok.path().to_str().unwrap().into(),
        ));
        let c = Config::load(&args(&[]), &env).unwrap();
        assert_eq!(c.intelligence_token.as_deref(), Some("file-token"));
        // The token never appears in the redacted Debug.
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("file-token"));
        assert!(dbg.contains("***"));
    }

    #[test]
    fn inline_token_wins_over_token_file() {
        let tok = write_tmp("from-file\n");
        let mut env = base_env();
        env.push(("AGENTD_INTELLIGENCE_TOKEN".into(), "from-inline".into()));
        env.push((
            "AGENTD_INTELLIGENCE_TOKEN_FILE".into(),
            tok.path().to_str().unwrap().into(),
        ));
        let c = Config::load(&args(&[]), &env).unwrap();
        assert_eq!(c.intelligence_token.as_deref(), Some("from-inline"));
    }

    #[test]
    fn token_file_flag_reads_via_cli() {
        let tok = write_tmp("flag-token");
        let c = Config::load(
            &args(&["--intelligence-token-file", tok.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.intelligence_token.as_deref(), Some("flag-token"));
    }

    #[test]
    fn missing_token_file_is_usage_error() {
        let mut env = base_env();
        env.push((
            "AGENTD_INTELLIGENCE_TOKEN_FILE".into(),
            "/no/such/token".into(),
        ));
        let e = Config::load(&args(&[]), &env).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    #[test]
    fn secret_file_ref_resolves_and_does_not_leak() {
        // A declared header with a {{secret-file:PATH}} ref validates (the file
        // exists) and the resolved secret never enters the manifest or the
        // redacted Debug — only the structural ref/name does.
        let secret = write_tmp("RESOLVED-SECRET-VALUE\n");
        let path = secret.path().to_str().unwrap().to_string();
        let file = write_tmp(&format!(
            r#"{{ "intelligence_headers": {{
                "authorization": "Bearer {{{{secret-file:{path}}}}}" }} }}"#
        ));
        let c = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        // The header TEMPLATE (the ref) is structural config and is stored…
        assert_eq!(
            c.intelligence_headers
                .get("authorization")
                .map(String::as_str),
            Some(format!("Bearer {{{{secret-file:{path}}}}}").as_str())
        );
        // …but the resolved secret value is NOT stored or logged.
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("RESOLVED-SECRET-VALUE"));
        // The resolver materializes it only at the moment of use.
        let env = |_: &str| None;
        let resolved =
            crate::sec::secret::resolve(c.intelligence_headers.get("authorization").unwrap(), &env)
                .unwrap();
        assert_eq!(resolved, "Bearer RESOLVED-SECRET-VALUE");
    }

    #[test]
    fn inline_secret_shaped_header_is_rejected() {
        // A credential-shaped header with an inline (non-ref) value is the
        // "secret in the file" footgun — exit 2 (RFC 0017 §3.1).
        let file = write_tmp(r#"{ "intelligence_headers": { "x-api-key": "sk-inline-literal" } }"#);
        let e = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
        // A {{secret:NAME}} ref in the same header is fine (a reference, not a
        // value). The ref resolves against the PROCESS env at startup (the runtime
        // truth), so set the real var for this check.
        // SAFETY: single-threaded test; the var is unique to this test.
        unsafe {
            std::env::set_var("AGENTD_TEST_HDR_KEY_0017", "k");
        }
        let file_ok = write_tmp(
            r#"{ "intelligence_headers": { "x-api-key": "{{secret:AGENTD_TEST_HDR_KEY_0017}}" } }"#,
        );
        let c = Config::load(
            &args(&["--config", file_ok.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        assert!(c.intelligence_headers.contains_key("x-api-key"));
        unsafe {
            std::env::remove_var("AGENTD_TEST_HDR_KEY_0017");
        }
    }

    #[test]
    fn unresolvable_secret_ref_in_header_is_rejected_at_validation() {
        // A {{secret:NAME}} whose env var is unset → exit 2 at startup (§6.2).
        let file = write_tmp(
            r#"{ "intelligence_headers": { "x-api-key": "{{secret:DEFINITELY_UNSET_VAR_XYZ}}" } }"#,
        );
        let e = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }
}
