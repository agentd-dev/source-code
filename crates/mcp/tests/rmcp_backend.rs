// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP client against a real server, at both revisions agentd reaches: a
//! 2025-11-25 server through the `initialize` handshake, and a 2026-07-28
//! server through `server/discover` alone, which holds every request to that
//! revision's header and `_meta` rules (`common::stateless_refusal`).
//!
//! These tests assert on the things that would silently break if the facade
//! were wired wrong: that the revision offered in `initialize` is the one agentd
//! pins (rmcp's `LATEST_WITH_INITIALIZE`, 2025-11-25), not the SDK's default
//! (its `LATEST`, 2026-07-28, which has no `initialize`); that declared
//! capabilities reach the handshake; that the server's capabilities are read
//! from whichever answer it gave, and gate what the client sends; that a
//! subscription uses the method the revision defines; and that tools and
//! resources come back in agentd's own wire types.
//!
//! It also pins the one SDK default agentd turns off: the SEP-2549 client
//! response cache. A read here is a request to the server, every time, and a
//! server that is gone or says no is an error — never the last answer replayed.

mod common;

use std::collections::HashMap;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{
    MODERN, Req, Seen, StatelessOpts, discover_answer, error, initialize_answer,
    initialize_refusal, methods, read_req, requests, respond, respond_raw, result,
    stateless_refusal,
};
use mcp::client::{FirstRequest, McpClient, McpError};
use mcp::inbound::{Answer, Handler, Inbound};
use mcp::rmcp_client::{RmcpBuilder, RmcpClient};
use rmcp::model::ProtocolVersion;
use serde_json::{Value, json};

/// How the test server starts a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// The 2025-11-25 handshake: `initialize`, answered at this revision,
    /// and a session.
    Handshake(&'static str),
    /// 2026-07-28: `server/discover`, no session, everything per request.
    Stateless,
}

const HANDSHAKE: Lifecycle = Lifecycle::Handshake("2025-11-25");
const LIFECYCLES: [Lifecycle; 2] = [HANDSHAKE, Lifecycle::Stateless];

impl Lifecycle {
    /// What a client reaching this server starts with.
    fn first(self) -> FirstRequest {
        match self {
            Lifecycle::Handshake(_) => FirstRequest::Initialize,
            Lifecycle::Stateless => FirstRequest::Discover,
        }
    }

    /// The revision the connection speaks.
    fn revision(self) -> &'static str {
        match self {
            Lifecycle::Handshake(v) => v,
            Lifecycle::Stateless => MODERN,
        }
    }
}

