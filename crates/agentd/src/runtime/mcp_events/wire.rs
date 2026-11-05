// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP Events wire, as SEP-3415 defines it ([SEP] in RFC 0045): the
//! methods, the error codes, the capability spellings, and the shapes agentd
//! reads. Everything draft-shaped lives here, so a change upstream is a change
//! in one place, and this module is deleted when rmcp ships typed Events.

use serde::Deserialize;
use serde_json::{Map, Value, json};

/// The extension's identifier [SEP L44-46].
pub const EXTENSION: &str = "io.modelcontextprotocol/events";

/// The revision the extension is defined for, and the only one agentd sends
/// `events/*` at [SEP L37, L865].
pub const REVISION: &str = "2026-07-28";

pub mod method {
    pub const LIST: &str = "events/list";
    pub const POLL: &str = "events/poll";
    /// Push: one long-lived request whose SSE body carries the stream's
    /// frames [MCPG RFC 0031 §2.1].
    pub const STREAM: &str = "events/stream";
    pub const ACTIVE: &str = "notifications/events/active";
    pub const EVENT: &str = "notifications/events/event";
    pub const HEARTBEAT: &str = "notifications/events/heartbeat";
    pub const ERROR: &str = "notifications/events/error";
    pub const TERMINATED: &str = "notifications/events/terminated";
    pub const SUBSCRIBE: &str = "events/subscribe";
    pub const UNSUBSCRIBE: &str = "events/unsubscribe";
    pub const LIST_CHANGED: &str = "notifications/events/list_changed";
}

/// The codes the extension requests — provisional until SEP-3415 is
/// accepted [SEP L750-763] — and the base ones it uses.
pub mod code {
    pub const NOT_FOUND: i64 = -32023;
    pub const FORBIDDEN: i64 = -32024;
    pub const RESOURCE_EXHAUSTED: i64 = -32025;
    pub const UNSUPPORTED: i64 = -32026;
    pub const CALLBACK_ENDPOINT_ERROR: i64 = -32027;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const HEADER_MISMATCH: i64 = -32020;

    /// The code a server means, read under either numbering: SEP-3415's
    /// `-32023`…`-32027`, or `-32011`…`-32015`, the design sketch's and
    /// ChatGPT's — which MCPG 0.1.0 sends by default (its `error_codes`
    /// numbering `draft`, MCPG RFC 0031 §2.6). Every path reads it: every
    /// `events/*` answer (`pool::call`), a push stream's frames and end, and a
    /// webhook `terminated` envelope. Any other code is unchanged.
    pub fn normalize(code: i64) -> i64 {
        if (-32015..=-32011).contains(&code) {
            code - 12
        } else {
            code
        }
    }
}

/// The server's settings object for the extension, read from its raw
/// `capabilities` in either spelling (decision Q2): SEP-3415's extension key
/// [SEP L54-75], or the design sketch's top-level `events`, which ChatGPT's
/// profile still has servers send [SEP L866]. `None`: it does not offer
/// Events, and no `events/*` request may be sent [SEP L94].
pub fn declared(capabilities: &Value) -> Option<&Value> {
    capabilities
        .get("extensions")
        .and_then(|e| e.get(EXTENSION))
        .or_else(|| capabilities.get("events"))
        .filter(|v| v.is_object())
}

/// The `subscriptions/listen` filter field that asks for
/// `notifications/events/list_changed` [SEP L196-208].
pub const LIST_CHANGED_FILTER: &str = "eventsListChanged";

/// What the client declares in its per-request capabilities [SEP L77-92].
pub fn client_extensions() -> Value {
    json!({ EXTENSION: {} })
}

/// One event type, as `events/list` describes it [SEP L115-181].
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Any non-empty subset of `poll`, `push`, `webhook`; a mode agentd does
    /// not know is kept and never chosen.
    #[serde(default)]
    pub delivery: Vec<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
    #[serde(default)]
    pub payload_schema: Option<Value>,
}

/// One page of `events/list`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPage {
    #[serde(default)]
    pub events: Vec<Descriptor>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One occurrence [SEP L223-236].
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Occurrence {
    pub event_id: String,
    pub name: String,
    pub timestamp: String,
    pub data: Value,
}

/// An `events/poll` answer [SEP L272-303]. The occurrences stay raw so one
/// that does not parse is rejected on its own, not the whole page. An absent
/// `cursor` is `null` [SEP L692].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PollPage {
    #[serde(default)]
    pub events: Vec<Value>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub next_poll_ms: Option<u64>,
}

