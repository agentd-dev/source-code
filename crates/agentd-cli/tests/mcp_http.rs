// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end test of the Streamable HTTP MCP client against a mock HTTP-MCP
//! server on a loopback TCP socket. Exercises the full lifecycle — connect →
//! initialize (capturing `Mcp-Session-Id`) → tools/list (application/json
//! response) → tools/call (SSE response with an interleaved notification) →
//! resources/read — with no process spawned, proving the transport end to end.
//!
//! These drive a real `McpClient` — the official SDK over agentd's own socket —
//! at a mock server on a real port, and assert from the *server's* side, which
//! is the only side that can tell whether the wire is right.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use agentd::mcp::client::McpClient;
use serde_json::{Value, json};

/// What the mock observed, so the test can assert protocol-level behaviour
/// (e.g. the session id was echoed after `initialize`).
#[derive(Default)]
struct Seen {
    session_ids: Vec<Option<String>>,
    protocol_versions: Vec<Option<String>>,
    methods: Vec<String>,
}

/// One parsed HTTP request: the JSON-RPC body + the routing/framing headers.
#[derive(Clone)]
struct HttpReq {
    session_id: Option<String>,
    protocol_version: Option<String>,
    body: Value,
}

fn read_http_request(stream: &TcpStream) -> Option<HttpReq> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    // Request line.
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut content_length = 0usize;
    let mut session_id = None;
    let mut protocol_version = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            match key.as_str() {
                "content-length" => content_length = val.parse().unwrap_or(0),
                "mcp-session-id" => session_id = Some(val),
                "mcp-protocol-version" => protocol_version = Some(val),
                _ => {}
            }
        }
    }
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf).ok()?;
    let body: Value = serde_json::from_slice(&buf).ok()?;
    Some(HttpReq {
        session_id,
        protocol_version,
        body,
    })
}

fn write_json(stream: &mut TcpStream, extra_header: &str, payload: &Value) {
    let body = serde_json::to_vec(payload).unwrap();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// Write a `text/event-stream` response: a leading notification event, then the
/// JSON-RPC response event. Exercises the SSE path + notification capture.
fn write_sse(stream: &mut TcpStream, notification: &Value, response: &Value) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(format!("data: {notification}\n\n").as_bytes());
    let _ = stream.write_all(format!("data: {response}\n\n").as_bytes());
    let _ = stream.flush();
}

fn accepted_notification(uri: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/resources/updated",
        "params": {"uri": uri}
    })
}

/// Spawn a mock HTTP-MCP server; returns its `http://…/mcp` endpoint and the
/// shared observation log. The server handles one request per connection (the
/// client sends `Connection: close` and opens a fresh connection per request).
fn spawn_mock() -> (String, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let seen_thread = Arc::clone(&seen);
    let uri = "mock://res";

    thread::spawn(move || {
        for conn in listener.incoming() {
            let mut stream = match conn {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Some(req) = read_http_request(&stream) else {
                continue;
            };
            let method = req.body["method"].as_str().unwrap_or("").to_string();
            let id = req.body.get("id").cloned().unwrap_or(Value::Null);
            {
                let mut g = seen_thread.lock().unwrap();
                g.session_ids.push(req.session_id.clone());
                g.protocol_versions.push(req.protocol_version.clone());
                g.methods.push(method.clone());
            }
            match method.as_str() {
                "initialize" => {
                    let payload = json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"resources": {"subscribe": true, "listChanged": true}, "tools": {}},
                            "serverInfo": {"name": "mock-http", "version": "0"}
                        }
                    });
                    // Assign a session the client must echo on later requests.
                    write_json(&mut stream, "Mcp-Session-Id: sess-1\r\n", &payload);
                }
                // A notification POST is acknowledged with 202 and no body.
                "notifications/initialized" => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                "tools/list" => {
                    let payload = json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"tools": [{"name": "echo", "description": "echo", "inputSchema": {"type": "object"}}]}
                    });
                    write_json(&mut stream, "", &payload);
                }
                "tools/call" => {
                    // Respond over SSE, preceded by a resources/updated notification.
                    let response = json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": "pong"}], "isError": false}
                    });
                    write_sse(&mut stream, &accepted_notification(uri), &response);
                }
                "resources/read" => {
                    let payload = json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"contents": [{"uri": uri, "mimeType": "text/plain", "text": "hello"}]}
                    });
                    write_json(&mut stream, "", &payload);
                }
                "resources/subscribe" => {
                    let payload = json!({"jsonrpc": "2.0", "id": id, "result": {}});
                    write_json(&mut stream, "", &payload);
                }
                _ => {
                    let payload = json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}
                    });
                    write_json(&mut stream, "", &payload);
                }
            }
        }
    });

    (endpoint, seen)
}

