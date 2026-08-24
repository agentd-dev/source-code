// SPDX-License-Identifier: AGPL-3.0-only
//! Configuration: precedence, then validate-at-startup.
//!
//! Precedence, top wins: `built-in default < config FILE < env var < CLI flag`.
//! Everything is env-settable (12-factor). The optional
//! declarative file ([`file`] — YAML or JSON, `--config`/`AGENTD_CONFIG`)
//! carries only verbose structural config (MCP-server inventory, declared
//! subscriptions, A2A peers, limits, model/log knobs) and **never** secrets —
//! those stay env/flag only. The whole config is validated **before any side
//! effect** — a bad config exits `2` in milliseconds, not after an LLM
//! round-trip.
//!
//! Module layout: [`file`] (the config document: format detection, the typed
//! `ConfigFile` shape, the JSON Schema), [`yaml`] (the hand-rolled YAML-subset
//! reader), [`paths`] (schema-derived path bindings: `AGENTD_<PATH>` env names
//! and `--<path>` flags for every config-file path), [`watch`] (the inotify
//! reload trigger).

pub mod directives;
pub mod envfile;
pub mod file;
pub mod paths;
pub mod prompt;
pub mod templates;
pub mod v2;
#[cfg(all(unix, feature = "config-watch"))]
pub mod watch;
pub mod yaml;

use crate::obs::log::Level;
use crate::sec::scope::TrifectaTag;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Execution mode. There is one supervisor loop; the mode only chooses the
/// predicate that decides when it is finished.
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
    /// Drive a pinned workflow (`--workflow <file>`) to a terminal graph
    /// status, then exit — the operator entry for deterministic DAGs.
    #[cfg(feature = "workflow")]
    Workflow,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Once => "once",
            Mode::Loop => "loop",
            Mode::Reactive => "reactive",
            Mode::Schedule => "schedule",
            #[cfg(feature = "workflow")]
            Mode::Workflow => "workflow",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "once" => Some(Mode::Once),
            "loop" => Some(Mode::Loop),
            "reactive" => Some(Mode::Reactive),
            "schedule" => Some(Mode::Schedule),
            #[cfg(feature = "workflow")]
            "workflow" => Some(Mode::Workflow),
            _ => None,
        }
    }
}

/// Model hot-swap policy (`--model-swap` / `AGENTD_MODEL_SWAP`): what an
/// in-flight run does when a reload changes the `model` under it. An endpoint
/// repoint that leaves the model unchanged is ALWAYS finish-on-old and
/// invisible, whatever this policy says — nothing about the turn changed.
/// Default `FinishOnOld`. Serialized into the `ControlMsg::SwapIntel` frame so
/// the child applies the same policy the supervisor was configured with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SwapPolicy {
    /// The turn in flight when the reload lands completes on the OLD model; the
    /// NEXT turn uses the new model over the full existing transcript. The
    /// natural turn-boundary behaviour — cheapest, and no work is thrown away.
    #[default]
    FinishOnOld,
    /// The turn in flight finishes (we never tear a `complete_once`) but its
    /// result is DISCARDED and the turn is RE-RUN on the new model from the same
    /// pre-turn transcript state. Costs one turn, and the step budget bounds
    /// how often it can happen. Opt-in.
    RestartTurn,
}

impl SwapPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            SwapPolicy::FinishOnOld => "finish-on-old",
            SwapPolicy::RestartTurn => "restart-turn",
        }
    }
    pub fn parse(s: &str) -> Option<SwapPolicy> {
        match s {
            "finish-on-old" => Some(SwapPolicy::FinishOnOld),
            "restart-turn" => Some(SwapPolicy::RestartTurn),
            _ => None,
        }
    }
}

/// Where `--serve-mcp` binds the served self-MCP. `Stdio` is the implicit
/// default (no `--serve-mcp`). The sole transport is
/// [`Http`](ServeTarget::Http) — `https://HOST:PORT` (TLS, the control plane) or
/// `http://LOOPBACK:PORT` (plaintext, loopback-only dev/tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeTarget {
    /// Bind an HTTP(S) listener at `bind` (a `host:port` authority). `tls` is the
    /// production control plane (`https://`); plaintext (`http://`) is admitted
    /// only for a loopback host (dev/tests).
    Http { bind: String, tls: bool },
    /// Bind a **unix domain socket** at `path` (`unix:///run/agentd/a2a.sock`) —
    /// the co-located-peers transport: same HTTP/1.1 + JSON-RPC over the socket,
    /// no TLS (the kernel authenticates the peer by uid), no TCP overhead.
    Unix { path: String },
}

impl ServeTarget {
    /// Parse a `--serve-mcp` value: `https://host:port` (or loopback
    /// `http://host:port` for dev). Returns a [`ConfigError::Usage`] (exit 2,
    /// before any side effect) on a bad scheme / missing port / a path.
    pub fn parse(spec: &str) -> Result<ServeTarget, ConfigError> {
        // The transport: `https://HOST:PORT` (TLS control plane) or
        // `http://LOOPBACK:PORT` (plaintext, loopback-only dev/tests). The bind is
        // the `host:port` authority (path/query rejected — this is a listener, not
        // a URL to fetch).
        if let Some(tls) = spec
            .strip_prefix("https://")
            .map(|_| true)
            .or_else(|| spec.strip_prefix("http://").map(|_| false))
        {
            let authority = spec.split("://").nth(1).unwrap_or("");
            if authority.is_empty() || authority.contains('/') {
                return Err(usage(format!(
                    "--serve-mcp: want http(s)://HOST:PORT with no path (got: {spec})"
                )));
            }
            let host = serve_host_of(authority);
            let port_ok = serve_port_of(authority).is_some();
            if host.is_empty() || !port_ok {
                return Err(usage(format!(
                    "a2a.listen: HTTP(S) target needs an explicit host:port (got: {spec})"
                )));
            }
            if !tls && !crate::net::http::is_loopback_host(host) {
                return Err(usage(format!(
                    "--serve-mcp: plaintext http:// is allowed for loopback only; use https:// (got: {spec})"
                )));
            }
            return Ok(ServeTarget::Http {
                bind: authority.to_string(),
                tls,
            });
        }
        if let Some(path) = spec
            .strip_prefix("unix://")
            .or_else(|| spec.strip_prefix("unix:"))
        {
            if path.is_empty() {
                return Err(usage(format!("unix listener needs a socket path: {spec}")));
            }
            if !cfg!(unix) {
                return Err(usage(format!(
                    "unix:// listeners are unix-only (got: {spec}); use https://"
                )));
            }
            return Ok(ServeTarget::Unix {
                path: path.to_string(),
            });
        }
        Err(usage(format!(
            "--serve-mcp: want https://host:port (or loopback http://host:port for dev): {spec}"
        )))
    }
}

impl Config {
    /// Validate the TLS material + auth for a `--serve-mcp` target. The
    /// cert/key/CA/bearer fields apply ONLY to an `https://` target; TLS needs
    /// `--serve-cert`+`--serve-key`; and a **non-loopback** listener MUST
    /// authenticate, by mTLS (`--serve-client-ca`) and/or a `--serve-bearer`
    /// token. Reaching the listener is never itself proof of trust, so an open
    /// control plane is refused at startup (exit 2) rather than served.
    fn validate_serve_auth(
        &self,
        target: &ServeTarget,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        let ServeTarget::Http { bind, tls } = target else {
            // A unix listener authenticates by kernel peer credentials
            // (same-uid); the TLS/bearer material below does not apply.
            return Ok(());
        };
        let (bind, tls) = (bind.as_str(), *tls);
        if tls {
            match (&self.serve_cert, &self.serve_key) {
                (Some(cert), Some(key)) => {
                    check_readable("--serve-cert", cert)?;
                    check_readable("--serve-key", key)?;
                }
                _ => {
                    return Err(usage(
                        "--serve-mcp https:// requires --serve-cert and --serve-key (PEM file paths)".into(),
                    ));
                }
            }
        } else if self.serve_cert.is_some() || self.serve_key.is_some() {
            return Err(usage(
                "--serve-cert/--serve-key need an https:// serve target (plaintext http:// is loopback dev only)".into(),
            ));
        }
        if let Some(ca) = &self.serve_client_ca {
            check_readable("--serve-client-ca", ca)?;
        }
        if let Some(bearer) = &self.serve_bearer {
            crate::sec::secret::refs_resolvable(bearer, env)
                .map_err(|e| usage(format!("--serve-bearer: {e}")))?;
        }
        // Never an open control plane: a listener reachable off-box must gate trust.
        let loopback = crate::net::http::is_loopback_host(serve_host_of(bind));
        if !loopback && self.serve_client_ca.is_none() && self.serve_bearer.is_none() {
            return Err(usage(
                "a non-loopback a2a.listen needs client auth: set a2a.tls.client_ca (mTLS) and/or a2a.bearer".into(),
            ));
        }
        Ok(())
    }
}

/// Confirm a file is present + readable (open checks read permission) without
/// retaining its contents — for cert/key/CA PEM paths, checked at startup so a
/// missing/unreadable file is exit 2, not a bind-time surprise.
fn check_readable(flag: &str, path: &str) -> Result<(), ConfigError> {
    std::fs::File::open(path).map_err(|e| usage(format!("{flag}: cannot read {path}: {e}")))?;
    Ok(())
}

/// The host part of a `host:port` authority, unbracketing an IPv6 literal
/// (`[::1]:8443` → `::1`). Never resolves — classifies the written form.
pub(crate) fn serve_host_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    authority.rsplit_once(':').map_or(authority, |(h, _)| h)
}

/// The port of a `host:port` authority (`Some` iff a non-zero `u16` is present).
fn serve_port_of(authority: &str) -> Option<u16> {
    let port_str = if authority.starts_with('[') {
        authority.rsplit_once("]:").map(|(_, p)| p)?
    } else {
        authority.rsplit_once(':').map(|(_, p)| p)?
    };
    port_str.parse::<u16>().ok().filter(|p| *p != 0)
}

/// A declared **A2A peer**: a name and a client transport endpoint to reach a
/// remote A2A agent (or the on-node gateway that forwards into the mesh).
/// `a2a.delegate` looks a peer up here and runs the A2A client against
/// `endpoint`, which is `https://host[:port]` (loopback `http://` for dev) or
/// `unix:/path` for a co-located peer. No secrets live here. Serializable so it
/// travels in the spawn payload to subagents, exactly like `mcp_servers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aPeerSpec {
    pub name: String,
    pub endpoint: String,
    /// Secret-FREE auth header templates presented TO the peer (e.g.
    /// `("authorization", "Bearer {{secret:PEER_TOKEN}}")`), resolved at dial
    /// time exactly like an MCP server's, so no credential is ever present in
    /// the spec, the manifest, the spawn payload or the logs. This is the
    /// bearer leg of peer client-auth.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Client-certificate PEM **file paths** for mutual TLS to the peer (the
    /// mTLS leg of peer client-auth). Both or neither; contents are loaded at
    /// dial time and never inlined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
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
    /// Dial an A2A peer over HTTP(S):
    /// `https://host[:port][/path]` (or loopback `http://` for dev/tests). The
    /// raw URL, parsed by the A2A client's HTTP dialer. A co-located peer may
    /// instead be dialled by `unix:///path` (same URL string, socket dial).
    Https(String),
}

impl A2aEndpoint {
    /// Parse an `--a2a-peer` endpoint. HTTPS-only: an `https://` peer URL, or
    /// a loopback `http://` for dev/tests. Returns a
    /// [`ConfigError::Usage`] (exit 2, before any side effect) on any problem.
    pub fn parse(spec: &str) -> Result<A2aEndpoint, ConfigError> {
        if spec.starts_with("https://") {
            return Ok(A2aEndpoint::Https(spec.to_string()));
        }
        if spec.starts_with("http://") {
            let host = crate::net::http::Url::parse(spec)
                .map(|u| u.host)
                .unwrap_or_default();
            if !crate::net::http::is_loopback_host(&host) {
                return Err(usage(format!(
                    "--a2a-peer: plaintext http:// is allowed for loopback only; use https:// (got: {spec})"
                )));
            }
            return Ok(A2aEndpoint::Https(spec.to_string()));
        }
        // `unix:///run/agentd/peer.sock` — the co-located fast lane: same A2A
        // protocol over a unix socket, authenticated by the kernel (uid) and
        // the socket file's mode instead of TLS. The client dialer branches on
        // the same string, so the variant stays one.
        if let Some(path) = spec
            .strip_prefix("unix://")
            .or_else(|| spec.strip_prefix("unix:"))
        {
            if path.is_empty() || !cfg!(unix) {
                return Err(usage(format!(
                    "--a2a-peer: unix: endpoint needs a socket path (unix-only): {spec}"
                )));
            }
            return Ok(A2aEndpoint::Https(spec.to_string()));
        }
        Err(usage(format!(
            "--a2a-peer: endpoint must be https://host[:port] (or loopback http:// for dev, or unix:///path for a co-located peer): {spec}"
        )))
    }
}

/// A declared MCP server. Serializable because it travels in the subagent spawn
/// payload as the child's scoped server subset.
///
/// The sole transport is a remote [`endpoint`](Self::endpoint) reached over
/// Streamable HTTP. There is no local process spawn, so no configuration path
/// can turn an MCP server into command execution on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct McpServerSpec {
    pub name: String,
    /// Remote MCP endpoint — `https://host[:port][/path]` (loopback `http://`
    /// for dev), reached over Streamable HTTP.
    pub endpoint: String,
    /// Secret-FREE auth/framing header templates (e.g. `("Authorization", "Bearer
    /// {{secret:MCP_TOKEN}}")`), resolved at connect time — no credential is
    /// ever present in the spec, manifest, spawn payload or logs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Operator-declared capability tags (`--mcp-tags`) for the Rule-of-Two
    /// trifecta check. Travels in the spawn payload so a child's narrowed grant
    /// carries the same tags. Empty = untagged, and the check treats an
    /// untagged server conservatively as `untrusted_input` — so forgetting to
    /// tag a server can only tighten the gate, never loosen it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<TrifectaTag>,
    /// Sign requests to THIS server with the AAuth agent identity.
    /// Per-server opt-in: `None` inherits the global default (sign all when an
    /// `--aauth-provider` is configured); `Some(false)` opts out; `Some(true)`
    /// opts in even if the global default were off. Travels in the spawn payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aauth: Option<bool>,
    /// OAuth 2.1 client-credentials for an endpoint behind an OAuth gateway:
    /// a refreshing `Authorization: Bearer …` fetched from the
    /// token endpoint. Secret-free (`client_secret` is a `{{secret:…}}`
    /// template). Travels in the spawn payload; takes the request-signer seam
    /// when set (mutually exclusive with per-server AAuth signing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOauthSpec>,
    /// The unified credential provider. When set it takes precedence over the
    /// narrower `oauth` / `aauth` settings. Travels in the spawn payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthSpec>,
    /// The `services:` catalog entry this server references. The credential
    /// cache key becomes `service:<name>`, so every consumer of the
    /// entry shares one cached login, and the per-instance `rate:` bucket is
    /// keyed by it. Travels in the spawn payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The entry's `rate:` (resolved at config load) — seeds the per-process
    /// pace registry at connect time, so worker and subagent processes pace
    /// their own in-loop calls too. Travels in the spawn payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<String>,
}

/// The runtime shape of an MCP server's OAuth 2.1 client-credentials config.
/// Serializable so it rides the spawn payload verbatim; `client_secret` stays a
/// `{{secret:…}}` template and is resolved only at token-fetch time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOauthSpec {
    pub token_url: String,
    pub client_id: String,
    /// A `{{secret:NAME}}` / `{{secret-file:PATH}}` template (never inline).
    pub client_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// The runtime shape of a unified `auth:` credential provider. Every credential
/// input stays a `{{secret:…}}` template, so this struct rides the spawn payload
/// and appears in logs without ever carrying a live credential. `kind` is one of
/// `static` / `oauth2` / `aws` / `spiffe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AuthSpec {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_authorization_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// A `{{secret:…}}` template for a confidential client (never inline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// static: a bearer token template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// static: an arbitrary header name (with `value`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// aws (SigV4): region, service (e.g. `bedrock`), and credential source
    /// (`env` / `static` / `sso` / `imds` / `irsa`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// aws `source: sso`: IAM Identity Center start URL, account, role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso_start_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_name: Option<String>,
    /// spiffe: SVID type (`jwt`/`x509`) + the SPIRE-written file paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub svid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt_svid_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub svid_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,
}

/// AAuth agent-identity settings. Serde-serializable so it rides the spawn
/// payload verbatim, giving one identity per process tree. The struct is always
/// defined rather than feature-gated, so the payload plumbing compiles the same
/// either way; the CLI flags that populate it require `--features aauth` at
/// validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AAuthSettings {
    /// The Agent Provider base URL (`https://apd.example`) — enroll + agent-token.
    pub provider: String,
    /// The durable Ed25519 key file (created 0600 if absent). A SHARED-FS path,
    /// like `--tls-ca`, so a re-exec'd subagent resolves the same identity.
    pub key_file: String,
    /// A one-time enrollment token template (`{{secret:…}}`), if the provider is
    /// in `token` mode. Secret-free (a reference, never an inline secret).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment_token: Option<String>,
    /// Path to an **enrollment assertion** file the provider federates against
    /// — e.g. a Kubernetes projected ServiceAccount token whose audience is the
    /// provider. Re-read fresh on every enroll (projected tokens rotate), so this
    /// is a PATH, not the assertion itself; it rides the spawn payload like
    /// `key_file`. Presented in the `/enroll` body; never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enroll_assertion_file: Option<String>,
    /// The user's Person Server (`ps` claim), which scopes the identity to a
    /// user. It is carried through enrollment; agentd does not run the
    /// interactive consent flow itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_server: Option<String>,
}

/// Does `s` name a remote MCP endpoint? True for the Streamable HTTP schemes
/// agentd dials.
pub fn is_mcp_endpoint(s: &str) -> bool {
    let s = s.trim();
    // This is a SHAPE test only, so plain `http://` passes here. Whether a
    // given `http://` host is admissible (loopback only) and whether socket
    // schemes are refused is decided by `mcp_endpoint_scheme_ok`, the single
    // gate every server — CLI or config file — flows through at validation.
    s.starts_with("https://") || s.starts_with("http://")
}

/// Whether an MCP-server endpoint scheme is admissible: `https://`, or a
/// loopback `http://` for dev. Socket schemes (`unix:`, `vsock:`) and
/// non-loopback plaintext are rejected. This gate runs BEFORE the reusable
/// crate's `McpEndpoint::parse`, which is more permissive, so that a
/// config-file server — which never goes through `is_mcp_endpoint` or CLI
/// parsing — is held to the same HTTPS-only rule as a flag.
pub fn mcp_endpoint_scheme_ok(endpoint: &str) -> Result<(), ConfigError> {
    let e = endpoint.trim();
    if e.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = e.strip_prefix("http://") {
        let host = rest.split('/').next().unwrap_or(rest);
        let host = if host.starts_with('[') {
            host.split(']').next().map_or(host, |h| &h[1..])
        } else {
            host.rsplit_once(':').map_or(host, |(h, _)| h)
        };
        if crate::net::http::is_loopback_host(host) {
            return Ok(());
        }
        return Err(usage(format!(
            "mcp endpoint plaintext http:// is allowed for loopback only; use https:// (got: {endpoint})"
        )));
    }
    Err(usage(format!(
        "mcp endpoint must be https://host[:port][/path] (got: {endpoint})"
    )))
}

