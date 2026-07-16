// SPDX-License-Identifier: Apache-2.0
//! The intelligence endpoint *list* and per-endpoint credentials. RFC 0018 §3.1/§3.2.
//!
//! `--intelligence` / `AGENTD_INTELLIGENCE` is an **ordered, comma-separated
//! list** (`https://gw-a.example,https://gw-b.example`); list order IS failover
//! priority (`eps[0]` is the primary). Each element is parsed by the HTTPS-only
//! transport resolver (target-vision pivot; plaintext `http://` is loopback-only
//! for dev/tests) and resolves its **own** credential by env name (§3.2). A
//! single-element list is exactly RFC 0006 — the failover/breaker machinery is
//! inert with one endpoint.
//!
//! Per-endpoint credential naming (RFC 0014 §6.4 / §3.2): the default
//! `AGENTD_INTELLIGENCE_TOKEN` (≡ endpoint 1), then `_2`, `_3`, … (1-indexed by
//! list position). Each has a `…_FILE` variant read through [`crate::sec::secret`]
//! (the secret-file reader landed in 0017-A). **The list URI carries no key**;
//! the resolved value is never logged/serialized (the `Secret`-no-`Serialize`
//! property holds — we hold it as an opaque `String` only in the dialer, never
//! in a config/manifest).

use std::time::Duration;

use super::client::{IntelError, Provider, Transport, resolve};
use super::health::{BreakerConfig, HealthRecord};

/// The default per-endpoint credential env var (≡ endpoint 1, RFC 0018 §3.2) —
/// the branded spelling agentd documents/emits.
const TOKEN_ENV: &str = "AGENTD_INTELLIGENCE_TOKEN";

/// The neutral (de-branded) credential env var (ACC SPEC L4 / env-convention.json)
/// accepted as an input alias for [`TOKEN_ENV`]. Credentials path only — the
/// resolved value is still held opaquely and never logged/serialized (L5).
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
    /// Structural transport scheme for the §4.4 resource body (`https`, or
    /// `http` for the loopback dev carve-out) — never the URL (RFC 0012 §3.7).
    pub(super) scheme: &'static str,
    /// Structural address for the §4.4 resource body (`host[:port]`) — the host
    /// only, no secret, no scheme, no path.
    pub(super) addr: String,
    /// Live health + circuit breaker (RFC 0018 §4).
    pub health: HealthRecord,
}

/// The ordered endpoint list with the sticky-primary `active` cursor (§3.3).
pub struct EndpointList {
    eps: Vec<Endpoint>,
    /// The index currently preferred (sticky-primary, §3.3). Plain `usize`
    /// behind the dialer's `&mut`/single-thread call path — the per-subagent
    /// `IntelClient` is not shared across threads.
    active: usize,
    breaker: BreakerConfig,
}

/// Resolve an env var (default impl; overridable in tests).
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

impl EndpointList {
    /// Parse the comma-list `uri` into an ordered `EndpointList`, resolving each
    /// endpoint's credential. The single `default_token` is endpoint 1's value
    /// when the env override is unset (it is the already-resolved
    /// `--intelligence-token`/`_FILE`, RFC 0006). Per-endpoint env overrides
    /// (`AGENTD_INTELLIGENCE_TOKEN_<N>` / `_FILE`) win when present.
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
                health: HealthRecord::new(),
            });
        }
        Ok(EndpointList {
            eps,
            active: 0,
            breaker: BreakerConfig::default(),
        })
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

    /// The failover attempt order (§3.3): the **active** index first, then the
    /// remaining endpoints in ascending list order, skipping any whose breaker is
    /// OPEN-and-cooling (`available` promotes an elapsed-cooldown endpoint to
    /// HALF-OPEN so it is probed). An empty result == all endpoints down (§6).
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

    /// Snap `active` back to the lowest-index endpoint whose breaker is not OPEN
    /// (sticky-primary, §3.3) — so once the primary re-closes, the next call
    /// returns to it and a fallback is temporary by construction. Returns the new
    /// active index if it changed.
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

    /// True when no endpoint is available — every breaker OPEN-and-cooling (§6).
    pub fn all_down(&self) -> bool {
        self.attempt_order().is_empty()
    }

    /// The active endpoint's bounded structural identity `(index, transport-scheme)`
    /// for the child→supervisor [`crate::subagent::protocol::AgentMsg::IntelHealth`]
    /// report — transport + index ONLY, NEVER the URL/cid/host or credential (RFC
    /// 0012 §3.7, mirroring the §4.4 resource-body redaction).
    pub fn active_identity(&self) -> (usize, &'static str) {
        (self.active, self.eps[self.active].scheme)
    }

    /// The `agentd://intelligence` resource body (RFC 0018 §4.4): the endpoint
    /// list (transport + index, NEVER the URL/creds), which is active, and each
    /// one's health (state/latency/error-rate). No secret, no URL (RFC 0012
    /// §3.7) — only the bounded structural `transport`+`addr` (cid:port / host,
    /// no scheme-borne secret) and the live atomics.
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

