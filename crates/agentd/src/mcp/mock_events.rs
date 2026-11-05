// SPDX-License-Identifier: AGPL-3.0-only
//! The built-in mock's MCP Events surface (SEP-3415), on its 2026-07-28
//! edge: two event types — `mock.tick`, pollable and streamed, with an
//! `inputSchema` and a `payloadSchema`, and `mock.webhook_only` — listed one
//! per page, and a cursor log per `topic` argument the control tools write
//! to.
//!
//! A third type, `md.instruction.activity`, is listed as instruction.md's
//! registry lists it: streamed and pollable, its `inputSchema` closed
//! (`additionalProperties: false`) over three optional properties —
//! `instructionId`, `orgId`, `types` — and an occurrence delivered where
//! `!has(arguments.instructionId) || arguments.instructionId ==
//! data.row.instruction_id`, as the registry matches. `mock.activity
//! {instruction_id, count?}` appends one; `mock.events_flags {activity_bare?}`
//! lists it without `instructionId`, so a request that sends one is refused
//! `InvalidParams`.
//!
//! Each request leaves a line on stderr, the server side's record of the
//! wire: `MOCK_EVENTS_LIST <cursor>`, `MOCK_EVENTS_POLL <params>` and
//! `MOCK_EVENTS_STREAM <params>`; a stream's end is `MOCK_EVENTS_STREAM_CLOSED
//! <client|dropped|restart>`.
//!
//! `events/stream` is served as MCPG 0.1.0 serves it (MCPG RFC 0031 §2.1,
//! §5.3): one SSE body of `notifications/events/*` frames tagged with the
//! request's id — `active {cursor, truncated}`, an `event` per occurrence
//! with the position after it, `heartbeat {cursor}` data frames (every
//! 250 ms here, `heartbeat_ms` to change it), and `active {truncated: true}`
//! again where the log was trimmed under it. Its refusals carry MCPG's
//! default numbering (`-32014` for a mode the type does not offer), not
//! SEP-3415's, which the poll path answers with.
//!
//! Control tools (`tools/call`, unlisted): `mock.event {topic, count, id?,
//! bad_schema?, oversize?, malformed?, traceparent?, tracestate?}` appends
//! occurrences, the last two as the occurrence's `_meta`;
//! `mock.events_changed {}` lists a third type, `mock.later`, and tells every
//! listen that asked for `eventsListChanged`; `mock.event_trim {topic}` drops everything appended so far and one more
//! occurrence the client never read, so its next poll is answered `truncated`; `mock.events_fail {code, data?,
//! times?}` answers the next polls with that error; `mock.events_flags
//! {always_more?, repeat?, ignore_max?}` makes every answer say `hasMore`, the
//! next answer repeat the last page that held occurrences, or every answer
//! carry all that is pending whatever `maxEvents` asks. Push's: `mock.events_flags
//! {no_push?, silent?, heartbeat_ms?}` lists `mock.tick` without `push`, holds
//! every open stream quiet (no frame, no heartbeat) while set, or paces its
//! heartbeats; `mock.stream_drop {}` cuts every open stream without a word;
//! `mock.stream_refuse {code, data?, times?}` answers the next stream opens
//! with that error.

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
    /// Answer every occurrence after the cursor, whatever `maxEvents` asks.
    ignore_max: bool,
    /// The last page answered, for `repeat`.
    last: Option<Value>,
    /// `mock.later` is listed (`mock.events_changed`).
    later: bool,
    /// `mock.tick` is listed without `push`.
    no_push: bool,
    /// Open streams send nothing at all.
    silent: bool,
    /// Between two heartbeats on a stream; 0 is the default.
    heartbeat_ms: u64,
    /// Bumped by `mock.stream_drop`: a stream opened before it is cut.
    drops: u64,
    /// Errors the next stream opens are answered with.
    refuse_stream: VecDeque<Value>,
    /// `md.instruction.activity` is listed without `instructionId`.
    activity_bare: bool,
}