/// The fully-resolved, validated configuration.
#[derive(Clone, PartialEq)]
pub struct Config {
    pub instruction: Option<String>,
    pub intelligence: Option<String>,
    pub intelligence_token: Option<String>,
    /// Path to a mounted file holding the intelligence credential
    /// (`--intelligence-token-file` / `AGENTD_INTELLIGENCE_TOKEN_FILE`). The
    /// token is read and trimmed from this file at load, and re-readable so a
    /// rotation is picked up; the resolved value lands in `intelligence_token`
    /// and never in a log. `--intelligence-token` is the inline alternative.
    pub intelligence_token_file: Option<String>,
    pub model: Option<String>,
    /// Model hot-swap policy (`--model-swap` / `AGENTD_MODEL_SWAP`): what an
    /// in-flight run does when a reload changes `model` under it.
    /// `finish-on-old` (default) | `restart-turn`. An endpoint repoint that
    /// leaves the model unchanged is always finish-on-old regardless.
    /// Reloadable: the reload fans the new policy down with the swap.
    pub model_swap: SwapPolicy,
    pub mcp_servers: Vec<McpServerSpec>,
    /// Declared remote-A2A delegation peers (`--a2a-peer name=endpoint`) —
    /// what `a2a.delegate` dials. Only honoured in `--features a2a` builds,
    /// which startup validation enforces.
    pub a2a_peers: Vec<A2aPeerSpec>,
    pub mode: Mode,
    pub subscribe: Vec<String>,
    /// Subscriptions routed to a **warm continue-session** rather than a fresh
    /// spawn per event: all events on the URI re-enter one live session, in
    /// order. Repeatable `--continue <uri>`.
    pub continue_subscribe: Vec<String>,
    pub interval: Option<Duration>,
    pub max_steps: u32,
    pub max_tokens: u64,
    /// Per-**instance** cumulative token budget across ALL runs/reactions
    /// (`--budget-tokens-lifetime` / `AGENT_BUDGET_TOKENS`). `0` = unbounded.
    /// Distinct from `max_tokens`, which boxes a
    /// single run: a bounded run folds `min(max_tokens, lifetime)` and trips
    /// `EXIT_BUDGET(7)`; a reactive instance stops accepting new reactions and
    /// drains when the cumulative cap is reached.
    pub budget_tokens_lifetime: u64,
    pub deadline: Option<Duration>,
    pub max_depth: u32,
    pub run_id: String,
    pub log_level: Level,
    pub drain_timeout: Duration,
    /// Path to a pinned workflow JSON file (`--workflow`), driven by
    /// `--mode workflow`. `None` unless a workflow is pinned.
    #[cfg(feature = "workflow")]
    pub workflow_file: Option<String>,
    /// Resume a pinned workflow from a checkpoint:
    /// `--workflow-resume <server>:<key>[@seq]` (+ `--workflow-resume-force`).
    /// The child fetches and verifies the envelope after connecting.
    #[cfg(feature = "workflow")]
    pub workflow_resume: Option<crate::subagent::protocol::WorkflowResumeRef>,
    pub serve_mcp: Option<String>,
    /// TLS server cert / key PEM **file paths** for an `https://` serve target.
    /// Required when serving TLS. Only the PATHS live here; the contents — one
    /// of them a private key — are read at bind time and never logged.
    pub serve_cert: Option<String>,
    pub serve_key: Option<String>,
    /// Client-CA PEM **file path** enabling mutual TLS on the serve target: peers
    /// must present a certificate chaining to it. This is the primary way the
    /// `Management` trust domain is minted.
    pub serve_client_ca: Option<String>,
    /// Bearer-token secret for the serve target — the ALTERNATIVE auth to mTLS
    /// (`Authorization: Bearer <token>` mints `Management`). A `sec::secret`
    /// template (`{{secret-file:PATH}}` / `{{secret:ENV}}`) or a literal; resolved
    /// at bind time, never logged.
    pub serve_bearer: Option<String>,
    /// Extra PEM CA **file path** trusted for OUTBOUND `https://` dials
    /// (intelligence, MCP servers, A2A peers, OAuth), ADDED to the bundled
    /// webpki roots — the private/in-cluster PKI trust anchor (`--tls-ca` /
    /// `AGENTD_TLS_CA`). Public material (a CA certificate, never a key);
    /// installed process-wide at startup ([`crate::net::tls::install_extra_ca`])
    /// and inherited by every subagent via the spawn payload. Set-once
    /// (restart-only): trust anchors must not move under a live run.
    pub tls_ca: Option<String>,
    /// AAuth agent-identity config: when the provider URL is set, agentd gets
    /// an Ed25519 identity + agent token and SIGNS every
    /// outbound MCP request. `None` = no AAuth (the default). Rides the spawn
    /// payload to subagents (one identity per process tree). Needs
    /// `--features aauth`.
    pub aauth: Option<AAuthSettings>,
    pub health_file: Option<String>,
    /// Inbound W3C `traceparent` to continue; with none set, a trace is minted
    /// from the run id so a run always has one.
    pub traceparent: Option<String>,
    /// Opt-in content capture. Off by default: telemetry logs hashes and
    /// lengths only, so a trace backend never becomes an unreviewed copy of
    /// every tool argument. `--log-content` adds the actual tool args/results,
    /// truncated. Propagates to children via the telemetry block.
    pub log_content: bool,
    /// Opt-in HTTP probe/scrape surface (`/metrics` + `/healthz` + `/readyz`).
    /// Off unless set; only honoured in `--features metrics` builds.
    pub metrics_addr: Option<String>,
    /// Opt-in cgroup-v2 active enforcement: `auto` (derive `<own-cgroup>/agentd`)
    /// or an absolute path under `/sys/fs/cgroup`. Each run gets a child cgroup
    /// for atomic `cgroup.kill` teardown. Best-effort — disabled if not writable;
    /// agentd stays cgroup-aware, never cgroup-requiring.
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
    /// instead of refusing at startup. A process-global operator override,
    /// deliberately NOT carried in the spawn payload — a child must be granted
    /// the exception on its own terms rather than inheriting it silently.
    pub allow_trifecta: bool,
    /// Optional 5-field UTC cron schedule for `--mode schedule`.
    /// Only honoured in `--features cron` builds; the production path is an
    /// external CronJob → `--mode once`.
    pub cron: Option<String>,
    /// Where to write the run-outcome report at the terminal transition
    /// (`--report-file PATH` / `AGENTD_REPORT_FILE`). Written atomically via a
    /// temp file and rename, so a reader never sees a half-written report. Off
    /// for a bare CLI run, and inert for `--mode reactive` — a reactive daemon
    /// has no single terminal outcome, which startup warns about.
    pub report_file: Option<String>,
    /// Operator remap for the two *policy* budget exit codes
    /// (`--budget-exit-code N`).
    /// `None` ⇒ no remap (the canonical table applies). When set, a final process
    /// exit of `EXIT_PARTIAL` (3) **or** `EXIT_BUDGET` (7) — and ONLY those two,
    /// the operator-tunable `policy`-intent codes — is returned to the OS as `N`
    /// instead, so a Job's `podFailurePolicy` can treat a budget/partial outcome
    /// as success-or-fail per operator policy. Every other code (a deadline 124, a
    /// refusal 5, a clean 0) is NEVER remapped. The run **report** still records
    /// the canonical 3/7 projection + the precise `status`, so the durable record
    /// stays truthful (and schema-valid) regardless of the remap.
    pub budget_exit_code: Option<i32>,
    /// Capacity of the bounded `agentd://events` ring (`--events-ring N` /
    /// `AGENTD_EVENTS_RING`): the last N emitted lines held in
    /// memory for the live-tail resource. Default 1024. Only consumed when the
    /// `events` surface is served (`--serve-mcp` + the `events` feature).
    pub events_ring: usize,
    /// Declared intelligence HTTP headers, settable only via the config file's
    /// `intelligence_headers`. Values are **templates** that may carry
    /// `{{secret:NAME}}` / `{{secret-file:PATH}}` refs: the names and refs are
    /// structural, while the resolved secret is never stored here or logged. An
    /// inline secret-shaped value is rejected at validation. A `BTreeMap`, so
    /// header order is deterministic.
    pub intelligence_headers: std::collections::BTreeMap<String, String>,
    /// Watch the config file for changes and reload (`--watch-config` /
    /// `AGENTD_WATCH_CONFIG`). When set, the reactive supervisor arms a raw
    /// `inotify` watch on the config file's PARENT DIRECTORY — a Kubernetes
    /// ConfigMap volume swap is an atomic directory-symlink rename, which a
    /// watch on the file itself would miss — and, on a change to the watched
    /// file, sets the SAME RELOAD latch SIGHUP sets, so there is exactly one
    /// reload routine to reason about. Always compiled (a uniform `Config`);
    /// `true` needs the
    /// `config-watch` build feature (validated, exit 2) AND a config file to
    /// watch (`--config`/`AGENTD_CONFIG`, else exit 2 — watching nothing is a
    /// usage error). Off by default; SIGHUP is the portable, dependency-free
    /// default trigger.
    pub watch_config: bool,
    /// The config files that were merged into the FILE layer, in order
    /// (`AGENTD_CONFIG` entries first, then each `--config`); empty when no file
    /// is in play. Informational — logged at startup, watched by
    /// `--watch-config`; never a reload diff (args/env are fixed for the
    /// process's life).
    pub config_files: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            instruction: None,
            intelligence: None,
            intelligence_token: None,
            intelligence_token_file: None,
            model: None,
            model_swap: SwapPolicy::FinishOnOld,
            mcp_servers: Vec::new(),
            a2a_peers: Vec::new(),
            mode: Mode::Once,
            subscribe: Vec::new(),
            continue_subscribe: Vec::new(),
            interval: None,
            max_steps: 50,
            max_tokens: 200_000,
            budget_tokens_lifetime: 0,
            deadline: Some(Duration::from_secs(600)),
            max_depth: 4,
            run_id: String::new(), // filled in load() if unset
            log_level: Level::Info,
            drain_timeout: Duration::from_secs(25),
            #[cfg(feature = "workflow")]
            workflow_file: None,
            #[cfg(feature = "workflow")]
            workflow_resume: None,
            serve_mcp: None,
            serve_cert: None,
            serve_key: None,
            serve_client_ca: None,
            serve_bearer: None,
            tls_ca: None,
            aauth: None,
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
            budget_exit_code: None,
            events_ring: crate::obs::log::EVENTS_RING_DEFAULT,
            intelligence_headers: std::collections::BTreeMap::new(),
            // Off by default; flipped to `true` when `--standby` is set unless
            // `AGENTD_WARM_INTEL` explicitly overrides (resolved in `load`).
            watch_config: false,
            config_files: Vec::new(),
        }
    }
}

// Redact the credential — never let it reach a log or a panic message.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("instruction", &self.instruction.as_deref().map(|_| "<set>"))
            // The raw `--intelligence` URI can be credential-bearing
            // (`http://user:pass@host`), so redact it to its transport SCHEME
            // only — matching `effective_view()` and the `config.loaded` event,
            // which are already scheme-only — and a Debug render can never leak
            // an inline endpoint credential.
            .field(
                "intelligence",
                &self
                    .intelligence
                    .as_deref()
                    .map(|u| format!("{}:<redacted>", u.split(':').next().unwrap_or(""))),
            )
            .field(
                "intelligence_token",
                &self.intelligence_token.as_ref().map(|_| "***"),
            )
            .field("intelligence_token_file", &self.intelligence_token_file)
            .field("model", &self.model)
            .field("model_swap", &self.model_swap.as_str())
            .field("mcp_servers", &self.mcp_servers)
            .field("a2a_peers", &self.a2a_peers)
            .field("mode", &self.mode)
            .field("subscribe", &self.subscribe)
            .field("continue_subscribe", &self.continue_subscribe)
            .field("interval", &self.interval)
            .field("max_steps", &self.max_steps)
            .field("max_tokens", &self.max_tokens)
            .field("budget_tokens_lifetime", &self.budget_tokens_lifetime)
            .field("deadline", &self.deadline)
            .field("max_depth", &self.max_depth)
            .field("run_id", &self.run_id)
            .field("log_level", &self.log_level)
            .field("drain_timeout", &self.drain_timeout)
            .field("serve_mcp", &self.serve_mcp)
            // Cert/key/CA are file PATHS, not secrets, so they are safe to
            // show; the bearer IS a credential, so only its presence appears.
            .field("serve_cert", &self.serve_cert)
            .field("serve_key", &self.serve_key)
            .field("serve_client_ca", &self.serve_client_ca)
            .field(
                "serve_bearer",
                &self.serve_bearer.as_ref().map(|_| "<redacted>"),
            )
            .field("tls_ca", &self.tls_ca)
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
            // Header NAMES only: a value may carry a {{secret:…}} ref, and a
            // rendered config is not a place a secret may reach.
            .field(
                "intelligence_headers",
                &self.intelligence_headers.keys().collect::<Vec<_>>(),
            )
            .field("watch_config", &self.watch_config)
            .field("config_files", &self.config_files)
            .finish()
    }
}

/// What `load()` can short-circuit with. `Help`/`Version`/`Capabilities` are
/// *not* errors (exit 0); `Usage` is a validation or parse failure (exit 2).
/// `Capabilities` carries the pretty-printed manifest JSON — the
/// side-effect-free admission probe (`agentd --capabilities`), short-circuited
/// before run-required validation so it succeeds even with no instruction,
/// which is what lets agentctl probe an image that has no run config yet.
#[derive(Debug)]
pub enum ConfigError {
    Help(String),
    Version(String),
    Capabilities(String),
    Usage(String),
    /// `--config-schema`: the JSON Schema of the config file,
    /// printed to **stdout**, exit 0 — a side-effect-free schema export so
    /// agentctl can validate a CR before applying it.
    Schema(String),
    /// `--validate-config`: the admission verdict. `Ok(line)` is
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

/// De-branding normalization: accept the neutral `AGENT_*` env prefix as an
/// input alias for the branded `AGENTD_*` one. Returns the env list with a
/// synthesized `AGENTD_<X>` entry for every `AGENT_<X>` whose branded form is
/// ABSENT — the branded spelling WINS when both are present, since it is the
/// more specific of the two. Branded keys are never dropped, and a
/// non-prefixed key (e.g. `INSTRUCTION`) is untouched. Done once, here, so
/// every downstream `AGENTD_*` read transparently honours `AGENT_*` too
/// without a per-read change.
pub(crate) fn debrand_env(env: &[(String, String)]) -> Vec<(String, String)> {
    let have: std::collections::HashSet<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    let mut out: Vec<(String, String)> = env.to_vec();
    for (k, v) in env {
        // `AGENTD_*` itself does NOT match `AGENT_` (the 6th char is `D`, not `_`),
        // so branded keys are never re-aliased; only true neutral keys are.
        if let Some(suffix) = k.strip_prefix("AGENT_") {
            let branded = format!("AGENTD_{suffix}");
            if !have.contains(branded.as_str()) {
                out.push((branded, v.clone()));
            }
        }
    }
    out
}

impl Config {
    /// Resolve config from CLI args (excluding the leading program name) and
    /// the environment, applying precedence — `built-in default < FILE < env <
    /// flag` — and validating the result before any side effect.
    pub fn load(args: &[String], env: &[(String, String)]) -> Result<Config, ConfigError> {
        // De-branding: every branded `AGENTD_*` env var also accepts
        // its neutral `AGENT_*` spelling on input. Normalize ONCE here — for any
        // `AGENT_<X>` present, synthesize an `AGENTD_<X>` entry iff the branded form
        // is absent (branded WINS on conflict, preserving back-compat) — so every
        // downstream `AGENTD_*` read below transparently honours `AGENT_*` too, with
        // no per-read change. The branded spelling is never dropped, only aliased.
        let env = debrand_env(env);
        let envmap: HashMap<&str, &str> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        // `--config-schema`: a side-effect-free schema export.
        // The schema is static (generated from the `ConfigFile` types), so it
        // short-circuits BEFORE the file is even read — exit 0, JSON to stdout.
        if args.iter().any(|a| a == "--config-schema") {
            let schema = crate::config::file::config_schema();
            let json = serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string());
            return Err(ConfigError::Schema(format!("{json}\n")));
        }
        // `--validate-config`: captured here, acted on at the end.
        // It is the side-effect-free admission verdict — it validates whatever
        // config is given and never requires an --instruction to *validate*.
        let validate_config = args.iter().any(|a| a == "--validate-config");

        let mut c = Config::default();

        // --- FILE layer (precedence layer 1) ---
        // `--config <path>` / `AGENTD_CONFIG`. The file is the lowest
        // non-default layer: env and flags below override it, while repeatable
        // list flags ADD to the file's lists. A malformed or unreadable file is
        // exit 2 BEFORE any side effect — it is parsed before the env and flag
        // layers touch `c`. Several files compose into ONE document, in order:
        // `AGENTD_CONFIG` (a `:`-separated list) first, then each `--config`,
        // with each later file merged over the earlier ones by RFC 7396 JSON
        // Merge Patch rules (objects merge, scalars and lists replace, `null`
        // unsets). Each file is YAML or JSON
        // by extension, else sniffed (`file::Format`).
        let config_paths = config_paths_from_map(args, &envmap).paths;
        let file_present = !config_paths.is_empty();
        if file_present {
            let (doc, loaded) = file::read_documents(&config_paths).map_err(usage)?;
            apply_document(&mut c, doc, "config file", false)?;
            c.config_files = loaded.into_iter().map(|(p, _)| p).collect();
        }

