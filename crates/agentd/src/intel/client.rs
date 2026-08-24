// SPDX-License-Identifier: AGPL-3.0-only
//! Intelligence client — endpoint *list* selection plus one round-trip.
//!
//! The transport is **HTTPS**: each `AGENTD_INTELLIGENCE` list element is an
//! `https://` URL, and plaintext `http://` is admitted only for a loopback host
//! as a dev carve-out. The wire is HTTP/1.1, with the gateway or provider
//! speaking an OpenAI-compatible `/chat/completions`. Each request opens its own
//! connection and sends `Connection: close`: model calls are seconds apart and
//! minutes long, so a connection pool would buy nothing and cost a whole class
//! of stale-socket failures.
//!
//! `--intelligence` is an ordered list — a primary plus fallbacks — and
//! `complete()` drives it through the sticky-primary failover policy
//! ([`super::failover`]) with a per-endpoint health record and circuit breaker
//! ([`super::health`]). Selection is the only layer the list adds: the
//! wire/adapter/JSON path underneath is identical either way, and with a single
//! endpoint the failover machinery is inert, so one endpoint costs nothing.

use crate::net::http::{Stream, Url};
use crate::wire::intel::{Request, Response};
use std::cell::RefCell;
use std::fmt;
use std::time::Duration;

use super::endpoints::EndpointList;
use super::{anthropic, bedrock, failover, openai};

/// Which in-binary adapter speaks to the endpoint. OpenAI-compatible is the
/// default; Anthropic Messages and Bedrock Converse are the other two dialects
/// compiled in. The set stops here deliberately: any other provider is reached
/// through a gateway, which keeps provider quirks out of the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAiCompatible,
    Anthropic,
    /// Amazon Bedrock Converse — the model id rides the URL path, not the body,
    /// and auth is SigV4 (an `intelligence.auth: {kind: aws}`), not a bearer.
    Bedrock,
}

impl Provider {
    pub(super) fn default_path(self) -> &'static str {
        match self {
            Provider::OpenAiCompatible => openai::DEFAULT_PATH,
            Provider::Anthropic => anthropic::DEFAULT_PATH,
            Provider::Bedrock => bedrock::DEFAULT_PATH,
        }
    }

    /// Map the config `intelligence.dialect` selector to a provider. `None`/empty
    /// ⇒ the OpenAI-compatible default; an unknown value ⇒ `None` (the caller
    /// keeps the default — validation rejects it earlier at the config layer).
    pub fn from_dialect(dialect: Option<&str>) -> Option<Provider> {
        match dialect.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("openai") | Some("openai-compatible") => Some(Provider::OpenAiCompatible),
            Some("anthropic") => Some(Provider::Anthropic),
            Some("bedrock") => Some(Provider::Bedrock),
            Some(_) => None,
        }
    }

    /// The effective request path for THIS request. Fixed for OpenAI and
    /// Anthropic (the configured or default path); Bedrock puts the URI-encoded
    /// model id in the path — `/model/{modelId}/converse` — so the path must be
    /// computed per request and the signer and the wire must be handed the same
    /// dynamic target, or the signature will not cover what is sent.
    pub(super) fn request_path(self, configured: &str, req: &Request) -> String {
        match self {
            Provider::Bedrock => bedrock::converse_path(&req.model),
            _ => configured.to_string(),
        }
    }
}

#[derive(Debug)]
pub enum IntelError {
    /// Transport / connection failure. Classed as fatal infrastructure, so a
    /// one-shot run exits 4 rather than reporting a model-level failure.
    Transport(std::io::Error),
    /// Non-2xx HTTP status from the endpoint.
    Http(u16, String),
    /// Malformed response body.
    Parse(String),
    /// A transport this build doesn't support (e.g. https without `tls`).
    Unsupported(String),
    /// Every endpoint in the list is down or broken after the bounded failover
    /// sweep. The boxed cause is the last failover-class error seen. Maps to the
    /// same fatal-infrastructure class as `Transport`, so a `once` run exits 4;
    /// a loop or reactive daemon backs off and retries rather than crashing.
    AllEndpointsDown(Option<Box<IntelError>>),
}

