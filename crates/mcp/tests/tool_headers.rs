// SPDX-License-Identifier: AGPL-3.0-only
//! Tool headers at MCP 2026-07-28.
//!
//! A tool whose schema annotates an argument with `x-mcp-header` must be
//! called with that argument in an `Mcp-Param-*` header, and rmcp sends one
//! only for a tool whose schema it listed on the same connection. A turn
//! worker's connection is handed its tools and never lists, so every
//! 2026-07-28 connection lists once when it starts. rmcp also drops, without
//! a word, a tool whose annotation it rejects: the connection says which.

mod common;

use common::*;
use mcp::client::{FirstRequest, McpClient};
use serde_json::{Value, json};
use std::time::Duration;

/// A tool whose `region` argument travels as the `Mcp-Param-Region` header.
fn regional() -> Value {
    json!({"name": "regional", "inputSchema": {"type": "object", "properties": {
        "region": {"type": "string", "x-mcp-header": "Region"}}}})
}

/// What a test server answers `tools/list` with.
#[derive(Clone)]
enum List {
    /// Its tools, as one JSON answer.
    Tools,
    /// This status and body instead.
    Answer(u16, Value),
    /// Its tools on an event stream, behind a log notification.
    Streamed,
}

/// A 2026-07-28 server listing `tools`, refusing a `regional` call whose
/// `Mcp-Param-Region` is missing or does not match the argument, the way
/// rmcp's own server does (`-32020`). `caps` and `list` override the
/// capabilities and the `tools/list` answer.
fn server(tools: Vec<Value>, caps: Value, list: List) -> (String, Seen) {
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
                        caps: caps.clone(),
                        ..StatelessOpts::default()
                    },
                ),
            ),
            "tools/list" => match &list {
                List::Answer(status, body) => {
                    let mut body = body.clone();
                    if body.get("id").is_some_and(|i| !i.is_null()) {
                        body["id"] = id.clone();
                    }
                    respond(w, *status, &[], &body)
                }
                List::Tools => respond(w, 200, &[], &result(&id, json!({"tools": tools}))),
                List::Streamed => respond_sse(
                    w,
                    &[],
                    &[
                        json!({"jsonrpc": "2.0", "method": "notifications/message",
                               "params": {"level": "info", "data": "listing"}}),
                        result(&id, json!({"tools": tools})),
                    ],
                ),
            },
            "tools/call" => {
                let args = &req.body["params"]["arguments"];
                if req.body["params"]["name"] == "regional"
                    && req.header("mcp-param-region") != args["region"].as_str()
                {
                    return respond(w, 400, &[], &error(&id, -32020, "Mcp-Param-Region"));
                }
                respond(
                    w,
                    200,
                    &[],
                    &result(
                        &id,
                        json!({
                    "content": [{"type": "text", "text": "ok"}], "isError": false}),
                    ),
                )
            }
            "subscriptions/listen" => respond(w, 400, &[], &error(&id, -32602, "no listens")),
            _ => respond(w, 404, &[], &error(&id, -32601, "Method not found")),
        }
    })
}

fn connect(ep: &str, first: FirstRequest) -> Result<McpClient, String> {
    let mut c = McpClient::connect("t", ep, vec![], Duration::from_secs(5))
        .unwrap()
        .start_with(first);
    c.initialize().map_err(|e| e.to_string())?;
    Ok(c)
}

#[test]
fn a_fresh_connection_calls_an_annotated_tool_with_its_header() {
    // As a turn worker does: connect and call, never list.
    let (ep, seen) = server(vec![regional()], json!({"tools": {}}), List::Tools);
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    c.call_tool("regional", Some(json!({"region": "eu"})))
        .expect("the call carries its header");
    let call = &requests(&seen, "tools/call")[0];
    assert_eq!(call.header("mcp-param-region"), Some("eu"));
    assert_eq!(
        methods(&seen),
        ["server/discover", "tools/list", "tools/call"]
    );
}

#[test]
fn a_list_streamed_behind_a_notification_still_gives_its_tools_their_headers() {
    // rmcp records a tool's `x-mcp-header` only from a list answer it takes
    // off the wire; one it drains from a stream that has just ended skips
    // that, and which of the two happens is a race inside rmcp. An answer the
    // server streams behind a notification is taken whole all the same, and
    // the notification reaches the host's queue. Several fresh connections,
    // so a lost race shows.
    for round in 0..20 {
        let (ep, seen) = server(vec![regional()], json!({"tools": {}}), List::Streamed);
        let c = connect(&ep, FirstRequest::Discover).unwrap();
        c.call_tool("regional", Some(json!({"region": "eu"})))
            .unwrap_or_else(|e| panic!("round {round}: the call carries its header: {e}"));
        assert_eq!(
            requests(&seen, "tools/call")[0].header("mcp-param-region"),
            Some("eu"),
            "round {round}"
        );
        let notes = c.drain_notifications();
        assert!(
            notes.iter().any(|n| n.method == "notifications/message"
                && n.params.as_ref().is_some_and(|p| p["data"] == "listing")),
            "round {round}: the notification ahead of the answer is queued: {notes:?}"
        );
    }
}

