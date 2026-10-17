// SPDX-License-Identifier: AGPL-3.0-only
//! Transport hardening: the places where a stream, or the body around an
//! answer, not the request itself, is the thing that has to be right.
//!
//! * The server→client notification stream (the `GET` dial) is a DIAL like any
//!   other, and a signed server rejects an unsigned one — silently, from the
//!   daemon's point of view, because nothing fails except the waking up.
//! * A server may interleave a request of its own on the response stream of a
//!   POST and block until it is answered. Frames that are collected and handed
//!   over afterwards arrive after the server gave up.
//! * The raw listener's reader parses a request head before anyone has
//!   authenticated, so its bounds are the only thing between a remote peer and
//!   our memory.
//! * A server's JSON-RPC answer is the request's answer wherever it rides — a
//!   `400` body, an event-stream frame with no id — and the bootstrap messages
//!   are handed to the SDK whole, so a refusal reaches the handshake as the
//!   server's own error. A session is taken only from the `initialize` answer.
//! * A response stream the SDK drops hangs up the HTTP exchange under it: at
//!   2026-07-28 the closed stream is the request's cancellation, and a reader
//!   that only noticed on its next frame would hold the server's side open.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mcp::http::{HttpTransport, McpEndpoint, RequestSigner};
use mcp::http_server::{
    HttpAcceptor, RawHandler, RawRequest, RawResponse, bind_tcp, spawn_accept_raw,
};
use mcp::inbound::{Answer, Handler as InboundHandler, Inbound};
use mcp::rmcp_client::RmcpBuilder;
use serde_json::{Value, json};

// ---------------------------------------------------------------- shared bits

/// One request as a stub server saw it: the request line and its headers.
type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

/// Read a request head; returns the request line and lowercased headers.
fn read_head(r: &mut BufReader<TcpStream>) -> Option<(String, Vec<(String, String)>)> {
    let mut start = String::new();
    if r.read_line(&mut start).ok()? == 0 {
        return None;
    }
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Some((start.trim_end().to_string(), headers))
}

/// Consume the declared body — a server that closes on unread bytes resets the
/// connection, which would destroy the response before the client reads it.
fn read_body(r: &mut BufReader<TcpStream>, headers: &[(String, String)]) -> Vec<u8> {
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 && r.read_exact(&mut body).is_err() {
        body.clear();
    }
    body
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

// ----------------------------------------- 1. the notification dial is signed

/// A signer that writes what it signed into the header value, so a test can tell
/// a signature over the GET dial from one copied off some other request.
struct StubSigner;
impl RequestSigner for StubSigner {
    fn sign(
        &self,
        method: &str,
        authority: &str,
        path: &str,
        body: &[u8],
    ) -> Result<Vec<(String, String)>, String> {
        Ok(vec![
            (
                "Signature-Input".to_string(),
                "sig1=(\"@method\" \"@authority\" \"@path\")".to_string(),
            ),
            (
                "Signature".to_string(),
                format!("sig1=:{method} {authority} {path} {}:", body.len()),
            ),
        ])
    }
    fn capabilities(&self) -> Option<String> {
        Some("interaction".to_string())
    }
}

/// A server that records every dial and answers it with an SSE stream.
fn spawn_sse_server(seen: Seen) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some((start, headers)) = read_head(&mut r) else {
                    return;
                };
                let _ = read_body(&mut r, &headers);
                seen.lock().unwrap().push((start, headers));
                let _ = w.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                );
                let _ = w.write_all(b": open\n\n");
                let _ = w.flush();
                // Hold the stream briefly so the client reads the head off a
                // live connection, as it would against a real server.
                std::thread::sleep(Duration::from_millis(200));
            });
        }
    });
    format!("http://{addr}/mcp")
}

#[test]
fn the_notification_stream_dial_carries_the_request_signature() {
    let seen: Seen = Arc::default();
    let ep = spawn_sse_server(Arc::clone(&seen));
    let signer: Arc<dyn RequestSigner> = Arc::new(StubSigner);
    let t = HttpTransport::new(McpEndpoint::parse(&ep).unwrap(), vec![])
        .with_signer(Some(Arc::clone(&signer)));

    t.open_events(Duration::from_secs(5), &[])
        .expect("the stub answers with an event stream");

    let dials = seen.lock().unwrap().clone();
    assert_eq!(dials.len(), 1, "one dial expected: {dials:?}");

    let authority = ep
        .trim_start_matches("http://")
        .trim_end_matches("/mcp")
        .to_string();
    let (line, headers) = &dials[0];
    assert!(line.starts_with("GET "), "{line}");
    assert_eq!(
        header(headers, "signature"),
        Some(format!("sig1=:GET {authority} /mcp 0:").as_str()),
        "the GET dial must be signed AS a GET over its own authority/path: {headers:?}"
    );
    assert!(
        header(headers, "signature-input").is_some(),
        "signature-input missing: {headers:?}"
    );
    assert_eq!(
        header(headers, "aauth-capabilities"),
        Some("interaction"),
        "the capabilities advert rides the dial too: {headers:?}"
    );
}

/// A signer whose credential cannot be produced: no login, a source that does
/// not answer.
struct NoCredential;
impl RequestSigner for NoCredential {
    fn sign(&self, _: &str, _: &str, _: &str, _: &[u8]) -> Result<Vec<(String, String)>, String> {
        Err("mcp:x: no cached credential".to_string())
    }
}

