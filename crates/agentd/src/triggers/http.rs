//! Minimal HTTP/1.1 server (RFC §13).
//!
//! Hand-rolled, zero external HTTP crate. The request surface is
//! tiny (parse a request line, read a Content-Length body, write a
//! structured response) and keeping it in-tree beats pulling in
//! hyper/axum and an async runtime.
//!
//! Threading: one accept loop, one thread per accepted connection.
//! Max body 1 MiB, max headers 16 KiB — hardened against
//! head-of-line attacks without a full framework.
//!
//! Routing: the `http_routes` block of the workflow becomes a
//! `(METHOD, PATH)` table. A request matches exactly one entry;
//! everything else returns 404 / 405.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::engine::{Engine, ExecutionOutcome, RunOptions, TriggerMeta};
use crate::error::{Error, Result};
use crate::workflow::WorkflowDoc;

/// Max body size accepted on an HTTP request. Declines larger
/// requests with 413 Payload Too Large.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Max size of the request-line + headers block before the server
/// gives up and returns 431.
const MAX_HEADERS_BYTES: usize = 16 * 1024;

/// One configured HTTP listener.
pub struct HttpServer {
    bind: SocketAddr,
    workflow: Arc<WorkflowDoc>,
    engine: Arc<Engine>,
    options: RunOptions,
    drain_timeout: Duration,
}

impl HttpServer {
    pub fn new(
        bind: SocketAddr,
        workflow: Arc<WorkflowDoc>,
        engine: Arc<Engine>,
        options: RunOptions,
    ) -> Self {
        Self {
            bind,
            workflow,
            engine,
            options,
            drain_timeout: Duration::from_secs(30),
        }
    }

    /// Override the graceful-drain budget (default 30 s). After the
    /// shutdown flag flips, the server stops accepting and waits up
    /// to this long for in-flight requests to complete.
    pub fn with_drain_timeout(mut self, d: Duration) -> Self {
        self.drain_timeout = d;
        self
    }

