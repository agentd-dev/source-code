//! Intelligence client trait + Unix-socket / HTTP / mock implementations.
//!
//! The handler layer talks to `dyn IntelligenceClient`. Each real
//! transport owns nothing beyond a dial address + timeouts; the
//! JSON-RPC envelope and length framing live in `protocol.rs`.

use std::sync::{Arc, Mutex};
#[cfg(any(unix, feature = "intel-http"))]
use std::time::Duration;

use crate::error::{Error, Result};
use crate::intelligence::protocol::{Request, Response};

// Both the Unix socket client and the HTTP client need the RPC
// envelope types + framing; only the Mock client and the reloadable
// wrapper don't. Gate to the union of those feature sets rather
// than to a single one.
#[cfg(any(unix, feature = "intel-http"))]
use crate::intelligence::protocol::{RpcRequest, RpcResponse};
#[cfg(unix)]
use crate::intelligence::protocol::{read_frame, write_frame};

#[cfg(any(unix, feature = "intel-http"))]
use std::io::{BufReader, BufWriter};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(feature = "intel-http")]
use std::io::{Read, Write};
#[cfg(feature = "intel-http")]
use std::net::{TcpStream, ToSocketAddrs};

/// One intelligence call. Synchronous — the workflow engine is
/// single-threaded per run and blocks the current node on the
/// response by design (RFC §15.3).
pub trait IntelligenceClient: Send + Sync {
    fn complete(&self, request: &Request) -> Result<Response>;
}

/// Shared handle used by registered handlers.
pub type IntelligenceRef = Arc<dyn IntelligenceClient>;

// ---------------------------------------------------------------------------
// Unix-socket client (Unix-only)
// ---------------------------------------------------------------------------

/// Connects to `/run/intelligence.sock` (or any path given), writes
/// one `complete` request per call, reads one response, closes.
/// Matches `sandbox::intelligence_server`'s wire so the same server
/// serves both the appliance's in-guest `agent` binary and the
/// workflow runtime.
///
/// `UnixClient` uses a monotonic per-instance id counter so
/// well-behaved servers can log the pairing, but correlation is not
/// required (one connection = one request/response pair).
///
/// Not available on Windows — use `--intel-http` there (enable the
/// `intel-http` feature). The server startup path refuses
/// `--intel-unix` on Windows with a clear error message.
#[cfg(unix)]
pub struct UnixClient {
    endpoint: PathBuf,
    timeout: Duration,
    id: Mutex<u64>,
}

#[cfg(unix)]
impl UnixClient {
    pub fn new(endpoint: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeout,
            id: Mutex::new(1),
        }
    }

    fn next_id(&self) -> u64 {
        let mut guard = self
            .id
            .lock()
            .expect("intelligence client id mutex poisoned");
        let id = *guard;
        *guard = id.wrapping_add(1).max(1);
        id
    }

    fn endpoint(&self) -> &Path {
        &self.endpoint
    }
}

#[cfg(unix)]
impl IntelligenceClient for UnixClient {
    fn complete(&self, request: &Request) -> Result<Response> {
        let stream = UnixStream::connect(self.endpoint())
            .map_err(|e| Error::Intelligence(format!("dial {}: {e}", self.endpoint().display())))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|e| Error::Intelligence(format!("set_read_timeout: {e}")))?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|e| Error::Intelligence(format!("set_write_timeout: {e}")))?;

        let id = self.next_id();
        let envelope = RpcRequest {
            jsonrpc: "2.0",
            id,
            method: "complete",
            params: request,
        };
        let payload = serde_json::to_vec(&envelope).map_err(Error::Json)?;

        // Split stream so write / read buffers don't wrap each other.
        let mut writer = BufWriter::new(&stream);
        write_frame(&mut writer, &payload)
            .map_err(|e| Error::Intelligence(format!("write request: {e}")))?;
        drop(writer);

        let mut reader = BufReader::new(&stream);
        let frame = read_frame(&mut reader)
            .map_err(|e| Error::Intelligence(format!("read response: {e}")))?;
        let parsed: RpcResponse = serde_json::from_slice(&frame).map_err(Error::Json)?;

        if let Some(err) = parsed.error {
            return Err(Error::Intelligence(format!(
                "backend error {code}: {msg}",
                code = err.code,
                msg = err.message
            )));
        }
        parsed.result.ok_or_else(|| {
            Error::Intelligence(
                "intelligence response contained neither `result` nor `error`".into(),
            )
        })
    }
}

