//! Outbound HTTP tool (RFC §10.2).
//!
//! Plain HTTP/1.1, hand-rolled — zero external HTTP crate (same
//! posture as the server in `triggers::http`). TLS is deliberately
//! out of scope for R2; HTTPS URLs fail loudly with a pointer at
//! the future `tools-http-tls` feature.
//!
//! One handler, two safety rails:
//!
//! 1. **Policy gate.** `Policy::check_http_request` vets the
//!    method + URL against the operator's allowlist before a
//!    socket opens.
//! 2. **Size caps.** Request body ≤ 1 MiB, response body ≤ 1 MiB.
//!    A malicious endpoint cannot OOM the runtime.
//!
//! Request shape comes from the existing [`NodeKind::HttpRequest`]
//! variant. Response shape:
//!
//! ```json
//! {
//!   "status": 200,
//!   "headers": { "content-type": "application/json", … },
//!   "body": "…",
//!   "bytes": 123
//! }
//! ```

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::engine::{ExecutionContext, HandlerRegistry, NodeHandler, NodeOutcome};
use crate::error::{Error, Result};
use crate::tools::policy::{Decision, PolicyRef};
use crate::tools::{resolve_string, resolve_value};
use crate::workflow::{Node, NodeKind};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;

pub(crate) fn register(registry: &mut HandlerRegistry, policy: PolicyRef) {
    registry.register("http_request", Box::new(HttpRequestHandler { policy }));
}

pub struct HttpRequestHandler {
    policy: PolicyRef,
}

impl NodeHandler for HttpRequestHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::HttpRequest {
            method,
            url_from,
            body_from,
        } = &node.kind
        else {
            return Err(kind_mismatch(node, "http_request"));
        };

        let url = resolve_string("http_request", ctx, url_from)?;
        let method_upper = method.to_ascii_uppercase();

        // Policy check before we open a socket.
        match self.policy.check_http_request(&method_upper, &url) {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                return Err(Error::Policy(format!(
                    "http_request `{method_upper} {url}`: {reason}"
                )));
            }
        }

        // Dry-run: never touch the network.
        if ctx.dry_run {
            return Ok(NodeOutcome::Continue {
                value: json!({
                    "method": method_upper,
                    "url": url,
                    "dry_run": true,
                }),
                branch: None,
            });
        }

        // Optional body.
        let body_bytes = match body_from {
            Some(path) => {
                let body_val = resolve_value("http_request", ctx, path)?;
                let body_str = match body_val {
                    Value::String(s) => s,
                    other => serde_json::to_string(&other).map_err(Error::Json)?,
                };
                let bytes = body_str.into_bytes();
                if bytes.len() > MAX_REQUEST_BODY_BYTES {
                    return Err(Error::Tool {
                        tool: "http_request".into(),
                        reason: format!(
                            "request body {} bytes exceeds cap ({MAX_REQUEST_BODY_BYTES})",
                            bytes.len()
                        ),
                    });
                }
                Some(bytes)
            }
            None => None,
        };

        let parsed = parse_url(&url)?;
        let outbound_traceparent = ctx.outbound_traceparent();
        let response = perform_request(
            &method_upper,
            &parsed,
            body_bytes.as_deref(),
            outbound_traceparent.as_deref(),
        )?;

        // Branch label on non-2xx so workflow authors can route
        // "HTTP error" edges cleanly via `when = "error"`.
        let branch = if (200..300).contains(&response.status) {
            None
        } else {
            Some("error".to_string())
        };

        Ok(NodeOutcome::Continue {
            value: json!({
                "status": response.status,
                "headers": response.headers,
                "body": response.body_string,
                "bytes": response.body_bytes_len,
            }),
            branch,
        })
    }
}

// ---------------------------------------------------------------------------
// URL parsing
// ---------------------------------------------------------------------------

struct ParsedUrl {
    host: String,
    port: u16,
    path_and_query: String,
}

fn parse_url(url: &str) -> Result<ParsedUrl> {
    if let Some(rest) = url.strip_prefix("https://") {
        let _ = rest;
        return Err(Error::Tool {
            tool: "http_request".into(),
            reason: format!(
                "https scheme not supported in this build \
                 (rebuild with `--features tools-http-tls` when the TLS \
                 client lands; URL: {url})"
            ),
        });
    }
    let rest = url.strip_prefix("http://").ok_or_else(|| Error::Tool {
        tool: "http_request".into(),
        reason: format!("URL must start with http:// (got `{url}`)"),
    })?;

    let (authority, path_and_query) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let port_str = &authority[i + 1..];
            let port = port_str.parse::<u16>().map_err(|_| Error::Tool {
                tool: "http_request".into(),
                reason: format!("invalid port in URL `{url}`"),
            })?;
            (authority[..i].to_string(), port)
        }
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(Error::Tool {
            tool: "http_request".into(),
            reason: format!("URL `{url}` has no host"),
        });
    }
    Ok(ParsedUrl {
        host,
        port,
        path_and_query: path_and_query.to_string(),
    })
}

