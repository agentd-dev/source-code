// SPDX-License-Identifier: AGPL-3.0-only
//! **The SDK's known gaps, worked around in one place.**
//!
//! The A2A project's SDK owns the protocol: the listener hands it every
//! request and sends on whatever it answers. Each function here bridges one
//! defect of the released SDK (`a2a-lf` 0.4.2, `a2a-pb` 0.3.2,
//! `a2a-server-lf` 0.5.3), and nothing outside this module may rewrite what
//! the SDK reads or writes. Each says where it is fixed upstream; **delete it
//! when `a2a-pb` / `a2a-server-lf` release that fix**, and its tests with it.
//!
//! | Gap | Upstream (a2aproject/a2a-rs) |
//! |---|---|
//! | [`request`]: a request without `params` | 4ab9443 "fix(jsonrpc): accept a request without params" |
//! | [`request`]: an integer past ±2^53 | no upstream fix yet (a2a-rs main, 2026-11-22) |
//! | [`whole_numbers`], [`answer`], [`Frames`]: an integer read back as `1.0` | 90e2876 "fix(a2a-pb): decode integral Value numbers as integers" |
//! | [`answer`]: `ListTasks`' empty fields | 1d12a9c "fix(a2a-pb): keep the REQUIRED fields of ListTasksResponse and AgentCard" |
//! | [`answer`]: the push-config page's empty fields, and `Delete…`'s `null` | no upstream fix yet (a2a-rs main, 2026-11-22) |
//! | [`outbound`]: an integer past ±2^53 in a request the client sends | no upstream fix yet (a2a-rs main, 2026-11-22) |
//! | [`whole_numbers`] on what the client reads back: an integer read as `1.0` | 90e2876 "fix(a2a-pb): decode integral Value numbers as integers" |
//! | the client's errors carry no HTTP status: a `401`, `403` or `502` whose body is not JSON-RPC reads as `-32603 failed to parse JSON-RPC response` (`JsonRpcTransport` reads the body without its status, and `reqwest` has no response hook; only a redirect's status is seen, by the redirect policy) | no upstream fix yet (a2a-rs main, 2026-11-23) |
//! | `mcp::a2a_client::framed`, used by `PaidPeer::send`: an x402 payment message sent byte for byte, its payload's whole numbers (`x402Version`) written by the SDK as doubles | 90e2876 — **delete when a2a-pb releases 90e2876**, and send the payment through the SDK |
//!
//! The client side (`mcp::a2a_client`) hands the SDK a typed request and
//! takes a typed answer, so it cannot touch the bytes: a DataPart's or a
//! metadata's whole number goes on the wire as the double the released
//! `a2a-pb` writes (`7` as `7.0`), which agentd's own listener reads back as
//! the integer ([`whole_numbers`]); and a peer's reply carrying an integer
//! past ±2^53 in one is refused by the SDK as a whole.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Bytes;
use serde_json::{Value, json};

/// The request body the SDK's router can read, or `None` when it reads the
/// body as sent (or the body is not a JSON object, which the router refuses
/// in its own words).
///
/// * **No `params`** (4ab9443). JSON-RPC 2.0 §4 lets a request omit them,
///   and `GetExtendedAgentCard` has none to send; the released router decodes
///   the missing member as `null` and refuses it as invalid params. The fix
///   reads it as `{}`, which is what this writes.
/// * **An integer no double holds exactly.** A DataPart is a
///   `google.protobuf.Value`, whose only number is a double, and the SDK's
///   ProtoJSON refuses such an integer rather than round it; carrying it is
///   rounding it, so it is written as the double it becomes.
pub fn request(body: &[u8]) -> Option<Vec<u8>> {
    let mut req = serde_json::from_slice::<Value>(body).ok()?;
    let obj = req.as_object_mut()?;
    let mut changed = false;
    if obj.get("params").is_none_or(Value::is_null) {
        obj.insert("params".to_string(), json!({}));
        changed = true;
    }
    if let Some(params) = obj.get_mut("params") {
        changed |= as_doubles(params);
    }
    changed.then(|| serde_json::to_vec(&req).unwrap_or_default())
}

/// The params of a request the client sends, as the SDK can read them into
/// its types: an integer no double holds exactly is the double it becomes,
/// as [`request`] does for one the listener is sent.
pub fn outbound(mut params: Value) -> Value {
    as_doubles(&mut params);
    params
}

/// Integers past ±2^53 in `v`, as doubles. Returns whether any was.
fn as_doubles(v: &mut Value) -> bool {
    const EXACT: u64 = 1 << 53;
    match v {
        Value::Number(n) => {
            let beyond = n.as_u64().is_some_and(|u| u >= EXACT)
                || n.as_i64().is_some_and(|i| i.unsigned_abs() >= EXACT);
            match n.as_f64() {
                Some(f) if beyond => {
                    *v = json!(f);
                    true
                }
                _ => false,
            }
        }
        Value::Array(a) => a.iter_mut().fold(false, |c, v| as_doubles(v) | c),
        Value::Object(o) => o.values_mut().fold(false, |c, v| as_doubles(v) | c),
        _ => false,
    }
}

