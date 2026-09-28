// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP client against a real server.
//!
//! Building on [`rmcp`] means inheriting spec-tracking from upstream, so these
//! tests assert on the things that would silently break if the facade were
//! wired wrong: that the revision on the wire is the one the SDK declares (not
//! one we picked), that declared capabilities reach the handshake, and that
//! tools and resources come back in agentd's own wire types.
//!
//! It also pins the one SDK default agentd turns off: the SEP-2549 client
//! response cache. A read here is a request to the server, every time, and a
//! server that is gone or says no is an error — never the last answer replayed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcp::inbound::{Answer, Handler, Inbound};
use mcp::rmcp_client::RmcpBuilder;
use serde_json::{Value, json};

#[derive(Default)]
struct Seen {
    init: Option<Value>,
    /// Every JSON-RPC method the client called, in order.
    methods: Vec<String>,
}
type Shared = Arc<Mutex<Seen>>;

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

fn respond(w: &mut TcpStream, body: &Value) {
    let b = serde_json::to_vec(body).unwrap();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    );
    let _ = w.write_all(head.as_bytes());
    let _ = w.write_all(&b);
    let _ = w.flush();
}

/// A minimal Streamable-HTTP MCP server: answers initialize, tools/list and
/// resources/list, and records the handshake for assertions.
fn spawn_server(seen: Shared) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some((start, body)) = read_http(&mut r) else {
                    return;
                };
                if start.starts_with("GET") || start.starts_with("DELETE") {
                    let _ = w
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                if msg.get("id").is_none() {
                    let _ = w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let id = msg["id"].clone();
                seen.lock().unwrap().methods.push(method.to_string());
                match method {
                    "initialize" => {
                        seen.lock().unwrap().init = Some(msg["params"].clone());
                        respond(
                            &mut w,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": {
                                    // Echo what the client asked for, the way a
                                    // real server does when it can speak it.
                                    "protocolVersion": msg["params"]["protocolVersion"],
                                    "capabilities": {"tools": {}, "resources": {"subscribe": true}},
                                    "serverInfo": {"name": "mock", "version": "0"}
                                }
                            }),
                        );
                    }
                    "tools/list" => respond(
                        &mut w,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"tools": [{
                                "name": "echo",
                                "description": "echo a string",
                                "inputSchema": {"type": "object", "properties": {"s": {"type": "string"}}}
                            }]}
                        }),
                    ),
                    "resources/list" => respond(
                        &mut w,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"resources": [{"uri": "file:///a.txt", "name": "a"}]}
                        }),
                    ),
                    "tools/call" => respond(
                        &mut w,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"content": [{"type": "text", "text": "echoed"}], "isError": false}
                        }),
                    ),
                    _ => respond(&mut w, &json!({"jsonrpc": "2.0", "id": id, "result": {}})),
                }
            });
        }
    });
    format!("http://{addr}/mcp")
}

struct Yes;
impl Handler for Yes {
    fn handle(&self, _req: Inbound) -> Option<Answer> {
        Some(Answer::Accept(json!({"env": "staging"})))
    }
}

