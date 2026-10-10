// SPDX-License-Identifier: AGPL-3.0-only
//! **The other direction**: a server sending the client a request.
//!
//! MCP is bidirectional, and a client that is deaf that way drops every
//! server→client request, including `ping`, which the spec says both sides MUST
//! answer. A server is entitled to read that silence as a dead connection.
//!
//! These tests stand up a real HTTP MCP server on a socket, drive a real
//! `McpClient` at it, and assert on what comes back over the wire: that the
//! handshake advertises only what we can answer, that `ping` is answered
//! unconditionally, and that an `elicitation/create` reaches the host handler
//! and its decision reaches the server.
//!
//! The SDK answers these, so what is really being asserted is that the
//! behaviour survives all the way out to the wire: it is checked from the
//! server's side, which is the only side that can tell.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;

use mcp::client::McpClient;
use mcp::inbound::{Answer, Handler, Inbound};
use serde_json::{Value, json};

/// What the mock server observed, for assertions after the exchange.
#[derive(Default)]
struct Seen {
    /// The `capabilities` object from the client's `initialize`.
    init_capabilities: Option<Value>,
    /// Every JSON-RPC message the client POSTed that was not a request of its
    /// own — i.e. the responses it sent us.
    responses: Vec<Value>,
}

type Shared = Arc<Mutex<Seen>>;

/// Read one HTTP request (headers + body by Content-Length) off `s`.
fn read_http(s: &mut BufReader<TcpStream>) -> Option<(String, Vec<u8>)> {
    let mut start = String::new();
    if s.read_line(&mut start).ok()? == 0 {
        return None;
    }
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if s.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        s.read_exact(&mut body).ok()?;
    }
    Some((start, body))
}

/// An `initialize` reply that assigns a session, as a Streamable-HTTP server
/// does. The client opens the server→client SSE channel for a session, so
/// without one there is nowhere for a server request to arrive.
fn json_response_with_session(body: &Value) -> Vec<u8> {
    let b = serde_json::to_vec(body).unwrap();
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    )
    .into_bytes();
    out.extend_from_slice(&b);
    out
}

fn json_response(body: &Value) -> Vec<u8> {
    let b = serde_json::to_vec(body).unwrap();
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    )
    .into_bytes();
    out.extend_from_slice(&b);
    out
}

/// A server that answers `initialize`, then pushes `push` (a server→client
/// request) down the SSE event stream and records whatever the client POSTs.
fn spawn_server(seen: Shared, push: Option<Value>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let seen = Arc::clone(&seen);
            let push = push.clone();
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some((start, body)) = read_http(&mut r) else {
                    return;
                };

                // The SSE channel: push one server→client request, then hold.
                if start.starts_with("GET") {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
                    let _ = w.write_all(head.as_bytes());
                    if let Some(p) = &push {
                        let _ = w.write_all(format!("data: {p}\n\n").as_bytes());
                        let _ = w.flush();
                    }
                    std::thread::sleep(Duration::from_secs(3));
                    return;
                }

                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                let is_request = !method.is_empty() && msg.get("id").is_some();
                let is_response = method.is_empty() && msg.get("id").is_some();

                // A RESPONSE to something we pushed — the thing under test.
                if is_response {
                    seen.lock().unwrap().responses.push(msg.clone());
                    let _ = w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    let _ = w.flush();
                    return;
                }
                // A notification from the client (e.g. `initialized`).
                if !is_request {
                    let _ = w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    let _ = w.flush();
                    return;
                }

                match method {
                    "initialize" => {
                        seen.lock().unwrap().init_capabilities = msg["params"]["capabilities"]
                            .as_object()
                            .map(|_| msg["params"]["capabilities"].clone());
                        // A session id, as a Streamable-HTTP server assigns on
                        // initialize. The client opens the server→client SSE
                        // channel for a session, so without this there is no
                        // channel for the pushes below to arrive on.
                        let _ = w.write_all(&json_response_with_session(&json!({
                            "jsonrpc": "2.0", "id": msg["id"],
                            "result": {
                                "protocolVersion": "2025-11-25",
                                "capabilities": {"tools": {}, "resources": {"subscribe": true}},
                                "serverInfo": {"name": "mock", "version": "0"}
                            }
                        })));
                    }
                    // Any other client request (resources/subscribe, …) gets a
                    // bare success — enough for the call to return so the
                    // client goes on to open its event stream.
                    _ => {
                        let _ = w.write_all(&json_response(&json!({
                            "jsonrpc": "2.0", "id": msg["id"], "result": {}
                        })));
                    }
                }
                let _ = w.flush();
            });
        }
    });
    format!("http://{addr}/mcp")
}

