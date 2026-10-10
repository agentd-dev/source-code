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
    pub const LIST_CHANGED: &str = "notifications/events/list_changed";
}

/// The codes the extension requests — provisional until SEP-3415 is
/// accepted [SEP L750-763] — and the base ones it uses.
pub mod code {
    pub const NOT_FOUND: i64 = -32023;
    pub const FORBIDDEN: i64 = -32024;
    pub const RESOURCE_EXHAUSTED: i64 = -32025;
    pub const UNSUPPORTED: i64 = -32026;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const HEADER_MISMATCH: i64 = -32020;
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
