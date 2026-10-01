// SPDX-License-Identifier: AGPL-3.0-only
//! Resource subscriptions against a server that misbehaves the ways real ones
//! do: one that restarts and forgets the session, one that closes a listen
//! stream, one that never offered subscriptions, and one that refuses a
//! subscribe once.
//!
//! Every one of these used to fail SILENTLY — a subscription that was gone, a
//! wake that never came, a URI reported as "already covered" that the server
//! never accepted — so each test counts what reached the server, which is the
//! only arbiter of whether a subscription exists.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mcp::client::{McpClient, McpError};
use mcp::rmcp_client::{ListenEvent, RmcpBuilder};
use rmcp::model::ProtocolVersion;
use serde_json::{Value, json};

/// One call that reached the server: `(method, session it carried, uri)`.
type Seen = (String, Option<String>, Option<String>);

/// The server's side of the story, shared with the test body.
#[derive(Default)]
struct Server {
    /// Bumped by a "restart": every session issued before it is forgotten,
    /// and a request that carries one is answered `404`.
    generation: AtomicU64,
    /// Leave `resources.subscribe` out of the capabilities.
    no_subscribe: AtomicBool,
    /// Answer `initialize` with `503`: a server still coming back up.
    refuse_handshake: AtomicBool,
    /// Refuse a `resources/subscribe` for these URIs, once each.
    refuse_once: Mutex<Vec<String>>,
    /// Every `(method, session the request carried, uri param)` that reached it.
    seen: Mutex<Vec<Seen>>,
    /// Open `GET` notification streams, closed by a restart the way a process
    /// that exits closes its sockets.
    streams: Mutex<Vec<TcpStream>>,
}

impl Server {
    fn session(&self) -> String {
        format!("s-{}", self.generation.load(Ordering::SeqCst))
    }

    /// Forget every session and drop every open stream.
    fn restart(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        for s in self.streams.lock().unwrap().drain(..) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    fn count(&self, method: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m == method)
            .count()
    }

    /// The URIs `method` was called with, in order.
    fn uris(&self, method: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m == method)
            .filter_map(|(_, _, u)| u.clone())
            .collect()
    }
}

struct Request {
    method: String,
    session: Option<String>,
    body: Vec<u8>,
}

fn read_request(r: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut start = String::new();
    if r.read_line(&mut start).ok()? == 0 {
        return None;
    }
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method: start.split_whitespace().next().unwrap_or("").to_string(),
        session: headers.get("mcp-session-id").cloned(),
        body,
    })
}

fn respond(w: &mut TcpStream, body: &Value, session: Option<&str>) {
    let b = serde_json::to_vec(body).unwrap();
    let sid = session
        .map(|s| format!("Mcp-Session-Id: {s}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{sid}Content-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    );
    let _ = w.write_all(head.as_bytes());
    let _ = w.write_all(&b);
    let _ = w.flush();
}

/// A Streamable-HTTP MCP server speaking `revision` (2025-11-25 when `None`),
/// with sessions it can forget ([`Server::restart`]).
fn spawn(server: Arc<Server>, revision: Option<&'static str>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let server = Arc::clone(&server);
            std::thread::spawn(move || serve(conn, &server, revision));
        }
    });
    format!("http://{addr}/mcp")
}