/// Wait for the server to record a response, or give up.
fn await_response(seen: &Shared, secs: u64) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if let Some(v) = seen.lock().unwrap().responses.first().cloned() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    None
}

struct Answers(Answer);
impl Handler for Answers {
    fn handle(&self, req: Inbound) -> Option<Answer> {
        // Assert the server's question survived the trip intact.
        if let Inbound::Elicit { message, .. } = &req {
            assert_eq!(message, "Which environment?");
        }
        Some(self.0.clone())
    }
}

#[test]
fn the_handshake_advertises_only_what_the_host_can_answer() {
    // No handler ⇒ no elicitation capability, so a server never asks.
    let seen: Shared = Arc::default();
    let ep = spawn_server(Arc::clone(&seen), None);
    let mut c = McpClient::connect("mock", &ep, vec![], Duration::from_secs(3)).unwrap();
    c.initialize().unwrap();
    let caps = seen.lock().unwrap().init_capabilities.clone().unwrap();
    assert!(caps.get("elicitation").is_none(), "undeclared: {caps}");

    // With a handler, the same handshake declares it.
    let seen2: Shared = Arc::default();
    let ep2 = spawn_server(Arc::clone(&seen2), None);
    let h: Arc<dyn Handler> = Arc::new(Answers(Answer::Decline));
    let mut c2 = McpClient::connect("mock", &ep2, vec![], Duration::from_secs(3))
        .unwrap()
        .with_elicitation(h);
    c2.initialize().unwrap();
    let caps2 = seen2.lock().unwrap().init_capabilities.clone().unwrap();
    assert_eq!(caps2["elicitation"], json!({}), "declared: {caps2}");
}

#[test]
fn a_server_ping_is_answered_over_the_event_stream() {
    // The failure this whole module guards against: a server pinging the client
    // gets silence and can rightly treat the session as dead.
    let seen: Shared = Arc::default();
    let ping = json!({"jsonrpc": "2.0", "id": 77, "method": "ping"});
    let ep = spawn_server(Arc::clone(&seen), Some(ping));
    let mut c = McpClient::connect("mock", &ep, vec![], Duration::from_secs(3)).unwrap();
    c.initialize().unwrap();
    // Opening the notification stream is what exposes us to server requests.
    let _ = c.subscribe("file:///x");

    let resp = await_response(&seen, 5).expect("the client never answered the ping");
    assert_eq!(resp["id"], 77);
    assert_eq!(resp["result"], json!({}));
    assert!(resp.get("error").is_none());
}

#[test]
fn an_elicitation_reaches_the_host_and_its_answer_reaches_the_server() {
    let seen: Shared = Arc::default();
    let elicit = json!({
        "jsonrpc": "2.0", "id": 9, "method": "elicitation/create",
        "params": {
            "message": "Which environment?",
            "requestedSchema": {"type": "object", "properties": {"env": {"type": "string"}}}
        }
    });
    let ep = spawn_server(Arc::clone(&seen), Some(elicit));
    let h: Arc<dyn Handler> = Arc::new(Answers(Answer::Accept(json!({"env": "staging"}))));
    let mut c = McpClient::connect("mock", &ep, vec![], Duration::from_secs(3))
        .unwrap()
        .with_elicitation(h);
    c.initialize().unwrap();
    let _ = c.subscribe("file:///x");

    let resp = await_response(&seen, 5).expect("the elicitation was never answered");
    assert_eq!(resp["id"], 9);
    assert_eq!(resp["result"]["action"], "accept");
    assert_eq!(resp["result"]["content"]["env"], "staging");
}

