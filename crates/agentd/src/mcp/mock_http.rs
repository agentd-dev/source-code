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
/// that the open `GET` SSE stream delivers. The mock also implements the RFC
/// 0021 §8.3 / RFC 0025 §4.1 **checkpointer tool profile** (`state.put` /
/// `state.get` / `state.list` / `state.delete` over an in-memory per-key history
/// with the monotonic-seq guard) plus a `flaky` tool (fails on its first call,
/// succeeds after) and a `mock.fault` control tool (fail the next N state calls)
/// — together they let the e2e + chaos suites prove crash → restore → complete
/// with no external infrastructure.
struct State {
    uri: String,
    emit: bool,
    pending_emit: AtomicBool,
    /// The checkpointer store: key → (seq → envelope). Monotonic per key.
    store: std::sync::Mutex<
        std::collections::BTreeMap<String, std::collections::BTreeMap<u64, serde_json::Value>>,
    >,
    /// `flaky` call counter (first call errors, later ones succeed).
    flaky_calls: std::sync::atomic::AtomicU64,
    /// Fault injection: remaining `state.*` calls to fail with a tool error.
    fail_next: std::sync::atomic::AtomicU64,
    /// Every `state.*` call performed (tool name) — `mock.ops` reports it.
    ops: std::sync::Mutex<Vec<String>>,
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
        store: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        flaky_calls: std::sync::atomic::AtomicU64::new(0),
        fail_next: std::sync::atomic::AtomicU64::new(0),
        ops: std::sync::Mutex::new(Vec::new()),
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
                    "capabilities": {"resources": {"subscribe": true, "listChanged": true}, "tools": {}, "prompts": {"listChanged": true}},
                    "serverInfo": {"name": "agentd-mock-http", "version": crate::VERSION}
                }),
            ),
            true,
        ),
        "ping" => (Response::ok(req.id, json!({})), false),
        "tools/list" => (
            Response::ok(
                req.id,
                json!({"tools": [
                    {"name": "state.put", "description": "checkpointer put", "inputSchema": {"type": "object"}},
                    {"name": "state.get", "description": "checkpointer get", "inputSchema": {"type": "object"}},
                    {"name": "state.list", "description": "checkpointer list", "inputSchema": {"type": "object"}},
                    {"name": "state.delete", "description": "checkpointer delete", "inputSchema": {"type": "object"}},
                    {"name": "flaky", "description": "fails once, then succeeds", "inputSchema": {"type": "object"}},
                    {"name": "mock.fault", "description": "fail the next N state.* calls", "inputSchema": {"type": "object"}},
                    {"name": "mock.ops", "description": "the state.* calls performed so far", "inputSchema": {"type": "object"}},
                    {"name": "knowledge.search", "description": "RAG search over the mock corpus", "inputSchema": {"type": "object"}},
                    {"name": "knowledge.get", "description": "fetch a mock document", "inputSchema": {"type": "object"}},
                    {"name": "knowledge.list", "description": "list mock documents", "inputSchema": {"type": "object"}},
                    {"name": "search.query", "description": "mock web search", "inputSchema": {"type": "object"}},
                    {"name": "search.fetch", "description": "mock page fetch", "inputSchema": {"type": "object"}},
                ]}),
            ),
            false,
        ),
        "tools/call" => (handle_tool_call(req, state), false),
        "resources/list" => (
            Response::ok(
                req.id,
                json!({"resources": [
                    {"uri": uri, "name": "mock"},
                    {"uri": "skill://incident-runbook", "name": "incident-runbook", "description": "Handle a production incident. When to use: an alert or outage report", "mimeType": "text/x-skill+markdown"},
                    {"uri": "mock://instruction", "name": "instruction", "mimeType": "text/plain"}
                ]}),
            ),
            false,
        ),
        "resources/read" => {
            let asked = req
                .params
                .as_ref()
                .and_then(|p| p.get("uri"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let (mime, text) = match asked.as_str() {
                "skill://incident-runbook" => ("text/x-skill+markdown", "# Incident runbook\n1. Acknowledge the alert. 2. Find the blast radius. 3. Mitigate first, root-cause later. 4. Write the timeline.".to_string()),
                "mock://instruction" => ("text/plain", "You are the mock-served agent. Follow the served instruction.".to_string()),
                _ => ("text/plain", "the watched resource changed".to_string()),
            };
            let uri_out = if asked.is_empty() { uri.clone() } else { asked };
            (
                Response::ok(
                    req.id,
                    json!({"contents": [{"uri": uri_out, "mimeType": mime, "text": text}]}),
                ),
                false,
            )
        }
        // Skills as prompts (RFC 0028 §7): the catalogue + a body per skill.
        "prompts/list" => (
            Response::ok(
                req.id,
                json!({"prompts": [
                    {"name": "review-pr", "description": "Review a pull request thoroughly. When to use: any code review request", "arguments": [{"name": "target", "description": "What to review", "required": false}]},
                    {"name": "deploy-safely", "description": "Deploy with a rollback plan"}
                ]}),
            ),
            false,
        ),
        "prompts/get" => {
            let params = req.params.clone().unwrap_or(json!({}));
            let name = params
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let target = params
                .get("arguments")
                .and_then(|a| a.get("target"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the change");
            let body = match name {
                "review-pr" => format!(
                    "# Skill: review-pr\nReview {target}: read the diff, check tests, look for security issues, summarize findings as bullets."
                ),
                "deploy-safely" => {
                    "# Skill: deploy-safely\nAlways deploy behind a flag with a rollback plan."
                        .to_string()
                }
                _ => {
                    return (
                        Response::err(
                            req.id,
                            json::INVALID_PARAMS,
                            format!("no such prompt: {name}"),
                        ),
                        false,
                    );
                }
            };
            (
                Response::ok(
                    req.id,
                    json!({"description": "skill body", "messages": [{"role": "user", "content": {"type": "text", "text": body}}]}),
                ),
                false,
            )
        }
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

/// One MCP `tools/call`: the RFC 0021 §8.3 / RFC 0025 §4.1 checkpointer profile
/// plus `flaky` and the `mock.*` controls. A tool result is standard MCP content:
/// one text part carrying the JSON **and** the same JSON as `structuredContent`
/// (the modern shape — the store adapter's default mapping reads
/// `result.structuredContent.*`, falling back to the text part).
fn handle_tool_call(req: Request, state: &State) -> Response {
    fn tool_ok(id: json::Id, v: serde_json::Value) -> Response {
        Response::ok(
            id,
            json!({"content": [{"type": "text", "text": v.to_string()}], "structuredContent": v, "isError": false}),
        )
    }
    fn tool_err(id: json::Id, msg: &str) -> Response {
        Response::ok(
            id,
            json!({"content": [{"type": "text", "text": msg}], "isError": true}),
        )
    }
    let params = req.params.clone().unwrap_or(json!({}));
    let name = params.get("name").and_then(serde_json::Value::as_str);
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let key = || {
        args.get("key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    if let Some(n) = name
        && n.starts_with("state.")
    {
        state
            .ops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n.to_string());
        // Fault injection armed by `mock.fault`: the next N state calls fail.
        let remaining = state.fail_next.load(Ordering::SeqCst);
        if remaining > 0 {
            state.fail_next.store(remaining - 1, Ordering::SeqCst);
            return tool_err(req.id, &format!("injected fault on {n}"));
        }
    }
    match name {
        Some("mock.fault") => {
            let n = args
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1);
            state.fail_next.store(n, Ordering::SeqCst);
            tool_ok(req.id, json!({"ok": true, "count": n}))
        }
        Some("mock.ops") => {
            let ops = state.ops.lock().unwrap_or_else(|e| e.into_inner()).clone();
            tool_ok(req.id, json!({"ops": ops}))
        }
        Some("state.put") => {
            let seq = args
                .get("seq")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let env = args.get("state").cloned().unwrap_or(json!(null));
            let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            let hist = store.entry(key()).or_default();
            let latest = hist.keys().next_back().copied().unwrap_or(0);
            if seq <= latest {
                // The monotonic-seq guard: a stale/duplicate writer is REFUSED
                // (`ok:false` + the latest seq) — the split-brain signal.
                return tool_ok(req.id, json!({"ok": false, "latest": latest}));
            }
            hist.insert(seq, env);
            tool_ok(req.id, json!({"ok": true, "seq": seq}))
        }
        Some("state.get") => {
            let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            match store.get(&key()) {
                None => tool_err(req.id, "no such key"),
                Some(hist) => {
                    let picked = match args.get("seq").and_then(serde_json::Value::as_u64) {
                        Some(seq) => hist.get(&seq),
                        None => hist.values().next_back(),
                    };
                    match picked {
                        Some(env) => tool_ok(req.id, json!({"state": env})),
                        None => tool_err(req.id, "no such seq"),
                    }
                }
            }
        }
        Some("state.list") => {
            let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(prefix) = args.get("prefix").and_then(serde_json::Value::as_str) {
                // RFC 0025 §4.1: every live key under `prefix` with its latest
                // seq (a tombstone — latest state null — is not listed).
                let keys: Vec<serde_json::Value> = store
                    .iter()
                    .filter(|(k, h)| {
                        k.starts_with(prefix)
                            && h.values().next_back().is_some_and(|v| {
                                !v.get("state").is_some_and(serde_json::Value::is_null)
                            })
                    })
                    .map(|(k, h)| json!({"key": k, "seq": h.keys().next_back().copied()}))
                    .collect();
                return tool_ok(req.id, json!({"keys": keys}));
            }
            // RFC 0021 §8.3 (v1 checkpointer): the seqs of ONE key.
            let seqs: Vec<u64> = store
                .get(&key())
                .map(|h| h.keys().copied().collect())
                .unwrap_or_default();
            tool_ok(req.id, json!({"seqs": seqs}))
        }
        Some("state.delete") => {
            let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            let existed = store.remove(&key()).is_some();
            tool_ok(req.id, json!({"ok": true, "existed": existed}))
        }
        Some("flaky") => {
            // The crash-recovery shape (RFC 0021 §8.4 e2e): the FIRST call hangs
            // (long enough for the harness to SIGKILL the agent mid-node — the
            // checkpoint cursor then sits AT this node); every later call
            // returns instantly. A resumed run re-enters the in-flight node
            // (at-least-once) and succeeds.
            let n = state.flaky_calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                std::thread::sleep(Duration::from_secs(60));
                tool_err(req.id, "flaky: the first call never completes in time")
            } else {
                tool_ok(req.id, json!({"ok": true, "attempt": n + 1}))
            }
        }
        // RFC 0028 §5/§6 profiles: a canned corpus for the knowledge and search
        // contracts (auto-context + tool e2e).
        Some("knowledge.search") => {
            let q = args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let top_k = args
                .get("top_k")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(5) as usize;
            let hits: Vec<serde_json::Value> = corpus()
                .iter()
                .filter(|(_, title, body)| q.is_empty() || q.split_whitespace().any(|w| title.to_ascii_lowercase().contains(w) || body.to_ascii_lowercase().contains(w)))
                .take(top_k)
                .enumerate()
                .map(|(i, (id, title, body))| json!({"id": id, "uri": format!("kb://{id}"), "title": title, "score": 1.0 - i as f64 * 0.1, "snippet": body.chars().take(120).collect::<String>(), "metadata": {"source": "mock"}}))
                .collect();
            tool_ok(req.id, json!({"hits": hits}))
        }
        Some("knowledge.get") => {
            let want = args
                .get("id")
                .or_else(|| args.get("uri"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim_start_matches("kb://")
                .to_string();
            match corpus().iter().find(|(id, _, _)| *id == want) {
                Some((id, title, body)) => tool_ok(
                    req.id,
                    json!({"content": body, "mime": "text/markdown", "metadata": {"id": id, "title": title}}),
                ),
                None => tool_err(req.id, "no such document"),
            }
        }
        Some("knowledge.list") => tool_ok(
            req.id,
            json!({"docs": corpus().iter().map(|(id, title, _)| json!({"id": id, "uri": format!("kb://{id}"), "title": title})).collect::<Vec<_>>()}),
        ),
        Some("search.query") => {
            let q = args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            tool_ok(
                req.id,
                json!({"results": [
                    {"title": format!("Result for {q}"), "url": format!("https://example.test/{}", q.replace(' ', "-")), "snippet": format!("A mock search result about {q}."), "source": "mock"},
                ]}),
            )
        }
        Some("search.fetch") => {
            let url = args
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            tool_ok(
                req.id,
                json!({"content": format!("<html><body>fetched {url}</body></html>"), "mime": "text/html", "final_url": url}),
            )
        }
        other => tool_err(req.id, &format!("no such tool: {other:?}")),
    }
}

/// The knowledge profile's canned corpus: `(id, title, body)`.
fn corpus() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "doc-1",
            "Deployment policy",
            "Deployments go through staging first; production deploys need a rollback plan and a canary of 5% for ten minutes.",
        ),
        (
            "doc-2",
            "Incident handbook",
            "During an incident, mitigate before root-causing; page the on-call; write a timeline within 24 hours.",
        ),
        (
            "doc-3",
            "Vacation policy",
            "Employees accrue 2 days of vacation per month; requests go to the manager two weeks ahead.",
        ),
    ]
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
