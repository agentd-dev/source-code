//! Intelligence client — endpoint *list* selection + one round-trip. RFC 0006 + RFC 0018.
//!
//! The transport (unix / https / vsock) is chosen by each `AGENTD_INTELLIGENCE`
//! list element; the wire is always HTTP/1.1 (the gateway/provider speaks
//! OpenAI-compatible `/chat/completions`). One request opens one connection
//! (`Connection: close`) — simple and robust; pooling is a non-goal (RFC 0018 §10).
//!
//! RFC 0018 makes the channel **resilient**: `--intelligence` is an ordered list
//! (primary + fallbacks); `complete()` drives the list through the sticky-primary
//! failover policy ([`super::failover`]) with a per-endpoint health record +
//! circuit breaker ([`super::health`]). A single-element list is byte-for-byte RFC
//! 0006 behaviour — the failover machinery is inert with one endpoint. The
//! wire/adapter/JSON path is UNCHANGED; only endpoint *selection* wraps it.

use crate::net::http::{Stream, Url};
use crate::wire::intel::{Request, Response};
use std::cell::RefCell;
use std::fmt;
use std::time::Duration;

use super::endpoints::EndpointList;
use super::{anthropic, failover, openai};

/// Which in-binary adapter speaks to the endpoint. OpenAI-compatible is the
/// canonical default; anthropic is the only other in-binary dialect. Anything
/// else lives behind a gateway (RFC 0006 §two-adapters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAiCompatible,
    Anthropic,
}

impl Provider {
    fn default_path(self) -> &'static str {
        match self {
            Provider::OpenAiCompatible => openai::DEFAULT_PATH,
            Provider::Anthropic => anthropic::DEFAULT_PATH,
        }
    }
}