/// A listener that counts every connection made to it, whether or not a
/// request ever arrives on it.
fn spawn_counting() -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let dials = Arc::new(Mutex::new(0usize));
    let counted = Arc::clone(&dials);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            *counted.lock().unwrap() += 1;
            drop(conn);
        }
    });
    (format!("http://{addr}/mcp"), dials)
}

/// A signer that cannot produce its credential fails the request before any
/// dial: a POST and the notification `GET` alike reach no server, and the
/// error carries the signer's cause.
#[test]
fn a_request_whose_credential_cannot_be_produced_is_not_sent() {
    let (ep, dials) = spawn_counting();
    let signer: Arc<dyn RequestSigner> = Arc::new(NoCredential);
    let t = HttpTransport::new(McpEndpoint::parse(&ep).unwrap(), vec![]).with_signer(Some(signer));

    let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let posted = t.send(Some(1), body, Duration::from_secs(2), &[], |_| {});
    match posted {
        Err(mcp::http::HttpError::Credential(cause)) => {
            assert_eq!(cause, "mcp:x: no cached credential")
        }
        other => panic!("a POST with no credential must not be sent: {other:?}"),
    }
    let opened = t.open_events(Duration::from_secs(2), &[]);
    assert!(
        matches!(opened, Err(mcp::http::HttpError::Credential(_))),
        "the notification stream with no credential must not be dialed"
    );
    let err = mcp::http::HttpError::Credential("mcp:x: no cached credential".into());
    assert!(
        err.to_string().contains("not sent") && err.to_string().contains("mcp:x"),
        "{err}"
    );

    // The handshake is refused the same way, naming the cause.
    let refused = RmcpBuilder::new("x", &ep, vec![], Duration::from_secs(2))
        .with_http(Arc::new(
            HttpTransport::new(McpEndpoint::parse(&ep).unwrap(), vec![])
                .with_signer(Some(Arc::new(NoCredential))),
        ))
        .connect();
    let e = refused.err().expect("a handshake with no credential fails");
    assert!(e.to_string().contains("no cached credential"), "{e}");

    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(*dials.lock().unwrap(), 0, "nothing may reach the server");
}

// -------------------------- 2. an interleaved server→client request is answered

/// Answers whatever the server asks, so the test can see the answer arrive.
struct SaysYes;
impl InboundHandler for SaysYes {
    fn handle(&self, _req: Inbound) -> Option<Answer> {
        Some(Answer::Accept(json!({"env": "staging"})))
    }
}

/// The elicitation answer, once the client POSTs it back.
#[derive(Default)]
struct Answered {
    got: Mutex<Option<Value>>,
    wake: Condvar,
}

/// A server that, on `tools/call`, interleaves an `elicitation/create` REQUEST
/// on the POST's own response stream and does not finish the reply until the
/// client answers it — the exchange the spec allows and a buffering transport
/// deadlocks.
fn spawn_elicit_server(state: Arc<Answered>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some((start, headers)) = read_head(&mut r) else {
                    return;
                };
                let body = read_body(&mut r, &headers);
                if !start.starts_with("POST") {
                    let _ = w
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                // A JSON-RPC RESPONSE from the client: the elicitation answer.
                if method.is_empty() && (msg.get("result").is_some() || msg.get("error").is_some())
                {
                    *state.got.lock().unwrap() = Some(msg.clone());
                    state.wake.notify_all();
                    let _ = w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                if msg.get("id").is_none() {
                    let _ = w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let id = msg["id"].clone();
                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "protocolVersion": msg["params"]["protocolVersion"],
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "elicit-mock", "version": "0"}
                        }
                    });
                    let b = serde_json::to_vec(&resp).unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        b.len()
                    );
                    let _ = w.write_all(head.as_bytes());
                    let _ = w.write_all(&b);
                    let _ = w.flush();
                    return;
                }
                if method != "tools/call" {
                    let resp = json!({"jsonrpc": "2.0", "id": id, "result": {}});
                    let b = serde_json::to_vec(&resp).unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        b.len()
                    );
                    let _ = w.write_all(head.as_bytes());
                    let _ = w.write_all(&b);
                    let _ = w.flush();
                    return;
                }

                // The tool call: ask first, answer later.
                let _ = w.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                );
                let ask = json!({
                    "jsonrpc": "2.0", "id": 77, "method": "elicitation/create",
                    "params": {
                        "mode": "form",
                        "message": "which env?",
                        "requestedSchema": {
                            "type": "object",
                            "properties": {"env": {"type": "string"}},
                            "required": ["env"]
                        }
                    }
                });
                let _ = w.write_all(format!("data: {ask}\n\n").as_bytes());
                let _ = w.flush();

                // Block on the answer, exactly as a real elicitation does,
                // keeping the stream alive with SSE comments the way a server
                // waiting on a human does. The bound is what keeps a failing
                // run finite instead of hung.
                let mut got = state.got.lock().unwrap();
                let deadline = Duration::from_secs(5);
                let start = Instant::now();
                while got.is_none() && start.elapsed() < deadline {
                    let (g, _) = state
                        .wake
                        .wait_timeout(got, Duration::from_millis(100))
                        .unwrap();
                    got = g;
                    let _ = w.write_all(b": keepalive\n\n");
                    let _ = w.flush();
                }
                let answered = got.is_some();
                drop(got);

                let text = if answered { "answered" } else { "unanswered" };
                let resp = json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"content": [{"type": "text", "text": text}], "isError": false}
                });
                let _ = w.write_all(format!("data: {resp}\n\n").as_bytes());
                let _ = w.flush();
            });
        }
    });
    format!("http://{addr}/mcp")
}

