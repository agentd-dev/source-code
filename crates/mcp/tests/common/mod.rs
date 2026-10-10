// SPDX-License-Identifier: AGPL-3.0-only
//! Test servers for the MCP connection lifecycle, at both revisions agentd
//! reaches: a 2025-11-25 server (the `initialize` handshake and a session),
//! which refuses `server/discover` in each of the shapes real servers use, and
//! a 2026-07-28 server (sessionless, everything per request), which holds every
//! request to the revision's header and `_meta` rules the way rmcp's own server
//! does. Each server records every request it is sent, so a test can assert
//! exactly what reached the wire.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One request as the server read it.
#[derive(Debug, Clone)]
pub struct Req {
    /// The request line, e.g. `POST /mcp HTTP/1.1`.
    pub line: String,
    /// Every header, name lower-cased, duplicates kept in order.
    pub headers: Vec<(String, String)>,
    /// The JSON body (`Null` when there is none).
    pub body: Value,
}

impl Req {
    pub fn method(&self) -> &str {
        self.body["method"].as_str().unwrap_or("")
    }
    pub fn id(&self) -> Value {
        self.body.get("id").cloned().unwrap_or(Value::Null)
    }
    /// The first value of header `name` (lower-case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    /// How many times header `name` (lower-case) was sent.
    pub fn count(&self, name: &str) -> usize {
        self.headers.iter().filter(|(k, _)| k == name).count()
    }
    pub fn is_get(&self) -> bool {
        self.line.starts_with("GET")
    }
    /// The request's `_meta` entry `io.modelcontextprotocol/<key>`.
    pub fn meta(&self, key: &str) -> Option<&Value> {
        self.body["params"]["_meta"].get(format!("io.modelcontextprotocol/{key}"))
    }
}

/// Read one request off `r`: the head, then the declared body.
pub fn read_req(r: &mut BufReader<TcpStream>) -> Option<Req> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 || h.trim().is_empty() {
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
    if len > 0 && r.read_exact(&mut body).is_err() {
        return None;
    }
    Some(Req {
        line: line.trim_end().to_string(),
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

/// Everything a server was sent, in arrival order.
pub type Seen = Arc<Mutex<Vec<Req>>>;

/// The methods of every POST a server saw, in order.
pub fn methods(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| !r.is_get())
        .map(|r| r.method().to_string())
        .collect()
}

/// How many requests a server saw for `method`.
pub fn count(seen: &Seen, method: &str) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| r.method() == method)
        .count()
}

/// The requests a server saw for `method`.
pub fn requests(seen: &Seen, method: &str) -> Vec<Req> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| r.method() == method)
        .cloned()
        .collect()
}

/// A server answering each connection on its own thread: agentd opens one
/// connection per POST and may have several in flight. `answer` writes the
/// whole response for one request.
pub fn spawn(answer: impl Fn(&Req, &mut TcpStream) + Send + Sync + 'static) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let ep = format!("http://{}/mcp", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let s = Arc::clone(&seen);
    let answer = Arc::new(answer);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (s, answer) = (Arc::clone(&s), Arc::clone(&answer));
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(30))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let Some(req) = read_req(&mut r) else { return };
                s.lock().unwrap().push(req.clone());
                answer(&req, &mut w);
            });
        }
    });
    (ep, seen)
}

/// Write a whole response: status, headers, body.
pub fn respond_raw(w: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &str) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = w.write_all(head.as_bytes());
    let _ = w.write_all(body.as_bytes());
    let _ = w.flush();
}

/// A JSON body.
pub fn respond(w: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &Value) {
    let mut all = vec![("Content-Type", "application/json")];
    all.extend_from_slice(headers);
    respond_raw(w, status, &all, &body.to_string());
}

/// An event stream carrying `frames`, then the end of it.
pub fn respond_sse(w: &mut TcpStream, headers: &[(&str, &str)], frames: &[Value]) {
    let body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
    let mut all = vec![("Content-Type", "text/event-stream")];
    all.extend_from_slice(headers);
    respond_raw(w, 200, &all, &body);
}

pub fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

// ------------------------------------------------------------- 2025-11-25