// ---------------------------------------------------------------------------
// HTTP client (feature `intel-http`)
// ---------------------------------------------------------------------------

/// Sends the same JSON-RPC 2.0 `complete` request the Unix client
/// does, as an HTTP POST to a configured endpoint. One
/// request/response per connection (`Connection: close`) so the
/// server can fan traffic across workers without us tracking keep
/// alive state.
///
/// v1: plain HTTP only. HTTPS is on the roadmap behind a separate
/// feature; for now, operators who need it terminate TLS at a
/// sidecar or reverse proxy and point `--intel-http` at the
/// localhost plaintext port. See `docs/capabilities.md
/// §llm_infer` for the deployment recipes.
///
/// Optional bearer auth via `Authorization: Bearer <token>`. The
/// token is held in-memory — pass it via env (`AGENTD_INTEL_HTTP_BEARER`)
/// or the CLI (`--intel-http-bearer PATH`).
#[cfg(feature = "intel-http")]
#[derive(Debug)]
pub struct HttpClient {
    endpoint: ParsedEndpoint,
    timeout: Duration,
    bearer_token: Option<String>,
    id: Mutex<u64>,
}

#[cfg(feature = "intel-http")]
#[derive(Debug, Clone)]
struct ParsedEndpoint {
    host: String,
    port: u16,
    path: String,
}

#[cfg(feature = "intel-http")]
impl HttpClient {
    /// `url` is `http://HOST[:PORT]/PATH`. Malformed URLs surface as
    /// `Error::Config` so the process fails at spawn rather than on
    /// the first inference call.
    pub fn new(url: &str, timeout: Duration) -> Result<Self> {
        Self::with_bearer(url, timeout, None)
    }

    /// Same as [`new`] but attaches a bearer token to every request.
    pub fn with_bearer(url: &str, timeout: Duration, bearer_token: Option<String>) -> Result<Self> {
        let endpoint = ParsedEndpoint::parse(url)?;
        Ok(Self {
            endpoint,
            timeout,
            bearer_token,
            id: Mutex::new(1),
        })
    }

    fn next_id(&self) -> u64 {
        let mut guard = self.id.lock().expect("intelligence HTTP id mutex poisoned");
        let id = *guard;
        *guard = id.wrapping_add(1).max(1);
        id
    }
}