#[test]
fn a_request_interleaved_on_a_post_stream_is_answered_not_buffered() {
    let state: Arc<Answered> = Arc::default();
    let ep = spawn_elicit_server(Arc::clone(&state));
    let h: Arc<dyn InboundHandler> = Arc::new(SaysYes);
    let client = RmcpBuilder::new("elicit", &ep, vec![], Duration::from_secs(10))
        .with_elicitation(h)
        .connect()
        .expect("connect");

    let out = client
        .call_tool("ask", Some(json!({})), None, Some(Duration::from_secs(10)))
        .expect("tools/call must complete — the server is waiting on our answer");
    assert_eq!(
        out["content"][0]["text"], "answered",
        "the server finished only if the elicitation was answered while its \
         reply was still open (buffered frames arrive after it gives up): {out}"
    );
    // The answer is a JSON-RPC RESPONSE, which the server acks `202` with no
    // body — a transport that waited for a reply to it would fail that send.
    let again = client
        .call_tool("ask", Some(json!({})), None, Some(Duration::from_secs(10)))
        .expect("the session survives having answered");
    assert_eq!(again["content"][0]["text"], "answered", "{again}");

    let got = state
        .got
        .lock()
        .unwrap()
        .clone()
        .expect("an answer was POSTed");
    assert_eq!(
        got["id"], 77,
        "the answer carries the server's request id: {got}"
    );
    assert_eq!(
        got["result"]["content"]["env"], "staging",
        "the host's answer reached the server: {got}"
    );
}

/// Answers like a person: after a while.
struct SaysYesSlowly(Duration);
impl InboundHandler for SaysYesSlowly {
    fn handle(&self, _req: Inbound) -> Option<Answer> {
        std::thread::sleep(self.0);
        Some(Answer::Accept(json!({"env": "staging"})))
    }
}

#[test]
fn an_operator_slower_than_the_connection_timeout_still_completes_the_call() {
    // The connection's timeout is a transport's — how long a socket may stay
    // silent — and a server that waits on a human keeps its stream alive. The
    // human's time is not the transport's: a call carrying an elicitation the
    // operator answers after that timeout must still complete.
    let state: Arc<Answered> = Arc::default();
    let ep = spawn_elicit_server(Arc::clone(&state));
    let timeout = Duration::from_millis(400);
    let h: Arc<dyn InboundHandler> = Arc::new(SaysYesSlowly(timeout * 3));
    let mut c = mcp::client::McpClient::connect("elicit", &ep, vec![], timeout)
        .expect("connect")
        .with_elicitation(h);
    c.initialize().expect("initialize");

    let r = c
        .call_tool("ask", Some(json!({})))
        .expect("the call waits for the operator, not for the connection timeout");
    assert_eq!(r.text(), "answered", "{}", r.text());
}

#[test]
fn a_bound_fires_while_the_operator_is_still_answering() {
    // A caller's deadline counts the operator's time too, and it has to fire
    // ON time: the host answers synchronously, and an answer awaited where the
    // SDK's worker runs would stall the timer until the human had answered —
    // the caller told "timed out" only after the answer went to the server.
    let state: Arc<Answered> = Arc::default();
    let ep = spawn_elicit_server(Arc::clone(&state));
    let h: Arc<dyn InboundHandler> = Arc::new(SaysYesSlowly(Duration::from_secs(3)));
    let mut c = mcp::client::McpClient::connect("elicit", &ep, vec![], Duration::from_secs(10))
        .expect("connect")
        .with_elicitation(h);
    c.initialize().expect("initialize");

    let bound = Duration::from_millis(400);
    let started = Instant::now();
    let r = c.call_tool_with_meta_within("ask", Some(json!({})), json!({}), bound);
    let took = started.elapsed();
    assert!(r.is_err(), "the call outlived its bound");
    assert!(
        took < Duration::from_millis(1500),
        "the bound fired only after the operator answered ({took:?})"
    );
}

// ------------------------------------- 3. the inbound head reader is bounded

