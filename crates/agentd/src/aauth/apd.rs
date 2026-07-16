// SPDX-License-Identifier: Apache-2.0
//! The Agent Provider (apd) client (RFC 0023 §Step 1–2): enroll the durable key
//! once for an identity, then fetch + cache + proactively-refresh a short-lived
//! **agent token**. All signed with the agent's own key (RFC 9421, `hwk`
//! scheme — the agent has no token yet), so there is no shared secret.
//!
//! Dependency-free beyond the in-house HTTP client + `ring` (via [`super::key`]).

use super::b64;
use super::key::AgentKey;
use super::sig::{self, SigKey};
use crate::net::http::{self, Url};
use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Refresh this early before the advertised expiry so an in-flight signed
/// request never rides a token that expires mid-call.
const REFRESH_SKEW: Duration = Duration::from_secs(60);
const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// Static config for reaching an Agent Provider.
#[derive(Debug, Clone)]
pub struct ApdConfig {
    /// The apd base URL (e.g. `https://apd.example`).
    pub base_url: String,
    /// A one-time enrollment token, if the apd runs in `token` mode
    /// (RFC 0023 §Step 1 — the human/operator provides it). `None` for
    /// open/self-hosted mode.
    pub enrollment_token: Option<String>,
    /// Path to an enrollment-assertion file for the provider's `federated` gate
    /// (RFC 0023 §5.1) — e.g. a Kubernetes projected ServiceAccount token. Read
    /// **fresh on every enroll** (the projected token rotates), so we hold the
    /// path, never the assertion. `None` when not using assertion enrollment.
    pub enroll_assertion_file: Option<String>,
    /// The user's chosen Person Server (`ps` claim), if this agent acts for a
    /// human under Case C. `None` for identity-only (Case A). Forwarded to
    /// enroll; the PS consent flow itself is a roadmap item (RFC 0023 §Case C).
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
}

/// A caching Agent-Provider token source. Holds the agent identity + a live
/// agent token; `token()` returns a valid one, enrolling/refreshing under the
/// hood. `Send + Sync`, cheap to share (one per agent process).
pub struct ApdClient {
    config: ApdConfig,
    key: AgentKey,
    timeout: Duration,
    agent_id: Mutex<Option<String>>,
    cached: Mutex<Option<Cached>>,
}

impl ApdClient {
    pub fn new(config: ApdConfig, key: AgentKey, timeout: Duration) -> ApdClient {
        ApdClient {
            config,
            key,
            timeout,
            agent_id: Mutex::new(None),
            cached: Mutex::new(None),
        }
    }

