// SPDX-License-Identifier: AGPL-3.0-only
//! Shared helpers for the conformance families: write config/playbook files
//! into a check-scoped temp dir, and drive the built-in mock LLM from a JSON
//! playbook (the `{"turns":[…]}` format the runtime e2e uses). Scoping every
//! file to the check's own temp dir is what lets checks run concurrently
//! without one clobbering another's config.
//!
//! And the A2A client every check speaks through, written the way a peer that
//! knows only the specification would write it: every request carries
//! `A2A-Version`, a message is `ROLE_USER` with an explicit `returnImmediately`,
//! the card is read by GET from its well-known path, and a command's extension
//! URI is DISCOVERED from that card rather than typed in — the suite is a black
//! box and links nothing from agentd, so it may not know the URI any other way.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::harness::{Harness, MockLlm, TempDir};
use serde_json::{Value, json};

/// The URIs of the extensions agentd declares, each spelled once for the
/// whole suite. They are wire strings a spec-only peer reads off the card, not
/// a link into agentd, so naming them keeps the suite a black box; a check
/// still confirms a card declares one before it relies on it.
pub const COMMAND: &str = "https://agentd.dev/a2a/ext/command";
pub const EVENTS: &str = "https://agentd.dev/a2a/ext/events";
pub const ANNOTATIONS: &str = "https://agentd.dev/a2a/ext/task-annotations";

/// Write `contents` to `name` inside `tmp`; return the absolute path.
pub fn write_file(tmp: &TempDir, name: &str, contents: &str) -> String {
    let p = tmp.path().join(name);
    std::fs::write(&p, contents).expect("write temp file");
    p.to_str().expect("utf8 path").to_string()
}

/// Launch the built-in mock LLM driven by a JSON `playbook` (`{"turns":[…]}` +
/// optional `"match"`), written into `tmp`. The intelligence endpoint is
/// [`MockLlm::uri`].
pub fn mock_llm(h: &Harness, tmp: &TempDir, playbook: &Value) -> MockLlm {
    let pb = write_file(tmp, "playbook.json", &playbook.to_string());
    h.mock_llm(&format!("file:{pb}"))
}

/// The protocol version every request states in `A2A-Version`: the spec's
/// literal, since a peer is written against a version, not against a server.
pub const A2A_VERSION: &str = "1.0";

/// The request line and headers of a POST to `/`. Every request the suite
/// sends is written here, so none of them can omit the version.
fn post_head(body_len: usize, extra: &[(&str, &str)]) -> String {
    head_stating(Some(A2A_VERSION), body_len, extra)
}

/// The head of a POST to `/` that states `version` in `A2A-Version`, or sends
/// no such header for `None`. Only the version gate's own check chooses the
/// version; every other request goes through [`post_head`].
fn head_stating(version: Option<&str>, body_len: usize, extra: &[(&str, &str)]) -> String {
    let mut head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n"
    );
    if let Some(v) = version {
        head.push_str(&format!("A2A-Version: {v}\r\n"));
    }
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    head
}

/// A response's header fields, names lowercased.
pub type Headers = Vec<(String, String)>;

/// The value of the header `name` (lowercase), if the response carried it.
pub fn header<'h>(headers: &'h Headers, name: &str) -> Option<&'h str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Read a response's status line and header fields.
fn read_head(reader: &mut BufReader<TcpStream>) -> (u16, Headers) {
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    let mut headers = Vec::new();
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        if l.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, headers)
}

/// Read one HTTP response to the end; returns `(status, headers, body)`. A
/// chunked body (an SSE stream) is returned with its chunk framing, which no
/// check parses past: the frames are found by their `data:` lines.
fn read_reply(s: TcpStream) -> (u16, Headers, String) {
    let mut reader = BufReader::new(s);
    let (code, headers) = read_head(&mut reader);
    let mut raw = Vec::new();
    reader.read_to_end(&mut raw).ok();
    if header(&headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        raw = dechunk(&raw);
    }
    (code, headers, String::from_utf8_lossy(&raw).into_owned())
}

/// The payload of a chunked body. Decoded rather than read with the framing
/// left in, because a chunk boundary may fall inside an SSE frame, and a check
/// that parses frames must see each one whole.
fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = raw.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&raw[..eol])
            .ok()
            .and_then(|l| usize::from_str_radix(l.split(';').next()?.trim(), 16).ok());
        let Some(size) = size.filter(|n| *n > 0) else {
            break;
        };
        let start = eol + 2;
        let end = (start + size).min(raw.len());
        out.extend_from_slice(&raw[start..end]);
        raw = raw.get(end + 2..).unwrap_or_default();
    }
    out
}