    /// Spawn the listener on its own thread. Returns a
    /// [`ServerHandle`] for orderly shutdown.
    pub fn spawn(self) -> Result<ServerHandle> {
        let listener = TcpListener::bind(self.bind).map_err(|e| Error::Workflow {
            workflow: self.workflow.name.clone(),
            reason: format!("bind {}: {e}", self.bind),
        })?;
        let local_addr = listener.local_addr().unwrap_or(self.bind);

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let shutdown_flag = shutdown.clone();
        let in_flight_accept = in_flight.clone();
        let workflow = self.workflow.clone();
        let engine = self.engine.clone();
        let options = self.options.clone();

        let handle = thread::spawn(move || {
            listener.set_nonblocking(true).ok();
            while !shutdown_flag.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

                        let wf = workflow.clone();
                        let eng = engine.clone();
                        let opts = options.clone();
                        let guard = InFlightGuard::acquire(in_flight_accept.clone());
                        thread::spawn(move || {
                            let _g = guard; // drop decrements counter
                            let _ = handle_connection(stream, &wf, &eng, &opts);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => {
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });

        Ok(ServerHandle {
            local_addr,
            shutdown,
            in_flight,
            drain_timeout: self.drain_timeout,
            thread: Some(handle),
        })
    }
}

/// RAII counter decrement for in-flight request tracking.
struct InFlightGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl InFlightGuard {
    fn acquire(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Handle on a running [`HttpServer`]. Dropping it triggers shutdown
/// and joins the accept thread.
pub struct ServerHandle {
    local_addr: SocketAddr,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    drain_timeout: Duration,
    thread: Option<thread::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Number of connections still being handled.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Request shutdown and return; does not wait. The accept loop
    /// sees the flag on its next poll and exits.
    pub fn request_shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Request shutdown, then block for up to `drain_timeout` for
    /// in-flight connections to complete. Returns `true` if the
    /// drain finished cleanly, `false` on deadline.
    pub fn shutdown_and_drain(mut self) -> bool {
        self.request_shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let deadline = std::time::Instant::now() + self.drain_timeout;
        while self.in_flight() > 0 {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
        true
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

/// Drive one accepted connection: parse a single request, route it,
/// run the workflow, write the response, close.
fn handle_connection<S: std::io::Read + Write>(
    stream: S,
    workflow: &WorkflowDoc,
    engine: &Engine,
    options: &RunOptions,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let parse_result = parse_request(&mut reader);
    let stream = reader.get_mut();

    let request = match parse_result {
        Ok(r) => r,
        Err(e) if e.silent_close => {
            // Peer went away before sending a request. No bytes to
            // reply to — just close quietly.
            return Ok(());
        }
        Err(e) => {
            write_response(stream, e.status, &e.body)?;
            return Ok(());
        }
    };

    // Route.
    let route = workflow
        .http_routes
        .iter()
        .find(|r| r.method.eq_ignore_ascii_case(&request.method) && r.path == request.path);
    let Some(route) = route else {
        // Distinguish "wrong method on a known path" (405) from
        // "unknown path entirely" (404) so clients see the right hint.
        let path_known = workflow.http_routes.iter().any(|r| r.path == request.path);
        let status = if path_known {
            Status::new(405, "Method Not Allowed")
        } else {
            Status::new(404, "Not Found")
        };
        write_response(
            stream,
            status,
            &json!({ "error": status.reason, "path": request.path }),
        )?;
        return Ok(());
    };

    // Parse body as JSON (or accept an empty body as `null`).
    let input = if request.body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&request.body) {
            Ok(v) => v,
            Err(e) => {
                write_response(
                    stream,
                    Status::new(400, "Bad Request"),
                    &json!({ "error": "invalid JSON body", "detail": e.to_string() }),
                )?;
                return Ok(());
            }
        }
    };

    // Run.
    match engine.run(
        workflow,
        &route.start_node,
        TriggerMeta::http(input),
        options.clone(),
    ) {
        Ok(outcome) => {
            let status = match &outcome {
                ExecutionOutcome::Completed { .. } => Status::new(200, "OK"),
                ExecutionOutcome::Failed { .. } => Status::new(422, "Unprocessable Entity"),
                ExecutionOutcome::TimedOut { .. } => Status::new(504, "Gateway Timeout"),
            };
            write_response(stream, status, &outcome)?;
            Ok(())
        }
        Err(e) => {
            write_response(
                stream,
                Status::new(500, "Internal Server Error"),
                &json!({ "error": format!("{e}") }),
            )?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct Status {
    code: u16,
    reason: &'static str,
}

impl Status {
    const fn new(code: u16, reason: &'static str) -> Self {
        Self { code, reason }
    }
}

struct ParseError {
    status: Status,
    body: Value,
    /// When true, the caller should close the connection without
    /// writing a response. Used for a clean EOF before any bytes
    /// arrive — there's nothing to reply to.
    silent_close: bool,
}

fn parse_request<R: BufRead>(reader: &mut R) -> std::result::Result<Request, ParseError> {
    // Request line.
    let mut line = String::new();
    let read = reader.read_line(&mut line);
    let n = match read {
        Ok(n) => n,
        Err(e) => {
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                return Err(silent_close());
            }
            return Err(bad(400, "request line read failed"));
        }
    };
    if n == 0 {
        return Err(silent_close());
    }
    let mut parts = line.trim_end().split(' ');
    let method = parts
        .next()
        .ok_or_else(|| bad(400, "missing method"))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| bad(400, "missing path"))?
        .to_string();
    // Strip query string if present — Phase 6 routes on path only.
    let path = path.split('?').next().unwrap_or(&path).to_string();
    let _ = parts.next(); // ignore HTTP version

    // Headers.
    let mut headers_bytes = n;
    let mut content_length = 0usize;
    let mut headers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    loop {
        let mut header_line = String::new();
        let read = reader
            .read_line(&mut header_line)
            .map_err(|_| bad(400, "header read failed"))?;
        if read == 0 {
            return Err(bad(400, "unexpected EOF in headers"));
        }
        headers_bytes += read;
        if headers_bytes > MAX_HEADERS_BYTES {
            return Err(ParseError {
                status: Status::new(431, "Request Header Fields Too Large"),
                body: json!({ "error": "headers too large" }),
                silent_close: false,
            });
        }
        let trimmed = header_line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let value = v.trim().to_string();
            if key == "content-length" {
                content_length = value
                    .parse::<usize>()
                    .map_err(|_| bad(400, "invalid Content-Length"))?;
            }
            headers.insert(key, value);
        }
    }

    if content_length > MAX_BODY_BYTES {
        return Err(ParseError {
            status: Status::new(413, "Payload Too Large"),
            body: json!({ "error": "body exceeds server cap", "cap_bytes": MAX_BODY_BYTES }),
            silent_close: false,
        });
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|_| bad(400, "truncated body"))?;
    }
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn bad(code: u16, msg: &'static str) -> ParseError {
    ParseError {
        status: Status::new(code, msg),
        body: json!({ "error": msg }),
        silent_close: false,
    }
}

/// Signal that the peer went away cleanly before sending a request.
/// The caller should close without writing a response.
fn silent_close() -> ParseError {
    ParseError {
        status: Status::new(0, ""),
        body: Value::Null,
        silent_close: true,
    }
}

// ---------------------------------------------------------------------------
// Response writing
// ---------------------------------------------------------------------------

fn write_response<S: Write, B: serde::Serialize>(
    stream: &mut S,
    status: Status,
    body: &B,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(body)?;
    let header = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        status.code,
        status.reason,
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{HandlerRegistry, StubHandler};
    use crate::tools::{policy::allow_all, register_default_tools};
    use crate::workflow::model::{Edge, HttpRoute, Node, NodeKind, StartNode, StartSource};
    use std::io::{BufReader, Read, Write};
    use std::net::TcpStream;

    fn minimal_wf() -> WorkflowDoc {
        WorkflowDoc {
            name: "t".into(),
            start_nodes: vec![StartNode {
                name: "on_http".into(),
                source: StartSource::Http,
                entry_node: Some("a".into()),
            }],
            http_routes: vec![HttpRoute {
                method: "POST".into(),
                path: "/run".into(),
                start_node: "on_http".into(),
                input_schema: None,
            }],
            nodes: vec![
                Node {
                    id: "a".into(),
                    kind: NodeKind::Merge,
                },
                Node {
                    id: "b".into(),
                    kind: NodeKind::Terminate,
                },
            ],
            edges: vec![Edge {
                from: "a".into(),
                to: "b".into(),
                when: None,
            }],
            ..Default::default()
        }
    }

    fn start_server(wf: WorkflowDoc) -> ServerHandle {
        let mut registry = HandlerRegistry::with_builtin_controls();
        register_default_tools(&mut registry, allow_all());
        registry.set_fallback(Box::new(StubHandler));
        let engine = Arc::new(Engine::new(registry));
        let server = HttpServer::new(
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(wf),
            engine,
            RunOptions::default(),
        );
        server.spawn().expect("spawn http server")
    }

    fn send(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, String) {
        send_with_headers(addr, method, path, &std::collections::HashMap::new(), body)
    }

    fn send_with_headers(
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &std::collections::HashMap<String, String>,
        body: &[u8],
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n",
            body.len()
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();

        let mut buf = String::new();
        let mut reader = BufReader::new(stream);
        reader.read_to_string(&mut buf).unwrap();
        let (status_line, rest) = buf.split_once("\r\n").unwrap_or((&buf, ""));
        let code = status_line
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        // Body starts after the empty line separating headers + body.
        let body = rest.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        (code, body.to_string())
    }

    #[test]
    fn routes_to_declared_path() {
        let handle = start_server(minimal_wf());
        let (code, body) = send(handle.local_addr(), "POST", "/run", b"{}");
        assert_eq!(code, 200);
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], "completed");
        handle.shutdown_and_drain();
    }

    #[test]
    fn unknown_path_returns_404() {
        let handle = start_server(minimal_wf());
        let (code, _body) = send(handle.local_addr(), "POST", "/nope", b"{}");
        assert_eq!(code, 404);
        handle.shutdown_and_drain();
    }

    #[test]
    fn wrong_method_on_known_path_returns_405() {
        let handle = start_server(minimal_wf());
        let (code, _body) = send(handle.local_addr(), "GET", "/run", b"");
        assert_eq!(code, 405);
        handle.shutdown_and_drain();
    }

    #[test]
    fn invalid_json_returns_400() {
        let handle = start_server(minimal_wf());
        let (code, body) = send(handle.local_addr(), "POST", "/run", b"not json");
        assert_eq!(code, 400);
        assert!(body.contains("invalid JSON"));
        handle.shutdown_and_drain();
    }

    #[test]
    fn empty_body_treated_as_null_input() {
        // A workflow that reads trigger and terminates — verifies the
        // empty body doesn't break the pipeline.
        let mut wf = minimal_wf();
        wf.nodes[0] = Node {
            id: "a".into(),
            kind: NodeKind::Condition {
                expr: "trigger.kind".into(),
            },
        };
        wf.edges = vec![
            Edge {
                from: "a".into(),
                to: "b".into(),
                when: Some("true".into()),
            },
            Edge {
                from: "a".into(),
                to: "b".into(),
                when: Some("false".into()),
            },
        ];
        let handle = start_server(wf);
        let (code, _body) = send(handle.local_addr(), "POST", "/run", b"");
        assert_eq!(code, 200);
        handle.shutdown_and_drain();
    }

    #[test]
    fn failed_workflow_maps_to_422() {
        let mut wf = minimal_wf();
        wf.nodes[0] = Node {
            id: "a".into(),
            kind: NodeKind::Fail {
                reason: Some("boom".into()),
            },
        };
        wf.edges.clear();
        let handle = start_server(wf);
        let (code, body) = send(handle.local_addr(), "POST", "/run", b"{}");
        assert_eq!(code, 422);
        assert!(body.contains("\"status\":\"failed\""));
        assert!(body.contains("boom"));
        handle.shutdown_and_drain();
    }

    #[test]
    fn oversized_body_returns_413() {
        // Claim 32 MiB without actually writing it; the server should
        // 413 on the Content-Length check before reading the body.
        let handle = start_server(minimal_wf());
        let addr = handle.local_addr();
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(
                b"POST /run HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Content-Length: 33554432\r\n\
                 Connection: close\r\n\
                 \r\n",
            )
            .unwrap();
        stream.flush().unwrap();

        let mut buf = String::new();
        let mut reader = BufReader::new(stream);
        reader.read_to_string(&mut buf).unwrap();
        assert!(buf.contains("413"));
        handle.shutdown_and_drain();
    }
}