// ---------------------------------------------------------------------------
// HTTP/1.1 client
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    headers: HashMap<String, String>,
    body_string: String,
    body_bytes_len: usize,
}

fn perform_request(
    method: &str,
    url: &ParsedUrl,
    body: Option<&[u8]>,
    traceparent: Option<&str>,
) -> Result<HttpResponse> {
    let sock_addr = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| Error::Tool {
            tool: "http_request".into(),
            reason: format!("resolve {}:{}: {e}", url.host, url.port),
        })?
        .next()
        .ok_or_else(|| Error::Tool {
            tool: "http_request".into(),
            reason: format!("no address for {}:{}", url.host, url.port),
        })?;

    let mut stream =
        TcpStream::connect_timeout(&sock_addr, DEFAULT_TIMEOUT).map_err(|e| Error::Tool {
            tool: "http_request".into(),
            reason: format!("connect {}:{}: {e}", url.host, url.port),
        })?;
    stream
        .set_read_timeout(Some(DEFAULT_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(DEFAULT_TIMEOUT)))
        .map_err(|e| Error::Tool {
            tool: "http_request".into(),
            reason: format!("set timeouts: {e}"),
        })?;

    // Write request.
    let body_len = body.map(<[u8]>::len).unwrap_or(0);
    let mut request = String::new();
    request.push_str(&format!("{method} {} HTTP/1.1\r\n", url.path_and_query));
    request.push_str(&format!("Host: {}:{}\r\n", url.host, url.port));
    request.push_str("Connection: close\r\n");
    request.push_str(&format!("Content-Length: {body_len}\r\n"));
    if body_len > 0 {
        request.push_str("Content-Type: application/json\r\n");
    }
    // W3C trace-context propagation. Keeps the
    // inbound trace-id + flags and inserts this run's fresh span id
    // as the parent so downstream services see the agent as their
    // direct parent. Only emitted when a `traceparent` arrived on
    // the inbound side — cron / fs_watch / manual runs don't
    // originate traces.
    if let Some(tp) = traceparent {
        request.push_str(&format!("traceparent: {tp}\r\n"));
    }
    request.push_str("\r\n");

    stream
        .write_all(request.as_bytes())
        .and_then(|()| match body {
            Some(b) => stream.write_all(b),
            None => Ok(()),
        })
        .and_then(|()| stream.flush())
        .map_err(|e| Error::Tool {
            tool: "http_request".into(),
            reason: format!("write: {e}"),
        })?;

    // Read response.
    let mut reader = BufReader::new(&stream);
    let status = parse_status_line(&mut reader)?;
    let (headers, content_length) = parse_response_headers(&mut reader)?;

    let take_len = content_length.min(MAX_RESPONSE_BODY_BYTES);
    let mut body = Vec::with_capacity(take_len.min(64 * 1024));
    reader
        .by_ref()
        .take(take_len as u64)
        .read_to_end(&mut body)
        .map_err(|e| Error::Tool {
            tool: "http_request".into(),
            reason: format!("read body: {e}"),
        })?;
    let truncated = content_length > MAX_RESPONSE_BODY_BYTES;
    let body_string = String::from_utf8_lossy(&body).into_owned();
    let body_bytes_len = body.len();
    if truncated {
        tracing::warn!(
            target: "agentd::audit",
            event = "http_response.truncated",
            claimed_bytes = content_length,
            cap_bytes = MAX_RESPONSE_BODY_BYTES,
        );
    }
    Ok(HttpResponse {
        status,
        headers,
        body_string,
        body_bytes_len,
    })
}

fn parse_status_line<R: BufRead>(reader: &mut R) -> Result<u16> {
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| Error::Tool {
        tool: "http_request".into(),
        reason: format!("read status line: {e}"),
    })?;
    let mut parts = line.trim_end().split(' ');
    let _http = parts.next();
    let code_str = parts.next().ok_or_else(|| Error::Tool {
        tool: "http_request".into(),
        reason: format!("malformed status line: `{}`", line.trim()),
    })?;
    code_str.parse::<u16>().map_err(|_| Error::Tool {
        tool: "http_request".into(),
        reason: format!("invalid status code `{code_str}`"),
    })
}

