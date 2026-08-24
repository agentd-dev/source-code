// SPDX-License-Identifier: AGPL-3.0-only
// The MCP client itself lives in the reusable `mcp` crate; re-export it so that
// `crate::mcp::client::{McpClient, McpError}` resolves here. [`from_spec`] below is
// the agentd-side integration — config, auth and identity — that belongs in this crate.
pub use ::mcp::client;
// Re-export the transport module so the agentd integration (and its tests) can
// name the `RequestSigner` seam that credential providers plug into.
pub use ::mcp::http;

/// The **service pace registry** behind a catalog entry's `rate:`: one
/// process-global map, seeded at the [`from_spec`] chokepoint — the ONLY place
/// every process (reactor, turn worker, flat subagent) constructs its MCP
/// clients — so a rated catalog entry paces its consumers wherever the call
/// actually executes. Buckets are keyed by SERVICE name rather than by server,
/// so every server referencing one entry shares a single bucket per process.
pub mod pace {
    use crate::supervisor::tree::TokenBucket;
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Reg = HashMap<String, (String, String)>; // server → (service, rate)
    static REG: Mutex<Option<Reg>> = Mutex::new(None);
    static BUCKETS: Mutex<Option<HashMap<String, TokenBucket>>> = Mutex::new(None);

    /// Called from `from_spec` for a spec carrying a catalog rate.
    pub fn register(server: &str, service: &str, rate: &str) {
        let mut g = REG.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(HashMap::new)
            .insert(server.to_string(), (service.to_string(), rate.to_string()));
    }

    /// Spend one token toward `server`'s service, if it is rated. `Ok(())`
    /// when unrated or a token was available; `Err(msg)` (a refusal the
    /// caller reports as a tool error, never a crash) when the bucket is dry.
    pub fn take(server: &str) -> Result<(), String> {
        let (service, rate) = {
            let g = REG.lock().unwrap_or_else(|e| e.into_inner());
            match g.as_ref().and_then(|m| m.get(server)) {
                Some((s, r)) => (s.clone(), r.clone()),
                None => return Ok(()),
            }
        };
        let (burst, per_s) = crate::supervisor::tree::parse_rate(&rate)
            .map_err(|e| format!("services.{service}.rate: {e}"))?;
        let mut g = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());
        let b = g
            .get_or_insert_with(HashMap::new)
            .entry(service.clone())
            .or_insert_with(|| TokenBucket::new(burst, f64::from(burst) / per_s));
        if b.try_take() {
            Ok(())
        } else {
            let retry = (per_s / f64::from(burst.max(1))).ceil().max(1.0) as u32;
            Err(format!(
                "service '{service}' rate exceeded (services.{service}.rate: {rate} paces this process); retry in ~{retry}s"
            ))
        }
    }
}

