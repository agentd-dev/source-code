// SPDX-License-Identifier: AGPL-3.0-only
//! Transport hardening: the three places where a stream, not a request, is the
//! thing that has to be right.
//!
//! * The server→client notification stream is a DIAL like any other, and a
//!   signed server rejects an unsigned one — silently, from the daemon's point
//!   of view, because nothing fails except the waking up.
//! * A server may interleave a request of its own on the response stream of a
//!   POST and block until it is answered. Frames that are collected and handed
//!   over afterwards arrive after the server gave up.
//! * The inbound reader parses a request head before anyone has authenticated,
//!   so its bounds are the only thing between a remote peer and our memory.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mcp::http::{HttpTransport, McpEndpoint, RequestSigner};
use mcp::http_server::{AllowAll, HttpAcceptor, bind_tcp, spawn_accept_http};
use mcp::inbound::{Answer, Handler as InboundHandler, Inbound};
use mcp::rmcp_client::RmcpBuilder;
use mcp::rpc::{Request, Response};
use mcp::server::{Handler, PeerOrigin, SharedWriter, SubRegistry};
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
    ) -> Vec<(String, String)> {
        vec![
            (
                "Signature-Input".to_string(),
                "sig1=(\"@method\" \"@authority\" \"@path\")".to_string(),
            ),
            (
                "Signature".to_string(),
                format!("sig1=:{method} {authority} {path} {}:", body.len()),
            ),
        ]
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
    t.set_protocol_version("2025-11-25".to_string());

    // The legacy era's long-lived GET stream.
    t.open_events(Duration::from_secs(5))
        .expect("the stub answers with an event stream");
    // …and the modern era's `subscriptions/listen` stream, which replaces it.
    let listen = br#"{"jsonrpc":"2.0","id":1,"method":"subscriptions/listen","params":{}}"#;
    t.open_listen(
        Duration::from_secs(5),
        listen,
        &[("Mcp-Method", "subscriptions/listen")],
    )
    .expect("the stub answers with an event stream");

    let dials = seen.lock().unwrap().clone();
    assert_eq!(dials.len(), 2, "two dials expected: {dials:?}");

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

    let (line, headers) = &dials[1];
    assert!(line.starts_with("POST "), "{line}");
    let sig = header(headers, "signature").unwrap_or_default();
    assert!(
        sig.starts_with("sig1=:POST ") && sig.ends_with(&format!(" {}:", listen.len())),
        "the listen dial must be signed as a POST over its body: {sig}"
    );
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

                // Block on the answer, exactly as a real elicitation does. The
                // bound is what keeps a failing run finite instead of hung.
                let mut got = state.got.lock().unwrap();
                let deadline = Duration::from_secs(3);
                let start = Instant::now();
                while got.is_none() && start.elapsed() < deadline {
                    let (g, _) = state
                        .wake
                        .wait_timeout(got, deadline - start.elapsed())
                        .unwrap();
                    got = g;
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
        .call_tool("ask", Some(json!({})))
        .expect("tools/call must complete — the server is waiting on our answer");
    assert_eq!(
        out["content"][0]["text"], "answered",
        "the server finished only if the elicitation was answered while its \
         reply was still open (buffered frames arrive after it gives up): {out}"
    );
    // The answer is a JSON-RPC RESPONSE, which the server acks `202` with no
    // body — a transport that waited for a reply to it would fail that send.
    let again = client
        .call_tool("ask", Some(json!({})))
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

// ------------------------------------- 3. the inbound head reader is bounded

/// The smallest handler that answers a tool call.
struct Trivial;
impl Handler for Trivial {
    fn dispatch(&self, req: Request, _o: PeerOrigin, _w: &SharedWriter, _c: u64) -> Response {
        Response::ok(req.id, json!({"ok": true}))
    }
}

fn spawn_bounded_server() -> String {
    let subs: SubRegistry = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let listener = bind_tcp("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    spawn_accept_http(
        listener,
        Arc::new(HttpAcceptor::Plain),
        Arc::new(Trivial),
        Arc::new(AllowAll),
        subs,
        Arc::new(AtomicU64::new(0)),
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
    let line = "POST /mcp HTTP/1.1\r\n";
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
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#;
    let ok = format!(
        "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{call}",
        call.len()
    );
    assert_eq!(status_of(&addr, ok.as_bytes()), 200);
}