// Scope: the handshake dialect belongs to the official SDK, which pins its
// LATEST to the legacy revision — agentd speaks whatever revision the SDK
// speaks, and gains the stateless (MODERN) one when the SDK does. Exercising a
// dialect agentd does not implement would be testing the mocks, not agentd.
//
// So what these cover is the part that is agentd's own: that a real server's
// lifecycle works end to end over agentd's own credentialed transport, that
// server-pushed notifications reach the reactor (the reactive wake), and that a
// dead endpoint fails as a transport error rather than a hang.

#[test]
fn streamable_http_full_lifecycle() {
    let (endpoint, seen) = spawn_mock();

    let mut client =
        McpClient::connect("mock", &endpoint, Vec::new(), Duration::from_secs(5)).expect("connect");
    client.initialize().expect("initialize handshake");

    // Version negotiation: the client advertised its latest (2025-11-25) but the
    // server responded with 2025-06-18 — the client must ADOPT the server's choice.
    assert_eq!(
        client.protocol_version(),
        Some("2025-06-18"),
        "the client adopts the version the server negotiated"
    );

    // Capabilities were parsed from the initialize result.
    assert!(client.capabilities().supports_tools(), "tools advertised");
    assert!(
        client.capabilities().supports_subscribe(),
        "resources.subscribe advertised"
    );

    // tools/list over an application/json response.
    let tools = client.list_tools().expect("tools/list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    // tools/call over an SSE response, with an interleaved notification.
    let result = client
        .call_tool("echo", Some(json!({"msg": "hi"})))
        .expect("tools/call");
    assert!(!result.is_error(), "call is not a tool-domain error");

    // The notification carried on the SSE response is captured — but WAIT for
    // it rather than demanding it has already landed. `drain_notifications`
    // returns what the SDK's reader task has queued so far, and that task is
    // not synchronised with the return of an unrelated `tools/call`: the
    // notification is delivered, just not necessarily before this line runs.
    // The failure this guards against: asserting immediately encodes a
    // happens-before the transport never promised, and flakes roughly one run in
    // six locally and more often on a loaded CI runner. What the reactive path
    // actually needs is that the wake ARRIVES, which is what this waits for.
    let mut notes = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while notes.is_empty() && std::time::Instant::now() < deadline {
        notes.extend(client.drain_notifications());
        if notes.is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(notes.len(), 1, "one resources/updated captured");
    assert_eq!(notes[0].method, "notifications/resources/updated");

    // resources/read round-trips.
    let read = client.read_resource("mock://res").expect("resources/read");
    assert_eq!(read.contents.len(), 1);

    // The session id AND the negotiated MCP-Protocol-Version are echoed on every
    // post-initialize request; the `initialize` request itself carries neither
    // (nothing agreed yet — the version header is a "subsequent request" MUST).
    let g = seen.lock().unwrap();
    let init_idx = g.methods.iter().position(|m| m == "initialize").unwrap();
    assert_eq!(
        g.protocol_versions[init_idx], None,
        "the initialize request must NOT carry MCP-Protocol-Version"
    );
    for (i, method) in g.methods.iter().enumerate() {
        if i > init_idx {
            assert_eq!(
                g.session_ids[i].as_deref(),
                Some("sess-1"),
                "request #{i} ({method}) must echo the session id"
            );
            assert_eq!(
                g.protocol_versions[i].as_deref(),
                Some("2025-06-18"),
                "request #{i} ({method}) must carry the negotiated MCP-Protocol-Version"
            );
        }
    }
}

#[test]
fn notification_get_stream_delivers_server_pushes() {
    // The built-in HTTP mock (debug/internal-mocks) serves the reactive
    // one-resource MCP over loopback TCP (announcing through an addr-file) and
    // pushes a resources/updated on the GET SSE stream after a subscribe. Prove
    // agentd's notification thread receives it (the reactive-over-HTTP push
    // channel).
    let addr_file = format!(
        "/tmp/agentd-mcp-notify-{}-{}.addr",
        std::process::id(),
        line!()
    );
    let addr_file_thread = addr_file.clone();
    std::thread::spawn(move || {
        agentd::mcp::mock_http::run(&addr_file_thread, "mock://res", true);
    });
    // Wait for the address announcement.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !std::path::Path::new(&addr_file).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "mock never announced its address"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let addr = std::fs::read_to_string(&addr_file).expect("read mock addr-file");

    let mut client = McpClient::connect(
        "mock",
        &format!("http://{}", addr.trim()),
        Vec::new(),
        Duration::from_secs(5),
    )
    .expect("connect");
    client.initialize().expect("initialize");
    assert!(client.capabilities().supports_subscribe());
    client.subscribe("mock://res").expect("subscribe");

    // Poll for the pushed notification (delivered on the GET SSE stream).
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut got = Vec::new();
    while got.is_empty() && std::time::Instant::now() < deadline {
        got = client.drain_notifications();
        if got.is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(
        got.len(),
        1,
        "one resources/updated pushed over the GET stream"
    );
    assert_eq!(got[0].method, "notifications/resources/updated");

    // Dropping the client stops the notification thread cleanly.
    drop(client);
    let _ = std::fs::remove_file(&addr_file);
}

#[test]
fn client_prompts_and_completions() {
    // A server advertising prompts + completions; exercise list_prompts /
    // get_prompt / complete (era-agnostic — this mock is legacy).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let Some(req) = read_http_request(&stream) else {
                continue;
            };
            let method = req.body["method"].as_str().unwrap_or("");
            let id = req.body.get("id").cloned().unwrap_or(Value::Null);
            let payload = match method {
                "initialize" => json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":"2025-11-25",
                    "capabilities":{"prompts":{},"completions":{}},
                    "serverInfo":{"name":"p","version":"0"}}}),
                "prompts/list" => json!({"jsonrpc":"2.0","id":id,"result":{
                    "prompts":[{"name":"greet","arguments":[{"name":"who","required":true}]}]}}),
                "prompts/get" => json!({"jsonrpc":"2.0","id":id,"result":{
                    "description":"greeting",
                    "messages":[{"role":"user","content":{"type":"text","text":"Hello!"}}]}}),
                "completion/complete" => json!({"jsonrpc":"2.0","id":id,"result":{
                    "completion":{"values":["alice","alan"],"hasMore":false}}}),
                _ => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"nope"}}),
            };
            write_json(&mut stream, "", &payload);
        }
    });

    let mut client =
        McpClient::connect("p", &endpoint, Vec::new(), Duration::from_secs(5)).expect("connect");
    client.initialize().expect("initialize");
    assert!(client.capabilities().supports_prompts());
    assert!(client.capabilities().supports_completions());

    let prompts = client.list_prompts().expect("prompts/list");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name, "greet");
    assert_eq!(prompts[0].arguments[0].required, Some(true));

    let got = client
        .get_prompt("greet", Some(json!({"who": "world"})))
        .expect("prompts/get");
    assert_eq!(got.messages.len(), 1);
    assert_eq!(got.description.as_deref(), Some("greeting"));

    let comp = client
        .complete(
            json!({"type": "ref/prompt", "name": "greet"}),
            json!({"name": "who", "value": "al"}),
        )
        .expect("completion/complete");
    assert_eq!(comp.completion.values, ["alice", "alan"]);
}

#[test]
fn connect_to_dead_endpoint_surfaces_transport_error() {
    // Nothing is listening on this port; initialize must fail fast, not hang.
    let mut client = McpClient::connect(
        "dead",
        "http://127.0.0.1:1/mcp",
        Vec::new(),
        Duration::from_millis(500),
    )
    .expect("connect is lazy — no dial yet");
    let err = client.initialize();
    assert!(
        err.is_err(),
        "initialize against a dead endpoint must error"
    );
}
