// SPDX-License-Identifier: Apache-2.0
//! A minimal blocking **Streamable HTTP** MCP server over **loopback TCP**, shared
//! by the conformance MCP servers (`confmcp`, `workmcp`). agentd's client connects
//! to these over `http://<addr>` (no stdio spawn). Thread-per-connection,
//! dependency-light (serde_json + std) — deliberately independent of the agentd
//! library so the servers stay a spec-correct external reference.
//!
//! Discovery handshake: the server binds `127.0.0.1:0` and writes the bound
//! `host:port` into an **addr-file** (atomically: tmp + rename) so the launching
//! harness waits for the file, reads the address, and hands agentd
//! `http://<addr>`.
//!
//! Per connection: a `GET` is the long-lived server→client notification SSE stream
//! (draining a shared queue a handler pushes to); a `POST` is one JSON-RPC frame
//! handed to the caller's `handle` closure, which returns the response Value to
//! send (or `None` for a `202` no-reply, e.g. a client notification).

use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A handle a request handler uses to push server→client notifications onto the
/// GET SSE stream. Cheap to clone (shared queue).
#[derive(Clone)]
pub struct Notifier(Arc<Mutex<VecDeque<String>>>);

impl Notifier {
    /// Queue a JSON-RPC notification for delivery on the GET SSE stream.
    pub fn push(&self, note: Value) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(note.to_string());
    }
}

/// Serve HTTP MCP over loopback TCP, announcing the bound address through
/// `addr_file`, forever. For each `POST`, `handle(&request, &notifier)` returns
/// the full JSON-RPC response Value to send, or `None` for a `202` no-reply.
pub fn serve<H>(addr_file: &str, handle: H) -> !
where
    H: Fn(&Value, &Notifier) -> Option<Value> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .unwrap_or_else(|e| panic!("mcp_http_server: bind 127.0.0.1:0: {e}"));
    let addr = listener.local_addr().expect("local_addr").to_string();
    // Announce the bound address atomically (tmp + rename) so the harness never
    // reads a half-written file.
    let tmp = format!("{addr_file}.tmp");
    std::fs::write(&tmp, &addr).unwrap_or_else(|e| panic!("mcp_http_server: write {tmp}: {e}"));
    std::fs::rename(&tmp, addr_file)
        .unwrap_or_else(|e| panic!("mcp_http_server: rename to {addr_file}: {e}"));
    let notes = Arc::new(Mutex::new(VecDeque::new()));
    let handle = Arc::new(handle);
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let notes = Arc::clone(&notes);
        let handle = Arc::clone(&handle);
        std::thread::spawn(move || conn_loop(stream, handle, Notifier(notes)));
    }
    unreachable!("incoming() is an infinite iterator")
}

fn conn_loop<H>(mut stream: TcpStream, handle: Arc<H>, notifier: Notifier)
where
    H: Fn(&Value, &Notifier) -> Option<Value>,
{
    let Some((line, body)) = read_http(&stream) else {
        return;
    };
    if line.starts_with("GET ") {
        serve_notifications(&mut stream, &notifier);
        return;
    }
    // POST: one JSON-RPC frame.
    let Ok(req) = serde_json::from_slice::<Value>(&body) else {
        let _ = stream
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    match handle(&req, &notifier) {
        Some(resp) => write_json(&mut stream, &resp),
        None => {
            let _ = stream.write_all(
                b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    }
}

/// Hold the GET SSE stream open, delivering queued notifications. Sends no
/// keep-alive comments (the client polls its stop flag via a read timeout).
fn serve_notifications(stream: &mut TcpStream, notifier: &Notifier) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();
    loop {
        let next = notifier
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        if let Some(data) = next {
            if stream
                .write_all(format!("data: {data}\n\n").as_bytes())
                .is_err()
            {
                return;
            }
            let _ = stream.flush();
        } else {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// Read one HTTP request off a clone of `stream`; return `(request_line, body)`.
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

/// A one-shot Streamable-HTTP JSON-RPC `POST` over loopback TCP — a test client
/// for driving a server directly (the work-claim protocol probe). `endpoint` is
/// `http://host:port` (or a bare `host:port`). Returns the parsed JSON-RPC
/// response Value. Panics on any I/O error (a conformance probe).
pub fn post(endpoint: &str, req: &Value) -> Value {
    let addr = endpoint.strip_prefix("http://").unwrap_or(endpoint);
    let mut stream = TcpStream::connect(addr)
        .unwrap_or_else(|e| panic!("mcp_http_server::post: connect {addr}: {e}"));
    let body = serde_json::to_vec(req).expect("serialize request");
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    stream.write_all(&body).expect("write body");
    stream.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).expect("read status");
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("read header") == 0 {
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
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf).expect("read body");
    serde_json::from_slice(&buf).unwrap_or(Value::Null)
}

fn write_json(stream: &mut TcpStream, payload: &Value) {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}
