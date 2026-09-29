// SPDX-License-Identifier: AGPL-3.0-only
//! What a tools/call and the notification stream put on the wire, asserted
//! from the server's side — the only side that can tell.
//!
//! * The persistent `_meta` a host stamps on a connection (run id, instance,
//!   traceparent) reaches the server in `params._meta`, merged with any
//!   per-call meta, and the tool's `arguments` carry the tool's arguments and
//!   nothing else: MCP reserves `params._meta`, and a strict tool schema
//!   (`additionalProperties: false`) refuses a stray `_meta` argument.
//! * A caller's per-call bound governs: a tool that never answers is a timeout
//!   error inside the bound, not a wait for the connection's long default.
//! * The `GET` notification stream carries the operator's credential, the
//!   negotiated `MCP-Protocol-Version`, and on a reconnect the `Last-Event-ID`
//!   it resumes from.
//! * Each operator header goes out exactly once per request: `Authorization`
//!   is not a list-valued field (RFC 9110 §5.3), so a doubled one is ambiguous.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mcp::client::McpClient;
use serde_json::{Value, json};

/// One request as the stub saw it.
#[derive(Clone, Debug)]
struct Req {
    line: String,
    /// Every header line, name lowercased, in arrival order — duplicates kept,
    /// because counting them is the point.
    headers: Vec<(String, String)>,
    body: Value,
}

impl Req {
    fn count(&self, name: &str) -> usize {
        self.headers.iter().filter(|(k, _)| k == name).count()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn is_get(&self) -> bool {
        self.line.starts_with("GET ")
    }
}

type Seen = Arc<Mutex<Vec<Req>>>;

/// How the stub answers the notification `GET`.
#[derive(Clone, Copy)]
enum GetMode {
    /// The first dial gets one event carrying an id, then the stream closes;
    /// later dials are held open quietly — so the client has a reason, and an
    /// id, to reconnect with.
    ResumableOnce,
    /// `405 Method Not Allowed`: the server offers no push channel.
    NotAllowed,
}

fn read_req(r: &mut BufReader<TcpStream>) -> Option<Req> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            break;
        }
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut body).ok()?;
    }
    Some(Req {
        line: line.trim_end().to_string(),
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

fn respond(w: &mut TcpStream, extra: &str, body: &Value) {
    let b = serde_json::to_vec(body).unwrap();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    );
    let _ = w.write_all(head.as_bytes());
    let _ = w.write_all(&b);
    let _ = w.flush();
}

/// A Streamable-HTTP MCP server that records every request. Its tools: `echo`
/// (declares `additionalProperties: false` and REFUSES any argument it does not
/// declare, the way a strict server validates) and `silent` (accepts the call
/// and never answers).
fn spawn_server(seen: Seen, get: GetMode) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let gets = Arc::new(Mutex::new(0usize));
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let seen = Arc::clone(&seen);
            let gets = Arc::clone(&gets);
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(30))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some(req) = read_req(&mut r) else {
                    return;
                };
                seen.lock().unwrap().push(req.clone());
                if req.is_get() {
                    let n = {
                        let mut g = gets.lock().unwrap();
                        *g += 1;
                        *g
                    };
                    match get {
                        GetMode::NotAllowed => {
                            let _ = w.write_all(
                                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                        GetMode::ResumableOnce => {
                            let _ = w.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                            );
                            if n == 1 {
                                let note = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
                                let _ =
                                    w.write_all(format!("id: ev-7\ndata: {note}\n\n").as_bytes());
                                let _ = w.flush();
                                // Closing is a graceful end the client resumes
                                // from, with the id it last saw.
                                return;
                            }
                            let _ = w.flush();
                            std::thread::sleep(Duration::from_secs(10));
                        }
                    }
                    return;
                }
                let msg = &req.body;
                if msg.get("id").is_none() || msg.get("method").is_none() {
                    let _ = w.write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
                let id = msg["id"].clone();
                match msg["method"].as_str().unwrap_or("") {
                    "initialize" => respond(
                        &mut w,
                        "Mcp-Session-Id: s-1\r\n",
                        &json!({"jsonrpc": "2.0", "id": id, "result": {
                            "protocolVersion": msg["params"]["protocolVersion"],
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "wire", "version": "0"}
                        }}),
                    ),
                    "tools/list" => respond(
                        &mut w,
                        "",
                        &json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                            {"name": "echo", "inputSchema": {"type": "object",
                                "properties": {"s": {"type": "string"}},
                                "additionalProperties": false}},
                            {"name": "silent", "inputSchema": {"type": "object"}}
                        ]}}),
                    ),
                    "tools/call" => match msg["params"]["name"].as_str() {
                        Some("silent") => {
                            // Hold the exchange open and say nothing, until
                            // the client gives up and closes it.
                            let mut buf = [0u8; 64];
                            while let Ok(n) = r.read(&mut buf) {
                                if n == 0 {
                                    break;
                                }
                            }
                        }
                        _ => {
                            let args = msg["params"]["arguments"]
                                .as_object()
                                .cloned()
                                .unwrap_or_default();
                            let stray: Vec<&String> = args.keys().filter(|k| *k != "s").collect();
                            let result = if stray.is_empty() {
                                json!({"content": [{"type": "text", "text": "ok"}], "isError": false})
                            } else {
                                json!({"content": [{"type": "text", "text": format!("additionalProperties: {stray:?}")}], "isError": true})
                            };
                            respond(
                                &mut w,
                                "",
                                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                            );
                        }
                    },
                    _ => respond(
                        &mut w,
                        "",
                        &json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    ),
                }
            });
        }
    });
    format!("http://{addr}/mcp")
}