#[cfg(feature = "intel-http")]
impl ParsedEndpoint {
    fn parse(raw: &str) -> Result<Self> {
        let rest = raw.strip_prefix("http://").ok_or_else(|| {
            Error::Config(format!(
                "intelligence HTTP endpoint must start with `http://`; got `{raw}`. \
                     HTTPS upstreams are not supported in v1 — terminate TLS at a sidecar \
                     and point --intel-http at the plaintext port."
            ))
        })?;
        let (authority, path) = match rest.find('/') {
            Some(slash) => (&rest[..slash], &rest[slash..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(Error::Config(format!(
                "intelligence HTTP endpoint has no host: `{raw}`"
            )));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => {
                let port = p.parse::<u16>().map_err(|_| {
                    Error::Config(format!(
                        "intelligence HTTP endpoint has invalid port in `{authority}`"
                    ))
                })?;
                (h.to_string(), port)
            }
            None => (authority.to_string(), 80),
        };
        Ok(Self {
            host,
            port,
            path: path.to_string(),
        })
    }
}

#[cfg(feature = "intel-http")]
impl IntelligenceClient for HttpClient {
    fn complete(&self, request: &Request) -> Result<Response> {
        let id = self.next_id();
        let envelope = RpcRequest {
            jsonrpc: "2.0",
            id,
            method: "complete",
            params: request,
        };
        let payload = serde_json::to_vec(&envelope).map_err(Error::Json)?;

        // Connect.
        let addr_str = format!("{}:{}", self.endpoint.host, self.endpoint.port);
        let sock = addr_str
            .to_socket_addrs()
            .map_err(|e| Error::Intelligence(format!("resolve {addr_str}: {e}")))?
            .next()
            .ok_or_else(|| Error::Intelligence(format!("no address for {addr_str}")))?;
        let mut stream = TcpStream::connect_timeout(&sock, self.timeout)
            .map_err(|e| Error::Intelligence(format!("connect {addr_str}: {e}")))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|()| stream.set_write_timeout(Some(self.timeout)))
            .map_err(|e| Error::Intelligence(format!("set timeouts: {e}")))?;

        // Build request.
        let mut req = String::with_capacity(256 + payload.len());
        req.push_str(&format!("POST {} HTTP/1.1\r\n", self.endpoint.path));
        req.push_str(&format!(
            "Host: {}:{}\r\n",
            self.endpoint.host, self.endpoint.port
        ));
        req.push_str("Connection: close\r\n");
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", payload.len()));
        if let Some(t) = &self.bearer_token {
            req.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        req.push_str("\r\n");
        stream
            .write_all(req.as_bytes())
            .and_then(|()| stream.write_all(&payload))
            .and_then(|()| stream.flush())
            .map_err(|e| Error::Intelligence(format!("write request: {e}")))?;

        // Read response.
        let mut reader = BufReader::new(&stream);
        let status = parse_http_status(&mut reader)?;
        let (_headers, body_len) = parse_http_headers(&mut reader)?;
        if !(200..300).contains(&status) {
            return Err(Error::Intelligence(format!(
                "upstream returned HTTP {status}"
            )));
        }

        // Body: bounded by a cap to avoid OOM on a misbehaving backend.
        const MAX_BODY: usize = 4 * 1024 * 1024;
        let take = body_len.min(MAX_BODY);
        let mut body = Vec::with_capacity(take.min(64 * 1024));
        reader
            .by_ref()
            .take(take as u64)
            .read_to_end(&mut body)
            .map_err(|e| Error::Intelligence(format!("read body: {e}")))?;
        if body_len > MAX_BODY {
            return Err(Error::Intelligence(format!(
                "response body {body_len} > cap {MAX_BODY}"
            )));
        }

        let parsed: RpcResponse = serde_json::from_slice(&body).map_err(Error::Json)?;
        if let Some(err) = parsed.error {
            return Err(Error::Intelligence(format!(
                "backend error {code}: {msg}",
                code = err.code,
                msg = err.message
            )));
        }
        parsed.result.ok_or_else(|| {
            Error::Intelligence(
                "intelligence HTTP response contained neither `result` nor `error`".into(),
            )
        })
    }
}

#[cfg(feature = "intel-http")]
fn parse_http_status<R: std::io::BufRead>(reader: &mut R) -> Result<u16> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| Error::Intelligence(format!("read status line: {e}")))?;
    let mut parts = line.trim_end().split(' ');
    let _http = parts.next();
    let code_str = parts
        .next()
        .ok_or_else(|| Error::Intelligence(format!("malformed status line: `{}`", line.trim())))?;
    code_str
        .parse::<u16>()
        .map_err(|_| Error::Intelligence(format!("non-numeric status: `{code_str}`")))
}

#[cfg(feature = "intel-http")]
fn parse_http_headers<R: std::io::BufRead>(
    reader: &mut R,
) -> Result<(std::collections::HashMap<String, String>, usize)> {
    let mut headers = std::collections::HashMap::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| Error::Intelligence(format!("read header: {e}")))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let value = v.trim().to_string();
            if key == "content-length"
                && let Ok(n) = value.parse::<usize>()
            {
                content_length = n;
            }
            headers.insert(key, value);
        }
    }
    Ok((headers, content_length))
}

// ---------------------------------------------------------------------------
// Mock client — test-only. Deterministic canned responses.
// ---------------------------------------------------------------------------

/// Test client that hands back a canned response per call. Stores
/// the requests it received for assertions.
#[derive(Debug, Default)]
pub struct MockClient {
    responses: Mutex<Vec<Response>>,
    received: Mutex<Vec<Request>>,
}