/// How a server that predates `server/discover` refuses it — every shape seen
/// in real servers and their gateways. Each must reach the 2025-11-25
/// handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyDiscover {
    /// 200, a JSON `-32601` (method not found) on the request's id.
    MethodNotFound200,
    /// 200, an event stream whose one frame is that `-32601`.
    MethodNotFoundSse,
    /// 200, an event stream whose one frame is an error with id `null`.
    NullIdErrorSse,
    /// 400, the TypeScript SDK's `-32000` "Server not initialized", id `null`.
    NotInitialized400,
    /// 400, the Python SDK's `-32600`, id `"server-error"`.
    ServerError400,
    /// 400, a plain-text body.
    PlainText400,
    /// 404, no body.
    NotFound404,
    /// 405, no body.
    MethodNotAllowed405,
    /// 500, a JSON-RPC `-32603` with id `null`.
    Internal500Json,
    /// 500, a plain-text body.
    Plain500,
    /// 200, `{}` — a success that is not a DiscoverResult.
    EmptySuccess200,
    /// 202 with no body.
    Accepted202,
}

pub const LEGACY_SHAPES: [LegacyDiscover; 12] = [
    LegacyDiscover::MethodNotFound200,
    LegacyDiscover::MethodNotFoundSse,
    LegacyDiscover::NullIdErrorSse,
    LegacyDiscover::NotInitialized400,
    LegacyDiscover::ServerError400,
    LegacyDiscover::PlainText400,
    LegacyDiscover::NotFound404,
    LegacyDiscover::MethodNotAllowed405,
    LegacyDiscover::Internal500Json,
    LegacyDiscover::Plain500,
    LegacyDiscover::EmptySuccess200,
    LegacyDiscover::Accepted202,
];

/// Refuse a `server/discover` (request `id`) the way `shape` says.
pub fn refuse_discover(
    w: &mut TcpStream,
    shape: LegacyDiscover,
    id: &Value,
    headers: &[(&str, &str)],
) {
    use LegacyDiscover::*;
    match shape {
        MethodNotFound200 => respond(w, 200, headers, &error(id, -32601, "Method not found")),
        MethodNotFoundSse => respond_sse(w, headers, &[error(id, -32601, "Method not found")]),
        NullIdErrorSse => respond_sse(
            w,
            headers,
            &[error(&Value::Null, -32601, "Method not found")],
        ),
        NotInitialized400 => respond(
            w,
            400,
            headers,
            &error(&Value::Null, -32000, "Bad Request: Server not initialized"),
        ),
        ServerError400 => respond(
            w,
            400,
            headers,
            &error(
                &json!("server-error"),
                -32600,
                "Bad Request: No valid session ID provided",
            ),
        ),
        PlainText400 => {
            let mut all = vec![("Content-Type", "text/plain")];
            all.extend_from_slice(headers);
            respond_raw(w, 400, &all, "Bad Request")
        }
        NotFound404 => respond_raw(w, 404, headers, ""),
        MethodNotAllowed405 => respond_raw(w, 405, headers, ""),
        Internal500Json => respond(
            w,
            500,
            headers,
            &error(&Value::Null, -32603, "Internal error"),
        ),
        Plain500 => {
            let mut all = vec![("Content-Type", "text/plain")];
            all.extend_from_slice(headers);
            respond_raw(w, 500, &all, "Internal Server Error")
        }
        EmptySuccess200 => respond(w, 200, headers, &result(id, json!({}))),
        Accepted202 => respond_raw(w, 202, headers, ""),
    }
}

/// The answer to `initialize` at `version`.
pub fn initialize_answer(id: &Value, version: &str, caps: &Value) -> Value {
    result(
        id,
        json!({"protocolVersion": version, "capabilities": caps,
               "serverInfo": {"name": "legacy", "version": "0"}}),
    )
}

/// What a 2025-11-25 test server does beyond the handshake.
#[derive(Clone)]
pub struct LegacyOpts {
    pub discover: LegacyDiscover,
    pub caps: Value,
    /// The session the `initialize` answer issues.
    pub session: String,
    /// A session stamped on the discover refusal (a server that issues one
    /// to every request), which must never ride the handshake.
    pub stamp_on_refusal: Option<String>,
    /// Delay before answering `server/discover`, in ms.
    pub delay_discover_ms: u64,
    /// Never answer `server/discover` (hold the connection open).
    pub silent_discover: bool,
    /// How `initialize` is answered: `None` answers it; `Some((status, code))`
    /// refuses it (code 0: no JSON-RPC body).
    pub refuse_initialize: Option<(u16, i64)>,
}