        // --- env layer ---
        // The two REQUIRED inputs each accept a bare spelling alongside the
        // prefixed one (`INSTRUCTION`/`INTELLIGENCE` next to `AGENT[D]_*`), so the
        // minimal quickstart is `INSTRUCTION=… INTELLIGENCE=… agentd`. Precedence
        // within the env layer is by specificity: branded > neutral (debrand_env
        // above) > bare — a prefixed spelling always wins over the bare one.
        if let Some(v) = envmap
            .get("AGENTD_INSTRUCTION")
            .or_else(|| envmap.get("INSTRUCTION"))
        {
            c.instruction = Some((*v).to_string());
        }
        if let Some(v) = envmap
            .get("AGENTD_INTELLIGENCE")
            .or_else(|| envmap.get("INTELLIGENCE"))
        {
            c.intelligence = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_INTELLIGENCE_TOKEN") {
            c.intelligence_token = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_INTELLIGENCE_TOKEN_FILE") {
            c.intelligence_token_file = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_TLS_CA") {
            c.tls_ca = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_MODEL") {
            c.model = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_MODEL_SWAP") {
            c.model_swap = SwapPolicy::parse(v).ok_or_else(|| {
                usage(format!(
                    "invalid AGENTD_MODEL_SWAP: {v} (want finish-on-old|restart-turn)"
                ))
            })?;
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
        // The per-instance lifetime budget. The neutral `AGENT_BUDGET_TOKENS`
        // is auto-aliased to `AGENTD_BUDGET_TOKENS` by the debranding pass
        // above, so only the branded name is read here.
        if let Some(v) = envmap.get("AGENTD_BUDGET_TOKENS") {
            c.budget_tokens_lifetime = v
                .parse()
                .map_err(|_| usage(format!("invalid AGENTD_BUDGET_TOKENS: {v}")))?;
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
        #[cfg(feature = "workflow")]
        if let Some(v) = envmap.get("AGENTD_WORKFLOW") {
            c.workflow_file = Some((*v).to_string());
        }
        #[cfg(feature = "workflow")]
        if let Some(v) = envmap.get("AGENTD_WORKFLOW_RESUME") {
            c.workflow_resume = Some(parse_workflow_resume(v)?);
        }
        if let Some(v) = envmap.get("AGENTD_SERVE_MCP") {
            c.serve_mcp = Some((*v).to_string());
        }
        // TLS material + auth for an `https://` serve target.
        if let Some(v) = envmap.get("AGENTD_SERVE_CERT") {
            c.serve_cert = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_SERVE_KEY") {
            c.serve_key = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_SERVE_CLIENT_CA") {
            c.serve_client_ca = Some((*v).to_string());
        }
        if let Some(v) = envmap.get("AGENTD_SERVE_BEARER") {
            c.serve_bearer = Some((*v).to_string());
        }
        // File-watch reload trigger. `AGENTD_WATCH_CONFIG` is a bool; a
        // `--watch-config` flag below overrides it. Needs the `config-watch`
        // build feature and a config file to watch — both validated, exit 2.
        if let Some(v) = envmap.get("AGENTD_WATCH_CONFIG") {
            c.watch_config = truthy(v);
        }
        // A single `AGENTD_A2A_PEER` env declares one peer: the env channel
        // carries one value, so more peers need repeated `--a2a-peer` flags.
        if let Some(v) = envmap.get("AGENTD_A2A_PEER") {
            c.a2a_peers.push(parse_a2a_peer_spec(v)?);
        }
        if let Some(v) = envmap.get("AGENTD_TRACEPARENT") {
            c.traceparent = Some((*v).to_string());
        }

        // --- env layer, path-derived names (config::paths) ---
        // Every config-file path is settable as `AGENTD_<PATH>` / `AGENT_<PATH>`
        // / bare `<PATH>` (`.` → `_`, upper-cased): `limits.max_steps` ⇒
        // `AGENTD_LIMITS_MAX_STEPS`. The names derive from the schema, so a
        // re-defined parameter set needs no plumbing here. Applied AFTER the
        // named env reads above, so where a short spelling and a path spelling
        // both name one field, the path spelling — the canonical form — wins
        // within the env layer. Flags below still override both.
        {
            let (doc, applied) = paths::env_document(&envmap).map_err(usage)?;
            if !applied.is_empty() {
                // Setting a path SETS its value: a list/map path from env
                // replaces the file's (the named `AGENTD_A2A_PEER` etc. add).
                apply_document(&mut c, doc, "env", true)?;
            }
        }

        // --- flag layer (overrides env) ---
        // `--mcp-tags` may precede or follow its `--mcp`; collect and apply once
        // every server is known.
        let mut mcp_tags: Vec<(String, Vec<TrifectaTag>)> = Vec::new();
        // `--capabilities` is the admission probe: captured here and resolved
        // after the whole config is parsed but BEFORE run-required validation,
        // so it reflects whatever config is present and still succeeds when
        // there is no instruction to run.
        let mut capabilities = false;
        // AAuth sub-flags accumulate here (order-independent) and are
        // assembled into `c.aauth` after the loop.
        let mut aauth_provider: Option<String> = None;
        let mut aauth_key_file: Option<String> = None;
        let mut aauth_enroll_token: Option<String> = None;
        let mut aauth_enroll_assertion_file: Option<String> = None;
        let mut aauth_person_server: Option<String> = None;
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
                "--config" | "-c" => {
                    let _ = take("--config")?;
                }
                // `--config=a.yaml` / `-c=a.yaml`: value already attached.
                a if matches!(config_flag(a), ConfigFlag::Inline(_)) => {}
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
                "--model-swap" => {
                    let v = take("--model-swap")?;
                    c.model_swap = SwapPolicy::parse(&v).ok_or_else(|| {
                        usage(format!(
                            "invalid --model-swap: {v} (want finish-on-old|restart-turn)"
                        ))
                    })?;
                }
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
                "--budget-tokens-lifetime" => {
                    let v = take("--budget-tokens-lifetime")?;
                    c.budget_tokens_lifetime = v
                        .parse()
                        .map_err(|_| usage(format!("invalid --budget-tokens-lifetime: {v}")))?;
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
                "--log-content" => c.log_content = true,
                "--allow-trifecta" => c.allow_trifecta = true,
                "--mcp-tags" => mcp_tags.push(parse_mcp_tags(&take("--mcp-tags")?)?),
                "--metrics-addr" => c.metrics_addr = Some(take("--metrics-addr")?),
                "--cgroup" => c.cgroup = Some(take("--cgroup")?),
                "--cgroup-memory-max" => c.cgroup_memory_max = Some(take("--cgroup-memory-max")?),
                "--cgroup-pids-max" => c.cgroup_pids_max = Some(take("--cgroup-pids-max")?),
                #[cfg(feature = "workflow")]
                "--workflow" => c.workflow_file = Some(take("--workflow")?),
                #[cfg(feature = "workflow")]
                "--workflow-resume" => {
                    // Order-independent with --workflow-resume-force: a force
                    // remembered from either side survives.
                    let force = c.workflow_resume.as_ref().is_some_and(|r| r.force);
                    let mut r = parse_workflow_resume(&take("--workflow-resume")?)?;
                    r.force = r.force || force;
                    c.workflow_resume = Some(r);
                }
                #[cfg(feature = "workflow")]
                "--workflow-resume-force" => {
                    match c.workflow_resume.as_mut() {
                        Some(r) => r.force = true,
                        // Order-independent: remember the force for a later
                        // --workflow-resume (validated below to require one).
                        None => {
                            c.workflow_resume = Some(crate::subagent::protocol::WorkflowResumeRef {
                                server: String::new(),
                                key: String::new(),
                                seq: None,
                                force: true,
                            })
                        }
                    }
                }
                "--serve-mcp" => c.serve_mcp = Some(take("--serve-mcp")?),
                "--serve-cert" => c.serve_cert = Some(take("--serve-cert")?),
                "--serve-key" => c.serve_key = Some(take("--serve-key")?),
                "--serve-client-ca" => c.serve_client_ca = Some(take("--serve-client-ca")?),
                "--serve-bearer" => c.serve_bearer = Some(take("--serve-bearer")?),
                "--tls-ca" => c.tls_ca = Some(take("--tls-ca")?),
                // AAuth: --aauth-provider is what turns it on; the rest fill
                // AAuthSettings. Gathered into `c.aauth` after the loop, so the
                // sub-flags may appear in any order.
                "--aauth-provider" => aauth_provider = Some(take("--aauth-provider")?),
                "--aauth-key-file" => aauth_key_file = Some(take("--aauth-key-file")?),
                "--aauth-enroll-token" => aauth_enroll_token = Some(take("--aauth-enroll-token")?),
                "--aauth-enroll-assertion-file" => {
                    aauth_enroll_assertion_file = Some(take("--aauth-enroll-assertion-file")?)
                }
                "--aauth-person-server" => {
                    aauth_person_server = Some(take("--aauth-person-server")?)
                }
                // File-watch reload trigger: watch the config file's
                // directory and reload on a change. Needs the
                // `config-watch` build feature + a `--config`/`AGENTD_CONFIG`
                // file (both validated, exit 2). Off by default; SIGHUP is the
                // portable default trigger.
                "--watch-config" => c.watch_config = true,
                "--health-file" => c.health_file = Some(take("--health-file")?),
                "--traceparent" => c.traceparent = Some(take("--traceparent")?),
                "--report-file" => c.report_file = Some(take("--report-file")?),
                // Remap the two operator-tunable `policy` budget codes
                // (EXIT_PARTIAL 3 / EXIT_BUDGET 7) to N at the final process
                // exit. N must be a valid POSIX exit byte (0..=255), and only
                // 3 and 7 are ever remapped — every other code carries a
                // meaning the operator does not get to redefine.
                "--budget-exit-code" => {
                    let v = take("--budget-exit-code")?;
                    let n: i32 = v
                        .parse()
                        .ok()
                        .filter(|n| (0..=255).contains(n))
                        .ok_or_else(|| {
                            usage(format!("invalid --budget-exit-code: {v} (want 0..=255)"))
                        })?;
                    c.budget_exit_code = Some(n);
                }
                "--events-ring" => {
                    let v = take("--events-ring")?;
                    c.events_ring = v
                        .parse()
                        .map_err(|_| usage(format!("invalid --events-ring: {v}")))?;
                }
                // Generic path flags (config::paths): any config-file path is a
                // flag — `--limits.max-steps 5` / `--limits-max-steps 5` — typed
                // by the schema and applied in argument order like every other
                // flag (last writer wins; lists add). A boolean path takes an
                // optional value (`--x` alone means true). Anything that is not
                // a known flag NOR a config path is the usual usage error.
                other => match paths::resolve_flag(other).map_err(usage)? {
                    Some(target) => {
                        let raw = if matches!(target.value_kind(), paths::Kind::Boolean)
                            && !it.peek().is_some_and(|n| !n.starts_with("--"))
                        {
                            "true".to_string()
                        } else {
                            it.next()
                                .cloned()
                                .ok_or_else(|| usage(format!("{other} requires a value")))?
                        };
                        let value = paths::coerce(target.value_kind(), &raw)
                            .map_err(|e| usage(format!("invalid {other}: {e}")))?;
                        // Setting a path SETS its value (a list path replaces the
                        // list); a `--<map>.<key>` entry flag merges ONE key.
                        let replace = target.entry.is_none();
                        apply_document(&mut c, target.document(value), other, replace)?;
                    }
                    None => return Err(usage(format!("unknown argument: {other}"))),
                },
            }
        }

        // Assemble the AAuth settings. The provider (flag or
        // AGENT_AAUTH_PROVIDER env) is what turns it on; a key file defaults to
        // `./agent.key` in the process cwd (a durable, shared-fs identity).
        let aauth_provider =
            aauth_provider.or_else(|| envmap.get("AGENT_AAUTH_PROVIDER").map(|v| v.to_string()));
        if let Some(provider) = aauth_provider {
            c.aauth = Some(AAuthSettings {
                provider,
                key_file: aauth_key_file
                    .or_else(|| envmap.get("AGENT_AAUTH_KEY_FILE").map(|v| v.to_string()))
                    .unwrap_or_else(|| "agent.key".to_string()),
                enrollment_token: aauth_enroll_token.or_else(|| {
                    envmap
                        .get("AGENT_AAUTH_ENROLL_TOKEN")
                        .map(|v| v.to_string())
                }),
                enroll_assertion_file: aauth_enroll_assertion_file.or_else(|| {
                    envmap
                        .get("AGENT_AAUTH_ENROLL_ASSERTION_FILE")
                        .map(|v| v.to_string())
                }),
                person_server: aauth_person_server.or_else(|| {
                    envmap
                        .get("AGENT_AAUTH_PERSON_SERVER")
                        .map(|v| v.to_string())
                }),
            });
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

        // `--capabilities` is owned by the settings loader
        // (`config::v2` / `runtime::capabilities`), which the binary routes to
        // first, so this branch is unreachable in the shipped binary and only
        // exists so the flat path answers something coherent if it is called.
        if capabilities {
            return Err(ConfigError::Capabilities(
                "{\"note\":\"--capabilities is served by the agentd loader\"}\n".to_string(),
            ));
        }

        // Resolve `--intelligence-token-file` into the token. An inline
        // `--intelligence-token` or env wins, being the higher-precedence
        // source; the file is the fallback. Read and trimmed here, but a
        // missing file is reported through `validate()` so `--validate-config`
        // collects it with the rest, and the resolved value never reaches a
        // log.
        c.resolve_token_file()?;

        // `--validate-config`: the side-effect-free admission verdict. Run the
        // FULL validation pipeline, collecting EVERY diagnostic
        // (not fast-failing on the first, unlike startup) so an operator/CI sees
        // all problems in one pass, then short-circuit with the verdict. It does
        // NOT require an --instruction to *validate* — it validates whatever it is
        // given. The caller prints to stderr and maps the result to exit 0/2.
        if validate_config {
            return Err(ConfigError::Validate(c.validate_collect_all(file_present)));
        }

        c.validate()?;
        // `--watch-config` requires a config FILE to watch: watching nothing
        // is a usage error. This is the one check that needs the
        // resolved file-presence (not a `Config` field), so it lives here in
        // `load` (and is mirrored in `validate_collect_all` for the admission
        // gate). Checked after `validate()` so the feature-gate error (in
        // `validate()`) surfaces first when both are wrong.
        if c.watch_config && !file_present {
            return Err(usage(
                "--watch-config requires a config file (--config / AGENTD_CONFIG)".into(),
            ));
        }
        Ok(c)
    }

    /// The config files in play for `args`/`env`, in merge order: the
    /// `AGENTD_CONFIG` / `AGENT_CONFIG` list (`:`-separated, PATH-style) first,
    /// then every `--config <path>` in argument order. Empty when none. Shared
    /// by `load`, the reload path, and the file watcher (which arms one watch
    /// per file). Pure.
    pub fn config_paths_from(args: &[String], env: &[(String, String)]) -> Vec<String> {
        let env = debrand_env(env);
        let envmap: HashMap<&str, &str> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        config_paths_from_map(args, &envmap).paths
    }

    /// Resolve `--intelligence-token-file` into `intelligence_token` when no
    /// inline token is set. A read failure surfaces as a usage error (exit 2 at
    /// startup; collected by `--validate-config`). The token is never logged —
    /// the error carries only the path.
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

    /// Run the full validation pipeline, collecting EVERY diagnostic as one
    /// NDJSON `config.{valid,invalid}` line set. `Ok(line)` ⇒ valid (exit 0);
    /// `Err(lines)` ⇒ one or more `config.invalid` lines (exit 2).
    ///
    /// Each independent check is run and its message collected, so the operator
    /// sees all problems at once. The check SET is exactly `validate()`'s: there
    /// is one validation authority, so the admission gate can never accept a
    /// config the startup path would refuse.
    fn validate_collect_all(&self, file_present: bool) -> Result<String, String> {
        let mut diags: Vec<String> = Vec::new();
        // `validate()` is fast-fail, so it cannot report everything on its own,
        // and re-running it once per fixed error would be O(n²) and brittle.
        // Instead the independent declarative checks run directly here and each
        // failing one is appended; the header/secret checks plus one final
        // `validate()` pass — which catches anything not separately enumerated
        // — give complete coverage from a single source of truth.
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
        // `--watch-config` needs a config FILE to watch — the one check that
        // depends on file presence, mirrored from `load`'s startup path so
        // the admission gate (`--validate-config`) rejects it too.
        if self.watch_config && !file_present {
            diags.push("--watch-config requires a config file (--config / AGENTD_CONFIG)".into());
        }
        // The reload-coherence check, with no running config at the admission
        // gate (`running = None`), so this reports the restart-only-field-in-
        // file WARNINGS and the reloadable-subset consistency ERRORS. An
        // admission webhook sees both; a coherence ERROR makes the verdict invalid.
        // (Internal-consistency errors here largely overlap with `validate()`'s
        // own checks, so dedup by message suffix to avoid a double line.)
        match Config::reload_coherence_check(self, None, file_present) {
            Ok(()) => {}
            Err(coh) => {
                for d in coh.into_iter().filter(|d| d.is_error()) {
                    let line = format!("{}: {}", d.field, d.msg);
                    if !diags.iter().any(|existing| existing.ends_with(&d.msg)) {
                        diags.push(line);
                    }
                }
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

    /// Validate the declared `intelligence_headers`: a value may be a plain
    /// scalar or carry `{{secret:NAME}}` / `{{secret-file:PATH}}` refs, but an
    /// **inline secret-shaped value** — a header named like a credential whose
    /// value is NOT a ref — is rejected, because a secret must be a reference
    /// rather than a literal in the file. Every ref must also resolve (the env
    /// var is set, the file exists), else exit 2.
    fn collect_header_diags(&self, diags: &mut Vec<String>) {
        let env = |k: &str| std::env::var(k).ok();
        for (name, value) in &self.intelligence_headers {
            // A credential-shaped header carrying a literal (non-ref) value
            // is the "inline secret in the file" footgun — reject it.
            if is_secret_shaped_key(name) && !crate::sec::secret::has_secret_ref(value) {
                diags.push(format!(
                    "intelligence_headers['{name}'] looks like a credential but has an inline value; \
                     use {{{{secret:NAME}}}} or {{{{secret-file:PATH}}}} (never an inline secret)"
                ));
                continue;
            }
            // Every secret ref must resolve at startup: a missing env var or
            // an unreadable file is exit 2 before any side effect, because a
            // ref that does not resolve means the header is simply not sent.
            if crate::sec::secret::has_secret_ref(value)
                && let Err(e) = crate::sec::secret::refs_resolvable(value, &env)
            {
                diags.push(format!("intelligence_headers['{name}']: {e}"));
            }
        }
    }

    /// The capability-tag union of the root agent's grant, for the Rule-of-Two
    /// trifecta check. An untagged MCP server contributes `untrusted_input`,
    /// the conservative default. Because a subagent's scope can only narrow,
    /// never widen, enforcing on this root union bounds the whole subagent
    /// tree.
    pub fn trifecta_grant_tags(&self) -> Vec<TrifectaTag> {
        let mut tags = Vec::new();
        for s in &self.mcp_servers {
            if s.tags.is_empty() {
                tags.push(TrifectaTag::UntrustedInput);
            } else {
                tags.extend(s.tags.iter().copied());
            }
        }
        tags
    }

    /// Reject inconsistent config before any side effect runs.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // A pinned workflow run (`--mode workflow`) carries its instructions in the
        // graph nodes, so it needs no top-level `--instruction` — and neither does
        // a PURE reactive WORKFLOW daemon (`--mode reactive --workflow` with no
        // subscription routes: its only reactions are the workflow's own
        // suspend/resume steps). A daemon that ALSO has --subscribe/--continue
        // routes spawns instruction reactions, so those still require one — an
        // empty-instruction reaction would hand the model a blank task.
        #[cfg(feature = "workflow")]
        let needs_instruction = self.mode != Mode::Workflow
            && !(self.mode == Mode::Reactive
                && self.workflow_file.is_some()
                && self.subscribe.is_empty()
                && self.continue_subscribe.is_empty());
        #[cfg(not(feature = "workflow"))]
        let needs_instruction = true;
        if needs_instruction
            && self
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
        // Per-endpoint credential probe: a named-but-unset per-endpoint token
        // *file* on ANY listed endpoint is exit 2. Failing fast at startup
        // beats discovering an unreadable secret at the moment of failover,
        // when the primary endpoint is already down.
        validate_endpoint_token_files(self.intelligence.as_deref().unwrap())?;
        for s in &self.mcp_servers {
            if s.name.is_empty() {
                return Err(usage("mcp server has an empty name".into()));
            }
            if s.endpoint.trim().is_empty() {
                return Err(usage(format!("mcp server '{}' has no endpoint", s.name)));
            }
            // The HTTPS-only gate runs FIRST: the reusable crate's parser also
            // accepts unix:/vsock:, so every server — CLI and config-file alike
            // — is held to http(s) here before it is delegated to.
            mcp_endpoint_scheme_ok(&s.endpoint)
                .map_err(|e| usage(format!("mcp server '{}': {e}", s.name)))?;
            // Validate that the endpoint parses and its auth header templates
            // resolve at startup, so an unreadable secret is a startup failure
            // rather than a surprise on first use.
            ::mcp::http::McpEndpoint::parse(&s.endpoint)
                .map_err(|e| usage(format!("mcp server '{}': {e}", s.name)))?;
            for (name, value) in &s.headers {
                if is_secret_shaped_key(name) && !crate::sec::secret::has_secret_ref(value) {
                    return Err(usage(format!(
                        "mcp server '{}' header '{name}' looks like a credential but has an inline value; use {{{{secret:NAME}}}} or {{{{secret-file:PATH}}}}",
                        s.name
                    )));
                }
            }
            crate::mcp::auth::headers_resolvable(&s.headers)
                .map_err(|e| usage(format!("mcp server '{}' header: {e}", s.name)))?;
        }
        if self.max_steps == 0 {
            return Err(usage("--max-steps must be > 0".into()));
        }
        // A zero events ring would hold nothing (every push instantly evicts) —
        // reject it so an operator who wants the live-tail surface gets a usable
        // window. Off by default; only consumed when serving.
        if self.events_ring == 0 {
            return Err(usage("--events-ring must be > 0".into()));
        }
        // The file-watch reload trigger (`--watch-config`) needs the
        // `config-watch` build feature. A silently-ignored `--watch-config`
        // would leave the operator believing a ConfigMap swap reloads the
        // daemon when in fact only SIGHUP does.
        if self.watch_config && !cfg!(feature = "config-watch") {
            return Err(usage(
                "--watch-config requires the 'config-watch' build feature".into(),
            ));
        }
        {
            #[cfg(feature = "workflow")]
            let wait_driven = self.workflow_file.is_some();
            #[cfg(not(feature = "workflow"))]
            let wait_driven = false;
            // A reactive WORKFLOW daemon's subscriptions come from its Wait nodes
            // dynamically — the workflow file stands in for a static --subscribe.
            if self.mode == Mode::Reactive
                && self.subscribe.is_empty()
                && self.continue_subscribe.is_empty()
                && !wait_driven
            {
                return Err(usage(
                    "--mode reactive requires at least one --subscribe or --continue <uri> (or --workflow on a workflow build)".into(),
                ));
            }
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
        // A pinned workflow run needs a workflow file, and the file needs
        // workflow mode: the two are inseparable, like --cron ⟺ --mode
        // schedule.
        #[cfg(feature = "workflow")]
        {
            if self.mode == Mode::Workflow && self.workflow_file.is_none() {
                return Err(usage("--mode workflow requires --workflow <file>".into()));
            }
            // Checkpoint resume is only meaningful for a pinned workflow run,
            // and the named checkpointer must be a configured server. Both
            // mistakes fail here, in milliseconds, before any network call.
            if let Some(r) = &self.workflow_resume {
                if r.server.is_empty() {
                    return Err(usage(
                        "--workflow-resume-force requires --workflow-resume <server>:<key>[@seq]"
                            .into(),
                    ));
                }
                if self.mode != Mode::Workflow {
                    return Err(usage(
                        "--workflow-resume is only valid with --mode workflow".into(),
                    ));
                }
                if !self.mcp_servers.iter().any(|s| s.name == r.server) {
                    return Err(usage(format!(
                        "--workflow-resume names server '{}', which is not a configured --mcp server",
                        r.server
                    )));
                }
            }
            if self.workflow_file.is_some()
                && self.mode != Mode::Workflow
                && self.mode != Mode::Reactive
            {
                return Err(usage(
                    "--workflow is only valid with --mode workflow or --mode reactive".into(),
                ));
            }
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
        // AAuth: the feature-gated flag must be present in the build, and the
        // provider must be a real http(s) URL — both exit 2 before any
        // network I/O (like every other feature/URL check).
        if let Some(a) = &self.aauth {
            if !cfg!(feature = "aauth") {
                return Err(usage(
                    "--aauth-provider needs a build with --features aauth".into(),
                ));
            }
            if crate::net::http::Url::parse(&a.provider).is_err() {
                return Err(usage(format!(
                    "--aauth-provider must be an http(s) URL (got: {})",
                    a.provider
                )));
            }
            if let Some(ps) = &a.person_server
                && crate::net::http::Url::parse(ps).is_err()
            {
                return Err(usage(format!(
                    "--aauth-person-server must be an http(s) URL (got: {ps})"
                )));
            }
        }
        // Validate the served-MCP target up front: a bad scheme, a missing
        // port, or a non-loopback plaintext bind exits 2 before any
        // listener is bound — mirroring the intelligence-URI check.
        if let Some(spec) = &self.serve_mcp {
            let target = ServeTarget::parse(spec)?;
            self.validate_serve_auth(&target, &|k: &str| std::env::var(k).ok())?;
        } else if self.serve_cert.is_some()
            || self.serve_key.is_some()
            || self.serve_client_ca.is_some()
            || self.serve_bearer.is_some()
        {
            return Err(usage(
                "a2a.tls.cert / a2a.tls.key / a2a.tls.client_ca / a2a.bearer require a2a.listen"
                    .into(),
            ));
        }
        // Outbound extra trust anchor (`--tls-ca`): needs the `tls` build feature
        // (a plaintext-only build has no dial to trust it on — silently ignoring
        // it would leave the operator believing the private CA is honored), and
        // the bundle must be present + a valid, addable CA PEM up front (exit 2,
        // not a first-dial surprise). Content check is side-effect-free here;
        // `main` installs the same bundle process-wide before the first dial.
        if let Some(ca) = &self.tls_ca {
            if !cfg!(feature = "tls") {
                return Err(usage("--tls-ca requires the 'tls' build feature".into()));
            }
            check_readable("--tls-ca", ca)?;
            #[cfg(feature = "tls")]
            {
                let pem =
                    std::fs::read(ca).map_err(|e| usage(format!("--tls-ca {ca}: read: {e}")))?;
                crate::net::tls::validate_ca_pem(&pem)
                    .map_err(|e| usage(format!("--tls-ca {ca}: {e}")))?;
            }
        }
        // Declared A2A delegation peers need the `a2a` build feature, and each
        // endpoint scheme is validated up front (exit 2 before
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
            // Peer client-auth (both legs) fails FAST at startup: header
            // templates must be secret-free + resolvable (bearer leg, same rule
            // as MCP servers), and mTLS material must come in a cert+key PAIR of
            // readable files (loaded at dial time, never inlined).
            for (name, value) in &peer.headers {
                if is_secret_shaped_key(name) && !crate::sec::secret::has_secret_ref(value) {
                    return Err(usage(format!(
                        "a2a peer '{}' header '{name}' looks like a credential but has an inline value; use {{{{secret:NAME}}}} or {{{{secret-file:PATH}}}}",
                        peer.name
                    )));
                }
            }
            crate::mcp::auth::headers_resolvable(&peer.headers)
                .map_err(|e| usage(format!("a2a peer '{}' header: {e}", peer.name)))?;
            match (&peer.client_cert, &peer.client_key) {
                (Some(_), None) | (None, Some(_)) => {
                    return Err(usage(format!(
                        "a2a peer '{}': client_cert and client_key must be set together",
                        peer.name
                    )));
                }
                (Some(cert), Some(key)) => {
                    for path in [cert, key] {
                        if let Err(e) = std::fs::metadata(path) {
                            return Err(usage(format!(
                                "a2a peer '{}': cannot read '{path}': {e}",
                                peer.name
                            )));
                        }
                    }
                }
                (None, None) => {}
            }
        }
        // Declared intelligence headers: reject an inline secret-shaped value,
        // and require every {{secret…}} ref to resolve. The `--validate-config`
        // path runs this same check through `collect_header_diags`, collecting
        // all of them, so the admission gate and startup never disagree.
        let mut header_diags = Vec::new();
        self.collect_header_diags(&mut header_diags);
        if let Some(first) = header_diags.into_iter().next() {
            return Err(usage(first));
        }
        // Rule of Two — the lethal-trifecta gate. It lives in `validate()` so
        // there is ONE validation authority: startup and `--validate-config`
        // share it and can never disagree. A grant co-locating all three legs
        // (untrusted input + sensitive data + egress) without `--allow-trifecta`
        // is refused as a config error (exit 2). The allowed-with-
        // `--allow-trifecta` case is NOT an error — it passes here, and the
        // supervisor (`main.rs`) emits the auditable `scope.trifecta_grant`
        // warning instead. A subagent's scope can only narrow, so the root
        // union bounds the whole subagent tree.
        if crate::sec::scope::check_trifecta(self.trifecta_grant_tags(), self.allow_trifecta)
            .is_refused()
        {
            return Err(usage(
                "refused — this grant gives one agent all three lethal-trifecta legs \
                 (untrusted input + sensitive data + egress). Split the capabilities across \
                 subagents, or relaunch with --allow-trifecta."
                    .into(),
            ));
        }
        Ok(())
    }
}

// ──────────────────────────────  hot reload  ────────────────────────────────
//
// The reloadable-vs-restart-only partition plus the coherence check that both
// the reload path and `--validate-config` run. This block is pure data and
// pure-CPU checks — no side effect, no subsystem touched; the apply step lives
// in `triggers::mode`. It compiles in every feature combination: the SIGHUP
// trigger and the reactive apply are `hot-reload`-gated, while the partition
// itself is always available, so `--validate-config` reports restart-only
// warnings on any build.

/// A reload diagnostic. `Warn` is advisory (a restart-only field
/// merely present in the file — it works, it just pins you to restart-to-change);
/// `Error` is fatal to the reload (it differs on a live reload, or the reloadable
/// subset is internally inconsistent). `--validate-config` reports both; the
/// reload path aborts on any `Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    /// The config field/path the diagnostic is about (e.g. `mode`, `mcp_servers`).
    pub field: String,
    /// `warn` (advisory) or `error` (fatal to the reload).
    pub level: DiagLevel,
    /// The human-readable reason.
    pub msg: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagLevel {
    Warn,
    Error,
}

impl Diag {
    // The warn-vs-reject distinction is part of the coherence-check contract:
    // a restart-only field merely *present in the file* is a Warn — it works,
    // it just pins you to restart-to-change — which is a different thing from a
    // field that DIFFERS on a live reload, an Error. The file schema exposes no
    // restart-only key at all, so the Warn path has no live caller; the
    // constructor stays so widening the file schema needs no new API.
    #[allow(dead_code)]
    fn warn(field: &str, msg: impl Into<String>) -> Diag {
        Diag {
            field: field.to_string(),
            level: DiagLevel::Warn,
            msg: msg.into(),
        }
    }
    fn error(field: &str, msg: impl Into<String>) -> Diag {
        Diag {
            field: field.to_string(),
            level: DiagLevel::Error,
            msg: msg.into(),
        }
    }
    pub fn is_error(&self) -> bool {
        self.level == DiagLevel::Error
    }
    pub fn level_str(&self) -> &'static str {
        match self.level {
            DiagLevel::Warn => "warn",
            DiagLevel::Error => "error",
        }
    }
}

/// The names of the **restart-only** fields. A live reload whose new-vs-running
/// diff touches ANY of these is rejected with `reason="restart_required"`;
/// whether that becomes a pod restart is agentctl's policy. They also drive the
/// "restart-only field set in the file" warning.
///
/// NB: `mcp_servers` is deliberately ABSENT because it is **reloadable**: a
/// validated reload re-handshakes the MCP server set at the quiesce boundary.
/// The name-keyed `servers`/`owner`/claim wiring in `triggers::mode` is what
/// makes that live re-handshake safe — a remove or add never shifts another
/// server's identity.
pub const RESTART_ONLY_FIELDS: &[&str] = &[
    "mode",
    // NB: `intelligence` (the endpoint list) and `model`/`model_swap` are
    // RELOADABLE through the runtime hot-swap primitive. A reload whose diff
    // repoints the endpoint list or changes the model is APPLIED at a turn
    // boundary — the supervisor fans `ctrl/swap_intel` to in-flight children —
    // rather than rejected, so they are deliberately absent from this list.
    // `mcp_servers` is likewise reloadable: re-handshaked, not rejected.
    "run_id",             // instance identity / idempotency key
    "serve_mcp",          // a live control socket must not rebind mid-flight
    "drain_timeout",      // validated against the pod grace at startup
    "continue_subscribe", // warm-session routing topology is restart-only
];

impl Config {
    /// Re-resolve config for a hot reload: re-read ONLY the file and re-merge
    /// built-in<file<env<flag. `args`/`env` are the process's
    /// original, fixed inputs — only the FILE can change between loads, so this
    /// keeps precedence correct (a flag still overrides the new file). Pure-CPU,
    /// no side effect. The returned `Config` is the fully-validated candidate; an
    /// invalid file/value is the same `ConfigError::Usage` startup would raise.
    ///
    /// NB: `--validate-config`/`--config-schema`/`--capabilities` short-circuit
    /// inside `load`, but those flags never reach a running reactive daemon, so a
    /// reload's `args` never carries them — this is the ordinary load path.
    pub fn reload(args: &[String], env: &[(String, String)]) -> Result<Config, ConfigError> {
        Config::load(args, env)
    }

    /// Advisory: a restart-only field set in the config FILE — "this field
    /// belongs in env/flag" — pushed as a `Warn`. The file schema exposes NO
    /// restart-only key: `mode`, `run_id` and `serve_mcp` are env/flag-only,
    /// and `mcp_servers`, the one structural field that could be mistaken for
    /// one, is RELOADABLE (a live re-handshake at the quiesce boundary). So
    /// there is nothing file-settable to warn about. The hook stays, consulting
    /// `file_present` — the gate a widened schema would use — so re-arming a
    /// warning needs no plumbing change.
    fn restart_only_file_warnings(&self, file_present: bool, _diags: &mut Vec<Diag>) {
        let _ = file_present; // the gate a widened file schema would use
    }

    /// The reload-coherence check, run by BOTH `--validate-config` and the
    /// reload path. Pure-CPU, no side effect.
    ///
    /// 1. (advisory) a restart-only field set in the FILE → `Warn` (`file_present`).
    /// 2. (live reload only) any restart-only field that DIFFERS between `new` and
    ///    `running` → `Error` naming the field, which aborts the reload.
    /// 3. the reloadable subset is internally consistent: every subscription/claim
    ///    references a declared server where required, and server names are unique.
    ///
    /// `Ok(())` if no `Error` diagnostics (the `Warn`s are still surfaced by the
    /// caller); `Err(diags)` carries every diagnostic when at least one is an error.
    pub fn reload_coherence_check(
        new: &Config,
        running: Option<&Config>,
        file_present: bool,
    ) -> Result<(), Vec<Diag>> {
        let mut diags = Vec::new();
        // 1. restart-only-field-in-file advisory warnings.
        new.restart_only_file_warnings(file_present, &mut diags);
        // 2. on a live reload, a restart-only diff is a hard reject.
        if let Some(run) = running {
            for &f in RESTART_ONLY_FIELDS {
                if new.restart_only_field_differs(run, f) {
                    diags.push(Diag::error(
                        f,
                        format!(
                            "restart-only field '{f}' changed on a live reload; reload refused, \
                             a pod restart is required"
                        ),
                    ));
                }
            }
        }
        // 3. reloadable-subset internal consistency.
        check_unique_server_names(new, &mut diags);
        check_subscriptions_reference_declared_servers(new, &mut diags);
        if diags.iter().any(Diag::is_error) {
            Err(diags)
        } else {
            // Surface advisory warnings to the caller too (it logs them) — an
            // all-warn result is still `Ok` (the reload proceeds; the warnings
            // are informational). The caller that wants the warnings reads them
            // via the validate-collect path; the reload path only needs the
            // pass/fail, so an Ok here means "no restart-only diff, apply".
            Ok(())
        }
    }

    /// Compare one restart-only field between `self` (new) and `running`. The
    /// match arms enumerate exactly [`RESTART_ONLY_FIELDS`] — a field added there
    /// without a comparison arm here defaults to `false` (no diff), which would
    /// silently let it reload, so the unit tests assert each named field is
    /// diff-detected. Pure.
    fn restart_only_field_differs(&self, running: &Config, field: &str) -> bool {
        match field {
            "mode" => self.mode != running.mode,
            "run_id" => self.run_id != running.run_id,
            "serve_mcp" => self.serve_mcp != running.serve_mcp,
            "drain_timeout" => self.drain_timeout != running.drain_timeout,
            "continue_subscribe" => self.continue_subscribe != running.continue_subscribe,
            _ => false,
        }
    }

    /// The reloadable, **redacted** view of the running config for
    /// `agentd://config/effective`. Carries ONLY the reloadable structural
    /// fields — no token, no URL, no secret, and header NAMES rather than
    /// values. Management-readable, and held to the same no-secret discipline
    /// as the manifest: nothing here can embed a credential.
    pub fn effective_view(&self) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "swap_policy": self.model_swap.as_str(),
            "max_tokens": self.max_tokens,
            "limits": {
                "max_steps": self.max_steps,
                "max_depth": self.max_depth,
                "deadline_secs": self.deadline.map(|d| d.as_secs()),
                // The per-instance lifetime budget; omitted when unbounded (0).
                "lifetime_tokens": (self.budget_tokens_lifetime > 0)
                    .then_some(self.budget_tokens_lifetime),
            },
            // Structural name + tags only — never the endpoint (its host/path can
            // be sensitive) nor the auth headers, mirroring the manifest.
            "mcp_servers": self.mcp_servers.iter().map(|s| {
                serde_json::json!({"name": s.name, "tags": s.tags})
            }).collect::<Vec<_>>(),
            "subscribe": self.subscribe,
            "log_level": self.log_level.as_str(),
            // Header NAMES only: a value may be a {{secret:…}} ref, and the
            // resolved value is never exposed on a readable surface.
            "intelligence_headers": self.intelligence_headers.keys().collect::<Vec<_>>(),
        })
    }
}

/// Check that declared MCP server names are unique. A duplicate would make the
/// name-keyed owner/claim map ambiguous, so it is an error.
fn check_unique_server_names(cfg: &Config, diags: &mut Vec<Diag>) {
    let mut seen = std::collections::HashSet::new();
    for s in &cfg.mcp_servers {
        if !seen.insert(s.name.as_str()) {
            diags.push(Diag::error(
                "mcp_servers",
                format!("duplicate MCP server name '{}'", s.name),
            ));
        }
    }
}

/// Check that every route that names an MCP server references a declared one.
/// This is the reload-time mirror of the startup `validate()` check: on a
/// reload the candidate must be self-consistent before any subsystem is
/// touched. Plain `--subscribe` URIs need no declared server — they bind to
/// whichever connected server supports them — so they are not checked here,
/// exactly as in `validate()`.
fn check_subscriptions_reference_declared_servers(cfg: &Config, diags: &mut Vec<Diag>) {
    // Plain `--subscribe` URIs bind to whichever connected server supports
    // them, so there is no server reference here to resolve. The hook stays
    // because this check is about the reloadable subset being self-consistent,
    // and a field that DOES name a server would belong here.
    let _ = (cfg, diags);
}

/// Heuristic: is this header name credential-shaped? A header so named must
/// carry a `{{secret:…}}` *reference*, never an inline literal, so a secret
/// cannot be smuggled into a config file under a plausible header name.
pub(crate) fn is_secret_shaped_key(name: &str) -> bool {
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

/// The single-line `config.valid` verdict, to stderr, exit 0.
fn config_valid_line() -> String {
    serde_json::json!({"event": "config.valid"}).to_string()
}

/// One machine-actionable `config.invalid` diagnostic line, to stderr, exit 2.
/// `msg` is the human-readable reason.
fn config_invalid_line(msg: &str) -> String {
    serde_json::json!({"event": "config.invalid", "msg": msg}).to_string()
}

/// Validate the `--intelligence` value as an ORDERED, comma-separated endpoint
/// list. At least one non-empty element is required, and every element's scheme
/// is validated — exit 2 naming the bad element. Checking every element, not
/// just the first, is the point: a transport this build cannot dial would
/// otherwise only be discovered at the moment of failover.
pub(crate) fn validate_intelligence_uri(uri: &str) -> Result<(), ConfigError> {
    let elements: Vec<&str> = uri
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if elements.is_empty() {
        return Err(usage(
            "missing intelligence endpoint (AGENTD_INTELLIGENCE or --intelligence)".into(),
        ));
    }
    for el in elements {
        validate_one_intelligence_uri(el)?;
    }
    Ok(())
}

/// Validate one endpoint URI's scheme. Intelligence is **HTTPS-only**:
/// `https://host[:port][/path]`, with plaintext `http://` admitted only for a
/// loopback host — the dev/test carve-out for the built-in mock LLM. Only the
/// *scheme shape* is the startup gate, and a bad scheme on any element is exit
/// 2. Whether this build can actually dial the transport (`https:` needs
/// `tls`) is left to the client, which reports `Unsupported` at dial time, so
/// a `--capabilities` or `--validate-config` probe of an https endpoint still
/// passes on a no-tls build.
fn validate_one_intelligence_uri(uri: &str) -> Result<(), ConfigError> {
    if uri.starts_with("https://") {
        return Ok(());
    }
    // `mock:<script>` — the offline dev endpoint (in-process mock LLM over
    // loopback). Admitted only where the client can actually serve it: debug
    // builds, or a release built `--features internal-mocks`.
    if uri.starts_with("mock:") {
        #[cfg(any(feature = "internal-mocks", debug_assertions))]
        return Ok(());
        #[cfg(not(any(feature = "internal-mocks", debug_assertions)))]
        return Err(usage(format!(
            "mock: intelligence needs a build with --features internal-mocks (got: {uri})"
        )));
    }
    if let Some(rest) = uri.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or(rest);
        // Split off the port: bracketed IPv6 keeps its brackets for the
        // loopback classifier; bare host:port loses the port.
        let host = if authority.starts_with('[') {
            authority.split(']').next().map_or(authority, |h| &h[1..])
        } else {
            authority.rsplit_once(':').map_or(authority, |(h, _)| h)
        };
        if crate::net::http::is_loopback_host(host) {
            return Ok(());
        }
        return Err(usage(format!(
            "plaintext http:// intelligence is allowed for loopback only (dev); use https:// (got: {uri})"
        )));
    }
    Err(usage(format!(
        "intelligence endpoint must be https://host[:port][/path] (got: {uri})"
    )))
}

/// Probe each listed endpoint's per-endpoint token *file* env var: a
/// `AGENTD_INTELLIGENCE_TOKEN[_N]_FILE` that is set but unreadable is
/// exit 2 before any side effect — we fail fast rather than discover a missing
/// secret on failover. Endpoint 1 (index 0) uses the bare name; later endpoints
/// are 1-indexed (`_2`, `_3`, …). The inline env wins over the file (so a set
/// inline var means the file is not consulted), matching the resolver. The
/// resolved bytes are dropped immediately and never logged.
fn validate_endpoint_token_files(uri: &str) -> Result<(), ConfigError> {
    let count = uri
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .count();
    for idx in 0..count {
        let (inline_var, file_var) = if idx == 0 {
            (
                "AGENTD_INTELLIGENCE_TOKEN".to_string(),
                "AGENTD_INTELLIGENCE_TOKEN_FILE".to_string(),
            )
        } else {
            let n = idx + 1;
            (
                format!("AGENTD_INTELLIGENCE_TOKEN_{n}"),
                format!("AGENTD_INTELLIGENCE_TOKEN_{n}_FILE"),
            )
        };
        // An inline override means the file is never consulted — skip the probe.
        if std::env::var(&inline_var).is_ok() {
            continue;
        }
        if let Ok(path) = std::env::var(&file_var) {
            crate::sec::secret::read_token_file(&path).map_err(usage)?;
        }
    }
    Ok(())
}

/// Parse `--mcp name=<endpoint>`. The value is a remote MCP endpoint
/// (`https://` / `http://`, Streamable HTTP) — the sole transport, since there
/// is no local process spawn. A non-endpoint value is rejected.
fn parse_mcp_spec(spec: &str) -> Result<McpServerSpec, ConfigError> {
    let (name, rhs) = spec
        .split_once('=')
        .ok_or_else(|| usage(format!("--mcp must be name=endpoint (got: {spec})")))?;
    let endpoint = rhs.trim();
    if name.is_empty() || endpoint.is_empty() {
        return Err(usage(format!("--mcp '{spec}' has empty name or endpoint")));
    }
    // `code` is RESERVED: workflow `tool` nodes address code-registered
    // (in-process, embedder-native) tools as server `code`, so a remote server
    // claiming the name would silently shadow them.
    if name == "code" {
        return Err(usage(
            "--mcp: the server name 'code' is reserved for code-registered tools".into(),
        ));
    }
    if !is_mcp_endpoint(endpoint) {
        return Err(usage(format!(
            "--mcp '{spec}': endpoint must be https://host[:port][/path] \
             (loopback http:// for dev)"
        )));
    }
    Ok(McpServerSpec {
        name: name.to_string(),
        endpoint: endpoint.to_string(),
        ..Default::default()
    })
}

/// Parse `--a2a-peer name=endpoint` into an [`A2aPeerSpec`]. The endpoint is
/// the remainder after the FIRST `=`, so a URL containing `=` in a query string
/// survives intact; the scheme itself is validated later in
/// [`Config::validate`] via [`A2aEndpoint::parse`].
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
        headers: Vec::new(),
        client_cert: None,
        client_key: None,
    })
}