impl fmt::Display for IntelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntelError::Transport(e) => write!(f, "intelligence transport error: {e}"),
            IntelError::Http(code, body) => write!(f, "intelligence HTTP {code}: {body}"),
            IntelError::Parse(m) => write!(f, "{m}"),
            IntelError::Unsupported(m) => write!(f, "{m}"),
            IntelError::AllEndpointsDown(cause) => match cause {
                Some(e) => write!(f, "all intelligence endpoints down (last error: {e})"),
                None => write!(f, "all intelligence endpoints down"),
            },
        }
    }
}
impl std::error::Error for IntelError {}

impl From<std::io::Error> for IntelError {
    fn from(e: std::io::Error) -> Self {
        IntelError::Transport(e)
    }
}

/// A resolved intelligence client over an ordered endpoint list.
pub struct IntelClient {
    /// The endpoint list, the sticky-primary cursor, and the per-endpoint
    /// health/breaker state. Behind a `RefCell` so `complete(&self)` can advance
    /// the cursor and record health without forcing a `&mut` through every call
    /// site in the loop. This is interior mutability, not sharing: the
    /// per-subagent client is single-threaded. A hot reload replaces the whole
    /// `IntelClient` rather than mutating this list in place.
    list: RefCell<EndpointList>,
    timeout: Duration,
    /// The run's trace id; when set, every completion carries a `traceparent`
    /// header so the model call joins the run's distributed trace.
    trace_id: Option<String>,
    /// All-endpoints-down backoff policy. `None` — the default, used by
    /// `once`-mode — means a single sweep: all-down returns immediately and the
    /// caller maps it to exit 4. `Some(policy)`, used by loop and reactive
    /// daemons, re-runs the sweep with bounded jittered backoff so a transient
    /// host-model roll recovers without the daemon dying; it resumes the instant
    /// any endpoint half-opens healthy.
    alldown: Option<AllDownPolicy>,
    /// Edge-triggered all-down reachability reporter. The model loop runs in a
    /// CHILD process that owns this breaker state, and the supervisor has no
    /// model of its own and no live view of it — this callback is the only way
    /// the reachability reaches the supervisor. When set, `complete()` invokes
    /// it exactly ONCE per transition of the list's all-down state: on
    /// **entering** all-down (every breaker open, or the sweep exhausted) and on
    /// **recovering** (any endpoint usable again). The child turns that into an
    /// `AgentMsg::IntelHealth` sent upward. Firing on the edge rather than per
    /// call means the steady state costs one bool compare. The report carries
    /// transport and index ONLY — never a URL or credential. Nothing here
    /// touches the data path; it is a pure upward report. Defaults to `None`
    /// for a one-shot run with no supervisor listening.
    health_reporter: Option<Box<dyn Fn(IntelHealthReport)>>,
    /// Last all-down state observed by the reporter, so we only fire on a change.
    last_all_down: std::cell::Cell<bool>,
}