#[test]
fn a_tool_rmcp_drops_is_reported_once() {
    let nested = json!({"name": "nested", "inputSchema": {"type": "object", "properties": {
        "outer": {"type": "object", "properties": {
            "inner": {"type": "string", "x-mcp-header": "Inner"}}}}}});
    let ok = json!({"name": "ok", "inputSchema": {"type": "object"}});
    let (ep, _) = server(vec![nested, ok], json!({"tools": {}}), List::Tools);
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    let names: Vec<String> = c
        .list_tools()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["ok"]);
    assert_eq!(c.drain_dropped_tools(), ["nested"]);
    c.list_tools().unwrap();
    assert!(
        c.drain_dropped_tools().is_empty(),
        "named once per connection"
    );
}

#[test]
fn a_handshake_connection_is_not_primed() {
    let (ep, seen) = legacy_server(LegacyOpts::default());
    let _c = connect(&ep, FirstRequest::Initialize).unwrap();
    assert_eq!(count(&seen, "tools/list"), 0);
}

#[test]
fn a_failed_priming_list_fails_the_connect() {
    let failures = [
        (
            500,
            json!({"jsonrpc": "2.0", "id": null,
                   "error": {"code": -32603, "message": "catalogue down"}}),
            "catalogue down",
        ),
        (
            200,
            json!({"jsonrpc": "2.0", "id": 0, "result": {"nope": 1}}),
            "tools/list",
        ),
    ];
    for (status, body, says) in failures {
        // It would hold a standing listen too, had it connected.
        let (ep, seen) = server(
            vec![],
            json!({"tools": {"listChanged": true}}),
            List::Answer(status, body),
        );
        let err = connect(&ep, FirstRequest::Discover).err().expect("refused");
        // Long enough for a listen opened in the background to have arrived.
        std::thread::sleep(Duration::from_millis(500));
        assert!(err.contains("tools/list:"), "{err}");
        assert!(err.contains(says), "{err}");
        assert_eq!(count(&seen, "subscriptions/listen"), 0, "{err}");
    }
}

#[test]
fn every_header_rmcp_adds_is_one_agentd_refuses() {
    // The headers the MCP client sets on its own are the ones an operator may
    // not set in config (a copy would be sent beside the client's, and the
    // server refuses the duplicate). Every header that reaches a 2026-07-28
    // server from discover, an annotated call, a read, a prompt and a listen
    // is either the transport's own or one of those; a header a later rmcp
    // adds fails here until the refusal covers it.
    let (ep, seen) = server(
        vec![regional()],
        json!({"tools": {}, "prompts": {}, "resources": {"subscribe": true}}),
        List::Tools,
    );
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    c.call_tool("regional", Some(json!({"region": "eu"})))
        .unwrap();
    let _ = c.read_resource("demo://x");
    let _ = c.get_prompt("p", None);
    let _ = c.subscribe("demo://x");
    let transport = [
        "host",
        "content-type",
        "accept",
        "content-length",
        "connection",
        "authorization",
    ];
    let reqs = seen.lock().unwrap().clone();
    let methods: Vec<&str> = reqs.iter().map(Req::method).collect();
    for m in [
        "server/discover",
        "tools/call",
        "resources/read",
        "prompts/get",
        "subscriptions/listen",
    ] {
        assert!(methods.contains(&m), "{m} was not sent: {methods:?}");
    }
    // And at 2025-11-25, where the transport adds the session it echoes.
    let (legacy, legacy_seen) = legacy_server(LegacyOpts::default());
    let c = connect(&legacy, FirstRequest::Initialize).unwrap();
    c.call_tool("echo", Some(json!({}))).unwrap();
    let mut reqs = reqs;
    reqs.extend(legacy_seen.lock().unwrap().iter().cloned());
    for r in &reqs {
        for (name, _) in &r.headers {
            assert!(
                transport.contains(&name.as_str()) || mcp::wire::is_protocol_header(name),
                "{}: '{name}' is set by the client but not refused in config",
                r.method()
            );
        }
        assert!(
            r.method() == "initialize"
                || r.headers
                    .iter()
                    .any(|(n, _)| mcp::wire::is_protocol_header(n)),
            "{}: no protocol header at all",
            r.method()
        );
    }
}