/// Parse `--mcp-tags name=tag,tag` into (server-name, tags). Tags are the
/// snake-case capability legs of the lethal trifecta.
pub(crate) fn parse_mcp_tags(spec: &str) -> Result<(String, Vec<TrifectaTag>), ConfigError> {
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

pub(crate) fn read_file(path: &str) -> Result<String, ConfigError> {
    std::fs::read_to_string(path)
        .map_err(|e| usage(format!("cannot read instruction file {path}: {e}")))
}

/// How one argument spells the config-file flag. `--config` is the canonical
/// form; `-c` is the short alias, and either may attach its value with `=`
/// (`-c=a.yaml`, `--config=a.yaml`) as well as separate it with a space.
pub(crate) enum ConfigFlag<'a> {
    /// `--config a.yaml` / `-c a.yaml` — the value is the NEXT argument.
    Separate,
    /// `--config=a.yaml` / `-c=a.yaml` — the value is attached.
    Inline(&'a str),
    /// Not the config flag at all.
    No,
}

/// Classify one argument as a spelling of the config-file flag.
pub(crate) fn config_flag(arg: &str) -> ConfigFlag<'_> {
    match arg {
        "--config" | "-c" => ConfigFlag::Separate,
        _ => match arg
            .strip_prefix("--config=")
            .or_else(|| arg.strip_prefix("-c="))
        {
            Some(v) => ConfigFlag::Inline(v),
            None => ConfigFlag::No,
        },
    }
}

