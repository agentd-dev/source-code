// SPDX-License-Identifier: AGPL-3.0-only
//! A request the SDK does not model, sent on the connection's own socket
//! (`McpClient::call_raw`): an extension's method, carrying what the SDK
//! puts on every request it sends.
//!
//! At 2026-07-28 the server refuses a request that leaves out a standard
//! header or a per-request `_meta` key, so the 2026-07-28 server here holds
//! every request to the rules the other tests' servers do
//! (`stateless_refusal`), and answers the extension method only once they
//! pass.

mod common;

use common::*;
use mcp::client::{FirstRequest, McpClient};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::time::{Duration, Instant};

const METHOD: &str = "x/echo";

/// A 2026-07-28 server whose extension method answers with what it was sent,
/// or with `answer` when one is given.
fn server(answer: Option<(u16, Value)>) -> (String, Seen) {
    spawn(move |req, w| {
        let id = req.id();
        let supported = [MODERN.to_string()];
        if let Some((status, body)) = stateless_refusal(req, &supported) {
            return respond(w, status, &[], &body);
        }
        match req.method() {
            "server/discover" => respond(
                w,
                200,
                &[],
                &discover_answer(
                    &id,
                    &StatelessOpts {
                        caps: json!({}),
                        ..StatelessOpts::default()
                    },
                ),
            ),
            METHOD => match &answer {
                Some((status, body)) => {
                    let mut body = body.clone();
                    body["id"] = id.clone();
                    respond(w, *status, &[], &body)
                }
                None => respond(
                    w,
                    200,
                    &[],
                    &result(
                        &id,
                        json!({"resultType": "complete", "got": req.body["params"]}),
                    ),
                ),
            },
            _ => respond(w, 404, &[], &error(&id, -32601, "Method not found")),
        }
    })
}

fn connect(ep: &str, first: FirstRequest) -> McpClient {
    let mut c = McpClient::connect("x", ep, vec![], Duration::from_secs(5))
        .unwrap()
        .start_with(first);
    c.initialize().expect("connect");
    c
}

fn params() -> Map<String, Value> {
    json!({"a": 1, "_meta": {"io.example/trace": "t-1"}})
        .as_object()
        .unwrap()
        .clone()
}

