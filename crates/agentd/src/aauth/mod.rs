// SPDX-License-Identifier: Apache-2.0
//! **AAuth [DRAFT]** — agent-side auth for calling AAuth-protected MCP servers
//! (RFC 0023). The agent holds an Ed25519 key, gets a short-lived agent token
//! from an Agent Provider, and **signs every MCP request** (RFC 9421); the MCP
//! server verifies the signature against the provider's keys and knows exactly
//! which agent is calling — no API key, no shared secret. [feature: aauth]
//!
//! ## What ships
//!
//! All three access modes, end to end (RFC 0023 §5, the request loop):
//! - **Case A (identity-based):** keygen/persist, enroll, agent-token
//!   cache+refresh, and per-request RFC 9421 signing on every MCP request.
//! - **Case B (resource-managed):** an `AAuth-Access` token a server returns is
//!   adopted and presented (`Authorization: AAuth …`) on the retry + later calls.
//! - **Case C (Person-Server / user-scoped):** a `401 requirement=auth-token`
//!   drives the Person-Server exchange ([`ps`]) — the human consents there —
//!   and the resulting user-scoped auth token is presented as the
//!   `Signature-Key` instead of the agent token.
//!
//! The transport runs the reaction loop (sign → inspect → satisfy → re-sign →
//! retry, bounded); discovery ([`discover`]) learns a server's
//! `content-digest` requirement (which the signature then covers). The whole
//! feature is OFF by default; a binary built without it carries no `ring` edge
//! and no signing path.

mod apd;
mod b64;
mod discover;
mod key;
mod ps;
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
        enroll_assertion_file: settings.enroll_assertion_file.clone(),
        person_server: settings.person_server.clone(),
        platform: "workload".into(),
    };
    install(AAuthClient::new(key, config, timeout));
    Ok(())
}

/// Per-authority AAuth state a client accumulates as it talks to servers.
#[derive(Default)]
struct AuthorityState {
    /// Case B: an opaque `AAuth-Access` token → `Authorization: AAuth <t>`.
    access: std::collections::HashMap<String, String>,
    /// Case C: a user-scoped auth-token (JWT) → presented as the `Signature-Key`
    /// instead of the agent token.
    auth_token: std::collections::HashMap<String, String>,
    /// Discovery: whether the server requires a `content-digest` cover.
    wants_digest: std::collections::HashMap<String, bool>,
}

/// The one AAuth client an agent process holds: its identity + provider token
/// source, the user's Person Server (Case C), and the per-authority state it
/// learns. Cheap to clone (`Arc` inside); installed on every MCP transport as
/// the request signer.
#[derive(Clone)]
pub struct AAuthClient {
    apd: Arc<ApdClient>,
    person_server: Option<String>,
    timeout: Duration,
    state: Arc<Mutex<AuthorityState>>,
}

impl AAuthClient {
    /// Build from a key + provider config. The key is loaded/created by the
    /// caller (`AgentKey::load_or_create`), so a self-hosted agent controls
    /// where its durable identity lives (RFC 0023 §Step 0).
    pub fn new(key: AgentKey, apd: ApdConfig, timeout: Duration) -> AAuthClient {
        let person_server = apd.person_server.clone();
        AAuthClient {
            apd: Arc::new(ApdClient::new(apd, key, timeout)),
            person_server,
            timeout,
            state: Arc::new(Mutex::new(AuthorityState::default())),
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

    /// Learn a server's discovery metadata (RFC 0023 §Step 3): record whether it
    /// requires a `content-digest` cover. Best-effort; called by `from_spec` at
    /// connect. `endpoint` is the MCP server URL.
    pub fn discover(&self, authority: &str, endpoint: &str) {
        if let Some(meta) = discover::fetch(endpoint, self.timeout) {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .wants_digest
                .insert(authority.to_string(), meta.content_digest);
        }
    }

    /// Adopt an `AAuth-Access` token a server returned (Case B), scoped to the
    /// authority it came from.
    pub fn adopt_access(&self, authority: &str, token: &str) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .access
            .insert(authority.to_string(), token.to_string());
    }
}

/// The [`::mcp::http::RequestSigner`] impl (RFC 0023 §5) — sign every request,
/// and react to the server's `AAuth-Requirement`.
impl ::mcp::http::RequestSigner for AAuthClient {
    fn sign(
        &self,
        method: &str,
        authority: &str,
        path: &str,
        body: &[u8],
    ) -> Vec<(String, String)> {
        let Ok(agent_token) = self.apd.token() else {
            return Vec::new();
        };
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Case C: if we hold a user-scoped auth token for this authority, present
        // IT as the key id (still signed with our own key); else the agent token.
        let key_id = st.auth_token.get(authority).cloned().unwrap_or(agent_token);
        let want_digest = *st.wants_digest.get(authority).unwrap_or(&false);
        let access = st.access.get(authority).cloned();
        drop(st);

        let digest = want_digest.then(|| sig::content_digest(body));
        let mut headers = sig::sign_request(
            self.apd.key(),
            method,
            authority,
            path,
            sig::SigKey::Jwt(&key_id),
            sig::now_secs(),
            digest.as_deref(),
        );
        // Case B: present an adopted opaque access token alongside the signature.
        if let Some(a) = access {
            headers.push(("Authorization".into(), format!("AAuth {a}")));
        }
        headers
    }

    fn on_response(&self, resp: &::mcp::http::AuthResponse, authority: &str) -> bool {
        // Case B: adopt an issued access token; retry so the next request
        // presents it.
        if let Some(access) = &resp.access {
            self.adopt_access(authority, access);
            return true;
        }
        // Case C: a user-scoped auth-token is required. Run the Person-Server
        // exchange (the human consents there) and cache the resulting token.
        let Some(requirement) = &resp.requirement else {
            return false;
        };
        if ps::wants_auth_token(requirement) {
            let already = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .auth_token
                .contains_key(authority);
            if already {
                return false; // already have one; a fresh 401 is a real denial
            }
            let (Some(ps_url), Some(resource_token), Ok(agent_token)) = (
                self.person_server.as_deref(),
                ps::resource_token(requirement),
                self.apd.token(),
            ) else {
                return false; // no PS configured, or no resource token → can't satisfy
            };
            let justification = format!(
                "agent {} requests access to {authority}",
                self.agent_id().unwrap_or_default()
            );
            match ps::exchange(
                ps_url,
                self.apd.key(),
                &agent_token,
                &resource_token,
                &justification,
                self.timeout,
            ) {
                Ok(auth_token) => {
                    self.state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .auth_token
                        .insert(authority.to_string(), auth_token);
                    return true; // retry, now presenting the auth token
                }
                Err(_) => return false, // denied / timed out — surface the 401
            }
        }
        // `agent-token` (Case A) is already satisfied by our proactive signing;
        // a repeat 401 there is a real rejection.
        false
    }

    fn wants_content_digest(&self, authority: &str) -> bool {
        *self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .wants_digest
            .get(authority)
            .unwrap_or(&false)
    }
}
