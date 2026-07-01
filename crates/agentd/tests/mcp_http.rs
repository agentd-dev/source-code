// SPDX-License-Identifier: Apache-2.0
//! End-to-end test of the Streamable HTTP MCP client (RFC 0004, v2.0.0) against a
//! mock HTTP-MCP server on a loopback TCP socket. Exercises the full lifecycle —
//! connect → initialize (capturing `Mcp-Session-Id`) → tools/list (application/json
//! response) → tools/call (SSE response with an interleaved notification) →
//! resources/read — with no process spawned, proving the transport end to end.

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
    methods: Vec<String>,
}

/// One parsed HTTP request: the JSON-RPC body + the `Mcp-Session-Id` header.
struct HttpReq {
    session_id: Option<String>,
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
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            } else if key == "mcp-session-id" {
                session_id = Some(val);
            }
        }
    }
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf).ok()?;
    let body: Value = serde_json::from_slice(&buf).ok()?;
    Some(HttpReq { session_id, body })
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

#[test]
fn streamable_http_full_lifecycle() {
    let (endpoint, seen) = spawn_mock();

    let mut client =
        McpClient::connect("mock", &endpoint, Vec::new(), Duration::from_secs(5)).expect("connect");
    client.initialize().expect("initialize handshake");

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

    // The notification carried on the SSE response was captured.
    let notes = client.drain_notifications();
    assert_eq!(notes.len(), 1, "one resources/updated captured");
    assert_eq!(notes[0].method, "notifications/resources/updated");

    // resources/read round-trips.
    let read = client.read_resource("mock://res").expect("resources/read");
    assert_eq!(read.contents.len(), 1);

    // The server-assigned session id was echoed on every post-initialize request.
    let g = seen.lock().unwrap();
    let init_idx = g.methods.iter().position(|m| m == "initialize").unwrap();
    for (i, sid) in g.session_ids.iter().enumerate() {
        if i > init_idx {
            assert_eq!(
                sid.as_deref(),
                Some("sess-1"),
                "request #{i} ({}) must echo the session id",
                g.methods[i]
            );
        }
    }
}

#[test]
fn notification_get_stream_delivers_server_pushes() {
    // The built-in HTTP mock (debug/internal-mocks) serves the reactive
    // one-resource MCP over a unix socket and pushes a resources/updated on the
    // GET SSE stream after a subscribe. Prove agentd's notification thread
    // receives it (the reactive-over-HTTP push channel).
    let sock = format!(
        "/tmp/agentd-mcp-notify-{}-{}.sock",
        std::process::id(),
        line!()
    );
    let sock_thread = sock.clone();
    std::thread::spawn(move || {
        agentd::mcp::mock_http::run(&sock_thread, "mock://res", true);
    });
    // Wait for the socket to appear.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !std::path::Path::new(&sock).exists() {
        assert!(std::time::Instant::now() < deadline, "mock socket never bound");
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut client = McpClient::connect(
        "mock",
        &format!("unix:{sock}"),
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
    assert_eq!(got.len(), 1, "one resources/updated pushed over the GET stream");
    assert_eq!(got[0].method, "notifications/resources/updated");

    // Dropping the client stops the notification thread cleanly.
    drop(client);
    let _ = std::fs::remove_file(&sock);
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
    assert!(err.is_err(), "initialize against a dead endpoint must error");
}