/// `v` with every integral number written as an integer (90e2876).
///
/// A DataPart's payload is a `google.protobuf.Value`, whose only number is a
/// double, and the released `a2a-pb` gives each one back as a JSON float, so
/// the `1` a caller sent reads `1.0` — in the message the handler is given,
/// and in every answer the SDK writes. The fix gives a whole number within
/// ±2^53 back as an integer, which is what this does: to the message a send
/// carries (`ports::RuntimePorts`), to a unary answer ([`answer`]) and to
/// each frame of a stream ([`Frames`]).
pub fn whole_numbers(v: Value) -> Value {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < EXACT => json!(f as i64),
            _ => Value::Number(n),
        },
        Value::Array(a) => Value::Array(a.into_iter().map(whole_numbers).collect()),
        Value::Object(o) => {
            Value::Object(o.into_iter().map(|(k, v)| (k, whole_numbers(v))).collect())
        }
        other => other,
    }
}

/// A unary answer to `method`, as the released SDK's fixes will write it.
/// Returns whether anything changed.
///
/// * Every integral number in the `result` is an integer again
///   ([`whole_numbers`], 90e2876): the data a DataPart carries — a read
///   op's answer, an artifact — is a `google.protobuf.Value`, whose numbers
///   the released `a2a-pb` writes as `1.0`.
/// * The fields the spec's response always carries are there. ProtoJSON
///   leaves out an empty string, an empty list and a zero, so the last page
///   of a listing comes back without its `nextPageToken` and an empty one
///   without its list — fields A2A marks REQUIRED, which a client reading
///   "no token" as "more to come" breaks on. For `ListTasks` that is
///   1d12a9c; the push-config page is not fixed upstream yet.
/// * `DeleteTaskPushNotificationConfig` answers `google.protobuf.Empty`,
///   whose ProtoJSON is `{}`, where the router writes `null` (not fixed
///   upstream yet).
pub fn answer(method: Option<&str>, envelope: &mut Value) -> bool {
    use a2a_types::methods;
    let Some(result) = envelope.get_mut("result") else {
        return false;
    };
    if method == Some(methods::DELETE_PUSH_CONFIG) && result.is_null() {
        *result = json!({});
        return true;
    }
    let whole = whole_numbers(result.clone());
    // `serde_json` tells `1.0` from `1`, so this is whether any number was.
    let mut changed = whole != *result;
    *result = whole;
    let fields: &[(&str, Value)] = match method {
        Some(methods::LIST_TASKS) => &[
            ("nextPageToken", json!("")),
            ("tasks", json!([])),
            ("totalSize", json!(0)),
            ("pageSize", json!(0)),
        ],
        Some(methods::LIST_PUSH_CONFIGS) => &[("nextPageToken", json!("")), ("configs", json!([]))],
        _ => &[],
    };
    if let Some(result) = result.as_object_mut() {
        for (field, empty) in fields {
            if !result.contains_key(*field) {
                result.insert((*field).to_string(), empty.clone());
                changed = true;
            }
        }
    }
    changed
}

/// An SSE answer's frames with their whole numbers integers again
/// ([`whole_numbers`], 90e2876). Every byte but a frame's `data` goes out as
/// the SDK wrote it — the `id:` a reconnect resumes from, a keep-alive — and
/// a frame with nothing to change goes out whole.
pub struct Frames<S> {
    inner: Pin<Box<S>>,
    /// Bytes read and not yet sent: a partial frame.
    buf: Vec<u8>,
    done: bool,
}

impl<S> Frames<S> {
    pub fn new(inner: S) -> Frames<S> {
        Frames {
            inner: Box::pin(inner),
            buf: Vec::new(),
            done: false,
        }
    }
}

