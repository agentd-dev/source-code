// SPDX-License-Identifier: AGPL-3.0-only
//! **AAuth** (a draft agent-identity protocol) — agent-side auth for calling
//! AAuth-protected MCP servers. The agent holds an Ed25519 key, gets a
//! short-lived agent token from an Agent Provider, and **signs every MCP
//! request** (RFC 9421); the MCP server verifies the signature against the
//! provider's keys and knows exactly which agent is calling — no API key, no
//! shared secret. [feature: aauth]
//!
//! ## The three access modes
//!
//! A server decides how it wants to be reached; this client covers all three:
//! - **Case A (identity-based):** keygen/persist, enroll, agent-token
//!   cache+refresh, and per-request RFC 9421 signing on every MCP request.
//! - **Case B (resource-managed):** an `AAuth-Access` token a server returns is
//!   adopted and presented (`Authorization: AAuth …`) on the retry + later calls.
//! - **Case C (Person-Server / user-scoped):** a `401 requirement=auth-token`
//!   drives the Person-Server exchange ([`ps`]) — the human consents there —
//!   and the resulting user-scoped auth token is presented as the
//!   `Signature-Key` instead of the agent token. It is held for the
//!   authority AND the principal the request was made for (the request's
//!   scope, `mcp::http::SCOPE_META`): one principal's token is never
//!   presented on another's call, and a call made for nobody presents none
//!   and earns none.
//!
//! Every mode starts from a signed request, so none sends one unsigned: a
//! request whose identity cannot be produced is not sent.
//!
//! The transport runs the reaction loop (sign → inspect → satisfy → re-sign →
//! retry, bounded); discovery ([`discover`]) learns a server's
//! `content-digest` requirement (which the signature then covers). The whole
//! feature is OFF by default; a binary built without it carries no `ring` edge
//! and no signing path.

mod apd;
mod discover;
mod key;
mod ps;
mod sig;

/// The per-request signer trait (from `agentd-mcp`), re-exported so an embedder
/// / test uses `agentd::aauth::RequestSigner` without depending on the mcp crate.
pub use ::mcp::http::RequestSigner;
pub use apd::{ApdClient, ApdConfig};
pub use discover::MetadataError;
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

use crate::subagent::protocol::{AAuthHandoff, AAuthKey, HandedServer, HandedToken};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The target the identity's outcomes are recorded under
/// ([`crate::auth::report`]): `auth.unavailable` and `auth.recovered` name it.
const TARGET: &str = "aauth";

/// After a refresh ahead of time fails, the next is not tried before this:
/// a provider that does not answer is not asked on every tick. A request
/// that needs a token still asks itself.
const RETRY_BEHIND: Duration = Duration::from_secs(30);

/// The file an instance child is composed a key held only in memory in,
/// in the child's own directory.
const INSTANCE_KEY_FILE: &str = "aauth-key";

/// The process-global AAuth client. Installed once — in the root by
/// [`setup`] and, across the re-exec boundary, in each turn worker and
/// subagent from its spawn payload by [`adopt`] (the agent has ONE identity
/// for its whole process tree). The MCP connect chokepoint (`mcp::from_spec`),
/// the model dial and the A2A client read it to sign outbound requests. Empty
/// in a process that configured no `--aauth-provider`, so the signing path is
/// inert.
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

/// The installed client (for discovery at connect / the capabilities surface).
pub fn installed() -> Option<&'static AAuthClient> {
    INSTALLED.get()
}

/// Refresh the installed client's agent token, in the background, when it
/// is near the instant it stops being good — so the supervisor holds a
/// token every child it spawns can be handed, however long it was idle.
/// Called on each reactor tick; asks the provider for nothing until the
/// token is due.
pub(crate) fn keep_fresh() {
    if let Some(client) = installed() {
        client.keep_fresh();
    }
}

/// The principal the request being signed is made for: the scope its
/// `_meta` named ([`::mcp::http::request_scope`]), which the host stamps
/// from the work's authority, never from anything a caller wrote.
fn acting() -> Option<String> {
    ::mcp::http::request_scope()
}

/// How a child hands a token it earned for an authority to the supervisor:
/// the authority, then the Case B access token or the Case C auth token,
/// then the principal a Case C token is for.
type HandUp = Box<dyn Fn(&str, Option<&str>, Option<&str>, Option<&str>) + Send + Sync>;

static HAND_UP: OnceLock<HandUp> = OnceLock::new();

/// Hand the tokens this child earns to `up`, which sends them to the
/// supervisor ([`crate::subagent::protocol::AgentMsg::AAuthToken`]). Set
/// once, in a turn worker or a subagent.
pub(crate) fn hand_up(
    up: impl Fn(&str, Option<&str>, Option<&str>, Option<&str>) + Send + Sync + 'static,
) {
    let _ = HAND_UP.set(Box::new(up));
}

/// A token a child earned for `authority` — a Case C one for `principal` —
/// kept by the installed client, so the children spawned after it are handed
/// it: the Case C one only children working for that principal.
pub(crate) fn heard_token(
    authority: &str,
    access: Option<&str>,
    auth_token: Option<&str>,
    principal: Option<&str>,
) {
    if let Some(client) = installed() {
        client.heard_token(authority, access, auth_token, principal);
    }
}