fn connect(ep: &str, headers: Vec<(String, String)>, timeout: Duration) -> McpClient {
    let mut c = McpClient::connect("wire", ep, headers, timeout).expect("connect");
    c.initialize().expect("initialize");
    c
}

fn calls(seen: &Seen) -> Vec<Req> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| r.body["method"] == "tools/call")
        .cloned()
        .collect()
}

/// Wait (bounded) until `pred` holds over what the server has seen.
fn wait_for(seen: &Seen, within: Duration, pred: impl Fn(&[Req]) -> bool) -> Vec<Req> {
    let until = Instant::now() + within;
    loop {
        let now = seen.lock().unwrap().clone();
        if pred(&now) || Instant::now() >= until {
            return now;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_connection_meta_rides_params_meta_merged_with_the_call_meta() {
    let seen: Seen = Arc::default();
    let ep = spawn_server(Arc::clone(&seen), GetMode::NotAllowed);
    let mut c = connect(&ep, vec![], Duration::from_secs(10));
    c.set_tool_meta(json!({
        "agent/run_id": "run-1",
        "agent/instance": "inst-1",
        "traceparent": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
    }));

    // The plain call carries the connection's meta…
    let r = c.call_tool("echo", Some(json!({"s": "hi"}))).expect("call");
    assert!(
        !r.is_error(),
        "a strict tool refused the call: {}",
        r.text()
    );
    // …and a call with its own meta carries both, the call's winning.
    let r = c
        .call_tool_with_meta(
            "echo",
            Some(json!({"s": "again"})),
            json!({"agent/claim_key": "k-1", "agent/instance": "override"}),
        )
        .expect("call with meta");
    assert!(
        !r.is_error(),
        "a strict tool refused the call: {}",
        r.text()
    );

    let calls = calls(&seen);
    assert_eq!(calls.len(), 2, "{calls:?}");
    for call in &calls {
        let params = &call.body["params"];
        let meta = &params["_meta"];
        assert_eq!(meta["agent/run_id"], "run-1", "{params}");
        assert!(
            meta["traceparent"]
                .as_str()
                .is_some_and(|t| t.starts_with("00-")),
            "{params}"
        );
        assert!(
            params["arguments"].get("_meta").is_none(),
            "the tool's arguments carry the tool's arguments only: {params}"
        );
    }
    assert_eq!(calls[0].body["params"]["_meta"]["agent/instance"], "inst-1");
    assert_eq!(calls[0].body["params"]["arguments"], json!({"s": "hi"}));
    assert_eq!(
        calls[1].body["params"]["_meta"]["agent/instance"],
        "override"
    );
    assert_eq!(calls[1].body["params"]["_meta"]["agent/claim_key"], "k-1");
    assert_eq!(calls[1].body["params"]["arguments"], json!({"s": "again"}));
}

#[test]
fn a_call_bound_ends_a_silent_tool_and_the_client_drops_promptly() {
    let seen: Seen = Arc::default();
    let ep = spawn_server(Arc::clone(&seen), GetMode::NotAllowed);
    // The connection's own bound is a minute: only the call's can end this.
    let c = connect(&ep, vec![], Duration::from_secs(60));

    let bound = Duration::from_millis(400);
    let started = Instant::now();
    let r = c.call_tool_with_meta_within("silent", Some(json!({})), json!({}), bound);
    let took = started.elapsed();
    match r {
        Err(mcp::client::McpError::Transport(m)) => {
            assert!(m.contains("timed out"), "{m}")
        }
        Err(other) => panic!("expected a timeout, got {other}"),
        Ok(r) => panic!("a silent tool answered: {}", r.text()),
    }
    assert!(
        took < bound + Duration::from_secs(2),
        "the call bound must govern, took {took:?}"
    );

    // The abandoned exchange stays open on the socket until the connection's
    // own timeout; it must not hold whoever drops the client that long.
    let started = Instant::now();
    drop(c);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "dropping the client waited on the abandoned call ({:?})",
        started.elapsed()
    );
}

#[test]
fn every_post_carries_each_operator_header_exactly_once() {
    let seen: Seen = Arc::default();
    let ep = spawn_server(Arc::clone(&seen), GetMode::NotAllowed);
    let c = connect(
        &ep,
        vec![
            ("Authorization".into(), "Bearer op-token".into()),
            ("X-Api-Key".into(), "k-1".into()),
        ],
        Duration::from_secs(10),
    );
    c.list_tools().expect("tools/list");
    c.call_tool("echo", Some(json!({"s": "x"}))).expect("call");

    let posts: Vec<Req> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.line.starts_with("POST "))
        .cloned()
        .collect();
    // initialize, notifications/initialized, tools/list, tools/call.
    assert!(posts.len() >= 4, "{posts:?}");
    for p in &posts {
        let what = p.body["method"].as_str().unwrap_or("?");
        assert_eq!(p.count("authorization"), 1, "{what}: {:?}", p.headers);
        assert_eq!(p.header("authorization"), Some("Bearer op-token"));
        assert_eq!(p.count("x-api-key"), 1, "{what}: {:?}", p.headers);
    }
}

#[test]
fn the_notification_stream_carries_its_headers_and_resumes_from_the_last_event() {
    let seen: Seen = Arc::default();
    let ep = spawn_server(Arc::clone(&seen), GetMode::ResumableOnce);
    let c = connect(
        &ep,
        vec![("Authorization".into(), "Bearer op-token".into())],
        Duration::from_secs(10),
    );
    let negotiated = c.protocol_version().expect("negotiated").to_string();

    // The first GET ends after one event; the SDK reconnects (after its
    // one-second backoff) from the id it saw.
    let all = wait_for(&seen, Duration::from_secs(10), |rs| {
        rs.iter().filter(|r| r.is_get()).count() >= 2
    });
    let gets: Vec<&Req> = all.iter().filter(|r| r.is_get()).collect();
    assert!(gets.len() >= 2, "the stream was not resumed: {all:?}");
    for g in &gets {
        assert_eq!(g.count("authorization"), 1, "{:?}", g.headers);
        assert_eq!(g.header("authorization"), Some("Bearer op-token"));
        assert_eq!(
            g.header("mcp-protocol-version"),
            Some(negotiated.as_str()),
            "{:?}",
            g.headers
        );
        assert_eq!(g.header("mcp-session-id"), Some("s-1"));
    }
    assert_eq!(
        gets[0].header("last-event-id"),
        None,
        "{:?}",
        gets[0].headers
    );
    assert_eq!(
        gets[1].header("last-event-id"),
        Some("ev-7"),
        "{:?}",
        gets[1].headers
    );
    drop(c);
}

#[test]
fn a_server_without_a_notification_stream_is_not_redialled() {
    // A 405 on the GET is the server saying it has no push channel; the SDK
    // takes it as such only when it is told, rather than seeing a stream that
    // ended and redialling it every second for the life of the connection.
    let seen: Seen = Arc::default();
    let ep = spawn_server(Arc::clone(&seen), GetMode::NotAllowed);
    let c = connect(&ep, vec![], Duration::from_secs(10));
    std::thread::sleep(Duration::from_secs(3));
    let gets = seen.lock().unwrap().iter().filter(|r| r.is_get()).count();
    assert_eq!(gets, 1, "the GET was redialled {gets} times");
    drop(c);
}