/// The smallest handler that answers a request.
struct Trivial;
impl RawHandler for Trivial {
    fn handle(&self, _req: &RawRequest) -> RawResponse {
        RawResponse::json(200, "OK", br#"{"ok":true}"#.to_vec())
    }
}

fn spawn_bounded_server() -> String {
    let listener = bind_tcp("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    spawn_accept_raw(
        listener,
        Arc::new(HttpAcceptor::Plain),
        Arc::new(Trivial),
        Duration::from_secs(5),
    )
    .unwrap();
    addr
}

/// Send raw bytes, read the status code. `0` means no status line came back.
fn status_of(addr: &str, req: &[u8]) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let _ = s.write_all(req);
    let _ = s.flush();
    let mut line = String::new();
    if BufReader::new(s).read_line(&mut line).unwrap_or(0) == 0 {
        return 0;
    }
    line.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

#[test]
fn an_oversized_request_head_is_refused_431() {
    let addr = spawn_bounded_server();

    // One header line longer than the 64 KiB head budget. Sized to be consumed
    // EXACTLY: the reader stops one byte past the budget, and a server that
    // closes with bytes still unread resets the connection, which would destroy
    // the 431 before the client could read it.
    let line = "POST /hook HTTP/1.1\r\n";
    let filler = 64 * 1024 - line.len() - "X-Huge: ".len() + 1;
    let mut req = Vec::from(line);
    req.extend_from_slice(b"X-Huge: ");
    req.extend(std::iter::repeat_n(b'a', filler));
    assert_eq!(status_of(&addr, &req), 431, "an unbounded header line");

    // Too MANY headers, all of them small: the byte cap never trips, so the
    // count cap is the only thing bounding the header Vec. 101 = one past it.
    let mut req = Vec::from(line);
    for i in 0..101 {
        req.extend_from_slice(format!("X-{i}: v\r\n").as_bytes());
    }
    assert_eq!(status_of(&addr, &req), 431, "an unbounded header count");

    // …and an ordinary request is untouched by either bound.
    let call = r#"{"event":"push"}"#;
    let ok = format!(
        "POST /hook HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{call}",
        call.len()
    );
    assert_eq!(status_of(&addr, ok.as_bytes()), 200);
}

// ------------------------------- 4. the server's answer, whatever carries it

use futures::StreamExt;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use std::collections::HashMap;

/// A server that answers each POST with the exact bytes `answer` returns for
/// the JSON-RPC message it carried, and a `GET` with `405`: no push channel.
fn spawn_scripted(answer: impl Fn(&Value) -> Vec<u8> + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let answer = Arc::new(answer);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let answer = Arc::clone(&answer);
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some((start, headers)) = read_head(&mut r) else {
                    return;
                };
                let body = read_body(&mut r, &headers);
                let out = if start.starts_with("POST") {
                    answer(&serde_json::from_slice(&body).unwrap_or(Value::Null))
                } else {
                    b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n".to_vec()
                };
                let _ = w.write_all(&out);
                let _ = w.flush();
            });
        }
    });
    format!("http://{addr}/mcp")
}

/// One raw HTTP response: a status line, extra header lines, a body.
fn raw(status: &str, headers: &[&str], body: &str) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for h in headers {
        out.push_str(h);
        out.push_str("\r\n");
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    out.into_bytes()
}

fn json_body(status: &str, body: &Value) -> Vec<u8> {
    raw(
        status,
        &["Content-Type: application/json"],
        &body.to_string(),
    )
}

/// A JSON-RPC error with the given id, code and message.
fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The `initialize` answer of a 2025-11-25 server, carrying session `s-1`.
fn initialized(msg: &Value) -> Vec<u8> {
    let resp = json!({
        "jsonrpc": "2.0", "id": msg["id"],
        "result": {
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "scripted", "version": "0"}
        }
    });
    raw(
        "200 OK",
        &["Content-Type: application/json", "Mcp-Session-Id: s-1"],
        &resp.to_string(),
    )
}

/// The `server/discover` request the SDK sends first on the discover path.
fn discover() -> ClientJsonRpcMessage {
    serde_json::from_value(
        json!({"jsonrpc": "2.0", "id": 0, "method": "server/discover", "params": {}}),
    )
    .expect("a server/discover request")
}

type Posted =
    Result<StreamableHttpPostResponse, StreamableHttpError<mcp::rmcp_transport::TransportError>>;

/// POST one message through agentd's socket, as the SDK's worker does.
fn post(ep: &str, message: ClientJsonRpcMessage) -> Posted {
    post_via(agentd(ep), ep, message)
}

/// agentd's socket to `ep`, as a connection builds it (without its queue).
fn agentd(ep: &str) -> mcp::rmcp_transport::AgentdHttp {
    let http = Arc::new(HttpTransport::new(McpEndpoint::parse(ep).unwrap(), vec![]));
    mcp::rmcp_transport::AgentdHttp::new(http, Duration::from_secs(5))
}

fn post_via(
    agentd: mcp::rmcp_transport::AgentdHttp,
    ep: &str,
    message: ClientJsonRpcMessage,
) -> Posted {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(agentd.post_message(Arc::from(ep), message, None, None, HashMap::new()))
}

/// The one JSON answer a POST was handed back as, or a panic naming what came
/// back instead.
fn whole(out: Posted) -> Value {
    match out {
        Ok(StreamableHttpPostResponse::Json(m, _)) => serde_json::to_value(m).unwrap(),
        other => panic!("expected one JSON answer, got {other:?}"),
    }
}

#[test]
fn a_discover_answer_comes_back_whole() {
    // The SDK's bootstrap reader looks for a RESPONSE on a stream and passes
    // over an error, so a discover refusal handed back as a stream would never
    // be seen. Handed back whole it is: as the server sent it…
    let ep =
        spawn_scripted(|_| json_body("200 OK", &rpc_error(json!(0), -32601, "Method not found")));
    let got = whole(post(&ep, discover()));
    assert_eq!(got["error"]["code"], -32601, "{got}");
    assert_eq!(got["id"], 0, "{got}");

    // …with a placeholder id put back on the request (the Python SDK's
    // middleware answers `"server-error"`)…
    let ep = spawn_scripted(|_| {
        json_body(
            "400 Bad Request",
            &rpc_error(json!("server-error"), -32601, "Method not found"),
        )
    });
    let got = whole(post(&ep, discover()));
    assert_eq!(got["error"]["code"], -32601, "{got}");
    assert_eq!(got["id"], 0, "re-correlated: {got}");

    // …and a null id left null: the bootstrap reader takes an id-less error
    // as the answer.
    let ep = spawn_scripted(|_| {
        json_body(
            "400 Bad Request",
            &rpc_error(Value::Null, -32000, "Bad Request"),
        )
    });
    let got = whole(post(&ep, discover()));
    assert_eq!(got["error"]["code"], -32000, "{got}");
    assert_eq!(got["id"], Value::Null, "{got}");

    // What the server says before its answer is not handed on: no lifecycle
    // exists yet to receive it.
    let ep = spawn_scripted(|_| {
        let note = json!({"jsonrpc": "2.0", "method": "notifications/message",
                          "params": {"level": "info", "data": "hello"}});
        let answer = rpc_error(json!(0), -32601, "Method not found");
        raw(
            "200 OK",
            &["Content-Type: text/event-stream"],
            &format!("data: {note}\n\ndata: {answer}\n\n"),
        )
    });
    let got = whole(post(&ep, discover()));
    assert_eq!(got["error"]["code"], -32601, "{got}");
}

