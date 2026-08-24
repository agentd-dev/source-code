// SPDX-License-Identifier: AGPL-3.0-only
//! The intelligence endpoint *list* and per-endpoint credentials.
//!
//! `--intelligence` / `AGENTD_INTELLIGENCE` is an **ordered, comma-separated
//! list** (`https://gw-a.example,https://gw-b.example`), and list order IS
//! failover priority: `eps[0]` is the primary. Each element goes through the
//! HTTPS-only transport resolver — plaintext `http://` is admitted for loopback
//! only — and resolves its **own** credential by env name. With a single
//! element the failover and breaker machinery is inert, so a one-endpoint list
//! behaves exactly like a plain dial.
//!
//! Per-endpoint credential naming: the default `AGENTD_INTELLIGENCE_TOKEN`
//! belongs to endpoint 1, then `_2`, `_3`, … 1-indexed by list position. Each
//! has a `…_FILE` variant read through [`crate::sec::secret`], which is what
//! makes credential rotation possible without restarting. **The list URI itself
//! carries no key**: the resolved value is held as an opaque `String` in the
//! dialer only, never placed in a config or manifest struct, and never logged or
//! serialized — that is why no `Serialize` impl may ever be added to a type
//! holding it.

use std::time::Duration;

use super::client::{IntelError, Provider, Transport, resolve};
use super::health::{BreakerConfig, HealthRecord};

/// The default per-endpoint credential env var, belonging to endpoint 1. This is
/// the branded spelling agentd documents and emits.
const TOKEN_ENV: &str = "AGENTD_INTELLIGENCE_TOKEN";

/// The neutral, de-branded credential env var accepted as an input alias for
/// [`TOKEN_ENV`], so a host that standardises on vendor-neutral `AGENT_*` names
/// needs no agentd-specific variable. It is an input alias only: the resolved
/// value is still held opaquely and never logged or serialized.
const TOKEN_ENV_NEUTRAL: &str = "AGENT_INTELLIGENCE_TOKEN";

/// A single resolved endpoint: its transport + the per-request HTTP framing +
/// its resolved credential + live health/breaker state.
pub struct Endpoint {
    /// The dialer-ready transport (tcp+tls; plaintext only for loopback dev).
    pub(super) transport: Transport,
    pub(super) http_path: String,
    pub(super) host_header: String,
    /// The resolved bearer credential for THIS endpoint (never logged/serialized).
    pub(super) token: Option<String>,
    pub(super) provider: Provider,
    /// Structural transport scheme reported in the observable resource body:
    /// `https`, or `http` for the loopback dev carve-out. Never the URL, which
    /// can carry a credential in its userinfo or path.
    pub(super) scheme: &'static str,
    /// Structural address reported in the observable resource body — `host[:port]`
    /// only, with no scheme, no path and therefore no secret.
    pub(super) addr: String,
    /// Extra request headers: the resolved `intelligence.headers`, pushed on
    /// every dial. Empty unless configured.
    pub(super) extra_headers: Vec<(String, String)>,
    /// An optional per-request signer — AWS SigV4 over the exact body — added on
    /// every dial alongside the AAuth headers. Shared across every endpoint of
    /// one client, since the credential is a property of the caller and not of
    /// the endpoint it happens to reach.
    pub(super) signer: Option<std::sync::Arc<dyn ::mcp::http::RequestSigner>>,
    /// Live health and circuit-breaker state for this endpoint.
    pub health: HealthRecord,
}

/// The ordered endpoint list with the sticky-primary `active` cursor.
pub struct EndpointList {
    eps: Vec<Endpoint>,
    /// The index currently preferred. A plain `usize` is sound here because the
    /// dialer reaches it through a single-threaded `&mut` call path: the
    /// per-subagent `IntelClient` is never shared across threads.
    active: usize,
    breaker: BreakerConfig,
}

/// Resolve an env var (default impl; overridable in tests).
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

impl EndpointList {
    /// Parse the comma-list `uri` into an ordered `EndpointList`, resolving each
    /// endpoint's credential. `default_token` — the already-resolved
    /// `--intelligence-token` or its `_FILE` form — supplies endpoint 1's value
    /// when its env override is unset, and applies to endpoint 1 only.
    /// Per-endpoint env overrides (`AGENTD_INTELLIGENCE_TOKEN_<N>` / `_FILE`)
    /// win when present.
    pub fn parse(uri: &str, default_token: Option<String>) -> Result<EndpointList, IntelError> {
        Self::parse_with_env(uri, default_token, &env)
    }