/// Build + install the root's AAuth client from settings: load the durable
/// key — or create it, when `create_key` — resolve a `{{secret:…}}`
/// enrollment token, verify the Agent Provider's metadata and fetch the first
/// agent token, then install the request signer. Idempotent. A bad key file
/// or secret, or provider metadata that is refused — an `issuer` that is not
/// the provider, a document that does not parse — is a clear `Err` the caller
/// surfaces (exit 2). A provider that does not answer is not: the failure is
/// reported, and every request that needs the provider fails, unsent, until
/// it answers.
pub fn setup(settings: &crate::config::AAuthSettings, timeout: Duration) -> Result<(), String> {
    if installed().is_some() {
        return Ok(());
    }
    let path = std::path::Path::new(&settings.key_file);
    let key = if settings.create_key {
        AgentKey::load_or_create(path)?
    } else {
        AgentKey::load(path)?
    };
    let enrollment_token = match &settings.enrollment_token {
        Some(tmpl) => Some(
            crate::sec::secret::resolve(tmpl, &|k| std::env::var(k).ok())
                .map_err(|e| format!("aauth: enroll_token: {e}"))?,
        ),
        None => None,
    };
    let config = ApdConfig {
        base_url: settings.provider.clone(),
        enrollment_token,
        enroll_assertion_file: settings.enroll_assertion_file.clone(),
        person_server: settings.person_server.clone(),
        platform: "workload".into(),
    };
    let client = AAuthClient::new(key, config, timeout).with_key_file(&settings.key_file);
    client.start()?;
    install(client);
    Ok(())
}

/// Install the identity the supervisor handed this child
/// ([`AAuthClient::handoff`]). Nothing is asked of the provider. A key file
/// that cannot be read, or that no longer holds the supervisor's key, is an
/// `Err`, reported: the child would have nothing to sign with.
pub(crate) fn adopt(handed: &AAuthHandoff, timeout: Duration) -> Result<(), String> {
    if installed().is_some() {
        return Ok(());
    }
    match AAuthClient::from_handoff(handed, timeout) {
        Ok(client) => install(client),
        // Only a failure is reported: a key that loads says nothing of
        // whether its agent token can be produced.
        Err(e) => return report(Err(e)),
    }
    Ok(())
}

/// The installed identity as a turn worker or a subagent working for
/// `principal` is handed it, when this process has one: the Case C tokens
/// held for that principal and no other's.
pub(crate) fn handoff(principal: Option<&str>) -> Option<AAuthHandoff> {
    installed().map(|c| c.handoff(principal))
}

/// The `security.aauth` an instance child is composed, so the child daemon
/// signs as this process does: the same provider and the same key, read from
/// the same file, which must still hold this process's key — the child loads
/// it and never makes one. A daemon reads its identity from its config, so a
/// key held only in memory is written for it to `<dir>/aauth-key` (`0600`, in
/// the child's `0700` directory) until the child exits
/// ([`forget_instance_key`]). The child enrolls that key and fetches its own
/// agent token when it starts, as any daemon does: no token is written into a
/// file. `enroll_token` is the operator's, and rides only as a `{{secret:…}}`
/// reference. `None` when this process has no identity.
pub(crate) fn instance_settings(
    dir: &std::path::Path,
    enroll_token: Option<&str>,
) -> Result<Option<serde_json::Value>, String> {
    installed()
        .map(|client| client.instance_settings(dir, enroll_token))
        .transpose()
}

/// Remove the copy of a key held only in memory that [`instance_settings`]
/// wrote to an instance child's directory `dir`, once the child has exited or
/// was never started: a respawn is composed the key again. Never the file the
/// key was loaded from.
pub(crate) fn forget_instance_key(dir: &std::path::Path) {
    forget_key_copy(dir, installed().and_then(|c| c.key_file.as_deref()));
}

/// [`forget_instance_key`], given the file the installed key was loaded
/// from.
fn forget_key_copy(dir: &std::path::Path, loaded_from: Option<&str>) {
    let copy = dir.join(INSTANCE_KEY_FILE);
    if loaded_from.is_none_or(|f| std::path::Path::new(f) != copy) {
        let _ = std::fs::remove_file(copy);
    }
}

/// Record an attempt to produce the identity under [`TARGET`], which the
/// error then starts with — so the cause loses its own `aauth: `.
fn report<T>(attempt: Result<T, String>) -> Result<T, String> {
    crate::auth::report::outcome(
        TARGET,
        attempt.map_err(|e| match e.strip_prefix("aauth: ") {
            Some(cause) => cause.to_string(),
            None => e,
        }),
    )
}

/// The unix-ms time that `at` is.
fn unix_ms_at(at: Instant) -> u64 {
    crate::auth::cache::now_ms()
        .saturating_add(at.saturating_duration_since(Instant::now()).as_millis() as u64)
}

/// The instant that unix-ms time `ms` is; one already past is now.
fn instant_at(ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(ms.saturating_sub(crate::auth::cache::now_ms()))
}

/// Per-authority AAuth state a client accumulates as it talks to servers.
#[derive(Default)]
struct AuthorityState {
    /// Case B: an opaque `AAuth-Access` token → `Authorization: AAuth <t>`.
    access: std::collections::HashMap<String, String>,
    /// Case C: a user-scoped auth-token (JWT) → presented as the `Signature-Key`
    /// instead of the agent token. Keyed by authority and principal.
    auth_token: std::collections::HashMap<(String, String), String>,
    /// Discovery: whether the server requires a `content-digest` cover, for
    /// each server that answered.
    wants_digest: std::collections::HashMap<String, bool>,
}

