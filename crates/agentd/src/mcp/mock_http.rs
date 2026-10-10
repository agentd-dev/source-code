// SPDX-License-Identifier: AGPL-3.0-only
//! A minimal built-in **Streamable HTTP** MCP server, for tests and for operators
//! kicking the tyres on reactive setups. Hidden mode:
//! `agentd --internal-mock-mcp-http <addr-file> <uri> [--no-emit] [--protocol-version <v>]`.
//!
//! Binds a **loopback TCP** listener on `127.0.0.1:0` and writes the bound
//! `host:port` into `<addr-file>` (atomically: tmp + rename;
//! [`crate::announce_addr`]) so the launching harness discovers the endpoint by
//! waiting for the file, then hands agentd `--mcp name=http://<addr>`.
//!
//! It serves one resource at `<uri>`, a checkpointer tool profile, canned
//! knowledge and search tools, a registry-shaped instruction, and the `mock.*`
//! tools a test steers it with — thread-per-connection, blocking, no dep. It
//! speaks one of the two lifecycles an MCP client reaches a server through
//! ([`Lifecycle`]; `--protocol-version` picks it):
//!
//! - **`Initialize`** (2025-11-25, the default). The `initialize` handshake
//!   issues a session; a subscription is `resources/subscribe`, after which one
//!   `notifications/resources/updated` goes down the long-lived `GET` stream
//!   (unless `emit` is off). `server/discover` is answered `-32601`, the way a
//!   server that predates it answers.
//! - **`Discover`** (2026-07-28). `server/discover` and no handshake, no
//!   session, no `GET`. Every request is held to the revision's rules — the
//!   `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` headers, the `_meta`
//!   every request carries, and the `Mcp-Param-Region` header the annotated
//!   `mock.regional` tool declares — and one that breaks a rule is refused
//!   `400` with the JSON-RPC error the revision names. `initialize` is refused
//!   as a revision it does not speak (`-32022`), `resources/subscribe` as a
//!   method the revision removed. A subscription is a `subscriptions/listen`
//!   stream: acknowledged (less what `mock.narrow` left out, and less every URI
//!   while subscriptions are off), sent one update for `<uri>` when the
//!   acknowledgment holds it and `emit` is on, then held until the client
//!   hangs up or the server restarts — or closed at once, without
//!   [`Options::hold_listen`].
//!
//! Each request that reaches it leaves a line on stderr, the server side's own
//! record of the wire, which an e2e counts instead of trusting the client:
//! `MOCK_DISCOVER <_meta>`, `MOCK_INITIALIZE <revision asked>`,
//! `MOCK_REFUSED <code> <method> <reason>`, `MOCK_SESSION <id>`, `MOCK_GET`,
//! `MOCK_SUBSCRIBE <uri>`, `MOCK_UNSUBSCRIBE <uri>`, `MOCK_LISTEN <filter>`,
//! `MOCK_LISTEN_CLOSED <client|restart|done>`, `MOCK_CALL <params>`,
//! `MOCK_READ <uri>`.
//!
//! It can also **restart** without exiting: `mock.restart` forgets every
//! session it issued (a request still carrying one is answered `404`, as
//! Streamable HTTP has a server say "no such session") and closes its open
//! notification streams and held listens, the way a process that exits drops
//! its sockets. That is how an e2e watches agentd notice a lost session and
//! subscribe again. Called with `{"subscribe": false}` it comes back without
//! `resources.subscribe`; after a restart `mock://instruction` reads
//! differently, as a publisher's edit made while the server was down would.
//! `mock.refuse_subscribe` refuses the next `count` subscribes — a
//! `resources/subscribe`, or a listen that names a URI — as a server still
//! warming up does. `mock.tools_changed` sends
//! `notifications/tools/list_changed` down every open stream that takes it.

use ::mcp::rpc::{self as json, Incoming, Request, Response};
use ::mcp::wire::{DISCOVER_PROTOCOL_VERSION, PROTOCOL_VERSION, method};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How a client reaches the mock, named by mechanism rather than by revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// The `initialize` handshake and its session (MCP 2025-11-25).
    Initialize,
    /// `server/discover`, then every request on its own (MCP 2026-07-28).
    Discover,
}

impl Lifecycle {
    /// The lifecycle of an MCP revision the mock speaks; `None` for any other.
    pub fn of_revision(revision: &str) -> Option<Lifecycle> {
        match revision {
            DISCOVER_PROTOCOL_VERSION => Some(Lifecycle::Discover),
            PROTOCOL_VERSION => Some(Lifecycle::Initialize),
            _ => None,
        }
    }
}

/// What the mock serves and how ([`run_with`]).
#[derive(Debug, Clone)]
pub struct Options {
    /// The one resource it serves and notifies for.
    pub uri: String,
    /// Send one `resources/updated` for `uri` after each subscription.
    pub emit: bool,
    /// Offer subscriptions (`resources.subscribe`). Off, it is a server that
    /// serves resources but never notifies; a restart may turn it off.
    pub subscribe: bool,
    pub lifecycle: Lifecycle,
    /// Hold a `subscriptions/listen` stream open until the client hangs up or
    /// the server restarts; off, close each one 50 ms after its frames.
    pub hold_listen: bool,
    /// How the 2026-07-28 edge declares MCP Events (`mock_events`).
    pub events: super::mock_events::Spelling,
}