    /// The resolved agent identity (`aauth:local@domain`), once enrolled.
    pub fn agent_id(&self) -> Option<String> {
        self.agent_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The signing key (the request signer signs with the agent's own key even
    /// when presenting an agent-token key id — RFC 0023 §Step 6).
    pub(super) fn key(&self) -> &AgentKey {
        &self.key
    }

    /// A currently-valid agent token, refreshing when the cached one is within
    /// [`REFRESH_SKEW`] of expiry (or absent). Enrolls first if needed. This is
    /// the whole "fully automatic, the user is never involved" refresh path.
    pub fn token(&self) -> Result<String, String> {
        {
            let cache = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = cache.as_ref()
                && Instant::now() < c.good_until
            {
                return Ok(c.token.clone());
            }
        }
        self.enroll_if_needed()?;
        let fresh = self.fetch_token()?;
        let token = fresh.token.clone();
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(fresh);
        Ok(token)
    }

    /// Enroll the durable key for an identity, once (idempotent — a second call
    /// after `agent_id` is set is a no-op). RFC 0023 §Step 1.
    fn enroll_if_needed(&self) -> Result<(), String> {
        if self.agent_id().is_some() {
            return Ok(());
        }
        let mut body = serde_json::json!({ "platform": self.config.platform });
        if let Some(t) = &self.config.enrollment_token {
            body["enrollment_token"] = serde_json::Value::String(t.clone());
        }
        // Federated gate (RFC 0023 §5.1): read the assertion FRESH on every
        // enroll — a projected SA token rotates, so a value cached at construction
        // would go stale across restarts/re-enrolls. The path rode the spawn
        // payload; the short-lived token never touches config or logs.
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

    fn fetch_token(&self) -> Result<Cached, String> {
        let resp: TokenResp = self.signed_post("/agent-token", &serde_json::json!({}))?;
        if let Some(agent) = resp.agent {
            *self.agent_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(agent);
        }
        // Act on the token's own claims (RFC 0023 §7.1 G4): the agent token is a
        // JWT, so its `exp` is the authoritative refresh deadline (the AP may omit
        // or disagree with `expires_in`), and its `cnf.jwk` MUST be our signing
        // key — a mismatch is fatal (every signed request would be rejected), so
        // we fail fast here rather than let it surface as a downstream 401 storm.
        // Opaque / non-JWT tokens parse to `None` and fall back to `expires_in`.
        let exp = inspect_agent_token(&resp.agent_token, &self.key)?;
        let ttl = exp
            .map(|e| Duration::from_secs(e.saturating_sub(sig::now_secs())))
            .or_else(|| resp.expires_in.map(Duration::from_secs))
            .unwrap_or(DEFAULT_TTL);
        Ok(Cached {
            token: resp.agent_token,
            good_until: Instant::now() + ttl.saturating_sub(REFRESH_SKEW),
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
/// `exp` and the `cnf.jwk` proof-of-possession binding. We do NOT verify the
/// token signature — the downstream resource server / model gateway does that;
/// we only react to claims we can use locally.
///
/// Returns the token `exp` (unix seconds) when present. Returns `Err` only on a
/// definite `cnf.jwk` ≠ signing-key mismatch — which would make every signed
/// request fail, so surfacing it here (at fetch) beats a silent 401 storm.
/// An opaque / non-JWT token, or one we can't parse, yields `Ok(None)` and is
/// treated exactly as before (caller falls back to `expires_in`).
fn inspect_agent_token(token: &str, key: &AgentKey) -> Result<Option<u64>, String> {
    // JWT = header.payload.signature; read only the payload segment.
    let Some(payload_b64) = token.split('.').nth(1) else {
        return Ok(None); // not JWT-shaped → opaque token, keep legacy behavior
    };
    let Ok(bytes) = b64::url_decode(payload_b64) else {
        return Ok(None);
    };
    let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(None);
    };
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

    #[test]
    fn exp_is_read_from_the_token() {
        let key = test_key();
        let tok = jwt(serde_json::json!({ "exp": 1_800_000_000u64 }));
        assert_eq!(
            inspect_agent_token(&tok, &key).unwrap(),
            Some(1_800_000_000)
        );
    }

    #[test]
    fn matching_cnf_passes_and_absent_cnf_is_fine() {
        let key = test_key();
        // cnf.jwk == our public jwk → ok.
        let tok = jwt(serde_json::json!({ "exp": 42u64, "cnf": { "jwk": key.public_jwk() } }));
        assert_eq!(inspect_agent_token(&tok, &key).unwrap(), Some(42));
        // no cnf at all → still ok (nothing to check).
        let tok = jwt(serde_json::json!({ "exp": 42u64 }));
        assert_eq!(inspect_agent_token(&tok, &key).unwrap(), Some(42));
    }

    #[test]
    fn mismatched_cnf_is_a_hard_error() {
        let key = test_key();
        let other = AgentKey::from_seed(&[1u8; 32]).unwrap();
        let tok = jwt(serde_json::json!({ "exp": 42u64, "cnf": { "jwk": other.public_jwk() } }));
        let err = inspect_agent_token(&tok, &key).unwrap_err();
        assert!(err.contains("cnf.jwk"), "{err}");
    }

    #[test]
    fn opaque_or_unparseable_token_is_legacy_none() {
        let key = test_key();
        // not JWT-shaped (no dots) → None, no error.
        assert_eq!(inspect_agent_token("opaque-token", &key).unwrap(), None);
        // JWT-shaped but payload isn't valid base64/JSON → None, no error.
        assert_eq!(inspect_agent_token("a.!!!.c", &key).unwrap(), None);
        // JWT with no exp claim → None (caller falls back to expires_in).
        let tok = jwt(serde_json::json!({ "sub": "aauth:x@ap" }));
        assert_eq!(inspect_agent_token(&tok, &key).unwrap(), None);
    }
}