/// Build an MCP client from a declared [`crate::config::McpServerSpec`]: resolve
/// its secret-free `{{secret:…}}` auth header templates (via [`auth`]) and connect
/// to the spec's remote `endpoint`, stamping agentd's client identity. The
/// config/auth-coupled counterpart of the crate's transport-only
/// [`client::McpClient::connect`]. Call `initialize` on the result before use.
pub fn from_spec(
    spec: &crate::config::McpServerSpec,
    timeout: std::time::Duration,
) -> Result<client::McpClient, client::McpError> {
    use client::{McpClient, McpError};
    if spec.endpoint.trim().is_empty() {
        return Err(McpError::Transport(format!(
            "mcp server '{}' has no endpoint",
            spec.name
        )));
    }
    let headers = auth::resolve_headers(&spec.headers).map_err(McpError::Transport)?;
    // AAuth: sign requests to this server with the agent identity.
    // Per-server opt-in — `spec.aauth == Some(false)` opts out; otherwise the
    // global default is "sign all when an identity is configured". The signing
    // path is absent entirely without `--features aauth`.
    #[cfg(feature = "aauth")]
    let aauth_signer = if spec.aauth == Some(false) {
        None
    } else {
        let s = crate::aauth::signer();
        // Learn the server's discovery metadata (content-digest requirement)
        // once at connect (best-effort). Only when we will actually sign it.
        if s.is_some()
            && let Some(client) = crate::aauth::installed()
        {
            let authority = ::mcp::http::authority_of(&spec.endpoint);
            client.discover(&authority, &spec.endpoint);
        }
        s
    };
    #[cfg(not(feature = "aauth"))]
    let aauth_signer: Option<std::sync::Arc<dyn ::mcp::http::RequestSigner>> = None;

    // Credential precedence: the unified `auth:` block wins (static / oauth2
    // device-login / client-credentials), then the narrower `oauth:`
    // client-credentials shortcut, then per-server AAuth signing. An endpoint
    // presents exactly one mechanism — they are never combined. The `auth:` and
    // `oauth:` paths are absent without `--features oauth`.
    #[cfg(feature = "oauth")]
    let signer: Option<std::sync::Arc<dyn ::mcp::http::RequestSigner>> = if let Some(a) = &spec.auth
    {
        // A server that references a catalog entry caches its credential under
        // `service:<entry>`, so every consumer of that entry shares one login
        // instead of each provoking its own. A standalone server keys its cache
        // per-server as `mcp:<name>`.
        let target = match &spec.service {
            Some(svc) => format!("service:{svc}"),
            None => format!("mcp:{}", spec.name),
        };
        crate::auth::device::signer_for(a, &target, timeout).map_err(McpError::Transport)?
    } else if let Some(o) = &spec.oauth {
        Some(
            std::sync::Arc::new(oauth::OAuthBearerSigner::new(o.clone(), timeout))
                as std::sync::Arc<dyn ::mcp::http::RequestSigner>,
        )
    } else {
        aauth_signer
    };
    #[cfg(not(feature = "oauth"))]
    let signer = aauth_signer;
    // A rated catalog entry paces its consumers within THIS process; registered
    // here because every process builds its clients through this one function.
    if let (Some(service), Some(rate)) = (&spec.service, &spec.rate) {
        pace::register(&spec.name, service, rate);
    }
    let client = McpClient::connect_signed(&spec.name, &spec.endpoint, headers, timeout, signer)?
        .with_client_info(::mcp::wire::Implementation {
            name: "agentd".into(),
            version: crate::VERSION.into(),
            title: None,
        });
    // SPIFFE X.509-SVID mTLS: set the transport client identity from the
    // SPIRE-written cert + key when the server declares `auth: {kind: spiffe,
    // svid: x509}`. Needs `--features tls` (mTLS); a JWT-SVID rides the signer seam.
    #[cfg(feature = "tls")]
    let client = match spiffe_x509_identity(spec)? {
        Some(id) => client.with_identity(id),
        None => client,
    };
    Ok(client)
}

/// Build a mutual-TLS client identity from a `kind: spiffe, svid: x509` auth
/// block (the SPIRE-written cert + key files). `None` for any other auth.
#[cfg(feature = "tls")]
fn spiffe_x509_identity(
    spec: &crate::config::McpServerSpec,
) -> Result<Option<crate::net::tls::ClientIdentity>, client::McpError> {
    use client::McpError;
    let Some(a) = &spec.auth else {
        return Ok(None);
    };
    if a.kind != "spiffe" || a.svid.as_deref() != Some("x509") {
        return Ok(None);
    }
    let cert_path = a
        .svid_file
        .as_deref()
        .ok_or_else(|| McpError::Transport("spiffe x509: svid_file is required".into()))?;
    let key_path = a
        .key_file
        .as_deref()
        .ok_or_else(|| McpError::Transport("spiffe x509: key_file is required".into()))?;
    let cert = std::fs::read(cert_path)
        .map_err(|e| McpError::Transport(format!("spiffe svid_file: {e}")))?;
    let key = std::fs::read(key_path)
        .map_err(|e| McpError::Transport(format!("spiffe key_file: {e}")))?;
    crate::net::tls::ClientIdentity::from_pem(&cert, &key)
        .map(Some)
        .map_err(|e| McpError::Transport(format!("spiffe svid: {e}")))
}

// Auth material resolution for remote MCP endpoints: materialize secret-free
// `{{secret:…}}` header templates into wire headers at connect time.
pub mod auth;
pub mod elicit;
// OAuth 2.1 client-credentials (M2M) token source for endpoints sitting behind an
// OAuth gateway. Feature-gated; dependency-free.
#[cfg(feature = "oauth")]
pub mod oauth;
// Built-in Streamable HTTP mock MCP server (the hidden `--internal-mock-mcp-http`
// mode) for the test + conformance suites: serves a one-resource reactive MCP over
// a unix socket, so the harness drives agentd's real HTTP transport end to end.
// In debug it is always present (so `cargo test` needs no flag); in release it
// ships only under `internal-mocks`, so the production binary carries no test
// scaffolding.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock_http;

// agentd-as-A2A-client: the remote-A2A-agent delegation backend. Connects to a
// declared peer over HTTP(S) with the JSON-RPC codec, runs `a2a.SendMessage` and
// then polls `a2a.GetTask` until the task reaches a terminal state, and returns
// the distillate. Reuses the wire types from `a2a`; adds no dependencies. The
// serving side of A2A is `runtime::a2a_server`.
#[cfg(feature = "a2a")]
pub mod a2a_client;