impl Options {
    /// `uri` at 2025-11-25, emitting, offering subscriptions, holding listens.
    pub fn new(uri: impl Into<String>) -> Options {
        Options {
            uri: uri.into(),
            emit: true,
            subscribe: true,
            lifecycle: Lifecycle::Initialize,
            hold_listen: true,
            events: super::mock_events::Spelling::Extension,
        }
    }
}

/// The `_meta` keys of the 2026-07-28 revision.
const META_PREFIX: &str = "io.modelcontextprotocol/";
const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
/// A header that does not match the request (2026-07-28).
const HEADER_MISMATCH: i64 = -32020;
/// A revision the server does not speak (2026-07-28).
const UNSUPPORTED_VERSION: i64 = -32022;

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
    lifecycle: Lifecycle,
    hold_listen: bool,
    /// Advertise `resources.subscribe`. Off, the mock is a server that offers
    /// resources but no subscriptions. A restart may turn it off.
    subscribe: AtomicBool,
    /// Subscribes still to refuse (`mock.refuse_subscribe`).
    refuse_subscribes: AtomicU64,
    /// URIs every listen acknowledgment leaves out (`mock.narrow`).
    narrow: Mutex<Vec<String>>,
    pending_emit: AtomicBool,
    /// Bumped by `mock.restart`; the session the mock issues is
    /// `mock-<generation>`, so every earlier one is forgotten, and every
    /// stream opened before it is closed.
    generation: AtomicU64,
    /// The server-to-client streams open now, which `mock.tools_changed`
    /// writes to.
    open: Mutex<Vec<Open>>,
    next_open: AtomicU64,
    /// The checkpointer store: key → (seq → envelope). Monotonic per key.
    store: Mutex<std::collections::BTreeMap<String, std::collections::BTreeMap<u64, Value>>>,
    /// `flaky` call counter (first call errors, later ones succeed).
    flaky_calls: AtomicU64,
    /// Fault injection: remaining `state.*` calls to fail with a tool error.
    fail_next: AtomicU64,
    /// Every `state.*` call performed (tool name) — `mock.ops` reports it.
    ops: Mutex<Vec<String>>,
    /// The name of every tool called so far, which `mock.tools_changed` can
    /// wait for.
    called: Mutex<std::collections::BTreeSet<String>>,
    /// The MCP Events surface (2026-07-28 edge), and how it is declared.
    events: super::mock_events::Events,
    events_spelling: super::mock_events::Spelling,
}

impl State {
    /// The session the mock issues now.
    fn session(&self) -> String {
        format!("mock-{}", self.generation.load(Ordering::SeqCst))
    }

    /// Take one of the refusals `mock.refuse_subscribe` asked for, if any are
    /// left. A compare-exchange loop, not `fetch_update`: newer toolchains
    /// deprecate that name for `try_update`, which the MSRV does not have.
    fn take_refusal(&self) -> bool {
        let left = &self.refuse_subscribes;
        let mut n = left.load(Ordering::SeqCst);
        loop {
            if n == 0 {
                return false;
            }
            match left.compare_exchange_weak(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(now) => n = now,
            }
        }
    }
}

/// A server-to-client stream the mock holds open: the `GET` notification
/// stream (2025-11-25) or a held `subscriptions/listen` (2026-07-28).
struct Open {
    id: u64,
    /// A clone of the stream's socket. Written only under the registry's lock,
    /// so two writers never interleave their frames.
    socket: TcpStream,
    /// The listen's request id, which every notification on it carries as its
    /// `subscriptionId`; `None` for a `GET` stream.
    subscription: Option<Value>,
    /// Takes `notifications/tools/list_changed`: a `GET` stream always, a
    /// listen when its acknowledgment holds `toolsListChanged`.
    tools: bool,
    /// Takes `notifications/events/list_changed`: a listen whose
    /// acknowledgment holds `eventsListChanged`.
    events: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Serve the mock on loopback TCP until the process is killed, announcing the
/// bound address through `addr_file`. Returns the process exit code.
pub fn run_with(addr_file: &str, opts: &Options) -> i32 {
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
        uri: opts.uri.clone(),
        emit: opts.emit,
        lifecycle: opts.lifecycle,
        hold_listen: opts.hold_listen,
        subscribe: AtomicBool::new(opts.subscribe),
        refuse_subscribes: AtomicU64::new(0),
        narrow: Mutex::new(Vec::new()),
        pending_emit: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        open: Mutex::new(Vec::new()),
        next_open: AtomicU64::new(0),
        store: Mutex::new(std::collections::BTreeMap::new()),
        flaky_calls: AtomicU64::new(0),
        fail_next: AtomicU64::new(0),
        ops: Mutex::new(Vec::new()),
        called: Mutex::new(std::collections::BTreeSet::new()),
        events: super::mock_events::Events::default(),
        events_spelling: opts.events,
    });
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let state = Arc::clone(&state);
        std::thread::spawn(move || handle_conn(stream, state));
    }
    0
}

/// One HTTP request per connection (the client sends `Connection: close`).
fn handle_conn(mut stream: TcpStream, state: Arc<State>) {
    let Some(http) = read_http(&stream) else {
        return;
    };
    match state.lifecycle {
        Lifecycle::Initialize => serve_initialize(&mut stream, &http, &state),
        Lifecycle::Discover => serve_discover(&mut stream, &http, &state),
    }
}