/// The registry's activity type, and the log its occurrences are kept in.
const ACTIVITY: &str = "md.instruction.activity";
const ACTIVITY_LOG: &str = "@activity";

/// The mock's Events state, shared by every connection.
#[derive(Default)]
pub struct Events {
    inner: Mutex<Inner>,
}

const NOT_FOUND: i64 = -32023;
const FORBIDDEN: i64 = -32024;
const UNSUPPORTED: i64 = -32026;
const INVALID_PARAMS: i64 = -32602;
/// MCPG 0.1.0's default numbering of `Unsupported`, the push path's.
const UNSUPPORTED_DRAFT: i64 = -32014;
/// The heartbeat interval when `heartbeat_ms` names none.
const HEARTBEAT_MS: u64 = 250;

impl Events {
    /// `(head, floor)` of `topic`: its newest sequence, and the position
    /// below which retention trimmed.
    pub fn position(&self, topic: &str) -> (u64, u64) {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let head = g.head.get(topic).copied().unwrap_or(0);
        let floor = g
            .log
            .get(topic)
            .and_then(|l| l.first())
            .map_or(head, |(s, _, _)| s - 1);
        (head, floor)
    }

    /// The occurrence at `seq` of `topic`, if it is kept.
    pub fn occurrence(&self, topic: &str, seq: u64) -> Option<Value> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.log
            .get(topic)?
            .iter()
            .find(|(s, _, _)| *s == seq)
            .map(|(_, _, o)| o.clone())
    }
}

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

/// `md.instruction.activity` as instruction.md lists it; `bare`, without
/// `instructionId`.
fn activity_descriptor(push: bool, bare: bool) -> Value {
    let mut properties = json!({
        "instructionId": {"type": "string"},
        "orgId": {"type": "string"},
        "types": {"type": "array", "items": {"type": "string",
                  "enum": ["published", "ref_moved", "binding_reported"]}},
    });
    if bare {
        properties
            .as_object_mut()
            .expect("an object")
            .remove("instructionId");
    }
    json!({
        "name": ACTIVITY,
        "description": "what happened to an instruction on the registry",
        "delivery": if push { json!(["push", "poll"]) } else { json!(["poll"]) },
        "inputSchema": {"type": "object", "additionalProperties": false, "properties": properties},
        "payloadSchema": {"type": "object"}
    })
}

/// The instruction an `md.instruction.activity` request filters on, or why
/// its arguments do not fit the type as listed.
fn activity_filter(args: &Map<String, Value>, bare: bool) -> Result<Option<String>, Value> {
    let fits = args.iter().all(|(k, v)| match k.as_str() {
        "instructionId" => !bare && v.is_string(),
        "orgId" => v.is_string(),
        "types" => v.is_array(),
        _ => false,
    });
    if !fits {
        return Err(refusal(
            INVALID_PARAMS,
            "arguments do not match inputSchema",
            None,
        ));
    }
    Ok(args
        .get("instructionId")
        .and_then(Value::as_str)
        .map(str::to_string))
}

/// Whether an occurrence passes the request's filter, as the registry
/// matches it.
fn passes(filter: Option<&str>, occurrence: &Value) -> bool {
    filter.is_none_or(|id| occurrence["data"]["row"]["instruction_id"] == id)
}

