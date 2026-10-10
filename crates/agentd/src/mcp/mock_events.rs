// SPDX-License-Identifier: AGPL-3.0-only
//! The built-in mock's MCP Events surface (SEP-3415), on its 2026-07-28
//! edge: two event types — `mock.tick`, pollable, with an `inputSchema` and a
//! `payloadSchema`, and `mock.webhook_only` — listed one per page, and a
//! cursor log per `topic` argument the control tools write to.
//!
//! Each request leaves a line on stderr, the server side's record of the
//! wire: `MOCK_EVENTS_LIST <cursor>` and `MOCK_EVENTS_POLL <params>`.
//!
//! Control tools (`tools/call`, unlisted): `mock.event {topic, count, id?,
//! bad_schema?, oversize?, malformed?}` appends occurrences;
//! `mock.event_trim {topic}` drops everything appended so far and one more
//! occurrence the client never read, so its next poll is answered `truncated`; `mock.events_fail {code, data?,
//! times?}` answers the next polls with that error; `mock.events_flags
//! {always_more?, repeat?}` makes every answer say `hasMore`, or the next
//! answer repeat the last page that held occurrences.

use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

/// How a mock declares Events in its `server/discover` capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spelling {
    /// SEP-3415's `capabilities.extensions["io.modelcontextprotocol/events"]`.
    Extension,
    /// The design sketch's top-level `capabilities.events`.
    TopLevel,
    /// Not declared: a server without Events.
    None,
}

#[derive(Default)]
struct Inner {
    /// topic → (seq, eventId, data), oldest first.
    log: BTreeMap<String, Vec<(u64, String, Value)>>,
    head: BTreeMap<String, u64>,
    /// Errors the next polls are answered with.
    fail: VecDeque<Value>,
    always_more: bool,
    repeat: bool,
    /// The last page answered, for `repeat`.
    last: Option<Value>,
}

/// The mock's Events state, shared by every connection.
#[derive(Default)]
pub struct Events {
    inner: Mutex<Inner>,
}

const NOT_FOUND: i64 = -32023;
const FORBIDDEN: i64 = -32024;
const UNSUPPORTED: i64 = -32026;
const INVALID_PARAMS: i64 = -32602;

/// The capabilities member that declares Events, in `spelling`.
pub fn declare(caps: &mut Value, spelling: Spelling) {
    match spelling {
        Spelling::Extension => {
            caps["extensions"] = json!({"io.modelcontextprotocol/events": {"listChanged": true}});
        }
        Spelling::TopLevel => caps["events"] = json!({"listChanged": true}),
        Spelling::None => {}
    }
}

fn descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "mock.tick",
            "description": "an occurrence the mock.event control tool appends",
            "delivery": ["poll"],
            "inputSchema": {"type": "object", "additionalProperties": false,
                            "properties": {"topic": {"type": "string"}}},
            "payloadSchema": {"type": "object", "required": ["n"],
                              "properties": {"n": {"type": "integer"}, "topic": {"type": "string"}}}
        }),
        json!({
            "name": "mock.webhook_only",
            "description": "a type delivered by webhook only",
            "delivery": ["webhook"],
            "inputSchema": {"type": "object"},
            "payloadSchema": {"type": "object"}
        }),
    ]
}

/// The error object of a refusal.
fn refusal(code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({"code": code, "message": message});
    if let Some(d) = data {
        e["data"] = d;
    }
    e
}

/// Answer one `events/*` request: `Ok(result)` or `Err(error object)`.
pub fn handle(events: &Events, method: &str, params: &Value) -> Result<Value, Value> {
    match method {
        "events/list" => {
            let cursor = params.get("cursor").and_then(Value::as_str);
            eprintln!("MOCK_EVENTS_LIST {}", cursor.unwrap_or("-"));
            // One type per page: the client must follow `nextCursor`.
            let all = descriptors();
            let at: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
            let mut page = json!({"resultType": "complete", "events": all.get(at).into_iter().collect::<Vec<_>>()});
            if at + 1 < all.len() {
                page["nextCursor"] = json!((at + 1).to_string());
            }
            Ok(page)
        }
        "events/poll" => {
            eprintln!("MOCK_EVENTS_POLL {params}");
            poll(events, params)
        }
        _ => Err(refusal(-32601, "Method not found", None)),
    }
}

