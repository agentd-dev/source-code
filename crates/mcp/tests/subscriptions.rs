// SPDX-License-Identifier: AGPL-3.0-only
//! Resource subscriptions against a server that misbehaves the ways real ones
//! do, at both revisions agentd reaches: one that restarts and forgets the
//! session, one that closes or ends a listen stream, one that never offered
//! subscriptions, and one that refuses a subscribe once.
//!
//! Every one of these used to fail SILENTLY — a subscription that was gone, a
//! wake that never came, a URI reported as "already covered" that the server
//! never accepted — so each test counts what reached the server, which is the
//! only arbiter of whether a subscription exists.
//!
//! The 2025-11-25 server is reached through `initialize` and holds a session;
//! the 2026-07-28 one through `server/discover` alone, with no session, and it
//! holds every request to that revision's rules the way rmcp's own server does
//! (`common::stateless_refusal`), recording each refusal. Every 2026-07-28 test
//! also asserts that nothing was refused, and that no handshake, no `GET` and
//! no session reached the server.

mod common;

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{
    MODERN, Req, StatelessOpts, discover_answer, error, initialize_answer, initialize_refusal,
    respond, respond_raw, result, stateless_refusal,
};
use mcp::client::{FirstRequest, McpClient, McpError};
use mcp::rmcp_client::{ListenEvent, RmcpBuilder, RmcpClient};
use serde_json::{Value, json};

/// How a connection to the test server starts, and everything that follows
/// from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// MCP 2025-11-25: `initialize` issues a session, a URI is subscribed
    /// with `resources/subscribe`, and updates come down the `GET` stream.
    Handshake,
    /// MCP 2026-07-28: `server/discover`, no session, and updates on a
    /// `subscriptions/listen` stream.
    Stateless,
}

const LIFECYCLES: [Lifecycle; 2] = [Lifecycle::Handshake, Lifecycle::Stateless];

/// How a held listen stream ends when the server restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ListenEnd {
    /// The connection closes with no last word.
    #[default]
    Close,
    /// The listen's final result: `{resultType: "complete"}` naming it.
    Complete,
    /// `notifications/cancelled` naming the listen.
    Cancelled,
}

/// The `_meta` key that names the listen a frame belongs to.
const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
/// The `_meta` key [`Server::push`] marks its updates with, so a test can tell
/// them from the one a listen sends when it opens.
const PUSHED: &str = "test/pushed";

/// A listen stream the server is holding open.
struct Held {
    /// The client's end of the connection: unique while it is open.
    peer: SocketAddr,
    /// The listen request's id, which every frame on it names.
    id: Value,
    /// The URIs it acknowledged.
    uris: Vec<String>,
    stream: TcpStream,
}

/// The server's side of the story, shared with the test body.
#[derive(Default)]
struct Server {
    /// Bumped by a "restart": every session issued before it is forgotten,
    /// and a request that carries one is answered `404`.
    generation: AtomicU64,
    /// Advertise `resources.subscribe` as false (2026-07-28) or leave it out
    /// (2025-11-25).
    no_subscribe: AtomicBool,
    /// Answer `initialize` with `503`: a server still coming back up.
    refuse_handshake: AtomicBool,
    /// Refuse a `resources/subscribe` for these URIs, once each.
    refuse_once: Mutex<Vec<String>>,
    /// Refuse the next `subscriptions/listen` with a JSON-RPC error.
    refuse_listen_once: AtomicBool,
    /// Leave these URIs out of every listen acknowledgment: a server that
    /// takes a narrower filter than it was asked for, which rmcp allows.
    narrow: Mutex<Vec<String>>,
    /// Answer every notification-stream `GET` with `404`, where the spec says
    /// `405` — a framework that routes only `POST`.
    get_404: AtomicBool,
    /// Hold each `resources/subscribe`, and each listen's acknowledgment,
    /// this long before answering.
    slow_subscribe_ms: AtomicU64,
    /// Keep each listen stream open after its acknowledgment and updates,
    /// until the client hangs up or the server restarts. Otherwise it closes.
    hold_listen: AtomicBool,
    /// Held listen streams still open. Counted down only when a read on the
    /// connection says it closed — end of stream, or a reset; a read that
    /// times out (`WouldBlock`, `TimedOut`) is a connection still open.
    open_listens: AtomicU64,
    /// How a restart ends each held listen.
    listen_end: Mutex<ListenEnd>,
    /// Every request that reached it, as read.
    seen: Mutex<Vec<Req>>,
    /// Every refusal it sent: `(code, method, message)`.
    refused: Mutex<Vec<(i64, String, String)>>,
    /// Open `GET` notification streams (2025-11-25), closed by a restart the
    /// way a process that exits closes its sockets.
    streams: Mutex<Vec<TcpStream>>,
    /// Held listen streams (2026-07-28), ended by a restart.
    listens: Mutex<Vec<Held>>,
    /// The `list_changed` notifications it advertises (2026-07-28):
    /// `"tools"`, `"prompts"`. Acknowledged when a listen asks for them.
    list_changed: Mutex<Vec<&'static str>>,
    /// What `resources/read` answers for a URI (`"x"` when absent): changed
    /// without a notification, it is a change a listen gap would lose.
    content: Mutex<std::collections::HashMap<String, String>>,
    /// Fail every `resources/read`.
    fail_reads: AtomicBool,
    /// Refuse this many flags-only listens (no URI), as `refuse_flags_how`.
    refuse_flags_only: AtomicU64,
    /// 0: JSON-RPC -32602; 1: a plain 500; 2: a stream closed before its
    /// acknowledgment.
    refuse_flags_how: AtomicU64,
    /// Refuse every listen whose filter asks for `toolsListChanged`.
    refuse_flagged: AtomicBool,
    /// Hold a flags-only listen's response head this long.
    slow_flags_head_ms: AtomicU64,
}

impl Server {
    fn session(&self) -> String {
        format!("s-{}", self.generation.load(Ordering::SeqCst))
    }