/// Resolve endpoint `idx` (0-based)'s credential. The per-endpoint env override
/// is 1-indexed: endpoint 0 → `AGENTD_INTELLIGENCE_TOKEN` (or the
/// already-resolved default), endpoint 1 → `AGENTD_INTELLIGENCE_TOKEN_2`, etc.
/// A `…_FILE` variant is read through the secret-file reader (rotation-friendly,
/// 0017-A). The override wins over the default; absent ⇒ no token for that
/// endpoint (a public/unauthenticated gateway is legal).
fn resolve_token(
    idx: usize,
    default_token: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>, IntelError> {
    // Endpoint 1 (idx 0) uses the bare names; later endpoints are 1-indexed
    // (`_2`, `_3`, …). Each branded `AGENTD_*` name has a neutral `AGENT_*` alias
    // (ACC SPEC L4) accepted on input — branded never dropped.
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

/// The structural `(scheme, addr)` for the §4.4 resource body — the bounded
/// transport identity only, never the URL path or any secret (RFC 0012 §3.7).
/// HTTPS-only (pivot Phase 1): `http` appears only for the loopback dev
/// carve-out; [`resolve`](super::client) already rejected everything else.
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
    /// AAuth (RFC 0023; agentctl RFC 0024 §7.1 — the modelgateway inbound
    /// posture): when a process AAuth identity is installed, **sign the
    /// intelligence dial** (RFC 9421) so the gateway can attest the agent by
    /// signature instead of source IP. Additive and identity-cover only — a
    /// non-AAuth endpoint ignores the headers, and the bearer (if any) still
    /// rides alongside. Empty without `--features aauth` or with no identity
    /// configured, so the default path is byte-identical to before.
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

    /// Build the request body + headers for this endpoint's dialect, then dial +
    /// round-trip exactly as RFC 0006 (`complete_once`). Returns the parsed
    /// response and the round-trip latency. The wire/adapter/JSON path is
    /// UNCHANGED (§3.4) — only endpoint *selection* wraps it.
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

        let (body, mut headers) = match self.provider {
            Provider::OpenAiCompatible => openai::build_request(req, self.token.as_deref()),
            Provider::Anthropic => anthropic::build_request(req, self.token.as_deref()),
        };
        if let Some(tid) = trace_id {
            headers.push((
                "traceparent".into(),
                crate::obs::trace::outbound_traceparent(tid),
            ));
        }
        // AAuth: sign the dial over the exact body bytes (content-digest cover
        // applies when discovery flagged it) before we borrow `headers`.
        for (k, v) in self.aauth_headers("POST", &self.http_path, &body) {
            headers.push((k, v));
        }
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let start = Instant::now();
        let mut stream = self.transport.connect(timeout)?;
        let resp = http::send(
            stream.as_mut(),
            &self.host_header,
            "POST",
            &self.http_path,
            &header_refs,
            &body,
        )?;
        let latency = start.elapsed();

        if !resp.is_success() {
            let snippet: String = resp.body_str().chars().take(512).collect();
            return Err(IntelError::Http(resp.status, snippet));
        }

        let mut parsed = match self.provider {
            Provider::OpenAiCompatible => openai::parse_response(&resp.body),
            Provider::Anthropic => anthropic::parse_response(&resp.body),
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

    /// RFC 0018 §5.4 model-discovery probe: one hand-rolled HTTP **GET** to the
    /// `/v1/models` sibling of this endpoint's chat path, over the SAME transport
    /// (tcp+tls) + the SAME bearer auth the chat call uses — no new client, no
    /// streaming. Returns the discovered model `id`s.
    ///
    /// **Best-effort, silent-degrade (§5.4):** the `anthropic` dialect has no list
    /// endpoint → `vec![]`; for OpenAI-compatible, a connection/transport failure,
    /// a non-2xx (e.g. 404 — discovery unsupported), or a non-JSON/unexpected body
    /// all yield `vec![]`. NEVER a failover-class error, NEVER fatal — the endpoint
    /// is fully usable with discovery unsupported (the configured model is dialed
    /// regardless). The caller bounds it with a SHORT timeout (off the hot path).
    pub(super) fn discover_models(&self, timeout: Duration) -> Vec<String> {
        use super::openai;
        use crate::net::http;

        // Dialect detection is already known from the provider (§5.4 — reuse, don't
        // re-detect). Anthropic has no list endpoint.
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
        // The retired transports are rejected at the same chokepoint.
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
        // ACC SPEC L4: the neutral `AGENT_INTELLIGENCE_TOKEN[_N]` spelling is
        // accepted on input (endpoint 1 bare; later endpoints 1-indexed).
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
        // Both spellings set ⇒ neutral-first is read; the branded form is still
        // accepted when the neutral one is absent (alias, never dropped).
        let env = env_of(&[
            ("AGENT_INTELLIGENCE_TOKEN", "neutral"),
            ("AGENTD_INTELLIGENCE_TOKEN", "branded"),
        ]);
        let list = EndpointList::parse_with_env("https://a.example", None, &env).unwrap();
        assert_eq!(list.ep(0).token.as_deref(), Some("neutral"));

        // Branded-only still resolves (back-compat).
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
    fn single_element_list_is_rfc_0006() {
        let env = env_of(&[]);
        let list = EndpointList::parse_with_env("https://intel.example", None, &env).unwrap();
        assert_eq!(list.len(), 1);
        // the failover machinery is inert: attempt order is just [0].
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
        // RFC 0012 §3.7: NEVER the token, NEVER a full URL (scheme prefix or path)
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