fn serve(conn: TcpStream, server: &Server, revision: Option<&'static str>) {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut w = conn.try_clone().unwrap();
    let mut r = BufReader::new(conn);
    let Some(req) = read_request(&mut r) else {
        return;
    };
    // A session this server no longer holds: Streamable HTTP's `404`.
    let stale = req.session.as_ref().is_some_and(|s| *s != server.session());
    if req.method == "GET" {
        if stale {
            let _ = w.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            return;
        }
        if req.session.is_none() {
            let _ = w.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
            return;
        }
        let _ = w.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = w.flush();
        server.streams.lock().unwrap().push(w);
        return;
    }
    if req.method != "POST" {
        let _ = w.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
        return;
    }
    let msg: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // The URI a call names; for a listen, the filter's URIs, comma-joined.
    let uri = match msg["params"]["notifications"]["resourceSubscriptions"].as_array() {
        Some(uris) => Some(
            uris.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(","),
        ),
        None => msg["params"]["uri"].as_str().map(str::to_string),
    };
    if !method.is_empty() {
        server
            .seen
            .lock()
            .unwrap()
            .push((method.clone(), req.session.clone(), uri.clone()));
    }
    if stale && method != "initialize" {
        let _ = w.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return;
    }
    let Some(id) = msg.get("id").cloned() else {
        let _ = w.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    if method == "initialize" && server.refuse_handshake.load(Ordering::SeqCst) {
        let _ = w.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
        return;
    }
    match method.as_str() {
        "initialize" => {
            let resources = if server.no_subscribe.load(Ordering::SeqCst) {
                json!({"listChanged": true})
            } else {
                json!({"subscribe": true, "listChanged": true})
            };
            let session = server.session();
            respond(
                &mut w,
                &ok(json!({
                    "protocolVersion": revision
                        .map(Value::from)
                        .unwrap_or_else(|| msg["params"]["protocolVersion"].clone()),
                    "capabilities": {"tools": {}, "resources": resources},
                    "serverInfo": {"name": "forgetful", "version": "0"}
                })),
                Some(&session),
            );
        }
        "resources/subscribe" => {
            let uri = uri.unwrap_or_default();
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
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "subscribe refused"}})
            } else {
                ok(json!({}))
            };
            respond(&mut w, &body, None);
        }
        "subscriptions/listen" => {
            // Acknowledge the filter, send one update for each URI in it, then
            // close the stream without a final result — a server that dropped
            // the connection, or restarted between two listens.
            let filter = msg["params"]["notifications"].clone();
            let meta = json!({"io.modelcontextprotocol/subscriptionId": id});
            let mut frames = vec![json!({
                "jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
                "params": {"_meta": meta, "notifications": filter}
            })];
            for u in filter["resourceSubscriptions"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                frames.push(json!({
                    "jsonrpc": "2.0", "method": "notifications/resources/updated",
                    "params": {"_meta": meta, "uri": u}
                }));
            }
            let _ = w.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            for f in frames {
                let _ = w.write_all(format!("data: {f}\n\n").as_bytes());
                let _ = w.flush();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        "tools/list" => respond(
            &mut w,
            &ok(json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]})),
            None,
        ),
        _ => respond(&mut w, &ok(json!({})), None),
    }
}

fn connect(ep: &str) -> McpClient {
    let mut c =
        McpClient::connect("forgetful", ep, vec![], Duration::from_secs(5)).expect("connect");
    c.initialize().expect("initialize");
    c
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
    let ep = spawn(Arc::clone(&server), None);
    let client = connect(&ep);
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
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m == "resources/subscribe")
            .skip(2)
            .all(|(_, s, _)| s.as_deref() == Some(fresh.as_str())),
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
    let ep = spawn(Arc::clone(&server), None);
    let client = connect(&ep);
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
    let ep = spawn(Arc::clone(&server), None);
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
    for revision in [None, Some(ProtocolVersion::V_2026_07_28.as_str())] {
        let server: Arc<Server> = Arc::default();
        server.no_subscribe.store(true, Ordering::SeqCst);
        let ep = spawn(Arc::clone(&server), revision);
        let client = connect(&ep);
        match client.subscribe("file:///a") {
            Err(McpError::Capability(m)) => {
                assert!(m.contains("resources.subscribe"), "{m}")
            }
            other => panic!("{revision:?}: must refuse, got {other:?}"),
        }
        assert_eq!(server.count("resources/subscribe"), 0, "{revision:?}");
        assert_eq!(server.count("subscriptions/listen"), 0, "{revision:?}");
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
    let ep = spawn(Arc::clone(&server), None);
    let client = connect(&ep);

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
    let ep = spawn(
        Arc::clone(&server),
        Some(ProtocolVersion::V_2026_07_28.as_str()),
    );
    let client = RmcpBuilder::new("forgetful", &ep, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");
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
}

#[test]
fn widening_a_listen_retires_the_stream_it_replaces() {
    // Adding a URI opens a listen with the wider filter. The pump of the
    // narrower one must stop with it — a dropped task handle does not stop
    // a task, and left running it would keep re-listening with the old filter
    // beside the new one, every wake for the first URI arriving twice.
    let server: Arc<Server> = Arc::default();
    let ep = spawn(
        Arc::clone(&server),
        Some(ProtocolVersion::V_2026_07_28.as_str()),
    );
    let client = RmcpBuilder::new("forgetful", &ep, vec![], Duration::from_secs(5))
        .connect()
        .expect("connect");
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
}

#[test]
fn a_redial_the_server_refuses_keeps_the_lost_uris_and_backs_off() {
    let server: Arc<Server> = Arc::default();
    let ep = spawn(Arc::clone(&server), None);
    let client = connect(&ep);
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