#[test]
fn the_handshake_speaks_whatever_revision_the_sdk_supports() {
    // Deliberately not pinned to a date: this backend follows rmcp's own
    // `LATEST`, so it adopts the stateless revision on the release that
    // promotes it — without a change here. Pinning our own constant would mean
    // asking servers for a dialect the SDK may not fully implement.
    let expected = rmcp::model::ProtocolVersion::LATEST.to_string();
    let seen: Shared = Arc::default();
    let ep = spawn_server(Arc::clone(&seen));
    let client = RmcpBuilder::new("mock", &ep, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");
    let init = seen
        .lock()
        .unwrap()
        .init
        .clone()
        .expect("no initialize seen");
    assert_eq!(init["protocolVersion"], expected, "handshake: {init}");
    assert_eq!(client.protocol_version(), Some(expected.as_str()));
    // …and it is a revision our own version table recognises, so the SDK and
    // this crate cannot drift apart unnoticed.
    assert!(mcp::version::is_supported_version(&expected));
}

#[test]
fn subscribing_uses_the_method_the_negotiated_revision_defines() {
    // The eras disagree: legacy has `resources/subscribe`, the stateless
    // revision replaces it with `subscriptions/listen`. Whichever rmcp
    // negotiates, we must call the one that version actually defines — a
    // `listen` against a legacy server is an unknown method.
    let seen: Shared = Arc::default();
    let ep = spawn_server(Arc::clone(&seen));
    let client = RmcpBuilder::new("mock", &ep, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");
    client.subscribe("file:///a.txt").expect("subscribe");

    let called = seen.lock().unwrap().methods.clone();
    let modern = matches!(
        mcp::version::era_of(client.protocol_version().unwrap_or("")),
        mcp::version::Era::Modern
    );
    if modern {
        assert!(
            called.iter().any(|m| m == "subscriptions/listen"),
            "modern revision should listen, saw: {called:?}"
        );
    } else {
        assert!(
            called.iter().any(|m| m == "resources/subscribe"),
            "legacy revision should subscribe, saw: {called:?}"
        );
        assert!(
            !called.iter().any(|m| m == "subscriptions/listen"),
            "listen is not defined at this revision: {called:?}"
        );
    }
}

#[test]
fn declared_capabilities_reach_the_handshake() {
    let seen: Shared = Arc::default();
    let ep = spawn_server(Arc::clone(&seen));
    let h: Arc<dyn Handler> = Arc::new(Yes);
    let _client = RmcpBuilder::new("mock", &ep, vec![], Duration::from_secs(5))
        .with_elicitation(h)
        .connect()
        .expect("connect");
    let init = seen.lock().unwrap().init.clone().unwrap();
    assert_eq!(
        init["capabilities"]["elicitation"],
        json!({}),
        "elicitation should be declared: {init}"
    );

    // …and absent when the host cannot answer one.
    let seen2: Shared = Arc::default();
    let ep2 = spawn_server(Arc::clone(&seen2));
    let _plain = RmcpBuilder::new("mock", &ep2, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");
    let init2 = seen2.lock().unwrap().init.clone().unwrap();
    assert!(
        init2["capabilities"].get("elicitation").is_none(),
        "undeclared: {init2}"
    );
}

#[test]
fn tools_and_resources_come_back_as_agentds_own_wire_types() {
    // The facade converts through JSON; this is the check that the two shapes
    // really do line up rather than silently dropping fields.
    let seen: Shared = Arc::default();
    let ep = spawn_server(Arc::clone(&seen));
    let client = RmcpBuilder::new("mock", &ep, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");

    let tools = client.list_tools().expect("tools/list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].description.as_deref(), Some("echo a string"));
    assert_eq!(tools[0].input_schema["properties"]["s"]["type"], "string");

    let resources = client.list_resources().expect("resources/list");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "file:///a.txt");

    // The negotiated server capabilities survive the translation.
    assert!(client.capabilities().tools.is_some());
    assert!(client.capabilities().resources.is_some());
}

#[test]
fn a_tool_call_round_trips() {
    let seen: Shared = Arc::default();
    let ep = spawn_server(Arc::clone(&seen));
    let mut client = RmcpBuilder::new("mock", &ep, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");
    client.set_tool_meta(json!({"agent/run_id": "r1"}));
    let out = client
        .call_tool("echo", Some(json!({"s": "hi"})))
        .expect("tools/call");
    assert_eq!(out["content"][0]["text"], "echoed");
}

/// A server that offers a long `ttlMs` on every cacheable result, the way the
/// registry does, and counts what actually reached it.
#[derive(Default)]
struct TtlState {
    /// Calls per JSON-RPC method, counted on the server side — the arbiter of
    /// whether a read happened, whatever the client reports.
    calls: Mutex<HashMap<String, usize>>,
    /// Answer `resources/read` with "not found" (a withdrawn instruction).
    fail_reads: AtomicBool,
}

impl TtlState {
    fn calls(&self, method: &str) -> usize {
        self.calls.lock().unwrap().get(method).copied().unwrap_or(0)
    }
}

/// Stops a [`spawn_ttl_server`]: afterwards nothing listens on its port, so a
/// request is refused rather than answered.
struct Stopper {
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
}

impl Stopper {
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // `accept` blocks; one dial wakes it so the loop sees the flag, breaks,
        // and drops the listener.
        let _ = TcpStream::connect(self.addr);
        // The loop is gone once the port refuses.
        for _ in 0..100 {
            if TcpStream::connect(self.addr).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the server on {} did not stop", self.addr);
    }
}

/// An hour, as the server's own offer of reuse — far past any test.
const TTL: u64 = 3_600_000;

fn spawn_ttl_server(state: Arc<TtlState>) -> (String, Stopper) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            if flag.load(Ordering::SeqCst) {
                break;
            }
            let Ok(conn) = conn else { continue };
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some((start, body)) = read_http(&mut r) else {
                    return;
                };
                if start.starts_with("GET") || start.starts_with("DELETE") {
                    let _ = w
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                if msg.get("id").is_none() {
                    let _ = w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let id = msg["id"].clone();
                let n = {
                    let mut calls = state.calls.lock().unwrap();
                    let n = calls.entry(method.to_string()).or_default();
                    *n += 1;
                    *n
                };
                let result = match method {
                    "initialize" => json!({
                        "protocolVersion": msg["params"]["protocolVersion"],
                        "capabilities": {"tools": {}, "resources": {"subscribe": true}, "prompts": {}},
                        "serverInfo": {"name": "ttl", "version": "0"}
                    }),
                    "resources/read" if state.fail_reads.load(Ordering::SeqCst) => {
                        respond(
                            &mut w,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": {"code": -32002, "message": "Resource not found"}
                            }),
                        );
                        return;
                    }
                    // The text changes with every read, so a client that shows
                    // the previous text did not ask.
                    "resources/read" => json!({
                        "contents": [{"uri": msg["params"]["uri"], "mimeType": "text/plain", "text": format!("v{n}")}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    "tools/list" => json!({
                        "tools": [{"name": "echo", "inputSchema": {"type": "object"}}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    "resources/list" => json!({
                        "resources": [{"uri": "file:///a.txt", "name": "a"}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    "prompts/list" => json!({
                        "prompts": [{"name": "p"}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    "resources/templates/list" => json!({
                        "resourceTemplates": [{"uriTemplate": "file:///{p}", "name": "t"}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    _ => json!({}),
                };
                respond(
                    &mut w,
                    &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                );
            });
        }
    });
    (format!("http://{addr}/mcp"), Stopper { stop, addr })
}

#[test]
fn every_read_reaches_the_server_even_when_it_offers_a_ttl() {
    // The server offers an hour of reuse on everything. rmcp's default would
    // take it: every call after the first answered from memory, no request on
    // the wire. agentd reads because it needs the current answer, so each call
    // must be one request — counted where it lands, on the server.
    let state: Arc<TtlState> = Arc::default();
    let (ep, _stop) = spawn_ttl_server(Arc::clone(&state));
    let client = RmcpBuilder::new("ttl", &ep, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");

    for i in 1..=3 {
        let read = client
            .read_resource("instruction://ins_x@stable")
            .expect("read");
        assert_eq!(
            read.text(),
            format!("v{i}"),
            "read {i} was not the server's current answer"
        );
    }
    for _ in 0..2 {
        client.list_tools().expect("tools/list");
        client.list_resources().expect("resources/list");
        client.list_prompts().expect("prompts/list");
        client
            .list_resource_templates()
            .expect("resources/templates/list");
    }
    assert_eq!(state.calls("resources/read"), 3);
    assert_eq!(state.calls("tools/list"), 2);
    assert_eq!(state.calls("resources/list"), 2);
    assert_eq!(state.calls("prompts/list"), 2);
    assert_eq!(state.calls("resources/templates/list"), 2);
}

#[test]
fn a_read_against_a_stopped_server_fails_instead_of_replaying_the_last_answer() {
    // The §7.7 freshness watch rests on this: a registry that is gone must be
    // a failed read, or `unavailable` never fires. rmcp's default serves the
    // cached answer while it is fresh and the expired one when a re-fetch
    // fails — either way a dead server "answers".
    let state: Arc<TtlState> = Arc::default();
    let (ep, stopper) = spawn_ttl_server(Arc::clone(&state));
    let client = RmcpBuilder::new("ttl", &ep, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");
    let uri = "instruction://ins_x@stable";
    assert_eq!(client.read_resource(uri).expect("read").text(), "v1");
    client.list_tools().expect("tools/list");

    stopper.stop();

    match client.read_resource(uri) {
        Err(mcp::client::McpError::Transport(m)) => {
            assert!(m.contains(&format!("resources/read {uri}")), "{m}")
        }
        other => panic!("a stopped server answered resources/read: {other:?}"),
    }
    match client.list_tools() {
        Err(mcp::client::McpError::Transport(m)) => assert!(m.contains("tools/list"), "{m}"),
        other => panic!("a stopped server answered tools/list: {other:?}"),
    }
}

#[test]
fn a_refusal_from_the_server_is_not_masked_by_an_earlier_answer() {
    // A withdrawn instruction: the registry is up and says "not found". That
    // is the strongest signal there is, and a cache that answers with the
    // last good copy turns it into a confirmation.
    let state: Arc<TtlState> = Arc::default();
    let (ep, _stop) = spawn_ttl_server(Arc::clone(&state));
    let client = RmcpBuilder::new("ttl", &ep, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");
    let uri = "instruction://ins_x@stable";
    assert_eq!(client.read_resource(uri).expect("read").text(), "v1");

    state.fail_reads.store(true, Ordering::SeqCst);

    let err = client
        .read_resource(uri)
        .expect_err("the server refused, the client must say so");
    assert!(err.to_string().contains("Resource not found"), "{err}");
    assert_eq!(state.calls("resources/read"), 2);
}
