// SPDX-License-Identifier: AGPL-3.0-only
//! The Agent Provider (apd) client: enroll the durable key once for an
//! identity, then fetch + cache + proactively-refresh a short-lived
//! **agent token**. All signed with the agent's own key (RFC 9421, `hwk`
//! scheme — the agent has no token yet), so there is no shared secret.
//!
//! Dependency-free beyond the in-house HTTP client + `ring` (via [`super::key`]).

use super::key::AgentKey;
use super::sig::{self, SigKey};
use crate::b64;
use crate::net::http::{self, Url};
use serde::Deserialize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Refresh this early before the advertised expiry so an in-flight signed
/// request never rides a token that expires mid-call.
const REFRESH_SKEW: Duration = Duration::from_secs(60);
const DEFAULT_TTL: Duration = Duration::from_secs(3600);
/// A held token is refreshed ahead of the instant it stops being good, by
/// this much — or by half its life, for a token that lives less than twice
/// this — so the token a child is handed outlives the child.
const REFRESH_AHEAD: Duration = Duration::from_secs(300);

/// Static config for reaching an Agent Provider.
#[derive(Debug, Clone)]
pub struct ApdConfig {
    /// The apd base URL (e.g. `https://apd.example`).
    pub base_url: String,
    /// A one-time enrollment token, if the apd runs in `token` mode (the
    /// human/operator provides it). `None` for open/self-hosted mode.
    pub enrollment_token: Option<String>,
    /// Path to an enrollment-assertion file for the provider's `federated`
    /// gate — e.g. a Kubernetes projected ServiceAccount token. Read
    /// **fresh on every enroll** (the projected token rotates), so we hold the
    /// path, never the assertion. `None` when not using assertion enrollment.
    pub enroll_assertion_file: Option<String>,
    /// The user's chosen Person Server (`ps` claim), if this agent acts for a
    /// human under Case C. `None` for identity-only (Case A). Forwarded to
    /// enroll, and cross-checked against the `ps` the issued token carries.
    pub person_server: Option<String>,
    /// Platform hint (`workload`, `cli`, …).
    pub platform: String,
}

#[derive(Deserialize)]
struct EnrollResp {
    agent: String,
}

#[derive(Deserialize)]
struct TokenResp {
    agent_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    agent: Option<String>,
}

struct Cached {
    token: String,
    good_until: Instant,
    fetched: Instant,
}

impl Cached {
    /// Due a refresh ahead of [`Cached::good_until`] ([`REFRESH_AHEAD`]).
    fn due(&self, now: Instant) -> bool {
        let life = self.good_until.saturating_duration_since(self.fetched);
        self.good_until.saturating_duration_since(now) < REFRESH_AHEAD.min(life / 2)
    }
}

/// A caching Agent-Provider token source. Holds the agent identity + a live
/// agent token; `token()` returns a valid one, verifying the provider and
/// enrolling/refreshing under the hood. `Send + Sync`, cheap to share (one per
/// agent process).
pub struct ApdClient {
    config: ApdConfig,
    key: AgentKey,
    timeout: Duration,
    /// The provider's metadata was read and accepted: nothing is enrolled or
    /// fetched from it, and no token of its is presented, before.
    verified: AtomicBool,
    agent_id: Mutex<Option<String>>,
    cached: Mutex<Option<Cached>>,
    /// Held while a token is fetched: callers that find the token due at
    /// once share one fetch rather than each asking the provider.
    fetching: Mutex<()>,
    /// When the last fetch failed, and why: a caller that waited on it takes
    /// its failure rather than ask again, so a provider that does not answer
    /// costs the callers waiting one timeout, not one each.
    failed: Mutex<Option<(Instant, String)>>,
}

impl ApdClient {
    pub fn new(config: ApdConfig, key: AgentKey, timeout: Duration) -> ApdClient {
        ApdClient {
            config,
            key,
            timeout,
            verified: AtomicBool::new(false),
            agent_id: Mutex::new(None),
            cached: Mutex::new(None),
            fetching: Mutex::new(()),
            failed: Mutex::new(None),
        }
    }