/// The params of one `events/poll` [SEP L255-270]. A `null` cursor is "start
/// from now", and `maxAgeMs` is then ignored [SEP L701], so it is left out.
pub fn poll_params(
    name: &str,
    arguments: &Map<String, Value>,
    cursor: Option<&str>,
    max_age_ms: Option<u64>,
    max_events: u32,
) -> Map<String, Value> {
    let mut p = Map::new();
    p.insert("name".into(), json!(name));
    p.insert("arguments".into(), Value::Object(arguments.clone()));
    p.insert("cursor".into(), cursor.map_or(Value::Null, |c| json!(c)));
    if let (Some(ms), Some(_)) = (max_age_ms, cursor) {
        p.insert("maxAgeMs".into(), json!(ms));
    }
    p.insert("maxEvents".into(), json!(max_events));
    p
}

/// The params of one `events/stream` [MCPG RFC 0031 §2.1]: `poll`'s,
/// without `maxEvents` — the server sends what it holds as it holds it.
/// A `null` cursor is "from now", and `maxAgeMs` goes with a cursor only.
pub fn stream_params(
    name: &str,
    arguments: &Map<String, Value>,
    cursor: Option<&str>,
    max_age_ms: Option<u64>,
) -> Map<String, Value> {
    let mut p = Map::new();
    p.insert("name".into(), json!(name));
    p.insert("arguments".into(), Value::Object(arguments.clone()));
    p.insert("cursor".into(), cursor.map_or(Value::Null, |c| json!(c)));
    if let (Some(ms), Some(_)) = (max_age_ms, cursor) {
        p.insert("maxAgeMs".into(), json!(ms));
    }
    p
}

/// The `_meta` key every frame of a long-lived request carries: the
/// request's id (2026-07-28 `basic/patterns/subscriptions`).
pub const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";

/// One frame of an `events/stream` body [MCPG RFC 0031 §2.1, §5.3]:
/// `active {cursor, truncated}` first, and again with `truncated: true`
/// where events were skipped mid-stream; an `event` per occurrence, whose
/// `cursor` is the position after it; a `heartbeat {cursor}` as an SSE data
/// frame at least every 30 s; an `error {error}` that is transient; and
/// `terminated {error}`, after which the stream closes.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Active {
        cursor: Option<String>,
        truncated: bool,
    },
    /// The occurrence, raw: checked on its own like a poll page's.
    Event(Value),
    Heartbeat {
        cursor: Option<String>,
    },
    Error(::mcp::rpc::RpcError),
    Terminated(::mcp::rpc::RpcError),
}

/// The frame a stream notification carries; `None` for any other message,
/// or one whose params are not the frame's shape. A cursor that is not a
/// non-empty string is none: a null cursor is never kept [MCPG RFC 0031
/// §2.3].
pub fn frame(n: &Value) -> Option<Frame> {
    let p = n.get("params").filter(|p| p.is_object())?;
    let cursor = || {
        p.get("cursor")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string)
    };
    let error = || serde_json::from_value::<::mcp::rpc::RpcError>(p.get("error")?.clone()).ok();
    Some(match n.get("method")?.as_str()? {
        method::ACTIVE => Frame::Active {
            cursor: cursor(),
            truncated: p.get("truncated").and_then(Value::as_bool) == Some(true),
        },
        method::EVENT => {
            let mut o = p.clone();
            // The stream's tag, not the occurrence's own `_meta`.
            if let Some(m) = o.get_mut("_meta").and_then(Value::as_object_mut) {
                m.remove(SUBSCRIPTION_ID);
                if m.is_empty()
                    && let Some(obj) = o.as_object_mut()
                {
                    obj.remove("_meta");
                }
            }
            Frame::Event(o)
        }
        method::HEARTBEAT => Frame::Heartbeat { cursor: cursor() },
        method::ERROR => Frame::Error(error()?),
        method::TERMINATED => Frame::Terminated(error()?),
        _ => return None,
    })
}

/// The params of one webhook `events/subscribe` — a first subscribe or a
/// refresh, the same call [SEP "Subscribing"]. `cursor` is the last one
/// persisted, `null` for "from now"; `maxAgeMs` goes with a cursor only.
pub fn subscribe_params(
    name: &str,
    arguments: &Map<String, Value>,
    url: &str,
    secret: &str,
    cursor: Option<&str>,
    max_age_ms: Option<u64>,
    ttl_ms: u64,
) -> Map<String, Value> {
    let mut p = Map::new();
    p.insert("name".into(), json!(name));
    p.insert("arguments".into(), Value::Object(arguments.clone()));
    p.insert(
        "delivery".into(),
        json!({"mode": "webhook", "url": url, "secret": secret}),
    );
    p.insert("cursor".into(), cursor.map_or(Value::Null, |c| json!(c)));
    if let (Some(ms), Some(_)) = (max_age_ms, cursor) {
        p.insert("maxAgeMs".into(), json!(ms));
    }
    p.insert("ttlMs".into(), json!(ttl_ms));
    p
}

