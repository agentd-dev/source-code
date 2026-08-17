// SPDX-License-Identifier: Apache-2.0
// The MCP client now lives in the reusable `mcp` crate (`mcp::client`); re-export
// so `crate::mcp::client::{McpClient, McpError}` keeps resolving. `from_spec`
// below is the agentd integration (config + auth + identity) that stays here.
pub use ::mcp::client;
// Re-export the transport module so the agentd integration (and its tests) can
// name the `RequestSigner` seam that credential providers (RFC 0031) plug into.
pub use ::mcp::http;

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
    // AAuth (RFC 0023): sign requests to this server with the agent identity.
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

    // Credential precedence (RFC 0031 §5): the unified `auth:` block wins (static
    // / oauth2 device-login / client-credentials), then the legacy `oauth:`
    // client-credentials shortcut, then per-server AAuth signing. An endpoint uses
    // one mechanism. The `auth:`/`oauth:` paths are absent without `--features oauth`.
    #[cfg(feature = "oauth")]
    let signer: Option<std::sync::Arc<dyn ::mcp::http::RequestSigner>> = if let Some(a) = &spec.auth
    {
        crate::auth::device::signer_for(a, &format!("mcp:{}", spec.name), timeout)
            .map_err(McpError::Transport)?
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
    let client = McpClient::connect_signed(&spec.name, &spec.endpoint, headers, timeout, signer)?
        .with_client_info(::mcp::wire::Implementation {
            name: "agentd".into(),
            version: crate::VERSION.into(),
            title: None,
        });
    // SPIFFE X.509-SVID mTLS (RFC 0031 §9): set the transport client identity from
    // the SPIRE-written cert + key when the server declares `auth: {kind: spiffe,
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

// The Streamable HTTP client transport (RFC 0004) now lives in the reusable `mcp`
// crate as `mcp::http`; `client` uses it directly (`::mcp::http`).
// Auth material resolution for remote MCP endpoints (RFC 0012 §3.7): materialize
// secret-free `{{secret:…}}` header templates into wire headers at connect time.
pub mod auth;
pub mod elicit;
// OAuth 2.1 client-credentials (M2M) token source for endpoints behind an OAuth
// gateway (RFC 0006 §auth). Feature-gated; dependency-free.
#[cfg(feature = "oauth")]
pub mod oauth;
// Built-in Streamable HTTP mock MCP server (the hidden `--internal-mock-mcp-http`
// mode, v2.0.0) for the test + conformance suites: serves a one-resource reactive
// MCP over a unix socket, so the harness drives agentd's HTTP transport end to end.
// In debug it's always present (so `cargo test` works with no flag); in release it
// ships only under `internal-mocks`, so the production binary carries no test
// scaffolding.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock_http;

// A2A client-side wire helpers (`TaskState` + request/response shaping) shared
// with `a2a_client`. The v1 self-MCP server + v1 A2A server surfaces were removed
// with the mode cut-over; the v2 A2A server is `runtime::a2a_server`.
#[cfg(feature = "a2a")]
pub mod a2a_wire;

// agentd-as-A2A-client: the remote-A2A-agent delegation backend (RFC 0020 §3).
// Connects to a declared peer over HTTP(S) + the RFC 0004
// JSON-RPC codec, runs `a2a.SendMessage` then polls `a2a.GetTask` to a terminal
// state, and returns the distillate. Reuses the wire types from `a2a`; no deps.
#[cfg(feature = "a2a")]
pub mod a2a_client;