#[derive(Debug)]
pub enum IntelError {
    /// Transport / connection failure (fatal infra → exit 4, RFC 0011).
    Transport(std::io::Error),
    /// Non-2xx HTTP status from the endpoint.
    Http(u16, String),
    /// Malformed response body.
    Parse(String),
    /// A transport this build doesn't support (e.g. https without `tls`).
    Unsupported(String),
    /// Every endpoint in the list is down/broken after the bounded failover
    /// sweep (RFC 0018 §6). The boxed cause is the last failover-class error
    /// seen. Maps to the same fatal-infra class as `Transport` → exit 4 on
    /// `once`; a loop/reactive daemon backs off rather than crashing.
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

/// A resolved intelligence client over an ordered endpoint list (RFC 0018 §3).
pub struct IntelClient {
    /// The endpoint list + sticky-primary cursor + per-endpoint health/breaker.
    /// Behind a `RefCell` so `complete(&self)` can advance the cursor / record
    /// health without forcing a `&mut` through the loop's call sites (the
    /// per-subagent client is single-threaded; this is interior mutability, not
    /// sharing). RFC 0018 §5.2 will swap the whole `IntelClient` on hot reload.
    list: RefCell<EndpointList>,
    timeout: Duration,
    /// The run's trace id; when set, every completion carries a `traceparent`
    /// header so the LLM call joins the run's distributed trace (RFC 0010).
    trace_id: Option<String>,
    /// All-endpoints-down backoff policy (RFC 0018 §6). `None` (the default,
    /// `once`-mode) means a single sweep: all-down returns immediately and the
    /// caller maps it to exit 4. `Some(policy)` (loop/reactive daemons) re-runs
    /// the sweep with bounded jittered backoff so a transient host-model roll
    /// recovers without the daemon crashing — it resumes the instant any endpoint
    /// half-opens healthy.
    alldown: Option<AllDownPolicy>,
}

/// Bounded, jittered all-down backoff (RFC 0018 §6 / §4.2). A daemon re-arms the
/// sweep up to `max_retries` times, sleeping `base × 2^n` (capped at `max`) with
/// per-attempt jitter, before surfacing the terminal all-down.
#[derive(Debug, Clone, Copy)]
pub struct AllDownPolicy {
    pub max_retries: u32,
    pub base: Duration,
    pub max: Duration,
}

impl Default for AllDownPolicy {
    fn default() -> AllDownPolicy {
        // The §4.2 default: 1s..30s jittered.
        AllDownPolicy {
            max_retries: 8,
            base: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

/// Per-endpoint dial transport — RFC 0006 verbatim, now owned by [`super::endpoints`].
pub enum Transport {
    Unix(String),
    Tcp { host: String, port: u16, tls: bool },
    Vsock { cid: u32, port: u32 },
}

impl IntelClient {
    /// Build from explicit parts (the subagent path — the spawn payload, not CLI
    /// `Config`). `uri` is the RFC 0018 endpoint *list* (a single element is RFC
    /// 0006). `default_token` is endpoint 1's resolved credential when its env
    /// override is unset; later endpoints resolve their own `_<N>` token.
    pub fn from_parts(uri: &str, default_token: Option<String>) -> Result<IntelClient, IntelError> {
        let list = EndpointList::parse(uri, default_token)?;
        Ok(IntelClient {
            list: RefCell::new(list),
            // Generous per-call ceiling; the run deadline is the real bound.
            timeout: Duration::from_secs(120),
            trace_id: None,
            alldown: None,
        })
    }

    /// Stamp the run's trace id so each completion carries a `traceparent`
    /// header (the LLM call joins the run's distributed trace, RFC 0010).
    pub fn set_trace_id(&mut self, trace_id: Option<String>) {
        self.trace_id = trace_id;
    }

    /// Enable the all-endpoints-down backoff (RFC 0018 §6) for a long-lived
    /// `loop`/`reactive` daemon: on all-down, re-arm the failover sweep with
    /// bounded jittered backoff instead of surfacing the terminal immediately.
    /// `once`-mode leaves this unset (a single sweep → exit 4). The run deadline
    /// still bounds the total wait (RFC 0011 §5 — backoff does not extend it).
    pub fn enable_alldown_backoff(&mut self, policy: AllDownPolicy) {
        self.alldown = Some(policy);
    }

    /// The number of configured endpoints (1 == RFC 0006).
    pub fn endpoint_count(&self) -> usize {
        self.list.borrow().len()
    }

    /// The run's trace id, if stamped (RFC 0010). Read on a hot-swap (RFC 0018
    /// §5.2) to re-stamp the rebuilt client so it keeps joining the run's trace.
    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.as_deref()
    }

    /// Whether the all-endpoints-down backoff is enabled (a long-lived
    /// loop/reactive daemon). Read on a hot-swap (RFC 0018 §5.2) so the rebuilt
    /// client preserves the daemon's resilience posture across a repoint.
    pub fn alldown_enabled(&self) -> bool {
        self.alldown.is_some()
    }

    /// One completion round-trip, driven through the failover policy (RFC 0018
    /// §3.3). The call-site signature is unchanged from RFC 0006, so the whole
    /// exit-code path (`IntelError` → `LoopAbort::Intel` → exit 4) is intact.
    ///
    /// When all endpoints are down (§6) and the all-down backoff is enabled
    /// (loop/reactive daemons), the sweep is re-armed with bounded jittered
    /// backoff so a transient host-model roll recovers without crashing the
    /// daemon; `once`-mode surfaces the terminal immediately (→ exit 4). A fatal
    /// auth failure (401/403) is NEVER backed off — it is a misconfiguration, not
    /// a transient outage (§6).
    pub fn complete(&self, req: &Request) -> Result<Response, IntelError> {
        // --- RFC 0018 §4.3 metric call site: one model call (no-op without metrics).
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
                let active_up = list.ep(list.active()).health.is_up();
                crate::obs::metrics::set_intel_up(active_up && !list.all_down());
            }

            match sweep.outcome {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    crate::obs::metrics::record_intel_error(error_reason(&e));
                    // All-down + a daemon backoff policy + an auth-free cause →
                    // back off and re-arm (§6). Anything else (a fatal class, no
                    // policy, or an exhausted budget) surfaces now.
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
    /// resource body (§4.4). The caller serializes transport/index/health only —
    /// NEVER the URL or any credential (RFC 0012 §3.7).
    pub fn with_list<R>(&self, f: impl FnOnce(&EndpointList) -> R) -> R {
        f(&self.list.borrow())
    }
}

/// The jittered backoff delay for all-down retry `attempt` (RFC 0018 §6): the
/// exponential `base × 2^attempt` capped at `max`, then ±25% jitter from a cheap
/// clock-seeded PRNG (no `rand` dependency — the minimalism moat).
fn backoff_delay(policy: &AllDownPolicy, attempt: u32) -> Duration {
    let shift = attempt.min(20);
    let scaled = policy.base.saturating_mul(1u32 << shift).min(policy.max);
    let ms = scaled.as_millis() as u64;
    // ±25% jitter, drawn from a clock-seeded splitmix64 step (no `rand` dep).
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

/// Map an [`IntelError`] to the frozen `agentd_intel_errors_total{reason}` label
/// domain (RFC 0016 §4.3: `unreachable`|`auth`|`timeout`|`5xx`|`other`).
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
/// Shared by [`super::endpoints`] (the list parser); unchanged from RFC 0006.
pub(super) fn resolve(
    uri: &str,
    provider: Provider,
) -> Result<(Transport, String, String), IntelError> {
    if let Some(path) = uri.strip_prefix("unix:") {
        return Ok((
            Transport::Unix(path.to_string()),
            provider.default_path().into(),
            "localhost".into(),
        ));
    }
    if let Some(rest) = uri.strip_prefix("vsock:") {
        let (cid, port) = rest
            .split_once(':')
            .and_then(|(c, p)| Some((c.parse().ok()?, p.parse().ok()?)))
            .ok_or_else(|| {
                IntelError::Unsupported(format!("bad vsock URI (want vsock:cid:port): {uri}"))
            })?;
        return Ok((
            Transport::Vsock { cid, port },
            provider.default_path().into(),
            "localhost".into(),
        ));
    }
    // http(s)
    let url = Url::parse(uri).map_err(IntelError::Unsupported)?;
    let http_path = if url.path == "/" {
        provider.default_path().to_string()
    } else {
        url.path.clone()
    };
    let host_header = url.host_header();
    let tls = url.is_tls();
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
            Transport::Unix(path) => Ok(Box::new(crate::net::unixsock::connect(path, timeout)?)),
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
            Transport::Vsock { cid, port } => connect_vsock(*cid, *port, timeout),
        }
    }
}

#[cfg(feature = "tls")]
fn connect_tls(host: &str, port: u16, timeout: Duration) -> Result<Box<dyn Stream>, IntelError> {
    let tcp = crate::net::http::connect_tcp(host, port, timeout)?;
    Ok(Box::new(
        crate::net::tls::connect(tcp, host).map_err(IntelError::Transport)?,
    ))
}

#[cfg(not(feature = "tls"))]
fn connect_tls(_host: &str, _port: u16, _timeout: Duration) -> Result<Box<dyn Stream>, IntelError> {
    Err(IntelError::Unsupported(
        "https:// intelligence requires building with --features tls (or use unix: to a sidecar that terminates TLS)".into(),
    ))
}

#[cfg(feature = "vsock")]
fn connect_vsock(cid: u32, port: u32, timeout: Duration) -> Result<Box<dyn Stream>, IntelError> {
    Ok(Box::new(
        crate::net::vsock::connect(cid, port, timeout).map_err(IntelError::Transport)?,
    ))
}

#[cfg(not(feature = "vsock"))]
fn connect_vsock(_cid: u32, _port: u32, _timeout: Duration) -> Result<Box<dyn Stream>, IntelError> {
    Err(IntelError::Unsupported(
        "vsock:// intelligence requires building with --features vsock".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_unix() {
        let (t, path, host) = resolve("unix:/run/intel.sock", Provider::OpenAiCompatible).unwrap();
        assert!(matches!(t, Transport::Unix(p) if p == "/run/intel.sock"));
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(host, "localhost");
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
    fn resolve_vsock() {
        let (t, _p, _h) = resolve("vsock:2:8080", Provider::OpenAiCompatible).unwrap();
        assert!(matches!(t, Transport::Vsock { cid: 2, port: 8080 }));
    }

    #[test]
    fn single_endpoint_client_builds() {
        let c = IntelClient::from_parts("unix:/run/intel.sock", None).unwrap();
        assert_eq!(c.endpoint_count(), 1);
    }

    #[test]
    fn comma_list_client_builds_with_all_endpoints() {
        let c = IntelClient::from_parts("unix:/a,unix:/b,unix:/c", None).unwrap();
        assert_eq!(c.endpoint_count(), 3);
    }

    #[test]
    fn all_endpoints_down_maps_to_unreachable_reason() {
        // exercises the §4.3 reason mapping for the §6 terminal.
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
        let mut c = IntelClient::from_parts("unix:/run/intel.sock", None).unwrap();
        assert!(c.trace_id.is_none());
        c.set_trace_id(Some("1234567890abcdef1234567890abcdef".into()));
        assert!(c.trace_id.is_some());
    }
}