impl MockClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a response to be returned on the next `complete` call.
    /// Calls are served in FIFO order; an empty queue yields an error.
    pub fn enqueue(&self, response: Response) {
        self.responses.lock().unwrap().push(response);
    }

    /// Convenience: enqueue a plain-text response.
    pub fn enqueue_text(&self, text: impl Into<String>) {
        self.enqueue(Response {
            content: text.into(),
            usage: Default::default(),
        });
    }

    pub fn received(&self) -> Vec<Request> {
        self.received.lock().unwrap().clone()
    }
}

impl IntelligenceClient for MockClient {
    fn complete(&self, request: &Request) -> Result<Response> {
        self.received.lock().unwrap().push(request.clone());
        let mut queue = self.responses.lock().unwrap();
        if queue.is_empty() {
            return Err(Error::Intelligence(
                "MockClient: no canned response enqueued".into(),
            ));
        }
        Ok(queue.remove(0))
    }
}

// ---------------------------------------------------------------------------
// Hot-reloadable intelligence wrapper
// ---------------------------------------------------------------------------

/// [`IntelligenceClient`] implementation that holds its inner
/// `Box<dyn IntelligenceClient>` behind an [`arc_swap::ArcSwap`] so
/// the runtime can replace the client atomically on SIGHUP. Handlers
/// continue holding an unchanging `Arc<dyn IntelligenceClient>`;
/// every call dereferences through the ArcSwap to the current
/// inner.
///
/// Typical reload use cases (bearer token rotation, endpoint
/// pointer flip, Unix → HTTP transport switch) all rebuild the
/// whole client from scratch and swap — there's no "partial
/// reload" concept.
pub struct ReloadableIntelClient {
    inner: arc_swap::ArcSwap<Box<dyn IntelligenceClient>>,
}

/// Forwarder so an already-shared `Arc<dyn IntelligenceClient>`
/// (tests, embedders) can sit behind the reloadable wrapper.
struct SharedClient(IntelligenceRef);
impl IntelligenceClient for SharedClient {
    fn complete(&self, request: &Request) -> Result<Response> {
        self.0.complete(request)
    }
}

impl ReloadableIntelClient {
    /// Wrap a shared handle. Test + embedder convenience; the
    /// runtime path constructs owned transports via [`Self::new`].
    pub fn from_ref(inner: IntelligenceRef) -> Self {
        Self::new(Box::new(SharedClient(inner)))
    }

    /// Wrap an initial client. Returned value implements
    /// [`IntelligenceClient`]; put it in an `Arc` and pass it to
    /// `intelligence::handler::register` exactly like any other
    /// `IntelligenceRef`.
    pub fn new(initial: Box<dyn IntelligenceClient>) -> Self {
        Self {
            inner: arc_swap::ArcSwap::from_pointee(initial),
        }
    }

    /// Atomically replace the inner client. In-flight calls that
    /// have already dereferenced the previous inner complete
    /// against it; subsequent calls see the new client.
    pub fn swap(&self, next: Box<dyn IntelligenceClient>) {
        self.inner.store(Arc::new(next));
    }
}