impl AuthorityState {
    /// The state as a child working for `principal` is handed it: its Case
    /// C tokens, and no other principal's. A Case C token within a minute of
    /// its `exp` is left out: a child earns a fresh one rather than present
    /// one the server is about to refuse.
    fn handed(&self, principal: Option<&str>) -> std::collections::BTreeMap<String, HandedServer> {
        let mut servers = std::collections::BTreeMap::<String, HandedServer>::new();
        for (authority, t) in &self.access {
            servers.entry(authority.clone()).or_default().access = Some(t.clone());
        }
        let live = sig::now_secs() + 60;
        for ((authority, p), t) in &self.auth_token {
            if Some(p.as_str()) == principal && apd::exp_of(t).is_none_or(|exp| exp > live) {
                servers.entry(authority.clone()).or_default().auth_token = Some(t.clone());
            }
        }
        for (authority, d) in &self.wants_digest {
            servers.entry(authority.clone()).or_default().content_digest = Some(*d);
        }
        servers
    }

    /// The state a child working for `principal` was handed.
    fn from_handed(
        servers: &std::collections::BTreeMap<String, HandedServer>,
        principal: Option<&str>,
    ) -> AuthorityState {
        let mut st = AuthorityState::default();
        for (authority, s) in servers {
            if let Some(t) = &s.access {
                st.access.insert(authority.clone(), t.clone());
            }
            if let (Some(t), Some(p)) = (&s.auth_token, principal) {
                st.auth_token
                    .insert((authority.clone(), p.to_string()), t.clone());
            }
            if let Some(d) = s.content_digest {
                st.wants_digest.insert(authority.clone(), d);
            }
        }
        st
    }
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
    /// The file the key was loaded from; `None` for a key held only in memory.
    key_file: Option<String>,
    /// A refresh ahead of time is still running.
    refreshing: Arc<AtomicBool>,
    /// The last refresh ahead of time failed: the next waits until then.
    retry_at: Arc<Mutex<Option<Instant>>>,
    /// A child's client: it was handed what the supervisor knows of each
    /// server, and asks no server's discovery document the supervisor read.
    handed: bool,
}

impl AAuthClient {
    /// Build from a key + provider config. The key is loaded/created by the
    /// caller (`AgentKey::load_or_create`), so a self-hosted agent controls
    /// where its durable identity lives.
    pub fn new(key: AgentKey, apd: ApdConfig, timeout: Duration) -> AAuthClient {
        AAuthClient::over(ApdClient::new(apd, key, timeout), timeout)
    }

    fn over(apd: ApdClient, timeout: Duration) -> AAuthClient {
        AAuthClient {
            person_server: apd.config().person_server.clone(),
            apd: Arc::new(apd),
            timeout,
            state: Arc::new(Mutex::new(AuthorityState::default())),
            key_file: None,
            refreshing: Arc::new(AtomicBool::new(false)),
            retry_at: Arc::new(Mutex::new(None)),
            handed: false,
        }
    }

    /// The file the key was loaded from. A child process is handed this path,
    /// and reads the key there, rather than the key itself; a client without
    /// one hands its children the key.
    pub fn with_key_file(mut self, path: impl Into<String>) -> AAuthClient {
        self.key_file = Some(path.into());
        self
    }

    /// Enroll + fetch the first token now, so a misconfig (unreachable apd, bad
    /// enrollment token) fails here rather than on the first MCP call.
    /// Returns the resolved agent identity. First validates the Agent-Provider
    /// metadata document: a host-poisoned `aauth-agent.json` (issuer ≠ the
    /// configured provider) aborts here rather than after we have already
    /// enrolled our durable public key with the wrong party.
    pub fn prime(&self) -> Result<String, String> {
        self.apd.token()?;
        Ok(self.apd.agent_id().unwrap_or_default())
    }

    /// The daemon's start: verify the provider's metadata, then fetch the
    /// first agent token. Refused metadata is the `Err`. A provider that does
    /// not answer, or that will not enroll the key or issue a token, is
    /// reported, and the next request that needs it asks again.
    fn start(&self) -> Result<(), String> {
        match self.apd.verify_provider_metadata() {
            Ok(()) => {
                let _ = report(self.apd.token());
                Ok(())
            }
            Err(MetadataError::Invalid(e)) => Err(e),
            Err(unreachable) => {
                let _ = report::<()>(Err(unreachable.to_string()));
                Ok(())
            }
        }
    }

    /// The agent token, its outcome reported.
    fn agent_token(&self) -> Result<String, String> {
        report(self.apd.token())
    }

    /// The resolved agent identity, once primed/first-signed.
    pub fn agent_id(&self) -> Option<String> {
        self.apd.agent_id()
    }

    /// This client as a turn worker or a subagent is handed it: the key (the
    /// file it was loaded from, or the seed of one held only in memory), the
    /// provider, the agent token held now, and what it knows of each server.
    /// Asks the provider for nothing: a token that is no longer good is not
    /// handed, and one near that is refreshed in the background
    /// ([`Self::keep_fresh`]), so the children that follow are handed a fresh
    /// one.
    pub(crate) fn handoff(&self, principal: Option<&str>) -> AAuthHandoff {
        let held = self.apd.held_token();
        self.keep_fresh();
        let servers = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handed(principal);
        let config = self.apd.config();
        let key = self.apd.key();
        AAuthHandoff {
            provider: config.base_url.clone(),
            key: match &self.key_file {
                Some(path) => AAuthKey::File(path.clone()),
                None => AAuthKey::Seed(key.seed_b64()),
            },
            thumbprint: key.thumbprint(),
            enrollment_token: config.enrollment_token.clone(),
            enroll_assertion_file: config.enroll_assertion_file.clone(),
            person_server: config.person_server.clone(),
            verified: self.apd.verified(),
            agent: self.apd.agent_id(),
            token: held.map(|(token, good_until)| HandedToken {
                token,
                good_until_ms: unix_ms_at(good_until),
            }),
            servers,
            principal: principal.map(str::to_string),
        }
    }

