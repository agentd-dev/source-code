// SPDX-License-Identifier: AGPL-3.0-only
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
//! `resources/subscribe` — over the Streamable HTTP transport
//! (thread-per-connection, blocking, no dep). After a subscribe it pushes one
//! `notifications/resources/updated` on the long-lived `GET` SSE stream (unless
//! `emit` is off), so a reactive agent reached over HTTP has something to react
//! to.
//!
//! It can also **restart** without exiting: the `mock.restart` tool forgets
//! every session it issued (a request still carrying one is answered `404`,
//! as Streamable HTTP has a server say "no such session") and closes its open
//! notification streams, the way a process that exits drops its sockets. That
//! is how an e2e watches agentd notice a lost session and re-subscribe. Called
//! with `{"subscribe": false}` it comes back without `resources.subscribe`;
//! after a restart `mock://instruction` reads differently, as a publisher's
//! edit made while the server was down would. `mock.refuse_subscribe` refuses
//! the next `count` subscribes, as a server still warming up does.
//!
//! [`run_listening`] speaks the stateless revision instead: a subscription is
//! a `subscriptions/listen` stream, which the mock acknowledges, sends one
//! update down (for `<uri>`, when it is in the filter) and then closes, as a
//! server that drops it does. `mock.narrow` makes every later acknowledgment
//! leave a URI out.