fn descriptors(later: bool, push: bool, activity_bare: bool) -> Vec<Value> {
    let tick_modes = if push {
        json!(["push", "poll", "webhook"])
    } else {
        json!(["poll", "webhook"])
    };
    let mut all = vec![
        json!({
            "name": "mock.tick",
            "description": "an occurrence the mock.event control tool appends",
            "delivery": tick_modes,
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
    ];
    all.push(activity_descriptor(push, activity_bare));
    if later {
        all.push(json!({
            "name": "mock.later",
            "description": "a type listed once mock.events_changed is called",
            "delivery": ["poll"],
            "inputSchema": {"type": "object"},
            "payloadSchema": {"type": "object"}
        }));
    }
    all
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
            let (later, push, bare) = {
                let g = events.inner.lock().unwrap_or_else(|e| e.into_inner());
                (g.later, !g.no_push, g.activity_bare)
            };
            let all = descriptors(later, push, bare);
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

/// The log a request reads and the instruction it filters on, from its
/// arguments, checked against the type's `inputSchema` as listed.
fn topic_of(
    name: &str,
    args: &Map<String, Value>,
    activity_bare: bool,
) -> Result<(String, Option<String>), Value> {
    if name == ACTIVITY {
        return Ok((
            ACTIVITY_LOG.to_string(),
            activity_filter(args, activity_bare)?,
        ));
    }
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
    Ok((topic, None))
}

fn poll(events: &Events, params: &Value) -> Result<Value, Value> {
    let mut g = events.inner.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = g.fail.pop_front() {
        return Err(e);
    }
    let name = params["name"].as_str().unwrap_or_default();
    match name {
        "mock.tick" | "mock.later" | ACTIVITY => {}
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
    let (topic, filter) = topic_of(name, &args, g.activity_bare)?;
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
    let max = if g.ignore_max {
        usize::MAX
    } else {
        params["maxEvents"].as_u64().unwrap_or(50) as usize
    };
    let after: Vec<&(u64, String, Value)> = log
        .iter()
        .filter(|(s, _, o)| *s > start && passes(filter.as_deref(), o))
        .collect();
    let page: Vec<&(u64, String, Value)> = after.iter().take(max).copied().collect();
    // Past what the filter passed over too, once the page holds the rest.
    let next = if after.len() > page.len() {
        page.last().map_or(start, |(s, _, _)| *s)
    } else {
        log.last().map_or(start, |(s, _, _)| *s).max(start)
    };
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

/// Where an `events/stream` starts.
pub struct StreamStart {
    pub topic: String,
    /// The position it reads after.
    pub after: u64,
    pub truncated: bool,
    /// The `mock.stream_drop` count it opened under.
    pub drops: u64,
    /// `md.instruction.activity`: the instruction it delivers for, if it
    /// names one.
    pub filter: Option<String>,
}

/// Open one `events/stream`: checked as a poll is, then from the cursor (or
/// from now), moved up to the floor — `truncated` — when the log was trimmed
/// past it. `Err(error object)` refuses it.
pub fn stream_open(events: &Events, params: &Value) -> Result<StreamStart, Value> {
    let mut g = events.inner.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = g.refuse_stream.pop_front() {
        return Err(e);
    }
    let name = params["name"].as_str().unwrap_or_default();
    match name {
        "mock.tick" | ACTIVITY if !g.no_push => {}
        "mock.tick" | ACTIVITY | "mock.webhook_only" | "mock.later" => {
            return Err(refusal(
                UNSUPPORTED_DRAFT,
                "Unsupported",
                Some(json!({"feature": "deliveryMode", "value": "push"})),
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
    let (topic, filter) = topic_of(name, &args, g.activity_bare)?;
    let head = g.head.get(&topic).copied().unwrap_or(0);
    let floor = g
        .log
        .get(&topic)
        .and_then(|l| l.first())
        .map_or(head, |(s, _, _)| s - 1);
    let (after, truncated) = match params.get("cursor").and_then(Value::as_str) {
        None => (head, false),
        Some(c) => {
            let from: u64 = c
                .strip_prefix('c')
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if from < floor {
                (floor, true)
            } else {
                (from, false)
            }
        }
    };
    let drops = g.drops;
    Ok(StreamStart {
        topic,
        after,
        truncated,
        drops,
        filter,
    })
}

/// What an open stream sends next, after position `after`: `Gap(floor)` when
/// the log was trimmed past it, then each occurrence kept. `Cut` once
/// `mock.stream_drop` was called since it opened; nothing while `silent`.
pub enum StreamStep {
    Cut,
    Quiet,
    Send {
        gap: Option<u64>,
        occurrences: Vec<(u64, Value)>,
        heartbeat_ms: u64,
    },
}

pub fn stream_step(events: &Events, start: &StreamStart, after: u64) -> StreamStep {
    let g = events.inner.lock().unwrap_or_else(|e| e.into_inner());
    if g.drops != start.drops {
        return StreamStep::Cut;
    }
    if g.silent {
        return StreamStep::Quiet;
    }
    let head = g.head.get(&start.topic).copied().unwrap_or(0);
    let log = g.log.get(&start.topic).cloned().unwrap_or_default();
    let floor = log.first().map_or(head, |(s, _, _)| s - 1);
    let gap = (after < floor).then_some(floor);
    let from = gap.unwrap_or(after);
    StreamStep::Send {
        gap,
        occurrences: log
            .into_iter()
            .filter(|(s, _, o)| *s > from && passes(start.filter.as_deref(), o))
            .map(|(s, _, o)| (s, o))
            .collect(),
        heartbeat_ms: if g.heartbeat_ms == 0 {
            HEARTBEAT_MS
        } else {
            g.heartbeat_ms
        },
    }
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
                // The trace the server produced the occurrence in, where the
                // SEP-414 keys go.
                let mut meta = Map::new();
                for key in ["traceparent", "tracestate"] {
                    if let Some(v) = args.get(key).filter(|v| v.is_string()) {
                        meta.insert(key.into(), v.clone());
                    }
                }
                if !meta.is_empty() {
                    o.insert("_meta".into(), Value::Object(meta));
                }
                g.log
                    .entry(topic.clone())
                    .or_default()
                    .push((seq, id.clone(), Value::Object(o)));
                appended.push(id);
            }
            Some(json!({"appended": appended}))
        }
        // The registry's activity for one instruction.
        "mock.activity" => {
            let instruction = args["instruction_id"].as_str().unwrap_or("ins_mock");
            let mut appended = Vec::new();
            for _ in 0..args["count"].as_u64().unwrap_or(1) {
                let seq = g.head.get(ACTIVITY_LOG).copied().unwrap_or(0) + 1;
                g.head.insert(ACTIVITY_LOG.to_string(), seq);
                let id = format!("act-{seq}");
                let o = json!({"eventId": id, "name": ACTIVITY,
                               "timestamp": "2026-10-10T00:00:00Z",
                               "data": {"n": seq, "row": {"instruction_id": instruction,
                                                          "type": "published"}}});
                g.log
                    .entry(ACTIVITY_LOG.to_string())
                    .or_default()
                    .push((seq, id.clone(), o));
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
        "mock.events_changed" => {
            g.later = true;
            Some(json!({"later": true}))
        }
        "mock.events_flags" => {
            if let Some(b) = args["always_more"].as_bool() {
                g.always_more = b;
            }
            if let Some(b) = args["repeat"].as_bool() {
                g.repeat = b;
            }
            if let Some(b) = args["ignore_max"].as_bool() {
                g.ignore_max = b;
            }
            if let Some(b) = args["no_push"].as_bool() {
                g.no_push = b;
            }
            if let Some(b) = args["silent"].as_bool() {
                g.silent = b;
            }
            if let Some(n) = args["heartbeat_ms"].as_u64() {
                g.heartbeat_ms = n;
            }
            if let Some(b) = args["activity_bare"].as_bool() {
                g.activity_bare = b;
            }
            Some(json!({"ok": true}))
        }
        "mock.stream_drop" => {
            g.drops += 1;
            Some(json!({"dropped": g.drops}))
        }
        "mock.stream_refuse" => {
            let times = args["times"].as_u64().unwrap_or(1);
            let code = args["code"].as_i64().unwrap_or(-32603);
            for _ in 0..times {
                g.refuse_stream.push_back(refusal(
                    code,
                    args["message"].as_str().unwrap_or("injected"),
                    args.get("data").cloned(),
                ));
            }
            Some(json!({"armed": times}))
        }
        _ => None,
    }
}