/// POST a JSON-RPC body with `extra` headers; returns the status, the
/// headers and the body — for a check that judges the HTTP layer too.
pub fn post_full(addr: &str, body: &str, extra: &[(&str, &str)]) -> (u16, Headers, String) {
    let s = open(addr, body, extra);
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    read_reply(s)
}

/// POST a JSON-RPC body stating `version` in `A2A-Version` (or none); returns
/// the envelope. The version gate's check is the only caller.
pub fn post_stating(addr: &str, version: Option<&str>, body: &str) -> Value {
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(head_stating(version, body.len(), &[]).as_bytes())
        .unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let (_, _, reply) = read_reply(s);
    serde_json::from_str(&reply).unwrap_or_else(|_| panic!("non-JSON A2A response: {reply:?}"))
}

/// One raw HTTP exchange; `(status, headers, body)`. By hand, because what a
/// browser sends — its `Origin`, its preflight — or a GET of the card is the
/// thing under test, not a JSON-RPC call.
pub fn exchange(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, Headers, String) {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: x\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(req.as_bytes()).expect("write request");
    read_reply(s)
}

/// POST a JSON-RPC body with `extra` headers; returns the response body.
pub fn post(addr: &str, body: &str, extra: &[(&str, &str)]) -> String {
    post_full(addr, body, extra).2
}

/// POST and hand back the connection, for a caller reading an SSE stream.
pub fn open(addr: &str, body: &str, extra: &[(&str, &str)]) -> TcpStream {
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.write_all(post_head(body.len(), extra).as_bytes())
        .unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();
    s
}

pub fn rpc_body(id: i64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// A JSON-RPC call; returns the whole envelope, so a check can assert on the
/// JSON-RPC **error** as well as the result.
pub fn rpc_value(addr: &str, id: i64, method: &str, params: Value) -> Value {
    let resp = post(addr, &rpc_body(id, method, params), &[]);
    serde_json::from_str(&resp).unwrap_or_else(|_| panic!("non-JSON A2A response: {resp:?}"))
}

/// A JSON-RPC call that must succeed; returns the `result` (panics — caught by
/// `run_check` and reported as a failure — on a transport / RPC error).
pub fn rpc(addr: &str, id: i64, method: &str, params: Value) -> Value {
    let v = rpc_value(addr, id, method, params);
    assert!(v.get("error").is_none(), "A2A rpc error for {method}: {v}");
    v["result"].clone()
}

/// The public agent card, fetched the way the spec publishes it.
pub fn get_card(addr: &str) -> Value {
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(
        b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: x\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let (status, _, body) = read_reply(s);
    assert_eq!(status, 200, "GET agent-card.json: {status} {body:?}");
    serde_json::from_str(&body).unwrap_or_else(|_| panic!("non-JSON agent card: {body:?}"))
}

/// The URI of the extension that carries agentd's commands, as the card
/// declares it — matched whole, because a URI is an exact identifier: a card
/// that declares something else under a similar name does not carry agentd's
/// commands, and a check sending one there would be testing nothing.
pub fn command_uri(card: &Value) -> String {
    card["capabilities"]["extensions"]
        .as_array()
        .and_then(|exts| {
            exts.iter()
                .filter_map(|e| e["uri"].as_str())
                .find(|u| *u == COMMAND)
        })
        .unwrap_or_else(|| panic!("the card declares no command extension: {card}"))
        .to_string()
}

/// The JSON-RPC method of the observation feed, named once for every check:
/// the method agentd's events extension declares.
pub fn feed_method() -> &'static str {
    "agentd.events/SubscribeToEvents"
}

/// The `A2A-Extensions` value a display client opens the feed with: the
/// events extension, which the method belongs to and is refused without, and
/// task-annotations, which a `task` event carries only when activated.
pub fn feed_activation() -> &'static str {
    static VALUE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VALUE.get_or_init(|| format!("{EVENTS}, {ANNOTATIONS}"))
}

/// A fresh `messageId`: no two messages the suite sends share one.
fn message_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "conf-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// `SendMessage` params for a natural-language message.
pub fn text_params(text: &str, task_id: Option<&str>, return_immediately: bool) -> Value {
    let mut message =
        json!({"role": "ROLE_USER", "messageId": message_id(), "parts": [{"text": text}]});
    if let Some(t) = task_id {
        message["taskId"] = json!(t);
    }
    json!({"message": message, "configuration": {"returnImmediately": return_immediately}})
}