    /// Forget every session, drop every open stream, and end every held
    /// listen as `listen_end` says.
    fn restart(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        for s in self.streams.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
        let end = *self.listen_end.lock().unwrap();
        for mut h in self.listens.lock().unwrap().drain(..) {
            let last = match end {
                ListenEnd::Close => None,
                ListenEnd::Complete => Some(result(
                    &h.id,
                    json!({"resultType": "complete", "_meta": {SUBSCRIPTION_ID: h.id}}),
                )),
                ListenEnd::Cancelled => Some(json!({
                    "jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": h.id}
                })),
            };
            if let Some(f) = last {
                let _ = h.stream.write_all(format!("data: {f}\n\n").as_bytes());
                let _ = h.stream.flush();
            }
            let _ = h.stream.shutdown(Shutdown::Both);
        }
    }

    /// Push `resources/updated` for `uri` down every open notification
    /// stream, and every held listen that acknowledged it.
    fn push(&self, uri: &str) {
        let note = json!({"jsonrpc": "2.0", "method": "notifications/resources/updated",
                          "params": {"uri": uri, "_meta": {PUSHED: true}}});
        for s in self.streams.lock().unwrap().iter_mut() {
            let _ = s.write_all(format!("data: {note}\n\n").as_bytes());
            let _ = s.flush();
        }
        for h in self.listens.lock().unwrap().iter_mut() {
            if !h.uris.iter().any(|u| u == uri) {
                continue;
            }
            let mut note = note.clone();
            note["params"]["_meta"][SUBSCRIPTION_ID] = h.id.clone();
            let _ = h.stream.write_all(format!("data: {note}\n\n").as_bytes());
            let _ = h.stream.flush();
        }
    }

    /// The POSTs that called `method`, in order.
    fn requests(&self, method: &str) -> Vec<Req> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| !r.is_get() && r.method() == method)
            .cloned()
            .collect()
    }

    fn count(&self, method: &str) -> usize {
        self.requests(method).len()
    }

    /// The URIs `method` was called with, in order; for a listen, the
    /// filter's URIs, comma-joined.
    fn uris(&self, method: &str) -> Vec<String> {
        self.requests(method).iter().filter_map(uri_of).collect()
    }

    fn open_listens(&self) -> u64 {
        self.open_listens.load(Ordering::SeqCst)
    }

    /// Send `method` (a notification without params) down every held listen.
    fn notify(&self, method: &str) {
        for h in self.listens.lock().unwrap().iter_mut() {
            let note = json!({"jsonrpc": "2.0", "method": method,
                              "params": {"_meta": {SUBSCRIPTION_ID: h.id}}});
            let _ = h.stream.write_all(format!("data: {note}\n\n").as_bytes());
            let _ = h.stream.flush();
        }
    }

    /// The listens a flags-only filter asked for (no URI), in order.
    fn flags_only_listens(&self) -> Vec<Req> {
        self.requests("subscriptions/listen")
            .into_iter()
            .filter(|r| r.body["params"]["notifications"]["resourceSubscriptions"].is_null())
            .collect()
    }

    /// Record a refusal and send it.
    fn refuse(&self, req: &Req, w: &mut TcpStream, status: u16, body: &Value) {
        self.refused.lock().unwrap().push((
            body["error"]["code"].as_i64().unwrap_or(0),
            req.method().to_string(),
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ));
        respond(w, status, &[], body);
    }
}

/// The URI a call names; for a listen, the filter's URIs, comma-joined.
fn uri_of(req: &Req) -> Option<String> {
    match req.body["params"]["notifications"]["resourceSubscriptions"].as_array() {
        Some(uris) => Some(
            uris.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(","),
        ),
        None => req.body["params"]["uri"].as_str().map(str::to_string),
    }
}

/// A Streamable-HTTP MCP server speaking `lifecycle`, with sessions it can
/// forget and listens it can end ([`Server::restart`]).
fn spawn(server: Arc<Server>, lifecycle: Lifecycle) -> String {
    let (ep, _) = common::spawn(move |req, w| {
        server.seen.lock().unwrap().push(req.clone());
        if !req.is_get() && !req.line.starts_with("POST") {
            return respond_raw(w, 405, &[], "");
        }
        match lifecycle {
            Lifecycle::Handshake => handshake(req, w, &server),
            Lifecycle::Stateless => stateless(req, w, &server),
        }
    });
    ep
}

fn tools(id: &Value) -> Value {
    result(
        id,
        json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}),
    )
}

/// The 2025-11-25 server.
fn handshake(req: &Req, w: &mut TcpStream, server: &Server) {
    let session = req.header("mcp-session-id");
    // A session this server no longer holds: Streamable HTTP's `404`.
    let stale = session.is_some_and(|s| s != server.session());
    if req.is_get() {
        if stale || server.get_404.load(Ordering::SeqCst) {
            return respond_raw(w, 404, &[], "");
        }
        if session.is_none() {
            return respond_raw(w, 405, &[], "");
        }
        let _ = w.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = w.flush();
        server.streams.lock().unwrap().push(w.try_clone().unwrap());
        return;
    }
    let method = req.method();
    if stale && method != "initialize" {
        return respond_raw(w, 404, &[], "");
    }
    if req.body.get("id").is_none() {
        return respond_raw(w, 202, &[], "");
    }
    let id = req.id();
    match method {
        "initialize" if server.refuse_handshake.load(Ordering::SeqCst) => {
            respond_raw(w, 503, &[], "")
        }
        "initialize" => {
            let resources = if server.no_subscribe.load(Ordering::SeqCst) {
                json!({"listChanged": true})
            } else {
                json!({"subscribe": true, "listChanged": true})
            };
            respond(
                w,
                200,
                &[("Mcp-Session-Id", &server.session())],
                &initialize_answer(
                    &id,
                    "2025-11-25",
                    &json!({"tools": {}, "resources": resources}),
                ),
            );
        }
        "resources/subscribe" => {
            std::thread::sleep(Duration::from_millis(
                server.slow_subscribe_ms.load(Ordering::SeqCst),
            ));
            let uri = uri_of(req).unwrap_or_default();
            let refused = {
                let mut once = server.refuse_once.lock().unwrap();
                match once.iter().position(|u| *u == uri) {
                    Some(i) => {
                        once.remove(i);
                        true
                    }
                    None => false,
                }
            };
            let body = if refused {
                error(&id, -32603, "subscribe refused")
            } else {
                result(&id, json!({}))
            };
            respond(w, 200, &[], &body);
        }
        "tools/list" => respond(w, 200, &[], &tools(&id)),
        "subscriptions/listen" => respond(w, 200, &[], &error(&id, -32601, "Method not found")),
        _ => respond(w, 200, &[], &result(&id, json!({}))),
    }
}

