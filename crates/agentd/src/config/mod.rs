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

#[cfg(feature = "sign")]
pub mod attest; // §7 instruction attestation (JWS/Ed25519, resolution manifest)
#[cfg(feature = "decrypt")]
pub mod decrypt; // on-the-fly instruction decryption: age v1 + JWE compact (RFC 0041)
pub mod effective; // --effective-config: the assembled document + where each setting came from
pub mod envelope;
pub mod fileset; // dir:+glob:+order: — one implementation, shared by workflows and instructions

/// GET a document over HTTP(S) — the `url:` document source. Redirects are
/// followed; a non-2xx is a refusal naming the status, because an error page
/// silently installed as an agent's instruction is the worst outcome here.
pub fn http_get(url: &str) -> Result<String, String> {
    use crate::net::http::{self, Url};
    const MAX_REDIRECTS: usize = 3;
    const MAX_BYTES: usize = 8 * 1024 * 1024;
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let u = Url::parse(&current).map_err(|e| format!("url {current}: {e}"))?;
        let tcp = http::connect_tcp(&u.host, u.port, std::time::Duration::from_secs(30))
            .map_err(|e| format!("connect {}: {e}", u.host))?;
        let mut stream: Box<dyn http::Stream> = if u.is_tls() {
            #[cfg(feature = "tls")]
            {
                Box::new(
                    crate::net::tls::connect(tcp, &u.host, None)
                        .map_err(|e| format!("tls {}: {e}", u.host))?,
                )
            }
            #[cfg(not(feature = "tls"))]
            {
                return Err("https requires building with --features tls".into());
            }
        } else {
            Box::new(tcp)
        };
        let resp = http::send(
            stream.as_mut(),
            &u.host_header(),
            "GET",
            &u.path,
            &[("Accept", "text/markdown, text/plain, */*")],
            &[],
        )
        .map_err(|e| format!("GET {current}: {e}"))?;
        if (301..=308).contains(&resp.status) {
            match resp.header("location") {
                Some(loc) => {
                    current = loc.to_string();
                    continue;
                }
                None => return Err(format!("{current}: redirect with no Location")),
            }
        }
        if !resp.is_success() {
            return Err(format!("{current}: HTTP {}", resp.status));
        }
        if resp.body.len() > MAX_BYTES {
            return Err(format!("{current}: document exceeds {MAX_BYTES} bytes"));
        }
        return match String::from_utf8(resp.body) {
            Ok(t) => Ok(t),
            Err(e) if envelope::looks_encrypted(e.as_bytes()) => Ok(envelope::armor(e.as_bytes())),
            Err(_) => Err(format!(
                "{current}: not UTF-8 and not a recognized encrypted envelope"
            )),
        };
    }
    Err(format!("{url}: too many redirects"))
} // encrypted-envelope detection (RFC 0041) — always compiled, no crypto
pub mod envfile;
pub mod file;
pub mod idoc;
pub mod paths;
pub mod prompt;
pub mod templates;
pub mod v2;
#[cfg(all(unix, feature = "config-watch"))]
pub mod watch;
pub mod yaml;

use crate::sec::scope::TrifectaTag;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

// ──────────────────────────────  hot reload  ────────────────────────────────
//
// The reloadable-vs-restart-only partition plus the coherence check that both
// the reload path and `--validate-config` run. This block is pure data and
// pure-CPU checks — no side effect, no subsystem touched; the apply step lives
// in `triggers::mode`. It compiles in every feature combination: the SIGHUP
// trigger and the reactive apply are `hot-reload`-gated, while the partition
// itself is always available, so `--validate-config` reports restart-only
// warnings on any build.

