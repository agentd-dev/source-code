// SPDX-License-Identifier: Apache-2.0
//! A minimal built-in **Streamable HTTP** MCP server, for tests and for operators
//! kicking the tyres on reactive setups. Hidden mode:
//! `agentd --internal-mock-mcp-http <addr-file> <uri> [--no-emit]`.
//!
//! Binds a **loopback TCP** listener on `127.0.0.1:0` and writes the bound
//! `host:port` into `<addr-file>` (atomically: tmp + rename;
//! [`crate::announce_addr`]) so the launching harness discovers the endpoint by
//! waiting for the file, then hands agentd `--mcp name=http://<addr>`.
//!
//! It serves one resource at `<uri>` — `initialize` (advertising
//! `resources.subscribe`), `resources/list`, `resources/read`,
//! `resources/subscribe` — over the RFC 0004 Streamable HTTP transport
//! (thread-per-connection, blocking, no dep). After a subscribe it pushes one
//! `notifications/resources/updated` on the long-lived `GET` SSE stream (unless
//! `emit` is off), so a reactive agent reached over HTTP has something to react
//! to.

use crate::json::{self, Incoming, Request, Response};
use crate::wire::mcp::{PROTOCOL_VERSION, method};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Cross-connection server state: a subscribe (on a POST) arms a one-shot push
/// that the open `GET` SSE stream delivers.
struct State {
    uri: String,
    emit: bool,
    pending_emit: AtomicBool,
}

/// Serve the mock on loopback TCP until the process is killed, announcing the
/// bound address through `addr_file`. Returns the process exit code.
pub fn run(addr_file: &str, uri: &str, emit: bool) -> i32 {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("internal-mock-mcp-http: bind 127.0.0.1:0: {e}");
            return 1;
        }
    };
    if let Err(e) = crate::announce_addr(addr_file, &listener) {
        eprintln!("internal-mock-mcp-http: write {addr_file}: {e}");
        return 1;
    }
    let state = Arc::new(State {
        uri: uri.to_string(),
        emit,
        pending_emit: AtomicBool::new(false),
    });
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let state = Arc::clone(&state);
        std::thread::spawn(move || handle_conn(stream, state));
    }
    0
}

/// One HTTP request per connection (the client sends `Connection: close`). A
/// `GET` is the notification SSE stream; a `POST` is one JSON-RPC frame.
fn handle_conn(mut stream: TcpStream, state: Arc<State>) {
    let Some((method_line, body)) = read_http(&stream) else {
        return;
    };
    let is_get = method_line.starts_with("GET ");
    if is_get {
        serve_notifications(&mut stream, &state);
        return;
    }
    // POST: parse the JSON-RPC frame.
    match serde_json::from_slice::<Incoming>(&body) {
        Ok(Incoming::Request(req)) => {
            let (resp, session) = handle_request(req, &state);
            let payload = serde_json::to_value(resp).unwrap_or(serde_json::Value::Null);
            write_json(&mut stream, payload, session);
        }
        // A notification POST (e.g. notifications/initialized) → 202, no body.
        Ok(Incoming::Notification(_)) | Ok(Incoming::Response(_)) | Err(_) => {
            let _ = stream.write_all(
                b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    }
}

/// Build the JSON-RPC response for one request. Returns the response and whether
/// to stamp the `Mcp-Session-Id` header (on `initialize`).
fn handle_request(req: Request, state: &State) -> (Response, bool) {
    let uri = &state.uri;
    match req.method.as_str() {
        "initialize" => (
            Response::ok(
                req.id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"resources": {"subscribe": true, "listChanged": true}, "tools": {}},
                    "serverInfo": {"name": "agentd-mock-http", "version": crate::VERSION}
                }),
            ),
            true,
        ),
        "ping" => (Response::ok(req.id, json!({})), false),
        "tools/list" => (Response::ok(req.id, json!({"tools": []})), false),
        "resources/list" => (
            Response::ok(req.id, json!({"resources": [{"uri": uri, "name": "mock"}]})),
            false,
        ),
        "resources/read" => (
            Response::ok(
                req.id,
                json!({"contents": [{"uri": uri, "mimeType": "text/plain", "text": "the watched resource changed"}]}),
            ),
            false,
        ),
        "resources/unsubscribe" => (Response::ok(req.id, json!({})), false),
        "resources/subscribe" => {
            // Arm the one-shot push the GET SSE stream will deliver.
            if state.emit {
                state.pending_emit.store(true, Ordering::SeqCst);
            }
            (Response::ok(req.id, json!({})), false)
        }
        other => (
            Response::err(
                req.id,
                json::METHOD_NOT_FOUND,
                format!("unsupported: {other}"),
            ),
            false,
        ),
    }
}

/// The long-lived `GET` SSE stream: hold it open and deliver the one-shot
/// `resources/updated` armed by a subscribe. Deliberately sends NO keep-alive
/// comments — the client polls its stop flag via a read timeout between events,
/// and a stream of comments would keep its SSE reader busy and defeat that. The
/// thread loops until the process exits (a test mock; the harness reaps it).
fn serve_notifications(stream: &mut TcpStream, state: &State) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();
    loop {
        if state.pending_emit.swap(false, Ordering::SeqCst) {
            let note = json::Notification::new(
                method::NOTIFY_RESOURCES_UPDATED,
                Some(json!({"uri": state.uri})),
            );
            let data = serde_json::to_string(&note).unwrap_or_default();
            if stream
                .write_all(format!("data: {data}\n\n").as_bytes())
                .is_err()
            {
                return;
            }
            let _ = stream.flush();
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Read one HTTP request (request line, headers, Content-Length body) off a
/// clone of `stream`. Returns `(request_line, body)` — headers beyond
/// Content-Length are unused by the mock.
fn read_http(stream: &TcpStream) -> Option<(String, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some((request_line, body))
}

/// Write an `application/json` HTTP response carrying `payload`, optionally
/// stamping the `Mcp-Session-Id` header.
fn write_json(stream: &mut TcpStream, payload: serde_json::Value, session: bool) {
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let session_hdr = if session {
        "Mcp-Session-Id: mock\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session_hdr}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}