/// The 2026-07-28 server.
fn stateless(req: &Req, w: &mut TcpStream, server: &Server) {
    if req.is_get() {
        return respond_raw(w, 405, &[], "");
    }
    let id = req.id();
    let supported = [MODERN.to_string()];
    if req.method() == "initialize" {
        let asked = req.body["params"]["protocolVersion"]
            .as_str()
            .unwrap_or_default();
        return server.refuse(req, w, 400, &initialize_refusal(&id, asked, &supported));
    }
    if let Some((status, body)) = stateless_refusal(req, &supported) {
        return server.refuse(req, w, status, &body);
    }
    match req.method() {
        "server/discover" => {
            // A listChanged flag only where a test asks for one.
            let lc = server.list_changed.lock().unwrap().clone();
            let caps = json!({"tools": {"listChanged": lc.contains(&"tools")},
                              "prompts": {"listChanged": lc.contains(&"prompts")},
                              "resources": {"subscribe": !server.no_subscribe.load(Ordering::SeqCst)}});
            let opts = StatelessOpts {
                caps,
                ..StatelessOpts::default()
            };
            respond(w, 200, &[], &discover_answer(&id, &opts));
        }
        "subscriptions/listen" => listen(req, w, server),
        // The revision removed them.
        "resources/subscribe" | "resources/unsubscribe" => {
            respond(w, 404, &[], &error(&id, -32601, "Method not found"))
        }
        "tools/list" => respond(w, 200, &[], &tools(&id)),
        "resources/read" => {
            if server.fail_reads.load(Ordering::SeqCst) {
                return respond(w, 200, &[], &error(&id, -32603, "read failed"));
            }
            let uri = req.body["params"]["uri"].as_str().unwrap_or_default();
            let text = server
                .content
                .lock()
                .unwrap()
                .get(uri)
                .cloned()
                .unwrap_or_else(|| "x".into());
            respond(
                w,
                200,
                &[],
                &result(&id, json!({"contents": [{"uri": uri, "text": text}]})),
            )
        }
        m if m.starts_with("notifications/") => respond_raw(w, 202, &[], ""),
        _ => respond(w, 200, &[], &error(&id, -32601, "Method not found")),
    }
}

