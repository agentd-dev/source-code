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
    let mut head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nA2A-Version: {A2A_VERSION}\r\nContent-Length: {body_len}\r\nConnection: close\r\n"
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    head
}

/// Read one HTTP response to the end; returns `(status, body)`.
fn read_reply(s: TcpStream) -> (u16, String) {
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        if l.trim().is_empty() {
            break;
        }
    }
    let mut b = String::new();
    reader.read_to_string(&mut b).unwrap();
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, b)
}

/// POST a JSON-RPC body with `extra` headers; returns the response body.
pub fn post(addr: &str, body: &str, extra: &[(&str, &str)]) -> String {
    let s = open(addr, body, extra);
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    read_reply(s).1
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
    let (status, body) = read_reply(s);
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
                .find(|u| *u == "https://agentd.dev/a2a/ext/command")
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
    "https://agentd.dev/a2a/ext/events, https://agentd.dev/a2a/ext/task-annotations"
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
            {"uri": "https://agentd.dev/a2a/ext/events"},
            {"uri": "https://agentd.dev/a2a/ext/command"},
        ]}});
        assert_eq!(command_uri(&card), "https://agentd.dev/a2a/ext/command");
        let look_alike = json!({"capabilities": {"extensions": [
            {"uri": "https://agentd.dev/a2a/ext/command/"},
            {"uri": "https://agentd.dev/a2a/ext/commands"},
        ]}});
        assert!(std::panic::catch_unwind(|| command_uri(&look_alike)).is_err());
    }
}