/// The edge-triggered intelligence-reachability report a child emits upward.
/// `active` is the bounded `(index, transport-scheme)` of the serving endpoint:
/// transport and index ONLY, never a URL, host, cid or credential, because this
/// crosses a process boundary into the supervisor's observable surface. It is
/// `None` on entering all-down, when nothing is serving.
pub struct IntelHealthReport {
    pub all_down: bool,
    pub active: Option<(usize, &'static str)>,
}

/// Bounded, jittered all-down backoff. A daemon re-arms the sweep up to
/// `max_retries` times, sleeping `base × 2^n` capped at `max` with per-attempt
/// jitter, before surfacing the terminal all-down. The bound matters: without
/// `max_retries` a permanently misconfigured endpoint would keep a daemon alive
/// and silent forever instead of failing visibly.
#[derive(Debug, Clone, Copy)]
pub struct AllDownPolicy {
    pub max_retries: u32,
    pub base: Duration,
    pub max: Duration,
}

impl Default for AllDownPolicy {
    fn default() -> AllDownPolicy {
        // 1s..30s jittered: fast enough to ride out a rolling model deploy,
        // slow enough that eight attempts do not hammer a dead provider.
        AllDownPolicy {
            max_retries: 8,
            base: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

/// Per-endpoint dial transport, owned by [`super::endpoints`]. Intelligence is
/// **HTTPS-only**, so there is exactly one TCP shape: `tls: true` in production.
/// `tls: false` exists solely for the loopback dev/test carve-out that backs the
/// built-in mock LLM, and [`resolve`] rejects it for any non-loopback host —
/// the variant cannot be reached with a real remote address.
#[derive(Debug)]
pub enum Transport {
    Tcp { host: String, port: u16, tls: bool },
}

impl IntelClient {
    /// Build from explicit parts — the subagent path, driven by the spawn
    /// payload rather than the CLI `Config`. `uri` is the endpoint *list*, which
    /// may hold a single element. `default_token` is endpoint 1's resolved
    /// credential, used only when its own env override is unset; every later
    /// endpoint resolves its own `_<N>`-suffixed token instead of inheriting
    /// this one, so a fallback never dials with the primary's credential.
    pub fn from_parts(uri: &str, default_token: Option<String>) -> Result<IntelClient, IntelError> {
        let list = EndpointList::parse(uri, default_token)?;
        Ok(IntelClient {
            list: RefCell::new(list),
            // Generous per-call ceiling; the run deadline is the real bound.
            timeout: Duration::from_secs(120),
            trace_id: None,
            alldown: None,
            health_reporter: None,
            last_all_down: std::cell::Cell::new(false),
        })
    }

    /// Attach the configured `intelligence.headers`, applied to every endpoint
    /// dial. Builder-style; call before use. An empty list leaves every endpoint
    /// with only the dialect's own headers.
    pub fn with_headers(self, headers: Vec<(String, String)>) -> IntelClient {
        if !headers.is_empty() {
            self.list.borrow_mut().set_extra_headers(headers);
        }
        self
    }

    /// Attach a per-request signer — AWS SigV4 — applied to every dial. The
    /// signature covers the method, host, request-target and body of the dial
    /// that is about to go out, so it is computed after the effective path is
    /// resolved rather than from the configured path. Builder-style; call before
    /// use.
    pub fn with_signer(
        self,
        signer: Option<std::sync::Arc<dyn ::mcp::http::RequestSigner>>,
    ) -> IntelClient {
        if let Some(s) = signer {
            self.list.borrow_mut().set_signer(s);
        }
        self
    }

    /// Select the wire dialect from `intelligence.dialect`, applied to every
    /// endpoint in the list. `None` or an unrecognised value keeps the
    /// OpenAI-compatible default; config validation rejects unknown dialects
    /// earlier, so reaching here with one is not a user-visible path.
    /// Builder-style; call before use.
    pub fn with_dialect(self, dialect: Option<&str>) -> IntelClient {
        if let Some(p) = Provider::from_dialect(dialect)
            && p != Provider::OpenAiCompatible
        {
            self.list.borrow_mut().set_provider(p);
        }
        self
    }

    /// Install the edge-triggered all-down reachability reporter: the child
    /// wires this to send an `AgentMsg::IntelHealth` up to the supervisor on
    /// each all-down ENTER/EXIT transition. The callback fires only when the
    /// list's all-down state changes, never once per call, so a steady state
    /// emits nothing. It sits off the data path entirely — a client with no
    /// reporter selects and dials identically.
    pub fn set_health_reporter(&mut self, reporter: Box<dyn Fn(IntelHealthReport)>) {
        self.health_reporter = Some(reporter);
    }

    /// Stamp the run's trace id so each completion carries a `traceparent`
    /// header and the model call joins the run's distributed trace.
    pub fn set_trace_id(&mut self, trace_id: Option<String>) {
        self.trace_id = trace_id;
    }

    /// Enable the all-endpoints-down backoff for a long-lived `loop` or
    /// `reactive` daemon: on all-down, re-arm the failover sweep with bounded
    /// jittered backoff instead of surfacing the terminal immediately.
    /// `once`-mode leaves this unset, so a single sweep leads to exit 4. The run
    /// deadline still bounds the total wait — backoff never extends it, so a
    /// daemon cannot outlive its deadline by retrying.
    pub fn enable_alldown_backoff(&mut self, policy: AllDownPolicy) {
        self.alldown = Some(policy);
    }

    /// The number of configured endpoints; one means no fallback exists.
    pub fn endpoint_count(&self) -> usize {
        self.list.borrow().len()
    }

    /// The run's trace id, if stamped. A hot-swap reads it to re-stamp the
    /// rebuilt client, so the run's trace survives a repoint unbroken.
    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.as_deref()
    }

    /// Whether the all-endpoints-down backoff is enabled, which it is for a
    /// long-lived loop or reactive daemon. A hot-swap reads it so the rebuilt
    /// client preserves the daemon's resilience posture across a repoint rather
    /// than silently reverting to one-shot semantics.
    pub fn alldown_enabled(&self) -> bool {
        self.alldown.is_some()
    }

    /// One completion round-trip, driven through the failover policy. Every
    /// error returned here feeds the exit-code path
    /// (`IntelError` → `LoopAbort::Intel` → exit 4).
    ///
    /// When all endpoints are down and the all-down backoff is enabled (loop and
    /// reactive daemons), the sweep is re-armed with bounded jittered backoff so
    /// a transient host-model roll recovers without killing the daemon;
    /// `once`-mode surfaces the terminal immediately and exits 4. A fatal auth
    /// failure (401/403) is NEVER backed off: it is a misconfiguration that
    /// retrying cannot repair, and retrying it would only delay the operator's
    /// signal.
    pub fn complete(&self, req: &Request) -> Result<Response, IntelError> {
        // One model call. A no-op when metrics are not enabled.
        crate::obs::metrics::record_intel_call();

        let mut attempt: u32 = 0;
        loop {
            let sweep = {
                let mut list = self.list.borrow_mut();
                failover::complete_resilient(&mut list, req, self.timeout, self.trace_id.as_deref())
            };

            // `set_intel_up` reflects the active endpoint's reachability (in rotation).
            {
                let list = self.list.borrow();
                let all_down = list.all_down();
                let active_up = list.ep(list.active()).health.is_up();
                crate::obs::metrics::set_intel_up(active_up && !all_down);
                // Upward report: fire ONLY on an all-down transition, so the
                // supervisor latches the child's reachability and the steady
                // state pays just this bool compare. Carries transport and index
                // only — never a URL or secret.
                if let Some(report) = &self.health_reporter
                    && self.last_all_down.replace(all_down) != all_down
                {
                    let active = (!all_down).then(|| list.active_identity());
                    report(IntelHealthReport { all_down, active });
                }
            }

            match sweep.outcome {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    crate::obs::metrics::record_intel_error(error_reason(&e));
                    // Back off and re-arm only when all three hold: the list is
                    // all-down, a daemon backoff policy is installed, and the
                    // cause is not an auth failure. Anything else — a fatal
                    // class, no policy, or an exhausted retry budget — surfaces
                    // to the caller now.
                    let backoff = match (&self.alldown, &e) {
                        (Some(p), IntelError::AllEndpointsDown(cause))
                            if !cause.as_deref().is_some_and(failover::is_auth)
                                && attempt < p.max_retries =>
                        {
                            *p
                        }
                        _ => return Err(e),
                    };
                    let delay = backoff_delay(&backoff, attempt);
                    attempt += 1;
                    std::thread::sleep(delay);
                    // Loop: the next sweep promotes any elapsed-cooldown breaker to
                    // half-open and resumes the instant an endpoint recovers.
                }
            }
        }
    }

    /// Borrow the endpoint list for the read-only `agentd://intelligence`
    /// resource body. The caller serializes transport, index and health only —
    /// never the URL or any credential, since that body is exposed to whoever
    /// can read the resource.
    pub fn with_list<R>(&self, f: impl FnOnce(&EndpointList) -> R) -> R {
        f(&self.list.borrow())
    }
}

/// The jittered backoff delay for all-down retry `attempt`: the exponential
/// `base × 2^attempt` capped at `max`, then ±25% jitter. Jitter keeps a fleet of
/// agents that lost the same provider from re-dialling in lockstep. The PRNG is
/// a clock-seeded splitmix64 step rather than a `rand` dependency, because the
/// crate's dependency count is a deliberate constraint and backoff jitter has no
/// cryptographic requirement.
fn backoff_delay(policy: &AllDownPolicy, attempt: u32) -> Duration {
    let shift = attempt.min(20);
    let scaled = policy.base.saturating_mul(1u32 << shift).min(policy.max);
    let ms = scaled.as_millis() as u64;
    // ±25% jitter, drawn from a clock-seeded splitmix64 step.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (attempt as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Map z → [-25%, +25%] of ms: window width is ms/2 (rounded up).
    let lo = ms.saturating_sub(ms / 4);
    let window = (ms / 2) + 1;
    Duration::from_millis(lo + z % window)
}

/// Map an [`IntelError`] to the `agentd_intel_errors_total{reason}` label
/// domain. That domain is frozen at `unreachable`, `auth`, `timeout`, `5xx` and
/// `other`: adding a label value would silently break every dashboard and alert
/// built on it, so a new error class must fold into one of these five.
fn error_reason(e: &IntelError) -> &'static str {
    match e {
        IntelError::Transport(io) => match io.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "timeout",
            _ => "unreachable",
        },
        IntelError::Http(401 | 403, _) => "auth",
        IntelError::Http(c, _) if (500..600).contains(c) => "5xx",
        IntelError::Http(_, _) => "other",
        IntelError::Parse(_) | IntelError::Unsupported(_) => "other",
        // All-down classifies by its underlying cause when present.
        IntelError::AllEndpointsDown(Some(cause)) => error_reason(cause),
        IntelError::AllEndpointsDown(None) => "unreachable",
    }
}