fn parse_response_headers<R: BufRead>(reader: &mut R) -> Result<(HashMap<String, String>, usize)> {
    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| Error::Tool {
            tool: "http_request".into(),
            reason: format!("read header: {e}"),
        })?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let value = v.trim().to_string();
            if key == "content-length" {
                content_length = value.parse::<usize>().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }
    Ok((headers, content_length))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn kind_mismatch(node: &Node, expected: &str) -> Error {
    Error::Tool {
        tool: expected.into(),
        reason: format!(
            "handler for `{expected}` received node `{}` of kind `{}`",
            node.id,
            node.kind.name()
        ),
    }
}

// Suppress dead-code lints when the compiled feature set doesn't
// reach these helpers from anywhere else in the module.
#[allow(dead_code)]
fn _keep_path_in_scope(_: &Path) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{RunOptions, TriggerMeta};
    use crate::tools::policy::{Decision, Policy, allow_all};
    use crate::workflow::model::Node;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;
    // Read / Write traits come in via `super::*` (the outer module
    // already imports them for the client body).

    fn ctx(input: Value) -> ExecutionContext {
        ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(input),
            &RunOptions::default(),
        )
    }

    fn node(id: &str, method: &str, url_path: &str, body_from: Option<&str>) -> Node {
        Node {
            id: id.into(),
            retry: None,
            kind: NodeKind::HttpRequest {
                method: method.into(),
                url_from: url_path.into(),
                body_from: body_from.map(Into::into),
            },
        }
    }

    fn spawn_fake_http(status: u16, body: &'static [u8]) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url_prefix = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read enough of the request to finish headers (simple:
            // read until \r\n\r\n or until we've got a reasonable chunk).
            let mut buf = vec![0u8; 4096];
            let mut seen = Vec::new();
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    // If Content-Length present, read that many body bytes.
                    let headers = String::from_utf8_lossy(&seen);
                    let content_len = headers
                        .lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let header_end = seen.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let body_already = seen.len() - header_end;
                    if body_already < content_len {
                        let mut rest = vec![0u8; content_len - body_already];
                        stream.read_exact(&mut rest).unwrap();
                        seen.extend_from_slice(&rest);
                    }
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Length: {len}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                len = body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
            seen
        });
        (url_prefix, handle)
    }

    #[test]
    fn traceparent_propagates_to_outbound_request() {
        // Inbound request carried a W3C traceparent (simulated by
        // constructing the trigger with `http_with_trace`). Outbound
        // `http_request` tool must inject a `traceparent` header
        // whose trace-id + flags match the inbound, with a fresh
        // 16-hex parent-id representing this run.
        use crate::observability::TraceParent;

        let (prefix, server) = spawn_fake_http(200, b"ok");
        let inbound = TraceParent {
            version: "00".into(),
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            parent_id: "00f067aa0ba902b7".into(),
            trace_flags: "01".into(),
        };
        let mut c = ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::http_with_trace(
                json!({ "url": format!("{prefix}/api") }),
                inbound.clone(),
            ),
            &RunOptions::default(),
        );
        let h = HttpRequestHandler {
            policy: allow_all(),
        };
        h.handle(&node("r", "GET", "trigger.url", None), &mut c)
            .unwrap();

        let raw_request = server.join().unwrap();
        let text = String::from_utf8_lossy(&raw_request);
        let tp_line = text
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("traceparent:"))
            .expect("outbound request must carry a traceparent header");
        let value = tp_line.split_once(':').unwrap().1.trim();
        let parts: Vec<&str> = value.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "00");
        assert_eq!(parts[1], inbound.trace_id);
        // parent-id must be a fresh 16-hex value, distinct from the
        // inbound parent-id (the agent is now the parent downstream
        // sees).
        assert_eq!(parts[2].len(), 16);
        assert!(parts[2].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(parts[2], inbound.parent_id);
        assert_eq!(parts[3], inbound.trace_flags);
    }

    #[test]
    fn no_traceparent_when_inbound_missing() {
        // Manual / cron-triggered runs don't originate traces; the
        // outbound request must NOT carry a traceparent header.
        let (prefix, server) = spawn_fake_http(200, b"ok");
        let mut c = ctx(json!({ "url": format!("{prefix}/") }));
        let h = HttpRequestHandler {
            policy: allow_all(),
        };
        h.handle(&node("r", "GET", "trigger.url", None), &mut c)
            .unwrap();
        let raw_request = server.join().unwrap();
        let text = String::from_utf8_lossy(&raw_request);
        assert!(
            !text.to_ascii_lowercase().contains("traceparent:"),
            "no inbound trace → no outbound traceparent (got: {text})"
        );
    }

    #[test]
    fn get_returns_status_and_body() {
        let (prefix, server) = spawn_fake_http(200, b"hello");
        let mut c = ctx(json!({ "url": format!("{prefix}/") }));
        let h = HttpRequestHandler {
            policy: allow_all(),
        };
        let out = h
            .handle(&node("r", "GET", "trigger.url", None), &mut c)
            .unwrap();
        match out {
            NodeOutcome::Continue { value, branch } => {
                assert_eq!(value["status"], 200);
                assert_eq!(value["body"], "hello");
                assert_eq!(value["bytes"], 5);
                assert!(branch.is_none());
            }
            _ => panic!(),
        }
        let _ = server.join();
    }

    #[test]
    fn non_2xx_sets_error_branch() {
        let (prefix, server) = spawn_fake_http(503, b"down");
        let mut c = ctx(json!({ "url": format!("{prefix}/") }));
        let h = HttpRequestHandler {
            policy: allow_all(),
        };
        let out = h
            .handle(&node("r", "GET", "trigger.url", None), &mut c)
            .unwrap();
        match out {
            NodeOutcome::Continue { branch, .. } => {
                assert_eq!(branch.as_deref(), Some("error"));
            }
            _ => panic!(),
        }
        let _ = server.join();
    }

    #[test]
    fn post_body_is_sent() {
        let (prefix, server) = spawn_fake_http(200, b"ok");
        let mut c = ctx(json!({
            "url": format!("{prefix}/"),
            "payload": { "n": 42 },
        }));
        let h = HttpRequestHandler {
            policy: allow_all(),
        };
        h.handle(
            &node("r", "POST", "trigger.url", Some("trigger.payload")),
            &mut c,
        )
        .unwrap();
        let seen = server.join().unwrap();
        let seen_str = String::from_utf8_lossy(&seen);
        assert!(seen_str.contains("POST "), "seen: {seen_str}");
        assert!(seen_str.contains(r#"{"n":42}"#), "seen: {seen_str}");
    }

    #[test]
    fn dry_run_does_not_hit_network() {
        // No server bound — if the handler tried to connect, this
        // would fail rather than dry-run.
        let mut c = ctx(json!({ "url": "http://127.0.0.1:1/" }));
        c.dry_run = true;
        let h = HttpRequestHandler {
            policy: allow_all(),
        };
        let out = h
            .handle(&node("r", "GET", "trigger.url", None), &mut c)
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["dry_run"], true);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn https_is_rejected_with_clear_error() {
        let mut c = ctx(json!({ "url": "https://example.com/" }));
        let h = HttpRequestHandler {
            policy: allow_all(),
        };
        let err = h
            .handle(&node("r", "GET", "trigger.url", None), &mut c)
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("https"), "msg: {msg}");
        assert!(msg.contains("tools-http-tls"), "msg: {msg}");
    }

    #[test]
    fn policy_deny_blocks_connection() {
        struct DenyAll;
        impl Policy for DenyAll {
            fn check_http_request(&self, _m: &str, _u: &str) -> Decision {
                Decision::Deny("denied by test".into())
            }
        }
        let mut c = ctx(json!({ "url": "http://127.0.0.1:1/" }));
        let h = HttpRequestHandler {
            policy: Arc::new(DenyAll),
        };
        let err = h
            .handle(&node("r", "GET", "trigger.url", None), &mut c)
            .unwrap_err();
        assert!(format!("{err}").contains("denied by test"));
    }

    #[test]
    fn parse_url_variants() {
        let u = parse_url("http://example.com/path?x=1").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 80);
        assert_eq!(u.path_and_query, "/path?x=1");

        let u = parse_url("http://host:8080").unwrap();
        assert_eq!(u.port, 8080);
        assert_eq!(u.path_and_query, "/");

        assert!(parse_url("ftp://x.com/").is_err());
        assert!(parse_url("https://x.com/").is_err());
        assert!(parse_url("http://:9/").is_err());
        assert!(parse_url("http://host:not_a_port/").is_err());
    }
}