    /// Start from what another process of the tree already holds: whether it
    /// verified the provider, the identity it enrolled, and its agent token
    /// with the instant it stops being good. Nothing is asked of the provider.
    pub(super) fn seeded(
        self,
        verified: bool,
        agent: Option<String>,
        token: Option<(String, Instant)>,
    ) -> ApdClient {
        self.verified.store(verified, Ordering::Release);
        *self.agent_id.lock().unwrap_or_else(|e| e.into_inner()) = agent;
        let fetched = Instant::now();
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) =
            token.map(|(token, good_until)| Cached {
                token,
                good_until,
                fetched,
            });
        self
    }

    /// The provider this client reaches, and how it enrolls.
    pub(super) fn config(&self) -> &ApdConfig {
        &self.config
    }

    /// Whether the provider's metadata has been verified.
    pub(super) fn verified(&self) -> bool {
        self.verified.load(Ordering::Acquire)
    }

    /// The agent token held now and the instant it stops being good, without
    /// asking the provider: `None` when there is none, or it is no longer
    /// good.
    pub(super) fn held_token(&self) -> Option<(String, Instant)> {
        let cache = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .as_ref()
            .filter(|c| Instant::now() < c.good_until)
            .map(|c| (c.token.clone(), c.good_until))
    }

    /// Whether the agent token is due a refresh: none is held, or the one
    /// held is near the instant it stops being good ([`REFRESH_AHEAD`]).
    pub(super) fn due(&self) -> bool {
        let cache = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        cache.as_ref().is_none_or(|c| c.due(Instant::now()))
    }

    /// The resolved agent identity (`aauth:local@domain`), once enrolled.
    pub fn agent_id(&self) -> Option<String> {
        self.agent_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The signing key. The request signer always signs with the agent's own
    /// key, even when the `Signature-Key` it presents is a bearer-looking
    /// agent/auth token: the token names the identity, the key proves it.
    pub(super) fn key(&self) -> &AgentKey {
        &self.key
    }

    /// A currently-valid agent token, refreshing when the cached one is within
    /// [`REFRESH_SKEW`] of expiry (or absent). Verifies the provider's
    /// metadata and enrolls first, until each has succeeded once. This is the
    /// whole "fully automatic, the user is never involved" refresh path.
    ///
    /// The cache is **in-memory by design**: the agent *key* is durable and
    /// `/enroll` is idempotent, so a restart costs only one cheap signed
    /// `/agent-token`. Persisting this short-lived JWT would add a
    /// secret-at-rest surface to save a round-trip that is usually already
    /// inside [`REFRESH_SKEW`] on the next start.
    pub fn token(&self) -> Result<String, String> {
        match self.held_token() {
            Some((token, _)) => Ok(token),
            None => self.fetch(false),
        }
    }

    /// A token fetched ahead of the held one's refresh, when it is
    /// [`Self::due`]; else the one held.
    pub(super) fn refresh(&self) -> Result<String, String> {
        self.fetch(true)
    }

    /// Fetch a token unless the one held is still good — and, `ahead`, not
    /// due a refresh — once the lock is taken: a caller that waited on another's
    /// fetch takes what it fetched, or how it failed.
    fn fetch(&self, ahead: bool) -> Result<String, String> {
        let asked = Instant::now();
        let _one = self.fetching.lock().unwrap_or_else(|e| e.into_inner());
        {
            let now = Instant::now();
            let cache = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = cache.as_ref()
                && now < c.good_until
                && !(ahead && c.due(now))
            {
                return Ok(c.token.clone());
            }
        }
        if let Some((at, e)) = &*self.failed.lock().unwrap_or_else(|e| e.into_inner())
            && *at > asked
        {
            return Err(e.clone());
        }
        let fetched = self
            .verify_provider_metadata()
            .map_err(|e| e.to_string())
            .and_then(|()| self.enroll_if_needed())
            .and_then(|()| self.fetch_token());
        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        match fetched {
            Ok(fresh) => {
                *failed = None;
                let token = fresh.token.clone();
                *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(fresh);
                Ok(token)
            }
            Err(e) => {
                *failed = Some((Instant::now(), e.clone()));
                Err(e)
            }
        }
    }

    /// Enroll the durable key for an identity, once (idempotent — a second call
    /// after `agent_id` is set is a no-op).
    fn enroll_if_needed(&self) -> Result<(), String> {
        if self.agent_id().is_some() {
            return Ok(());
        }
        let mut body = serde_json::json!({ "platform": self.config.platform });
        if let Some(t) = &self.config.enrollment_token {
            body["enrollment_token"] = serde_json::Value::String(t.clone());
        }
        // Federated gate: read the assertion FRESH on every enroll — a projected
        // SA token rotates, so a value cached at construction would go stale
        // across restarts/re-enrolls. Only the path travels in config and the
        // spawn payload; the short-lived token itself never touches either, nor
        // the logs.
        if let Some(path) = &self.config.enroll_assertion_file {
            let assertion = std::fs::read_to_string(path)
                .map_err(|e| format!("aauth: enrollment assertion file {path}: {e}"))?;
            let assertion = assertion.trim();
            if assertion.is_empty() {
                return Err(format!("aauth: enrollment assertion file {path} is empty"));
            }
            body["enrollment_assertion"] = serde_json::Value::String(assertion.to_string());
        }
        if let Some(ps) = &self.config.person_server {
            body["ps"] = serde_json::Value::String(ps.clone());
        }
        let resp: EnrollResp = self.signed_post("/enroll", &body)?;
        *self.agent_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(resp.agent);
        Ok(())
    }

    /// Validate the Agent-Provider metadata document: if the AP publishes
    /// `/.well-known/aauth-agent.json`, its `issuer` MUST match the configured
    /// provider (§12.10 anti-host-poisoning). An absent document is fine (the
    /// enroll/token endpoints are bootstrap conventions, not advertised); a
    /// contradicting or unparseable one is refused, and one that could not be
    /// read leaves the provider unverified. Asked until it succeeds once.
    pub(super) fn verify_provider_metadata(&self) -> Result<(), super::discover::MetadataError> {
        if self.verified() {
            return Ok(());
        }
        super::discover::fetch_agent_provider(&self.config.base_url, self.timeout)?;
        self.verified.store(true, Ordering::Release);
        Ok(())
    }

    fn fetch_token(&self) -> Result<Cached, String> {
        let resp: TokenResp = self.signed_post("/agent-token", &serde_json::json!({}))?;
        if let Some(agent) = resp.agent {
            *self.agent_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(agent);
        }
        // Act on the token's own claims: the agent token is a JWT, so its `exp`
        // is the authoritative refresh deadline (the AP may omit or disagree with
        // `expires_in`), its `iss` MUST be the provider we enrolled with, and its
        // `cnf.jwk` MUST be our signing key — a mismatch on either is fatal (every
        // signed request would be rejected), so we fail fast here rather than let
        // it surface as a downstream 401 storm. Opaque / non-JWT tokens parse to
        // `None` and fall back to `expires_in`.
        let exp = inspect_agent_token(
            &resp.agent_token,
            &self.key,
            Some(&self.config.base_url),
            self.config.person_server.as_deref(),
        )?;
        let ttl = exp
            .map(|e| Duration::from_secs(e.saturating_sub(sig::now_secs())))
            .or_else(|| resp.expires_in.map(Duration::from_secs))
            .unwrap_or(DEFAULT_TTL);
        let fetched = Instant::now();
        Ok(Cached {
            token: resp.agent_token,
            good_until: fetched + ttl.saturating_sub(REFRESH_SKEW),
            fetched,
        })
    }

    /// POST a JSON body to `{base}{path}`, signed with the durable key (hwk
    /// scheme — the apd verifies against the presented public key). Parses the
    /// JSON response into `T`.
    fn signed_post<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, String> {
        let full = format!("{}{path}", self.config.base_url.trim_end_matches('/'));
        let url = Url::parse(&full).map_err(|e| format!("aauth: apd url {full}: {e}"))?;
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        // The apd calls cover the body digest (integrity of the enroll/token
        // request), signed with the durable key via the hwk scheme.
        let digest = sig::content_digest(&bytes);
        let owned = sig::sign_request(
            &self.key,
            "POST",
            &url.host_header(),
            &url.path,
            SigKey::Hwk,
            sig::now_secs(),
            Some(&digest),
        );
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        for (k, v) in &owned {
            headers.push((k.as_str(), v.as_str()));
        }

        let mut stream = connect(&url, self.timeout)?;
        let resp = http::send(
            stream.as_mut(),
            &url.host_header(),
            "POST",
            &url.path,
            &headers,
            &bytes,
        )
        .map_err(|e| format!("aauth: apd {path}: {e}"))?;
        if !resp.is_success() {
            return Err(format!(
                "aauth: apd {path} returned HTTP {} ({})",
                resp.status,
                resp.header("signature-error")
                    .or_else(|| resp.header("aauth-error"))
                    .unwrap_or("no detail")
            ));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|e| format!("aauth: apd {path}: bad response: {e}"))
    }
}

