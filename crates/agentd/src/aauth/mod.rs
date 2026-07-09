// SPDX-License-Identifier: Apache-2.0
//! **AAuth [DRAFT]** — agent-side auth for calling AAuth-protected MCP servers
//! (RFC 0023). The agent holds an Ed25519 key, gets a short-lived agent token
//! from an Agent Provider, and **signs every MCP request** (RFC 9421); the MCP
//! server verifies the signature against the provider's keys and knows exactly
//! which agent is calling — no API key, no shared secret. [feature: aauth]
//!
//! ## What ships in this draft
//!
//! **Case A (identity-based MCP)** — the common case — is implemented end to
//! end: keygen/persist, enroll, agent-token cache+refresh, and per-request
//! signing wired onto the MCP transport as a [`::mcp::http::RequestSigner`].
//! **Case B (resource-managed)** is partially supported: an `AAuth-Access`
//! token returned by a server is adopted and presented on subsequent calls.
//! **Case C (Person-Server / user-scoped auth)** is scaffolded (the `ps` claim
//! is enrolled) but the interactive consent flow is a documented roadmap item
//! (RFC 0023 §Case C) — a `401 requirement=auth-token` currently surfaces as a
//! clear error rather than driving a PS consent round-trip.
//!
//! The whole feature is OFF by default; a binary built without it carries no
//! `ring` edge and no signing path.

mod apd;
mod b64;
mod key;
mod sig;

/// The per-request signer trait (from `agentd-mcp`), re-exported so an embedder
/// / test uses `agentd::aauth::RequestSigner` without depending on the mcp crate.
pub use ::mcp::http::RequestSigner;
pub use apd::{ApdClient, ApdConfig};
pub use key::AgentKey;

/// Verify an Ed25519 signature over `msg` against a 32-byte public key — the
/// operation a real AAuth MCP server performs on each request. Exposed so an
/// embedder (or a test) can check a produced signature without a direct `ring`
/// dependency. `Ok(())` = valid.
pub fn verify_ed25519(public_key: &[u8], msg: &[u8], signature: &[u8]) -> Result<(), String> {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(msg, signature)
        .map_err(|_| "aauth: signature verification failed".into())
}

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The process-global AAuth client (RFC 0023). Installed once — in the root and,
/// across the re-exec boundary, in each subagent from its spawn payload (the
/// agent has ONE identity for its whole process tree). The MCP connect
/// chokepoint (`mcp::from_spec`) reads it to sign outbound requests. Empty in a
/// binary that configured no `--aauth-provider`, so the signing path is inert.
static INSTALLED: OnceLock<AAuthClient> = OnceLock::new();

/// Install the process AAuth client (idempotent — a second call is ignored).
pub fn install(client: AAuthClient) {
    let _ = INSTALLED.set(client);
}

/// The installed client's request signer, if AAuth is configured. `mcp::from_spec`
/// attaches this to every server it connects when present.
pub fn signer() -> Option<Arc<dyn ::mcp::http::RequestSigner>> {
    INSTALLED
        .get()
        .map(|c| Arc::new(c.clone()) as Arc<dyn ::mcp::http::RequestSigner>)
}

/// The installed client (for priming at startup / the capabilities surface).
pub fn installed() -> Option<&'static AAuthClient> {
    INSTALLED.get()
}

/// Build + install the process AAuth client from settings (RFC 0023): load or
/// create the durable key, resolve a `{{secret:…}}` enrollment token, and
/// install the request signer. Called once by the root and by each subagent
/// (from its spawn payload). Idempotent; a bad key file / secret is a clear
/// `Err` the caller surfaces (exit 2 shape).
pub fn setup(settings: &crate::config::AAuthSettings, timeout: Duration) -> Result<(), String> {
    if installed().is_some() {
        return Ok(());
    }
    let key = AgentKey::load_or_create(std::path::Path::new(&settings.key_file))?;
    let enrollment_token = match &settings.enrollment_token {
        Some(tmpl) => Some(crate::sec::secret::resolve(tmpl, &|k| {
            std::env::var(k).ok()
        })?),
        None => None,
    };
    let config = ApdConfig {
        base_url: settings.provider.clone(),
        enrollment_token,
        person_server: settings.person_server.clone(),
        platform: "workload".into(),
    };
    install(AAuthClient::new(key, config, timeout));
    Ok(())
}

/// The one AAuth client an agent process holds: its identity + provider token
/// source, plus the live per-server `AAuth-Access` tokens (Case B). Cheap to
/// clone (`Arc` inside); installed on every MCP transport as the request signer.
#[derive(Clone)]
pub struct AAuthClient {
    apd: Arc<ApdClient>,
    /// Case-B opaque access tokens, keyed by authority (host[:port]).
    access: Arc<Mutex<std::collections::HashMap<String, String>>>,
}

impl AAuthClient {
    /// Build from a key + provider config. The key is loaded/created by the
    /// caller (`AgentKey::load_or_create`), so a self-hosted agent controls
    /// where its durable identity lives (RFC 0023 §Step 0).
    pub fn new(key: AgentKey, apd: ApdConfig, timeout: Duration) -> AAuthClient {
        AAuthClient {
            apd: Arc::new(ApdClient::new(apd, key, timeout)),
            access: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Enroll + fetch the first token now, so a misconfig (unreachable apd, bad
    /// enrollment token) fails at startup rather than on the first MCP call.
    /// Returns the resolved agent identity.
    pub fn prime(&self) -> Result<String, String> {
        self.apd.token()?;
        Ok(self.apd.agent_id().unwrap_or_default())
    }

    /// The resolved agent identity, once primed/first-signed.
    pub fn agent_id(&self) -> Option<String> {
        self.apd.agent_id()
    }

    /// Adopt an `AAuth-Access` token a server returned (Case B), scoped to the
    /// authority it came from.
    pub fn adopt_access(&self, authority: &str, token: &str) {
        self.access
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(authority.to_string(), token.to_string());
    }
}

/// The [`::mcp::http::RequestSigner`] impl: every outbound MCP request gets the
/// three RFC 9421 headers, signed with a live agent token. A token-fetch error
/// yields NO headers (the request goes unsigned and the server answers with its
/// AAuth requirement) rather than blocking the transport.
impl ::mcp::http::RequestSigner for AAuthClient {
    fn sign(&self, method: &str, authority: &str, path: &str) -> Vec<(String, String)> {
        let Ok(token) = self.apd.token() else {
            return Vec::new();
        };
        let mut headers: Vec<(String, String)> = sig::sign_request(
            self.apd.key(),
            method,
            authority,
            path,
            sig::SigKey::Jwt(&token),
            sig::now_secs(),
        )
        .into_iter()
        .collect();
        // Case B: if we hold an opaque access token for this authority, present
        // it as `Authorization: AAuth <token>` alongside the signature.
        if let Some(access) = self
            .access
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(authority)
        {
            headers.push(("Authorization".into(), format!("AAuth {access}")));
        }
        headers
    }

    fn capabilities(&self) -> Option<String> {
        // What interaction shapes this agent can drive on a 202/interaction.
        // The draft handles neither yet, so advertise nothing (omit the header).
        None
    }
}
