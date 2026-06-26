//! Intelligence client — transport selection + one round-trip. RFC 0006.
//!
//! The transport (unix / https / vsock) is chosen by the `AGENTD_INTELLIGENCE`
//! URI; the wire is always HTTP/1.1 (the gateway/provider speaks
//! OpenAI-compatible `/chat/completions`). One request opens one connection
//! (`Connection: close`) — simple and robust; pooling is a later optimisation.

use crate::config::Config;
use crate::net::http::{self, Stream, Url};
use crate::wire::intel::{Request, Response};
use std::fmt;
use std::time::Duration;

use super::{anthropic, openai};

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
}

impl fmt::Display for IntelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntelError::Transport(e) => write!(f, "intelligence transport error: {e}"),
            IntelError::Http(code, body) => write!(f, "intelligence HTTP {code}: {body}"),
            IntelError::Parse(m) => write!(f, "{m}"),
            IntelError::Unsupported(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for IntelError {}

impl From<std::io::Error> for IntelError {
    fn from(e: std::io::Error) -> Self {
        IntelError::Transport(e)
    }
}

/// A resolved intelligence endpoint.
pub struct IntelClient {
    transport: Transport,
    http_path: String,
    host_header: String,
    token: Option<String>,
    provider: Provider,
    timeout: Duration,
    /// The run's trace id; when set, every completion carries a `traceparent`
    /// header so the LLM call joins the run's distributed trace (RFC 0010).
    trace_id: Option<String>,
}

enum Transport {
    Unix(String),
    Tcp { host: String, port: u16, tls: bool },
    Vsock { cid: u32, port: u32 },
}

impl IntelClient {
    /// Build from validated config. `cfg.intelligence` is guaranteed present
    /// and well-formed by `Config::validate`.
    pub fn from_config(cfg: &Config) -> Result<IntelClient, IntelError> {
        let uri = cfg.intelligence.as_deref().unwrap_or_default();
        Self::from_parts(uri, cfg.intelligence_token.clone())
    }

    /// Build from explicit parts (the subagent path — the spawn payload, not
    /// CLI `Config`). Provider is OpenAI-compatible by default (RFC 0006).
    pub fn from_parts(uri: &str, token: Option<String>) -> Result<IntelClient, IntelError> {
        let provider = Provider::OpenAiCompatible;
        let (transport, http_path, host_header) = resolve(uri, provider)?;
        Ok(IntelClient {
            transport,
            http_path,
            host_header,
            token,
            provider,
            // Generous per-call ceiling; the run deadline is the real bound.
            timeout: Duration::from_secs(120),
            trace_id: None,
        })
    }

    /// Stamp the run's trace id so each completion carries a `traceparent`
    /// header (the LLM call joins the run's distributed trace, RFC 0010).
    pub fn set_trace_id(&mut self, trace_id: Option<String>) {
        self.trace_id = trace_id;
    }

    /// Append a fresh-span `traceparent` header when a trace id is set. Pure
    /// (testable without a connection).
    fn apply_trace_header(&self, headers: &mut Vec<(String, String)>) {
        if let Some(tid) = &self.trace_id {
            headers.push((
                "traceparent".into(),
                crate::obs::trace::outbound_traceparent(tid),
            ));
        }
    }

    /// One completion round-trip.
    pub fn complete(&self, req: &Request) -> Result<Response, IntelError> {
        let (body, mut headers) = match self.provider {
            Provider::OpenAiCompatible => openai::build_request(req, self.token.as_deref()),
            Provider::Anthropic => anthropic::build_request(req, self.token.as_deref()),
        };
        self.apply_trace_header(&mut headers);
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let mut stream = self.transport.connect(self.timeout)?;
        let resp = http::send(
            stream.as_mut(),
            &self.host_header,
            "POST",
            &self.http_path,
            &header_refs,
            &body,
        )?;

        if !resp.is_success() {
            let snippet: String = resp.body_str().chars().take(512).collect();
            return Err(IntelError::Http(resp.status, snippet));
        }

        match self.provider {
            Provider::OpenAiCompatible => openai::parse_response(&resp.body),
            Provider::Anthropic => anthropic::parse_response(&resp.body),
        }
        .map_err(IntelError::Parse)
    }
}

/// Parse the intelligence URI into (transport, http-path, host-header).
fn resolve(uri: &str, provider: Provider) -> Result<(Transport, String, String), IntelError> {
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
    fn connect(&self, timeout: Duration) -> Result<Box<dyn Stream>, IntelError> {
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
    let tcp = http::connect_tcp(host, port, timeout)?;
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
    fn trace_header_appended_only_when_set() {
        // Constructing the client does not connect, so this is a pure check of
        // the outbound-trace wiring (RFC 0010).
        let mut c = IntelClient::from_parts("unix:/run/intel.sock", None).unwrap();
        let mut headers = vec![("authorization".to_string(), "Bearer x".to_string())];

        c.apply_trace_header(&mut headers);
        assert_eq!(headers.len(), 1, "no trace id set → no traceparent header");

        let tid = "1234567890abcdef1234567890abcdef";
        c.set_trace_id(Some(tid.to_string()));
        c.apply_trace_header(&mut headers);
        assert_eq!(headers.len(), 2);
        let (k, v) = &headers[1];
        assert_eq!(k, "traceparent");
        assert!(
            v.contains(tid),
            "traceparent carries the run's trace id: {v}"
        );
        assert!(
            v.starts_with("00-") && v.ends_with("-01"),
            "well-formed traceparent: {v}"
        );
    }
}