/// Answer what only the lifecycle decides: the handshake or discover, a
/// request refused for breaking the revision's rules, and the subscription
/// methods. `None` leaves `req` to the caller.
fn lifecycle_answer(lifecycle: Lifecycle, req: &Req, caps: &Value) -> Option<(u16, Value)> {
    let id = req.id();
    let unknown = || Some((200, error(&id, -32601, "Method not found")));
    match lifecycle {
        Lifecycle::Handshake(revision) => match req.method() {
            "initialize" => Some((200, initialize_answer(&id, revision, caps))),
            "server/discover" | "subscriptions/listen" => unknown(),
            _ => None,
        },
        Lifecycle::Stateless => {
            let supported = [MODERN.to_string()];
            if req.method() == "initialize" {
                let asked = req.body["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or_default();
                return Some((400, initialize_refusal(&id, asked, &supported)));
            }
            if let Some(refusal) = stateless_refusal(req, &supported) {
                return Some(refusal);
            }
            match req.method() {
                "server/discover" => {
                    let opts = StatelessOpts {
                        caps: caps.clone(),
                        ..StatelessOpts::default()
                    };
                    Some((200, discover_answer(&id, &opts)))
                }
                // The revision removed them.
                "resources/subscribe" | "resources/unsubscribe" => {
                    Some((404, error(&id, -32601, "Method not found")))
                }
                _ => None,
            }
        }
    }
}

/// The session a 2025-11-25 `initialize` answer issues; nothing else does.
fn issued(lifecycle: Lifecycle, req: &Req) -> Vec<(&'static str, &'static str)> {
    match lifecycle {
        Lifecycle::Handshake(_) if req.method() == "initialize" => {
            vec![("Mcp-Session-Id", "s-1")]
        }
        _ => vec![],
    }
}

/// What the server offers unless a test says otherwise.
fn default_caps() -> Value {
    json!({"tools": {}, "resources": {"subscribe": true}})
}

/// A minimal Streamable-HTTP MCP server speaking `lifecycle`: answers tools,
/// resources, prompts and completion, and the subscription method its
/// revision defines — at 2026-07-28 a listen is acknowledged, then closed.
/// Records every request.
fn spawn_server(lifecycle: Lifecycle) -> (String, Seen) {
    spawn_server_with(lifecycle, default_caps())
}

/// [`spawn_server`], advertising `caps`.
fn spawn_server_with(lifecycle: Lifecycle, caps: Value) -> (String, Seen) {
    common::spawn(move |req, w| {
        if !req.line.starts_with("POST") {
            return respond_raw(w, 405, &[], "");
        }
        if req.body.get("id").is_none() {
            return respond_raw(w, 202, &[], "");
        }
        let id = req.id();
        if let Some((status, body)) = lifecycle_answer(lifecycle, req, &caps) {
            return respond(w, status, &issued(lifecycle, req), &body);
        }
        let answer = match req.method() {
            "subscriptions/listen" => return acknowledge(req, w),
            "tools/list" => json!({"tools": [{
                "name": "echo",
                "description": "echo a string",
                "inputSchema": {"type": "object", "properties": {"s": {"type": "string"}}}
            }]}),
            "resources/list" => json!({"resources": [{"uri": "file:///a.txt", "name": "a"}]}),
            "tools/call" => {
                json!({"content": [{"type": "text", "text": "echoed"}], "isError": false})
            }
            "prompts/get" => json!({"messages": [
                {"role": "user", "content": {"type": "text", "text": "hi"}}]}),
            "completion/complete" => json!({"completion": {"values": ["x1"]}}),
            "resources/subscribe" => json!({}),
            _ => return respond(w, 200, &[], &error(&id, -32601, "Method not found")),
        };
        respond(w, 200, &[], &result(&id, answer))
    })
}

/// Acknowledge a listen's URIs (this server offers no list-changed flag),
/// then end the stream.
fn acknowledge(req: &Req, w: &mut TcpStream) {
    use std::io::Write;
    let id = req.id();
    let uris = req.body["params"]["notifications"]["resourceSubscriptions"].clone();
    let ack = json!({
        "jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
        "params": {"_meta": {"io.modelcontextprotocol/subscriptionId": id},
                   "notifications": {"resourceSubscriptions": uris}}
    });
    let _ = write!(
        w,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {ack}\n\n"
    );
    let _ = w.flush();
}

/// The SDK backend, connected the way `lifecycle` starts.
fn connect(ep: &str, lifecycle: Lifecycle) -> RmcpClient {
    RmcpBuilder::new("mock", ep, vec![], Duration::from_secs(5))
        .start_with(lifecycle.first())
        .connect()
        .unwrap_or_else(|e| panic!("{lifecycle:?}: {e}"))
}

struct Yes;
impl Handler for Yes {
    fn handle(&self, _req: Inbound) -> Option<Answer> {
        Some(Answer::Accept(json!({"env": "staging"})))
    }
}

#[test]
fn the_handshake_offers_the_newest_revision_with_an_initialize() {
    // rmcp's `LATEST` (and its default) is 2026-07-28, which has no
    // `initialize`; a handshake that offered it would ask for a revision the
    // handshake cannot be. agentd offers the newest one that still has one.
    let expected = "2025-11-25";
    let (ep, seen) = spawn_server(HANDSHAKE);
    let client = connect(&ep, HANDSHAKE);
    let init = &requests(&seen, "initialize")[0].body["params"];
    assert_eq!(init["protocolVersion"], expected, "handshake: {init}");
    assert_eq!(client.protocol_version(), Some(expected));
    // …and agentd's mock server answers the same revision, pinned to rmcp's
    // constant, so the two cannot drift apart unnoticed.
    assert_eq!(
        mcp::wire::PROTOCOL_VERSION,
        ProtocolVersion::LATEST_WITH_INITIALIZE.as_str()
    );
}

#[test]
fn subscribing_uses_the_method_the_negotiated_revision_defines() {
    // The revisions disagree: up to 2025-11-25 a client calls
    // `resources/subscribe`; from 2026-07-28 on `subscriptions/listen` replaces
    // it. The connection must call the one its revision defines — a `listen`
    // against an older server is an unknown method, and a `subscribe` against
    // a newer one is a method that revision removed. A server that answers
    // the handshake with an older revision than offered is held to that one.
    let cases = [
        (HANDSHAKE, "resources/subscribe", "subscriptions/listen"),
        (
            Lifecycle::Handshake(ProtocolVersion::V_2025_06_18.as_str()),
            "resources/subscribe",
            "subscriptions/listen",
        ),
        (
            Lifecycle::Stateless,
            "subscriptions/listen",
            "resources/subscribe",
        ),
    ];
    for (lifecycle, calls, never) in cases {
        let (ep, seen) = spawn_server(lifecycle);
        let client = connect(&ep, lifecycle);
        assert_eq!(
            client.protocol_version(),
            Some(lifecycle.revision()),
            "{lifecycle:?}"
        );
        client
            .subscribe("file:///a.txt")
            .unwrap_or_else(|e| panic!("{lifecycle:?}: {e}"));
        let called = methods(&seen);
        assert!(
            called.iter().any(|m| m == calls),
            "{lifecycle:?} should call {calls}, saw: {called:?}"
        );
        assert!(
            !called.iter().any(|m| m == never),
            "{lifecycle:?} does not define {never}: {called:?}"
        );
        assert_eq!(client.subscribed(), ["file:///a.txt"], "{lifecycle:?}");
    }
}

#[test]
fn capabilities_come_from_the_discover_answer() {
    // What a 2026-07-28 server offers is in its discover answer, and that is
    // what gates what the client sends: an operation the server offered
    // reaches it, and one it did not is refused here, by name, with nothing
    // on the wire.
    let connect = |ep: &str| {
        let mut c = McpClient::connect("mock", ep, vec![], Duration::from_secs(5))
            .unwrap()
            .start_with(FirstRequest::Discover);
        c.initialize().expect("connect");
        c
    };
    let prompt = || ("p", None);
    let reference = || json!({"type": "ref/prompt", "name": "p"});
    let argument = || json!({"name": "a", "value": "x"});

    let offered = json!({"tools": {}, "prompts": {}, "completions": {},
                         "resources": {"subscribe": true}});
    let (ep, seen) = spawn_server_with(Lifecycle::Stateless, offered);
    let c = connect(&ep);
    let (name, args) = prompt();
    let got = c.get_prompt(name, args).expect("prompts/get");
    assert_eq!(got.messages.len(), 1);
    let done = c
        .complete(reference(), argument())
        .expect("completion/complete");
    assert_eq!(done.completion.values, ["x1"]);
    c.subscribe("file:///a.txt").expect("listen");
    assert_eq!(
        methods(&seen)[..4],
        [
            "server/discover",
            "prompts/get",
            "completion/complete",
            "subscriptions/listen"
        ]
    );

    let withheld = json!({"tools": {}, "resources": {"subscribe": false}});
    let (ep, seen) = spawn_server_with(Lifecycle::Stateless, withheld);
    let c = connect(&ep);
    let (name, args) = prompt();
    match c.get_prompt(name, args) {
        Err(McpError::Capability(m)) => assert!(m.contains("prompts"), "{m}"),
        other => panic!("prompts were not offered: {other:?}"),
    }
    match c.complete(reference(), argument()) {
        Err(McpError::Capability(m)) => assert!(m.contains("completions"), "{m}"),
        other => panic!("completions were not offered: {other:?}"),
    }
    match c.subscribe("file:///a.txt") {
        Err(McpError::Capability(m)) => assert!(m.contains("resources.subscribe"), "{m}"),
        other => panic!("subscriptions were not offered: {other:?}"),
    }
    assert_eq!(methods(&seen), ["server/discover"], "nothing reached it");
}

#[test]
fn declared_capabilities_reach_the_handshake() {
    let (ep, seen) = spawn_server(HANDSHAKE);
    let h: Arc<dyn Handler> = Arc::new(Yes);
    let _client = RmcpBuilder::new("mock", &ep, vec![], Duration::from_secs(5))
        .with_elicitation(h)
        .connect()
        .expect("connect");
    let init = &requests(&seen, "initialize")[0].body["params"];
    assert_eq!(
        init["capabilities"]["elicitation"],
        json!({}),
        "elicitation should be declared: {init}"
    );

    // …and absent when the host cannot answer one.
    let (ep2, seen2) = spawn_server(HANDSHAKE);
    let _plain = connect(&ep2, HANDSHAKE);
    let init2 = &requests(&seen2, "initialize")[0].body["params"];
    assert!(
        init2["capabilities"].get("elicitation").is_none(),
        "undeclared: {init2}"
    );
}

#[test]
fn tools_and_resources_come_back_as_agentds_own_wire_types() {
    // The facade converts through JSON; this is the check that the two shapes
    // really do line up rather than silently dropping fields.
    for lifecycle in LIFECYCLES {
        let (ep, _) = spawn_server(lifecycle);
        let client = connect(&ep, lifecycle);

        let tools = client.list_tools().expect("tools/list");
        assert_eq!(tools.len(), 1, "{lifecycle:?}");
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description.as_deref(), Some("echo a string"));
        assert_eq!(tools[0].input_schema["properties"]["s"]["type"], "string");

        let resources = client.list_resources().expect("resources/list");
        assert_eq!(resources.len(), 1, "{lifecycle:?}");
        assert_eq!(resources[0].uri, "file:///a.txt");

        // The negotiated server capabilities survive the translation.
        assert!(client.capabilities().tools.is_some(), "{lifecycle:?}");
        assert!(client.capabilities().resources.is_some(), "{lifecycle:?}");
    }
}