/// The config files an invocation will load, and **how they were chosen**.
///
/// The provenance is not a detail: a file the operator NAMED (`--config` /
/// `AGENTD_CONFIG`) is a decision they made, while a DISCOVERED `.agentd.yml`
/// is a file that happened to be in the working directory when they typed a
/// flags-only command. The two get different trust (see the discovered-config
/// containment in `config::v2::load`), so the loader must be able to tell them
/// apart rather than seeing one flat list of paths.
pub(crate) struct ConfigPaths {
    /// The files to load, in merge order (earlier is overridden by later).
    pub paths: Vec<String>,
    /// True when `paths` came from DISCOVERY — nothing named a config, so a
    /// dotfile in the working directory was adopted. Never true alongside a
    /// named path: discovery is a fallback for an empty list.
    pub discovered: bool,
}

/// The ordered config-file list over an already-debranded env map: the
/// `AGENTD_CONFIG` entries (`:`-separated, empty entries skipped) then each
/// `--config` value. Env first so a platform-injected base is overridden by an
/// operator's explicit `--config` overlay (later wins).
pub(crate) fn config_paths_from_map(args: &[String], envmap: &HashMap<&str, &str>) -> ConfigPaths {
    let mut paths: Vec<String> = envmap
        .get("AGENTD_CONFIG")
        .map(|v| {
            v.split(':')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match config_flag(a) {
            ConfigFlag::Separate => {
                if let Some(v) = it.next() {
                    paths.push(v.clone());
                }
            }
            ConfigFlag::Inline(v) => paths.push(v.to_string()),
            ConfigFlag::No => {}
        }
    }
    // Nothing named a config, so look for the project's own: `.agentd.yml` in
    // the working directory, the way a linter or a formatter picks up its
    // dotfile. Only ever a fallback — an explicit `--config` or `AGENTD_CONFIG`
    // means the caller has already decided.
    let mut discovered = false;
    if paths.is_empty() && !is_informational(args) {
        paths.extend(discovered_config_in(Path::new(".")));
        discovered = !paths.is_empty();
    }
    ConfigPaths { paths, discovered }
}

/// The file names agentd looks for when an invocation names no config.
///
/// Two spellings because `.yml` and `.yaml` are both idiomatic and guessing
/// wrong should not mean silence — a config file the tool ignores is the worst
/// outcome of the three.
pub const DISCOVERED_CONFIG_NAMES: [&str; 2] = [".agentd.yml", ".agentd.yaml"];

/// Which of [`DISCOVERED_CONFIG_NAMES`] exist in `dir`, in order.
///
/// Returns **all** matches rather than the first, so that two present at once
/// surfaces as an error at load rather than a silent pick between them. Callers
/// that get more than one refuse to start.
pub fn discovered_config_in(dir: &Path) -> Vec<String> {
    DISCOVERED_CONFIG_NAMES
        .iter()
        .map(|n| dir.join(n))
        .filter(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Whether this invocation only wants to print something.
///
/// `--help` and `--version` must work in any directory. Discovering a config
/// for them would mean a stray `.agentd.yml` two levels of `cd` away could make
/// `agentd --help` fail, which is an unreasonable way to learn a file is
/// malformed.
fn is_informational(args: &[String]) -> bool {
    args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-h" | "--help"
                | "-V"
                | "--version"
                | "--config-schema"
                | "--config-schema=2"
                | "--workflow-schema"
        )
    })
}

/// Type a config DOCUMENT (a merged file set, the env-path layer, or one
/// `--<path>` flag) and overlay it onto `c`. `replace_lists` selects the
/// list/map semantics for the top-level keys the document actually carries:
/// `false` = **add** to what is there (the file layer, the named repeatable
/// flags' semantics; also a `--<map>.<key>` entry flag, which merges one key);
/// `true` = **set** — the document's value replaces the list/map (setting a
/// path from env or a `--<path>` flag). Keys absent from the document are never
/// touched either way.
fn apply_document(
    c: &mut Config,
    doc: serde_json::Value,
    source: &str,
    replace_lists: bool,
) -> Result<(), ConfigError> {
    let present: Vec<String> = doc
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let cf = file::ConfigFile::from_document(doc, source).map_err(usage)?;
    if replace_lists {
        for key in &present {
            match key.as_str() {
                "mcp_servers" => c.mcp_servers.clear(),
                "subscribe" => c.subscribe.clear(),
                "a2a_peers" => c.a2a_peers.clear(),
                "intelligence_headers" => c.intelligence_headers.clear(),
                _ => {}
            }
        }
    }
    apply_config_file(c, cf, source)
}

