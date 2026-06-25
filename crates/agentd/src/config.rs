//! Configuration: precedence + validate-at-startup. RFC 0011 §2-§3.
//!
//! Precedence, top wins: `built-in default < config file < env var < CLI
//! flag`. Everything is env-settable (12-factor); the file is only for
//! verbose structural bits (MCP server lists) and **never for secrets**
//! (env/flag only). The whole config is validated **before any side effect**
//! — a bad config exits `2` in milliseconds, not after an LLM round-trip.
//!
//! (M1 implements `default < env < flag`; the optional config-file layer is a
//! later milestone and slots between default and env. Flag/env names are the
//! stable surface.)

use crate::obs::log::Level;
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

/// A declared MCP server: a name and the argv to spawn it (stdio transport).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    pub name: String,
    pub command: Vec<String>,
}

/// The fully-resolved, validated configuration.
#[derive(Clone, PartialEq)]
pub struct Config {
    pub instruction: Option<String>,
    pub intelligence: Option<String>,
    pub intelligence_token: Option<String>,
    pub model: Option<String>,
    pub mcp_servers: Vec<McpServerSpec>,
    pub mode: Mode,
    pub subscribe: Vec<String>,
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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            instruction: None,
            intelligence: None,
            intelligence_token: None,
            model: None,
            mcp_servers: Vec::new(),
            mode: Mode::Once,
            subscribe: Vec::new(),
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
        }
    }
}

// Redact the credential — never let it reach a log or a panic message.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("instruction", &self.instruction.as_deref().map(|_| "<set>"))
            .field("intelligence", &self.intelligence)
            .field("intelligence_token", &self.intelligence_token.as_ref().map(|_| "***"))
            .field("model", &self.model)
            .field("mcp_servers", &self.mcp_servers)
            .field("mode", &self.mode)
            .field("subscribe", &self.subscribe)
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
            .finish()
    }
}

/// What `load()` can short-circuit with. `Help`/`Version` are *not* errors
/// (exit 0); `Usage` is a validation/parse failure (exit 2, RFC 0011 §5).
#[derive(Debug)]
pub enum ConfigError {
    Help(String),
    Version(String),
    Usage(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Help(s) | ConfigError::Version(s) => write!(f, "{s}"),
            ConfigError::Usage(s) => write!(f, "{s}"),
        }
    }
}

impl Config {
    /// Resolve config from CLI args (excluding argv[0]) and the environment,
    /// applying precedence (env then flags over defaults) and validating.
    pub fn load(args: &[String], env: &[(String, String)]) -> Result<Config, ConfigError> {
        let envmap: HashMap<&str, &str> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut c = Config::default();

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
        if let Some(v) = envmap.get("AGENTD_MODEL") {
            c.model = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_MODE") {
            c.mode = Mode::parse(v).ok_or_else(|| usage(format!("invalid AGENTD_MODE: {v}")))?;
        }
        if let Some(v) = envmap.get("AGENTD_MAX_STEPS") {
            c.max_steps = v.parse().map_err(|_| usage(format!("invalid AGENTD_MAX_STEPS: {v}")))?;
        }
        if let Some(v) = envmap.get("AGENTD_MAX_TOKENS") {
            c.max_tokens =
                v.parse().map_err(|_| usage(format!("invalid AGENTD_MAX_TOKENS: {v}")))?;
        }
        if let Some(v) = envmap.get("AGENTD_DEADLINE") {
            c.deadline = Some(parse_duration(v).map_err(usage)?);
        }
        if let Some(v) = envmap.get("AGENTD_RUN_ID") {
            c.run_id = (*v).to_string();
        }
        if let Some(v) = envmap.get("AGENTD_LOG_LEVEL") {
            c.log_level = Level::parse(v).ok_or_else(|| usage(format!("invalid AGENTD_LOG_LEVEL: {v}")))?;
        }
        if let Some(v) = envmap.get("AGENTD_DRAIN_TIMEOUT") {
            c.drain_timeout = parse_duration(v).map_err(usage)?;
        }
        if let Some(v) = envmap.get("AGENTD_ENABLE_EXEC") {
            c.enable_exec = truthy(v);
        }
        if let Some(v) = envmap.get("AGENTD_SERVE_MCP") {
            c.serve_mcp = Some((*v).to_string());
        }

        // --- flag layer (overrides env) ---
        let mut it = args.iter().peekable();
        while let Some(arg) = it.next() {
            let mut take = |name: &str| -> Result<String, ConfigError> {
                it.next().cloned().ok_or_else(|| usage(format!("{name} requires a value")))
            };
            match arg.as_str() {
                "-h" | "--help" => return Err(ConfigError::Help(help_text())),
                "-V" | "--version" => {
                    return Err(ConfigError::Version(format!("agentd {}\n", crate::VERSION)));
                }
                "--instruction" => c.instruction = Some(take("--instruction")?),
                "--instruction-file" => {
                    let p = take("--instruction-file")?;
                    c.instruction = Some(read_file(&p)?);
                }
                "--intelligence" => c.intelligence = Some(take("--intelligence")?),
                "--intelligence-token" => c.intelligence_token = Some(take("--intelligence-token")?),
                "--model" => c.model = Some(take("--model")?),
                "--mcp" => {
                    let spec = take("--mcp")?;
                    c.mcp_servers.push(parse_mcp_spec(&spec)?);
                }
                "--mode" => {
                    let v = take("--mode")?;
                    c.mode = Mode::parse(&v).ok_or_else(|| usage(format!("invalid --mode: {v}")))?;
                }
                "--subscribe" => c.subscribe.push(take("--subscribe")?),
                "--interval" => c.interval = Some(parse_duration(&take("--interval")?).map_err(usage)?),
                "--max-steps" => {
                    let v = take("--max-steps")?;
                    c.max_steps = v.parse().map_err(|_| usage(format!("invalid --max-steps: {v}")))?;
                }
                "--max-tokens" => {
                    let v = take("--max-tokens")?;
                    c.max_tokens = v.parse().map_err(|_| usage(format!("invalid --max-tokens: {v}")))?;
                }
                "--deadline" => c.deadline = Some(parse_duration(&take("--deadline")?).map_err(usage)?),
                "--max-depth" => {
                    let v = take("--max-depth")?;
                    c.max_depth = v.parse().map_err(|_| usage(format!("invalid --max-depth: {v}")))?;
                }
                "--run-id" => c.run_id = take("--run-id")?,
                "--log-level" => {
                    let v = take("--log-level")?;
                    c.log_level = Level::parse(&v).ok_or_else(|| usage(format!("invalid --log-level: {v}")))?;
                }
                "--drain-timeout" => c.drain_timeout = parse_duration(&take("--drain-timeout")?).map_err(usage)?,
                "--enable-exec" => c.enable_exec = true,
                "--serve-mcp" => c.serve_mcp = Some(take("--serve-mcp")?),
                "--health-file" => c.health_file = Some(take("--health-file")?),
                other => return Err(usage(format!("unknown argument: {other}"))),
            }
        }

        if c.run_id.is_empty() {
            c.run_id = generate_run_id();
        }
        c.validate()?;
        Ok(c)
    }