#[test]
fn a_tool_call_round_trips() {
    for lifecycle in LIFECYCLES {
        let (ep, seen) = spawn_server(lifecycle);
        let client = connect(&ep, lifecycle);
        let meta = json!({"agent/run_id": "r1"}).as_object().cloned();
        let out = client
            .call_tool("echo", Some(json!({"s": "hi"})), meta, None)
            .unwrap_or_else(|e| panic!("{lifecycle:?}: {e}"));
        assert_eq!(out["content"][0]["text"], "echoed", "{lifecycle:?}");
        let call = &requests(&seen, "tools/call")[0].body["params"];
        assert_eq!(call["arguments"], json!({"s": "hi"}), "{lifecycle:?}");
        assert_eq!(call["_meta"]["agent/run_id"], "r1", "{lifecycle:?}");
    }
}

#[test]
fn the_run_meta_rides_beside_the_protocol_meta_at_2026_07_28() {
    // At 2026-07-28 `params._meta` also carries the protocol itself — the
    // revision, the client, its capabilities — on every request. agentd's own
    // call meta (the run id a server dedupes a retried call by) must ride
    // beside it, neither replaced by it nor replacing it.
    let (ep, seen) = spawn_server(Lifecycle::Stateless);
    let client = connect(&ep, Lifecycle::Stateless);
    let meta = json!({"agent/run_id": "r-1"}).as_object().cloned();
    client
        .call_tool("echo", None, meta, None)
        .expect("tools/call");
    let call = &requests(&seen, "tools/call")[0];
    let m = &call.body["params"]["_meta"];
    assert_eq!(m["agent/run_id"], "r-1", "{m}");
    assert_eq!(m["io.modelcontextprotocol/protocolVersion"], MODERN, "{m}");
    assert!(call.meta("clientInfo").is_some(), "{m}");
}