/// Send `text`; returns the envelope.
pub fn send_text(addr: &str, id: i64, text: &str, return_immediately: bool) -> Value {
    rpc_value(
        addr,
        id,
        "SendMessage",
        text_params(text, None, return_immediately),
    )
}

/// Run the command `op` the way a client that read the card would: the command
/// extension's URI is looked up on the card, the message is marked with it, and
/// the request activates it. Returns the envelope.
pub fn send_command(addr: &str, id: i64, op: &str) -> Value {
    let uri = command_uri(&get_card(addr));
    let resp = post(
        addr,
        &rpc_body(id, "SendMessage", command_params(&uri, op)),
        &[("A2A-Extensions", &uri)],
    );
    serde_json::from_str(&resp).unwrap_or_else(|_| panic!("non-JSON A2A response: {resp:?}"))
}

/// `SendMessage` params for the command `op`, marked with the extension `uri`.
fn command_params(uri: &str, op: &str) -> Value {
    json!({
        "message": {
            "role": "ROLE_USER",
            "messageId": message_id(),
            "extensions": [uri],
            "parts": [{"data": {"agentd": {"op": op}}}],
        },
        "configuration": {"returnImmediately": false},
    })
}

/// Send the command `op` dressed the way a careless client would: `activate`
/// says whether the `A2A-Extensions` header names the extension, `mark` whether
/// the message lists it. A client that read the card does both; the checks
/// that prove each one is REQUIRED leave it out. Returns the envelope.
pub fn send_command_dressed(addr: &str, id: i64, op: &str, activate: bool, mark: bool) -> Value {
    let uri = command_uri(&get_card(addr));
    let mut params = command_params(&uri, op);
    if !mark {
        params["message"]
            .as_object_mut()
            .expect("a message")
            .remove("extensions");
    }
    let headers: &[(&str, &str)] = if activate {
        &[("A2A-Extensions", &uri)]
    } else {
        &[]
    };
    let resp = post(addr, &rpc_body(id, "SendMessage", params), headers);
    serde_json::from_str(&resp).unwrap_or_else(|_| panic!("non-JSON A2A response: {resp:?}"))
}

/// Send the command `op`, marked, with `activation` as the whole
/// `A2A-Extensions` value — for a check that activates another extension
/// beside the command one. Returns the envelope.
pub fn send_command_activating(addr: &str, id: i64, op: &str, activation: &str) -> Value {
    let uri = command_uri(&get_card(addr));
    let resp = post(
        addr,
        &rpc_body(id, "SendMessage", command_params(&uri, op)),
        &[("A2A-Extensions", activation)],
    );
    serde_json::from_str(&resp).unwrap_or_else(|_| panic!("non-JSON A2A response: {resp:?}"))
}

/// Stream the command `op` (`SendStreamingMessage`), dressed as a client that
/// read the card; the status, the headers and the frames.
pub fn stream_command(addr: &str, id: i64, op: &str) -> (u16, Headers, Vec<Frame>) {
    let uri = command_uri(&get_card(addr));
    stream(
        addr,
        &rpc_body(id, "SendStreamingMessage", command_params(&uri, op)),
        &[("A2A-Extensions", &uri)],
    )
}

/// One server-sent event: its `id:` line, if it had one, and its data.
#[derive(Debug, Clone)]
pub struct Frame {
    pub id: Option<String>,
    pub data: Value,
}

/// The events of an SSE body, in order. A `data:` line that is not JSON is
/// kept as a string, so a malformed frame fails the check that reads it
/// rather than vanishing.
pub fn sse_frames(body: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    for event in body.split("\n\n") {
        let mut id = None;
        let mut data = String::new();
        for line in event.lines() {
            if let Some(v) = line.strip_prefix("id:") {
                id = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push_str(v.trim());
            }
        }
        if !data.is_empty() {
            let data = serde_json::from_str(&data).unwrap_or(Value::String(data));
            frames.push(Frame { id, data });
        }
    }
    frames
}

/// POST a streaming request and read the stream to its close; the status,
/// the headers and the frames (none when the answer was plain JSON).
pub fn stream(addr: &str, body: &str, extra: &[(&str, &str)]) -> (u16, Headers, Vec<Frame>) {
    let (status, headers, reply) = post_full(addr, body, extra);
    (status, headers, sse_frames(&reply))
}