/// Heuristic: is this header name credential-shaped? A header so named must
/// carry a `{{secret:…}}` *reference*, never an inline literal, so a secret
/// cannot be smuggled into a config file under a plausible header name.
pub fn is_secret_shaped_key(name: &str) -> bool {
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
    /// True when `paths` came from DISCOVERY — nothing named a config, so the
    /// chain was walked. Never true alongside a named path: discovery is a
    /// fallback for an empty list.
    pub discovered: bool,
    /// Set when one RUNG of the chain had two spellings at once. Carried as
    /// data rather than returned as an error so this stays pure and the file
    /// watcher can call it; the loader turns it into a usage error.
    pub ambiguous: Option<String>,
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
    // Nothing named a config, so walk the discovery chain — a user default, the
    // project's own file, a machine-local overlay — the way a linter or a
    // formatter picks up its dotfile. Only ever a fallback: an explicit
    // `--config` or `AGENTD_CONFIG` means the caller has already decided, and
    // silently merging a stray `agentd.local.yml` into a named production
    // config would be the worst kind of surprise.
    let mut discovered = false;
    let mut ambiguous = None;
    if paths.is_empty() && !is_informational(args) {
        match discovered_chain(Path::new("."), envmap) {
            Ok(found) => {
                discovered = !found.is_empty();
                paths.extend(found);
            }
            Err(e) => ambiguous = Some(e),
        }
    }
    ConfigPaths {
        paths,
        discovered,
        ambiguous,
    }
}

/// One rung of the discovery chain: the spellings that name the SAME logical
/// file. Two spellings because `.yml` and `.yaml` are both idiomatic and
/// guessing wrong should not mean silence — a config file the tool ignores is
/// the worst outcome of the three.
///
/// The user rung, under `$XDG_CONFIG_HOME` (else `~/.config`): defaults that
/// follow the person, not the checkout.
pub const USER_CONFIG_NAMES: [&str; 2] = ["config.yml", "config.yaml"];

/// The project rung, in the working directory. The dotted spellings are the
/// original discovery names and stay valid: they shipped, and silently
/// ignoring one would break the setups that adopted it.
pub const PROJECT_CONFIG_NAMES: [&str; 4] =
    ["agentd.yml", "agentd.yaml", ".agentd.yml", ".agentd.yaml"];

/// The local rung: a machine-specific overlay that is expected to be
/// git-ignored, so a checkout can be pointed at a dev endpoint without the
/// change ever being committable by accident.
pub const LOCAL_CONFIG_NAMES: [&str; 2] = ["agentd.local.yml", "agentd.local.yaml"];