#[test]
fn an_undeclared_capability_is_refused_rather_than_ignored() {
    // A server may feature-detect by calling. Silence is the one answer that
    // teaches it nothing; -32601 tells it to stop asking.
    let seen: Shared = Arc::default();
    let elicit = json!({
        "jsonrpc": "2.0", "id": 5, "method": "elicitation/create",
        "params": {"message": "Which environment?", "requestedSchema": {}}
    });
    let ep = spawn_server(Arc::clone(&seen), Some(elicit));
    let mut c = McpClient::connect("mock", &ep, vec![], Duration::from_secs(3)).unwrap();
    c.initialize().unwrap();
    let _ = c.subscribe("file:///x");

    let resp = await_response(&seen, 5).expect("no refusal was sent");
    assert_eq!(resp["id"], 5);
    assert_eq!(resp["error"]["code"], -32601);
}

/// Answers every elicitation with `{x: 1}`.
struct GivesOne;
impl Handler for GivesOne {
    fn handle(&self, _: Inbound) -> Option<Answer> {
        Some(Answer::Accept(json!({"x": 1})))
    }
}

#[test]
fn an_elicitation_inside_a_2026_07_28_tool_call_round_trips() {
    // At 2026-07-28 a server asks mid-call by answering the call
    // `input_required` (SEP-2322); the client answers and calls again with a
    // new id, echoing `requestState` and carrying its answers.
    let (ep, seen) = common::spawn(|req, w| {
        let id = req.id();
        let supported = [common::MODERN.to_string()];
        if let Some((status, body)) = common::stateless_refusal(req, &supported) {
            return common::respond(w, status, &[], &body);
        }
        match req.method() {
            "server/discover" => common::respond(
                w,
                200,
                &[],
                &common::discover_answer(&id, &common::StatelessOpts::default()),
            ),
            "tools/list" => common::respond(
                w,
                200,
                &[],
                &common::result(
                    &id,
                    json!({"tools": [{"name": "ask", "inputSchema": {"type": "object"}}]}),
                ),
            ),
            "tools/call" if req.body["params"]["requestState"].is_null() => common::respond(
                w,
                200,
                &[],
                &common::result(
                    &id,
                    json!({
                    "resultType": "input_required",
                    "inputRequests": {"q": {"method": "elicitation/create", "params": {
                        "message": "how many?",
                        "requestedSchema": {"type": "object",
                                            "properties": {"x": {"type": "number"}}}}}},
                    "requestState": "s1"}),
                ),
            ),
            "tools/call" => common::respond(
                w,
                200,
                &[],
                &common::result(
                    &id,
                    json!({
                    "content": [{"type": "text", "text": "thanks"}], "isError": false}),
                ),
            ),
            _ => common::respond(w, 404, &[], &common::error(&id, -32601, "Method not found")),
        }
    });
    let mut c = McpClient::connect("mrtr", &ep, vec![], Duration::from_secs(5))
        .unwrap()
        .start_with(mcp::client::FirstRequest::Discover)
        .with_elicitation(Arc::new(GivesOne));
    c.initialize().unwrap();
    let out = c
        .call_tool("ask", Some(json!({})))
        .expect("the call completes");
    assert_eq!(out.content[0]["text"], "thanks", "{out:?}");

    let calls = common::requests(&seen, "tools/call");
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].id(), calls[1].id(), "the retry is a new request");
    assert!(
        calls[0]
            .meta("clientCapabilities")
            .is_some_and(|c| c.get("elicitation").is_some()),
        "{:?}",
        calls[0].meta("clientCapabilities")
    );
    let retry = &calls[1].body["params"];
    assert_eq!(retry["requestState"], "s1");
    assert_eq!(retry["inputResponses"]["q"]["action"], "accept", "{retry}");
    assert_eq!(
        retry["inputResponses"]["q"]["content"],
        json!({"x": 1}),
        "{retry}"
    );
}
