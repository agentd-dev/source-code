//! Pluggable secret sources (`[[secrets]]`) — a drop-in superset of
//! environment-variable indirection.
//!
//! Every secret-consuming field in agentd (`api_key_env`,
//! `tokens_env`, `credentials_env`, `secret_env`, MCP child `env`,
//! `{{secret:NAME}}` header placeholders) names a secret and resolves
//! it through ONE function: [`resolve`]. Resolution order:
//!
//! 1. A `[[secrets]]` entry with that name → its declared source.
//! 2. Otherwise → the process environment (today's behaviour, so
//!    existing workflows keep working unchanged).
//!
//! That single path is the reusability contract: declaring a source
//! for a name upgrades **every** consumer at once — an OAuth2 token
//! works as an LLM backend key, an outbound `Authorization` header, a
//! bearer-auth token set, or an MCP server's `DATABASE_URL`, with no
//! consumer-side changes.
//!
//! Sources:
//!
//! | `source` | Behaviour | Feature |
//! |---|---|---|
//! | `env`     | live read of another env var (aliasing) | always |
//! | `file`    | live read per resolve — rotation by replacing the file (k8s `Secret` mounts, Vault Agent sidecars, SOPS output) | always |
//! | `command` | argv-declared exec, stdout trimmed, cached until reload | `secrets-exec` |
//! | `oauth2`  | client-credentials grant, cached until `expires_in − skew` | `secrets-oauth2` |
//!
//! Invariants (the point of the module):
//!
//! - Secret values are **never serialised**: no `Serialize`, `Debug`
//!   prints `***`, values live only in process memory.
//! - The `read_env` node and the agent-loop `read_env` tool do NOT
//!   consult the registry — a registry secret can never flow into the
//!   execution context, node outputs, or run records.
//! - Workflow TOML carries source *references* (env var names, file
//!   paths, endpoints) — never secret material. `deny_unknown_fields`
//!   keeps a literal `client_secret = "..."` from even parsing.

use std::collections::HashMap;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[cfg(feature = "secrets-oauth2")]
pub mod oauth2;

// ---------------------------------------------------------------------------
// Config shapes (`[[secrets]]`)
// ---------------------------------------------------------------------------

/// One `[[secrets]]` entry: a name and where its value comes from.
///
/// Deserialization is manual: serde's `deny_unknown_fields` is
/// silently ineffective under `flatten`/internal tags, and "a literal
/// `client_secret = ...` in the TOML is a parse error" is a security
/// property of this module, not a nicety. The custom impl checks every
/// field against the chosen source's allowed set.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SecretDef {
    /// The name consumers reference (`api_key_env = "<name>"`,
    /// `{{secret:<name>}}`, …). Shadows an identically-named env var.
    pub name: String,
    #[serde(flatten)]
    pub source: SourceDef,
}

impl<'de> Deserialize<'de> for SecretDef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as DeError;
        let mut map = serde_json::Map::deserialize(deserializer)?;
        let name = map
            .remove("name")
            .and_then(|v| v.as_str().map(str::to_string))
            .ok_or_else(|| DeError::custom("[[secrets]] entry needs a string `name`"))?;
        let tag = map
            .get("source")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                DeError::custom(format!(
                    "secret `{name}`: needs `source = \"env\" | \"file\" | \"command\" | \"oauth2\"`"
                ))
            })?;
        let allowed: &[&str] = match tag.as_str() {
            "env" => &["source", "var"],
            "file" => &["source", "path", "trim"],
            "command" => &["source", "argv"],
            "oauth2" => &[
                "source",
                "token_url",
                "client_id_env",
                "client_secret_env",
                "scopes",
                "extra_params",
                "auth_style",
                "skew_secs",
            ],
            other => {
                return Err(DeError::custom(format!(
                    "secret `{name}`: unknown source `{other}` (expected env / file / command / oauth2)"
                )));
            }
        };
        for key in map.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(DeError::custom(format!(
                    "secret `{name}`: unknown field `{key}` for source `{tag}` — \
                     secret material never goes in the workflow file; allowed: {}",
                    allowed.join(", ")
                )));
            }
        }
        let source: SourceDef = serde_json::from_value(serde_json::Value::Object(map))
            .map_err(|e| DeError::custom(format!("secret `{name}`: {e}")))?;
        Ok(SecretDef { name, source })
    }
}