/// A frame the server could not put the request's id on.
fn id_less_frame() -> Vec<u8> {
    let frame = rpc_error(Value::Null, -32601, "no such method, said the server");
    raw(
        "200 OK",
        &["Content-Type: text/event-stream"],
        &format!("data: {frame}\n\n"),
    )
}

#[test]
fn an_error_frame_that_cannot_carry_the_id_answers_its_post() {
    // A POST's stream carries only its own exchange, so an error on it is its
    // answer whatever the id says — not a notification to be dropped.
    let ep = spawn_scripted(|_| id_less_frame());
    let got = whole(post(&ep, discover()));
    assert_eq!(got["error"]["code"], -32601, "{got}");

    // And on a call, where the SDK routes by id: the frame is put back on the
    // call, which ends with the server's error rather than waiting out its
    // stream for an answer that came.
    let ep = spawn_scripted(|msg| match msg["method"].as_str() {
        Some("initialize") => initialized(msg),
        Some("tools/call") => id_less_frame(),
        _ => raw("202 Accepted", &[], ""),
    });
    let mut c = mcp::client::McpClient::connect("frames", &ep, vec![], Duration::from_secs(5))
        .expect("connect");
    c.initialize().expect("initialize");
    let err = c
        .call_tool("t", Some(json!({})))
        .expect_err("the server refused the call")
        .to_string();
    assert!(err.contains("no such method, said the server"), "{err}");
}

#[test]
fn a_json_rpc_error_in_a_4xx_or_5xx_body_is_the_requests_answer() {
    for (status, code, message) in [
        ("400 Bad Request", -32020, "Mcp-Name does not match"),
        (
            "500 Internal Server Error",
            -32603,
            "the tool's backend is down",
        ),
    ] {
        let ep = spawn_scripted(move |msg| match msg["method"].as_str() {
            Some("initialize") => initialized(msg),
            Some("tools/call") => json_body(status, &rpc_error(Value::Null, code, message)),
            _ => raw("202 Accepted", &[], ""),
        });
        let mut c = mcp::client::McpClient::connect("bodies", &ep, vec![], Duration::from_secs(5))
            .expect("connect");
        c.initialize().expect("initialize");
        let err = c
            .call_tool("t", Some(json!({})))
            .expect_err("the server refused the call")
            .to_string();
        assert!(
            err.contains(message),
            "{status}: the server's own error: {err}"
        );
    }
}

#[test]
fn an_auth_failure_or_a_bodyless_status_stays_a_status() {
    let challenge = rpc_error(Value::Null, -32001, "the server's own words on auth");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        // Whatever an auth failure's body says, it is the status the credential
        // machinery and the operator act on — never an answer.
        ("401", json_body("401 Unauthorized", &challenge)),
        ("401", raw("401 Unauthorized", &[], "")),
        ("403", json_body("403 Forbidden", &challenge)),
        ("404", raw("404 Not Found", &[], "")),
        (
            "503",
            raw(
                "503 Service Unavailable",
                &["Content-Type: text/plain"],
                "down",
            ),
        ),
        // A gateway's own JSON error is not a JSON-RPC answer: no `jsonrpc`,
        // so the status is what the operator sees.
        (
            "503",
            json_body(
                "503 Service Unavailable",
                &json!({"error": {"code": 503, "message": "the server's own words: unavailable",
                                  "status": "UNAVAILABLE"}}),
            ),
        ),
    ];
    for (code, bytes) in cases {
        let ep = spawn_scripted(move |_| bytes.clone());
        let err = match post(&ep, discover()) {
            Err(e) => e.to_string(),
            Ok(other) => panic!("HTTP {code} is a status, not an answer: {other:?}"),
        };
        assert!(
            err.contains(&format!("server returned HTTP {code}")),
            "{err}"
        );
        assert!(!err.contains("the server's own words"), "{err}");
    }
}

#[test]
fn a_success_with_no_body_is_no_answer() {
    // A `202` to a request: accepted, and not answered. Said as that, not as a
    // JSON parse error about an empty body.
    let ep = spawn_scripted(|_| raw("202 Accepted", &[], ""));
    let err = match post(&ep, discover()) {
        Err(e) => e.to_string(),
        Ok(other) => panic!("a 202 is no answer: {other:?}"),
    };
    assert!(err.contains("no JSON-RPC response"), "{err}");
}