use ::mcp::rpc::{self as json, Incoming, Request, Response};
use ::mcp::wire::{PROTOCOL_VERSION, method};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Cross-connection server state: a subscribe (on a POST) arms a one-shot push
/// that the open `GET` SSE stream delivers. The mock also implements the
/// **checkpointer tool profile** (`state.put` / `state.get` / `state.list` /
/// `state.delete` over an in-memory per-key history with the monotonic-seq
/// guard) plus a `flaky` tool (fails on its first call, succeeds after) and a
/// `mock.fault` control tool (fail the next N state calls) — together they let
/// the e2e + chaos suites prove crash → restore → complete with no external
/// infrastructure.
struct State {
    uri: String,
    emit: bool,
    /// Advertise `resources.subscribe`. Off, the mock is a server that offers
    /// resources but no subscriptions. A restart may turn it off.
    subscribe: AtomicBool,
    /// `resources/subscribe` calls still to refuse (`mock.refuse_subscribe`).
    refuse_subscribes: AtomicU64,
    /// Speak the stateless revision: subscriptions are `subscriptions/listen`.
    listen: bool,
    /// URIs every listen acknowledgment leaves out (`mock.narrow`).
    narrow: std::sync::Mutex<Vec<String>>,
    pending_emit: AtomicBool,
    /// Bumped by `mock.restart`; the session the mock issues is
    /// `mock-<generation>`, so every earlier one is forgotten.
    generation: AtomicU64,
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

impl State {
    /// The session the mock issues now.
    fn session(&self) -> String {
        format!("mock-{}", self.generation.load(Ordering::SeqCst))
    }
}

/// Serve the mock on loopback TCP until the process is killed, announcing the
/// bound address through `addr_file`. Returns the process exit code.
pub fn run(addr_file: &str, uri: &str, emit: bool) -> i32 {
    run_offering(addr_file, uri, emit, true)
}

/// [`run`], advertising `resources.subscribe` only when `subscribe` is set —
/// a server that serves resources but offers no subscriptions.
pub fn run_offering(addr_file: &str, uri: &str, emit: bool, subscribe: bool) -> i32 {
    serve(addr_file, uri, emit, subscribe, false)
}

/// [`run`] at the stateless revision, where a subscription is a
/// `subscriptions/listen` stream (see the module docs).
pub fn run_listening(addr_file: &str, uri: &str) -> i32 {
    serve(addr_file, uri, true, true, true)
}

fn serve(addr_file: &str, uri: &str, emit: bool, subscribe: bool, listen: bool) -> i32 {
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
        subscribe: AtomicBool::new(subscribe),
        refuse_subscribes: AtomicU64::new(0),
        listen,
        narrow: std::sync::Mutex::new(Vec::new()),
        pending_emit: AtomicBool::new(false),
        generation: AtomicU64::new(0),
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
    let Some((method_line, session, body)) = read_http(&stream) else {
        return;
    };
    // A session from before the last `mock.restart`: this server no longer
    // knows it. A request with no session at all (the handshake, a test's own
    // control call) is served.
    let frame = serde_json::from_slice::<Incoming>(&body);
    let handshake = matches!(&frame, Ok(Incoming::Request(r)) if r.method == "initialize");
    if !handshake && session.is_some_and(|s| s != state.session()) {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let is_get = method_line.starts_with("GET ");
    if is_get {
        serve_notifications(&mut stream, &state);
        return;
    }
    // POST: the JSON-RPC frame. A listen answers with a stream of its own.
    if let Ok(Incoming::Request(req)) = &frame
        && req.method == "subscriptions/listen"
    {
        serve_listen(&mut stream, req, &state);
        return;
    }
    match frame {
        Ok(Incoming::Request(req)) => {
            let (resp, session) = handle_request(req, &state);
            let payload = serde_json::to_value(resp).unwrap_or(serde_json::Value::Null);
            write_json(&mut stream, payload, session.then(|| state.session()));
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
                    "protocolVersion": if state.listen { "2026-07-28" } else { PROTOCOL_VERSION },
                    "capabilities": {"resources": {"subscribe": state.subscribe.load(Ordering::SeqCst), "listChanged": true}, "tools": {}, "prompts": {"listChanged": true}},
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
                    {"name": "mock.slow", "description": "answer after `ms` milliseconds", "inputSchema": {"type": "object"}},
                    {"name": "mock.restart", "description": "forget every session, as a restarted server does", "inputSchema": {"type": "object"}},
                    {"name": "mock.narrow", "description": "leave `uri` out of every later listen acknowledgment", "inputSchema": {"type": "object"}},
                    {"name": "mock.refuse_subscribe", "description": "refuse the next `count` resources/subscribe calls", "inputSchema": {"type": "object"}},
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
            // Every read that REACHED the server, logged like `MOCK_BINDINGS`,
            // so an e2e can count reads on this side instead of trusting the
            // consumer's own `instruction.loaded` — the server log is the
            // arbiter of whether a read happened.
            eprintln!("MOCK_READ {asked}");
            // A REGISTRY-shaped instruction: the document plus the
            // `md.instruction/*` alignment metadata a registry serves
            // (RFC-0028 §3.3), signed with a FIXED test seed when the `sign`
            // feature is built, so the consumer's §7.6 verification — and its
            // fail-closed refusals — have coverage with no live gateway.
            if asked.starts_with("instruction://") {
                return (Response::ok(req.id, registry_read(&asked)), false);
            }
            let (mime, text) = match asked.as_str() {
                "skill://incident-runbook" => ("text/x-skill+markdown", "# Incident runbook\n1. Acknowledge the alert. 2. Find the blast radius. 3. Mitigate first, root-cause later. 4. Write the timeline.".to_string()),
                // A restart reads differently: the publisher's edit made
                // while the server was down.
                "mock://instruction" => ("text/plain", match state.generation.load(Ordering::SeqCst) {
                    0 => "You are the mock-served agent. Follow the served instruction.".to_string(),
                    g => format!("You are the mock-served agent, restarted {g} time(s). Follow the served instruction."),
                }),
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
        // Skills as prompts: the catalogue, then a body per skill.
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
            // Every subscribe that REACHED the server, like `MOCK_READ`: the
            // server side's own count of what a client subscribed.
            let asked = req
                .params
                .as_ref()
                .and_then(|p| p.get("uri"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            eprintln!("MOCK_SUBSCRIBE {asked}");
            // Take one refusal if any are left. A compare-exchange loop, not
            // `fetch_update`: newer toolchains deprecate that name for
            // `try_update`, which the MSRV does not have.
            let refused = {
                let left = &state.refuse_subscribes;
                let mut n = left.load(Ordering::SeqCst);
                loop {
                    if n == 0 {
                        break false;
                    }
                    match left.compare_exchange_weak(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(_) => break true,
                        Err(now) => n = now,
                    }
                }
            };
            if refused {
                return (
                    Response::err(req.id, json::INTERNAL_ERROR, "subscribe refused"),
                    false,
                );
            }
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

/// One MCP `tools/call`: the checkpointer profile plus `flaky` and the `mock.*`
/// controls. A tool result is standard MCP content: one text part carrying the
/// JSON **and** the same JSON as `structuredContent`. Both are emitted because
/// the store adapter's default mapping reads `result.structuredContent.*` and
/// falls back to the text part, so serving both exercises either path.
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
    // Every call's params as they reached the server — `_meta` and arguments
    // both — so an e2e can assert what the wire carried, not what the client
    // meant to send.
    eprintln!("MOCK_CALL {params}");
    // A strict server: `_meta` belongs in `params._meta` (MCP reserves it
    // there), and no tool here declares an argument by that name, so one
    // arriving inside `arguments` is refused the way `additionalProperties:
    // false` refuses any undeclared property.
    if args.get("_meta").is_some() {
        return tool_err(
            req.id,
            "arguments: additional property '_meta' is not allowed",
        );
    }
    // The registry's consumer-alignment tools (RFC-0028 §3.3). The call is
    // echoed on stderr so an e2e can assert WHAT the consumer reported
    // without needing a live registry.
    if let Some(n) = name
        && n.starts_with("instructions.bindings.")
    {
        eprintln!("MOCK_BINDINGS {n} {args}");
        return match n {
            "instructions.bindings.create" => {
                tool_ok(req.id, json!({"bindingId": "sub_mock_1", "mode": "follow"}))
            }
            _ => tool_ok(req.id, json!({"ok": true})),
        };
    }
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
        // A restart: every session issued so far is forgotten, and the
        // subscriptions with them — nothing is pushed until a client
        // subscribes again on a new session.
        Some("mock.restart") => {
            state.pending_emit.store(false, Ordering::SeqCst);
            if let Some(sub) = args.get("subscribe").and_then(serde_json::Value::as_bool) {
                state.subscribe.store(sub, Ordering::SeqCst);
            }
            let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
            eprintln!("MOCK_RESTART {generation}");
            tool_ok(req.id, json!({"ok": true, "generation": generation}))
        }
        Some("mock.narrow") => {
            let uri = args
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            state
                .narrow
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(uri);
            tool_ok(req.id, json!({"ok": true}))
        }
        Some("mock.refuse_subscribe") => {
            let n = args
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1);
            state.refuse_subscribes.store(n, Ordering::SeqCst);
            tool_ok(req.id, json!({"ok": true, "count": n}))
        }
        Some("mock.fault") => {
            let n = args
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1);
            state.fail_next.store(n, Ordering::SeqCst);
            tool_ok(req.id, json!({"ok": true, "count": n}))
        }
        // A live tool that takes its time: nothing on the wire until it
        // answers, the way a JSON-answering server computes before it replies.
        Some("mock.slow") => {
            let ms = args
                .get("ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            std::thread::sleep(Duration::from_millis(ms));
            tool_ok(req.id, json!({"state": {"slept_ms": ms}}))
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
                // A prefix listing returns every LIVE key under `prefix` with
                // its latest seq; a tombstone (latest state null) is omitted,
                // because a deleted key must read as absent to the restorer.
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
            // Without a `prefix`, a list reports the seqs held for ONE key.
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
            // The crash-recovery shape: the FIRST call hangs
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
        // A canned corpus standing in for the knowledge and search tool
        // contracts, so auto-context and tool e2e need no external service.
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

/// One `subscriptions/listen`: acknowledge the filter (less what `mock.narrow`
/// left out), send one `resources/updated` for the mock's resource when the
/// acknowledgment holds it, and close the stream without a final result — a
/// server that dropped it. Every listen that reached the mock is logged
/// `MOCK_LISTEN <uris>`.
fn serve_listen(stream: &mut TcpStream, req: &Request, state: &State) {
    let mut filter = req
        .params
        .as_ref()
        .and_then(|p| p.get("notifications"))
        .cloned()
        .unwrap_or(json!({}));
    let narrow = state
        .narrow
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(uris) = filter["resourceSubscriptions"].as_array_mut() {
        uris.retain(|u| !narrow.iter().any(|n| u == n));
    }
    let uris: Vec<&str> = filter["resourceSubscriptions"]
        .as_array()
        .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    eprintln!("MOCK_LISTEN {}", uris.join(","));
    let meta = json!({"io.modelcontextprotocol/subscriptionId": serde_json::to_value(&req.id).unwrap_or_default()});
    let mut frames = vec![json!({
        "jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
        "params": {"_meta": meta, "notifications": filter}
    })];
    if uris.contains(&state.uri.as_str()) {
        frames.push(json!({
            "jsonrpc": "2.0", "method": method::NOTIFY_RESOURCES_UPDATED,
            "params": {"_meta": meta, "uri": state.uri}
        }));
    }
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    for f in frames {
        let _ = stream.write_all(format!("data: {f}\n\n").as_bytes());
        let _ = stream.flush();
    }
    std::thread::sleep(Duration::from_millis(50));
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
    let generation = state.generation.load(Ordering::SeqCst);
    loop {
        // A restart drops the stream, as an exiting process drops its sockets.
        if state.generation.load(Ordering::SeqCst) != generation {
            return;
        }
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
/// clone of `stream`. Returns `(request_line, mcp_session_id, body)` — the
/// other headers are unused by the mock.
fn read_http(stream: &TcpStream) -> Option<(String, Option<String>, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut content_length = 0usize;
    let mut session = None;
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
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("mcp-session-id")
        {
            session = Some(v.trim().to_string());
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some((request_line, session, body))
}

/// Write an `application/json` HTTP response carrying `payload`, optionally
/// stamping the `Mcp-Session-Id` header.
fn write_json(stream: &mut TcpStream, payload: serde_json::Value, session: Option<String>) {
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let session_hdr = session
        .map(|s| format!("Mcp-Session-Id: {s}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session_hdr}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// The document this mock's registry serves. `REGISTRY_VERSION` moves when the
/// caller asks for `@next`, so a test can watch an apply boundary.
const REGISTRY_DOC: &str = "---\nspec: \"1\"\nid: instruction://ins_mock\n---\n# Mock registry agent\n\nServe the mock.\n\n:::!workflow{name=mock-drain}\nsteps:\n  start: { kind: manual }\n  done:  { kind: finish, depends_on: [start] }\n:::\n";
const REGISTRY_DOC_V2: &str = "---\nspec: \"1\"\nid: instruction://ins_mock\n---\n# Mock registry agent\n\nServe the mock, version two.\n\n:::!workflow{name=mock-drain}\nsteps:\n  start: { kind: manual }\n  done:  { kind: finish, depends_on: [start] }\n:::\n";
/// The fixed Ed25519 seed the mock signs with; a test derives the public key
/// from it to pin the publisher.
pub const MOCK_SIGN_SEED: [u8; 32] = [7u8; 32];
/// The REGISTRY's delivery key — a different key from the publisher's, at its
/// own document, exactly as the instruction.md gateway serves it. A verifier
/// that reaches for the publisher key (or the publisher's JWKS shape) to check
/// a delivery signature passes a naive mock and fails a real registry.
pub const MOCK_DELIVERY_SEED: [u8; 32] = [9u8; 32];
const MOCK_PUBLISHER: &str = "https://instruction.md/pub/mock";
const MOCK_PUBLISHER_KID: &str = "mock-1";
/// Only the signing paths name the delivery key; a build without `sign`
/// serves no attestations at all.
#[cfg(feature = "sign")]
const MOCK_DELIVERY_KID: &str = "delivery";
const MOCK_DELIVERY_KEYS_URI: &str = "instruction://delivery-keys.json";

/// `resources/read` for an `instruction://…` uri: contents + `_meta`, plus
/// the SEP-2549 reuse offer (`ttlMs`, `cacheScope`) the registry sends. That
/// offer is what makes an SDK response cache reachable in CI: without it rmcp
/// never stores an entry, so a consumer that answered re-reads from memory
/// passed every test here while confirming a dead registry in production.
fn registry_read(uri: &str) -> serde_json::Value {
    let mut out = registry_contents(uri);
    out["ttlMs"] = json!(3_600_000);
    out["cacheScope"] = json!("private");
    out
}

fn registry_contents(uri: &str) -> serde_json::Value {
    // The publisher's JWKS, for key discovery.
    if uri == MOCK_DELIVERY_KEYS_URI {
        return json!({"contents": [{"uri": uri, "mimeType": "application/json",
            "text": mock_delivery_jwks()}]});
    }
    if uri.contains("/keys.json") {
        return json!({"contents": [{"uri": uri, "mimeType": "application/json",
            "text": mock_jwks()}]});
    }
    let v2 = uri.ends_with("@next");
    let text = if v2 { REGISTRY_DOC_V2 } else { REGISTRY_DOC };
    let version_id = if v2 { "ver_mock_2" } else { "ver_mock_1" };
    let mut meta = json!({
        "md.instruction/canonical": "instruction://ins_mock",
        "md.instruction/versionId": version_id,
        "md.instruction/revision": if v2 { 2 } else { 1 },
        "md.instruction/ref": if v2 { "next" } else { "stable" },
        "md.instruction/spec": "1",
        "md.instruction/publisher": MOCK_PUBLISHER,
        "md.instruction/resolution": "raw",
        "md.instruction/publisherKeys": "instruction://pub/mock/keys.json",
        "md.instruction/deliveryKeys": MOCK_DELIVERY_KEYS_URI,
    });
    let digest = instruction_core::digest(text.as_bytes());
    meta["md.instruction/digest"] = json!(digest);
    meta["md.instruction/deliveredDigest"] = json!(digest);
    if let Some((author, delivery)) = mock_signatures(text, version_id) {
        meta["md.instruction/signature"] = json!(author);
        meta["md.instruction/deliverySignature"] = json!(delivery);
        // The publisher kid describes the AUTHOR signature only; the delivery
        // JWS names its own key in its header.
        meta["md.instruction/kid"] = json!(MOCK_PUBLISHER_KID);
        meta["md.instruction/signatureKeyState"] = json!("active");
    }
    json!({"contents": [{"uri": uri, "mimeType": "text/markdown; variant=instruction",
        "text": text, "_meta": meta}]})
}

#[cfg(feature = "sign")]
fn mock_jwks() -> String {
    let key = crate::aauth::AgentKey::from_seed(&MOCK_SIGN_SEED).expect("test seed");
    let x = crate::aauth::b64::url_nopad(key.public_bytes());
    // The registry's real document carries more than `keys` + `publisher`;
    // served here so a consumer that denies unknown fields fails in CI
    // rather than in production.
    json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig",
                     "kid": MOCK_PUBLISHER_KID, "state": "active", "x": x,
                     "source": "platform", "createdAt": "2026-09-06T00:00:00.000Z"}],
           "publisher": MOCK_PUBLISHER, "organizationId": "org_mock",
           "platformSigning": true, "revision": 1,
           "updatedAt": "2026-09-06T00:00:00.000Z"})
    .to_string()
}

/// The REGISTRY's delivery key document: `{issuer, keys}` — no `publisher`.
#[cfg(feature = "sign")]
fn mock_delivery_jwks() -> String {
    let key = crate::aauth::AgentKey::from_seed(&MOCK_DELIVERY_SEED).expect("test seed");
    let x = crate::aauth::b64::url_nopad(key.public_bytes());
    json!({"issuer": "https://instruction.md/pub",
           "keys": [{"kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig",
                     "kid": MOCK_DELIVERY_KID, "x": x}]})
    .to_string()
}
#[cfg(not(feature = "sign"))]
fn mock_delivery_jwks() -> String {
    json!({"issuer": "https://instruction.md/pub", "keys": []}).to_string()
}
#[cfg(not(feature = "sign"))]
fn mock_jwks() -> String {
    json!({"keys": []}).to_string()
}

/// `(author_jws, delivery_jws)` over this document, for the fixed test reader.
#[cfg(feature = "sign")]
fn mock_signatures(text: &str, version_id: &str) -> Option<(String, String)> {
    use crate::config::attest::{Claims, sign_kid};
    let digest = instruction_core::digest(text.as_bytes());
    let author = Claims {
        spec: crate::config::attest::SPEC_CLAIM.into(),
        typ: "author".into(),
        doc: "instruction://ins_mock".into(),
        version: version_id.into(),
        digest: crate::config::attest::author_digest(text.as_bytes()),
        capabilities: vec!["core".into(), "compute".into()],
        publisher: MOCK_PUBLISHER.into(),
        iat: 1,
        exp: u64::MAX / 2,
        aud: None,
        manifest: None,
        author: None,
    };
    let pub_key = crate::aauth::AgentKey::from_seed(&MOCK_SIGN_SEED).ok()?;
    let a_jws = sign_kid(&pub_key, &author, Some(MOCK_PUBLISHER_KID)).ok()?;
    let delivery = Claims {
        typ: "delivery".into(),
        digest,
        aud: Some("agent://mock-reader".into()),
        // The manifest the reference implementation accounts for this
        // delivery, in the signed form a registry embeds (S7) — not a
        // hand-written one, which drifted from the shape a strict reader
        // requires while every test against this mock passed.
        manifest: Some(mock_manifest(text)?),
        author: Some(a_jws.clone()),
        ..author
    };
    let del_key = crate::aauth::AgentKey::from_seed(&MOCK_DELIVERY_SEED).ok()?;
    Some((
        a_jws,
        sign_kid(&del_key, &delivery, Some(MOCK_DELIVERY_KID)).ok()?,
    ))
}
/// The §7.4 manifest of delivering `text` as the mock serves it — no
/// parameters, no facts, nothing to include — in its signed form.
#[cfg(feature = "sign")]
fn mock_manifest(text: &str) -> Option<instruction_core::Manifest> {
    let ctx = instruction_core::Context {
        grants: instruction_core::doc::all_families(),
        ..Default::default()
    };
    let doc = instruction_core::parse(text).ok()?;
    Some(
        instruction_core::deliver(&doc, &ctx)
            .ok()?
            .manifest
            .signed_form(),
    )
}
#[cfg(not(feature = "sign"))]
fn mock_signatures(_text: &str, _v: &str) -> Option<(String, String)> {
    None
}