    /// Reject inconsistent config before any side effect (RFC 0011 §2).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.instruction.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Err(usage("missing instruction (INSTRUCTION env or --instruction)".into()));
        }
        if self.intelligence.as_deref().unwrap_or("").is_empty() {
            return Err(usage("missing intelligence endpoint (AGENTD_INTELLIGENCE or --intelligence)".into()));
        }
        validate_intelligence_uri(self.intelligence.as_deref().unwrap())?;
        for s in &self.mcp_servers {
            if s.name.is_empty() || s.command.is_empty() {
                return Err(usage(format!("mcp server '{}' has empty name or command", s.name)));
            }
        }
        if self.max_steps == 0 {
            return Err(usage("--max-steps must be > 0".into()));
        }
        if self.mode == Mode::Reactive && self.subscribe.is_empty() {
            return Err(usage("--mode reactive requires at least one --subscribe <uri>".into()));
        }
        if self.mode == Mode::Schedule && self.interval.is_none() {
            return Err(usage("--mode schedule requires --interval <dur>".into()));
        }
        Ok(())
    }
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

/// Parse `--mcp name=cmd arg arg`. The command is whitespace-split into argv
/// (M1 simplification; the config-file layer carries argv arrays verbatim).
fn parse_mcp_spec(spec: &str) -> Result<McpServerSpec, ConfigError> {
    let (name, cmd) = spec
        .split_once('=')
        .ok_or_else(|| usage(format!("--mcp must be name=command (got: {spec})")))?;
    let command: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
    if name.is_empty() || command.is_empty() {
        return Err(usage(format!("--mcp '{spec}' has empty name or command")));
    }
    Ok(McpServerSpec { name: name.to_string(), command })
}

fn read_file(path: &str) -> Result<String, ConfigError> {
    std::fs::read_to_string(path)
        .map_err(|e| usage(format!("cannot read instruction file {path}: {e}")))
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
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
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
         \x20 --model <NAME>              model id (or AGENTD_MODEL)\n\
         \n\
         TOOLS / MCP:\n\
         \x20 --mcp name=command          declare an MCP server (repeatable; stdio)\n\
         \x20 --serve-mcp <unix:/path>    serve agentd's own MCP\n\
         \x20 --enable-exec               expose the gated exec tool\n\
         \n\
         MODE / TRIGGERS:\n\
         \x20 --mode once|loop|reactive|schedule   (default once)\n\
         \x20 --subscribe <uri>           subscribe to an MCP resource (repeatable)\n\
         \x20 --interval <dur>            loop/schedule interval (e.g. 5m)\n\
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
         \x20 --drain-timeout <dur>       graceful drain budget (default 25s; < pod grace)\n\
         \x20 --health-file <PATH>        liveness heartbeat file\n\
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
    fn reactive_requires_subscribe() {
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            ("AGENTD_INTELLIGENCE".into(), "unix:/x".into()),
        ];
        let e = Config::load(&args(&["--mode", "reactive"]), &env).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
        // with a subscription it validates
        let c = Config::load(&args(&["--mode", "reactive", "--subscribe", "file://a"]), &env).unwrap();
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
        assert_eq!(c.mcp_servers[0].command, vec!["mcp-server-fs", "--root", "/data"]);
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
    fn token_redacted_in_debug() {
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            ("AGENTD_INTELLIGENCE".into(), "https://api.example/v1".into()),
            ("AGENTD_INTELLIGENCE_TOKEN".into(), "super-secret".into()),
        ];
        let c = Config::load(&[], &env).unwrap();
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("super-secret"));
        assert!(dbg.contains("***"));
    }
}