/// Where a secret's value comes from. Tagged by `source = "..."`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SourceDef {
    /// Alias another environment variable (live read).
    Env { var: String },
    /// Read a file (live, per resolve — rotation just works). The
    /// value is trimmed of trailing whitespace unless `trim = false`.
    File {
        path: String,
        #[serde(default = "default_true")]
        trim: bool,
    },
    /// Run an argv-declared command; stdout (trimmed) is the value.
    /// Cached after the first resolve until the registry is rebuilt
    /// (startup / SIGHUP). Feature `secrets-exec`.
    Command { argv: Vec<String> },
    /// OAuth2 client-credentials grant against `token_url`; the
    /// access token is cached until `expires_in − skew`. The client
    /// credentials themselves resolve through this registry (so they
    /// can come from env, a file, or a command). Feature
    /// `secrets-oauth2`.
    Oauth2 {
        token_url: String,
        client_id_env: String,
        client_secret_env: String,
        #[serde(default)]
        scopes: Vec<String>,
        /// Extra form parameters some providers require (e.g. Auth0's
        /// `audience`). Values are literals — never secret material.
        #[serde(default)]
        extra_params: HashMap<String, String>,
        /// `body` (credentials in the form, the common case) or
        /// `basic` (RFC 6749 §2.3.1 Authorization header).
        #[serde(default)]
        auth_style: Option<String>,
        /// Refresh this many seconds before expiry. Default 60.
        #[serde(default)]
        skew_secs: Option<u64>,
    },
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretError {
    pub name: String,
    pub reason: String,
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "secret `{}`: {}", self.name, self.reason)
    }
}