#[test]
fn a_raw_request_carries_what_the_revision_requires() {
    let (ep, seen) = server(None);
    let c = connect(&ep, FirstRequest::Discover);
    let got = c
        .call_raw(
            METHOD,
            params(),
            json!({"io.example/ext": {}}),
            Duration::from_secs(5),
        )
        .expect("the connection answers")
        .expect("the server answers");
    // The server took it — every rule `stateless_refusal` holds passed — and
    // it carried the method's own params and `_meta` beside the revision's.
    let sent = &got["got"];
    assert_eq!(sent["a"], 1, "{got}");
    let meta = &sent["_meta"];
    assert_eq!(meta["io.example/trace"], "t-1", "{got}");
    assert_eq!(meta["io.modelcontextprotocol/protocolVersion"], MODERN);
    assert!(
        meta["io.modelcontextprotocol/clientInfo"]["name"].is_string(),
        "{got}"
    );
    assert_eq!(
        meta["io.modelcontextprotocol/clientCapabilities"]["extensions"],
        json!({"io.example/ext": {}}),
        "{got}"
    );
    let req = &requests(&seen, METHOD)[0];
    assert_eq!(req.header("mcp-method"), Some(METHOD));
    assert_eq!(req.header("mcp-protocol-version"), Some(MODERN));
    assert_eq!(req.count("mcp-name"), 0, "the revision does not define it");
    assert!(
        req.id().as_i64().is_some_and(|i| i >= 1 << 40),
        "an id the SDK's counter never reaches: {}",
        req.id()
    );
    // Two raw requests, two ids.
    c.call_raw(METHOD, Map::new(), Value::Null, Duration::from_secs(5))
        .unwrap()
        .unwrap();
    let ids: Vec<Value> = requests(&seen, METHOD).iter().map(|r| r.id()).collect();
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn a_refusal_comes_back_with_its_code_and_data() {
    let refusal = json!({"jsonrpc": "2.0", "id": 0, "error": {
        "code": -32023, "message": "Not found", "data": {"kind": "event"}}});
    let (ep, _) = server(Some((400, refusal)));
    let c = connect(&ep, FirstRequest::Discover);
    let e = c
        .call_raw(METHOD, Map::new(), Value::Null, Duration::from_secs(5))
        .expect("the connection answers")
        .expect_err("the server refused");
    assert_eq!(e.code, -32023);
    assert_eq!(e.data, Some(json!({"kind": "event"})));
}

#[test]
fn a_result_it_cannot_follow_is_a_protocol_error() {
    let interim = json!({"jsonrpc": "2.0", "id": 0, "result": {
        "resultType": "input_required", "inputRequests": {}}});
    let (ep, _) = server(Some((200, interim)));
    let c = connect(&ep, FirstRequest::Discover);
    let e = c
        .call_raw(METHOD, Map::new(), Value::Null, Duration::from_secs(5))
        .expect_err("not a result this call can follow")
        .to_string();
    assert!(e.contains("input_required"), "{e}");
}

#[test]
fn at_2025_11_25_it_rides_the_session_and_adds_no_meta() {
    // The handshake agreed the revision and the capabilities once; the
    // request carries the session and the revision header, and nothing a
    // 2026-07-28 request needs.
    let (ep, seen) = spawn(|req, w| {
        let id = req.id();
        match req.method() {
            "initialize" => respond(
                w,
                200,
                &[("Mcp-Session-Id", "s-1")],
                &initialize_answer(&id, "2025-11-25", &json!({})),
            ),
            m if m.starts_with("notifications/") => respond_raw(w, 202, &[], ""),
            METHOD => respond(
                w,
                200,
                &[],
                &result(&id, json!({"got": req.body["params"]})),
            ),
            _ if req.is_get() => respond_raw(w, 405, &[], ""),
            _ => respond(w, 404, &[], &error(&id, -32601, "Method not found")),
        }
    });
    let c = connect(&ep, FirstRequest::Initialize);
    let got = c
        .call_raw(
            METHOD,
            params(),
            json!({"io.example/ext": {}}),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
    assert_eq!(got["got"], json!(params()), "untouched: {got}");
    let req = &requests(&seen, METHOD)[0];
    assert_eq!(req.header("mcp-session-id"), Some("s-1"));
    assert_eq!(req.header("mcp-protocol-version"), Some("2025-11-25"));
    assert_eq!(req.count("mcp-method"), 0);
}

#[test]
fn an_answer_that_keeps_streaming_is_hung_up_at_the_bound() {
    // Keepalives reset a per-read timeout for ever; the bound is the whole
    // exchange's. What the server said meanwhile reaches the host's queue.
    let (ep, _) = spawn(|req, w| {
        let id = req.id();
        let supported = [MODERN.to_string()];
        if let Some((status, body)) = stateless_refusal(req, &supported) {
            return respond(w, status, &[], &body);
        }
        match req.method() {
            "server/discover" => respond(
                w,
                200,
                &[],
                &discover_answer(
                    &id,
                    &StatelessOpts {
                        caps: json!({}),
                        ..StatelessOpts::default()
                    },
                ),
            ),
            METHOD => {
                let _ = w.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                for _ in 0..100 {
                    let note = json!({"jsonrpc": "2.0", "method": "notifications/message",
                                      "params": {"level": "info", "data": "still here"}});
                    if w.write_all(format!("data: {note}\n\n").as_bytes()).is_err() {
                        return;
                    }
                    let _ = w.flush();
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            _ => respond(w, 404, &[], &error(&id, -32601, "Method not found")),
        }
    });
    let c = connect(&ep, FirstRequest::Discover);
    let started = Instant::now();
    let e = c
        .call_raw(METHOD, Map::new(), Value::Null, Duration::from_secs(1))
        .expect_err("hung up at the bound");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "took {:?}: {e}",
        started.elapsed()
    );
    assert!(
        c.drain_notifications()
            .iter()
            .any(|n| n.method == "notifications/message"),
        "the stream's notifications reach the host"
    );
}

/// A 2026-07-28 server whose `x/stream` answers with an SSE body: each of
/// `frames` (`"{SUB}"` in one replaced by the request's id), a pause of
/// `hold_ms`, then `last` when one is given.
fn streaming(frames: Vec<Value>, hold_ms: u64, last: Option<Value>) -> (String, Seen) {
    spawn(move |req, w| {
        let id = req.id();
        let supported = [MODERN.to_string()];
        if let Some((status, body)) = stateless_refusal(req, &supported) {
            return respond(w, status, &[], &body);
        }
        match req.method() {
            "server/discover" => respond(
                w,
                200,
                &[],
                &discover_answer(
                    &id,
                    &StatelessOpts {
                        caps: json!({}),
                        ..StatelessOpts::default()
                    },
                ),
            ),
            "x/stream" => {
                let _ = w.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                let ours = |v: &Value| -> Value {
                    serde_json::from_str(&v.to_string().replace("\"{SUB}\"", &id.to_string()))
                        .unwrap()
                };
                for f in &frames {
                    // A comment between frames, as SSE allows: not a frame.
                    let _ = w.write_all(format!(": keep\n\ndata: {}\n\n", ours(f)).as_bytes());
                    let _ = w.flush();
                }
                std::thread::sleep(Duration::from_millis(hold_ms));
                if let Some(l) = &last {
                    let mut l = ours(l);
                    l["id"] = id.clone();
                    let _ = w.write_all(format!("data: {l}\n\n").as_bytes());
                    let _ = w.flush();
                }
            }
            _ => respond(w, 404, &[], &error(&id, -32601, "Method not found")),
        }
    })
}

fn note(method: &str, sub: Value, params: Value) -> Value {
    let mut p = params;
    p["_meta"] = json!({"io.modelcontextprotocol/subscriptionId": sub});
    json!({"jsonrpc": "2.0", "method": method, "params": p})
}

#[test]
fn a_stream_hands_on_its_own_frames_in_order_and_ends_with_the_answer() {
    let frames = vec![
        note("n/active", json!("{SUB}"), json!({"cursor": "c0"})),
        // Another request's subscription: not this stream's.
        note("n/event", json!(7), json!({"cursor": "x"})),
        note("n/event", json!("{SUB}"), json!({"cursor": "c1"})),
        note("n/heartbeat", json!("{SUB}"), json!({"cursor": "c1"})),
    ];
    let last = json!({"jsonrpc": "2.0", "result": {"resultType": "complete"}});
    let (ep, seen) = streaming(frames, 0, Some(last));
    let c = connect(&ep, FirstRequest::Discover);
    let handle = mcp::http::StreamHandle::new(Duration::from_secs(5));
    let mut got = Vec::new();
    let end = c.stream_raw(
        "x/stream",
        params(),
        json!({"io.example/ext": {}}),
        Duration::from_secs(5),
        &handle,
        |f| {
            got.push((
                f["method"].as_str().unwrap_or("").to_string(),
                f["params"]["cursor"].clone(),
            ));
            std::ops::ControlFlow::Continue(())
        },
    );
    assert!(
        matches!(&end, mcp::rmcp_client::StreamEnd::Answered(r) if r["resultType"] == "complete"),
        "{end:?}"
    );
    assert_eq!(
        got,
        [
            ("n/active".to_string(), json!("c0")),
            ("n/event".to_string(), json!("c1")),
            ("n/heartbeat".to_string(), json!("c1")),
        ]
    );
    // Stamped as any raw request is.
    let req = &requests(&seen, "x/stream")[0];
    assert_eq!(req.header("mcp-method"), Some("x/stream"));
    assert_eq!(req.header("mcp-protocol-version"), Some(MODERN));
    assert_eq!(
        req.meta("clientCapabilities").unwrap()["extensions"],
        json!({"io.example/ext": {}})
    );
}

#[test]
fn a_stream_silent_past_its_idle_bound_ends_silent() {
    let frames = vec![note("n/active", json!("{SUB}"), json!({}))];
    let (ep, _) = streaming(frames, 10_000, None);
    let c = connect(&ep, FirstRequest::Discover);
    let handle = mcp::http::StreamHandle::new(Duration::from_secs(30));
    let started = Instant::now();
    let end = c.stream_raw(
        "x/stream",
        Map::new(),
        Value::Null,
        Duration::from_secs(5),
        &handle,
        |_| {
            // The server's own pace says how quiet it may go.
            handle.set_idle(Duration::from_millis(300));
            std::ops::ControlFlow::Continue(())
        },
    );
    assert!(
        matches!(end, mcp::rmcp_client::StreamEnd::Silent),
        "{end:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_stream_ends_when_its_reader_breaks_or_its_handle_is_closed() {
    let frames = vec![
        note("n/active", json!("{SUB}"), json!({})),
        note("n/terminated", json!("{SUB}"), json!({})),
        note("n/event", json!("{SUB}"), json!({})),
    ];
    let (ep, _) = streaming(frames.clone(), 10_000, None);
    let c = connect(&ep, FirstRequest::Discover);
    let handle = mcp::http::StreamHandle::new(Duration::from_secs(30));
    let mut seen = 0;
    let end = c.stream_raw(
        "x/stream",
        Map::new(),
        Value::Null,
        Duration::from_secs(5),
        &handle,
        |f| {
            seen += 1;
            if f["method"] == "n/terminated" {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        },
    );
    assert!(
        matches!(end, mcp::rmcp_client::StreamEnd::Stopped),
        "{end:?}"
    );
    assert_eq!(seen, 2, "nothing after the break");

    // Closed from another thread, mid-stream.
    let (ep, _) = streaming(frames[..1].to_vec(), 10_000, None);
    let c = connect(&ep, FirstRequest::Discover);
    let handle = std::sync::Arc::new(mcp::http::StreamHandle::new(Duration::from_secs(30)));
    let h = std::sync::Arc::clone(&handle);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        h.close();
    });
    let started = Instant::now();
    let end = c.stream_raw(
        "x/stream",
        Map::new(),
        Value::Null,
        Duration::from_secs(5),
        &handle,
        |_| std::ops::ControlFlow::Continue(()),
    );
    assert!(
        matches!(end, mcp::rmcp_client::StreamEnd::Stopped),
        "{end:?}"
    );
    assert!(handle.closed());
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_stream_refused_at_open_comes_back_with_its_code() {
    let refusal = json!({"jsonrpc": "2.0", "id": 0, "error": {
        "code": -32014, "message": "Unsupported",
        "data": {"feature": "deliveryMode", "value": "push"}}});
    let (ep, _) = server(Some((200, refusal)));
    let c = connect(&ep, FirstRequest::Discover);
    let handle = mcp::http::StreamHandle::new(Duration::from_secs(30));
    let end = c.stream_raw(
        METHOD,
        Map::new(),
        Value::Null,
        Duration::from_secs(5),
        &handle,
        |_| std::ops::ControlFlow::Continue(()),
    );
    match end {
        mcp::rmcp_client::StreamEnd::Refused(e) => {
            assert_eq!(e.code, -32014);
            assert_eq!(e.data.unwrap()["value"], "push");
        }
        other => panic!("{other:?}"),
    }
}