fn connect(url: &Url, timeout: Duration) -> Result<Box<dyn http::Stream>, String> {
    let tcp = http::connect_tcp(&url.host, url.port, timeout)
        .map_err(|e| format!("aauth: connect {}: {e}", url.host))?;
    if url.is_tls() {
        #[cfg(feature = "tls")]
        {
            let s = crate::net::tls::connect(tcp, &url.host, None)
                .map_err(|e| format!("aauth: tls {}: {e}", url.host))?;
            Ok(Box::new(s))
        }
        #[cfg(not(feature = "tls"))]
        {
            Err("aauth: https apd requires --features tls".to_string())
        }
    } else {
        Ok(Box::new(tcp))
    }
}

/// Best-effort read of the claims we *act on* in the (JWT) agent token: the real
/// `exp`, the `iss` (must be the configured provider), the `ps` (must be the
/// configured person server), and the `cnf.jwk` proof-of-possession binding. We
/// do NOT verify the token signature — the downstream resource server / model
/// gateway does that; we only react to claims we can use locally.
///
/// Returns the token `exp` (unix seconds) when present. Returns `Err` on a
/// definite `iss` ≠ configured-provider, `ps` ≠ configured-person-server, or
/// `cnf.jwk` ≠ signing-key mismatch — each would make a signed request fail (or
/// route the PS exchange to the wrong server), so surfacing it here (at fetch)
/// beats a silent downstream failure. `expected_iss` / `expected_ps` are the
/// configured provider / person server; pass `None` to skip that check. An
/// opaque / non-JWT token, or one we can't parse, yields `Ok(None)` (caller falls
/// back to `expires_in`).
fn inspect_agent_token(
    token: &str,
    key: &AgentKey,
    expected_iss: Option<&str>,
    expected_ps: Option<&str>,
) -> Result<Option<u64>, String> {
    let Some(claims) = claims_of(token) else {
        return Ok(None); // not JWT-shaped → opaque token; nothing local to check
    };
    // iss: the token must come from the provider we enrolled with — a token
    // issued by some other AP is not one we should present.
    if let (Some(want), Some(iss)) = (expected_iss, claims.get("iss").and_then(|v| v.as_str()))
        && !super::discover::issuer_matches(iss, want)
    {
        return Err(format!(
            "aauth: agent token iss {iss:?} is not the configured provider {want:?}"
        ));
    }
    // ps: the protocol makes `ps` a per-agent-instance claim (§5.2.1), enrolled
    // from our config; if the AP echoed a DIFFERENT person server, Case C would
    // route the consent exchange — resource token and all — to the wrong PS.
    if let (Some(want), Some(ps)) = (expected_ps, claims.get("ps").and_then(|v| v.as_str()))
        && !super::discover::issuer_matches(ps, want)
    {
        return Err(format!(
            "aauth: agent token ps {ps:?} is not the configured person server {want:?}"
        ));
    }
    // cnf.jwk: agentd is single-key, so the token must bind our durable key (the
    // one we present + sign with). A mismatch never works — fail fast.
    if let Some(cnf) = claims.get("cnf").and_then(|c| c.get("jwk")) {
        let ours = key.public_jwk();
        let matches = ["kty", "crv", "x"]
            .iter()
            .all(|f| cnf.get(*f) == ours.get(*f));
        if !matches {
            return Err("aauth: agent token cnf.jwk does not match the signing key".into());
        }
    }
    Ok(claims.get("exp").and_then(|e| e.as_u64()))
}