/// POST a request whose answer may be a stream that never closes (the feed),
/// and return as soon as the first frame arrives: the status, the headers,
/// and the first frame's data — or, for a plain JSON answer, the envelope.
pub fn first_frame(addr: &str, body: &str, extra: &[(&str, &str)]) -> (u16, Headers, Value) {
    let s = open(addr, body, extra);
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(s);
    let (status, headers) = read_head(&mut reader);
    let sse = header(&headers, "content-type").is_some_and(|c| c.starts_with("text/event-stream"));
    if !sse {
        let mut b = Vec::new();
        reader.read_to_end(&mut b).ok();
        if header(&headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
        {
            b = dechunk(&b);
        }
        let v = serde_json::from_slice(&b).unwrap_or(Value::Null);
        return (status, headers, v);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(v) = serde_json::from_str::<Value>(data.trim())
        {
            return (status, headers, v);
        }
    }
    (status, headers, Value::Null)
}

/// A JSON-RPC call presenting `bearer`; the status, the headers and the
/// envelope (`Null` when the body is not JSON).
pub fn rpc_as(
    addr: &str,
    bearer: &str,
    id: i64,
    method: &str,
    params: Value,
) -> (u16, Headers, Value) {
    let auth = format!("Bearer {bearer}");
    let extra: &[(&str, &str)] = if bearer.is_empty() {
        &[]
    } else {
        &[("Authorization", &auth)]
    };
    let (status, headers, body) = post_full(addr, &rpc_body(id, method, params), extra);
    (
        status,
        headers,
        serde_json::from_str(&body).unwrap_or(Value::Null),
    )
}

/// A free loopback port (bind :0, read it, drop). agentd rebinds within ms.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// The URIs of every extension the card declares, in its order.
pub fn declared_extensions(card: &Value) -> Vec<String> {
    card["capabilities"]["extensions"]
        .as_array()
        .map(|exts| {
            exts.iter()
                .filter_map(|e| e["uri"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Block until the listener accepts a connection, or fail past the deadline.
pub fn wait_ready(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a2a listener never became connectable at {addr}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request states the version and the content type: the head is
    /// written in one place, and this pins what that place writes.
    #[test]
    fn every_request_states_the_version_and_the_content_type() {
        let head = post_head(2, &[("A2A-Extensions", "urn:x")]).to_ascii_lowercase();
        assert!(head.starts_with("post / http/1.1\r\n"), "{head}");
        assert!(
            head.contains("\r\ncontent-type: application/json\r\n"),
            "{head}"
        );
        assert!(head.contains("\r\na2a-version: 1.0\r\n"), "{head}");
        assert!(head.contains("\r\na2a-extensions: urn:x\r\n"), "{head}");
        assert!(head.ends_with("\r\n\r\n"), "{head}");
    }

    /// A message is `ROLE_USER` with an explicit `returnImmediately`, and a
    /// command carries the extension it was sent under.
    #[test]
    fn messages_are_spec_shaped_and_commands_are_marked() {
        let c = command_params("urn:x", "status");
        assert_eq!(c["message"]["role"], "ROLE_USER", "{c}");
        assert_eq!(c["message"]["extensions"], json!(["urn:x"]), "{c}");
        assert_eq!(c["configuration"]["returnImmediately"], false, "{c}");

        let t = text_params("hi", Some("t-1"), true);
        assert_eq!(t["message"]["role"], "ROLE_USER", "{t}");
        assert_eq!(t["message"]["taskId"], "t-1", "{t}");
        assert_eq!(t["configuration"]["returnImmediately"], true, "{t}");
        assert_ne!(
            text_params("a", None, false)["message"]["messageId"],
            text_params("a", None, false)["message"]["messageId"],
            "no two messages share an id"
        );
    }

    /// The command URI is the one the card declares, exactly: a card
    /// without it — or with only a look-alike — has no command extension.
    #[test]
    fn the_command_uri_is_the_declared_one() {
        let card = json!({"capabilities": {"extensions": [
            {"uri": EVENTS},
            {"uri": COMMAND},
        ]}});
        assert_eq!(command_uri(&card), COMMAND);
        let look_alike = json!({"capabilities": {"extensions": [
            {"uri": format!("{COMMAND}/")},
            {"uri": format!("{COMMAND}s")},
        ]}});
        assert!(std::panic::catch_unwind(|| command_uri(&look_alike)).is_err());
    }
}