/// The 2025-11-25 edge. A `GET` is the notification SSE stream; a `POST` is
/// one JSON-RPC frame.
fn serve_initialize(stream: &mut TcpStream, http: &Http, state: &State) {
    // A session from before the last `mock.restart`: this server no longer
    // knows it. A request with no session at all (the handshake, a test's own
    // control call) is served.
    let frame = serde_json::from_slice::<Incoming>(&http.body);
    let handshake = matches!(&frame, Ok(Incoming::Request(r)) if r.method == method::INITIALIZE);
    if !handshake
        && http
            .header("mcp-session-id")
            .is_some_and(|s| s != state.session())
    {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    if http.is_get() {
        serve_notifications(stream, state);
        return;
    }
    match frame {
        Ok(Incoming::Request(req)) => {
            if req.method == "server/discover" {
                // A server that predates `server/discover`: the method is one
                // it does not have.
                eprintln!("MOCK_DISCOVER {}", meta_of(req.params.as_ref()));
            }
            let (resp, session) = handle_request(req, http, state);
            let payload = serde_json::to_value(resp).unwrap_or(Value::Null);
            write_json(stream, 200, &payload, session.then(|| state.session()));
        }
        // A notification POST (e.g. notifications/initialized) → 202, no body.
        Ok(Incoming::Notification(_)) | Ok(Incoming::Response(_)) | Err(_) => accepted(stream),
    }
}

/// The 2026-07-28 edge: no session and no `GET`, every request held to the
/// revision's rules before it is answered.
fn serve_discover(stream: &mut TcpStream, http: &Http, state: &State) {
    if http.is_get() {
        eprintln!("MOCK_GET");
        let _ = stream.write_all(
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    // The revision has no sessions; a client that sends one is said, and
    // served all the same — what it must not do is depend on one.
    if let Some(session) = http.header("mcp-session-id") {
        eprintln!("MOCK_SESSION {session}");
    }
    let body: Value = serde_json::from_slice(&http.body).unwrap_or(Value::Null);
    let called = body["method"].as_str().unwrap_or_default().to_string();
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    if called == method::INITIALIZE {
        // The revision asked for is not one this server speaks.
        let asked = body["params"]["protocolVersion"]
            .as_str()
            .unwrap_or_default();
        eprintln!("MOCK_INITIALIZE {asked}");
        let data = json!({"supported": [DISCOVER_PROTOCOL_VERSION], "requested": asked});
        return refuse(
            stream,
            400,
            &id,
            &called,
            UNSUPPORTED_VERSION,
            "Unsupported protocol version",
            Some(data),
        );
    }
    if called == "server/discover" {
        eprintln!("MOCK_DISCOVER {}", meta_of(body.get("params")));
    }
    if let Some((code, reason, data)) = stateless_refusal(http, &body) {
        return refuse(stream, 400, &id, &called, code, &reason, data);
    }
    match called.as_str() {
        "server/discover" => {
            let payload = json!({"jsonrpc": "2.0", "id": id, "result": discover_result(state)});
            write_json(stream, 200, &payload, None);
        }
        "subscriptions/listen" => serve_listen(stream, &id, &body, state),
        // MCP Events (SEP-3415): only where it is declared [SEP L94].
        m if m.starts_with("events/")
            && state.events_spelling != super::mock_events::Spelling::None =>
        {
            match super::mock_events::handle(&state.events, m, &body["params"]) {
                Ok(result) => write_json(
                    stream,
                    200,
                    &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    None,
                ),
                Err(error) => {
                    let status = if error["code"] == json::METHOD_NOT_FOUND {
                        404
                    } else {
                        200
                    };
                    write_json(
                        stream,
                        status,
                        &json!({"jsonrpc": "2.0", "id": id, "error": error}),
                        None,
                    )
                }
            }
        }
        // The revision removed them: a subscription is a listen.
        method::RESOURCES_SUBSCRIBE | method::RESOURCES_UNSUBSCRIBE => refuse(
            stream,
            404,
            &id,
            &called,
            json::METHOD_NOT_FOUND,
            "Method not found",
            None,
        ),
        m if m.starts_with("notifications/") => accepted(stream),
        _ => match serde_json::from_value::<Request>(body) {
            Ok(req) => {
                let (resp, _) = handle_request(req, http, state);
                let mut payload = serde_json::to_value(resp).unwrap_or(Value::Null);
                // Every result at this revision says how to read it.
                if let Some(result) = payload.get_mut("result").and_then(Value::as_object_mut) {
                    result
                        .entry("resultType")
                        .or_insert_with(|| json!("complete"));
                }
                // A method it does not have is a `404`, as rmcp's server
                // answers one.
                let status = if payload["error"]["code"] == json::METHOD_NOT_FOUND {
                    404
                } else {
                    200
                };
                write_json(stream, status, &payload, None);
            }
            Err(_) => accepted(stream),
        },
    }
}

/// Why a 2026-07-28 server refuses this request, checked in the order rmcp's
/// own server checks: the revision header, the revision itself, `_meta`, then
/// the routing headers — and, for `mock.regional`, the `Mcp-Param-Region`
/// header its `x-mcp-header` annotation declares. `None` when it is
/// acceptable.
fn stateless_refusal(http: &Http, body: &Value) -> Option<(i64, String, Option<Value>)> {
    let mismatch = |why: &str| Some((HEADER_MISMATCH, why.to_string(), None));
    if http.count("mcp-protocol-version") != 1 {
        return mismatch("MCP-Protocol-Version must be sent once");
    }
    let asked = http.header("mcp-protocol-version").unwrap_or_default();
    if asked != DISCOVER_PROTOCOL_VERSION {
        let data = json!({"supported": [DISCOVER_PROTOCOL_VERSION], "requested": asked});
        return Some((
            UNSUPPORTED_VERSION,
            "Unsupported protocol version".into(),
            Some(data),
        ));
    }
    let params = &body["params"];
    let meta = &params["_meta"];
    if meta[format!("{META_PREFIX}protocolVersion")].as_str() != Some(asked) {
        return mismatch("_meta protocolVersion does not match the header");
    }
    if meta
        .get(format!("{META_PREFIX}clientCapabilities"))
        .is_none()
        || meta.get(format!("{META_PREFIX}clientInfo")).is_none()
    {
        return Some((
            json::INVALID_PARAMS,
            "_meta lacks clientInfo or clientCapabilities".into(),
            None,
        ));
    }
    let called = body["method"].as_str().unwrap_or_default();
    if http.count("mcp-method") != 1 || http.header("mcp-method") != Some(called) {
        return mismatch("Mcp-Method does not match the body");
    }
    let name = match called {
        method::TOOLS_CALL | method::PROMPTS_GET => params["name"].as_str(),
        method::RESOURCES_READ => params["uri"].as_str(),
        _ => None,
    };
    if let Some(name) = name
        && (http.count("mcp-name") != 1 || http.header("mcp-name") != Some(name))
    {
        return mismatch("Mcp-Name does not match the body");
    }
    if called == method::TOOLS_CALL
        && params["name"] == "mock.regional"
        && (http.count("mcp-param-region") != 1
            || http.header("mcp-param-region") != params["arguments"]["region"].as_str())
    {
        return mismatch("Mcp-Param-Region does not match the argument");
    }
    None
}

/// Refuse a request with `status` and the JSON-RPC error `code`, and say so.
fn refuse(
    stream: &mut TcpStream,
    status: u16,
    id: &Value,
    method: &str,
    code: i64,
    reason: &str,
    data: Option<Value>,
) {
    eprintln!("MOCK_REFUSED {code} {method} {reason}");
    let mut error = json!({"code": code, "message": reason});
    if let Some(data) = data {
        error["data"] = data;
    }
    write_json(
        stream,
        status,
        &json!({"jsonrpc": "2.0", "id": id, "error": error}),
        None,
    );
}

/// The answer to `server/discover`: every field the revision requires.
fn discover_result(state: &State) -> Value {
    let mut capabilities = json!({
        "resources": {"subscribe": state.subscribe.load(Ordering::SeqCst), "listChanged": true},
        "tools": {"listChanged": true},
        "prompts": {"listChanged": true}
    });
    super::mock_events::declare(&mut capabilities, state.events_spelling);
    json!({
        "resultType": "complete",
        "supportedVersions": [DISCOVER_PROTOCOL_VERSION],
        "capabilities": capabilities,
        "ttlMs": 0,
        "cacheScope": "private",
        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "agentd-mock-http", "version": crate::VERSION}}
    })
}

/// A request's `params._meta`, for a log line (`null` when it has none).
fn meta_of(params: Option<&Value>) -> Value {
    params
        .and_then(|p| p.get("_meta"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn accepted(stream: &mut TcpStream) {
    let _ = stream
        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

/// Build the JSON-RPC response for one request. Returns the response and whether
/// to stamp the `Mcp-Session-Id` header (on `initialize`, which only the
/// 2025-11-25 edge passes on).
fn handle_request(req: Request, http: &Http, state: &State) -> (Response, bool) {
    let uri = &state.uri;
    match req.method.as_str() {
        "initialize" => {
            let asked = req
                .params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            eprintln!("MOCK_INITIALIZE {asked}");
            (
                Response::ok(
                    req.id,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {"resources": {"subscribe": state.subscribe.load(Ordering::SeqCst), "listChanged": true}, "tools": {}, "prompts": {"listChanged": true}},
                        "serverInfo": {"name": "agentd-mock-http", "version": crate::VERSION}
                    }),
                ),
                true,
            )
        }
        "ping" => (Response::ok(req.id, json!({})), false),
        "tools/list" => (
            Response::ok(
                req.id,
                with_revision_tools(
                    state,
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
                        {"name": "mock.refuse_subscribe", "description": "refuse the next `count` subscribes", "inputSchema": {"type": "object"}},
                        {"name": "mock.tools_changed", "description": "send notifications/tools/list_changed down every open stream that takes it", "inputSchema": {"type": "object"}},
                        {"name": "knowledge.search", "description": "RAG search over the mock corpus", "inputSchema": {"type": "object"}},
                        {"name": "knowledge.get", "description": "fetch a mock document", "inputSchema": {"type": "object"}},
                        {"name": "knowledge.list", "description": "list mock documents", "inputSchema": {"type": "object"}},
                        {"name": "search.query", "description": "mock web search", "inputSchema": {"type": "object"}},
                        {"name": "search.fetch", "description": "mock page fetch", "inputSchema": {"type": "object"}},
                    ]}),
                ),
            ),
            false,
        ),
        "tools/call" => (handle_tool_call(req, http, state), false),
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
        "resources/unsubscribe" => {
            let uri = req
                .params
                .as_ref()
                .and_then(|p| p.get("uri"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            eprintln!("MOCK_UNSUBSCRIBE {uri}");
            (Response::ok(req.id, json!({})), false)
        }
        "resources/subscribe" => {
            // Every subscribe that REACHED the server, like `MOCK_READ`: the
            // server side's own count of what a client subscribed.
            let asked = req
                .params
                .as_ref()
                .and_then(|p| p.get("uri"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            // Decided before the line is written, as for a listen.
            let refused = state.take_refusal();
            eprintln!("MOCK_SUBSCRIBE {asked}");
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
fn handle_tool_call(req: Request, http: &Http, state: &State) -> Response {
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
    // both, and under `headers` the `Mcp-Param-*` headers it came with — so an
    // e2e can assert what the wire carried, not what the client meant to send.
    let mut logged = params.clone();
    let param_headers: serde_json::Map<String, Value> = http
        .headers
        .iter()
        .filter(|(k, _)| k.starts_with("mcp-param-"))
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    if !param_headers.is_empty() {
        logged["headers"] = Value::Object(param_headers);
    }
    eprintln!("MOCK_CALL {logged}");
    if let Some(n) = name {
        lock(&state.called).insert(n.to_string());
    }
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
    // The listing changed: tell every listen that asked.
    if name == Some("mock.events_changed") {
        let _ = super::mock_events::control(&state.events, "mock.events_changed", &args);
        let mut told = 0;
        for o in lock(&state.open).iter_mut().filter(|o| o.events) {
            let params = o
                .subscription
                .as_ref()
                .map(|id| json!({"_meta": {SUBSCRIPTION_ID: id}}));
            let note = json::Notification::new("notifications/events/list_changed", params);
            if send_frame(&mut o.socket, &note) {
                told += 1;
            }
        }
        eprintln!("MOCK_EVENTS_CHANGED {told}");
        return tool_ok(req.id, json!({"told": told}));
    }
    if let Some(n) = name
        && let Some(out) = super::mock_events::control(&state.events, n, &args)
    {
        return tool_ok(req.id, out);
    }
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
        // Every client that holds a stream open for it is told the tool list
        // changed — once `receivers` such streams are open and a call of the
        // tool `after` has reached the mock (bounded), so a caller can wait
        // for the clients it means to tell, and for one of them to be past a
        // point of its own. Answers with its arguments and how many were told.
        Some("mock.tools_changed") => {
            let want = args
                .get("receivers")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize;
            let after = args.get("after").and_then(serde_json::Value::as_str);
            match tools_changed(state, want, after) {
                Ok(told) => {
                    let mut out = if args.is_object() {
                        args.clone()
                    } else {
                        json!({})
                    };
                    out["notified"] = json!(told);
                    tool_ok(req.id, out)
                }
                Err(open) => tool_err(
                    req.id,
                    &format!(
                        "mock.tools_changed: {open} of {want} receivers open, {after:?} called: {}",
                        after.is_none_or(|t| lock(&state.called).contains(t))
                    ),
                ),
            }
        }
        // The annotated tool: its `region` travels as `Mcp-Param-Region` at
        // 2026-07-28, which the edge refuses the call without.
        Some("mock.regional") => tool_ok(
            req.id,
            json!({"region": args.get("region").cloned().unwrap_or(Value::Null)}),
        ),
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

/// The tool list, plus — at 2026-07-28, where a tool's `x-mcp-header`
/// annotation names a header its argument travels in — `mock.regional`, whose
/// `region` is `Mcp-Param-Region`, and `mock.nested`, whose annotation sits on
/// a nested property, which the client rejects and drops: the tool a host must
/// say it lost.
fn with_revision_tools(state: &State, mut list: Value) -> Value {
    if state.lifecycle == Lifecycle::Discover
        && let Some(tools) = list["tools"].as_array_mut()
    {
        tools.push(json!({
            "name": "mock.regional",
            "description": "answers with the region it was called for",
            "inputSchema": {"type": "object", "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"}}}
        }));
        tools.push(json!({
            "name": "mock.nested",
            "description": "a nested x-mcp-header annotation the client drops",
            "inputSchema": {"type": "object", "properties": {"outer": {"type": "object",
                "properties": {"inner": {"type": "string", "x-mcp-header": "Inner"}}}}}
        }));
    }
    list
}

/// Send `notifications/tools/list_changed` down every open stream that takes
/// it, once `receivers` of them are open and the tool `after` has been called.
/// Returns how many were told, or the open count when that never held within
/// 30 s (nothing is sent then). The frames are written before this returns,
/// so a caller that sees the answer knows each client has them on its socket.
fn tools_changed(state: &State, receivers: usize, after: Option<&str>) -> Result<usize, usize> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let called = after.is_none_or(|t| lock(&state.called).contains(t));
        {
            let mut open = lock(&state.open);
            let ready = open.iter().filter(|o| o.tools).count();
            if called && ready >= receivers {
                let mut told = 0;
                for o in open.iter_mut().filter(|o| o.tools) {
                    let params = o
                        .subscription
                        .as_ref()
                        .map(|id| json!({"_meta": {SUBSCRIPTION_ID: id}}));
                    let note = json::Notification::new(method::NOTIFY_TOOLS_LIST_CHANGED, params);
                    if send_frame(&mut o.socket, &note) {
                        told += 1;
                    }
                }
                eprintln!("MOCK_TOOLS_CHANGED {told}");
                return Ok(told);
            }
            if Instant::now() >= deadline {
                return Err(ready);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Write one SSE `data:` frame; `false` when the client is gone.
fn send_frame(socket: &mut TcpStream, frame: &impl serde::Serialize) -> bool {
    let data = serde_json::to_string(frame).unwrap_or_default();
    socket
        .write_all(format!("data: {data}\n\n").as_bytes())
        .and_then(|()| socket.flush())
        .is_ok()
}

/// Wait on an open stream until the client hangs up (`"client"`) or the server
/// restarts (`"restart"`). The client sends nothing more on it, so a read
/// returns only at its end; it times out every 25 ms to look at the
/// generation, and, when `tick` is given, to run it — `false` from `tick` ends
/// the wait as the client being gone.
fn hold(
    stream: &mut TcpStream,
    state: &State,
    generation: u64,
    mut tick: impl FnMut(&mut TcpStream) -> bool,
) -> &'static str {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(25)));
    let mut byte = [0u8; 1];
    loop {
        if state.generation.load(Ordering::SeqCst) != generation {
            return "restart";
        }
        if !tick(stream) {
            return "client";
        }
        match stream.read(&mut byte) {
            Ok(0) => return "client",
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return "client",
        }
    }
}

/// One `subscriptions/listen` (2026-07-28). Every URI in the filter is logged
/// `MOCK_SUBSCRIBE <uri>`, and the filter `MOCK_LISTEN <filter>`. A refusal
/// `mock.refuse_subscribe` armed answers a listen that names a URI with a
/// JSON-RPC `-32603`. Otherwise the filter is acknowledged — less what
/// `mock.narrow` left out, and less every URI while subscriptions are off —
/// one `resources/updated` for the mock's resource follows when `emit` is on
/// and the acknowledgment holds it, and the stream is held until the client
/// hangs up or the server restarts (or, without `hold_listen`, closed 50 ms
/// later without a final result: a server that dropped it). How it ended is
/// logged `MOCK_LISTEN_CLOSED <client|restart|done>`.
fn serve_listen(stream: &mut TcpStream, id: &Value, body: &Value, state: &State) {
    let asked = match &body["params"]["notifications"] {
        f @ Value::Object(_) => f.clone(),
        _ => json!({}),
    };
    let wanted: Vec<&str> = asked["resourceSubscriptions"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    // Decided before the lines are written, so a test that sees them and then
    // arms a refusal never has it taken by the listen it saw.
    let refused = !wanted.is_empty() && state.take_refusal();
    for uri in &wanted {
        eprintln!("MOCK_SUBSCRIBE {uri}");
    }
    eprintln!("MOCK_LISTEN {asked}");
    if refused {
        let refused = json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": json::INTERNAL_ERROR, "message": "subscribe refused"}});
        return write_json(stream, 200, &refused, None);
    }
    let narrow = lock(&state.narrow).clone();
    let subscribe = state.subscribe.load(Ordering::SeqCst);
    let mut filter = asked.clone();
    if let Some(uris) = filter["resourceSubscriptions"].as_array_mut() {
        uris.retain(|u| subscribe && !narrow.iter().any(|n| u == n));
    }
    // Events' list changes are honoured only where Events is declared: a
    // server that did not declare `listChanged` leaves the field out of its
    // acknowledgment [SEP L208].
    if state.events_spelling == super::mock_events::Spelling::None
        && let Some(f) = filter.as_object_mut()
    {
        f.remove("eventsListChanged");
    }
    let tools = filter["toolsListChanged"] == true;
    let events = filter["eventsListChanged"] == true;
    let meta = json!({SUBSCRIPTION_ID: id});
    let mut frames = vec![json!({
        "jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
        "params": {"_meta": meta, "notifications": filter}
    })];
    let acked = filter["resourceSubscriptions"]
        .as_array()
        .is_some_and(|a| a.iter().any(|u| *u == state.uri));
    if state.emit && acked {
        frames.push(json!({
            "jsonrpc": "2.0", "method": method::NOTIFY_RESOURCES_UPDATED,
            "params": {"_meta": meta, "uri": state.uri}
        }));
    }
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    for f in &frames {
        out.push_str(&format!("data: {f}\n\n"));
    }
    if !state.hold_listen {
        let _ = stream.write_all(out.as_bytes());
        let _ = stream.flush();
        std::thread::sleep(Duration::from_millis(50));
        eprintln!("MOCK_LISTEN_CLOSED done");
        return;
    }
    // Held: registered before the acknowledgment leaves, and written under the
    // registry's lock, so a client that has the acknowledgment can count on
    // `mock.tools_changed` reaching it, and its frames never interleave.
    let generation = state.generation.load(Ordering::SeqCst);
    let open_id = state.next_open.fetch_add(1, Ordering::SeqCst);
    {
        let mut open = lock(&state.open);
        if stream.write_all(out.as_bytes()).is_err() || stream.flush().is_err() {
            eprintln!("MOCK_LISTEN_CLOSED client");
            return;
        }
        if let Ok(socket) = stream.try_clone() {
            open.push(Open {
                id: open_id,
                socket,
                subscription: Some(id.clone()),
                tools,
                events,
            });
        }
    }
    let how = hold(stream, state, generation, |_| true);
    lock(&state.open).retain(|o| o.id != open_id);
    let _ = stream.shutdown(Shutdown::Both);
    eprintln!("MOCK_LISTEN_CLOSED {how}");
}

/// The long-lived `GET` SSE stream (2025-11-25): hold it open and deliver the
/// one-shot `resources/updated` armed by a subscribe, and whatever
/// `mock.tools_changed` sends. Deliberately sends NO keep-alive comments — the
/// client polls its stop flag via a read timeout between events, and a stream
/// of comments would keep its SSE reader busy and defeat that. It ends when
/// the client hangs up or the server restarts, as an exiting process drops
/// its sockets.
fn serve_notifications(stream: &mut TcpStream, state: &State) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    let generation = state.generation.load(Ordering::SeqCst);
    let open_id = state.next_open.fetch_add(1, Ordering::SeqCst);
    {
        let mut open = lock(&state.open);
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
        if let Ok(socket) = stream.try_clone() {
            open.push(Open {
                id: open_id,
                socket,
                subscription: None,
                tools: true,
                events: false,
            });
        }
    }
    hold(stream, state, generation, |stream| {
        if !state.pending_emit.swap(false, Ordering::SeqCst) {
            return true;
        }
        let note = json::Notification::new(
            method::NOTIFY_RESOURCES_UPDATED,
            Some(json!({"uri": state.uri})),
        );
        let _open = lock(&state.open);
        send_frame(stream, &note)
    });
    lock(&state.open).retain(|o| o.id != open_id);
    let _ = stream.shutdown(Shutdown::Both);
}

/// One HTTP request as the mock read it.
struct Http {
    /// The request line, e.g. `POST /mcp HTTP/1.1`.
    line: String,
    /// Every header, its name lower-cased, duplicates kept in arrival order.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Http {
    fn is_get(&self) -> bool {
        self.line.starts_with("GET ")
    }

    /// The first value of header `name` (lower-case).
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// How many times header `name` (lower-case) was sent.
    fn count(&self, name: &str) -> usize {
        self.headers.iter().filter(|(k, _)| k == name).count()
    }
}

/// Read one HTTP request (request line, headers, Content-Length body) off a
/// clone of `stream`.
fn read_http(stream: &TcpStream) -> Option<Http> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut headers = Vec::new();
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
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0usize);
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some(Http {
        line: line.trim_end().to_string(),
        headers,
        body,
    })
}

/// Write an `application/json` HTTP response carrying `payload`, optionally
/// stamping the `Mcp-Session-Id` header.
fn write_json(stream: &mut TcpStream, status: u16, payload: &Value, session: Option<String>) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Status",
    };
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let session_hdr = session
        .map(|s| format!("Mcp-Session-Id: {s}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{session_hdr}Content-Length: {}\r\nConnection: close\r\n\r\n",
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
/// The document `@endmatter` serves: no fence at all — front matter and
/// prose — followed by end matter that changes on every read, under a new
/// versionId each time: a publisher editing only the record. Fence-free on
/// purpose, so only a consumer whose delivery gate sees front and end matter
/// strips the record; one that reads it raw delivers a new text per read.
const REGISTRY_DOC_PROSE: &str = "---\nspec: \"1\"\nid: instruction://ins_mock\n---\n# Mock registry agent\n\nServe the mock, in prose.\n";
/// The document `@variants` serves: one variant per model family (§5.2's
/// `agent` key) and one for any host but agentd, so what a consumer delivers
/// shows which facts it chose them by — its operator's model, read where the
/// read happens, or none at all.
pub const REGISTRY_DOC_VARIANTS: &str = "---\nspec: \"1\"\nid: instruction://ins_mock\n---\n# Mock registry agent\n\n:::when{agent=\"claude\"}\nAnswer as the claude variant.\n:::\n\n:::when{agent=\"gpt\"}\nAnswer as the gpt variant, at somewhat greater length.\n:::\n\n:::unless{host=\"agentd\"}\nAnswer as a host other than agentd.\n:::\n";
/// How many times `@endmatter` has been read: the counter its end matter and
/// versionId carry. Per mock process, which is per test.
static END_MATTER_READS: AtomicU64 = AtomicU64::new(0);
/// The registry's RESOLVED read of the mock document (`…?resolved=1`): text
/// §3.5 already ran on for this reader. It is delivered text a second
/// delivery would change — bold labels, an example quoting a rule fence and
/// an author-note opener — so a consumer that delivers it again is caught
/// byte for byte.
pub const REGISTRY_RESOLVED_TEXT: &str = "# Mock registry agent\n\n**MUST:** Serve the mock.\n\n**EXAMPLE:**\nA rule is written as:\n\n:::must\nNever promise a refund.\n:::\n\n<!-- and a note like this -->\n\nWorkflow `mock-drain` is configured.\n";
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
    // A query selects the registry's resolution: `?resolved=1` serves the
    // RESOLVED read, `?resolved=stale` the same text under the digest of
    // something else. Everything else reads the reference before it.
    let (base, query) = uri.split_once('?').unwrap_or((uri, ""));
    let resolved = query.starts_with("resolved=");
    let v2 = base.ends_with("@next");
    // `@narrow` serves version one with a delivery attestation that caps
    // below the author's (`core` only): what a registry narrowing a
    // document for one reader serves, and a verifier that intersects only
    // the author's set would not see.
    let narrow = base.ends_with("@narrow");
    let end_matter = base.ends_with("@endmatter");
    let mut text = (if v2 { REGISTRY_DOC_V2 } else { REGISTRY_DOC }).to_string();
    // `@foreign` serves ANOTHER document under this URI — one the same
    // publisher legitimately signed for its own id: what a registry swapping
    // documents under a pinned URI serves. Every signature over it is valid;
    // only its `doc` claim says it is not the document the pin names.
    if base.ends_with("@foreign") {
        text = text.replace("id: instruction://ins_mock", "id: instruction://ins_other");
    }
    let version_id = if base.ends_with("@variants") {
        text = REGISTRY_DOC_VARIANTS.to_string();
        "ver_mock_variants".to_string()
    } else if end_matter {
        let n = END_MATTER_READS.fetch_add(1, Ordering::SeqCst) + 1;
        text = format!("{REGISTRY_DOC_PROSE}\n---\nreviewed: {n}\n---\n");
        format!("ver_mock_em_{n}")
    } else if v2 {
        "ver_mock_2".to_string()
    } else {
        "ver_mock_1".to_string()
    };
    let version_id = version_id.as_str();
    // `@selfsigned` serves version one carrying its own author JWS in its
    // front matter — the transport-independent form a file or OCI source
    // carries. The author signs the §7.2 author digest, which excludes that
    // line, so the JWS signed over the bare document is the one it carries.
    if base.ends_with("@selfsigned")
        && let Some((author, _)) = mock_signatures(&text, version_id, MOCK_CAPABILITIES, 1)
    {
        text = text.replacen("---\n# ", &format!("signature: {author}\n---\n# "), 1);
    }
    let text = text.as_str();
    let mut meta = json!({
        "md.instruction/canonical": "instruction://ins_mock",
        "md.instruction/versionId": version_id,
        "md.instruction/revision": if v2 { 2 } else { 1 },
        "md.instruction/ref": base.rsplit_once('@').map_or("stable", |(_, r)| r),
        "md.instruction/spec": "1",
        "md.instruction/publisher": MOCK_PUBLISHER,
        "md.instruction/resolution": if resolved { "resolved" } else { "raw" },
        "md.instruction/publisherKeys": "instruction://pub/mock/keys.json",
        "md.instruction/deliveryKeys": MOCK_DELIVERY_KEYS_URI,
    });
    let digest = instruction_core::digest(text.as_bytes());
    meta["md.instruction/digest"] = json!(digest);
    meta["md.instruction/deliveredDigest"] = json!(digest);
    if resolved {
        // The authored digest stays the document's; the delivered one is the
        // resolved text's — or, for `stale`, the authored document's, which
        // is what a registry serving one text under another's digest sends.
        let delivered = if query == "resolved=stale" {
            digest.clone()
        } else {
            instruction_core::digest(REGISTRY_RESOLVED_TEXT.as_bytes())
        };
        meta["md.instruction/deliveredDigest"] = json!(delivered);
        // Signed as a registry signs a resolved read: the author signature
        // covers the AUTHORED document, which these bytes are not, and the
        // delivery attestation covers these bytes and chains to it through
        // the authored document's manifest.
        if let Some((author, delivery)) = mock_signatures_over(
            text,
            REGISTRY_RESOLVED_TEXT,
            version_id,
            MOCK_CAPABILITIES,
            1,
        ) {
            meta["md.instruction/signature"] = json!(author);
            meta["md.instruction/deliverySignature"] = json!(delivery);
            meta["md.instruction/kid"] = json!(MOCK_PUBLISHER_KID);
            meta["md.instruction/signatureKeyState"] = json!("active");
        }
        return json!({"contents": [{"uri": uri, "mimeType": "text/markdown; variant=instruction",
            "text": REGISTRY_RESOLVED_TEXT, "_meta": meta}]});
    }
    let delivery_caps: &[&str] = if narrow { &["core"] } else { MOCK_CAPABILITIES };
    if let Some((author, delivery)) = mock_signatures(text, version_id, delivery_caps, 1) {
        meta["md.instruction/signature"] = json!(author);
        // `@reauthored` serves a second valid author signature over the same
        // document (signed a moment later), not the one its delivery chains
        // to.
        if base.ends_with("@reauthored")
            && let Some((later, _)) = mock_signatures(text, version_id, delivery_caps, 2)
        {
            meta["md.instruction/signature"] = json!(later);
        }
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

/// What the mock's author attestation lets a document activate.
const MOCK_CAPABILITIES: &[&str] = &["core", "compute"];

/// `(author_jws, delivery_jws)` over this document, for the fixed test
/// reader, the delivery capped at `delivery_caps`, both issued at `iat`.
#[cfg(feature = "sign")]
fn mock_signatures(
    text: &str,
    version_id: &str,
    delivery_caps: &[&str],
    iat: u64,
) -> Option<(String, String)> {
    mock_signatures_over(text, text, version_id, delivery_caps, iat)
}

/// [`mock_signatures`] for a read whose bytes are `delivered` — the
/// registry's own delivery of the `authored` document, for a resolved read —
/// rather than the authored text itself.
#[cfg(feature = "sign")]
fn mock_signatures_over(
    text: &str,
    delivered: &str,
    version_id: &str,
    delivery_caps: &[&str],
    iat: u64,
) -> Option<(String, String)> {
    use crate::config::attest::{Claims, sign_kid};
    let digest = instruction_core::digest(delivered.as_bytes());
    let author = Claims {
        spec: crate::config::attest::SPEC_CLAIM.into(),
        typ: "author".into(),
        // The `doc` claim is the authored document's id (§7.3), as a
        // publisher signs it — not the URI it happens to be served under.
        doc: instruction_core::front_matter_id(text.as_bytes())
            .unwrap_or_else(|| "instruction://ins_mock".into()),
        version: version_id.into(),
        digest: crate::config::attest::author_digest(text.as_bytes()),
        capabilities: MOCK_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        publisher: MOCK_PUBLISHER.into(),
        iat,
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
        capabilities: delivery_caps.iter().map(|c| c.to_string()).collect(),
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
fn mock_signatures(_text: &str, _v: &str, _caps: &[&str], _iat: u64) -> Option<(String, String)> {
    None
}
#[cfg(not(feature = "sign"))]
fn mock_signatures_over(
    _text: &str,
    _delivered: &str,
    _v: &str,
    _caps: &[&str],
    _iat: u64,
) -> Option<(String, String)> {
    None
}