    /// `parse` with an injectable env reader (for tests).
    pub fn parse_with_env(
        uri: &str,
        default_token: Option<String>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<EndpointList, IntelError> {
        let provider = Provider::OpenAiCompatible;
        let parts: Vec<&str> = uri
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(IntelError::Unsupported(
                "empty intelligence endpoint list".into(),
            ));
        }
        let mut eps = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            let (transport, http_path, host_header) = resolve(part, provider)?;
            let token = resolve_token(i, default_token.as_deref(), env)?;
            let (scheme, addr) = scheme_and_addr(part);
            eps.push(Endpoint {
                transport,
                http_path,
                host_header,
                token,
                provider,
                scheme,
                addr,
                extra_headers: Vec::new(),
                signer: None,
                health: HealthRecord::new(),
            });
        }
        Ok(EndpointList {
            eps,
            active: 0,
            breaker: BreakerConfig::default(),
        })
    }

    /// Apply the configured `intelligence.headers` to every endpoint. Called
    /// once after construction, before any dial.
    pub fn set_extra_headers(&mut self, headers: Vec<(String, String)>) {
        for e in &mut self.eps {
            e.extra_headers = headers.clone();
        }
    }

    /// Attach a per-request AWS SigV4 signer to every endpoint in the list.
    pub fn set_signer(&mut self, signer: std::sync::Arc<dyn ::mcp::http::RequestSigner>) {
        for e in &mut self.eps {
            e.signer = Some(signer.clone());
        }
    }

    /// Select the wire dialect for every endpoint from `intelligence.dialect`.
    /// A host-only endpoint resolved its path with the OpenAI default before the
    /// dialect was known, so a still-defaulted path is re-pointed at the chosen
    /// dialect's default. A path the operator wrote explicitly is left untouched,
    /// because they may be routing through a gateway mount. Bedrock overrides the
    /// path per request in any case.
    pub fn set_provider(&mut self, provider: Provider) {
        let default = provider.default_path();
        for e in &mut self.eps {
            e.provider = provider;
            if e.http_path == super::openai::DEFAULT_PATH {
                e.http_path = default.to_string();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.eps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.eps.is_empty()
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn breaker_config(&self) -> &BreakerConfig {
        &self.breaker
    }

    pub fn ep(&self, idx: usize) -> &Endpoint {
        &self.eps[idx]
    }

    pub fn iter(&self) -> impl Iterator<Item = &Endpoint> {
        self.eps.iter()
    }

    /// The failover attempt order: the **active** index first, then the
    /// remaining endpoints in ascending list order, skipping any whose breaker is
    /// OPEN and still cooling. `available` promotes an endpoint whose cooldown
    /// has elapsed to HALF-OPEN, so it is probed here rather than skipped
    /// forever. An empty result means every endpoint is down.
    pub fn attempt_order(&self) -> Vec<usize> {
        let mut order = Vec::with_capacity(self.eps.len());
        if self.eps[self.active].health.available(&self.breaker) {
            order.push(self.active);
        }
        for idx in 0..self.eps.len() {
            if idx == self.active {
                continue;
            }
            if self.eps[idx].health.available(&self.breaker) {
                order.push(idx);
            }
        }
        order
    }

    /// Snap `active` back to the lowest-index endpoint whose breaker is not OPEN.
    /// This is what makes the primary sticky: once it re-closes, the next call
    /// returns to it, so serving from a fallback is temporary by construction and
    /// a list never silently drifts onto its last entry. Returns the new active
    /// index if it changed.
    pub fn prefer_lowest_healthy(&mut self) -> Option<usize> {
        let target = (0..self.eps.len()).find(|&i| self.eps[i].health.is_up());
        if let Some(t) = target
            && t != self.active
        {
            self.active = t;
            return Some(t);
        }
        None
    }

    /// Mark `idx` as the active endpoint (it just succeeded). Returns the new
    /// active index if it changed.
    pub fn set_active(&mut self, idx: usize) -> Option<usize> {
        if idx != self.active {
            self.active = idx;
            Some(idx)
        } else {
            None
        }
    }

    /// True when no endpoint is available, i.e. every breaker is OPEN and still
    /// cooling.
    pub fn all_down(&self) -> bool {
        self.attempt_order().is_empty()
    }

    /// The active endpoint's bounded structural identity `(index,
    /// transport-scheme)` for the child→supervisor
    /// [`crate::subagent::protocol::AgentMsg::IntelHealth`] report. Transport and
    /// index ONLY — never the URL, host, cid or credential — matching the
    /// redaction the served resource body applies, so neither route can leak
    /// what the other withholds.
    pub fn active_identity(&self) -> (usize, &'static str) {
        (self.active, self.eps[self.active].scheme)
    }

    /// The `agentd://intelligence` resource body: the endpoint list by transport
    /// and index, which one is active, and each one's health — state, latency
    /// and error rate. It must contain no secret and no URL: only the bounded
    /// structural `transport` and `addr` (a bare `host[:port]`, which cannot
    /// carry a scheme-borne credential) plus the live health atomics. Anything
    /// added here becomes readable by every holder of the resource.
    pub fn body(&self, model: Option<&str>) -> serde_json::Value {
        use serde_json::json;
        let cfg = &self.breaker;
        let endpoints: Vec<serde_json::Value> = self
            .eps
            .iter()
            .enumerate()
            .map(|(i, ep)| {
                let h = &ep.health;
                let mut e = json!({
                    "index": i,
                    "transport": ep.scheme,
                    "addr": ep.addr,
                    "state": h.state().as_str(),
                    "active": i == self.active,
                    "ewma_latency_ms": h.ewma_latency_ms(),
                    "error_rate": h.error_rate(),
                    "consec_fail": h.consec_fail(),
                });
                if let serde_json::Value::Object(m) = &mut e {
                    if let Some(ms) = h.last_ok_ms_ago() {
                        m.insert("last_ok_ms_ago".into(), json!(ms));
                    }
                    if h.state() == super::health::BreakerState::Open {
                        if let Some(ms) = h.opened_ms_ago() {
                            m.insert("opened_ms_ago".into(), json!(ms));
                        }
                        m.insert(
                            "cooldown_ms".into(),
                            json!(h.cooldown(cfg).as_millis() as u64),
                        );
                        m.insert("last_err".into(), json!(h.last_err_kind().as_str()));
                    }
                }
                e
            })
            .collect();
        json!({
            "active": self.active,
            "all_down": self.all_down(),
            "model": model,
            "endpoints": endpoints,
        })
    }
}

/// Resolve endpoint `idx`'s credential. `idx` is 0-based but the env override is
/// 1-indexed: endpoint 0 reads `AGENTD_INTELLIGENCE_TOKEN` or falls back to the
/// already-resolved default, endpoint 1 reads `AGENTD_INTELLIGENCE_TOKEN_2`, and
/// so on. A `…_FILE` variant is read through the secret-file reader, which is
/// what allows the credential to be rotated on disk. An env override wins over
/// the default. Absence is not an error: an endpoint with no token dials
/// unauthenticated, which a public gateway legitimately allows.
fn resolve_token(
    idx: usize,
    default_token: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>, IntelError> {
    // Endpoint 1 (idx 0) uses the bare names; later endpoints are 1-indexed
    // (`_2`, `_3`, …). Each branded `AGENTD_*` name has a neutral `AGENT_*`
    // alias accepted on input; the branded spelling is always still honoured.
    let (inline_var, file_var, inline_var_n, file_var_n) = if idx == 0 {
        (
            TOKEN_ENV.to_string(),
            format!("{TOKEN_ENV}_FILE"),
            TOKEN_ENV_NEUTRAL.to_string(),
            format!("{TOKEN_ENV_NEUTRAL}_FILE"),
        )
    } else {
        let n = idx + 1;
        (
            format!("{TOKEN_ENV}_{n}"),
            format!("{TOKEN_ENV}_{n}_FILE"),
            format!("{TOKEN_ENV_NEUTRAL}_{n}"),
            format!("{TOKEN_ENV_NEUTRAL}_{n}_FILE"),
        )
    };
    // Precedence: explicit inline env override > file override > the resolved
    // default (only for endpoint 0). Higher-precedence inline wins. At each tier
    // the neutral `AGENT_*` spelling is read first, then the branded `AGENTD_*`.
    if let Some(v) = env(&inline_var_n).or_else(|| env(&inline_var)) {
        return Ok(Some(v));
    }
    if let Some(path) = env(&file_var_n).or_else(|| env(&file_var)) {
        let tok = crate::sec::secret::read_token_file(&path).map_err(IntelError::Unsupported)?;
        return Ok(Some(tok));
    }
    if idx == 0 {
        return Ok(default_token.map(str::to_string));
    }
    Ok(None)
}

/// The structural `(scheme, addr)` published in the observable resource body:
/// the bounded transport identity only, never the URL path or any secret.
/// `http` appears only for the loopback dev carve-out, because
/// [`resolve`](super::client) has already rejected every other non-HTTPS form
/// before an endpoint reaches here.
fn scheme_and_addr(uri: &str) -> (&'static str, String) {
    if let Some(rest) = uri.strip_prefix("https://") {
        ("https", host_only(rest))
    } else if let Some(rest) = uri.strip_prefix("http://") {
        ("http", host_only(rest))
    } else {
        ("unknown", String::new())
    }
}

/// The host[:port] of an `http(s)://host[:port]/path`, dropping the path (it may
/// be sensitive and is not addressing).
fn host_only(rest: &str) -> String {
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// A tool name in provider-safe wire form: OpenAI/Anthropic require tool names to
/// match `^[a-zA-Z0-9_-]+$`, so every other char — notably the `.` in agentd's
/// namespaced self-tools (`resource.read`, `subagent.spawn`) — becomes `_`. A
/// per-request reverse map restores the original name for routing.
fn wire_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Endpoint {
    /// When a process AAuth identity is installed, sign the intelligence dial
    /// with an RFC 9421 HTTP message signature so the gateway can attest the agent by
    /// signature rather than by source IP. The headers only add identity cover:
    /// a gateway that does not understand them ignores them, and the bearer
    /// token, if any, still rides alongside. Returns empty without
    /// `--features aauth` or with no identity configured, so an unsigned dial is
    /// the no-identity default rather than a failure.
    #[cfg(feature = "aauth")]
    fn aauth_headers(&self, method: &str, path: &str, body: &[u8]) -> Vec<(String, String)> {
        match crate::aauth::signer() {
            Some(signer) => signer.sign(method, &self.host_header, path, body),
            None => Vec::new(),
        }
    }
    #[cfg(not(feature = "aauth"))]
    fn aauth_headers(&self, _method: &str, _path: &str, _body: &[u8]) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Build the request body and headers for this endpoint's dialect, then dial
    /// and round-trip once. Returns the parsed response and the measured
    /// round-trip latency, which the caller feeds into this endpoint's health
    /// record. Endpoint selection happens above this call; nothing here consults
    /// the list or the breaker.
    pub(super) fn complete_once(
        &self,
        req: &crate::wire::intel::Request,
        timeout: Duration,
        trace_id: Option<&str>,
    ) -> Result<(crate::wire::intel::Response, Duration), IntelError> {
        use super::{anthropic, openai};
        use crate::net::http;
        use std::collections::HashMap;
        use std::time::Instant;

        // Provider tool-name compatibility: real OpenAI/Anthropic reject tool names
        // that aren't `^[a-zA-Z0-9_-]+$`, but agentd uses dotted namespaced names
        // (`resource.read`, `subagent.spawn`, …). Sanitize every place a name rides
        // the wire — the `tools` definitions AND the prior `tool_calls` in the
        // assistant message history (which get re-sent each turn) — and map the
        // returned `tool_calls` back to the originals so routing is unaffected.
        // No-op (no clone) when every name is already wire-safe.
        use crate::wire::intel::Message;
        let dirty = |n: &str| wire_tool_name(n) != n;
        let must_sanitize = req.tools.iter().any(|t| dirty(&t.name))
            || req.messages.iter().any(|m| {
                matches!(m, Message::Assistant { tool_calls, .. }
                    if tool_calls.iter().any(|tc| dirty(&tc.name)))
            });
        let mut wire_to_orig: HashMap<String, String> = HashMap::new();
        let owned_req;
        let req: &crate::wire::intel::Request = if must_sanitize {
            let mut r = req.clone();
            for t in &mut r.tools {
                let w = wire_tool_name(&t.name);
                if w != t.name {
                    wire_to_orig.insert(w.clone(), t.name.clone());
                    t.name = w;
                }
            }
            for m in &mut r.messages {
                if let Message::Assistant { tool_calls, .. } = m {
                    for tc in tool_calls {
                        let w = wire_tool_name(&tc.name);
                        if w != tc.name {
                            wire_to_orig.insert(w.clone(), tc.name.clone());
                            tc.name = w;
                        }
                    }
                }
            }
            owned_req = r;
            &owned_req
        } else {
            req
        };

        use super::bedrock;
        let (body, mut headers) = match self.provider {
            Provider::OpenAiCompatible => openai::build_request(req, self.token.as_deref()),
            Provider::Anthropic => anthropic::build_request(req, self.token.as_deref()),
            Provider::Bedrock => bedrock::build_request(req, self.token.as_deref()),
        };
        // The effective request path: fixed for OpenAI and Anthropic; Bedrock
        // derives `/model/{modelId}/converse` from the request. The SAME string
        // is sent on the wire AND fed to the AAuth/SigV4 signers below, so the
        // signature covers the exact request-target. Recomputing it separately
        // for either use would silently invalidate every signature.
        let path = self.provider.request_path(&self.http_path, req);
        if let Some(tid) = trace_id {
            headers.push((
                "traceparent".into(),
                crate::obs::trace::outbound_traceparent(tid),
            ));
        }
        // The configured `intelligence.headers` ride every dial — a gateway
        // routing header, for instance. Pushed BEFORE the AAuth headers so the
        // signature covers a header set that is already final; a header the
        // dialect set is not removed.
        for (k, v) in &self.extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        // AAuth: sign the dial over the exact body bytes (content-digest cover
        // applies when discovery flagged it) before we borrow `headers`.
        for (k, v) in self.aauth_headers("POST", &path, &body) {
            headers.push((k, v));
        }
        // An AWS SigV4 signature over the exact body, present when an `aws`
        // intelligence auth is configured — native Bedrock, or a Bedrock or
        // API-Gateway-fronted endpoint. Signed over `path`, so the dynamic
        // Bedrock target is inside the signature rather than appended after it.
        if let Some(signer) = &self.signer {
            for (k, v) in signer.sign("POST", &self.host_header, &path, &body) {
                headers.push((k, v));
            }
        }
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Same-endpoint transient retry. Every dial opens a fresh connection, so
        // a re-dial is safe. A 429 or 5xx is usually a momentary provider blip,
        // so it is retried a bounded number of times with short backoff BEFORE
        // the error escapes to cross-endpoint failover — or, in `once` mode
        // which arms no higher-level retry loop, straight to exit 4. A
        // non-transient 4xx (bad request, auth) is a caller error that would be
        // identical on a re-dial, so it surfaces immediately rather than burning
        // the run deadline. The body and headers, including the AAuth signature
        // over that body, are built once above and unchanged across attempts, so
        // every re-dial sends byte-identical bytes and the signature stays valid.
        const TRANSIENT_RETRIES: u32 = 2;
        let mut attempt: u32 = 0;
        let (resp, latency) = loop {
            let start = Instant::now();
            let mut stream = self.transport.connect(timeout)?;
            let resp = http::send(
                stream.as_mut(),
                &self.host_header,
                "POST",
                &path,
                &header_refs,
                &body,
            )?;
            let latency = start.elapsed();
            if resp.is_success() {
                break (resp, latency);
            }
            if super::failover::is_transient_status(resp.status) && attempt < TRANSIENT_RETRIES {
                attempt += 1;
                // Short exponential backoff (250ms, 500ms) — enough to ride out a
                // blip, bounded so total added wait stays well under a second.
                std::thread::sleep(Duration::from_millis(250 * (1u64 << (attempt - 1))));
                continue;
            }
            let snippet: String = resp.body_str().chars().take(512).collect();
            return Err(IntelError::Http(resp.status, snippet));
        };

        let mut parsed = match self.provider {
            Provider::OpenAiCompatible => openai::parse_response(&resp.body),
            Provider::Anthropic => anthropic::parse_response(&resp.body),
            Provider::Bedrock => bedrock::parse_response(&resp.body),
        }
        .map_err(IntelError::Parse)?;
        // Undo the wire sanitization: route by the original (dotted) tool names.
        if !wire_to_orig.is_empty() {
            for tc in &mut parsed.tool_calls {
                if let Some(orig) = wire_to_orig.get(&tc.name) {
                    tc.name = orig.clone();
                }
            }
        }
        Ok((parsed, latency))
    }

    /// Model-discovery probe: one hand-rolled HTTP **GET** to the `/v1/models`
    /// sibling of this endpoint's chat path, over the SAME transport and the
    /// SAME bearer auth the chat call uses — no second client, no streaming.
    /// Returns the discovered model `id`s.
    ///
    /// **Best-effort with silent degrade.** The `anthropic` dialect has no list
    /// endpoint and returns `vec![]`. For an OpenAI-compatible endpoint, a
    /// connection or transport failure, a non-2xx status (a 404 simply means
    /// discovery is unsupported), or a body that is not the expected JSON all
    /// yield `vec![]` too. None of these is a failover-class error and none is
    /// fatal: the endpoint stays fully usable without discovery, since the
    /// configured model is dialed regardless. Recording a probe failure against
    /// the endpoint's health would let an optional feature open a breaker on a
    /// perfectly healthy provider, which is why nothing here touches health. The
    /// caller bounds this with a short timeout.
    pub(super) fn discover_models(&self, timeout: Duration) -> Vec<String> {
        use super::openai;
        use crate::net::http;

        // The dialect is already settled by the configured provider, so there is
        // nothing to sniff here. Anthropic has no list endpoint.
        if self.provider != Provider::OpenAiCompatible {
            return Vec::new();
        }

        let path = openai::models_path(&self.http_path);
        // Same auth header the chat call sends (`Authorization: Bearer …`), no body.
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(tok) = self.token.as_deref() {
            headers.push(("authorization".into(), format!("Bearer {tok}")));
        }
        // Sign the discovery GET too (over its own `/v1/models` path), so a
        // signature-attesting gateway accepts it exactly like the chat dial.
        for (k, v) in self.aauth_headers("GET", &path, &[]) {
            headers.push((k, v));
        }
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Connect → GET → parse. Any error degrades to [] (silent, never fatal).
        let Ok(mut stream) = self.transport.connect(timeout) else {
            return Vec::new();
        };
        let Ok(resp) = http::send(
            stream.as_mut(),
            &self.host_header,
            "GET",
            &path,
            &header_refs,
            &[],
        ) else {
            return Vec::new();
        };
        if !resp.is_success() {
            // 404 / 4xx / 5xx → discovery unsupported for this endpoint.
            return Vec::new();
        }
        openai::parse_models(&resp.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_headers_apply_to_every_endpoint() {
        // `intelligence.headers` (and a device-login bearer) reach the wire via
        // `set_extra_headers`, applied to every failover endpoint, not just the
        // primary.
        let mut list = EndpointList::parse(
            "https://a.example/v1,https://b.example/v1",
            Some("tok".into()),
        )
        .unwrap();
        assert!(list.eps.iter().all(|e| e.extra_headers.is_empty()));
        list.set_extra_headers(vec![("X-Team".into(), "ops".into())]);
        for e in &list.eps {
            assert_eq!(
                e.extra_headers,
                vec![("X-Team".to_string(), "ops".to_string())]
            );
        }
    }

    #[test]
    fn wire_tool_name_maps_only_illegal_chars() {
        // The provider pattern is ^[a-zA-Z0-9_-]+$: dots (agentd's namespace
        // separator) and anything else become `_`; legal names pass through.
        assert_eq!(wire_tool_name("resource.read"), "resource_read");
        assert_eq!(wire_tool_name("subagent.spawn"), "subagent_spawn");
        assert_eq!(wire_tool_name("math.factorial"), "math_factorial");
        // Already-legal names are untouched (so the fast path stays a no-op).
        assert_eq!(wire_tool_name("get_weather"), "get_weather");
        assert_eq!(wire_tool_name("list-files"), "list-files");
        assert_eq!(
            wire_tool_name("calculate_triangle_area"),
            "calculate_triangle_area"
        );
        // Other illegal chars (spaces, slashes) also normalize.
        assert_eq!(wire_tool_name("a b/c"), "a_b_c");
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn comma_list_parses_to_n_endpoints_in_order() {
        let env = env_of(&[]);
        let list = EndpointList::parse_with_env(
            "https://gw-a.example:8443,https://gw-b.example:8444,https://intel.example",
            None,
            &env,
        )
        .unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list.ep(0).scheme, "https");
        assert_eq!(list.ep(0).addr, "gw-a.example:8443");
        assert_eq!(list.ep(1).addr, "gw-b.example:8444");
        assert_eq!(list.ep(2).scheme, "https");
        assert_eq!(list.active(), 0);
    }

    #[test]
    fn whitespace_around_elements_is_trimmed() {
        let env = env_of(&[]);
        let list =
            EndpointList::parse_with_env(" https://a.example , https://b.example ", None, &env)
                .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list.ep(0).addr, "a.example");
        assert_eq!(list.ep(1).addr, "b.example");
    }

    #[test]
    fn empty_list_is_an_error() {
        let env = env_of(&[]);
        assert!(EndpointList::parse_with_env("", None, &env).is_err());
        assert!(EndpointList::parse_with_env("   ,  ,", None, &env).is_err());
    }

    #[test]
    fn bad_element_scheme_is_an_error() {
        let env = env_of(&[]);
        let r = EndpointList::parse_with_env("https://a.example,ftp://nope", None, &env);
        assert!(matches!(r, Err(IntelError::Unsupported(_))));
        // Non-HTTPS transports are rejected at the same chokepoint.
        for uri in ["unix:/a", "vsock:3:8080", "http://not-loopback.example"] {
            let r = EndpointList::parse_with_env(uri, None, &env);
            assert!(matches!(r, Err(IntelError::Unsupported(_))), "{uri}");
        }
    }

    #[test]
    fn per_endpoint_token_env_resolves_by_position() {
        // endpoint 1 uses the bare name (or the default); endpoint 2 uses `_2`.
        let env = env_of(&[
            ("AGENTD_INTELLIGENCE_TOKEN", "tok-a"),
            ("AGENTD_INTELLIGENCE_TOKEN_2", "tok-b"),
        ]);
        let list = EndpointList::parse_with_env("https://a.example,https://b.example", None, &env)
            .unwrap();
        assert_eq!(list.ep(0).token.as_deref(), Some("tok-a"));
        assert_eq!(list.ep(1).token.as_deref(), Some("tok-b"));
    }

    #[test]
    fn endpoint_0_falls_back_to_default_token_when_env_unset() {
        let env = env_of(&[]);
        let list = EndpointList::parse_with_env(
            "https://a.example,https://b.example",
            Some("default".into()),
            &env,
        )
        .unwrap();
        // endpoint 0 inherits the resolved default; endpoint 1 has none.
        assert_eq!(list.ep(0).token.as_deref(), Some("default"));
        assert_eq!(list.ep(1).token, None);
    }

    #[test]
    fn per_endpoint_env_override_wins_over_default() {
        let env = env_of(&[("AGENTD_INTELLIGENCE_TOKEN", "from-env")]);
        let list = EndpointList::parse_with_env("https://a.example", Some("default".into()), &env)
            .unwrap();
        assert_eq!(list.ep(0).token.as_deref(), Some("from-env"));
    }

    #[test]
    fn neutral_token_env_is_accepted_as_an_alias() {
        // The neutral `AGENT_INTELLIGENCE_TOKEN[_N]` spelling is accepted on
        // input (endpoint 1 bare; later endpoints 1-indexed).
        let env = env_of(&[
            ("AGENT_INTELLIGENCE_TOKEN", "neutral-a"),
            ("AGENT_INTELLIGENCE_TOKEN_2", "neutral-b"),
        ]);
        let list = EndpointList::parse_with_env("https://a.example,https://b.example", None, &env)
            .unwrap();
        assert_eq!(list.ep(0).token.as_deref(), Some("neutral-a"));
        assert_eq!(list.ep(1).token.as_deref(), Some("neutral-b"));
    }

    #[test]
    fn branded_token_env_wins_over_neutral_on_conflict() {
        // Both spellings set: the neutral name is read first. The branded form
        // is still honoured whenever the neutral one is absent.
        let env = env_of(&[
            ("AGENT_INTELLIGENCE_TOKEN", "neutral"),
            ("AGENTD_INTELLIGENCE_TOKEN", "branded"),
        ]);
        let list = EndpointList::parse_with_env("https://a.example", None, &env).unwrap();
        assert_eq!(list.ep(0).token.as_deref(), Some("neutral"));

        // The branded name alone resolves when no neutral one is set.
        let env = env_of(&[("AGENTD_INTELLIGENCE_TOKEN", "branded")]);
        let list = EndpointList::parse_with_env("https://a.example", None, &env).unwrap();
        assert_eq!(list.ep(0).token.as_deref(), Some("branded"));
    }

    #[test]
    fn token_file_variant_reads_from_disk() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "file-secret").unwrap();
        let path = f.path().to_str().unwrap().to_string();
        let pairs = [("AGENTD_INTELLIGENCE_TOKEN_2_FILE", path.as_str())];
        let env = env_of(&pairs);
        let list = EndpointList::parse_with_env("https://a.example,https://b.example", None, &env)
            .unwrap();
        assert_eq!(list.ep(1).token.as_deref(), Some("file-secret"));
    }

    #[test]
    fn single_element_list_has_inert_failover() {
        let env = env_of(&[]);
        let list = EndpointList::parse_with_env("https://intel.example", None, &env).unwrap();
        assert_eq!(list.len(), 1);
        // The failover machinery is inert: attempt order is just [0].
        assert_eq!(list.attempt_order(), vec![0]);
        assert!(!list.all_down());
    }

    #[test]
    fn attempt_order_skips_open_endpoint_and_snaps_back() {
        use super::super::health::ErrKind;
        let env = env_of(&[]);
        let mut list =
            EndpointList::parse_with_env("https://a.example,https://b.example", None, &env)
                .unwrap();
        let cfg = *list.breaker_config();
        // open endpoint 0's breaker (threshold 3)
        for _ in 0..3 {
            list.ep(0).health.record_failure(ErrKind::Refused, &cfg);
        }
        // attempt order now skips 0, yields [1]
        assert_eq!(list.attempt_order(), vec![1]);
        // and 1 is the lowest healthy → prefer_lowest_healthy moves active there
        assert_eq!(list.prefer_lowest_healthy(), Some(1));
        assert_eq!(list.active(), 1);
        // endpoint 0 recovers → snap back to it
        list.ep(0).health.record_success(Duration::from_millis(5));
        assert_eq!(list.prefer_lowest_healthy(), Some(0));
        assert_eq!(list.active(), 0);
    }

    #[test]
    fn resource_body_has_health_and_no_url_or_token() {
        use super::super::health::ErrKind;
        let env = env_of(&[("AGENTD_INTELLIGENCE_TOKEN", "super-secret-tok")]);
        let list = EndpointList::parse_with_env(
            "https://gw-a.example:8443,https://gw-b.example/v1/secret-path",
            None,
            &env,
        )
        .unwrap();
        // make endpoint 1 broken, endpoint 0 healthy + active
        list.ep(0).health.record_success(Duration::from_millis(41));
        let cfg = *list.breaker_config();
        for _ in 0..3 {
            list.ep(1).health.record_failure(ErrKind::Refused, &cfg);
        }
        let body = list.body(Some("claude-opus-4"));
        let text = body.to_string();
        // schema: active/all_down/model/endpoints[]
        assert_eq!(body["active"], 0);
        assert_eq!(body["model"], "claude-opus-4");
        assert_eq!(body["endpoints"][0]["transport"], "https");
        assert_eq!(body["endpoints"][0]["addr"], "gw-a.example:8443");
        assert_eq!(body["endpoints"][0]["state"], "closed");
        assert_eq!(body["endpoints"][0]["active"], true);
        assert_eq!(body["endpoints"][0]["ewma_latency_ms"], 41);
        assert_eq!(body["endpoints"][1]["state"], "open");
        assert_eq!(body["endpoints"][1]["last_err"], "refused");
        // The body must carry neither the token nor a full URL (no scheme
        // prefix, no path).
        assert!(!text.contains("super-secret-tok"), "token leaked: {text}");
        assert!(!text.contains("https://"), "full URI leaked: {text}");
        assert!(!text.contains("secret-path"), "URL path leaked: {text}");
    }

    #[test]
    fn all_down_when_every_breaker_open() {
        use super::super::health::ErrKind;
        let env = env_of(&[]);
        let list = EndpointList::parse_with_env("https://a.example,https://b.example", None, &env)
            .unwrap();
        let cfg = *list.breaker_config();
        for ep in list.iter() {
            for _ in 0..3 {
                ep.health.record_failure(ErrKind::Refused, &cfg);
            }
        }
        assert!(list.all_down());
        assert!(list.attempt_order().is_empty());
    }
}