impl IntelligenceClient for ReloadableIntelClient {
    fn complete(&self, request: &Request) -> Result<Response> {
        self.inner.load().complete(request)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::protocol::Message;
    #[cfg(unix)]
    use std::io::{BufReader, BufWriter, Read, Write};
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use tempfile::TempDir;

    fn sample_request() -> Request {
        Request {
            model: "fast".into(),
            messages: vec![Message {
                role: "user".into(),
                content: "ping".into(),
            }],
            max_tokens: Some(16),
            temperature: None,
        }
    }

    #[test]
    fn mock_client_round_trip() {
        let c = MockClient::new();
        c.enqueue_text("pong");
        let out = c.complete(&sample_request()).unwrap();
        assert_eq!(out.content, "pong");
        assert_eq!(c.received().len(), 1);
    }

    #[test]
    fn mock_client_errors_when_empty() {
        let c = MockClient::new();
        let err = c.complete(&sample_request()).unwrap_err();
        assert!(format!("{err}").contains("no canned response"));
    }

    #[test]
    fn reloadable_swap_redirects_calls() {
        // Prove the trait-object clone handlers hold sees the swap
        // — same pattern the policy reload test exercises, mirrored
        // here for the intelligence path.
        let first = Box::new(MockClient::new()) as Box<dyn IntelligenceClient>;
        let reloadable = Arc::new(ReloadableIntelClient::new(first));

        // The Arc dyn view handlers would hold:
        let dyn_view: IntelligenceRef = reloadable.clone();

        // First inner has no canned response — expect an error.
        let err = dyn_view.complete(&sample_request()).unwrap_err();
        assert!(format!("{err}").contains("no canned response"));

        // Swap to a second client that has a response enqueued.
        let second = MockClient::new();
        second.enqueue_text("hello-after-reload");
        reloadable.swap(Box::new(second) as Box<dyn IntelligenceClient>);

        let resp = dyn_view.complete(&sample_request()).unwrap();
        assert_eq!(resp.content, "hello-after-reload");
    }

    /// End-to-end test against an in-process Unix socket "server"
    /// that mimics `sandbox::intelligence_server`'s frame protocol.
    #[cfg(unix)]
    #[test]
    fn unix_client_round_trip_against_fake_server() {
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("intel.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        // Server side: read one frame, write a canned response.
        let server_handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let request_frame = read_frame(&mut reader).expect("read request");
            let parsed: serde_json::Value = serde_json::from_slice(&request_frame).unwrap();
            assert_eq!(parsed["method"], "complete");
            assert_eq!(parsed["params"]["model"], "fast");

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": parsed["id"],
                "result": {
                    "content": "pong",
                    "usage": { "prompt_tokens": 3, "completion_tokens": 1 }
                }
            });
            let payload = serde_json::to_vec(&response).unwrap();
            let mut writer = BufWriter::new(&stream);
            write_frame(&mut writer, &payload).unwrap();
            writer.flush().unwrap();
        });

        let client = UnixClient::new(sock_path, Duration::from_secs(2));
        let resp = client.complete(&sample_request()).unwrap();
        assert_eq!(resp.content, "pong");
        assert_eq!(resp.usage.prompt_tokens, 3);

        server_handle.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_client_surfaces_rpc_error() {
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("intel.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server_handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let _ = read_frame(&mut reader);
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": { "code": -32001, "message": "script miss" }
            });
            let payload = serde_json::to_vec(&response).unwrap();
            let mut writer = BufWriter::new(&stream);
            write_frame(&mut writer, &payload).unwrap();
        });

        let client = UnixClient::new(sock_path, Duration::from_secs(2));
        let err = client.complete(&sample_request()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("-32001"), "msg: {msg}");
        assert!(msg.contains("script miss"), "msg: {msg}");
        server_handle.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_client_fails_on_missing_endpoint() {
        let client = UnixClient::new("/definitely/not/a/socket", Duration::from_millis(100));
        let err = client.complete(&sample_request()).unwrap_err();
        assert!(format!("{err}").contains("dial"));
    }

    #[cfg(unix)]
    #[test]
    fn monotonic_request_ids() {
        let client = UnixClient::new("/tmp", Duration::from_secs(1));
        let a = client.next_id();
        let b = client.next_id();
        let c = client.next_id();
        assert!(a < b && b < c);
    }

    // Silence the unused-imports warning if the Write import migrates.
    #[cfg(unix)]
    fn _retain_write_import_for_future_use() {
        let _w: Option<Box<dyn Write>> = None;
        let _r: Option<Box<dyn Read>> = None;
    }

    // ---------------------------------------------------------------------
    // HTTP client
    // ---------------------------------------------------------------------
    #[cfg(feature = "intel-http")]
    mod http {
        use super::super::*;
        use super::sample_request;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::thread;

        /// Tiny in-process HTTP server that reads one request and
        /// writes the supplied response. Returns `(addr, handler,
        /// captured_request)` — tests assert on the captured bytes.
        fn spawn_fake_server(response_body: &[u8]) -> (String, thread::JoinHandle<Vec<u8>>) {
            spawn_fake_server_ext(200, "OK", response_body)
        }
        fn spawn_fake_server_ext(
            status_code: u16,
            status_reason: &'static str,
            response_body: &[u8],
        ) -> (String, thread::JoinHandle<Vec<u8>>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = format!("http://{}/rpc", listener.local_addr().unwrap());
            let body = response_body.to_vec();
            let handle = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(&stream);
                // Read request line + headers.
                let mut req_bytes = Vec::new();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    let n = reader.read_line(&mut line).unwrap();
                    if n == 0 {
                        break;
                    }
                    req_bytes.extend_from_slice(line.as_bytes());
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = rest.trim().parse::<usize>().unwrap_or(0);
                    }
                }
                // Read body.
                let mut body_buf = vec![0u8; content_length];
                reader.read_exact(&mut body_buf).unwrap();
                req_bytes.extend_from_slice(&body_buf);

                // Write response.
                let resp = format!(
                    "HTTP/1.1 {status_code} {status_reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
                stream.flush().unwrap();
                req_bytes
            });
            (addr, handle)
        }

        #[test]
        fn http_client_round_trip() {
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": "pong",
                    "usage": { "prompt_tokens": 3, "completion_tokens": 1 }
                }
            });
            let (url, server) = spawn_fake_server(&serde_json::to_vec(&resp).unwrap());
            let client = HttpClient::new(&url, Duration::from_secs(2)).unwrap();
            let out = client.complete(&sample_request()).unwrap();
            assert_eq!(out.content, "pong");
            assert_eq!(out.usage.prompt_tokens, 3);
            let req = server.join().unwrap();
            let req_str = String::from_utf8_lossy(&req);
            assert!(req_str.contains("POST /rpc HTTP/1.1"));
            assert!(req_str.contains(r#""method":"complete""#));
        }

        #[test]
        fn http_client_attaches_bearer() {
            let resp = serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "result": { "content": "ok", "usage": {} }
            });
            let (url, server) = spawn_fake_server(&serde_json::to_vec(&resp).unwrap());
            let client =
                HttpClient::with_bearer(&url, Duration::from_secs(2), Some("s3cret".into()))
                    .unwrap();
            let _ = client.complete(&sample_request()).unwrap();
            let req = String::from_utf8(server.join().unwrap()).unwrap();
            assert!(
                req.contains("Authorization: Bearer s3cret\r\n"),
                "req: {req}"
            );
        }

        #[test]
        fn http_client_surfaces_rpc_error() {
            let resp = serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": -32002, "message": "model unavailable" }
            });
            let (url, _server) = spawn_fake_server(&serde_json::to_vec(&resp).unwrap());
            let client = HttpClient::new(&url, Duration::from_secs(2)).unwrap();
            let err = client.complete(&sample_request()).unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("-32002"), "msg: {msg}");
            assert!(msg.contains("model unavailable"));
        }

        #[test]
        fn http_client_surfaces_non_2xx() {
            let (url, _server) = spawn_fake_server_ext(503, "Service Unavailable", b"{}");
            let client = HttpClient::new(&url, Duration::from_secs(2)).unwrap();
            let err = client.complete(&sample_request()).unwrap_err();
            assert!(format!("{err}").contains("HTTP 503"));
        }

        #[test]
        fn http_client_rejects_https_url() {
            let err =
                HttpClient::new("https://example.com/v1", Duration::from_secs(1)).unwrap_err();
            assert!(format!("{err}").contains("must start with `http://`"));
        }

        #[test]
        fn http_client_rejects_missing_host() {
            let err = HttpClient::new("http://", Duration::from_secs(1)).unwrap_err();
            assert!(format!("{err}").contains("no host"));
        }

        #[test]
        fn http_client_default_port_80() {
            // Can't actually connect to :80 in CI; just validate parse.
            let c = HttpClient::new("http://example.com/v1", Duration::from_secs(1)).unwrap();
            assert_eq!(c.endpoint.port, 80);
            assert_eq!(c.endpoint.host, "example.com");
            assert_eq!(c.endpoint.path, "/v1");
        }

        #[test]
        fn http_client_parses_port() {
            let c =
                HttpClient::new("http://127.0.0.1:11434/v1/chat", Duration::from_secs(1)).unwrap();
            assert_eq!(c.endpoint.port, 11434);
        }
    }
}