/// The user rung's directory: `$XDG_CONFIG_HOME/agentd`, else `~/.config/agentd`.
/// `None` when neither variable is set — a daemon with no HOME (a scratch
/// container, a systemd unit without one) simply has no user rung rather than
/// resolving a path relative to nothing.
pub fn user_config_dir(envmap: &HashMap<&str, &str>) -> Option<PathBuf> {
    if let Some(x) = envmap.get("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(Path::new(x).join("agentd"));
    }
    envmap
        .get("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| Path::new(h).join(".config").join("agentd"))
}

/// Which of `names` exist in `dir`, in order.
///
/// Returns **all** matches rather than the first, so that two spellings of one
/// rung present at once surfaces as an error rather than a silent pick between
/// them. Callers that get more than one refuse to start.
pub fn present_in(dir: &Path, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .map(|n| dir.join(n))
        .filter(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// The whole discovery chain, LOWEST precedence first: the user rung, then the
/// project rung, then the local overlay. Every rung that has a file
/// contributes one, and they merge in that order — so a user default is
/// overridden by the project's config and that by a machine-local overlay,
/// with flags and environment still on top of all three.
///
/// `Err` names the rung that is ambiguous. Ambiguity is per RUNG, not across
/// the chain: `agentd.yml` beside `agentd.local.yml` is the design, while
/// `agentd.yml` beside `agentd.yaml` is a coin toss nobody should have to
/// debug.
pub fn discovered_chain(cwd: &Path, envmap: &HashMap<&str, &str>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut rungs: Vec<(&str, PathBuf, &[&str])> = Vec::new();
    if let Some(d) = user_config_dir(envmap) {
        rungs.push(("user", d, &USER_CONFIG_NAMES));
    }
    rungs.push(("project", cwd.to_path_buf(), &PROJECT_CONFIG_NAMES));
    rungs.push(("local", cwd.to_path_buf(), &LOCAL_CONFIG_NAMES));
    for (label, dir, names) in rungs {
        let found = present_in(&dir, names);
        if found.len() > 1 {
            return Err(format!(
                "the {label} config is ambiguous: {} are both present; keep one (or name the file with --config)",
                found.join(" and ")
            ));
        }
        out.extend(found);
    }
    Ok(out)
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

pub(crate) fn usage(msg: String) -> ConfigError {
    ConfigError::Usage(format!("agentd: {msg}"))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The chain: a user default, the project's file, a machine-local overlay —
    /// in that order, so each is overridden by the more specific one. Rungs
    /// compose; SPELLINGS within one rung do not.
    #[test]
    fn the_discovery_chain_layers_user_then_project_then_local() {
        let root = std::env::temp_dir().join(format!("agentd-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, cwd) = (root.join("home"), root.join("work"));
        std::fs::create_dir_all(home.join(".config").join("agentd")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let home_s = home.to_string_lossy().into_owned();
        let envmap: HashMap<&str, &str> = [("HOME", home_s.as_str())].into_iter().collect();

        // An empty chain is not an error: agentd runs on its built-in defaults.
        assert!(discovered_chain(&cwd, &envmap).unwrap().is_empty());

        std::fs::write(home.join(".config/agentd/config.yml"), "a: 1\n").unwrap();
        std::fs::write(cwd.join("agentd.yml"), "b: 2\n").unwrap();
        std::fs::write(cwd.join("agentd.local.yml"), "c: 3\n").unwrap();
        let chain = discovered_chain(&cwd, &envmap).unwrap();
        assert_eq!(chain.len(), 3, "{chain:?}");
        assert!(chain[0].ends_with("config.yml"), "{chain:?}");
        assert!(chain[1].ends_with("agentd.yml"), "{chain:?}");
        assert!(chain[2].ends_with("agentd.local.yml"), "{chain:?}");

        // Two spellings of ONE rung is the coin toss nobody should debug.
        std::fs::write(cwd.join("agentd.yaml"), "b: 9\n").unwrap();
        let e = discovered_chain(&cwd, &envmap).unwrap_err();
        assert!(e.contains("project config is ambiguous"), "{e}");
        std::fs::remove_file(cwd.join("agentd.yaml")).unwrap();

        // XDG wins over HOME when both are set.
        let xdg = root.join("xdg");
        std::fs::create_dir_all(xdg.join("agentd")).unwrap();
        std::fs::write(xdg.join("agentd/config.yml"), "a: 7\n").unwrap();
        let xdg_s = xdg.to_string_lossy().into_owned();
        let envmap2: HashMap<&str, &str> = [
            ("HOME", home_s.as_str()),
            ("XDG_CONFIG_HOME", xdg_s.as_str()),
        ]
        .into_iter()
        .collect();
        let chain = discovered_chain(&cwd, &envmap2).unwrap();
        assert!(chain[0].starts_with(&xdg_s), "{chain:?}");

        // No HOME at all: no user rung, and no panic reaching for one.
        let bare: HashMap<&str, &str> = HashMap::new();
        assert_eq!(discovered_chain(&cwd, &bare).unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    // ──────────────────────────── config file ────────────────────────────────

    // ─────────────────────────── --watch-config ──────────────────────────────

    // ───────────────────────────  --validate-config  ─────────────────────────

    // ────────────────────────────  --config-schema  ──────────────────────────

    // ──────────────────────────────  secret refs  ────────────────────────────

    // ───────────────────────  hot-reload coherence  ──────────────────────────
}