#[test]
fn a_session_is_taken_only_from_the_initialize_answer() {
    // Every answer is stamped; only the `initialize` answer's stamp is a
    // session.
    let ep = spawn_scripted(|msg| {
        let sid = match msg["method"].as_str() {
            Some("initialize") => "y",
            _ if msg["id"] == 9 => "z",
            _ => "x",
        };
        let resp = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}});
        raw(
            "200 OK",
            &[
                "Content-Type: application/json",
                &format!("Mcp-Session-Id: {sid}"),
            ],
            &resp.to_string(),
        )
    });
    let t = HttpTransport::new(McpEndpoint::parse(&ep).unwrap(), vec![]);
    let send = |id: i64, method: &str| {
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}});
        t.send(
            Some(id),
            body.to_string().as_bytes(),
            Duration::from_secs(5),
            &[],
            |_| {},
        )
        .expect("answered");
    };

    send(0, "server/discover");
    send(1, "tools/list");
    assert_eq!(
        t.session_id(),
        None,
        "a stamp on another answer is not a session"
    );
    send(2, "initialize");
    assert_eq!(t.session_id().as_deref(), Some("y"));
    send(9, "tools/list");
    assert_eq!(
        t.session_id().as_deref(),
        Some("y"),
        "the session is the initialize answer's"
    );
}

#[test]
fn a_refused_initialize_opens_no_session() {
    // A handshake the server refused — a revision it does not speak — opened
    // nothing, whatever it stamped; the retry must not carry the stamp.
    let ep = spawn_scripted(|msg| {
        raw(
            "400 Bad Request",
            &["Content-Type: application/json", "Mcp-Session-Id: refused"],
            &rpc_error(msg["id"].clone(), -32022, "Unsupported protocol version").to_string(),
        )
    });
    let t = HttpTransport::new(McpEndpoint::parse(&ep).unwrap(), vec![]);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    let _ = t.send(
        Some(1),
        body.to_string().as_bytes(),
        Duration::from_secs(5),
        &[],
        |_| {},
    );
    assert_eq!(t.session_id(), None);
}

#[test]
fn an_initialize_answered_on_a_stream_completes_the_handshake() {
    // A server that speaks before it answers `initialize` (a log line) keeps
    // the answer's stream open across the first frame: the exchange must
    // outlive `post_message`, which returns with that first frame, until the
    // answer arrives.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let ep = format!("http://{}/mcp", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some((start, headers)) = read_head(&mut r) else {
                    return;
                };
                let body = read_body(&mut r, &headers);
                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                if start.starts_with("GET") {
                    let _ = w.write_all(&raw("405 Method Not Allowed", &[], ""));
                    return;
                }
                if msg["method"] != "initialize" {
                    let _ = w.write_all(&raw("202 Accepted", &[], ""));
                    return;
                }
                let _ = w.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nMcp-Session-Id: s-1\r\nConnection: close\r\n\r\n",
                );
                let log = json!({"jsonrpc": "2.0", "method": "notifications/message",
                                 "params": {"level": "info", "data": "starting"}});
                let _ = write!(w, "data: {log}\n\n");
                let _ = w.flush();
                std::thread::sleep(Duration::from_millis(300));
                let answer = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {
                    "protocolVersion": msg["params"]["protocolVersion"],
                    "capabilities": {"events": {"x": 1}},
                    "serverInfo": {"name": "chatty", "version": "0"}}});
                let _ = write!(w, "data: {answer}\n\n");
                let _ = w.flush();
            });
        }
    });
    let mut c = mcp::client::McpClient::connect("chatty", &ep, vec![], Duration::from_secs(5))
        .expect("connect");
    c.initialize()
        .expect("the handshake completes after the server's log line");
    assert_eq!(c.protocol_version().as_deref(), Some("2025-11-25"));
    // The answer's capabilities are kept as sent though it came at the end
    // of a stream: members rmcp does not model are not lost.
    assert_eq!(c.capability("events"), Some(json!({"x": 1})));
}

#[test]
fn a_404_with_a_json_rpc_body_on_a_session_marks_it_lost() {
    // SDK servers answer a forgotten session with a `404` that says so in
    // JSON-RPC. The body is the call's answer, and the `404` is still what
    // tells the host to re-dial.
    let ep = spawn_scripted(|msg| match msg["method"].as_str() {
        Some("initialize") => initialized(msg),
        Some("tools/list") => json_body(
            "404 Not Found",
            &rpc_error(msg["id"].clone(), -32001, "Session not found"),
        ),
        _ => raw("202 Accepted", &[], ""),
    });
    let mut c = mcp::client::McpClient::connect("forgets", &ep, vec![], Duration::from_secs(5))
        .expect("connect");
    c.initialize().expect("initialize");
    let err = c.list_tools().expect_err("the session is gone");
    assert!(
        matches!(err, mcp::client::McpError::SessionExpired(_)),
        "{err}"
    );
    assert!(c.session_lost());
}

#[test]
fn an_initialize_refused_with_a_json_rpc_error_says_the_servers_error() {
    let ep = spawn_scripted(|_| {
        json_body(
            "400 Bad Request",
            &rpc_error(Value::Null, -32022, "Unsupported protocol version"),
        )
    });
    let err = match RmcpBuilder::new("refuses", &ep, vec![], Duration::from_secs(5)).connect() {
        Err(e) => e.to_string(),
        Ok(_) => panic!("the server refused the handshake"),
    };
    assert!(err.contains("Unsupported protocol version"), "{err}");
}

// ------------------------------- 5. a dropped response stream hangs up

/// The `tools/call` request (id 1) the SDK's worker posts for a call.
fn tools_call() -> ClientJsonRpcMessage {
    serde_json::from_value(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "slow", "arguments": {}}
    }))
    .expect("a tools/call request")
}