impl<S, E> futures_util::Stream for Frames<S>
where
    S: futures_util::Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(end) = this.buf.windows(2).position(|w| w == b"\n\n") {
                let event: Vec<u8> = this.buf.drain(..end + 2).collect();
                return Poll::Ready(Some(Ok(Bytes::from(frame(event)))));
            }
            if this.done {
                return Poll::Ready(
                    (!this.buf.is_empty()).then(|| Ok(Bytes::from(std::mem::take(&mut this.buf)))),
                );
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => this.buf.extend_from_slice(&chunk),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => this.done = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// One SSE event, its blank line included, with [`answer`]'s numbers
/// applied to its data.
fn frame(event: Vec<u8>) -> Vec<u8> {
    let text = match std::str::from_utf8(&event) {
        Ok(t) => t,
        Err(_) => return event,
    };
    let data: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| d.strip_prefix(' ').unwrap_or(d))
        .collect();
    let Ok(mut envelope) = serde_json::from_str::<Value>(&data.join("\n")) else {
        return event;
    };
    if !answer(None, &mut envelope) {
        return event;
    }
    let mut out = String::with_capacity(event.len());
    let mut wrote = false;
    for line in text.lines().filter(|l| !l.is_empty()) {
        if line.starts_with("data:") {
            if !wrote {
                out.push_str("data: ");
                out.push_str(&envelope.to_string());
                out.push('\n');
                wrote = true;
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_without_params_reads_as_empty_params() {
        let out = request(br#"{"jsonrpc":"2.0","id":1,"method":"GetExtendedAgentCard"}"#).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["params"], json!({}));
        let out = request(br#"{"jsonrpc":"2.0","id":1,"method":"X","params":null}"#).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["params"], json!({}));
        // Nothing to do: the body goes as sent, byte for byte.
        assert!(request(br#"{"jsonrpc":"2.0","id":1,"method":"X","params":{"a":1}}"#).is_none());
        // Not an object: the router's to refuse.
        assert!(request(b"[1]").is_none());
        assert!(request(b"not json").is_none());
    }

    #[test]
    fn an_integer_past_2_53_is_carried_as_its_double() {
        let out = request(
            br#"{"jsonrpc":"2.0","id":1,"method":"X","params":{"n":[9007199254740993, 7]}}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v["params"]["n"][0].is_f64(), "{v}");
        assert_eq!(v["params"]["n"][1], json!(7));
    }

    #[test]
    fn whole_numbers_are_integers_again() {
        assert_eq!(
            whole_numbers(json!({"a": 1.0, "b": [2.0, 2.5], "c": 1e300})),
            json!({"a": 1, "b": [2, 2.5], "c": 1e300})
        );
    }

    #[test]
    fn a_page_keeps_its_required_fields() {
        let mut v = json!({"jsonrpc": "2.0", "id": 1, "result": {"tasks": [{"id": "t"}]}});
        assert!(answer(Some("ListTasks"), &mut v));
        assert_eq!(
            v["result"],
            json!({"tasks": [{"id": "t"}], "nextPageToken": "", "totalSize": 0, "pageSize": 0})
        );
        let mut v = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        assert!(answer(Some("ListTaskPushNotificationConfigs"), &mut v));
        assert_eq!(v["result"], json!({"nextPageToken": "", "configs": []}));
        let mut v = json!({"jsonrpc": "2.0", "id": 1, "result": null});
        assert!(answer(Some("DeleteTaskPushNotificationConfig"), &mut v));
        assert_eq!(v["result"], json!({}));
        // An error, and a result with nothing to complete, are left alone.
        let mut v = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001}});
        assert!(!answer(Some("ListTasks"), &mut v));
        let mut v = json!({"jsonrpc": "2.0", "id": 1, "result": {"id": "t", "n": 2.5}});
        assert!(!answer(Some("GetTask"), &mut v));
    }

    #[test]
    fn an_answers_whole_numbers_are_integers() {
        let mut v = json!({"jsonrpc": "2.0", "id": 1, "result": {"message": {"parts": [
            {"data": {"turns": 1.0, "ratio": 0.5}}]}}});
        assert!(answer(Some("SendMessage"), &mut v));
        let data = &v["result"]["message"]["parts"][0]["data"];
        assert!(data["turns"].is_u64(), "{v}");
        assert_eq!(data["ratio"], json!(0.5));
    }

    #[tokio::test]
    async fn a_streams_frames_have_whole_numbers() {
        use futures_util::StreamExt;
        let chunks: Vec<Result<Bytes, ()>> =
            vec![
            Ok(Bytes::from_static(b":\n\nid: 1\ndata: {\"id\":1,\"result\":")),
            Ok(Bytes::from_static(
                b"{\"message\":{\"n\":2.0}}}\n\nid: 2\ndata: {\"id\":1,\"result\":{\"x\":0.5}}\n\n",
            )),
        ];
        let out: Vec<Bytes> = Frames::new(futures_util::stream::iter(chunks))
            .map(|c| c.unwrap())
            .collect()
            .await;
        let out: Vec<&str> = out
            .iter()
            .map(|b| std::str::from_utf8(b).unwrap())
            .collect();
        assert_eq!(
            out,
            [
                ":\n\n",
                "id: 1\ndata: {\"id\":1,\"result\":{\"message\":{\"n\":2}}}\n\n",
                "id: 2\ndata: {\"id\":1,\"result\":{\"x\":0.5}}\n\n",
            ]
        );
    }
}