    /// Refresh the agent token in the background when none is held or the
    /// one held is near the instant it stops being good; asks the provider
    /// for nothing otherwise.
    fn keep_fresh(&self) {
        if self.apd.due() {
            self.refresh_behind();
        }
    }

    /// Fetch an agent token off the caller's thread, one fetch at a time, and
    /// not again within [`RETRY_BEHIND`] of one that failed.
    fn refresh_behind(&self) {
        let waiting = self
            .retry_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|at| Instant::now() < at);
        if waiting || self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let client = self.clone();
        let spawned = std::thread::Builder::new()
            .name("agentd-aauth".into())
            .spawn(move || {
                let failed = report(client.apd.refresh()).is_err();
                *client.retry_at.lock().unwrap_or_else(|e| e.into_inner()) =
                    failed.then(|| Instant::now() + RETRY_BEHIND);
                client.refreshing.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.refreshing.store(false, Ordering::Release);
        }
    }

    /// [`heard_token`] for this client. A Case C token for nobody is not
    /// kept.
    fn heard_token(
        &self,
        authority: &str,
        access: Option<&str>,
        auth_token: Option<&str>,
        principal: Option<&str>,
    ) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = access {
            st.access.insert(authority.to_string(), a.to_string());
        }
        if let (Some(t), Some(p)) = (auth_token, principal) {
            st.auth_token
                .insert((authority.to_string(), p.to_string()), t.to_string());
        }
    }

    /// Hand a token this child earned for `authority` up to the supervisor,
    /// in a process that has one.
    fn earned(
        &self,
        authority: &str,
        access: Option<&str>,
        auth_token: Option<&str>,
        principal: Option<&str>,
    ) {
        if let Some(up) = HAND_UP.get() {
            up(authority, access, auth_token, principal);
        }
    }

    /// [`instance_settings`] for this client.
    fn instance_settings(
        &self,
        dir: &std::path::Path,
        enroll_token: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let key_file = match &self.key_file {
            Some(path) => {
                let held = AgentKey::load(std::path::Path::new(path))?;
                if held.thumbprint() != self.apd.key().thumbprint() {
                    return Err(format!(
                        "aauth: key file {path} no longer holds this agent's key"
                    ));
                }
                path.clone()
            }
            None => {
                let path = dir.join(INSTANCE_KEY_FILE);
                self.apd.key().write_over(&path)?;
                path.display().to_string()
            }
        };
        let config = self.apd.config();
        let mut a = serde_json::json!({"provider": config.base_url, "key_file": key_file});
        if let Some(t) = enroll_token.filter(|t| crate::sec::secret::has_secret_ref(t)) {
            a["enroll_token"] = serde_json::json!(t);
        }
        if let Some(f) = &config.enroll_assertion_file {
            a["enroll_assertion_file"] = serde_json::json!(f);
        }
        if let Some(ps) = &config.person_server {
            a["person_server"] = serde_json::json!(ps);
        }
        Ok(a)
    }

    /// The client a child builds from what it was handed.
    fn from_handoff(handed: &AAuthHandoff, timeout: Duration) -> Result<AAuthClient, String> {
        let (key, key_file) = match &handed.key {
            AAuthKey::File(path) => (AgentKey::load(std::path::Path::new(path))?, Some(path)),
            AAuthKey::Seed(seed) => {
                let seed = crate::b64::url_decode(seed)
                    .map_err(|e| format!("aauth: the handed key: {e}"))?;
                (AgentKey::from_seed(&seed)?, None)
            }
        };
        if key.thumbprint() != handed.thumbprint {
            return Err(match key_file {
                Some(path) => {
                    format!("aauth: key file {path} no longer holds the supervisor's key")
                }
                None => "aauth: the handed key is not the supervisor's".into(),
            });
        }
        let config = ApdConfig {
            base_url: handed.provider.clone(),
            enrollment_token: handed.enrollment_token.clone(),
            enroll_assertion_file: handed.enroll_assertion_file.clone(),
            person_server: handed.person_server.clone(),
            platform: "workload".into(),
        };
        let apd = ApdClient::new(config, key, timeout).seeded(
            handed.verified,
            handed.agent.clone(),
            handed
                .token
                .as_ref()
                .map(|t| (t.token.clone(), instant_at(t.good_until_ms))),
        );
        let mut client = AAuthClient::over(apd, timeout);
        client.state = Arc::new(Mutex::new(AuthorityState::from_handed(
            &handed.servers,
            handed.principal.as_deref(),
        )));
        client.handed = true;
        Ok(match key_file {
            Some(path) => client.with_key_file(path),
            None => client,
        })
    }

    /// Learn a server's discovery metadata: record whether it requires a
    /// `content-digest` cover. Best-effort; called by `from_spec` at connect.
    /// `endpoint` is the MCP server URL. A child asks no server the
    /// supervisor heard from; a server that does not answer leaves what was
    /// known of it.
    pub fn discover(&self, authority: &str, endpoint: &str) {
        if self.handed
            && self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .wants_digest
                .contains_key(authority)
        {
            return;
        }
        if let Some(digest) = discover::fetch(endpoint, self.timeout) {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .wants_digest
                .insert(authority.to_string(), digest);
        }
    }

    /// Adopt an `AAuth-Access` token a server returned (Case B), scoped to the
    /// authority it came from. A child hands it up, so the children spawned
    /// after it present it too.
    pub fn adopt_access(&self, authority: &str, token: &str) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .access
            .insert(authority.to_string(), token.to_string());
        self.earned(authority, Some(token), None, None);
    }
}

