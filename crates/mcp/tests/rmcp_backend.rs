// SPDX-License-Identifier: Apache-2.0
//! The official-SDK backend against a real server.
//!
//! The point of adopting [`rmcp`] is inheriting spec-tracking from upstream, so
//! these tests assert on the things that would silently regress if the facade
//! were wired wrong: that we ask for the NEWEST protocol revision rather than
//! rmcp's conservative default, that the declared capabilities reach the
//! handshake, and that tools/resources come back in agentd's own wire types.
#![cfg(feature = "rmcp-client")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcp::inbound::{Answer, Handler, Inbound};
use mcp::rmcp_client::RmcpBuilder;
use serde_json::{Value, json};

#[derive(Default)]
struct Seen {
    init: Option<Value>,
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
                match method {
                    "initialize" => {
                        seen.lock().unwrap().init = Some(msg["params"].clone());
                        respond(
                            &mut w,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": {
                                    "protocolVersion": "2026-07-28",
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
fn the_handshake_asks_for_the_newest_revision_not_rmcps_default() {
    // rmcp's ProtocolVersion::LATEST is 2025-11-25. Adopting the SDK must not
    // silently give up the newer stateless revision this crate targets.
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
    assert_eq!(init["protocolVersion"], "2026-07-28", "handshake: {init}");
    assert_eq!(client.protocol_version(), Some("2026-07-28"));
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