/// The params of one `events/unsubscribe`: the subscription key's parts the
/// client supplies [SEP "Unsubscribing"].
pub fn unsubscribe_params(
    name: &str,
    arguments: &Map<String, Value>,
    url: &str,
) -> Map<String, Value> {
    let mut p = Map::new();
    p.insert("name".into(), json!(name));
    p.insert("arguments".into(), Value::Object(arguments.clone()));
    p.insert("delivery".into(), json!({"url": url}));
    p
}

/// An `events/subscribe` answer [SEP "Subscribing", "Webhook delivery
/// status"]. `refreshBefore: null` is no expiry, which agentd never asks for.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscribed {
    pub id: String,
    #[serde(default)]
    pub refresh_before: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub delivery_status: Option<DeliveryStatus>,
}

/// How the server's deliveries to the callback are going. Optional, and
/// `lastError` is one of a closed set of categories, never endpoint output.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryStatus {
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub throttled: Option<bool>,
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
}

impl DeliveryStatus {
    /// Some deliveries failed: suspended, or a last error reported.
    pub fn failing(&self) -> bool {
        self.active == Some(false) || self.last_error.is_some()
    }
}

/// The params of one `events/list` page.
pub fn list_params(cursor: Option<&str>) -> Map<String, Value> {
    let mut p = Map::new();
    if let Some(c) = cursor {
        p.insert("cursor".into(), json!(c));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SEP's own examples, as JSON (its blocks are JSONC: the comments
    /// and trailing commas are dropped, nothing else changed), read through
    /// these types: a drift here is a change upstream to look at.
    #[test]
    fn the_seps_examples_read_through_the_wire_types() {
        // [SEP L117-176]
        let list: ListPage = serde_json::from_value(json!({
          "resultType": "complete",
          "events": [
            {
              "name": "email.received",
              "description": "Fires when a new email arrives in the inbox",
              "delivery": ["poll"],
              "inputSchema": {"type": "object", "properties": {
                  "from": {"type": "string", "description": "Glob pattern for sender address"},
                  "subject_contains": {"type": "string"},
                  "redact_pii": {"type": "boolean", "default": false, "description": "Strip PII from event payloads"},
                  "include_body_preview": {"type": "boolean", "default": true, "description": "Include a snippet of the email body"}}},
              "payloadSchema": {"type": "object", "properties": {
                  "messageId": {"type": "string"}, "from": {"type": "string"},
                  "subject": {"type": "string"}, "receivedAt": {"type": "string", "format": "date-time"}}}
            },
            {
              "name": "incident.created",
              "description": "Fires when a new PagerDuty incident is created",
              "delivery": ["webhook", "push", "poll"],
              "inputSchema": {"type": "object", "properties": {
                  "severity": {"type": "string", "enum": ["P1", "P2", "P3", "P4"]},
                  "service": {"type": "string"},
                  "deduplicate_window_seconds": {"type": "integer", "default": 0, "description": "Suppress duplicate alerts within this window"}}},
              "payloadSchema": {"...": "..."},
              "_meta": {"...": "..."}
            }
          ],
          "nextCursor": "..."
        }))
        .unwrap();
        assert_eq!(list.events.len(), 2);
        assert_eq!(list.events[0].delivery, ["poll"]);
        assert_eq!(list.events[1].delivery, ["webhook", "push", "poll"]);
        assert!(list.events[0].payload_schema.is_some());
        assert_eq!(list.next_cursor.as_deref(), Some("..."));

        // [SEP L274-295]
        let page: PollPage = serde_json::from_value(json!({
          "resultType": "complete",
          "events": [{
              "eventId": "evt_001",
              "name": "email.received",
              "timestamp": "2026-02-19T15:30:00Z",
              "data": {"messageId": "msg_xyz", "from": "dsp@anthropic.com",
                       "subject": "MCP spec review", "receivedAt": "2026-02-19T15:29:58Z"}
          }],
          "cursor": "historyId_99842",
          "truncated": false,
          "hasMore": false,
          "nextPollMs": 30000
        }))
        .unwrap();
        assert_eq!(page.cursor.as_deref(), Some("historyId_99842"));
        assert_eq!(page.next_poll_ms, Some(30000));
        assert!(!page.has_more && !page.truncated);
        let o: Occurrence = serde_json::from_value(page.events[0].clone()).unwrap();
        assert_eq!(o.event_id, "evt_001");
        assert_eq!(o.data["messageId"], "msg_xyz");

        // An absent cursor is a null one [SEP L692].
        let quiet: PollPage = serde_json::from_value(json!({"events": []})).unwrap();
        assert_eq!(quiet.cursor, None);
    }

    #[test]
    fn the_declaration_is_read_in_either_spelling() {
        // SEP-3415's extension key [SEP L60-73].
        let sep = json!({"extensions": {EXTENSION: {"listChanged": true}}});
        assert_eq!(declared(&sep).unwrap()["listChanged"], true);
        // The sketch's top-level key, which ChatGPT's profile sends.
        let top = json!({"events": {}});
        assert!(declared(&top).is_some());
        // Neither: no Events.
        assert!(declared(&json!({"tools": {}})).is_none());
        assert!(declared(&json!({"extensions": {"io.example/x": {}}})).is_none());
        // A declaration that is not an object is not one.
        assert!(declared(&json!({"events": true})).is_none());
    }

    #[test]
    fn the_seps_webhook_examples_read_through_the_wire_types() {
        // [SEP "Subscribing"]
        let ok: Subscribed = serde_json::from_value(json!({
          "resultType": "complete",
          "id": "sub_a3f1c8e2b0d49f7e",
          "refreshBefore": "2026-02-19T16:30:00Z",
          "cursor": "cursor_start_001",
          "truncated": false
        }))
        .unwrap();
        assert_eq!(ok.id, "sub_a3f1c8e2b0d49f7e");
        assert!(ok.delivery_status.is_none());
        // [SEP "Webhook delivery status"]
        let failing: Subscribed = serde_json::from_value(json!({
          "resultType": "complete", "id": "sub_a", "refreshBefore": "2026-02-19T17:00:00Z",
          "cursor": "cursor_xyz", "truncated": false,
          "deliveryStatus": {"active": false, "lastDeliveryAt": "2026-02-19T15:45:00Z",
                             "lastError": "http_4xx", "failedSince": "2026-02-19T15:50:00Z"}
        }))
        .unwrap();
        assert!(failing.delivery_status.unwrap().failing());
        let throttled: DeliveryStatus = serde_json::from_value(json!({
          "active": true, "lastDeliveryAt": "2026-02-19T16:28:00Z", "lastError": null,
          "throttled": true, "retryAfterMs": 60000
        }))
        .unwrap();
        assert!(!throttled.failing());
        assert_eq!(throttled.retry_after_ms, Some(60000));
        let args = json!({"severity": "P1"}).as_object().unwrap().clone();
        let p = subscribe_params(
            "incident.created",
            &args,
            "https://h/x",
            "whsec_k",
            None,
            Some(1),
            3_600_000,
        );
        assert_eq!(
            p["delivery"],
            json!({"mode": "webhook", "url": "https://h/x", "secret": "whsec_k"})
        );
        assert_eq!(p["cursor"], Value::Null);
        assert!(!p.contains_key("maxAgeMs"));
        assert_eq!(p["ttlMs"], 3_600_000);
        let u = unsubscribe_params("incident.created", &args, "https://h/x");
        assert_eq!(u["delivery"], json!({"url": "https://h/x"}));
    }

    /// MCPG 0.1.0's own frames (`libs/mcp-wire/…/events/wire.rs`,
    /// `stream_frames_match_the_sketch`), read through [`frame`].
    #[test]
    fn mcpgs_stream_frames_read_through_the_frame_type() {
        let sub = json!({SUBSCRIPTION_ID: 1});
        let active = json!({"jsonrpc": "2.0", "method": "notifications/events/active",
            "params": {"cursor": "c1", "truncated": false, "_meta": sub}});
        assert_eq!(
            frame(&active),
            Some(Frame::Active {
                cursor: Some("c1".into()),
                truncated: false
            })
        );
        let gap = json!({"jsonrpc": "2.0", "method": "notifications/events/active",
            "params": {"cursor": "c9", "truncated": true, "_meta": sub}});
        assert!(matches!(
            frame(&gap),
            Some(Frame::Active {
                truncated: true,
                ..
            })
        ));
        // A null cursor is none: never kept.
        let beat = json!({"jsonrpc": "2.0", "method": "notifications/events/heartbeat",
            "params": {"cursor": null, "_meta": {SUBSCRIPTION_ID: "s-1"}}});
        assert_eq!(frame(&beat), Some(Frame::Heartbeat { cursor: None }));
        let event = json!({"jsonrpc": "2.0", "method": "notifications/events/event",
            "params": {"eventId": "evt_001", "name": "email.received",
                       "timestamp": "2026-02-19T15:30:00Z", "data": {"subject": "hi"},
                       "cursor": "c2", "_meta": {SUBSCRIPTION_ID: 1, "traceparent": "00-x"}}});
        let Some(Frame::Event(o)) = frame(&event) else {
            panic!("an event")
        };
        assert_eq!(o["eventId"], "evt_001");
        assert_eq!(o["cursor"], "c2");
        assert_eq!(
            o["_meta"],
            json!({"traceparent": "00-x"}),
            "the stream's tag is gone"
        );
        let bare = json!({"method": "notifications/events/event",
            "params": {"eventId": "e", "_meta": {SUBSCRIPTION_ID: 1}}});
        let Some(Frame::Event(o)) = frame(&bare) else {
            panic!("an event")
        };
        assert!(o.get("_meta").is_none(), "{o}");
        let ended = json!({"jsonrpc": "2.0", "method": "notifications/events/terminated",
            "params": {"error": {"code": -32012, "message": "forbidden",
                                 "data": {"reason": "revoked"}}, "_meta": sub}});
        let Some(Frame::Terminated(e)) = frame(&ended) else {
            panic!("terminated")
        };
        assert_eq!(e.code, -32012);
        assert_eq!(code::normalize(e.code), code::FORBIDDEN);
        let transient = json!({"method": "notifications/events/error",
            "params": {"error": {"code": -32603, "message": "store"}}});
        assert!(matches!(frame(&transient), Some(Frame::Error(e)) if e.code == -32603));
        // Not a frame: another notification, a frame without its params'
        // shape, the stream's final result.
        assert_eq!(
            frame(&json!({"method": "notifications/message", "params": {}})),
            None
        );
        assert_eq!(
            frame(&json!({"method": "notifications/events/terminated", "params": {}})),
            None
        );
        assert_eq!(
            frame(&json!({"jsonrpc": "2.0", "id": 1, "result": {"resultType": "complete"}})),
            None
        );
    }

    #[test]
    fn either_numbering_reads_as_the_same_code() {
        for (draft, sep) in [
            (-32011, code::NOT_FOUND),
            (-32012, code::FORBIDDEN),
            (-32013, code::RESOURCE_EXHAUSTED),
            (-32014, code::UNSUPPORTED),
            (-32015, code::CALLBACK_ENDPOINT_ERROR),
        ] {
            assert_eq!(code::normalize(draft), sep);
            assert_eq!(code::normalize(sep), sep);
        }
        assert_eq!(
            code::normalize(code::METHOD_NOT_FOUND),
            code::METHOD_NOT_FOUND
        );
        assert_eq!(code::normalize(-32010), -32010);
        assert_eq!(code::normalize(-32016), -32016);
    }

    #[test]
    fn a_stream_from_now_carries_no_max_age_and_asks_no_page() {
        let args = json!({"topic": "t"}).as_object().unwrap().clone();
        let p = stream_params("mock.tick", &args, None, Some(3_600_000));
        assert_eq!(p["cursor"], Value::Null);
        assert!(!p.contains_key("maxAgeMs"));
        assert!(!p.contains_key("maxEvents"));
        let p = stream_params("mock.tick", &args, Some("c3"), Some(3_600_000));
        assert_eq!(p["cursor"], "c3");
        assert_eq!(p["maxAgeMs"], 3_600_000);
    }

    #[test]
    fn a_poll_from_now_carries_no_max_age() {
        let args = json!({"severity": "P1"}).as_object().unwrap().clone();
        let p = poll_params("incident.created", &args, None, Some(3_600_000), 50);
        assert_eq!(p["cursor"], Value::Null);
        assert!(
            !p.contains_key("maxAgeMs"),
            "ignored with a null cursor [SEP L701]"
        );
        let p = poll_params("incident.created", &args, Some("c-1"), Some(3_600_000), 50);
        assert_eq!(p["cursor"], "c-1");
        assert_eq!(p["maxAgeMs"], 3_600_000);
        assert_eq!(p["arguments"]["severity"], "P1");
        assert_eq!(p["maxEvents"], 50);
        let p = poll_params("incident.created", &args, Some("c-1"), None, 50);
        assert!(!p.contains_key("maxAgeMs"), "replay: none");
    }
}