/// Parse one intelligence URI element into (transport, http-path, host-header).
/// Shared by [`super::endpoints`], the list parser. The transport is HTTPS-only:
/// `https://host[:port][/path]`. Plaintext `http://` is admitted ONLY for a
/// loopback host, the dev/test carve-out that lets the built-in mock LLM be
/// dialled. Everything else — `unix:`, `vsock:`, non-loopback `http://` — is
/// rejected. This function is the single chokepoint that every construction
/// path flows through (CLI config, spawn payload, hot reload), which is what
/// makes the HTTPS rule impossible to bypass by picking another entry point.
pub(super) fn resolve(
    uri: &str,
    provider: Provider,
) -> Result<(Transport, String, String), IntelError> {
    // `mock:<script>` — the offline dev endpoint: spawns the built-in mock
    // LLM in-process and dials it over loopback. Debug builds always carry it;
    // release only under `--features internal-mocks`, so a production binary
    // has no way to be pointed at fake intelligence.
    #[cfg(any(feature = "internal-mocks", debug_assertions))]
    if let Some(script) = uri.strip_prefix("mock:") {
        let addr = super::mock::inprocess(script).map_err(IntelError::Unsupported)?;
        return resolve(&format!("http://{addr}"), provider);
    }
    #[cfg(not(any(feature = "internal-mocks", debug_assertions)))]
    if uri.starts_with("mock:") {
        return Err(IntelError::Unsupported(
            "mock: intelligence needs a build with --features internal-mocks".into(),
        ));
    }
    let url = Url::parse(uri).map_err(|_| {
        IntelError::Unsupported(format!(
            "intelligence endpoint must be https://host[:port][/path] (got: {uri})"
        ))
    })?;
    let tls = url.is_tls();
    if !tls && !crate::net::http::is_loopback_host(&url.host) {
        return Err(IntelError::Unsupported(format!(
            "plaintext http:// intelligence is allowed for loopback only (dev); use https:// (got: {uri})"
        )));
    }
    let http_path = if url.path == "/" {
        provider.default_path().to_string()
    } else {
        url.path.clone()
    };
    let host_header = url.host_header();
    Ok((
        Transport::Tcp {
            host: url.host,
            port: url.port,
            tls,
        },
        http_path,
        host_header,
    ))
}

