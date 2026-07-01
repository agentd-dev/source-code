// SPDX-License-Identifier: Apache-2.0
//! A minimal built-in **Streamable HTTP** MCP server, for tests and for operators
//! kicking the tyres on reactive setups. Hidden mode:
//! `agentd --internal-mock-mcp-http <unix-socket> <uri> [--no-emit]`.
//!
//! The v2.0.0 counterpart of the stdio [`super::mock`]: it serves the same one
//! resource at `<uri>` — `initialize` (advertising `resources.subscribe`),
//! `resources/list`, `resources/read`, `resources/subscribe` — over the RFC 0004
//! Streamable HTTP transport on a unix socket (thread-per-connection, blocking,
//! no dep). After a subscribe it pushes one `notifications/resources/updated`
//! on the long-lived `GET` SSE stream (unless `emit` is off), so a reactive agent
//! reached over HTTP has something to react to.

use crate::json::{self, Incoming, Request, Response};
use crate::wire::mcp::{PROTOCOL_VERSION, method};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
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

/// Serve the mock over the unix socket at `socket` (a bare path or `unix:PATH`)
/// until the process is killed. Returns the process exit code.
pub fn run(socket: &str, uri: &str, emit: bool) -> i32 {
    let path = socket.strip_prefix("unix:").unwrap_or(socket);
    let _ = std::fs::remove_file(path); // clear a stale socket
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("internal-mock-mcp-http: bind {path}: {e}");
            return 1;
        }
    };
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
fn handle_conn(mut stream: UnixStream, state: Arc<State>) {
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
            let _ = stream
                .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
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
            Response::err(req.id, json::METHOD_NOT_FOUND, format!("unsupported: {other}")),
            false,
        ),
    }
}

/// The long-lived `GET` SSE stream: hold it open and deliver the one-shot
/// `resources/updated` armed by a subscribe. Exits when the peer closes.
fn serve_notifications(stream: &mut UnixStream, state: &State) {
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
            if stream.write_all(format!("data: {data}\n\n").as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
        } else {
            // Keep-alive comment; also how we detect a closed peer (write fails).
            if stream.write_all(b":\n\n").is_err() {
                return;
            }
            let _ = stream.flush();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Read one HTTP request (request line, headers, Content-Length body) off a
/// clone of `stream`. Returns `(request_line, body)` — headers beyond
/// Content-Length are unused by the mock.
fn read_http(stream: &UnixStream) -> Option<(String, Vec<u8>)> {
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
fn write_json(stream: &mut UnixStream, payload: serde_json::Value, session: bool) {
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let session_hdr = if session { "Mcp-Session-Id: mock\r\n" } else { "" };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session_hdr}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}