fn poll(events: &Events, params: &Value) -> Result<Value, Value> {
    let mut g = events.inner.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = g.fail.pop_front() {
        return Err(e);
    }
    let name = params["name"].as_str().unwrap_or_default();
    match name {
        "mock.tick" => {}
        "mock.webhook_only" => {
            return Err(refusal(
                UNSUPPORTED,
                "Unsupported",
                Some(json!({"feature": "deliveryMode", "value": "poll"})),
            ));
        }
        _ => {
            return Err(refusal(
                NOT_FOUND,
                "Not found",
                Some(json!({"kind": "event"})),
            ));
        }
    }
    let args = params["arguments"].as_object().cloned().unwrap_or_default();
    if args.keys().any(|k| k != "topic") || args.get("topic").is_some_and(|t| !t.is_string()) {
        return Err(refusal(
            INVALID_PARAMS,
            "arguments do not match inputSchema",
            None,
        ));
    }
    let topic = args
        .get("topic")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();
    if topic == "forbidden" {
        return Err(refusal(FORBIDDEN, "Forbidden", None));
    }
    if g.repeat
        && let Some(last) = g.last.clone()
    {
        g.repeat = false;
        return Ok(last);
    }
    let head = g.head.get(&topic).copied().unwrap_or(0);
    let Some(cursor) = params.get("cursor").and_then(Value::as_str) else {
        // `null` is "start from now": nothing, and the position to come back
        // from [SEP L297].
        return Ok(
            json!({"resultType": "complete", "events": [], "cursor": format!("c{head}"),
                         "truncated": false, "hasMore": g.always_more, "nextPollMs": 100}),
        );
    };
    let from: u64 = cursor
        .strip_prefix('c')
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let log = g.log.get(&topic).cloned().unwrap_or_default();
    // Everything up to the oldest kept occurrence's predecessor was trimmed.
    let floor = log.first().map_or(head, |(s, _, _)| s - 1);
    let (start, truncated) = if from < floor {
        (floor, true)
    } else {
        (from, false)
    };
    let max = params["maxEvents"].as_u64().unwrap_or(50) as usize;
    let after: Vec<&(u64, String, Value)> = log.iter().filter(|(s, _, _)| *s > start).collect();
    let page: Vec<&(u64, String, Value)> = after.iter().take(max).copied().collect();
    let next = page.last().map_or(start, |(s, _, _)| *s);
    let events: Vec<Value> = page.iter().map(|(_, _, o)| o.clone()).collect();
    let out = json!({
        "resultType": "complete",
        "events": events,
        "cursor": format!("c{next}"),
        "truncated": truncated,
        "hasMore": g.always_more || after.len() > page.len(),
        "nextPollMs": 100,
    });
    if !page.is_empty() {
        g.last = Some(out.clone());
    }
    Ok(out)
}

/// One control tool, by name; `None` when `name` is not one of them.
pub fn control(events: &Events, name: &str, args: &Value) -> Option<Value> {
    let mut g = events.inner.lock().unwrap_or_else(|e| e.into_inner());
    let topic = args["topic"].as_str().unwrap_or("default").to_string();
    match name {
        "mock.event" => {
            let count = args["count"].as_u64().unwrap_or(1);
            let mut appended = Vec::new();
            for _ in 0..count {
                let seq = g.head.get(&topic).copied().unwrap_or(0) + 1;
                g.head.insert(topic.clone(), seq);
                let id = args["id"]
                    .as_str()
                    .map_or_else(|| format!("evt-{topic}-{seq}"), str::to_string);
                let mut data = json!({"n": seq, "topic": topic});
                if args["bad_schema"] == true {
                    data["n"] = json!("not a number");
                }
                if args["oversize"] == true {
                    data["blob"] = json!("x".repeat(300 * 1024));
                }
                let mut o = Map::new();
                if args["malformed"] != true {
                    o.insert("eventId".into(), json!(id));
                }
                o.insert("name".into(), json!("mock.tick"));
                o.insert("timestamp".into(), json!("2026-10-10T00:00:00Z"));
                o.insert("data".into(), data);
                g.log
                    .entry(topic.clone())
                    .or_default()
                    .push((seq, id.clone(), Value::Object(o)));
                appended.push(id);
            }
            Some(json!({"appended": appended}))
        }
        // Retention passed the client by: everything appended so far is gone,
        // and one occurrence it never read with it.
        "mock.event_trim" => {
            g.log.remove(&topic);
            *g.head.entry(topic.clone()).or_default() += 1;
            Some(json!({"trimmed": topic}))
        }
        "mock.events_fail" => {
            let times = args["times"].as_u64().unwrap_or(1);
            let code = args["code"].as_i64().unwrap_or(-32603);
            for _ in 0..times {
                g.fail.push_back(refusal(
                    code,
                    args["message"].as_str().unwrap_or("injected"),
                    args.get("data").cloned(),
                ));
            }
            Some(json!({"armed": times}))
        }
        "mock.events_flags" => {
            if let Some(b) = args["always_more"].as_bool() {
                g.always_more = b;
            }
            if let Some(b) = args["repeat"].as_bool() {
                g.repeat = b;
            }
            Some(json!({"ok": true}))
        }
        _ => None,
    }
}