/// Watch a connection the server has finished writing to, until the client
/// closes its side or `deadline` passes: the instant the close was seen, and
/// whatever bytes the client sent meanwhile.
///
/// A read that times out (`WouldBlock` or `TimedOut`, depending on the
/// platform) is a connection still open; only end-of-stream, or a reset or
/// broken pipe, is the client hanging up. Counting a timeout as a close would
/// see a hang-up within 50 ms of every exchange, hung up or not.
fn watch_for_close(conn: &mut TcpStream, deadline: Instant) -> (Option<Instant>, Vec<u8>) {
    conn.set_read_timeout(Some(Duration::from_millis(50))).ok();
    let mut sent = Vec::new();
    while Instant::now() < deadline {
        let mut b = [0u8; 1];
        match conn.read(&mut b) {
            Ok(0) => return (Some(Instant::now()), sent),
            Ok(_) => sent.push(b[0]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return (Some(Instant::now()), sent),
        }
    }
    (None, sent)
}

/// What a holding server saw of the client's side once it had answered: when
/// the client closed, if it did, and any bytes it sent.
type Watched = std::sync::mpsc::Receiver<(Option<Instant>, Vec<u8>)>;

/// A server that answers one POST with an event stream carrying `frames`,
/// then holds the connection and reports what it saw of the client's side
/// (see [`watch_for_close`]) over the next 5 s.
fn spawn_holding(frames: String) -> (String, Watched) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((conn, _)) = listener.accept() else {
            return;
        };
        conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut w = conn.try_clone().unwrap();
        let mut r = BufReader::new(conn);
        let Some((_, headers)) = read_head(&mut r) else {
            return;
        };
        let _ = read_body(&mut r, &headers);
        let _ = w.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n\r\n",
        );
        let _ = w.write_all(frames.as_bytes());
        let _ = w.flush();
        let _ = tx.send(watch_for_close(
            &mut w,
            Instant::now() + Duration::from_secs(5),
        ));
    });
    (format!("http://{addr}/mcp"), rx)
}

/// POST `message` through agentd's socket and hand the response stream to
/// `use_stream`, as the SDK's worker would — on a runtime that outlives it.
fn with_stream<T>(
    ep: &str,
    message: ClientJsonRpcMessage,
    use_stream: impl AsyncFnOnce(
        futures::stream::BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
    ) -> T,
) -> T {
    let http = Arc::new(HttpTransport::new(McpEndpoint::parse(ep).unwrap(), vec![]));
    // A socket timeout far past every bound below: what ends the exchange
    // must be the hang-up, not the socket giving up on a silent server.
    let agentd = mcp::rmcp_transport::AgentdHttp::new(http, Duration::from_secs(30));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            match agentd
                .post_message(Arc::from(ep), message, None, None, HashMap::new())
                .await
            {
                Ok(StreamableHttpPostResponse::Sse(stream, _)) => use_stream(stream).await,
                other => panic!("expected a response stream, got {other:?}"),
            }
        })
}

#[test]
fn a_response_stream_the_sdk_drops_hangs_up_its_connection() {
    // A call in progress: the server has said something (a progress
    // notification) and is still working. Then the SDK drops the stream — a
    // listen replaced, a request cancelled. At 2026-07-28 that close IS the
    // cancellation, so the server must see the connection go, now — not
    // whenever its next frame would have found the reader gone, and not at
    // the socket's 30 s timeout.
    let progress = json!({"jsonrpc": "2.0", "method": "notifications/progress",
                          "params": {"progressToken": 1, "progress": 1}});
    let (ep, seen) = spawn_holding(format!("data: {progress}\n\n"));
    let dropped = with_stream(&ep, tools_call(), async |mut stream| {
        let first = stream.next().await.expect("a frame").expect("an event");
        assert!(
            first
                .data
                .as_deref()
                .unwrap_or("")
                .contains("notifications/progress"),
            "{first:?}"
        );
        drop(stream);
        Instant::now()
    });
    let (closed, _) = seen.recv().expect("the server reports");
    let closed = closed.expect("the server never saw the connection close");
    assert!(
        closed.saturating_duration_since(dropped) < Duration::from_secs(3),
        "hung up {:?} after the drop",
        closed.saturating_duration_since(dropped)
    );
}

#[test]
fn a_finished_exchange_closes_with_its_answer_not_with_its_stream() {
    // The hang-up handle keeps its own descriptor on the socket, and a socket
    // closes only when its last descriptor does. So the exchange lets go of
    // that descriptor when it ends: the connection closes once the answer is
    // read, as it did before the handle existed — not whenever the SDK gets
    // round to dropping a stream it has already read the answer from.
    let answer = json!({"jsonrpc": "2.0", "id": 1,
                        "result": {"content": [], "isError": false}});
    let (ep, seen) = spawn_holding(format!("data: {answer}\n\n"));
    let (closed, answered) = with_stream(&ep, tools_call(), async |mut stream| {
        let first = stream.next().await.expect("a frame").expect("an event");
        assert!(
            first.data.as_deref().unwrap_or("").contains("isError"),
            "{first:?}"
        );
        let answered = Instant::now();
        // The stream is still held while the server reports.
        let (closed, _) = seen.recv().expect("the server reports");
        drop(stream);
        (closed, answered)
    });
    let closed = closed.expect("the connection outlived its answer");
    assert!(
        closed.saturating_duration_since(answered) < Duration::from_secs(3),
        "closed {:?} after the answer",
        closed.saturating_duration_since(answered)
    );
}