/// Overlay a typed [`file::ConfigFile`] onto `c` — the ONE overlay operation
/// every document layer uses: the config FILE (precedence layer 1), the
/// path-derived env layer, and each generic `--<path>` flag. Only keys
/// the document actually sets are written (field-wise); later layers override
/// them. List-valued keys (`mcp_servers`, `subscribe`, `a2a_peers`, the header
/// maps) **add to** the list — repeatable-flag semantics for every layer. Maps
/// the file's `endpoint`+`headers` into the runtime `McpServerSpec`, and
/// flattens the glob→tags map to the server's tag set. `source` names the layer
/// in error messages (`config file`, `env`, or the flag).
fn apply_config_file(
    c: &mut Config,
    cf: file::ConfigFile,
    source: &str,
) -> Result<(), ConfigError> {
    // The intelligence endpoint LIST is file-settable and reloadable, so a
    // ConfigMap repoint is a hot swap. The transport scheme is data; the
    // credential is NEVER inline here — env or `_FILE` only — and the validate
    // pass rejects a secret-shaped value just as it does for headers.
    if let Some(intelligence) = cf.intelligence {
        c.intelligence = Some(intelligence);
    }
    if let Some(policy) = cf.model_swap {
        c.model_swap = SwapPolicy::parse(&policy).ok_or_else(|| {
            usage(format!(
                "{source}: invalid model_swap: {policy} (want finish-on-old|restart-turn)"
            ))
        })?;
    }
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
        if let Some(lt) = limits.lifetime_tokens {
            c.budget_tokens_lifetime = lt;
        }
    }
    if let Some(level) = cf.log_level {
        c.log_level = Level::parse(&level)
            .ok_or_else(|| usage(format!("{source}: invalid log_level: {level}")))?;
    }
    // mcp_servers: each file object → one McpServerSpec over the HTTP
    // transport: a remote `endpoint` plus secret-free header templates, with no
    // local process spawn. The glob→tags map flattens to the union of declared
    // tags. Seeds the list.
    for s in cf.mcp_servers {
        if s.name.is_empty() {
            return Err(usage(format!("{source}: an mcp server has an empty name")));
        }
        let endpoint = match s.endpoint {
            Some(ep) if !ep.trim().is_empty() => ep,
            _ => {
                return Err(usage(format!(
                    "{source}: mcp server '{}' has no endpoint \
                     (an MCP server is always a remote endpoint)",
                    s.name
                )));
            }
        };
        let headers = s.headers.into_iter().collect::<Vec<(String, String)>>();
        let mut tags: Vec<TrifectaTag> = Vec::new();
        for tag_list in s.tags.values() {
            for t in tag_list {
                let tag = TrifectaTag::parse(t).ok_or_else(|| {
                    usage(format!(
                        "{source}: mcp server '{}' has unknown trifecta tag '{t}' \
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
            endpoint,
            headers,
            tags,
            aauth: s.aauth,
            // OAuth client-credentials + the unified `auth:` block + the
            // service catalog are settings-document-only surfaces; this flat
            // config path does not carry them.
            oauth: None,
            auth: None,
            service: None,
            rate: None,
        });
    }
    c.subscribe.extend(cf.subscribe);
    for p in cf.a2a_peers {
        if p.name.is_empty() || p.endpoint.is_empty() {
            return Err(usage(format!(
                "{source}: a2a peer '{}' has an empty name or endpoint",
                p.name
            )));
        }
        c.a2a_peers.push(A2aPeerSpec {
            name: p.name,
            endpoint: p.endpoint,
            headers: p.headers.into_iter().collect(),
            client_cert: p.client_cert,
            client_key: p.client_key,
        });
    }
    // Declared intelligence headers (templates; secret-shaped values validated).
    c.intelligence_headers.extend(cf.intelligence_headers);
    Ok(())
}

/// Parse `--workflow-resume <server>:<key>[@seq]`. The server
/// is the configured `--mcp` name of the checkpointer; the key identifies the
/// state lineage (`{run_id}` interpolates later); `@seq` pins a specific
/// envelope (fork/time-travel) — latest when absent.
#[cfg(feature = "workflow")]
fn parse_workflow_resume(
    spec: &str,
) -> Result<crate::subagent::protocol::WorkflowResumeRef, ConfigError> {
    let (server, rest) = spec.split_once(':').ok_or_else(|| {
        usage(format!(
            "--workflow-resume: want <server>:<key>[@seq] (got: {spec})"
        ))
    })?;
    let (key, seq) = match rest.rsplit_once('@') {
        Some((k, s)) => {
            let seq: u64 = s
                .parse()
                .map_err(|_| usage(format!("--workflow-resume: bad @seq {s:?} (want a number)")))?;
            (k, Some(seq))
        }
        None => (rest, None),
    };
    if server.trim().is_empty() || key.trim().is_empty() {
        return Err(usage(format!(
            "--workflow-resume: server and key must be non-empty (got: {spec})"
        )));
    }
    Ok(crate::subagent::protocol::WorkflowResumeRef {
        server: server.to_string(),
        key: key.to_string(),
        seq,
        force: false,
    })
}

pub(crate) fn usage(msg: String) -> ConfigError {
    ConfigError::Usage(format!("agentd: {msg}"))
}

pub(crate) fn truthy(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Parse `600s`, `5m`, `2h`, `30d`, `2w`, `500ms`, or a bare integer
/// (seconds). Days and weeks exist because retention, dunning, and cadence
/// windows are naturally written in them — `30d` reads, `720h` gets checked
/// with a calculator.
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
        "d" => Duration::from_secs(n * 86_400),
        "w" => Duration::from_secs(n * 604_800),
        other => return Err(format!("unknown duration unit '{other}' in {s}")),
    };
    Ok(d)
}

/// A unique-enough run id for the default case (time + pid). The operator
/// overrides it with `--run-id` / `AGENTD_RUN_ID` when a retry must be
/// idempotent — the run id is the idempotency key, so a retry that wants to be
/// recognised as the same run must reuse it.
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
         \x20 agentd --instruction <TEXT> --intelligence <URI> [--mcp name=endpoint ...] [options]\n\
         \n\
         REQUIRED:\n\
         \x20 --instruction <TEXT>        the task (or INSTRUCTION / AGENT_INSTRUCTION env)\n\
         \x20 --instruction-file <PATH>   read the instruction from a file\n\
         \x20 --intelligence <URI>        https://host[:port][/path] (comma-list = failover order; http:// loopback-only for dev; or INTELLIGENCE / AGENT_INTELLIGENCE env)\n\
         \n\
         INTELLIGENCE:\n\
         \x20 --intelligence-token <T>    bearer/key (or AGENT_INTELLIGENCE_TOKEN)\n\
         \x20 --intelligence-token-file <PATH>  read the token from a mounted file (rotation; or AGENT_INTELLIGENCE_TOKEN_FILE)\n\
         \x20 --model <NAME>              model id (or AGENT_MODEL)\n\
         \x20 --model-swap <finish-on-old|restart-turn>  in-flight model-change policy (default finish-on-old; or AGENT_MODEL_SWAP)\n\
         \n\
         TOOLS / MCP:\n\
         \x20 --mcp name=endpoint         declare a remote MCP server (repeatable; https://host[:port][/path])\n\
         \x20 --tls-ca <PATH>             extra PEM CA(s) trusted for outbound https (private/in-cluster PKI; added to the bundled roots)\n\
         \x20 --aauth-provider <URL>      [DRAFT] Agent Provider — sign every MCP request with an Ed25519 agent identity (needs --features aauth; or AGENT_AAUTH_PROVIDER)\n\
         \x20 --aauth-key-file <PATH>     durable Ed25519 key file (created 0600 if absent; default agent.key; or AGENT_AAUTH_KEY_FILE)\n\
         \x20 --aauth-enroll-token <T>    one-time enrollment token ({{secret:…}}; provider `token` mode; or AGENT_AAUTH_ENROLL_TOKEN)\n\
         \x20 --aauth-enroll-assertion-file <PATH>  enrollment assertion file — e.g. a projected K8s SA token (provider `federated` mode; re-read each enroll; or AGENT_AAUTH_ENROLL_ASSERTION_FILE)\n\
         \x20 --aauth-person-server <URL> [DRAFT] Person Server for user-scoped identity (Case C; or AGENT_AAUTH_PERSON_SERVER)\n\
         \x20 --serve-mcp <TARGET>        serve agentd's own MCP over HTTP(S): https://host:port (or loopback http:// for dev)\n\
         \x20 --a2a-peer name=<ENDPOINT>  declare a remote A2A delegation peer: https://host[:port] (repeatable; needs --features a2a)\n\
         \x20 --mcp-tags name=t,t         capability tags: untrusted_input|sensitive|egress\n\
         \x20 --allow-trifecta            permit all three capability legs in one agent\n\
         \n\
         MODE / TRIGGERS:\n\
         \x20 --mode once|loop|reactive|schedule|workflow   (default once)\n\
         \x20 --workflow <FILE>           pinned workflow JSON, driven by --mode workflow (needs --features workflow; or AGENT_WORKFLOW)\n\
         \x20 --workflow-resume <REF>     resume from a checkpoint: <server>:<key>[@seq] (needs --mode workflow; or AGENT_WORKFLOW_RESUME)\n\
         \x20 --workflow-resume-force     override the workflow-hash check (graph-edit-and-continue)\n\
         \x20 --subscribe <uri>           subscribe to an MCP resource (repeatable)\n\
         \x20 --continue <uri>            subscribe, routed to one warm session (repeatable)\n\
         \x20 --interval <dur>            loop/schedule interval (e.g. 5m)\n\
         \x20 --cron <5-field>           schedule on a UTC cron expr (needs --features cron)\n\
         \n\
         LIMITS:\n\
         \x20 --max-steps <N>             per-run step cap (default 50)\n\
         \x20 --max-tokens <N>            per-run token budget (default 200000)\n\
         \x20 --budget-tokens-lifetime <N>  per-INSTANCE cumulative token cap across all runs/reactions (0/unset = unbounded; or AGENT_BUDGET_TOKENS)\n\
         \x20 --deadline <dur>            wall-clock deadline (default 600s)\n\
         \x20 --max-depth <N>             subagent tree depth cap (default 4)\n\
         \n\
         RUNTIME:\n\
         \x20 --run-id <ID>               idempotency key (or AGENT_RUN_ID)\n\
         \x20 --log-level <L>             trace|debug|info|warn|error (default info)\n\
         \x20 --log-content               log tool args/results, not just lengths (opt-in)\n\
         \x20 --drain-timeout <dur>       graceful drain budget (default 25s; < pod grace)\n\
         \x20 --health-file <PATH>        liveness heartbeat file\n\
         \x20 --metrics-addr <host:port>  serve /metrics+/healthz+/readyz (`:port` = all IPv4 ifaces; needs --features metrics)\n\
         \x20 --cgroup <auto|PATH>        per-run cgroup for atomic cgroup.kill teardown (best-effort)\n\
         \x20 --cgroup-memory-max <SIZE>  per-run memory.max (max|512M|2G|bytes; needs --cgroup + delegation)\n\
         \x20 --cgroup-pids-max <N>       per-run pids.max (max|count of THREADS; needs --cgroup + delegation)\n\
         \x20 --traceparent <W3C>         continue an upstream trace (or AGENT_TRACEPARENT)\n\
         \x20 --report-file <PATH>        write the run-outcome report at terminal (atomic; inert for reactive)\n\
         \x20 --budget-exit-code <N>      remap the policy budget codes (3/7 only) to N at process exit (0..=255)\n\
         \x20 --events-ring <N>           agent://events ring size (default 1024; needs --serve-mcp + --features events)\n\
         \x20 --capabilities             print the capabilities manifest (JSON) and exit\n\
         \n\
         CONFIG FILE:\n\
         \x20 --config <PATH>             load a config file, YAML or JSON; repeatable — later files override earlier ones (or AGENT_CONFIG=a.yaml:b.yaml)\n\
         \x20 --validate-config          load+validate (file+env+flags), print the verdict, exit 0/2\n\
         \x20 --config-schema            print the config-file JSON Schema and exit\n\
         \x20 --watch-config             reload on config-file change via inotify (needs --config + --features config-watch; or AGENT_WATCH_CONFIG)\n\
         \x20 -h, --help / -V, --version\n\
         \n\
         {paths}",
        ver = crate::VERSION,
        paths = paths::help_section(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn config_flag_accepts_short_and_inline_spellings() {
        // `--config a.yaml`, `-c a.yaml`, and either with the value attached by
        // `=` all name the same file layer, in argument order.
        let env: Vec<(String, String)> = vec![];
        for spelling in [
            args(&["--config", "a.yaml"]),
            args(&["-c", "a.yaml"]),
            args(&["--config=a.yaml"]),
            args(&["-c=a.yaml"]),
        ] {
            assert_eq!(
                Config::config_paths_from(&spelling, &env),
                vec!["a.yaml".to_string()],
                "spelling {spelling:?}"
            );
        }
        // Mixed spellings merge in order (later wins downstream).
        assert_eq!(
            Config::config_paths_from(&args(&["-c", "base.yaml", "--config=over.yaml"]), &env),
            vec!["base.yaml".to_string(), "over.yaml".to_string()]
        );
        // The env layer comes first, then the flags.
        assert_eq!(
            Config::config_paths_from(
                &args(&["-c=flag.yaml"]),
                &[("AGENTD_CONFIG".into(), "env.yaml".into())]
            ),
            vec!["env.yaml".to_string(), "flag.yaml".to_string()]
        );
        // A bare `-c` with nothing after it contributes no path (and the arg
        // loop reports the usage error).
        assert!(Config::config_paths_from(&args(&["-c"]), &env).is_empty());
        // Not the config flag: neither a different flag nor a lookalike value.
        assert!(Config::config_paths_from(&args(&["--cluster-shard", "a"]), &env).is_empty());
    }

    /// `.agentd.yml` in the working directory, picked up when the invocation
    /// named no config — and never when it did.
    #[test]
    fn a_dotfile_is_discovered_only_when_nothing_else_named_a_config() {
        let dir = std::env::temp_dir().join(format!("agentd-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing there yet.
        assert!(discovered_config_in(&dir).is_empty());

        std::fs::write(dir.join(".agentd.yml"), "config_version: \"1\"\n").unwrap();
        let found = discovered_config_in(&dir);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with(".agentd.yml"), "{found:?}");

        // Both spellings are reported, so the caller can refuse rather than
        // silently pick one: whichever it chose, somebody would be editing the
        // other and wondering why nothing changed.
        std::fs::write(dir.join(".agentd.yaml"), "config_version: \"1\"\n").unwrap();
        assert_eq!(discovered_config_in(&dir).len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The informational invocations must work in any directory — a stray
    /// dotfile must not be able to break `--help`.
    #[test]
    fn help_and_version_do_not_discover_a_config() {
        for a in [
            "--help",
            "-h",
            "--version",
            "-V",
            "--config-schema",
            "--workflow-schema",
        ] {
            assert!(is_informational(&args(&[a])), "{a} should be informational");
        }
        assert!(!is_informational(&args(&["--validate-config"])));
        assert!(!is_informational(&args(&[])));
    }

    #[test]
    fn flags_override_env() {
        let env = vec![
            ("AGENTD_INTELLIGENCE".into(), "https://intel.example".into()),
            ("INSTRUCTION".into(), "from-env".into()),
        ];
        let c = Config::load(&args(&["--instruction", "from-flag"]), &env).unwrap();
        assert_eq!(c.instruction.as_deref(), Some("from-flag"));
        assert_eq!(c.intelligence.as_deref(), Some("https://intel.example"));
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_mode_and_workflow_file_are_inseparable() {
        let intel_only = vec![(
            "AGENTD_INTELLIGENCE".to_string(),
            "https://intel.example".to_string(),
        )];
        // --mode workflow without --workflow → usage error.
        let e = Config::load(
            &args(&["--mode", "workflow", "--instruction", "x"]),
            &intel_only,
        )
        .unwrap_err();
        assert!(
            format!("{e}").contains("--mode workflow requires --workflow"),
            "{e}"
        );
        // --workflow without --mode workflow → usage error.
        let e = Config::load(&args(&["--workflow", "/tmp/g.json"]), &base_env()).unwrap_err();
        assert!(format!("{e}").contains("--workflow is only valid"), "{e}");
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn a_reactive_workflow_daemon_needs_no_subscribe_or_instruction() {
        // The workflow's Wait nodes ARE the subscriptions, and its nodes carry
        // the work — `--mode reactive --workflow <file>` stands alone.
        let c = Config::load(
            &args(&["--mode", "reactive", "--workflow", "/tmp/wf.json"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.mode, Mode::Reactive);
        assert_eq!(c.workflow_file.as_deref(), Some("/tmp/wf.json"));
        // A plain reactive daemon still requires a subscription.
        let e = Config::load(&args(&["--mode", "reactive"]), &base_env()).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
        // And --workflow still refuses the modes it means nothing in.
        let e = Config::load(
            &args(&[
                "--mode",
                "loop",
                "--interval",
                "5m",
                "--workflow",
                "/tmp/wf.json",
                "--instruction",
                "x",
            ]),
            &base_env(),
        )
        .unwrap_err();
        assert!(format!("{e}").contains("--workflow is only valid"), "{e}");
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn a_reactive_workflow_with_subscriptions_still_requires_an_instruction() {
        // Subscription routes spawn instruction reactions — a blank task is a
        // wiring mistake even when a workflow also rides the daemon.
        let intel_only = vec![(
            "AGENTD_INTELLIGENCE".to_string(),
            "https://intel.example".to_string(),
        )];
        let e = Config::load(
            &args(&[
                "--mode",
                "reactive",
                "--workflow",
                "/tmp/wf.json",
                "--subscribe",
                "file:///inbox",
            ]),
            &intel_only,
        )
        .unwrap_err();
        assert!(format!("{e}").contains("missing instruction"), "{e}");
        // A PURE workflow daemon (no routes) still needs none.
        let c = Config::load(
            &args(&["--mode", "reactive", "--workflow", "/tmp/wf.json"]),
            &intel_only,
        )
        .unwrap();
        assert!(c.instruction.as_deref().unwrap_or("").is_empty());
        // With an instruction the combo is fine.
        let c = Config::load(
            &args(&[
                "--mode",
                "reactive",
                "--workflow",
                "/tmp/wf.json",
                "--subscribe",
                "file:///inbox",
                "--instruction",
                "triage it",
            ]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.subscribe.len(), 1);
        assert!(c.workflow_file.is_some());
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_mode_does_not_require_an_instruction() {
        // The workflow carries its instructions, so `--mode workflow` needs no
        // `--instruction` (it still needs intelligence, for the Agent nodes).
        let intel_only = vec![(
            "AGENTD_INTELLIGENCE".to_string(),
            "https://intel.example".to_string(),
        )];
        let c = Config::load(
            &args(&["--mode", "workflow", "--workflow", "/tmp/g.json"]),
            &intel_only,
        )
        .unwrap();
        assert_eq!(c.mode, Mode::Workflow);
        assert_eq!(c.workflow_file.as_deref(), Some("/tmp/g.json"));
        assert!(c.instruction.as_deref().unwrap_or("").is_empty());
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_resume_parses_and_validates() {
        let intel_only = vec![(
            "AGENTD_INTELLIGENCE".to_string(),
            "https://intel.example".to_string(),
        )];
        // Full form, with a configured checkpointer server: server:key@seq.
        let c = Config::load(
            &args(&[
                "--mode",
                "workflow",
                "--workflow",
                "/tmp/g.json",
                "--mcp",
                "state=https://ckpt.internal/mcp",
                "--workflow-resume",
                "state:run/abc@17",
                "--workflow-resume-force",
            ]),
            &intel_only,
        )
        .unwrap();
        let r = c.workflow_resume.expect("parsed");
        assert_eq!(r.server, "state");
        assert_eq!(r.key, "run/abc");
        assert_eq!(r.seq, Some(17));
        assert!(r.force);

        // Force is order-independent (force first, then the ref).
        let c = Config::load(
            &args(&[
                "--mode",
                "workflow",
                "--workflow",
                "/tmp/g.json",
                "--mcp",
                "state=https://ckpt.internal/mcp",
                "--workflow-resume-force",
                "--workflow-resume",
                "state:run/abc",
            ]),
            &intel_only,
        )
        .unwrap();
        assert!(c.workflow_resume.unwrap().force);

        // Misconfigs are exit-2-shaped errors, pre-network: bad spec, force
        // without a ref, a non-workflow mode, an unconfigured server name.
        for bad in [
            vec![
                "--mode",
                "workflow",
                "--workflow",
                "/g",
                "--workflow-resume",
                "nocolon",
            ],
            vec![
                "--mode",
                "workflow",
                "--workflow",
                "/g",
                "--workflow-resume-force",
            ],
            vec!["--instruction", "x", "--workflow-resume", "s:k"],
            vec![
                "--mode",
                "workflow",
                "--workflow",
                "/g",
                "--workflow-resume",
                "ghost:k",
            ],
        ] {
            assert!(
                Config::load(&args(&bad), &base_env()).is_err(),
                "{bad:?} must be refused"
            );
        }
        // env spelling works too.
        let mut env = intel_only.clone();
        env.push(("AGENT_WORKFLOW_RESUME".into(), "state:run/xyz".into()));
        let c = Config::load(
            &args(&[
                "--mode",
                "workflow",
                "--workflow",
                "/g",
                "--mcp",
                "state=https://ckpt.internal/mcp",
            ]),
            &env,
        )
        .unwrap();
        assert_eq!(c.workflow_resume.unwrap().key, "run/xyz");
    }

    fn base_env() -> Vec<(String, String)> {
        vec![
            ("INSTRUCTION".into(), "x".into()),
            ("AGENTD_INTELLIGENCE".into(), "https://intel.example".into()),
        ]
    }

    #[test]
    fn neutral_agent_env_prefix_is_accepted_as_an_alias() {
        // The neutral `AGENT_*` prefix is accepted on input wherever the
        // branded `AGENTD_*` one is, through one envmap normalization.
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            (
                "AGENT_INTELLIGENCE".into(),
                "https://neutral.example".into(),
            ),
            ("AGENT_RUN_ID".into(), "run-neutral".into()),
            ("AGENT_MAX_STEPS".into(), "42".into()),
        ];
        let c = Config::load(&args(&[]), &env).unwrap();
        assert_eq!(c.intelligence.as_deref(), Some("https://neutral.example"));
        assert_eq!(c.run_id, "run-neutral");
        assert_eq!(c.max_steps, 42);
    }

    #[test]
    fn branded_env_wins_over_neutral_on_conflict() {
        // Both spellings present ⇒ the branded `AGENTD_*` value wins (back-compat),
        // and the branded-only path still works (neutral merely also accepted).
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            (
                "AGENTD_INTELLIGENCE".into(),
                "https://branded.example".into(),
            ),
            (
                "AGENT_INTELLIGENCE".into(),
                "https://neutral.example".into(),
            ),
        ];
        let c = Config::load(&args(&[]), &env).unwrap();
        assert_eq!(c.intelligence.as_deref(), Some("https://branded.example"));
    }

    #[test]
    fn bare_env_spellings_work_for_the_two_required_inputs() {
        // The bare `INTELLIGENCE` mirrors the bare `INSTRUCTION`: the minimal
        // quickstart is `INSTRUCTION=… INTELLIGENCE=… agentd` with no prefix.
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            ("INTELLIGENCE".into(), "https://bare.example".into()),
        ];
        let c = Config::load(&args(&[]), &env).unwrap();
        assert_eq!(c.intelligence.as_deref(), Some("https://bare.example"));
    }

    #[test]
    fn prefixed_env_wins_over_the_bare_spelling() {
        // Specificity order within the env layer: branded > neutral > bare.
        // The neutral AGENT_* forms (debranded to AGENTD_*) beat the bare
        // aliases, for BOTH required inputs — AGENT_INSTRUCTION included.
        let env = vec![
            ("INSTRUCTION".into(), "bare-task".into()),
            ("AGENT_INSTRUCTION".into(), "neutral-task".into()),
            ("INTELLIGENCE".into(), "https://bare.example".into()),
            (
                "AGENT_INTELLIGENCE".into(),
                "https://neutral.example".into(),
            ),
        ];
        let c = Config::load(&args(&[]), &env).unwrap();
        assert_eq!(c.instruction.as_deref(), Some("neutral-task"));
        assert_eq!(c.intelligence.as_deref(), Some("https://neutral.example"));
    }

    #[test]
    fn debrand_env_synthesizes_branded_only_when_absent() {
        // Unit-level: a neutral key without a branded counterpart gets a synthesized
        // branded entry; a present branded key is left untouched (branded wins).
        let env = vec![
            ("AGENT_MODE".into(), "loop".into()),
            ("AGENTD_RUN_ID".into(), "kept".into()),
            ("AGENT_RUN_ID".into(), "ignored".into()),
            ("INSTRUCTION".into(), "x".into()),
        ];
        let out = debrand_env(&env);
        let get = |k: &str| {
            out.iter()
                .filter(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>()
        };
        // Neutral-only AGENT_MODE → synthesized AGENTD_MODE.
        assert_eq!(get("AGENTD_MODE"), vec!["loop"]);
        // Branded present → not overwritten by the neutral form.
        assert_eq!(get("AGENTD_RUN_ID"), vec!["kept"]);
        // Non-prefixed keys are passed through unchanged.
        assert_eq!(get("INSTRUCTION"), vec!["x"]);
    }

    #[test]
    fn report_file_and_events_ring_parse_from_flag_and_env() {
        // Default: off, with the 1024-entry ring.
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
    fn budget_exit_code_flag_parses_and_range_checks() {
        // Default: no remap (the canonical table applies).
        let c = Config::load(&args(&[]), &base_env()).unwrap();
        assert_eq!(c.budget_exit_code, None);
        // A valid POSIX exit byte is accepted.
        let c = Config::load(&args(&["--budget-exit-code", "0"]), &base_env()).unwrap();
        assert_eq!(c.budget_exit_code, Some(0));
        let c = Config::load(&args(&["--budget-exit-code", "42"]), &base_env()).unwrap();
        assert_eq!(c.budget_exit_code, Some(42));
        // Out of the 0..=255 byte range, or non-numeric ⇒ EXIT_USAGE (2).
        for bad in ["256", "-1", "nope"] {
            let e = Config::load(&args(&["--budget-exit-code", bad]), &base_env()).unwrap_err();
            assert!(
                matches!(e, ConfigError::Usage(_)),
                "{bad} must be a usage error"
            );
        }
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
            &args(&[
                "--mcp-tags",
                "fs=sensitive,egress",
                "--mcp",
                "fs=https://fs.example",
            ]),
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
            &args(&[
                "--mcp",
                "fs=https://fs.example",
                "--mcp-tags",
                "ghost=egress",
            ]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(bad_server, ConfigError::Usage(_)));
        let bad_tag = Config::load(
            &args(&["--mcp", "fs=https://fs.example", "--mcp-tags", "fs=bogus"]),
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
    fn trifecta_grant_tags_defaults_untagged_to_untrusted() {
        let c = Config::load(&args(&["--mcp", "fs=https://fs.example"]), &base_env()).unwrap();
        let tags = c.trifecta_grant_tags();
        assert!(tags.contains(&TrifectaTag::UntrustedInput)); // untagged server
        assert!(!tags.contains(&TrifectaTag::Sensitive)); // one leg → not a trifecta
    }

    #[test]
    fn missing_instruction_is_usage_error() {
        let env = vec![("AGENTD_INTELLIGENCE".into(), "https://intel.example".into())];
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
            ("AGENTD_INTELLIGENCE".into(), "https://intel.example".into()),
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
            ("AGENTD_INTELLIGENCE".into(), "https://intel.example".into()),
        ];
        // A `--mcp name=<endpoint>` is the Streamable HTTP transport (the only one).
        let c = Config::load(&args(&["--mcp", "fs=https://mcp.example.com/mcp"]), &env).unwrap();
        assert_eq!(c.mcp_servers.len(), 1);
        assert_eq!(c.mcp_servers[0].name, "fs");
        assert_eq!(c.mcp_servers[0].endpoint, "https://mcp.example.com/mcp");
    }

    #[test]
    fn mcp_endpoint_spec_parsing() {
        // HTTPS-only: remote Streamable HTTP endpoints.
        assert!(is_mcp_endpoint("https://mcp.example.com/mcp"));
        assert!(is_mcp_endpoint("http://localhost:8080/mcp"));
        // Socket schemes and a stdio argv command are NOT endpoints.
        assert!(!is_mcp_endpoint("unix:/run/mcp.sock"));
        assert!(!is_mcp_endpoint("vsock:3:5000"));
        assert!(!is_mcp_endpoint("mcp-server-fs --root /data"));
        assert!(parse_mcp_spec("fs=mcp-server-fs --root /data").is_err());
        assert!(parse_mcp_spec("fs=unix:/run/mcp.sock").is_err());

        for ep in ["https://mcp.example.com/mcp", "http://127.0.0.1:8080/mcp"] {
            let spec = parse_mcp_spec(&format!("fs={ep}")).unwrap();
            assert_eq!(spec.name, "fs");
            assert_eq!(spec.endpoint, ep);
        }
    }

    #[test]
    fn mcp_endpoint_scheme_gate_is_https_only() {
        // The validation gate every server (CLI + config-file) flows through.
        assert!(mcp_endpoint_scheme_ok("https://mcp.example/mcp").is_ok());
        assert!(mcp_endpoint_scheme_ok("http://127.0.0.1:8080/mcp").is_ok());
        for bad in [
            "unix:/run/mcp.sock",
            "vsock:3:5000",
            "http://mcp.example:8080/mcp",
        ] {
            assert!(
                mcp_endpoint_scheme_ok(bad).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn mcp_endpoint_is_required_and_validated() {
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            ("AGENTD_INTELLIGENCE".into(), "https://intel.example".into()),
        ];
        // A valid endpoint spec loads clean.
        let mut c =
            Config::load(&args(&["--mcp", "fs=https://mcp.example.com/mcp"]), &env).unwrap();
        assert!(c.validate().is_ok());
        // An empty endpoint is rejected.
        c.mcp_servers[0].endpoint.clear();
        assert!(c.validate().is_err(), "an empty endpoint must fail");
        // An unparseable endpoint is rejected.
        c.mcp_servers[0].endpoint = "ftp://nope/".into();
        assert!(
            c.validate().is_err(),
            "an unsupported endpoint scheme must fail"
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
    fn multi_endpoint_list_accepts_ordered_comma_list() {
        // --intelligence is an ORDERED comma-separated list.
        let env = vec![("INSTRUCTION".into(), "x".into())];
        let c = Config::load(
            &args(&[
                "--intelligence",
                "https://a.example,https://b.example,https://c.example",
            ]),
            &env,
        )
        .unwrap();
        // the raw scalar is preserved; the client parses it into N endpoints.
        assert_eq!(
            c.intelligence.as_deref(),
            Some("https://a.example,https://b.example,https://c.example")
        );
    }

    #[test]
    fn multi_endpoint_bad_element_scheme_is_exit_2() {
        // A bad scheme on ANY element rejects the whole list.
        let env = vec![("INSTRUCTION".into(), "x".into())];
        let e = Config::load(
            &args(&[
                "--intelligence",
                "https://a.example,ftp://nope,https://c.example",
            ]),
            &env,
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    #[test]
    fn empty_endpoint_list_is_exit_2() {
        // An all-empty/whitespace list is "missing endpoint".
        let env = vec![("INSTRUCTION".into(), "x".into())];
        let e = Config::load(&args(&["--intelligence", " , , "]), &env).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    #[test]
    fn serve_target_http_parses() {
        assert_eq!(
            ServeTarget::parse("https://0.0.0.0:8443").unwrap(),
            ServeTarget::Http {
                bind: "0.0.0.0:8443".into(),
                tls: true
            }
        );
        // loopback plaintext is allowed (dev); a bracketed IPv6 loopback too.
        assert_eq!(
            ServeTarget::parse("http://127.0.0.1:9000").unwrap(),
            ServeTarget::Http {
                bind: "127.0.0.1:9000".into(),
                tls: false
            }
        );
        assert!(matches!(
            ServeTarget::parse("http://[::1]:9000"),
            Ok(ServeTarget::Http { tls: false, .. })
        ));
        // non-loopback plaintext, a path, or a missing port → usage error
        for bad in [
            "http://10.0.0.5:9000",
            "https://host:8443/mcp",
            "https://host",
        ] {
            assert!(
                matches!(ServeTarget::parse(bad), Err(ConfigError::Usage(_))),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn serve_auth_gates_the_control_plane() {
        let base = |extra: &[&str]| {
            let mut a = vec!["--instruction", "x", "--intelligence", "https://i.example"];
            a.extend_from_slice(extra);
            let args: Vec<String> = a.iter().map(|s| s.to_string()).collect();
            Config::load(&args, &[]).and_then(|c| c.validate().map(|_| c))
        };
        // A non-loopback https target with no auth is refused (open control plane).
        assert!(base(&["--serve-mcp", "https://0.0.0.0:8443"]).is_err());
        // https:// with no cert/key is refused even on loopback.
        assert!(base(&["--serve-mcp", "https://127.0.0.1:8443"]).is_err());
        // Loopback plaintext needs no auth (dev).
        assert!(base(&["--serve-mcp", "http://127.0.0.1:9000"]).is_ok());
        // TLS material on a plaintext target is rejected; a unix target skips
        // the TLS/auth material checks entirely (the kernel authenticates).
        assert!(base(&["--serve-mcp", "unix:/x.sock", "--serve-bearer", "t"]).is_ok());
        assert!(base(&["--serve-mcp", "http://127.0.0.1:9000", "--serve-cert", "/x"]).is_err());
        // Serve auth flags without --serve-mcp is a misconfig.
        assert!(base(&["--serve-bearer", "t"]).is_err());
    }

    #[test]
    fn serve_target_rejects_unsupported_socket_schemes() {
        // `unix:` is accepted for co-located peers — the kernel authenticates
        // them by uid — while `vsock:` and `tcp:` are not served at all: exit 2.
        assert!(matches!(
            ServeTarget::parse("unix:/run/agentd.sock"),
            Ok(ServeTarget::Unix { ref path }) if path == "/run/agentd.sock"
        ));
        assert!(matches!(
            ServeTarget::parse("unix:///run/agentd.sock"),
            Ok(ServeTarget::Unix { ref path }) if path == "/run/agentd.sock"
        ));
        for bad in ["vsock:5005", "vsock:2:5005", "tcp:1234"] {
            assert!(
                matches!(ServeTarget::parse(bad), Err(ConfigError::Usage(_))),
                "{bad} must be a usage error"
            );
        }
    }

    #[test]
    fn serve_mcp_validation_runs_at_load() {
        // a loopback http serve target parses through full load().
        let c = Config::load(
            &args(&["--serve-mcp", "http://127.0.0.1:9000"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.serve_mcp.as_deref(), Some("http://127.0.0.1:9000"));
        // A foreign scheme is rejected at load (exit 2) before any side effect.
        let e = Config::load(&args(&["--serve-mcp", "tcp:9000"]), &base_env()).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
    }

    #[test]
    fn a2a_peer_spec_parses_name_and_endpoint() {
        // The endpoint is the remainder after the first '=', so the unix:/vsock:
        // scheme passes through verbatim (no second '=' to confuse the split).
        let spec = parse_a2a_peer_spec("mesh=https://peer.example").unwrap();
        assert_eq!(spec.name, "mesh");
        assert_eq!(spec.endpoint, "https://peer.example");
        // Missing '=' / empty halves are usage errors.
        assert!(matches!(
            parse_a2a_peer_spec("noequals"),
            Err(ConfigError::Usage(_))
        ));
        assert!(matches!(
            parse_a2a_peer_spec("=https://x"),
            Err(ConfigError::Usage(_))
        ));
        assert!(matches!(
            parse_a2a_peer_spec("mesh="),
            Err(ConfigError::Usage(_))
        ));
    }

    #[test]
    fn a2a_endpoint_https_parses_and_gates_plaintext() {
        assert_eq!(
            A2aEndpoint::parse("https://peer.example:8443/a2a").unwrap(),
            A2aEndpoint::Https("https://peer.example:8443/a2a".into())
        );
        // loopback plaintext is allowed (dev); non-loopback plaintext is exit 2.
        assert!(matches!(
            A2aEndpoint::parse("http://127.0.0.1:9000"),
            Ok(A2aEndpoint::Https(_))
        ));
        assert!(matches!(
            A2aEndpoint::parse("http://peer.example:9000"),
            Err(ConfigError::Usage(_))
        ));
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn a2a_peer_flag_parses_and_validates_on_a2a_build() {
        // A valid https peer loads through full validation.
        let c = Config::load(
            &args(&["--a2a-peer", "mesh=https://peer.example:8443/a2a"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.a2a_peers.len(), 1);
        assert_eq!(c.a2a_peers[0].name, "mesh");
        assert_eq!(c.a2a_peers[0].endpoint, "https://peer.example:8443/a2a");

        // A unix peer endpoint parses (the co-located fast lane)…
        assert!(
            Config::load(
                &args(&["--a2a-peer", "mesh=unix:/run/peer.sock"]),
                &base_env()
            )
            .is_ok()
        );
        // …while vsock, non-loopback plaintext, and bare tcp stay rejected at
        // load (exit 2) before any side effect.
        for bad in [
            "mesh=vsock:2:5005",
            "mesh=http://peer.example:9000",
            "mesh=tcp:9000",
        ] {
            let e = Config::load(&args(&["--a2a-peer", bad]), &base_env()).unwrap_err();
            assert!(matches!(e, ConfigError::Usage(_)), "{bad} must be exit 2");
        }

        // A duplicate peer name is rejected.
        let dup = Config::load(
            &args(&[
                "--a2a-peer",
                "mesh=https://a.example",
                "--a2a-peer",
                "mesh=https://b.example",
            ]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(dup, ConfigError::Usage(_)));
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn a2a_peer_client_auth_is_validated_at_startup() {
        // A secret-shaped INLINE header value on a peer is rejected —
        // templates only, the same rule as MCP servers.
        let file = write_tmp(
            r#"{ "a2a_peers": [{ "name": "mesh", "endpoint": "https://peer.example/a2a",
                 "headers": { "authorization": "Bearer sk-live-inline-oops" } }] }"#,
        );
        let e = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)), "{e}");
        assert!(format!("{e}").contains("a2a peer 'mesh' header"), "{e}");

        // A resolvable {{secret:…}} template passes (the PROCESS env carries the
        // secret — the resolver reads std::env, like the MCP header resolver).
        let file = write_tmp(
            r#"{ "a2a_peers": [{ "name": "mesh", "endpoint": "https://peer.example/a2a",
                 "headers": { "authorization": "Bearer {{secret:A2A_PEER_AUTH_TEST_TOKEN}}" } }] }"#,
        );
        // SAFETY: single-threaded test; unique var name avoids cross-test races.
        unsafe { std::env::set_var("A2A_PEER_AUTH_TEST_TOKEN", "tok") };
        let c = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        unsafe { std::env::remove_var("A2A_PEER_AUTH_TEST_TOKEN") };
        assert_eq!(
            c.a2a_peers[0].headers.len(),
            1,
            "template stored, not resolved"
        );
        assert!(
            c.a2a_peers[0].headers[0].1.contains("{{secret:"),
            "the SPEC keeps the template, never the material"
        );

        // client_cert without client_key (and vice versa) is a pairing error.
        let file = write_tmp(
            r#"{ "a2a_peers": [{ "name": "mesh", "endpoint": "https://peer.example/a2a",
                 "client_cert": "/tls/cert.pem" }] }"#,
        );
        let e = Config::load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(
            format!("{e}").contains("client_cert and client_key must be set together"),
            "{e}"
        );
    }

    #[cfg(not(feature = "a2a"))]
    #[test]
    fn a2a_peer_requires_the_a2a_feature() {
        // The flag parses, but validation rejects it without the build feature.
        let e = Config::load(
            &args(&["--a2a-peer", "mesh=https://peer.example"]),
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

    #[test]
    fn debug_redacts_credential_bearing_intelligence_uri() {
        // The raw `--intelligence` URI can carry inline creds
        // (`https://user:pass@host`). The Debug impl must show the SCHEME only,
        // never the userinfo/host/path, mirroring effective_view.
        let env = vec![
            ("INSTRUCTION".into(), "x".into()),
            (
                "AGENTD_INTELLIGENCE".into(),
                "https://alice:hunter2@internal.example/v1".into(),
            ),
        ];
        let c = Config::load(&[], &env).unwrap();
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("hunter2"), "creds leaked: {dbg}");
        assert!(!dbg.contains("internal.example"), "host leaked: {dbg}");
        assert!(dbg.contains("https:<redacted>"), "scheme missing: {dbg}");
    }

    #[test]
    fn help_text_lists_model_swap() {
        // Fix 3: --model-swap is parsed+validated but was missing from --help.
        let h = match Config::load(&args(&["--help"]), &[]).unwrap_err() {
            ConfigError::Help(s) => s,
            other => panic!("expected Help, got {other:?}"),
        };
        assert!(h.contains("--model-swap"), "help omits --model-swap");
        assert!(h.contains("finish-on-old|restart-turn"));
    }

    // ──────────────────────────── config file ────────────────────────────────

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
                    { "name": "web", "endpoint": "https://web.example.com/mcp",
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
        assert_eq!(c.mcp_servers[0].endpoint, "https://web.example.com/mcp");
        assert_eq!(c.mcp_servers[0].tags, vec![TrifectaTag::UntrustedInput]);
        assert_eq!(c.subscribe, vec!["fs:file:///watch/inbox"]);
    }

    #[test]
    fn budget_tokens_lifetime_parses_from_flag_env_and_file() {
        // The per-instance lifetime cap. Unbounded (0) by default.
        assert_eq!(
            Config::load(&args(&[]), &base_env())
                .unwrap()
                .budget_tokens_lifetime,
            0
        );
        // Flag.
        let c = Config::load(&args(&["--budget-tokens-lifetime", "2000000"]), &base_env()).unwrap();
        assert_eq!(c.budget_tokens_lifetime, 2_000_000);
        // Env (the neutral `AGENT_BUDGET_TOKENS` is aliased to `AGENTD_*`).
        let mut env = base_env();
        env.push(("AGENT_BUDGET_TOKENS".into(), "500000".into()));
        assert_eq!(
            Config::load(&args(&[]), &env)
                .unwrap()
                .budget_tokens_lifetime,
            500_000
        );
        // Config-file `limits.lifetime_tokens`, and flag > file precedence.
        let file = write_tmp(r#"{ "model": "m", "limits": { "lifetime_tokens": 111 } }"#);
        let path = file.path().to_str().unwrap().to_string();
        assert_eq!(
            Config::load(&args(&["--config", &path]), &base_env())
                .unwrap()
                .budget_tokens_lifetime,
            111
        );
        assert_eq!(
            Config::load(
                &args(&["--config", &path, "--budget-tokens-lifetime", "222"]),
                &base_env()
            )
            .unwrap()
            .budget_tokens_lifetime,
            222
        );
    }

    #[test]
    fn env_and_flag_override_file_per_precedence() {
        // built-in < FILE < env < flag.
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

    /// A temp config file with a real extension (`Format::detect` reads it).
    fn write_tmp_ext(contents: &str, ext: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn config_file_may_be_yaml() {
        // The same structural config in YAML — nested limits, a server list
        // with tags, subscriptions in block style — loads exactly like JSON.
        let yaml = write_tmp_ext(
            r#"
# agentd config
model: claude-from-yaml
max_tokens: 1234567
limits:
  max_steps: 77
  max_depth: 3
  deadline_secs: 120
mcp_servers:
  - name: web
    endpoint: https://web.example.com/mcp
    tags:
      "*": [untrusted_input]
subscribe:
  - fs:file:///watch/inbox
log_level: warn
"#,
            "yaml",
        );
        let c = Config::load(
            &args(&["--config", yaml.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.model.as_deref(), Some("claude-from-yaml"));
        assert_eq!(c.max_tokens, 1_234_567);
        assert_eq!(c.max_steps, 77);
        assert_eq!(c.max_depth, 3);
        assert_eq!(c.deadline, Some(Duration::from_secs(120)));
        assert_eq!(c.mcp_servers.len(), 1);
        assert_eq!(c.mcp_servers[0].name, "web");
        assert_eq!(c.mcp_servers[0].tags, vec![TrifectaTag::UntrustedInput]);
        assert_eq!(c.subscribe, vec!["fs:file:///watch/inbox"]);
        assert_eq!(c.log_level, Level::Warn);

        // `.yml` too; and a YAML typo is still exit 2, naming the key.
        let bad = write_tmp_ext("model: m\nmax_token: 5\n", "yml");
        let e = Config::load(
            &args(&["--config", bad.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)), "{e}");
        assert!(format!("{e}").contains("max_token"), "{e}");
        // A YAML syntax error is exit 2 with the line named.
        let bad = write_tmp_ext("model: m\n\tlimits: {}\n", "yaml");
        let e = Config::load(
            &args(&["--config", bad.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(format!("{e}").contains("line 2"), "{e}");
    }

    #[test]
    fn path_env_vars_set_config_paths() {
        // Every config-file path is an env var named after the path:
        // `limits.max_steps` ⇒ AGENTD_LIMITS_MAX_STEPS / AGENT_… / bare.
        let file = write_tmp_ext(
            "limits:\n  max_steps: 1\n  max_depth: 9\nmodel: f\n",
            "yaml",
        );
        let mut env = base_env();
        env.push(("AGENTD_LIMITS_MAX_STEPS".into(), "5".into()));
        env.push(("AGENT_LIMITS_DEADLINE_SECS".into(), "30".into())); // neutral spelling
        env.push(("LIMITS_LIFETIME_TOKENS".into(), "4000".into())); // bare spelling
        env.push(("MODEL_SWAP".into(), "restart-turn".into())); // bare enum
        env.push(("AGENTD_SUBSCRIBE".into(), "a://1, a://2".into())); // list
        env.push((
            "AGENTD_INTELLIGENCE_HEADERS".into(),
            "{x-team: ops}".into(), // object literal
        ));
        let c = Config::load(&args(&["--config", file.path().to_str().unwrap()]), &env).unwrap();
        assert_eq!(c.max_steps, 5, "env path beats the file");
        assert_eq!(
            c.max_depth, 9,
            "untouched sibling path keeps the file value"
        );
        assert_eq!(c.deadline, Some(Duration::from_secs(30)));
        assert_eq!(c.budget_tokens_lifetime, 4000);
        assert_eq!(c.model_swap, SwapPolicy::RestartTurn);
        assert_eq!(c.subscribe, vec!["a://1", "a://2"]);
        assert_eq!(
            c.intelligence_headers.get("x-team").map(String::as_str),
            Some("ops")
        );
        assert_eq!(c.model.as_deref(), Some("f"));

        // Precedence within env: branded > neutral > bare.
        let mut env = base_env();
        env.push(("LIMITS_MAX_STEPS".into(), "1".into()));
        env.push(("AGENT_LIMITS_MAX_STEPS".into(), "2".into()));
        assert_eq!(Config::load(&args(&[]), &env).unwrap().max_steps, 2);
        env.push(("AGENTD_LIMITS_MAX_STEPS".into(), "3".into()));
        assert_eq!(Config::load(&args(&[]), &env).unwrap().max_steps, 3);

        // A value that does not type per the schema is exit 2, naming the var.
        let mut env = base_env();
        env.push(("AGENTD_LIMITS_MAX_STEPS".into(), "many".into()));
        let e = Config::load(&args(&[]), &env).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)));
        assert!(format!("{e}").contains("AGENTD_LIMITS_MAX_STEPS"), "{e}");
        let mut env = base_env();
        env.push(("AGENTD_LOG_LEVEL".into(), "loud".into()));
        let e = Config::load(&args(&[]), &env).unwrap_err();
        assert!(format!("{e}").contains("AGENTD_LOG_LEVEL"), "{e}");
    }

    #[test]
    fn generic_path_flags_override_env_and_file() {
        // Any config path is a flag: `--limits.max-steps` / `--limits-max-steps`
        // / `--limits.max_steps`; a flag beats env beats file; unknown flags are
        // still refused.
        let file = write_tmp_ext("limits:\n  max_steps: 1\n", "yaml");
        let mut env = base_env();
        env.push(("AGENTD_LIMITS_MAX_STEPS".into(), "2".into()));
        for spelling in [
            "--limits.max-steps",
            "--limits-max-steps",
            "--limits.max_steps",
        ] {
            let c = Config::load(
                &args(&["--config", file.path().to_str().unwrap(), spelling, "3"]),
                &env,
            )
            .unwrap();
            assert_eq!(c.max_steps, 3, "{spelling}");
        }
        // Flags apply in order: the last writer wins, whichever spelling.
        let c = Config::load(
            &args(&["--max-steps", "4", "--limits.max_steps", "5"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.max_steps, 5);
        let c = Config::load(
            &args(&["--limits.max_steps", "5", "--max-steps", "6"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.max_steps, 6);
        // A list path flag ADDS (repeatable-flag semantics), like --subscribe.
        let c = Config::load(
            &args(&["--subscribe", "a://1", "--subscribe", "[a://2, a://3]"]),
            &base_env(),
        )
        .unwrap();
        // (--subscribe is a named flag: its value is one URI, verbatim.)
        assert_eq!(c.subscribe, vec!["a://1", "[a://2, a://3]"]);
        let c = Config::load(
            &args(&[
                "--mcp-servers",
                "[{name: q, endpoint: https://q.example/mcp}]",
            ]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.mcp_servers.len(), 1);
        assert_eq!(c.mcp_servers[0].name, "q");
        // Typed by the schema: a non-integer is refused, naming the flag.
        let e = Config::load(&args(&["--limits.max-steps", "lots"]), &base_env()).unwrap_err();
        assert!(format!("{e}").contains("--limits.max-steps"), "{e}");
        // An enum path is checked against its set.
        let e = Config::load(&args(&["--model-swap", "sideways"]), &base_env()).unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)), "{e}");
        // Not a config path → the usual unknown-argument refusal.
        let e = Config::load(&args(&["--no-such-thing", "1"]), &base_env()).unwrap_err();
        assert!(format!("{e}").contains("unknown argument"), "{e}");
        // A path flag with no value is refused.
        let e = Config::load(&args(&["--limits.max-steps"]), &base_env()).unwrap_err();
        assert!(format!("{e}").contains("requires a value"), "{e}");
    }

    #[test]
    fn multiple_config_files_merge_in_order_later_wins() {
        // AGENTD_CONFIG (a `:` list) first, then each --config; a later file
        // overrides earlier ones: scalars replace, objects merge, lists replace,
        // `null` unsets. The merged document is ONE file layer — env and flags
        // still override it.
        let base = write_tmp_ext(
            "model: base
log_level: warn
limits:
  max_steps: 1
  max_depth: 2
subscribe: [a://1, a://2]
",
            "yaml",
        );
        let site = write_tmp_ext(
            r#"{ "model": "site", "limits": { "max_steps": 5 }, "subscribe": ["a://3"] }"#,
            "json",
        );
        let over = write_tmp_ext(
            "model: over
log_level: null
limits:
  max_depth: 7
",
            "yml",
        );
        let mut env = base_env();
        env.push((
            "AGENT_CONFIG".into(), // the neutral spelling of the list, base first
            format!("{}:{}", base.path().display(), site.path().display()),
        ));
        let c = Config::load(&args(&["--config", over.path().to_str().unwrap()]), &env).unwrap();
        assert_eq!(
            c.model.as_deref(),
            Some("over"),
            "last file wins on a scalar"
        );
        assert_eq!(
            c.max_steps, 5,
            "site's limits.max_steps survives (objects merge)"
        );
        assert_eq!(c.max_depth, 7, "over's limits.max_depth wins");
        assert_eq!(c.subscribe, vec!["a://3"], "a later file REPLACES a list");
        assert_eq!(
            c.log_level,
            Level::Info,
            "`null` unsets → back to the default"
        );
        assert_eq!(c.config_files.len(), 3);
        assert!(c.config_files[0].ends_with(".yaml") && c.config_files[2].ends_with(".yml"));
        // Order matters: the same overlay first, then base → base wins.
        let c = Config::load(
            &args(&[
                "--config",
                over.path().to_str().unwrap(),
                "--config",
                base.path().to_str().unwrap(),
            ]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.model.as_deref(), Some("base"));
        assert_eq!(c.max_depth, 2);
        // Env and flags still beat every file.
        let mut env2 = base_env();
        env2.push(("AGENTD_MODEL".into(), "from-env".into()));
        let c = Config::load(
            &args(&[
                "--config",
                base.path().to_str().unwrap(),
                "--config",
                over.path().to_str().unwrap(),
                "--limits.max-depth",
                "9",
            ]),
            &env2,
        )
        .unwrap();
        assert_eq!(c.model.as_deref(), Some("from-env"));
        assert_eq!(c.max_depth, 9);
        // A broken file anywhere in the list is exit 2 naming that file.
        let e = Config::load(
            &args(&[
                "--config",
                base.path().to_str().unwrap(),
                "--config",
                "/no/such/overlay.yaml",
            ]),
            &base_env(),
        )
        .unwrap_err();
        assert!(format!("{e}").contains("/no/such/overlay.yaml"), "{e}");
        let paths = Config::config_paths_from(
            &args(&["--config", "b.yaml"]),
            &[("AGENT_CONFIG".into(), "x.yaml::y.json:".into())],
        );
        assert_eq!(paths, vec!["x.yaml", "y.json", "b.yaml"]);
    }

    #[test]
    fn setting_a_path_replaces_the_value_while_named_flags_add() {
        // A path SET (env or `--<path>`) replaces the list/map at that path; the
        // named repeatable flags (`--mcp`, `--subscribe`) ADD; a `--<map>.<key>`
        // entry flag merges one key.
        let file = write_tmp_ext(
            "subscribe: [a://file]
mcp_servers:
  - name: web
    endpoint: https://web.example/mcp
intelligence_headers:
  keep: me
  x-team: file
",
            "yaml",
        );
        let path = file.path().to_str().unwrap().to_string();
        // env list path replaces the file's list.
        let mut env = base_env();
        env.push(("AGENTD_SUBSCRIBE".into(), "a://env".into()));
        let c = Config::load(&args(&["--config", &path]), &env).unwrap();
        assert_eq!(c.subscribe, vec!["a://env"]);
        // ...and the named flag adds to that.
        let c = Config::load(&args(&["--config", &path, "--subscribe", "a://flag"]), &env).unwrap();
        assert_eq!(c.subscribe, vec!["a://env", "a://flag"]);
        // `--mcp-servers '[…]'` sets the whole list; `--mcp` adds one.
        let c = Config::load(
            &args(&[
                "--config",
                &path,
                "--mcp-servers",
                "[{name: q, endpoint: https://q.example/mcp}]",
                "--mcp",
                "x=https://x.example/mcp",
            ]),
            &base_env(),
        )
        .unwrap();
        let names: Vec<&str> = c.mcp_servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["q", "x"]);
        // A map ENTRY flag merges one key (exact spelling), keeping the others.
        let c = Config::load(
            &args(&["--config", &path, "--intelligence_headers.x-team", "ops"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(
            c.intelligence_headers.get("x-team").map(String::as_str),
            Some("ops")
        );
        assert_eq!(
            c.intelligence_headers.get("keep").map(String::as_str),
            Some("me")
        );
        // The whole-map form replaces the map.
        let c = Config::load(
            &args(&["--config", &path, "--intelligence-headers", "{only: this}"]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(c.intelligence_headers.len(), 1);
        assert_eq!(
            c.intelligence_headers.get("only").map(String::as_str),
            Some("this")
        );
        // Reaching into a list by index is refused with a clear message.
        let e = Config::load(&args(&["--mcp-servers.0.aauth", "true"]), &base_env()).unwrap_err();
        assert!(format!("{e}").contains("array elements"), "{e}");
    }

    #[test]
    fn help_lists_every_config_path_with_flag_and_env() {
        let h = match Config::load(&args(&["--help"]), &[]) {
            Err(ConfigError::Help(h)) => h,
            other => panic!("expected help, got {other:?}"),
        };
        assert!(h.contains("CONFIG PATHS"), "{h}");
        assert!(h.contains("limits.max_steps"), "{h}");
        assert!(h.contains("--limits-max-steps"), "{h}");
        assert!(h.contains("AGENTD_LIMITS_MAX_STEPS"), "{h}");
        assert!(h.contains("YAML or JSON"), "{h}");
    }

    #[test]
    fn reload_re_reads_a_yaml_file() {
        // The reload path is `load` again over the ORIGINAL args/env; a rewritten
        // YAML file is picked up with flags still winning over it.
        let file = write_tmp_ext("model: v1\nlimits:\n  max_steps: 10\n", "yaml");
        let path = file.path().to_str().unwrap().to_string();
        let a = args(&["--config", &path, "--max-depth", "2"]);
        let env = base_env();
        let running = Config::load(&a, &env).unwrap();
        assert_eq!(running.model.as_deref(), Some("v1"));
        assert_eq!(running.max_steps, 10);
        std::fs::write(
            &path,
            "model: v2\nlimits:\n  max_steps: 20\n  max_depth: 7\n",
        )
        .unwrap();
        let reloaded = Config::reload(&a, &env).unwrap();
        assert_eq!(reloaded.model.as_deref(), Some("v2"));
        assert_eq!(reloaded.max_steps, 20);
        assert_eq!(
            reloaded.max_depth, 2,
            "the flag still overrides the new file"
        );
        // A now-broken YAML file is a rejected reload (Usage), not a crash.
        std::fs::write(&path, "model: [unterminated\n").unwrap();
        assert!(matches!(
            Config::reload(&a, &env),
            Err(ConfigError::Usage(_))
        ));
    }

    #[test]
    fn flag_mcp_and_subscribe_add_to_the_file_list() {
        // Repeatable list flags ADD to the file's lists (the one documented
        // deviation from pure last-writer-wins).
        let file = write_tmp(
            r#"{ "mcp_servers": [{ "name": "web", "endpoint": "https://web.example.com/mcp" }],
                "subscribe": ["fs:file:///a"] }"#,
        );
        let c = Config::load(
            &args(&[
                "--config",
                file.path().to_str().unwrap(),
                "--mcp",
                "fs=https://fs.example",
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

    // ─────────────────────────── --watch-config ──────────────────────────────

    /// Without the `config-watch` build feature, `--watch-config` (even WITH a
    /// config file) is a usage error — never silently ignored (the operator would
    /// believe a ConfigMap swap reloads when only SIGHUP would).
    #[cfg(not(feature = "config-watch"))]
    #[test]
    fn watch_config_requires_config_watch_feature() {
        let file = write_tmp(r#"{ "model": "m" }"#);
        let e = Config::load(
            &args(&["--config", file.path().to_str().unwrap(), "--watch-config"]),
            &base_env(),
        )
        .unwrap_err();
        match e {
            ConfigError::Usage(msg) => assert!(
                msg.contains("--watch-config requires the 'config-watch' build feature"),
                "got: {msg}"
            ),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    /// With the feature, `--watch-config` + a `--config` file parses and sets the
    /// always-compiled `watch_config` flag.
    #[cfg(feature = "config-watch")]
    #[test]
    fn watch_config_parses_with_a_config_file() {
        let file = write_tmp(r#"{ "model": "m" }"#);
        let c = Config::load(
            &args(&["--config", file.path().to_str().unwrap(), "--watch-config"]),
            &base_env(),
        )
        .unwrap();
        assert!(c.watch_config);
    }

    /// `AGENTD_WATCH_CONFIG` env parses too (a flag would override it).
    #[cfg(feature = "config-watch")]
    #[test]
    fn watch_config_parses_from_env() {
        let file = write_tmp(r#"{ "model": "m" }"#);
        let mut env = base_env();
        env.push(("AGENTD_CONFIG".into(), file.path().to_str().unwrap().into()));
        env.push(("AGENTD_WATCH_CONFIG".into(), "true".into()));
        let c = Config::load(&args(&[]), &env).unwrap();
        assert!(c.watch_config);
    }

    /// `--watch-config` with NO config file is a usage error — watching nothing is
    /// meaningless. (Only exercised on a `config-watch` build; off
    /// the feature the feature-gate error fires first.)
    #[cfg(feature = "config-watch")]
    #[test]
    fn watch_config_requires_a_config_file() {
        let e = Config::load(&args(&["--watch-config"]), &base_env()).unwrap_err();
        match e {
            ConfigError::Usage(msg) => assert!(
                msg.contains("--watch-config requires a config file"),
                "got: {msg}"
            ),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    /// The admission gate (`--validate-config`) also rejects `--watch-config`
    /// without a config file — the same diagnostic, collected.
    #[cfg(feature = "config-watch")]
    #[test]
    fn validate_config_flags_watch_config_without_a_file() {
        let v = validate_verdict(&["--validate-config", "--watch-config"], &base_env());
        let lines = v.expect_err("watch-config without a file is invalid");
        assert!(
            lines.contains("--watch-config requires a config file"),
            "got: {lines}"
        );
    }

    // ───────────────────────────  --validate-config  ─────────────────────────

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
        // reactive with no subscribe → invalid. Verdict is Err.
        let v = validate_verdict(&["--validate-config", "--mode", "reactive"], &base_env());
        let lines = v.unwrap_err();
        assert!(lines.contains("config.invalid"));
        // Each line is parseable NDJSON.
        for line in lines.lines() {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn validate_config_refuses_a_trifecta_only_config_exit2() {
        // The trifecta gate lives in `validate()`, the
        // ONE validation authority, so `--validate-config` must REFUSE a complete
        // trifecta exactly as startup does: a verdict that says "valid" for a
        // config the daemon would refuse is worse than no verdict at all. One
        // server tagged with all three legs, no override.
        let v = validate_verdict(
            &[
                "--validate-config",
                "--mcp",
                "s=https://s.example",
                "--mcp-tags",
                "s=untrusted_input,sensitive,egress",
            ],
            &base_env(),
        );
        let lines = v.expect_err("a trifecta-only config must be invalid");
        assert!(lines.contains("config.invalid"), "got: {lines}");
        assert!(lines.contains("lethal-trifecta"), "got: {lines}");
        for line in lines.lines() {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn validate_config_and_startup_agree_on_trifecta() {
        // The same trifecta config: startup `load()` errors (Usage, exit 2) and
        // `--validate-config` returns an invalid verdict — they can never disagree.
        let trifecta = [
            "--mcp",
            "s=https://s.example",
            "--mcp-tags",
            "s=untrusted_input,sensitive,egress",
        ];
        // Startup path (no --validate-config): a Usage error.
        let startup = Config::load(&args(&trifecta), &base_env()).unwrap_err();
        assert!(matches!(startup, ConfigError::Usage(_)));
        // --allow-trifecta makes BOTH paths pass.
        let mut allowed = vec!["--allow-trifecta"];
        allowed.extend_from_slice(&trifecta);
        assert!(Config::load(&args(&allowed), &base_env()).is_ok());
        let mut allowed_vc = vec!["--validate-config", "--allow-trifecta"];
        allowed_vc.extend_from_slice(&trifecta);
        assert!(validate_verdict(&allowed_vc, &base_env()).is_ok());
    }

    #[test]
    fn validate_config_runs_without_an_instruction() {
        // No INSTRUCTION at all: --validate-config still produces a verdict (it
        // does not need an instruction to *run*); the missing-instruction shows
        // up as an invalid diagnostic, not a crash.
        let env = vec![("AGENTD_INTELLIGENCE".into(), "https://intel.example".into())];
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

    // ────────────────────────────  --config-schema  ──────────────────────────

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

    // ──────────────────────────────  secret refs  ────────────────────────────

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
    #[cfg(feature = "aauth")]
    fn aauth_flags_and_validation() {
        // Provider + all sub-flags parse into AAuthSettings (order-independent).
        let c = Config::load(
            &args(&[
                "--aauth-key-file",
                "/tmp/id.key",
                "--aauth-provider",
                "https://apd.example",
                "--aauth-enroll-token",
                "{{secret:ENROLL}}",
                "--aauth-enroll-assertion-file",
                "/var/run/secrets/aauth/token",
                "--aauth-person-server",
                "https://ps.example",
            ]),
            &base_env(),
        )
        .unwrap();
        let a = c.aauth.expect("aauth configured");
        assert_eq!(a.provider, "https://apd.example");
        assert_eq!(a.key_file, "/tmp/id.key");
        assert_eq!(a.enrollment_token.as_deref(), Some("{{secret:ENROLL}}"));
        assert_eq!(
            a.enroll_assertion_file.as_deref(),
            Some("/var/run/secrets/aauth/token")
        );
        assert_eq!(a.person_server.as_deref(), Some("https://ps.example"));

        // The assertion file path also parses from its env spelling.
        let mut env = base_env();
        env.push(("AGENT_AAUTH_PROVIDER".into(), "https://apd.example".into()));
        env.push((
            "AGENT_AAUTH_ENROLL_ASSERTION_FILE".into(),
            "/var/run/secrets/aauth/token".into(),
        ));
        let a = Config::load(&args(&[]), &env).unwrap().aauth.unwrap();
        assert_eq!(
            a.enroll_assertion_file.as_deref(),
            Some("/var/run/secrets/aauth/token")
        );

        // Key file defaults; env spelling; a bad provider URL is exit 2.
        let mut env = base_env();
        env.push(("AGENT_AAUTH_PROVIDER".into(), "https://apd.example".into()));
        let c = Config::load(&args(&[]), &env).unwrap();
        assert_eq!(c.aauth.unwrap().key_file, "agent.key");
        assert!(Config::load(&args(&["--aauth-provider", "not-a-url"]), &base_env()).is_err());
        assert!(
            Config::load(
                &args(&[
                    "--aauth-provider",
                    "https://apd.example",
                    "--aauth-person-server",
                    "nope"
                ]),
                &base_env()
            )
            .is_err()
        );
        // No provider ⇒ no aauth (the sub-flags alone are inert).
        assert!(
            Config::load(&args(&["--aauth-key-file", "/x"]), &base_env())
                .unwrap()
                .aauth
                .is_none()
        );
    }

    #[test]
    #[cfg(feature = "tls")]
    fn tls_ca_flag_env_and_content_validation() {
        // A real CA PEM (the net crate's test fixture) parses + validates.
        let ca = write_tmp(include_str!("../../../net/tests/fixtures/ca.pem"));
        let ca_path = ca.path().to_str().unwrap().to_string();

        // Flag form.
        let c = Config::load(&args(&["--tls-ca", &ca_path]), &base_env()).unwrap();
        assert_eq!(c.tls_ca.as_deref(), Some(ca_path.as_str()));
        // A file PATH is public material — visible in the redacted Debug.
        assert!(format!("{c:?}").contains(&ca_path));

        // Env form, branded + neutral (debrand alias).
        for key in ["AGENTD_TLS_CA", "AGENT_TLS_CA"] {
            let mut env = base_env();
            env.push((key.into(), ca_path.clone()));
            let c = Config::load(&args(&[]), &env).unwrap();
            assert_eq!(c.tls_ca.as_deref(), Some(ca_path.as_str()), "via {key}");
        }

        // A missing file is exit 2 at load, not a first-dial surprise.
        let err = Config::load(&args(&["--tls-ca", "/nonexistent/ca.pem"]), &base_env());
        assert!(matches!(err, Err(ConfigError::Usage(_))));

        // Junk content (readable, but not a CA PEM) is exit 2 too.
        let junk = write_tmp("not a pem");
        let err = Config::load(
            &args(&["--tls-ca", junk.path().to_str().unwrap()]),
            &base_env(),
        );
        assert!(matches!(err, Err(ConfigError::Usage(_))));
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
        // The "secret in the file" footgun — exit 2.
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
        // A {{secret:NAME}} whose env var is unset → exit 2 at startup.
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

    // ───────────────────────  hot-reload coherence  ──────────────────────────

    /// A valid reactive baseline config to diff reloads against.
    fn reactive_base() -> Config {
        Config::load(
            &args(&["--mode", "reactive", "--subscribe", "file:///in.json"]),
            &base_env(),
        )
        .unwrap()
    }

    #[test]
    fn coherence_rejects_a_differing_restart_only_field() {
        // A restart-only field that DIFFERS on a live
        // reload is a hard reject naming the field.
        let running = reactive_base();
        for mutate in [
            (|c: &mut Config| c.mode = Mode::Loop) as fn(&mut Config),
            |c: &mut Config| c.run_id = "different-run-id".into(),
            |c: &mut Config| c.serve_mcp = Some("https://a.example:8443".into()),
            |c: &mut Config| c.drain_timeout = Duration::from_secs(99),
        ] {
            let mut new = running.clone();
            mutate(&mut new);
            let diags = Config::reload_coherence_check(&new, Some(&running), true)
                .expect_err("a restart-only diff must be rejected");
            assert!(
                diags
                    .iter()
                    .any(|d| d.is_error() && d.msg.contains("restart-only")),
                "expected a restart-only error, got {diags:?}"
            );
        }
    }

    #[test]
    fn coherence_accepts_a_reloadable_diff() {
        // log_level / model / subscribe / mcp_servers, the intelligence
        // endpoint list and the model-swap policy are all reloadable, so a diff
        // in any of them passes the coherence check untouched.
        let running = reactive_base();
        for mutate in [
            (|c: &mut Config| c.log_level = Level::Debug) as fn(&mut Config),
            |c: &mut Config| c.model = Some("claude-opus-4".into()),
            |c: &mut Config| c.max_tokens = 999_999,
            |c: &mut Config| c.max_steps = 123,
            |c: &mut Config| c.subscribe = vec!["file:///in.json".into(), "file:///b.json".into()],
            // The MCP server inventory is reloadable via a re-handshake.
            |c: &mut Config| {
                c.mcp_servers = vec![McpServerSpec {
                    name: "added".into(),
                    endpoint: "unix:/mcp-new.sock".into(),
                    ..Default::default()
                }]
            },
            // An endpoint repoint is a reloadable hot swap.
            |c: &mut Config| c.intelligence = Some("https://other.example".into()),
            |c: &mut Config| c.model_swap = SwapPolicy::RestartTurn,
        ] {
            let mut new = running.clone();
            mutate(&mut new);
            assert!(
                Config::reload_coherence_check(&new, Some(&running), true).is_ok(),
                "a reloadable diff must be accepted",
            );
        }
    }

    #[test]
    fn mcp_servers_is_reloadable_not_restart_only() {
        // `mcp_servers` is not restart-only: `triggers::mode` performs a live
        // re-handshake, so adding, removing or editing a server is APPLIED at
        // the quiesce boundary rather than rejected.
        assert!(
            !RESTART_ONLY_FIELDS.contains(&"mcp_servers"),
            "mcp_servers must NOT be restart-only"
        );
        let running = reactive_base();
        // ADD a server.
        let mut added = running.clone();
        added.mcp_servers.push(McpServerSpec {
            name: "extra".into(),
            endpoint: "unix:/mcp-extra.sock".into(),
            ..Default::default()
        });
        assert!(
            Config::reload_coherence_check(&added, Some(&running), true).is_ok(),
            "adding an MCP server must pass the coherence check (it is reloadable)"
        );
        // EDIT a server's endpoint (a changed server = remove-then-add at apply).
        let mut with_server = running.clone();
        with_server.mcp_servers = vec![McpServerSpec {
            name: "s".into(),
            endpoint: "unix:/mcp-orig.sock".into(),
            ..Default::default()
        }];
        let mut edited = with_server.clone();
        edited.mcp_servers[0].endpoint = "https://mcp-edited.example".into();
        assert!(
            Config::reload_coherence_check(&edited, Some(&with_server), true).is_ok(),
            "editing an MCP server must pass the coherence check (it is reloadable)"
        );
    }

    #[test]
    fn model_swap_flag_and_env_parse_and_default() {
        // `--model-swap` / `AGENTD_MODEL_SWAP` selects the policy; the default
        // is finish-on-old.
        let def = Config::load(&args(&[]), &base_env()).unwrap();
        assert_eq!(def.model_swap, SwapPolicy::FinishOnOld);
        let flag = Config::load(&args(&["--model-swap", "restart-turn"]), &base_env()).unwrap();
        assert_eq!(flag.model_swap, SwapPolicy::RestartTurn);
        let mut env = base_env();
        env.push(("AGENTD_MODEL_SWAP".into(), "restart-turn".into()));
        let e = Config::load(&args(&[]), &env).unwrap();
        assert_eq!(e.model_swap, SwapPolicy::RestartTurn);
        // A bad value is exit 2 (Usage), like any other invalid scalar.
        assert!(matches!(
            Config::load(&args(&["--model-swap", "nope"]), &base_env()),
            Err(ConfigError::Usage(_))
        ));
    }

    #[test]
    fn intelligence_is_reloadable_not_restart_only() {
        // `intelligence` (the endpoint list) is not restart-only: a repoint is
        // APPLIED as a hot swap rather than rejected.
        assert!(
            !RESTART_ONLY_FIELDS.contains(&"intelligence"),
            "intelligence must NOT be restart-only"
        );
        let running = reactive_base();
        let mut new = running.clone();
        new.intelligence = Some("https://gw-b.example:1234".into());
        assert!(
            Config::reload_coherence_check(&new, Some(&running), true).is_ok(),
            "an endpoint repoint must pass the coherence check (it is reloadable)"
        );
    }

    #[test]
    fn coherence_rejects_duplicate_server_names() {
        let mut cfg = reactive_base();
        cfg.mcp_servers = vec![
            McpServerSpec {
                name: "dup".into(),
                endpoint: "unix:/a.sock".into(),
                ..Default::default()
            },
            McpServerSpec {
                name: "dup".into(),
                endpoint: "unix:/b.sock".into(),
                ..Default::default()
            },
        ];
        let diags = Config::reload_coherence_check(&cfg, None, false)
            .expect_err("duplicate server names must be an error");
        assert!(
            diags
                .iter()
                .any(|d| d.is_error() && d.msg.contains("duplicate"))
        );
    }

    #[test]
    fn restart_only_set_pins_the_immutable_fields() {
        // The partition pins mode / identity / transport, and each named field
        // is diff-detected
        // by `restart_only_field_differs` (a field listed but not compared would
        // silently reload — guard against that regression).
        for &f in RESTART_ONLY_FIELDS {
            let mut a = reactive_base();
            let b = a.clone();
            // Mutate the field on `a` and assert the diff is detected.
            match f {
                "mode" => a.mode = Mode::Loop,
                "run_id" => a.run_id = "x".into(),
                "serve_mcp" => a.serve_mcp = Some("https://s.example:8443".into()),
                "drain_timeout" => a.drain_timeout = Duration::from_secs(123),
                "continue_subscribe" => a.continue_subscribe = vec!["u".into()],
                other => panic!("RESTART_ONLY_FIELDS has an unmapped field '{other}'"),
            }
            assert!(
                a.restart_only_field_differs(&b, f),
                "restart-only field '{f}' must be diff-detected"
            );
        }
    }

    #[test]
    fn effective_view_carries_no_secret_or_url() {
        // The effective view is reloadable and REDACTED — no token, no endpoint
        // URL, no resolved {{secret:…}} value, header NAMES only.
        const TOKEN: &str = "super-secret-effective-token";
        let mut env = base_env();
        env.push(("AGENTD_INTELLIGENCE_TOKEN".into(), TOKEN.into()));
        env.push((
            "AGENTD_INTELLIGENCE".into(),
            "https://user:embedded-cred@api.example/v1".into(),
        ));
        let mut cfg =
            Config::load(&args(&["--mcp", "vault=https://vault.example/mcp"]), &env).unwrap();
        cfg.intelligence_headers
            .insert("x-api-key".into(), "{{secret:SOME_NAME}}".into());
        let view = cfg.effective_view();
        let blob = serde_json::to_string(&view).unwrap();
        assert!(!blob.contains(TOKEN), "token leaked into effective view");
        assert!(!blob.contains("embedded-cred"), "URL creds leaked");
        assert!(!blob.contains("api.example"), "endpoint host leaked");
        assert!(!blob.contains("SOME_NAME"), "header ref value leaked");
        assert!(!blob.contains("vault-secret.sock"), "mcp endpoint leaked");
        // The structural reloadable fields ARE present (name + header KEY).
        assert_eq!(view["mcp_servers"][0]["name"], serde_json::json!("vault"));
        assert_eq!(
            view["intelligence_headers"],
            serde_json::json!(["x-api-key"])
        );
    }
}