/// The claims of a JWT-shaped token (header.payload.signature: the payload
/// segment, unverified); `None` for an opaque token or one that does not
/// parse.
fn claims_of(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&b64::url_decode(payload).ok()?).ok()
}

/// A JWT-shaped token's `exp` (unix seconds); `None` for an opaque token, or
/// one that carries none.
pub(super) fn exp_of(token: &str) -> Option<u64> {
    claims_of(token)?.get("exp")?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a JWT-shaped `header.payload.sig` string whose payload is `claims`.
    /// The header + signature are dummies — `inspect_agent_token` reads only the
    /// payload and never verifies the signature.
    fn jwt(claims: serde_json::Value) -> String {
        let payload = b64::url_nopad(serde_json::to_vec(&claims).unwrap().as_slice());
        format!("e30.{payload}.sig") // header "e30" = {}
    }

    fn test_key() -> AgentKey {
        AgentKey::from_seed(&[9u8; 32]).unwrap()
    }

    const AP: Option<&str> = Some("https://ap.example");
    const PS: Option<&str> = Some("https://ps.example");

    #[test]
    fn exp_is_read_from_the_token() {
        let key = test_key();
        let tok = jwt(serde_json::json!({ "exp": 1_800_000_000u64 }));
        assert_eq!(
            inspect_agent_token(&tok, &key, AP, PS).unwrap(),
            Some(1_800_000_000)
        );
    }

    #[test]
    fn matching_cnf_passes_and_absent_cnf_is_fine() {
        let key = test_key();
        // cnf.jwk == our public jwk → ok.
        let tok = jwt(serde_json::json!({ "exp": 42u64, "cnf": { "jwk": key.public_jwk() } }));
        assert_eq!(inspect_agent_token(&tok, &key, AP, PS).unwrap(), Some(42));
        // no cnf at all → still ok (nothing to check).
        let tok = jwt(serde_json::json!({ "exp": 42u64 }));
        assert_eq!(inspect_agent_token(&tok, &key, AP, PS).unwrap(), Some(42));
    }

    #[test]
    fn mismatched_cnf_is_a_hard_error() {
        let key = test_key();
        let other = AgentKey::from_seed(&[1u8; 32]).unwrap();
        let tok = jwt(serde_json::json!({ "exp": 42u64, "cnf": { "jwk": other.public_jwk() } }));
        let err = inspect_agent_token(&tok, &key, AP, PS).unwrap_err();
        assert!(err.contains("cnf.jwk"), "{err}");
    }

    #[test]
    fn matching_iss_passes_mismatched_iss_hard_errors() {
        let key = test_key();
        // iss == configured provider (trailing slash tolerated) → ok.
        let tok = jwt(serde_json::json!({ "exp": 42u64, "iss": "https://ap.example/" }));
        assert_eq!(inspect_agent_token(&tok, &key, AP, PS).unwrap(), Some(42));
        // iss is a DIFFERENT provider → fail fast.
        let tok = jwt(serde_json::json!({ "exp": 42u64, "iss": "https://evil.example" }));
        let err = inspect_agent_token(&tok, &key, AP, PS).unwrap_err();
        assert!(err.contains("iss"), "{err}");
        // expected_iss = None → issuer check skipped.
        assert_eq!(inspect_agent_token(&tok, &key, None, PS).unwrap(), Some(42));
    }

    #[test]
    fn matching_ps_passes_mismatched_ps_hard_errors() {
        let key = test_key();
        // ps == configured person server → ok.
        let tok = jwt(serde_json::json!({ "exp": 42u64, "ps": "https://ps.example" }));
        assert_eq!(inspect_agent_token(&tok, &key, AP, PS).unwrap(), Some(42));
        // ps is a DIFFERENT person server → fail fast.
        let tok = jwt(serde_json::json!({ "exp": 42u64, "ps": "https://other-ps.example" }));
        let err = inspect_agent_token(&tok, &key, AP, PS).unwrap_err();
        assert!(err.contains("ps"), "{err}");
        // no configured PS (identity-only, Case A) → ps check skipped.
        assert_eq!(inspect_agent_token(&tok, &key, AP, None).unwrap(), Some(42));
    }

    #[test]
    fn opaque_or_unparseable_token_is_legacy_none() {
        let key = test_key();
        // not JWT-shaped (no dots) → None, no error.
        assert_eq!(
            inspect_agent_token("opaque-token", &key, AP, PS).unwrap(),
            None
        );
        // JWT-shaped but payload isn't valid base64/JSON → None, no error.
        assert_eq!(inspect_agent_token("a.!!!.c", &key, AP, PS).unwrap(), None);
        // JWT with no exp claim → None (caller falls back to expires_in).
        let tok = jwt(serde_json::json!({ "sub": "aauth:x@ap" }));
        assert_eq!(inspect_agent_token(&tok, &key, AP, PS).unwrap(), None);
    }
}