impl Default for LegacyOpts {
    fn default() -> Self {
        LegacyOpts {
            discover: LegacyDiscover::MethodNotFound200,
            caps: json!({"tools": {}}),
            session: "s-1".into(),
            stamp_on_refusal: None,
            delay_discover_ms: 0,
            silent_discover: false,
            refuse_initialize: None,
        }
    }
}

/// A 2025-11-25 server: refuses `server/discover` as `opts.discover` says,
/// answers `initialize` with a session, and serves tools and resources.
pub fn legacy_server(opts: LegacyOpts) -> (String, Seen) {
    spawn(move |req, w| {
        if req.is_get() {
            return respond_raw(w, 405, &[], "");
        }
        let id = req.id();
        match req.method() {
            "server/discover" => {
                if opts.silent_discover {
                    std::thread::sleep(Duration::from_secs(60));
                    return;
                }
                std::thread::sleep(Duration::from_millis(opts.delay_discover_ms));
                let stamp: Vec<(&str, &str)> = opts
                    .stamp_on_refusal
                    .as_deref()
                    .map(|s| vec![("Mcp-Session-Id", s)])
                    .unwrap_or_default();
                refuse_discover(w, opts.discover, &id, &stamp)
            }
            "initialize" => match opts.refuse_initialize {
                Some((status, 0)) => respond_raw(w, status, &[], ""),
                Some((status, code)) => {
                    respond(w, status, &[], &error(&id, code, "initialize refused"))
                }
                None => respond(
                    w,
                    200,
                    &[("Mcp-Session-Id", &opts.session)],
                    &initialize_answer(&id, "2025-11-25", &opts.caps),
                ),
            },
            m if m.starts_with("notifications/") => respond_raw(w, 202, &[], ""),
            "tools/list" => respond(
                w,
                200,
                &[],
                &result(
                    &id,
                    json!({"tools": [
                {"name": "echo", "inputSchema": {"type": "object"}}]}),
                ),
            ),
            "tools/call" => respond(
                w,
                200,
                &[],
                &result(
                    &id,
                    json!({
                "content": [{"type": "text", "text": "ok"}], "isError": false}),
                ),
            ),
            "resources/read" => respond(
                w,
                200,
                &[],
                &result(
                    &id,
                    json!({
                "contents": [{"uri": req.body["params"]["uri"], "text": "x"}]}),
                ),
            ),
            _ => respond(w, 200, &[], &error(&id, -32601, "Method not found")),
        }
    })
}

// ------------------------------------------------------------- 2026-07-28

/// The one revision a 2026-07-28-only test server speaks.
pub const MODERN: &str = "2026-07-28";

/// Why a 2026-07-28 server refuses `req`, checked in the order rmcp's own
/// server checks: the revision header, the revision itself, `_meta`, then the
/// routing headers. `None` when it is acceptable.
pub fn stateless_refusal(req: &Req, supported: &[String]) -> Option<(u16, Value)> {
    let id = req.id();
    let hm = |m: &str| error(&id, -32020, m);
    if req.count("mcp-protocol-version") != 1 {
        return Some((400, hm("MCP-Protocol-Version must be sent once")));
    }
    let asked = req.header("mcp-protocol-version").unwrap_or("");
    if !supported.iter().any(|s| s == asked) {
        let mut e = error(&id, -32022, "Unsupported protocol version");
        e["error"]["data"] = json!({"supported": supported, "requested": asked});
        return Some((400, e));
    }
    if req.meta("protocolVersion").and_then(Value::as_str) != Some(asked) {
        return Some((400, hm("_meta protocolVersion does not match the header")));
    }
    if req.meta("clientCapabilities").is_none() || req.meta("clientInfo").is_none() {
        return Some((
            400,
            error(&id, -32602, "_meta lacks clientInfo or clientCapabilities"),
        ));
    }
    if req.count("mcp-method") != 1 || req.header("mcp-method") != Some(req.method()) {
        return Some((400, hm("Mcp-Method does not match the body")));
    }
    let name = match req.method() {
        "tools/call" | "prompts/get" => req.body["params"]["name"].as_str(),
        "resources/read" => req.body["params"]["uri"].as_str(),
        _ => None,
    };
    if let Some(name) = name
        && (req.count("mcp-name") != 1 || req.header("mcp-name") != Some(name))
    {
        return Some((400, hm("Mcp-Name does not match the body")));
    }
    None
}