fn err(name: &str, reason: impl Into<String>) -> SecretError {
    SecretError {
        name: name.to_string(),
        reason: reason.into(),
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Cached value for sources that don't re-read per resolve.
#[derive(Clone)]
enum Cached {
    Value(String),
    #[cfg(feature = "secrets-oauth2")]
    Token(oauth2::CachedToken),
}

// Deliberately NOT Serialize; Debug never prints material.
impl std::fmt::Debug for Cached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// The installed secret sources. Build from a workflow's `[[secrets]]`
/// at startup (and on SIGHUP); consumers go through [`resolve`].
#[derive(Default)]
pub struct SecretsRegistry {
    defs: HashMap<String, SourceDef>,
    cache: std::sync::Mutex<HashMap<String, Cached>>,
}

impl std::fmt::Debug for SecretsRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Names are operator-facing; values never appear.
        f.debug_struct("SecretsRegistry")
            .field("names", &self.defs.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SecretsRegistry {
    /// Build from `[[secrets]]` entries. Rejects duplicate names and
    /// sources this build can't serve (feature-gated), and fail-fasts
    /// each entry once so a bad source surfaces at startup, not at
    /// first request.
    pub fn build(defs: &[SecretDef]) -> Result<Self, SecretError> {
        let mut map = HashMap::new();
        for def in defs {
            if map.insert(def.name.clone(), def.source.clone()).is_some() {
                return Err(err(&def.name, "declared more than once"));
            }
            feature_check(&def.name, &def.source)?;
        }
        let reg = Self {
            defs: map,
            cache: std::sync::Mutex::new(HashMap::new()),
        };
        // Startup probe: a named-but-unresolvable secret is a startup
        // error (the same contract api_key_env always had).
        for def in defs {
            reg.resolve(&def.name)?;
        }
        Ok(reg)
    }

    /// Resolve a name through this registry: declared source first,
    /// process environment as the fallback.
    pub fn resolve(&self, name: &str) -> Result<String, SecretError> {
        let Some(source) = self.defs.get(name) else {
            // Fallback: the environment — the pre-`[[secrets]]`
            // behaviour every existing workflow relies on.
            return match std::env::var(name) {
                Ok(v) if !v.trim().is_empty() => Ok(v),
                _ => Err(err(
                    name,
                    "not declared in [[secrets]] and the env var is unset or empty",
                )),
            };
        };
        match source {
            SourceDef::Env { var } => match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => Ok(v),
                _ => Err(err(name, format!("env var `{var}` is unset or empty"))),
            },
            SourceDef::File { path, trim } => {
                let raw = std::fs::read_to_string(path)
                    .map_err(|e| err(name, format!("read {path}: {e}")))?;
                let v = if *trim {
                    raw.trim_end().to_string()
                } else {
                    raw
                };
                if v.is_empty() {
                    return Err(err(name, format!("file {path} is empty")));
                }
                Ok(v)
            }
            SourceDef::Command { argv } => self.resolve_command(name, argv),
            #[allow(unused_variables)]
            SourceDef::Oauth2 {
                token_url,
                client_id_env,
                client_secret_env,
                scopes,
                extra_params,
                auth_style,
                skew_secs,
            } => {
                #[cfg(feature = "secrets-oauth2")]
                {
                    self.resolve_oauth2(
                        name,
                        token_url,
                        client_id_env,
                        client_secret_env,
                        scopes,
                        extra_params,
                        auth_style.as_deref(),
                        skew_secs.unwrap_or(60),
                    )
                }
                #[cfg(not(feature = "secrets-oauth2"))]
                Err(err(
                    name,
                    "source `oauth2` needs the `secrets-oauth2` feature; rebuild with --features secrets-oauth2",
                ))
            }
        }
    }

    #[cfg(feature = "secrets-exec")]
    fn resolve_command(&self, name: &str, argv: &[String]) -> Result<String, SecretError> {
        if let Some(Cached::Value(v)) = self.cache.lock().unwrap().get(name) {
            return Ok(v.clone());
        }
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| err(name, "command source has an empty argv"))?;
        let out = std::process::Command::new(program)
            .args(args)
            .output()
            .map_err(|e| err(name, format!("spawn `{program}`: {e}")))?;
        if !out.status.success() {
            return Err(err(name, format!("`{program}` exited with {}", out.status)));
        }
        let v = String::from_utf8(out.stdout)
            .map_err(|_| err(name, "command output is not UTF-8"))?
            .trim_end()
            .to_string();
        if v.is_empty() {
            return Err(err(name, format!("`{program}` produced no output")));
        }
        self.cache
            .lock()
            .unwrap()
            .insert(name.to_string(), Cached::Value(v.clone()));
        Ok(v)
    }

    #[cfg(not(feature = "secrets-exec"))]
    fn resolve_command(&self, name: &str, _argv: &[String]) -> Result<String, SecretError> {
        Err(err(
            name,
            "source `command` needs the `secrets-exec` feature; rebuild with --features secrets-exec",
        ))
    }

    #[cfg(feature = "secrets-oauth2")]
    #[allow(clippy::too_many_arguments)]
    fn resolve_oauth2(
        &self,
        name: &str,
        token_url: &str,
        client_id_env: &str,
        client_secret_env: &str,
        scopes: &[String],
        extra_params: &HashMap<String, String>,
        auth_style: Option<&str>,
        skew_secs: u64,
    ) -> Result<String, SecretError> {
        if let Some(Cached::Token(tok)) = self.cache.lock().unwrap().get(name)
            && !tok.expired(skew_secs)
        {
            return Ok(tok.access_token.clone());
        }
        // The client credentials resolve through THIS registry, so
        // they may themselves come from env, a file, or a command.
        let client_id = self.resolve(client_id_env)?;
        let client_secret = self.resolve(client_secret_env)?;
        let tok = oauth2::fetch_token(
            name,
            token_url,
            &client_id,
            &client_secret,
            scopes,
            extra_params,
            auth_style,
        )?;
        let access = tok.access_token.clone();
        self.cache
            .lock()
            .unwrap()
            .insert(name.to_string(), Cached::Token(tok));
        Ok(access)
    }
}

/// Reject sources this build can't serve — at registry build, so the
/// error names the missing feature at startup.
fn feature_check(name: &str, source: &SourceDef) -> Result<(), SecretError> {
    match source {
        SourceDef::Command { .. } if cfg!(not(feature = "secrets-exec")) => Err(err(
            name,
            "source `command` needs the `secrets-exec` feature; rebuild with --features secrets-exec",
        )),
        SourceDef::Oauth2 { .. } if cfg!(not(feature = "secrets-oauth2")) => Err(err(
            name,
            "source `oauth2` needs the `secrets-oauth2` feature; rebuild with --features secrets-oauth2",
        )),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Process-wide install — the `std::env`-shaped front door
// ---------------------------------------------------------------------------

static REGISTRY: std::sync::OnceLock<ArcSwap<SecretsRegistry>> = std::sync::OnceLock::new();

fn global() -> &'static ArcSwap<SecretsRegistry> {
    REGISTRY.get_or_init(|| ArcSwap::from_pointee(SecretsRegistry::default()))
}

/// Install (or replace, on SIGHUP) the process-wide registry. An empty
/// registry — the default before any install — falls everything back
/// to the environment.
pub fn install(registry: SecretsRegistry) {
    global().store(Arc::new(registry));
}

/// Resolve a secret name through the installed registry, falling back
/// to the process environment. The single front door every consumer
/// uses — `api_key_env`, auth bindings, MCP child env, header
/// placeholders.
pub fn resolve(name: &str) -> Result<String, SecretError> {
    global().load().resolve(name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, source: SourceDef) -> SecretDef {
        SecretDef {
            name: name.into(),
            source,
        }
    }

    #[test]
    fn env_fallback_matches_legacy_behaviour() {
        let reg = SecretsRegistry::default();
        let key = "AGENTD_TEST_SECRET_FALLBACK";
        // SAFETY: single-threaded test scope (Rust 2024 marker).
        unsafe { std::env::set_var(key, "from-env") };
        assert_eq!(reg.resolve(key).unwrap(), "from-env");
        unsafe { std::env::remove_var(key) };
        let e = reg.resolve(key).unwrap_err();
        assert!(e.reason.contains("unset or empty"), "{e}");
    }

    #[test]
    fn file_source_reads_live_and_trims() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("token");
        std::fs::write(&p, "s3cret\n").unwrap();
        let reg = SecretsRegistry::build(&[def(
            "API_TOKEN",
            SourceDef::File {
                path: p.display().to_string(),
                trim: true,
            },
        )])
        .unwrap();
        assert_eq!(reg.resolve("API_TOKEN").unwrap(), "s3cret");
        // Rotation: replace the file, next resolve sees the new value
        // — no restart, no cache.
        std::fs::write(&p, "rotated\n").unwrap();
        assert_eq!(reg.resolve("API_TOKEN").unwrap(), "rotated");
    }

    #[test]
    fn declared_name_shadows_env() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("v");
        std::fs::write(&p, "from-file").unwrap();
        let key = "AGENTD_TEST_SECRET_SHADOW";
        unsafe { std::env::set_var(key, "from-env") };
        let reg = SecretsRegistry::build(&[def(
            key,
            SourceDef::File {
                path: p.display().to_string(),
                trim: true,
            },
        )])
        .unwrap();
        assert_eq!(reg.resolve(key).unwrap(), "from-file");
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn env_alias_source_resolves_other_var() {
        let key = "AGENTD_TEST_SECRET_ALIAS_TARGET";
        unsafe { std::env::set_var(key, "aliased") };
        let reg =
            SecretsRegistry::build(&[def("FRIENDLY_NAME", SourceDef::Env { var: key.into() })])
                .unwrap();
        assert_eq!(reg.resolve("FRIENDLY_NAME").unwrap(), "aliased");
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn duplicate_names_and_missing_files_fail_at_build() {
        let dup = vec![
            def("A", SourceDef::Env { var: "X".into() }),
            def("A", SourceDef::Env { var: "Y".into() }),
        ];
        assert!(
            SecretsRegistry::build(&dup)
                .unwrap_err()
                .reason
                .contains("more than once")
        );

        let missing = vec![def(
            "B",
            SourceDef::File {
                path: "/definitely/not/here".into(),
                trim: true,
            },
        )];
        // The startup probe catches it at build, not first request.
        assert!(SecretsRegistry::build(&missing).is_err());
    }

    #[cfg(feature = "secrets-exec")]
    #[test]
    fn command_source_runs_and_caches() {
        let dir = tempfile::TempDir::new().unwrap();
        let counter = dir.path().join("count");
        // A command with a side effect per invocation: appends a line,
        // prints a value. Cached ⇒ the file gains exactly one line
        // across two resolves.
        let script = dir.path().join("emit.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho run >> {}\necho tok-123\n",
                counter.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let reg = SecretsRegistry::build(&[def(
                "EXEC_TOKEN",
                SourceDef::Command {
                    argv: vec![script.display().to_string()],
                },
            )])
            .unwrap();
            assert_eq!(reg.resolve("EXEC_TOKEN").unwrap(), "tok-123");
            assert_eq!(reg.resolve("EXEC_TOKEN").unwrap(), "tok-123");
            let runs = std::fs::read_to_string(&counter).unwrap();
            assert_eq!(runs.lines().count(), 1, "command must be cached");
        }
    }

    #[cfg(not(feature = "secrets-exec"))]
    #[test]
    fn command_source_without_feature_is_a_build_error() {
        let e = SecretsRegistry::build(&[def(
            "X",
            SourceDef::Command {
                argv: vec!["true".into()],
            },
        )])
        .unwrap_err();
        assert!(e.reason.contains("secrets-exec"), "{e}");
    }

    #[test]
    fn debug_never_prints_material() {
        let key = "AGENTD_TEST_SECRET_DEBUG";
        unsafe { std::env::set_var(key, "hunter2") };
        let reg = SecretsRegistry::build(&[def("N", SourceDef::Env { var: key.into() })]).unwrap();
        let _ = reg.resolve("N").unwrap();
        let rendered = format!("{reg:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("N"), "{rendered}");
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn toml_shape_parses_and_rejects_literal_secrets() {
        let ok: SecretDef = toml::from_str(
            r#"
            name = "SALESFORCE_TOKEN"
            source = "oauth2"
            token_url = "https://login.salesforce.com/services/oauth2/token"
            client_id_env = "SF_CLIENT_ID"
            client_secret_env = "SF_CLIENT_SECRET"
            scopes = ["api"]
            "#,
        )
        .unwrap();
        assert_eq!(ok.name, "SALESFORCE_TOKEN");
        // A literal secret field doesn't exist — deny_unknown_fields
        // makes the temptation a parse error.
        let bad = toml::from_str::<SecretDef>(
            r#"
            name = "X"
            source = "oauth2"
            token_url = "https://x/token"
            client_id_env = "ID"
            client_secret_env = "S"
            client_secret = "hunter2"
            "#,
        );
        assert!(bad.is_err());
    }
}