/// The [`::mcp::http::RequestSigner`] impl — sign every request, and react to
/// the server's `AAuth-Requirement` so a 401 that names a satisfiable
/// requirement becomes one retry rather than a hard failure.
impl ::mcp::http::RequestSigner for AAuthClient {
    /// Sign the request with the agent's key, presenting the user-scoped auth
    /// token held for `authority` and the principal the request is made for
    /// (Case C), or else the agent token. An agent
    /// token that cannot be produced — a provider that does not answer or is
    /// not verified, that will not enroll the key or issue a token, or issues
    /// one that is not ours — is the `Err`, and the request is not sent.
    fn sign(
        &self,
        method: &str,
        authority: &str,
        path: &str,
        body: &[u8],
    ) -> Result<Vec<(String, String)>, String> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let auth_token =
            acting().and_then(|p| st.auth_token.get(&(authority.to_string(), p)).cloned());
        let want_digest = *st.wants_digest.get(authority).unwrap_or(&false);
        let access = st.access.get(authority).cloned();
        drop(st);
        // Case C: a user-scoped auth token held for this authority is presented
        // as the key id (still signed with our own key); else the agent token.
        let key_id = match auth_token {
            Some(t) => t,
            None => self.agent_token()?,
        };

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
        Ok(headers)
    }

    fn on_response(&self, resp: &::mcp::http::AuthResponse, authority: &str) -> bool {
        // Case B: adopt an issued access token; retry so the next request
        // presents it.
        if let Some(access) = &resp.access {
            self.adopt_access(authority, access);
            return true;
        }
        // Case C: a user-scoped auth-token is required. Run the Person-Server
        // exchange (the human consents there) and cache the resulting token,
        // for the principal this request is made for — a request made for
        // nobody earns none.
        let Some(requirement) = &resp.requirement else {
            return false;
        };
        if ps::wants_auth_token(requirement) {
            let Some(principal) = acting() else {
                return false;
            };
            let key = (authority.to_string(), principal.clone());
            let already = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .auth_token
                .contains_key(&key);
            if already {
                return false; // already have one; a fresh 401 is a real denial
            }
            let (Some(ps_url), Some(resource_token), Ok(agent_token)) = (
                self.person_server.as_deref(),
                ps::resource_token(requirement),
                self.agent_token(),
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
                    self.earned(authority, None, Some(&auth_token), Some(&principal));
                    self.state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .auth_token
                        .insert(key, auth_token);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// Every request a mock provider served, as `METHOD /path`, in order.
    type Served = Arc<Mutex<Vec<String>>>;

    /// A mock Agent Provider on `listener`: its metadata names `issuer`, or
    /// its own origin when `None`; it enrolls any key and issues `tok-<n>`,
    /// counting from 1, that expire in an hour.
    fn serve_provider(listener: TcpListener, issuer: Option<&str>) -> Served {
        serve_provider_for(listener, issuer, 3600)
    }

    /// [`serve_provider`], its tokens expiring in `ttl` seconds.
    fn serve_provider_for(listener: TcpListener, issuer: Option<&str>, ttl: u64) -> Served {
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let issuer = issuer.map(str::to_string).unwrap_or(origin);
        let served = Served::default();
        let log = Arc::clone(&served);
        std::thread::spawn(move || {
            let mut issued = 0;
            for mut s in listener.incoming().flatten() {
                let line = read_line(&mut s);
                log.lock().unwrap().push(line.clone());
                let body = if line.contains("/.well-known/aauth-agent.json") {
                    format!(r#"{{"issuer":"{issuer}"}}"#)
                } else if line.contains("/enroll") {
                    r#"{"agent":"aauth:tree@apd.test"}"#.to_string()
                } else {
                    issued += 1;
                    format!(r#"{{"agent_token":"tok-{issued}","expires_in":{ttl}}}"#)
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        served
    }

    /// Read one request off `s`; returns its method and path.
    fn read_line(s: &mut TcpStream) -> String {
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).ok();
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':')
                && k.eq_ignore_ascii_case("content-length")
            {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; len];
        let _ = r.read_exact(&mut body);
        line.split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn client(provider: &str, seed: u8) -> AAuthClient {
        AAuthClient::new(
            AgentKey::from_seed(&[seed; 32]).unwrap(),
            ApdConfig {
                base_url: provider.to_string(),
                enrollment_token: None,
                enroll_assertion_file: None,
                person_server: None,
                platform: "workload".into(),
            },
            Duration::from_secs(2),
        )
    }

    /// The `Signature-Key` a set of signature headers presents.
    fn presented(headers: &[(String, String)]) -> &str {
        headers
            .iter()
            .find(|(k, _)| k == "Signature-Key")
            .map(|(_, v)| v.as_str())
            .unwrap_or_default()
    }

    /// Whether `headers` sign `POST host/mcp` with `key` — what an AAuth
    /// server checks.
    fn signed_by(headers: &[(String, String)], key: &AgentKey) -> bool {
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let params = get("Signature-Input");
        let params = params.trim_start_matches("sig=");
        let base = format!(
            "\"@method\": POST\n\"@authority\": host\n\"@path\": /mcp\n\"signature-key\": {}\n\"@signature-params\": {params}",
            get("Signature-Key")
        );
        let sig = get("Signature");
        let raw = sig.trim_start_matches("sig=:").trim_end_matches(':');
        crate::b64::url_decode(raw)
            .is_ok_and(|s| verify_ed25519(key.public_bytes(), base.as_bytes(), &s).is_ok())
    }

    /// A provider that does not answer leaves its agent token unproduced: the
    /// request is refused, not signed with nothing or sent unsigned. Once it
    /// answers, its metadata is read before the key is enrolled or a token
    /// fetched, and the request is signed.
    #[test]
    fn a_request_whose_agent_token_cannot_be_produced_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let c = client(&format!("http://{addr}"), 3);
        assert!(
            c.start().is_ok(),
            "a provider that does not answer does not refuse the start"
        );
        assert!(!c.apd.verified());
        let err = c.sign("POST", "host", "/mcp", b"{}").unwrap_err();
        assert!(err.starts_with("aauth: provider metadata"), "{err}");

        let served = serve_provider(TcpListener::bind(addr).unwrap(), None);
        let headers = c.sign("POST", "host", "/mcp", b"{}").expect("signed");
        assert_eq!(presented(&headers), "sig=jwt;jwt=\"tok-1\"");
        assert!(signed_by(&headers, c.apd.key()));
        assert_eq!(
            *served.lock().unwrap(),
            [
                "GET /.well-known/aauth-agent.json",
                "POST /enroll",
                "POST /agent-token"
            ]
        );
    }

    /// Metadata naming another issuer refuses the start, before the key is
    /// enrolled with whoever served it.
    #[test]
    fn provider_metadata_naming_another_issuer_refuses_the_start() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let provider = format!("http://{}", listener.local_addr().unwrap());
        let served = serve_provider(listener, Some("https://evil.example"));
        let c = client(&provider, 3);
        let err = c.start().unwrap_err();
        assert!(err.contains("possible host-poisoning"), "{err}");
        assert!(c.sign("POST", "host", "/mcp", b"{}").is_err());
        assert!(
            served.lock().unwrap().iter().all(|r| r.starts_with("GET ")),
            "nothing was enrolled: {:?}",
            served.lock().unwrap()
        );
    }

    /// A child handed the supervisor's identity signs with the supervisor's
    /// key and presents its agent token, and asks the provider for nothing.
    #[test]
    fn a_child_signs_as_the_supervisor_without_asking_the_provider() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let provider = format!("http://{}", listener.local_addr().unwrap());
        let served = serve_provider(listener, None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.key");
        let key = AgentKey::load_or_create(&path).unwrap();
        let root = AAuthClient::new(
            AgentKey::load(&path).unwrap(),
            client(&provider, 0).apd.config().clone(),
            Duration::from_secs(2),
        )
        .with_key_file(path.display().to_string());
        root.start().unwrap();
        let asked = served.lock().unwrap().len();
        assert_eq!(asked, 3, "{:?}", served.lock().unwrap());

        // Over the spawn pipe, as JSON.
        let handed = root.handoff(None);
        assert_eq!(handed.key, AAuthKey::File(path.display().to_string()));
        let wire = serde_json::to_string(&handed).unwrap();
        let child = AAuthClient::from_handoff(
            &serde_json::from_str(&wire).unwrap(),
            Duration::from_secs(2),
        )
        .expect("adopted");
        let headers = child.sign("POST", "host", "/mcp", b"{}").expect("signed");
        assert_eq!(presented(&headers), "sig=jwt;jwt=\"tok-1\"");
        assert!(
            signed_by(&headers, &key),
            "signed with the supervisor's key"
        );
        assert_eq!(child.agent_id().as_deref(), Some("aauth:tree@apd.test"));
        assert_eq!(
            served.lock().unwrap().len(),
            asked,
            "the child asked nothing"
        );

        // A key held only in memory is handed as its seed.
        let ephemeral = client(&provider, 8);
        ephemeral.start().unwrap();
        let handed = ephemeral.handoff(None);
        assert!(matches!(handed.key, AAuthKey::Seed(_)));
        let child = AAuthClient::from_handoff(&handed, Duration::from_secs(2)).unwrap();
        let headers = child.sign("POST", "host", "/mcp", b"{}").unwrap();
        assert!(signed_by(&headers, ephemeral.apd.key()));
    }

    /// A child never signs as anyone but the supervisor: a key file that now
    /// holds another key is refused, and a missing one is not created.
    #[test]
    fn a_child_refuses_a_key_file_that_is_not_the_supervisors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.key");
        let root = AAuthClient::new(
            AgentKey::load_or_create(&path).unwrap(),
            client("http://127.0.0.1:9", 0).apd.config().clone(),
            Duration::from_secs(2),
        )
        .with_key_file(path.display().to_string());
        let handed = root.handoff(None);

        std::fs::remove_file(&path).unwrap();
        AgentKey::load_or_create(&path).unwrap();
        let err = AAuthClient::from_handoff(&handed, Duration::from_secs(2))
            .err()
            .unwrap();
        assert!(
            err.contains("no longer holds the supervisor's key"),
            "{err}"
        );

        std::fs::remove_file(&path).unwrap();
        let err = AAuthClient::from_handoff(&handed, Duration::from_secs(2))
            .err()
            .unwrap();
        assert!(err.contains("read key"), "{err}");
        assert!(!path.exists(), "a child does not make a key of its own");
    }

    /// A handoff that finds no token to hand fetches one behind it, so the
    /// next child is handed a token instead of fetching its own.
    #[test]
    fn a_handoff_with_no_token_fetches_one_for_the_next() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let provider = format!("http://{}", listener.local_addr().unwrap());
        let served = serve_provider(listener, None);
        let root = client(&provider, 5);
        assert!(root.handoff(None).token.is_none(), "nothing fetched yet");
        let deadline = Instant::now() + Duration::from_secs(10);
        let next = loop {
            let handed = root.handoff(None);
            if handed.token.is_some() || Instant::now() > deadline {
                break handed;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(next.verified);
        assert_eq!(next.token.map(|t| t.token).as_deref(), Some("tok-1"));
        assert_eq!(
            served.lock().unwrap().len(),
            3,
            "one fetch, behind the handoffs"
        );
    }

    /// A JWT-shaped token whose payload is `claims` (unsigned: agentd reads
    /// only the claims it acts on).
    fn jwt(claims: serde_json::Value) -> String {
        format!(
            "e30.{}.sig",
            crate::b64::url_nopad(&serde_json::to_vec(&claims).unwrap())
        )
    }

    /// A server that serves `doc` as its AAuth resource metadata; returns its
    /// MCP endpoint and how many times it was asked.
    fn serve_resource(doc: &'static str) -> (String, Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(0));
        let count = Arc::clone(&asked);
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                read_line(&mut s);
                *count.lock().unwrap() += 1;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{doc}",
                    doc.len()
                );
            }
        });
        (endpoint, asked)
    }

    /// A child is handed what the tree knows of each server it signs for —
    /// the access token a server issued, the user-scoped token the Person
    /// Server issued for it, whether it wants a `content-digest` — and so
    /// presents them without earning them, and asks no discovery document
    /// the supervisor read. A user-scoped token about to expire is not handed.
    #[test]
    fn a_child_is_handed_what_the_tree_knows_of_each_server() {
        let (endpoint, asked) = serve_resource(r#"{"content_digest":true}"#);
        let authority = ::mcp::http::authority_of(&endpoint);
        let root = client("http://127.0.0.1:9", 4);
        root.discover(&authority, &endpoint);
        assert_eq!(*asked.lock().unwrap(), 1);
        let user = jwt(serde_json::json!({"exp": sig::now_secs() + 3600}));
        let ending = jwt(serde_json::json!({"exp": sig::now_secs() + 10}));
        root.heard_token(&authority, Some("acc-1"), Some(&user), Some("user:alice"));
        root.heard_token("ending.example", None, Some(&ending), Some("user:alice"));
        // Another principal's child is handed the server's access token and
        // none of the user-scoped ones.
        let bob = root.handoff(Some("user:bob"));
        assert_eq!(bob.servers[&authority].access.as_deref(), Some("acc-1"));
        assert!(bob.servers[&authority].auth_token.is_none(), "{bob:?}");

        let handed = root.handoff(Some("user:alice"));
        assert!(!handed.servers.contains_key("ending.example"), "{handed:?}");
        let wire = serde_json::to_string(&handed).unwrap();
        let child = AAuthClient::from_handoff(
            &serde_json::from_str(&wire).unwrap(),
            Duration::from_secs(2),
        )
        .unwrap();
        // The child works for alice: its requests are made for her.
        let headers = ::mcp::http::with_request_scope(Some("user:alice"), || {
            child.sign("POST", &authority, "/mcp", b"{}")
        })
        .unwrap();
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(header("Signature-Key"), format!("sig=jwt;jwt=\"{user}\""));
        assert_eq!(header("Authorization"), "AAuth acc-1");
        assert!(
            header("Signature-Input").contains("\"content-digest\""),
            "{headers:?}"
        );
        child.discover(&authority, &endpoint);
        assert_eq!(*asked.lock().unwrap(), 1, "the child asked nothing");

        // A server the supervisor never heard from is the child's to ask.
        let (other, asked) = serve_resource("{}");
        child.discover(&::mcp::http::authority_of(&other), &other);
        assert_eq!(*asked.lock().unwrap(), 1);
    }

    /// A token a child earns from a server is handed up to the supervisor,
    /// which hands it to the children spawned after.
    #[test]
    fn a_token_a_child_earns_is_handed_to_the_children_after_it() {
        type Earned = Arc<Mutex<Vec<(String, Option<String>)>>>;
        let earned = Earned::default();
        let up = Arc::clone(&earned);
        hand_up(move |authority, access, _, _| {
            up.lock()
                .unwrap()
                .push((authority.to_string(), access.map(str::to_string)))
        });
        let root = client("http://127.0.0.1:9", 6);
        let child = AAuthClient::from_handoff(&root.handoff(None), Duration::from_secs(2)).unwrap();
        let issued = ::mcp::http::AuthResponse {
            status: 401,
            access: Some("acc-7".into()),
            ..Default::default()
        };
        assert!(child.on_response(&issued, "b.example"), "retried with it");
        let up: Vec<_> = earned
            .lock()
            .unwrap()
            .iter()
            .filter(|(a, _)| a == "b.example")
            .cloned()
            .collect();
        assert_eq!(up, [("b.example".to_string(), Some("acc-7".to_string()))]);

        // The supervisor's reactor keeps what it heard.
        root.heard_token("b.example", Some("acc-7"), None, None);
        assert_eq!(
            root.handoff(None).servers["b.example"].access.as_deref(),
            Some("acc-7")
        );
    }

    /// The token held is refreshed ahead of the instant it stops being good,
    /// so the next child is handed a fresh one while the held one is still
    /// handed meanwhile; a supervisor idle across that instant still holds
    /// one to hand.
    #[test]
    fn a_token_near_its_end_is_refreshed_ahead() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let provider = format!("http://{}", listener.local_addr().unwrap());
        // Good for two seconds: due a refresh one second in.
        let served = serve_provider_for(listener, None, 62);
        let root = client(&provider, 7);
        root.start().unwrap();
        let good_until = Instant::now() + Duration::from_secs(2);
        let token = |c: &AAuthClient| c.apd.held_token().map(|(t, _)| t);
        assert_eq!(token(&root).as_deref(), Some("tok-1"));
        root.keep_fresh();
        assert_eq!(served.lock().unwrap().len(), 3, "not due yet");

        std::thread::sleep(Duration::from_millis(1100));
        let handed = root.handoff(None);
        assert_eq!(handed.token.map(|t| t.token).as_deref(), Some("tok-1"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while token(&root).as_deref() == Some("tok-1") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(Instant::now() < good_until, "refreshed only once it ended");
        assert_eq!(token(&root).as_deref(), Some("tok-2"));
        assert_eq!(
            served.lock().unwrap().last().map(String::as_str),
            Some("POST /agent-token")
        );
    }

    /// A refresh ahead of time that fails is not tried again on every tick:
    /// a provider that does not answer is asked once in [`RETRY_BEHIND`].
    #[test]
    fn a_failed_refresh_waits_before_the_next() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let provider = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(0));
        let count = Arc::clone(&asked);
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                read_line(&mut s);
                *count.lock().unwrap() += 1;
                let _ = write!(
                    s,
                    "HTTP/1.1 503 Busy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        let root = client(&provider, 8);
        let deadline = Instant::now() + Duration::from_secs(5);
        while root.retry_at.lock().unwrap().is_none() && Instant::now() < deadline {
            root.keep_fresh();
            std::thread::sleep(Duration::from_millis(10));
        }
        for _ in 0..20 {
            root.keep_fresh();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*asked.lock().unwrap(), 1, "asked once, then waited");
    }

    /// Callers that find no token at once share one fetch, and its failure:
    /// a provider that accepts and never answers costs the callers waiting on
    /// it one timeout, not one each.
    #[test]
    fn callers_at_once_share_one_fetch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let provider = format!("http://{}", listener.local_addr().unwrap());
        let held = Arc::new(Mutex::new(Vec::new()));
        let hold = Arc::clone(&held);
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                hold.lock().unwrap().push(s);
            }
        });
        let apd = Arc::new(ApdClient::new(
            client(&provider, 10).apd.config().clone(),
            AgentKey::from_seed(&[10; 32]).unwrap(),
            Duration::from_millis(500),
        ));
        let start = Instant::now();
        let callers: Vec<_> = (0..4)
            .map(|_| {
                let apd = Arc::clone(&apd);
                std::thread::spawn(move || apd.token())
            })
            .collect();
        for c in callers {
            assert!(c.join().unwrap().is_err());
        }
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(held.lock().unwrap().len(), 1, "asked once");
    }

    /// An instance child is composed the agent's own key file only while it
    /// still holds the agent's key: one that was replaced or removed refuses
    /// the spawn, and is not made again. A key held only in memory is written
    /// for the child, and that copy alone is removed after it.
    #[test]
    fn an_instance_child_is_composed_only_the_agents_own_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.key");
        let root = AAuthClient::new(
            AgentKey::load_or_create(&path).unwrap(),
            client("http://127.0.0.1:9", 0).apd.config().clone(),
            Duration::from_secs(2),
        )
        .with_key_file(path.display().to_string());
        let child_dir = dir.path().join("inst-1");
        std::fs::create_dir(&child_dir).unwrap();
        let composed = root.instance_settings(&child_dir, None).unwrap();
        assert_eq!(composed["key_file"], path.display().to_string());

        std::fs::remove_file(&path).unwrap();
        AgentKey::generate().unwrap().persist(&path).unwrap();
        let err = root.instance_settings(&child_dir, None).unwrap_err();
        assert!(err.contains("no longer holds this agent's key"), "{err}");
        std::fs::remove_file(&path).unwrap();
        let err = root.instance_settings(&child_dir, None).unwrap_err();
        assert!(err.contains("read key"), "{err}");
        assert!(!path.exists(), "no key was made for the child");

        let ephemeral = client("http://127.0.0.1:9", 9);
        let composed = ephemeral.instance_settings(&child_dir, None).unwrap();
        let copy = child_dir.join(INSTANCE_KEY_FILE);
        assert_eq!(composed["key_file"], copy.display().to_string());
        assert_eq!(
            AgentKey::load(&copy).unwrap().thumbprint(),
            ephemeral.apd.key().thumbprint()
        );
        // The file a key was loaded from is never taken for a copy.
        forget_key_copy(&child_dir, Some(&copy.display().to_string()));
        assert!(copy.exists());
        forget_key_copy(&child_dir, None);
        assert!(!copy.exists(), "the copy is removed once the child is done");
    }

    /// A user-scoped (Case C) token is held for the authority AND the
    /// principal it was earned for: presented only on that principal's
    /// requests — never another's, never a request made for nobody — and a
    /// request made for nobody does not run the Person-Server exchange.
    #[test]
    fn a_user_scoped_token_is_presented_for_its_principal_alone() {
        let root = client("http://127.0.0.1:9", 9);
        let alice = jwt(serde_json::json!({"exp": sig::now_secs() + 3600}));
        root.heard_token("mcp.example", None, Some(&alice), Some("user:alice"));
        // A Case C token for nobody is not kept.
        root.heard_token("mcp.example", None, Some("orphan"), None);
        let key_of = |p: Option<&str>| {
            ::mcp::http::with_request_scope(p, || root.sign("POST", "mcp.example", "/mcp", b"{}"))
                .map(|h| presented(&h).to_string())
        };
        assert_eq!(
            key_of(Some("user:alice")).unwrap(),
            format!("sig=jwt;jwt=\"{alice}\"")
        );
        for other in [Some("user:bob"), None] {
            // The agent token is all another principal's request may carry;
            // this provider does not answer, so there is none to sign with.
            let e = key_of(other).unwrap_err();
            assert!(!e.contains(&alice), "{other:?}: {e}");
        }
        let wants = ::mcp::http::AuthResponse {
            status: 401,
            requirement: Some(r#"auth-token; resource-token="RT""#.into()),
            ..Default::default()
        };
        assert!(
            !::mcp::http::with_request_scope(None, || root.on_response(&wants, "other.example")),
            "a request made for nobody earns no user-scoped token"
        );
    }
}