/// The answer to `server/discover`, every required field present.
pub fn discover_answer(id: &Value, opts: &StatelessOpts) -> Value {
    let mut r = json!({
        "resultType": "complete",
        "supportedVersions": opts.supported(),
        "capabilities": opts.caps.clone(),
        "ttlMs": 0,
        "cacheScope": "private",
        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "modern", "version": "0"}},
    });
    if opts.omit_cache_fields {
        let o = r.as_object_mut().unwrap();
        o.remove("resultType");
        o.remove("ttlMs");
        o.remove("cacheScope");
    }
    result(id, r)
}

/// How a 2026-07-28-only server refuses `initialize`: the revision asked for
/// is not one it speaks.
pub fn initialize_refusal(id: &Value, asked: &str, supported: &[String]) -> Value {
    let mut e = error(id, -32022, "Unsupported protocol version");
    e["error"]["data"] = json!({"supported": supported, "requested": asked});
    e
}

/// What a 2026-07-28-only test server does.
#[derive(Clone)]
pub struct StatelessOpts {
    pub delay_discover_ms: u64,
    pub silent_discover: bool,
    /// Stamp `Mcp-Session-Id` on every answer (a server that should not).
    pub stamp_session: Option<String>,
    /// Leave `resultType`, `ttlMs` and `cacheScope` out of the discover answer.
    pub omit_cache_fields: bool,
    /// The revisions it lists; `[2026-07-28]` by default.
    pub supported: Option<Vec<String>>,
    pub caps: Value,
    /// What `tools/list` answers.
    pub tools: Vec<Value>,
    /// Answer the first `server/discover` with this instead (e.g. a `-32022`
    /// naming another revision), then behave normally.
    pub first_discover: Option<(u16, Value)>,
}

impl Default for StatelessOpts {
    fn default() -> Self {
        StatelessOpts {
            delay_discover_ms: 0,
            silent_discover: false,
            stamp_session: None,
            omit_cache_fields: false,
            supported: None,
            caps: json!({"tools": {}, "resources": {}}),
            tools: vec![json!({"name": "echo", "inputSchema": {"type": "object"}})],
            first_discover: None,
        }
    }
}

impl StatelessOpts {
    pub fn supported(&self) -> Vec<String> {
        self.supported
            .clone()
            .unwrap_or_else(|| vec![MODERN.to_string()])
    }
}

/// A 2026-07-28-only server: `server/discover` and no `initialize`, every
/// request held to the revision's rules, no session.
pub fn stateless_server(opts: StatelessOpts) -> (String, Seen) {
    let first = Arc::new(Mutex::new(opts.first_discover.clone()));
    spawn(move |req, w| {
        let stamp: Vec<(&str, &str)> = opts
            .stamp_session
            .as_deref()
            .map(|s| vec![("Mcp-Session-Id", s)])
            .unwrap_or_default();
        if req.is_get() {
            return respond_raw(w, 405, &stamp, "");
        }
        let id = req.id();
        let supported = opts.supported();
        if req.method() == "initialize" {
            let asked = req.body["params"]["protocolVersion"].as_str().unwrap_or("");
            return respond(w, 400, &stamp, &initialize_refusal(&id, asked, &supported));
        }
        if req.method() == "server/discover" {
            if let Some((status, body)) = first.lock().unwrap().take() {
                let mut body = body;
                body["id"] = id.clone();
                return respond(w, status, &stamp, &body);
            }
            if opts.silent_discover {
                std::thread::sleep(Duration::from_secs(60));
                return;
            }
            std::thread::sleep(Duration::from_millis(opts.delay_discover_ms));
        }
        if let Some((status, body)) = stateless_refusal(req, &supported) {
            return respond(w, status, &stamp, &body);
        }
        match req.method() {
            "server/discover" => respond(w, 200, &stamp, &discover_answer(&id, &opts)),
            m if m.starts_with("notifications/") => respond_raw(w, 202, &stamp, ""),
            "tools/list" => respond(w, 200, &stamp, &result(&id, json!({"tools": opts.tools}))),
            "tools/call" => respond(
                w,
                200,
                &stamp,
                &result(
                    &id,
                    json!({
                "content": [{"type": "text", "text": "ok"}], "isError": false}),
                ),
            ),
            "resources/read" => respond(
                w,
                200,
                &stamp,
                &result(
                    &id,
                    json!({
                "contents": [{"uri": req.body["params"]["uri"], "text": "x"}]}),
                ),
            ),
            // rmcp's server answers a method it does not know `404`.
            _ => respond(w, 404, &stamp, &error(&id, -32601, "Method not found")),
        }
    })
}