#[test]
fn a_bootstrap_post_dropped_while_it_waits_hangs_up() {
    // `server/discover` waits inside `post_message` for its whole answer. If
    // the SDK drops that wait — its own bound on the bootstrap — the exchange
    // goes with it, exactly as a dropped stream's does.
    let (ep, seen) = spawn_holding(String::new());
    let http = Arc::new(HttpTransport::new(McpEndpoint::parse(&ep).unwrap(), vec![]));
    let agentd = mcp::rmcp_transport::AgentdHttp::new(http, Duration::from_secs(30));
    let dropped = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let post = agentd.post_message(
                Arc::from(ep.as_str()),
                discover(),
                None,
                None,
                HashMap::new(),
            );
            let waited = tokio::time::timeout(Duration::from_millis(300), post).await;
            assert!(waited.is_err(), "the server never answers");
            Instant::now()
        });
    let (closed, _) = seen.recv().expect("the server reports");
    let closed = closed.expect("the server never saw the connection close");
    assert!(
        closed.saturating_duration_since(dropped) < Duration::from_secs(3),
        "hung up {:?} after the drop",
        closed.saturating_duration_since(dropped)
    );
}

#[test]
fn a_tools_list_answer_comes_back_whole() {
    // rmcp records each tool's `x-mcp-header` schema only from a list answer
    // it takes off the wire, not from one it drains from a stream that just
    // ended, so a list answered on a stream could leave the next call without
    // its `Mcp-Param-*` headers, depending on timing. Handed back whole, the
    // answer is always recorded.
    let ep = spawn_scripted(|msg| {
        json_body(
            "200 OK",
            &json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [
                {"name": "t", "inputSchema": {"type": "object"}}]}}),
        )
    });
    let list: ClientJsonRpcMessage = serde_json::from_value(
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}}),
    )
    .unwrap();
    let got = whole(post(&ep, list));
    assert_eq!(got["result"]["tools"][0]["name"], "t", "{got}");
}

#[test]
fn what_a_server_streams_ahead_of_a_tools_list_answer_goes_to_the_queue() {
    // The answer still comes back whole; a notification the handler would
    // have queued is queued, one it would have ignored is not, in order.
    let ep = spawn_scripted(|msg| {
        let log = json!({"jsonrpc": "2.0", "method": "notifications/message",
                         "params": {"level": "info", "data": "listing"}});
        let other = json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                           "params": {"requestId": 1}});
        let changed = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        let answer = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [
            {"name": "t", "inputSchema": {"type": "object"}}]}});
        raw(
            "200 OK",
            &["Content-Type: text/event-stream"],
            &format!("data: {log}\n\ndata: {other}\n\ndata: {changed}\n\ndata: {answer}\n\n"),
        )
    });
    let list = || -> ClientJsonRpcMessage {
        serde_json::from_value(
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}}),
        )
        .unwrap()
    };
    let queue: Arc<Mutex<Vec<mcp::rpc::Notification>>> = Arc::default();
    let got = whole(post_via(
        agentd(&ep).with_notifications(Arc::clone(&queue)),
        &ep,
        list(),
    ));
    assert_eq!(got["result"]["tools"][0]["name"], "t", "{got}");
    let queued: Vec<(String, Value)> = queue
        .lock()
        .unwrap()
        .iter()
        .map(|n| (n.method.clone(), n.params.clone().unwrap_or_default()))
        .collect();
    assert_eq!(
        queued,
        [
            (
                "notifications/message".to_string(),
                json!({"level": "info", "data": "listing"})
            ),
            ("notifications/tools/list_changed".to_string(), json!({})),
        ]
    );

    // A connection with no queue hands the stream on as it came.
    assert!(
        matches!(post(&ep, list()), Ok(StreamableHttpPostResponse::Sse(..))),
        "no queue, no hand-off"
    );

    // A request ahead of the answer is not taken: only the handler can answer
    // it, so it and the answer go on the stream.
    let ep = spawn_scripted(|msg| {
        let ask = json!({"jsonrpc": "2.0", "id": 9, "method": "ping"});
        let answer = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": []}});
        raw(
            "200 OK",
            &["Content-Type: text/event-stream"],
            &format!("data: {ask}\n\ndata: {answer}\n\n"),
        )
    });
    let queue: Arc<Mutex<Vec<mcp::rpc::Notification>>> = Arc::default();
    let out = post_via(
        agentd(&ep).with_notifications(Arc::clone(&queue)),
        &ep,
        list(),
    );
    assert!(
        matches!(out, Ok(StreamableHttpPostResponse::Sse(..))),
        "a request ahead goes on the stream: {out:?}"
    );
    assert!(queue.lock().unwrap().is_empty());
}

#[test]
fn a_list_stream_that_ends_unanswered_answers_with_the_reason() {
    // A request ahead of the answer sends the list down the stream; a stream
    // that then ends with no answer answers the list with the transport's
    // reason, as any other request's stream does — not with the SDK's generic
    // "closed before its final response".
    let ep = spawn_scripted(|_| {
        let ask = json!({"jsonrpc": "2.0", "id": 9, "method": "ping"});
        raw(
            "200 OK",
            &["Content-Type: text/event-stream"],
            &format!("data: {ask}\n\n"),
        )
    });
    let list: ClientJsonRpcMessage = serde_json::from_value(
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}}),
    )
    .unwrap();
    let frames: Vec<Value> = with_stream(&ep, list, async |stream| {
        use futures::StreamExt;
        stream
            .filter_map(|e| async move { e.ok()?.data })
            .map(|d| serde_json::from_str(&d).unwrap())
            .collect()
            .await
    });
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["method"], "ping", "{frames:?}");
    assert_eq!(frames[1]["id"], 3, "{frames:?}");
    assert!(
        frames[1]["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{frames:?}"
    );
}