/// Acknowledge a listen's filter — less any URI in `narrow`, and any
/// list-changed flag, which this server does not offer — then send one update
/// for each URI in it. Then hold the stream ([`Server::hold_listen`]) or
/// close it without a final result: a server that dropped the connection, or
/// restarted between two listens.
fn listen(req: &Req, w: &mut TcpStream, server: &Server) {
    let id = req.id();
    std::thread::sleep(Duration::from_millis(
        server.slow_subscribe_ms.load(Ordering::SeqCst),
    ));
    if server.refuse_listen_once.swap(false, Ordering::SeqCst) {
        return respond(w, 200, &[], &error(&id, -32603, "listen refused"));
    }
    let asked = &req.body["params"]["notifications"];
    if server.refuse_flagged.load(Ordering::SeqCst) && asked["toolsListChanged"] == true {
        return respond(
            w,
            400,
            &[],
            &error(&id, -32602, "toolsListChanged is not offered"),
        );
    }
    let flags_only = asked["resourceSubscriptions"].is_null();
    // Listens arrive one at a time here, so a load then a decrement is not
    // a race.
    if flags_only && server.refuse_flags_only.load(Ordering::SeqCst) > 0 {
        server.refuse_flags_only.fetch_sub(1, Ordering::SeqCst);
        return match server.refuse_flags_how.load(Ordering::SeqCst) {
            0 => respond(
                w,
                400,
                &[],
                &error(&id, -32602, "flags-only listens refused"),
            ),
            1 => respond_raw(w, 500, &[], "boom"),
            _ => {
                let _ = w.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                let _ = w.flush();
            }
        };
    }
    if flags_only {
        std::thread::sleep(Duration::from_millis(
            server.slow_flags_head_ms.load(Ordering::SeqCst),
        ));
    }
    // What it takes of the filter: the URIs (less `narrow`), and a
    // list-changed flag only if it offers that notification.
    let offered = server.list_changed.lock().unwrap().clone();
    let mut filter = asked.clone();
    if let Some(f) = filter.as_object_mut() {
        f.retain(|k, v| {
            k == "resourceSubscriptions"
                || (k == "toolsListChanged" && offered.contains(&"tools") && v == true)
                || (k == "promptsListChanged" && offered.contains(&"prompts") && v == true)
        });
    }
    let narrow = server.narrow.lock().unwrap().clone();
    if let Some(uris) = filter["resourceSubscriptions"].as_array_mut() {
        uris.retain(|u| !narrow.iter().any(|n| u == n));
    }
    let uris: Vec<String> = filter["resourceSubscriptions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u.as_str().map(str::to_string))
        .collect();
    let meta = json!({SUBSCRIPTION_ID: id});
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    let mut frames = vec![json!({
        "jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
        "params": {"_meta": meta, "notifications": filter}
    })];
    for u in &uris {
        frames.push(json!({
            "jsonrpc": "2.0", "method": "notifications/resources/updated",
            "params": {"_meta": meta, "uri": u}
        }));
    }
    for f in frames {
        out.push_str(&format!("data: {f}\n\n"));
    }
    if !server.hold_listen.load(Ordering::SeqCst) {
        let _ = w.write_all(out.as_bytes());
        let _ = w.flush();
        std::thread::sleep(Duration::from_millis(50));
        return;
    }
    // Held: counted and reachable by `push` and `restart` before the
    // acknowledgment leaves, so a client that has it can count on both. The
    // lock keeps a push from interleaving with these frames.
    let peer = w.peer_addr().unwrap();
    {
        let mut held = server.listens.lock().unwrap();
        server.open_listens.fetch_add(1, Ordering::SeqCst);
        held.push(Held {
            peer,
            id,
            uris,
            stream: w.try_clone().unwrap(),
        });
        let _ = w.write_all(out.as_bytes());
        let _ = w.flush();
    }
    // Until the client hangs up, or a restart shuts the stream down.
    let _ = w.set_read_timeout(Some(Duration::from_millis(50)));
    let mut byte = [0u8; 1];
    loop {
        match w.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    server.listens.lock().unwrap().retain(|h| h.peer != peer);
    server.open_listens.fetch_sub(1, Ordering::SeqCst);
}

/// What a 2026-07-28 connection never does: send what the server refuses,
/// handshake, open a `GET` stream (or any request but a `POST`), or carry a
/// session.
fn assert_stateless_wire(server: &Server) {
    let refused = server.refused.lock().unwrap().clone();
    assert!(refused.is_empty(), "the server refused: {refused:?}");
    for r in server.seen.lock().unwrap().iter() {
        assert!(r.line.starts_with("POST"), "{}", r.line);
        assert!(
            !matches!(r.method(), "initialize" | "notifications/initialized"),
            "a handshake at 2026-07-28: {}",
            r.method()
        );
        assert_eq!(
            r.header("mcp-session-id"),
            None,
            "{} carried a session",
            r.method()
        );
    }
}

/// A connection started the way `lifecycle` starts one.
fn connect(ep: &str, lifecycle: Lifecycle) -> McpClient {
    let first = match lifecycle {
        Lifecycle::Handshake => FirstRequest::Initialize,
        Lifecycle::Stateless => FirstRequest::Discover,
    };
    let mut c = McpClient::connect("forgetful", ep, vec![], Duration::from_secs(5))
        .expect("connect")
        .start_with(first);
    c.initialize().expect("initialize");
    c
}

/// A 2026-07-28 connection, made through the SDK backend directly, which says
/// what its listen holds ([`RmcpClient::subscribed`]). `timeout` is the
/// connection's own, which also bounds each silence on a listen stream.
fn listening(server: &Arc<Server>, timeout: Duration) -> RmcpClient {
    let ep = spawn(Arc::clone(server), Lifecycle::Stateless);
    RmcpBuilder::new("forgetful", &ep, vec![], timeout)
        .start_with(FirstRequest::Discover)
        .connect()
        .expect("connect")
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_forgotten_session_is_said_and_a_redial_brings_back_every_subscription() {
    let server: Arc<Server> = Arc::default();
    let ep = spawn(Arc::clone(&server), Lifecycle::Handshake);
    let client = connect(&ep, Lifecycle::Handshake);
    client.subscribe("file:///a").expect("subscribe a");
    client.subscribe("file:///b").expect("subscribe b");
    assert!(!client.session_lost());

    // The server restarts. Its sessions, and every subscription they held,
    // are gone.
    server.restart();

    match client.list_tools() {
        Err(McpError::SessionExpired(m)) => assert!(m.contains("tools/list"), "{m}"),
        other => panic!("a forgotten session must be said as one: {other:?}"),
    }
    assert!(client.session_lost());
    assert!(client.redial_due(), "the first re-dial is due at once");
    // The SDK did not handshake again behind the host's back: the host is the
    // one that knows the subscriptions to restore.
    assert_eq!(server.count("initialize"), 1);

    let lost = client
        .redial_within(Duration::from_secs(5))
        .expect("redial");
    assert_eq!(lost, vec!["file:///a".to_string(), "file:///b".to_string()]);
    assert_eq!(server.count("initialize"), 2);
    assert!(
        !client.session_lost(),
        "the new connection has a live session"
    );

    // Nothing is subscribed on the new connection until the host asks: the
    // set is rebuilt from what the server accepts, so both reach it again.
    for u in &lost {
        client.subscribe(u).expect("re-subscribe");
    }
    let subscribed = server.uris("resources/subscribe");
    assert_eq!(
        subscribed,
        ["file:///a", "file:///b", "file:///a", "file:///b"],
        "each URI subscribed again on the new session"
    );
    let fresh = server.session();
    assert!(
        server
            .requests("resources/subscribe")
            .iter()
            .skip(2)
            .all(|r| r.header("mcp-session-id") == Some(fresh.as_str())),
        "the re-subscribes ride the new session"
    );
    client.list_tools().expect("the re-dialed connection works");
}

#[test]
fn an_idle_connection_learns_of_a_forgotten_session_from_its_notification_stream() {
    // The daemon's shape: subscribed, idle, making no calls — exactly when a
    // restarted server would otherwise go unnoticed. The notification stream
    // drops with the restart, its redial carries the dead session, and the
    // `404` it gets is the signal.
    let server: Arc<Server> = Arc::default();
    let ep = spawn(Arc::clone(&server), Lifecycle::Handshake);
    let client = connect(&ep, Lifecycle::Handshake);
    client.subscribe("file:///a").expect("subscribe");
    wait_until("the notification stream to open", || {
        !server.streams.lock().unwrap().is_empty()
    });

    server.restart();

    wait_until("the stream's redial to find the session gone", || {
        client.session_lost()
    });
    assert_eq!(server.count("initialize"), 1, "no silent re-initialize");
}

#[test]
fn a_notification_on_a_forgotten_session_is_the_sdks_session_expired() {
    // The messages the SDK waits on the HEAD of — `initialize`, a
    // notification — report a forgotten session in the SDK's own terms, which
    // with `reinit_on_expired_session(false)` reach the caller instead of a
    // handshake made behind its back.
    use rmcp::model::{ClientJsonRpcMessage, ClientNotification, InitializedNotification};
    use rmcp::transport::streamable_http_client::{StreamableHttpClient, StreamableHttpError};

    let server: Arc<Server> = Arc::default();
    let ep = spawn(Arc::clone(&server), Lifecycle::Handshake);
    let http = Arc::new(mcp::http::HttpTransport::new(
        mcp::http::McpEndpoint::parse(&ep).unwrap(),
        vec![],
    ));
    // A session, issued the way the handshake gets one.
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                      "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                                 "clientInfo": {"name": "t", "version": "0"}}});
    http.send(
        Some(1),
        init.to_string().as_bytes(),
        Duration::from_secs(5),
        &[],
        |_| {},
    )
    .expect("initialize");
    assert_eq!(http.session_id().as_deref(), Some("s-0"));

    server.restart();

    let agentd = mcp::rmcp_transport::AgentdHttp::new(Arc::clone(&http), Duration::from_secs(5));
    let note = ClientJsonRpcMessage::notification(ClientNotification::InitializedNotification(
        InitializedNotification {
            method: Default::default(),
            extensions: Default::default(),
        },
    ));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let out =
        rt.block_on(agentd.post_message(Arc::from(ep.as_str()), note, None, None, HashMap::new()));
    assert!(
        matches!(out, Err(StreamableHttpError::SessionExpired)),
        "a 404 on a session is SessionExpired: {out:?}"
    );
    assert!(http.session_lost());
}

#[test]
fn a_server_without_the_subscribe_capability_is_refused_without_a_call() {
    // A server that never offered subscriptions would accept the call, or
    // ignore the URI in a listen filter, and never notify: a wait parked for
    // ever. Refused here, by name, before anything is sent — at either
    // revision.
    for lifecycle in LIFECYCLES {
        let server: Arc<Server> = Arc::default();
        server.no_subscribe.store(true, Ordering::SeqCst);
        let ep = spawn(Arc::clone(&server), lifecycle);
        let client = connect(&ep, lifecycle);
        match client.subscribe("file:///a") {
            Err(McpError::Capability(m)) => {
                assert!(m.contains("resources.subscribe"), "{lifecycle:?}: {m}")
            }
            other => panic!("{lifecycle:?}: must refuse, got {other:?}"),
        }
        assert_eq!(server.count("resources/subscribe"), 0, "{lifecycle:?}");
        assert_eq!(server.count("subscriptions/listen"), 0, "{lifecycle:?}");
        if lifecycle == Lifecycle::Stateless {
            assert_stateless_wire(&server);
        }
    }
}

#[test]
fn a_failed_subscribe_is_asked_again_and_only_the_new_uri_is_sent() {
    let server: Arc<Server> = Arc::default();
    server
        .refuse_once
        .lock()
        .unwrap()
        .extend(["file:///a".to_string(), "file:///c".to_string()]);
    let ep = spawn(Arc::clone(&server), Lifecycle::Handshake);
    let client = connect(&ep, Lifecycle::Handshake);

    // Refused once. The retry is a real request, not "already covered".
    client.subscribe("file:///a").expect_err("refused");
    client
        .subscribe("file:///a")
        .expect("the retry reaches the server");
    assert_eq!(
        server.uris("resources/subscribe"),
        ["file:///a", "file:///a"]
    );

    // Adding a URI sends that URI alone — the ones held stay held, and one
    // that fails does not take the next add down with it.
    client.subscribe("file:///b").expect("b");
    client.subscribe("file:///c").expect_err("c refused");
    client.subscribe("file:///d").expect("d, after c failed");
    assert_eq!(
        server.uris("resources/subscribe"),
        [
            "file:///a",
            "file:///a",
            "file:///b",
            "file:///c",
            "file:///d"
        ]
    );
}

#[test]
fn a_listen_stream_the_server_closes_is_opened_again_and_said() {
    let server: Arc<Server> = Arc::default();
    let client = listening(&server, Duration::from_secs(5));
    client.subscribe("file:///a").expect("listen");

    // The server ends every listen stream after one update; the pump opens it
    // again — once at the shortest backoff, then after twice that.
    wait_until("the listen to be opened again", || {
        server.count("subscriptions/listen") >= 3
    });
    let mut events = Vec::new();
    let mut updates = 0;
    wait_until("the pump to report the re-listens", || {
        events.extend(client.drain_listen_events());
        updates += client
            .drain_notifications()
            .iter()
            .filter(|n| n.method == "notifications/resources/updated")
            .count();
        events
            .iter()
            .filter(|e| **e == ListenEvent::Resumed)
            .count()
            >= 2
    });
    assert!(
        matches!(&events[0], ListenEvent::Ended { retry_ms, .. } if *retry_ms == 250),
        "the end is said, with the wait before the retry: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ListenEvent::Ended { retry_ms, .. } if *retry_ms == 500)),
        "a stream that keeps ending backs off: {events:?}"
    );
    assert!(
        updates >= 2,
        "updates keep arriving across the re-listens: {events:?}"
    );
    assert_stateless_wire(&server);
}

#[test]
fn widening_a_listen_retires_the_stream_it_replaces() {
    // Adding a URI opens a listen with the wider filter. The pump of the
    // narrower one must stop with it — a dropped task handle does not stop
    // a task, and left running it would keep re-listening with the old filter
    // beside the new one, every wake for the first URI arriving twice.
    let server: Arc<Server> = Arc::default();
    let client = listening(&server, Duration::from_secs(5));
    client.subscribe("file:///a").expect("listen a");
    client.subscribe("file:///b").expect("listen a,b");
    let widened = server.count("subscriptions/listen");
    wait_until("the wider listen to be opened again twice", || {
        server
            .uris("subscriptions/listen")
            .iter()
            .filter(|u| *u == "file:///a,file:///b")
            .count()
            >= 3
    });
    let after: Vec<String> = server
        .uris("subscriptions/listen")
        .into_iter()
        .skip(widened)
        .collect();
    assert!(
        after.iter().all(|u| u == "file:///a,file:///b"),
        "only the wider filter is listened on once it replaced the narrow one: {after:?}"
    );
    assert_stateless_wire(&server);
}

#[test]
fn widening_a_held_listen_hangs_up_the_one_it_replaces() {
    // At 2026-07-28 a client cancels a listen by closing its stream: there is
    // no `notifications/cancelled` to send. Widening the filter opens a new
    // listen and drops the old one, and unless dropping it closes the
    // connection, the server goes on holding the old subscription — and a
    // socket, and a thread on each side — until the connection's timeout, a
    // minute here, far past the three seconds allowed.
    let server: Arc<Server> = Arc::default();
    server.hold_listen.store(true, Ordering::SeqCst);
    let client = listening(&server, Duration::from_secs(60));
    client.subscribe("file:///a").expect("listen a");
    wait_until("the first listen to be held", || server.open_listens() == 1);

    client.subscribe("file:///b").expect("listen a,b");
    let deadline = Instant::now() + Duration::from_secs(3);
    while server.open_listens() != 1 {
        assert!(
            Instant::now() < deadline,
            "the replaced listen is still open: {} held",
            server.open_listens()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The one still open is the wider one.
    let held: Vec<Vec<String>> = server
        .listens
        .lock()
        .unwrap()
        .iter()
        .map(|h| h.uris.clone())
        .collect();
    assert_eq!(held, [["file:///a", "file:///b"]]);
    assert_eq!(
        server.uris("subscriptions/listen"),
        ["file:///a", "file:///a,file:///b"]
    );
    assert_eq!(client.subscribed(), ["file:///a", "file:///b"]);
    assert_stateless_wire(&server);
}

#[test]
fn the_listen_pump_re_listens_after_its_stream_ends_whichever_way_it_ends() {
    // A listen stream that ends carries no more wakes, however it ends: the
    // connection closes, the server sends the listen's final result, or it
    // cancels the listen. Each end is said, the pump listens again, and the
    // new listen is one the server's next update reaches.
    let ends = [
        (ListenEnd::Close, None),
        (ListenEnd::Complete, Some("complete")),
        (ListenEnd::Cancelled, Some("cancel")),
    ];
    for (end, reason_says) in ends {
        let server: Arc<Server> = Arc::default();
        server.hold_listen.store(true, Ordering::SeqCst);
        *server.listen_end.lock().unwrap() = end;
        let client = listening(&server, Duration::from_secs(5));
        client.subscribe("file:///a").expect("listen");
        wait_until("the listen to be held", || server.open_listens() == 1);

        server.restart();

        let mut events = Vec::new();
        wait_until("the pump to listen again", || {
            events.extend(client.drain_listen_events());
            events.contains(&ListenEvent::Resumed)
        });
        match &events[..] {
            [
                ListenEvent::Ended {
                    reason,
                    retry_ms: 250,
                    idle: false,
                },
                ListenEvent::Resumed,
            ] => {
                if let Some(word) = reason_says {
                    assert!(reason.contains(word), "{end:?}: {reason}");
                }
            }
            other => panic!("{end:?}: {other:?}"),
        }
        assert_eq!(server.count("subscriptions/listen"), 2, "{end:?}");

        // A wake sent after the resume reaches the host.
        server.push("file:///a");
        let mut pushed = false;
        wait_until("the update sent after the resume", || {
            pushed |= client.drain_notifications().iter().any(|n| {
                n.method == "notifications/resources/updated"
                    && n.params
                        .as_ref()
                        .is_some_and(|p| p["_meta"][PUSHED] == true)
            });
            pushed
        });
        assert_eq!(client.subscribed(), ["file:///a"], "{end:?}");
        assert_stateless_wire(&server);
    }
}

#[test]
fn a_restarted_2026_07_28_server_is_a_re_listen_not_a_lost_session() {
    // A 2025-11-25 server that restarts forgets the session, and the host
    // must re-dial and re-subscribe. A 2026-07-28 server has no session to
    // forget: its restart ends the listen stream, the pump listens again, and
    // the connection is otherwise as it was — no re-dial, no handshake.
    let server: Arc<Server> = Arc::default();
    server.hold_listen.store(true, Ordering::SeqCst);
    let ep = spawn(Arc::clone(&server), Lifecycle::Stateless);
    let client = connect(&ep, Lifecycle::Stateless);
    client.subscribe("file:///a").expect("listen");
    wait_until("the listen to be held", || server.open_listens() == 1);

    server.restart();

    let mut events = Vec::new();
    wait_until("the pump to listen again", || {
        assert!(
            !client.session_lost(),
            "a 2026-07-28 restart lost a session"
        );
        events.extend(client.drain_listen_events());
        events.contains(&ListenEvent::Resumed)
    });
    assert!(
        matches!(
            &events[..],
            [ListenEvent::Ended { .. }, ListenEvent::Resumed]
        ),
        "{events:?}"
    );
    assert!(!client.session_lost());
    assert!(!client.redial_due());
    client.list_tools().expect("the connection still works");
    assert!(!client.session_lost());
    assert_eq!(server.count("server/discover"), 1, "no re-dial");
    assert_stateless_wire(&server);
}

#[test]
fn a_redial_the_server_refuses_keeps_the_lost_uris_and_backs_off() {
    let server: Arc<Server> = Arc::default();
    let ep = spawn(Arc::clone(&server), Lifecycle::Handshake);
    let client = connect(&ep, Lifecycle::Handshake);
    client.subscribe("file:///a").expect("subscribe");
    server.restart();
    let _ = client.list_tools();
    assert!(client.redial_due());

    // Still coming back up: the handshake is refused.
    server.refuse_handshake.store(true, Ordering::SeqCst);
    client
        .redial_within(Duration::from_secs(5))
        .expect_err("refused");
    assert_eq!(client.redial_failures(), 1);
    assert!(
        client.session_lost(),
        "the lost connection stays until one replaces it"
    );
    assert!(
        !client.redial_due(),
        "a failed re-dial is not retried on the next tick"
    );

    // Up again, and the wait over: the URIs the lost session held are still
    // there to restore.
    server.refuse_handshake.store(false, Ordering::SeqCst);
    wait_until("the backoff to pass", || client.redial_due());
    let lost = client
        .redial_within(Duration::from_secs(5))
        .expect("redial");
    assert_eq!(lost, ["file:///a"]);
    assert_eq!(
        client.redial_failures(),
        0,
        "a success starts the count over"
    );
}

#[test]
fn a_listen_acknowledged_without_the_uri_is_refused_and_not_recorded() {
    // rmcp lets a server acknowledge a narrower filter than it was asked for,
    // and then drops every update outside it. Recorded anyway, the URI read
    // as "already covered" on every later try while nothing ever woke it.
    let server: Arc<Server> = Arc::default();
    let client = listening(&server, Duration::from_secs(5));
    client.subscribe("file:///a").expect("a");
    server.narrow.lock().unwrap().push("file:///b".into());
    match client.subscribe("file:///b") {
        Err(McpError::Capability(m)) => assert!(m.contains("file:///b"), "{m}"),
        other => panic!("a URI the server left out must be refused: {other:?}"),
    }
    assert_eq!(
        client.subscribed(),
        ["file:///a"],
        "only what was acknowledged"
    );

    // The server takes it now: asking again reaches it, and holds.
    server.narrow.lock().unwrap().clear();
    let before = server.count("subscriptions/listen");
    client
        .subscribe("file:///b")
        .expect("b, acknowledged this time");
    assert_eq!(server.count("subscriptions/listen"), before + 1);
    assert_eq!(client.subscribed(), ["file:///a", "file:///b"]);
    assert_stateless_wire(&server);
}

#[test]
fn a_refused_listen_is_asked_again() {
    // The 2026-07-28 half of "a failed subscribe is retried for real".
    let server: Arc<Server> = Arc::default();
    let client = listening(&server, Duration::from_secs(5));
    server.refuse_listen_once.store(true, Ordering::SeqCst);
    client.subscribe("file:///a").expect_err("refused");
    assert!(client.subscribed().is_empty());
    client
        .subscribe("file:///a")
        .expect("the retry reaches the server");
    assert_eq!(
        server.uris("subscriptions/listen")[..2],
        ["file:///a", "file:///a"]
    );
    assert_eq!(client.subscribed(), ["file:///a"]);
    assert_stateless_wire(&server);
}

#[test]
fn a_re_listen_acknowledged_narrower_drops_the_uris_and_says_so() {
    // The pump's own re-listen gets the same check: a URI the server stopped
    // taking leaves the accepted set, and the host hears which.
    let server: Arc<Server> = Arc::default();
    let client = listening(&server, Duration::from_secs(5));
    client.subscribe("file:///a").expect("a");
    client.subscribe("file:///b").expect("a,b");
    server.narrow.lock().unwrap().push("file:///b".into());
    let mut events = Vec::new();
    wait_until("the re-listen to come back narrower", || {
        events.extend(client.drain_listen_events());
        events
            .iter()
            .any(|e| matches!(e, ListenEvent::Narrowed { .. }))
    });
    assert!(
        events.contains(&ListenEvent::Narrowed {
            dropped: vec!["file:///b".into()]
        }),
        "{events:?}"
    );
    assert_eq!(client.subscribed(), ["file:///a"]);
    assert_stateless_wire(&server);
}

#[test]
fn an_update_queued_on_the_lost_connection_survives_the_redial() {
    // The server's last wake before it restarted reached the old connection,
    // and the host had not drained it yet. The re-dial replaces the
    // connection whole; the update must not go with it.
    let server: Arc<Server> = Arc::default();
    let ep = spawn(Arc::clone(&server), Lifecycle::Handshake);
    let client = connect(&ep, Lifecycle::Handshake);
    client.subscribe("file:///a").expect("subscribe");
    wait_until("the notification stream to open", || {
        !server.streams.lock().unwrap().is_empty()
    });
    server.push("file:///a");
    std::thread::sleep(Duration::from_millis(300));

    server.restart();
    let _ = client.list_tools();
    assert!(client.session_lost());
    client
        .redial_within(Duration::from_secs(5))
        .expect("redial");
    let updates: Vec<_> = client
        .drain_notifications()
        .into_iter()
        .filter(|n| n.method == "notifications/resources/updated")
        .collect();
    assert_eq!(
        updates.len(),
        1,
        "the queued update is still there to drain"
    );
}

#[test]
fn a_server_that_loses_every_new_session_is_re_dialed_on_a_growing_wait() {
    // A server that answers the notification stream's `GET` with `404` makes
    // every session look lost the moment it opens. Each handshake succeeds,
    // so a schedule that started over on every success re-dialed it about
    // once a second, for ever.
    let server: Arc<Server> = Arc::default();
    server.get_404.store(true, Ordering::SeqCst);
    let ep = spawn(Arc::clone(&server), Lifecycle::Handshake);
    let client = connect(&ep, Lifecycle::Handshake);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        if client.redial_due() {
            client
                .redial_within(Duration::from_secs(5))
                .expect("the handshake itself succeeds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Re-dials at 0, +1s and +3s, the next due at +7s: three, where a reset
    // schedule makes one per GET retry.
    let redials = server.count("initialize") - 1;
    assert!(
        (2..=3).contains(&redials),
        "re-dialed {redials} times in 4s"
    );
}

#[test]
fn a_bounded_subscribe_gives_up_at_its_bound() {
    // A `resources/subscribe` at 2025-11-25, a listen's acknowledgment at
    // 2026-07-28: either way a slow server is a timeout at the bound.
    for lifecycle in LIFECYCLES {
        let server: Arc<Server> = Arc::default();
        server.slow_subscribe_ms.store(3_000, Ordering::SeqCst);
        let ep = spawn(Arc::clone(&server), lifecycle);
        let client = connect(&ep, lifecycle);
        let t = Instant::now();
        match client.subscribe_within("file:///a", Duration::from_millis(200)) {
            Err(McpError::Transport(m)) => assert!(m.contains("timed out"), "{lifecycle:?}: {m}"),
            other => panic!("{lifecycle:?}: a slow server is a timeout at the bound: {other:?}"),
        }
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "{lifecycle:?}: {:?}",
            t.elapsed()
        );
        assert!(
            client.subscribe("file:///a").is_ok(),
            "{lifecycle:?}: unbounded, the same subscribe waits the server out"
        );
        match lifecycle {
            Lifecycle::Handshake => assert_eq!(server.count("resources/subscribe"), 2),
            Lifecycle::Stateless => {
                assert!(server.count("subscriptions/listen") >= 2);
                assert_eq!(server.count("resources/subscribe"), 0);
                assert_stateless_wire(&server);
            }
        }
    }
}

// --------------------------------------------------- listens at 2026-07-28

/// A 2026-07-28 connection with its own timeout and listen idle bound.
fn listening_with(server: &Arc<Server>, timeout: Duration, idle: Duration) -> RmcpClient {
    let ep = spawn(Arc::clone(server), Lifecycle::Stateless);
    RmcpBuilder::new("forgetful", &ep, vec![], timeout)
        .start_with(FirstRequest::Discover)
        .with_listen_idle(idle)
        .connect()
        .expect("connect")
}

/// Wait for the next listen event matching `want`, keeping every event seen.
fn wait_event(
    c: &RmcpClient,
    seen: &mut Vec<ListenEvent>,
    what: &str,
    want: impl Fn(&ListenEvent) -> bool,
) {
    wait_until(what, || {
        seen.extend(c.drain_listen_events());
        seen.iter().any(&want)
    });
}

/// The `resources/updated` the pump queued itself after a re-listen: the
/// server's own updates carry `_meta`, a resync's carries only the URI.
fn synthetic(notes: &[mcp::rpc::Notification]) -> Vec<String> {
    notes
        .iter()
        .filter(|n| n.method == "notifications/resources/updated")
        .filter_map(|n| n.params.as_ref())
        .filter(|p| p.get("_meta").is_none())
        .filter_map(|p| p["uri"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn a_quiet_listen_outlives_the_connection_timeout_and_an_idle_cut_is_said_as_idle() {
    let server = Arc::new(Server::default());
    server.hold_listen.store(true, Ordering::SeqCst);
    let c = listening_with(&server, Duration::from_secs(1), Duration::from_secs(3));
    c.subscribe("demo://a").expect("subscribe");
    let acked = Instant::now();
    let mut events = Vec::new();
    // Quiet past the connection's 1 s: still open.
    while acked.elapsed() < Duration::from_millis(2500) {
        events.extend(c.drain_listen_events());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(events.is_empty(), "cut before its idle bound: {events:?}");
    wait_event(&c, &mut events, "the idle cut", |e| {
        matches!(e, ListenEvent::Ended { idle: true, .. })
    });
    wait_event(&c, &mut events, "the re-listen", |e| {
        *e == ListenEvent::Resumed
    });
    // Any other end is not idle.
    events.clear();
    server.restart();
    wait_event(&c, &mut events, "the closed stream", |e| {
        matches!(e, ListenEvent::Ended { .. })
    });
    assert!(
        matches!(events[0], ListenEvent::Ended { idle: false, .. }),
        "{events:?}"
    );
}

#[test]
fn a_listen_idle_bound_below_the_connection_timeout_is_raised_to_it() {
    let server = Arc::new(Server::default());
    server.hold_listen.store(true, Ordering::SeqCst);
    let c = listening_with(&server, Duration::from_secs(3), Duration::from_secs(1));
    c.subscribe("demo://a").expect("subscribe");
    let acked = Instant::now();
    let mut events = Vec::new();
    while acked.elapsed() < Duration::from_millis(2500) {
        events.extend(c.drain_listen_events());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(events.is_empty(), "cut at the 1 s idle bound: {events:?}");
    wait_event(&c, &mut events, "the idle cut", |e| {
        matches!(e, ListenEvent::Ended { idle: true, .. })
    });
}

/// Subscribe `uris` on a held listen and let the pump settle: the baseline
/// read, the updates the listen opens with and their re-reads, then long
/// enough that those reads count as seen on a live stream.
fn settled(server: &Arc<Server>, uris: &[&str]) -> RmcpClient {
    server.hold_listen.store(true, Ordering::SeqCst);
    let c = listening_with(server, Duration::from_secs(5), Duration::from_secs(60));
    for u in uris {
        c.subscribe(u).expect("subscribe");
    }
    // A baseline read and a re-read per URI on the last listen.
    wait_until("the pump's reads", || {
        server.count("resources/read") >= 2 * uris.len()
    });
    std::thread::sleep(Duration::from_millis(1300));
    let _ = c.drain_notifications();
    let _ = c.drain_listen_events();
    c
}

/// End every held listen and change nothing, or what `in_gap` changes, while
/// the next one is being opened; wait until the re-listen's resync is done.
fn re_listen(server: &Arc<Server>, c: &RmcpClient, in_gap: impl FnOnce()) {
    let reads = server.count("resources/read");
    let held = c.subscribed().len();
    server.slow_subscribe_ms.store(300, Ordering::SeqCst);
    server.restart();
    in_gap();
    let mut events = Vec::new();
    wait_event(c, &mut events, "the re-listen", |e| {
        *e == ListenEvent::Resumed
    });
    wait_until("the resync's reads", || {
        server.count("resources/read") >= reads + held
    });
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
fn a_change_during_a_re_listen_gap_wakes_once_and_an_unchanged_resource_does_not() {
    let server = Arc::new(Server::default());
    let c = settled(&server, &["demo://a", "demo://b"]);
    re_listen(&server, &c, || {
        server
            .content
            .lock()
            .unwrap()
            .insert("demo://a".into(), "changed in the gap".into());
    });
    assert_eq!(synthetic(&c.drain_notifications()), ["demo://a"]);
}

#[test]
fn a_live_update_moves_the_baseline() {
    let server = Arc::new(Server::default());
    let c = settled(&server, &["demo://a"]);
    // Changed and notified while the listen is live: the host is woken by
    // the update itself, and the gap that follows changes nothing.
    server
        .content
        .lock()
        .unwrap()
        .insert("demo://a".into(), "changed live".into());
    let reads = server.count("resources/read");
    server.push("demo://a");
    wait_until("the live update's re-read", || {
        server.count("resources/read") > reads
    });
    std::thread::sleep(Duration::from_millis(1300));
    let _ = c.drain_notifications();
    re_listen(&server, &c, || {});
    assert!(synthetic(&c.drain_notifications()).is_empty());
}

#[test]
fn a_read_that_fails_after_a_re_listen_wakes_conservatively() {
    let server = Arc::new(Server::default());
    let c = settled(&server, &["demo://a"]);
    re_listen(&server, &c, || {
        server.fail_reads.store(true, Ordering::SeqCst)
    });
    assert_eq!(synthetic(&c.drain_notifications()), ["demo://a"]);
}

/// A 2026-07-28 connection, through the client hosts use, to a server
/// advertising `list_changed` for `kinds`.
fn standing(server: &Arc<Server>, kinds: &[&'static str]) -> McpClient {
    *server.list_changed.lock().unwrap() = kinds.to_vec();
    server.hold_listen.store(true, Ordering::SeqCst);
    let ep = spawn(Arc::clone(server), Lifecycle::Stateless);
    connect(&ep, Lifecycle::Stateless)
}

fn drained(c: &McpClient, method: &str) -> bool {
    c.drain_notifications().iter().any(|n| n.method == method)
}

#[test]
fn a_standing_listen_carries_list_changed_without_any_uri() {
    for (kind, flag, note) in [
        (
            "tools",
            "toolsListChanged",
            "notifications/tools/list_changed",
        ),
        (
            "prompts",
            "promptsListChanged",
            "notifications/prompts/list_changed",
        ),
    ] {
        let server = Arc::new(Server::default());
        let c = standing(&server, &[kind]);
        wait_until("the standing listen", || server.open_listens() == 1);
        let listens = server.flags_only_listens();
        assert_eq!(listens.len(), 1, "{kind}");
        assert_eq!(
            listens[0].body["params"]["notifications"][flag], true,
            "{kind}"
        );
        server.notify(note);
        wait_until(note, || drained(&c, note));
    }
    // Nothing advertised: no listen at all.
    let server = Arc::new(Server::default());
    let _c = standing(&server, &[]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(server.count("subscriptions/listen"), 0);
}

#[test]
fn unsubscribing_the_last_uri_keeps_the_standing_listen() {
    let server = Arc::new(Server::default());
    let c = standing(&server, &["tools"]);
    wait_until("the standing listen", || server.open_listens() == 1);
    c.subscribe("demo://a").expect("subscribe");
    c.unsubscribe("demo://a").expect("unsubscribe");
    let last = server.requests("subscriptions/listen").pop().unwrap();
    let filter = &last.body["params"]["notifications"];
    assert_eq!(filter["toolsListChanged"], true, "{filter}");
    assert!(filter["resourceSubscriptions"].is_null(), "{filter}");
    wait_until("one listen held", || server.open_listens() == 1);
    server.notify("notifications/tools/list_changed");
    wait_until("the list_changed", || {
        drained(&c, "notifications/tools/list_changed")
    });
}

#[test]
fn a_handshake_connection_opens_no_listen() {
    // The 2025-11-25 server advertises `resources.listChanged`, which reaches
    // a handshake connection on its `GET` stream: no listen is opened for it.
    let server = Arc::new(Server::default());
    let ep = spawn(Arc::clone(&server), Lifecycle::Handshake);
    let _c = connect(&ep, Lifecycle::Handshake);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(server.count("subscriptions/listen"), 0);
}

#[test]
fn a_refused_standing_listen_does_not_fail_the_connect_and_is_said_once() {
    for how in 0..3 {
        let server = Arc::new(Server::default());
        server.refuse_flags_only.store(3, Ordering::SeqCst);
        server.refuse_flags_how.store(how, Ordering::SeqCst);
        let c = standing(&server, &["tools"]);
        c.list_tools().expect("the connection works without it");
        let mut events = Vec::new();
        wait_until("the standing listen held", || {
            events.extend(c.drain_listen_events());
            events.contains(&ListenEvent::Resumed)
        });
        let unavailable = events
            .iter()
            .filter(|e| matches!(e, ListenEvent::Unavailable { .. }))
            .count();
        assert_eq!(
            unavailable, 1,
            "said once, refused three times ({how}): {events:?}"
        );
        assert_eq!(server.flags_only_listens().len(), 4, "({how})");
        wait_until("the accepted listen open", || server.open_listens() == 1);
        server.notify("notifications/tools/list_changed");
        wait_until("the list_changed", || {
            drained(&c, "notifications/tools/list_changed")
        });
    }
}

#[test]
fn a_slow_standing_listen_does_not_delay_the_connect() {
    let server = Arc::new(Server::default());
    *server.list_changed.lock().unwrap() = vec!["tools"];
    server.hold_listen.store(true, Ordering::SeqCst);
    server.slow_flags_head_ms.store(3000, Ordering::SeqCst);
    let ep = spawn(Arc::clone(&server), Lifecycle::Stateless);
    let started = Instant::now();
    let c = connect(&ep, Lifecycle::Stateless);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    wait_until("the standing listen", || server.open_listens() == 1);
    server.notify("notifications/tools/list_changed");
    wait_until("the list_changed", || {
        drained(&c, "notifications/tools/list_changed")
    });
}

#[test]
fn a_server_refusing_the_list_changed_flag_keeps_its_uri_subscriptions() {
    // It advertises `tools.listChanged` and refuses any filter asking for it:
    // the standing listen can never open, and a URI must still be watched.
    let server = Arc::new(Server::default());
    server.refuse_flagged.store(true, Ordering::SeqCst);
    let c = standing(&server, &["tools"]);
    c.subscribe("demo://a")
        .expect("subscribed without the flag");
    let last = server.requests("subscriptions/listen").pop().unwrap();
    assert!(last.body["params"]["notifications"]["toolsListChanged"].is_null());
    let mut events = Vec::new();
    wait_until("the downgrade said", || {
        events.extend(c.drain_listen_events());
        events
            .iter()
            .any(|e| matches!(e, ListenEvent::FlagsRefused { .. }))
    });
    wait_until("one listen held", || server.open_listens() == 1);
    let _ = c.drain_notifications();
    server.push("demo://a");
    wait_until("the update", || {
        c.drain_notifications()
            .iter()
            .any(|n| n.method == "notifications/resources/updated")
    });
}