/// A server that offers a long `ttlMs` on every cacheable result, the way the
/// registry does, and counts what actually reached it.
#[derive(Default)]
struct TtlState {
    /// Calls per JSON-RPC method, counted on the server side — the arbiter of
    /// whether a read happened, whatever the client reports.
    calls: Mutex<HashMap<String, usize>>,
    /// Answer `resources/read` with "not found" (a withdrawn instruction).
    fail_reads: AtomicBool,
}

impl TtlState {
    fn calls(&self, method: &str) -> usize {
        self.calls.lock().unwrap().get(method).copied().unwrap_or(0)
    }
}

/// Stops a [`spawn_ttl_server`]: afterwards nothing listens on its port, so a
/// request is refused rather than answered.
struct Stopper {
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
}

impl Stopper {
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // `accept` blocks; one dial wakes it so the loop sees the flag, breaks,
        // and drops the listener.
        let _ = TcpStream::connect(self.addr);
        // The loop is gone once the port refuses.
        for _ in 0..100 {
            if TcpStream::connect(self.addr).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the server on {} did not stop", self.addr);
    }
}

/// An hour, as the server's own offer of reuse — far past any test.
const TTL: u64 = 3_600_000;

/// The TTL server speaking `lifecycle`. At 2026-07-28 the discover answer
/// offers the same hour as every list and read.
fn spawn_ttl_server(state: Arc<TtlState>, lifecycle: Lifecycle) -> (String, Stopper) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let caps = json!({"tools": {}, "resources": {"subscribe": true}, "prompts": {}});
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            if flag.load(Ordering::SeqCst) {
                break;
            }
            let Ok(conn) = conn else { continue };
            let (state, caps) = (Arc::clone(&state), caps.clone());
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some(req) = read_req(&mut r) else {
                    return;
                };
                if !req.line.starts_with("POST") {
                    return respond_raw(&mut w, 405, &[], "");
                }
                if req.body.get("id").is_none() {
                    return respond_raw(&mut w, 202, &[], "");
                }
                let id = req.id();
                let method = req.method();
                let n = {
                    let mut calls = state.calls.lock().unwrap();
                    let n = calls.entry(method.to_string()).or_default();
                    *n += 1;
                    *n
                };
                if let Some((status, mut body)) = lifecycle_answer(lifecycle, &req, &caps) {
                    if method == "server/discover" && status == 200 {
                        body["result"]["ttlMs"] = json!(TTL);
                    }
                    return respond(&mut w, status, &issued(lifecycle, &req), &body);
                }
                let answer = match method {
                    "resources/read" if state.fail_reads.load(Ordering::SeqCst) => {
                        return respond(
                            &mut w,
                            200,
                            &[],
                            &error(&id, -32002, "Resource not found"),
                        );
                    }
                    // The text changes with every read, so a client that shows
                    // the previous text did not ask.
                    "resources/read" => json!({
                        "contents": [{"uri": req.body["params"]["uri"], "mimeType": "text/plain", "text": format!("v{n}")}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    "tools/list" => json!({
                        "tools": [{"name": "echo", "inputSchema": {"type": "object"}}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    "resources/list" => json!({
                        "resources": [{"uri": "file:///a.txt", "name": "a"}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    "prompts/list" => json!({
                        "prompts": [{"name": "p"}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    "resources/templates/list" => json!({
                        "resourceTemplates": [{"uriTemplate": "file:///{p}", "name": "t"}],
                        "ttlMs": TTL, "cacheScope": "private"
                    }),
                    _ => json!({}),
                };
                respond(&mut w, 200, &[], &result(&id, answer));
            });
        }
    });
    (format!("http://{addr}/mcp"), Stopper { stop, addr })
}

#[test]
fn every_read_reaches_the_server_even_when_it_offers_a_ttl() {
    // The server offers an hour of reuse on everything. rmcp's default would
    // take it: every call after the first answered from memory, no request on
    // the wire. agentd reads because it needs the current answer, so each call
    // must be one request — counted where it lands, on the server.
    for lifecycle in LIFECYCLES {
        let state: Arc<TtlState> = Arc::default();
        let (ep, _stop) = spawn_ttl_server(Arc::clone(&state), lifecycle);
        let client = connect(&ep, lifecycle);

        for i in 1..=3 {
            let read = client
                .read_resource("instruction://ins_x@stable")
                .unwrap_or_else(|e| panic!("{lifecycle:?}: {e}"));
            assert_eq!(
                read.text(),
                format!("v{i}"),
                "{lifecycle:?}: read {i} was not the server's current answer"
            );
        }
        for _ in 0..2 {
            client.list_tools().expect("tools/list");
            client.list_resources().expect("resources/list");
            client.list_prompts().expect("prompts/list");
            client
                .list_resource_templates()
                .expect("resources/templates/list");
        }
        for (method, n) in [
            ("resources/read", 3),
            ("tools/list", 2),
            ("resources/list", 2),
            ("prompts/list", 2),
            ("resources/templates/list", 2),
        ] {
            assert_eq!(state.calls(method), n, "{lifecycle:?}: {method}");
        }
    }
}

#[test]
fn a_read_against_a_stopped_server_fails_instead_of_replaying_the_last_answer() {
    // The §7.7 freshness watch rests on this: a registry that is gone must be
    // a failed read, or `unavailable` never fires. rmcp's default serves the
    // cached answer while it is fresh and the expired one when a re-fetch
    // fails — either way a dead server "answers".
    for lifecycle in LIFECYCLES {
        let state: Arc<TtlState> = Arc::default();
        let (ep, stopper) = spawn_ttl_server(Arc::clone(&state), lifecycle);
        let client = connect(&ep, lifecycle);
        let uri = "instruction://ins_x@stable";
        assert_eq!(client.read_resource(uri).expect("read").text(), "v1");
        client.list_tools().expect("tools/list");

        stopper.stop();

        match client.read_resource(uri) {
            Err(McpError::Transport(m)) => {
                assert!(
                    m.contains(&format!("resources/read {uri}")),
                    "{lifecycle:?}: {m}"
                )
            }
            other => panic!("{lifecycle:?}: a stopped server answered resources/read: {other:?}"),
        }
        match client.list_tools() {
            Err(McpError::Transport(m)) => assert!(m.contains("tools/list"), "{lifecycle:?}: {m}"),
            other => panic!("{lifecycle:?}: a stopped server answered tools/list: {other:?}"),
        }
    }
}

#[test]
fn a_refusal_from_the_server_is_not_masked_by_an_earlier_answer() {
    // A withdrawn instruction: the registry is up and says "not found". That
    // is the strongest signal there is, and a cache that answers with the
    // last good copy turns it into a confirmation.
    for lifecycle in LIFECYCLES {
        let state: Arc<TtlState> = Arc::default();
        let (ep, _stop) = spawn_ttl_server(Arc::clone(&state), lifecycle);
        let client = connect(&ep, lifecycle);
        let uri = "instruction://ins_x@stable";
        assert_eq!(client.read_resource(uri).expect("read").text(), "v1");

        state.fail_reads.store(true, Ordering::SeqCst);

        let err = match client.read_resource(uri) {
            Err(e) => e,
            Ok(r) => panic!("{lifecycle:?}: the server refused, the client must say so: {r:?}"),
        };
        assert!(
            err.to_string().contains("Resource not found"),
            "{lifecycle:?}: {err}"
        );
        assert_eq!(state.calls("resources/read"), 2, "{lifecycle:?}");
    }
}