impl Transport {
    pub(super) fn connect(&self, timeout: Duration) -> Result<Box<dyn Stream>, IntelError> {
        use crate::net::http;
        match self {
            Transport::Tcp {
                host,
                port,
                tls: false,
            } => Ok(Box::new(http::connect_tcp(host, *port, timeout)?)),
            Transport::Tcp {
                host,
                port,
                tls: true,
            } => connect_tls(host, *port, timeout),
        }
    }
}

#[cfg(feature = "tls")]
fn connect_tls(host: &str, port: u16, timeout: Duration) -> Result<Box<dyn Stream>, IntelError> {
    let tcp = crate::net::http::connect_tcp(host, port, timeout)?;
    Ok(Box::new(
        // Server-authenticated TLS: the endpoint's certificate is verified, but
        // no client identity is presented. Intelligence endpoints authenticate
        // agentd by bearer token or SigV4, not by client certificate.
        crate::net::tls::connect(tcp, host, None).map_err(IntelError::Transport)?,
    ))
}

#[cfg(not(feature = "tls"))]
fn connect_tls(_host: &str, _port: u16, _timeout: Duration) -> Result<Box<dyn Stream>, IntelError> {
    Err(IntelError::Unsupported(
        "https:// intelligence requires building with --features tls".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_non_https_transports() {
        // HTTPS-only: unix: and vsock: targets are rejected at the single
        // resolve() chokepoint rather than being feature-gated away.
        for uri in ["unix:/run/intel.sock", "vsock:2:8080", "not-a-url"] {
            let err = resolve(uri, Provider::OpenAiCompatible).unwrap_err();
            assert!(
                matches!(err, IntelError::Unsupported(_)),
                "{uri} must be rejected, got: {err:?}"
            );
        }
    }

    #[test]
    fn resolve_allows_plaintext_http_for_loopback_only() {
        // The dev/test carve-out: the built-in mock LLM binds 127.0.0.1.
        for uri in [
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            "http://[::1]:8080",
        ] {
            let (t, _p, _h) = resolve(uri, Provider::OpenAiCompatible).unwrap();
            assert!(matches!(t, Transport::Tcp { tls: false, .. }), "{uri}");
        }
        let err = resolve("http://intel.example:8080", Provider::OpenAiCompatible).unwrap_err();
        assert!(
            matches!(err, IntelError::Unsupported(m) if m.contains("loopback")),
            "non-loopback plaintext must be rejected"
        );
    }

    #[test]
    fn resolve_https_full_url() {
        let (t, path, host) = resolve(
            "https://api.openai.com/v1/chat/completions",
            Provider::OpenAiCompatible,
        )
        .unwrap();
        assert!(matches!(
            t,
            Transport::Tcp {
                tls: true,
                port: 443,
                ..
            }
        ));
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(host, "api.openai.com");
    }

    #[test]
    fn resolve_https_host_only_uses_default_path() {
        let (_t, path, _host) =
            resolve("https://gateway.local", Provider::OpenAiCompatible).unwrap();
        assert_eq!(path, "/v1/chat/completions");
    }

    #[test]
    fn single_endpoint_client_builds() {
        let c = IntelClient::from_parts("https://intel.example", None).unwrap();
        assert_eq!(c.endpoint_count(), 1);
    }

    #[test]
    fn comma_list_client_builds_with_all_endpoints() {
        let c = IntelClient::from_parts(
            "https://a.example,https://b.example,https://c.example",
            None,
        )
        .unwrap();
        assert_eq!(c.endpoint_count(), 3);
    }

    #[test]
    fn all_endpoints_down_maps_to_unreachable_reason() {
        // The all-down terminal classifies by its underlying cause.
        let cause = Box::new(IntelError::Http(503, "x".into()));
        assert_eq!(
            error_reason(&IntelError::AllEndpointsDown(Some(cause))),
            "5xx"
        );
        assert_eq!(
            error_reason(&IntelError::AllEndpointsDown(None)),
            "unreachable"
        );
        assert_eq!(error_reason(&IntelError::Http(401, "x".into())), "auth");
    }

    #[test]
    fn trace_header_propagates_to_endpoint_dialect() {
        // Construction does not connect; we only assert the trace id is held and
        // would be applied per endpoint (the per-endpoint dial appends it).
        let mut c = IntelClient::from_parts("https://intel.example", None).unwrap();
        assert!(c.trace_id.is_none());
        c.set_trace_id(Some("1234567890abcdef1234567890abcdef".into()));
        assert!(c.trace_id.is_some());
    }
}
