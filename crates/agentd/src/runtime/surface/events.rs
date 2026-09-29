// SPDX-License-Identifier: AGPL-3.0-only
//! The observation feed's vocabulary: every kind an event may carry, who may
//! see it, the shape of its `data`, and the frames and params of
//! `agentd.events/SubscribeToEvents` — the events extension's schema bundle.
//!
//! One source for all of it. The feed's push asserts each event against this
//! in debug builds, the extended card lists the kinds from it, the published
//! schema is built from it, and a source scan holds every push in the tree to
//! [`FeedKind::ALL`] — so a kind cannot be pushed that a client was never
//! told about, and a shape cannot change without the schema changing with it.
//!
//! Each `data` schema is written from the code that builds the payload
//! (`Run::summary`, `Contexts::status`, `Activity::to_value`, the status
//! document, the step and lifecycle pushes, the audit mirror), not from what
//! a client happens to read, because a client reading less than is sent is
//! fine and a schema promising less than is sent is not.

use serde_json::{Value, json};

use super::ext::{EVENTS_EXTENSION, EVENTS_METHOD, schema_of};
use crate::config::settings::DeviceScope;

/// Who an event of a kind may reach — the feed's visibility tag says which
/// principal exactly; this says which tags a kind may carry at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Every named subscriber: facts about the instance anyone attached may
    /// act on (it is draining, a setting moved).
    All,
    /// The principal whose work it is, and operators. An item with no owner
    /// is the operator's alone.
    Owner,
    /// Operators only: the instance's internals and its security events.
    Operator,
}

/// One kind of feed event. The closed vocabulary the events extension
/// publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedKind {
    Task,
    TaskRemoved,
    Run,
    RunRemoved,
    Step,
    Conversation,
    ConversationRemoved,
    Subagent,
    SubagentRemoved,
    Child,
    ChildRemoved,
    Activity,
    ActivityRemoved,
    Status,
    Lifecycle,
    Config,
    Audit,
    Auth,
}

impl FeedKind {
    /// Every kind, in the order the published tables list them.
    pub const ALL: &[FeedKind] = &[
        FeedKind::Task,
        FeedKind::TaskRemoved,
        FeedKind::Run,
        FeedKind::RunRemoved,
        FeedKind::Step,
        FeedKind::Conversation,
        FeedKind::ConversationRemoved,
        FeedKind::Subagent,
        FeedKind::SubagentRemoved,
        FeedKind::Child,
        FeedKind::ChildRemoved,
        FeedKind::Activity,
        FeedKind::ActivityRemoved,
        FeedKind::Status,
        FeedKind::Lifecycle,
        FeedKind::Config,
        FeedKind::Audit,
        FeedKind::Auth,
    ];

    /// The kind's name on the wire. One exhaustive match, so a variant cannot
    /// be added without one.
    pub fn as_str(self) -> &'static str {
        match self {
            FeedKind::Task => "task",
            FeedKind::TaskRemoved => "task.removed",
            FeedKind::Run => "run",
            FeedKind::RunRemoved => "run.removed",
            FeedKind::Step => "step",
            FeedKind::Conversation => "conversation",
            FeedKind::ConversationRemoved => "conversation.removed",
            FeedKind::Subagent => "subagent",
            FeedKind::SubagentRemoved => "subagent.removed",
            FeedKind::Child => "child",
            FeedKind::ChildRemoved => "child.removed",
            FeedKind::Activity => "activity",
            FeedKind::ActivityRemoved => "activity.removed",
            FeedKind::Status => "status",
            FeedKind::Lifecycle => "lifecycle",
            FeedKind::Config => "config",
            FeedKind::Audit => "audit",
            FeedKind::Auth => "auth",
        }
    }

    /// The kind `name` names, matched exactly.
    pub fn of(name: &str) -> Option<FeedKind> {
        FeedKind::ALL.iter().copied().find(|k| k.as_str() == name)
    }

    /// Who an event of this kind may reach. Written from the tags the call
    /// sites push with: a run or a task is its owner's and so is its departure
    /// — an owner shown a task must be told when retention drops it; a step
    /// and a subagent describe the instance's internals.
    pub fn audience(self) -> Audience {
        match self {
            FeedKind::Lifecycle | FeedKind::Config => Audience::All,
            FeedKind::Task
            | FeedKind::TaskRemoved
            | FeedKind::Run
            | FeedKind::RunRemoved
            | FeedKind::Conversation
            | FeedKind::ConversationRemoved
            | FeedKind::Activity
            | FeedKind::ActivityRemoved => Audience::Owner,
            FeedKind::Step
            | FeedKind::Subagent
            | FeedKind::SubagentRemoved
            | FeedKind::Child
            | FeedKind::ChildRemoved
            | FeedKind::Status
            | FeedKind::Audit
            | FeedKind::Auth => Audience::Operator,
        }
    }

    /// The schema of an event's `data`. Self-contained — no `$ref` — so a
    /// push is checked against it alone and the bundle can place it anywhere.
    pub fn data_schema(self) -> Value {
        match self {
            // The spec's Task, nothing beside it: what it is linked to and
            // who owns it are its annotations, stripped per subscriber.
            FeedKind::Task => closed(&["task"], json!({"task": task_schema()})),
            // A departure names what left and nothing else: its owner already
            // holds the rest, and anyone else must not learn it.
            FeedKind::TaskRemoved
            | FeedKind::RunRemoved
            | FeedKind::ConversationRemoved
            | FeedKind::SubagentRemoved
            | FeedKind::ChildRemoved
            | FeedKind::ActivityRemoved => closed(&["id"], json!({"id": string()})),
            // `Run::summary`.
            FeedKind::Run => closed(
                &[
                    "id",
                    "workflow",
                    "status",
                    "start",
                    "steps",
                    "tokens",
                    "created",
                    "updated",
                    "finished",
                    "output",
                    "error",
                    "task",
                    "principal",
                ],
                json!({
                    "id": string(),
                    "workflow": string(),
                    "status": string(),
                    "start": string(),
                    "steps": {"type": "object", "additionalProperties": count()},
                    "tokens": count(),
                    "created": count(),
                    "updated": count(),
                    "finished": count_or_null(),
                    "output": true,
                    "error": string_or_null(),
                    "task": string_or_null(),
                    "principal": string_or_null(),
                }),
            ),
            // A step starting, then the same step done — two pushes in
            // `steps.rs` with different fields, told apart by `phase`.
            FeedKind::Step => json!({
                "oneOf": [
                    closed(
                        &["run", "step", "kind", "phase", "attempt"],
                        json!({
                            "run": string(),
                            "step": string(),
                            "kind": string(),
                            "phase": {"const": "start"},
                            "attempt": count(),
                        }),
                    ),
                    closed(
                        &["run", "step", "phase", "status", "attempt", "tokens", "err"],
                        json!({
                            "run": string(),
                            "step": string(),
                            "phase": {"const": "done"},
                            "status": string(),
                            "attempt": count(),
                            "tokens": count(),
                            "err": string_or_null(),
                        }),
                    ),
                ],
            }),
            // `Contexts::status`: `id` is the key it is kept under, `contextId`
            // the name its owner addresses it by.
            FeedKind::Conversation => closed(
                &[
                    "id",
                    "contextId",
                    "kind",
                    "version",
                    "messages",
                    "est_tokens",
                    "turns",
                    "principal",
                    "skills",
                    "plan",
                    "updated",
                ],
                json!({
                    "id": string(),
                    "contextId": string(),
                    "kind": string(),
                    "version": count(),
                    "messages": count(),
                    "est_tokens": count(),
                    "turns": count(),
                    "principal": string_or_null(),
                    "skills": {"type": "array", "items": string()},
                    "plan": string_or_null(),
                    "updated": count(),
                }),
            ),
            FeedKind::Subagent => closed(
                &["handle", "mode", "status", "tokens", "error", "updated"],
                json!({
                    "handle": string(),
                    "mode": string(),
                    "status": string(),
                    "tokens": count(),
                    "error": string_or_null(),
                    "updated": count(),
                }),
            ),
            // `Children::status`: one OS child.
            FeedKind::Child => closed(
                &["node", "pid", "kind", "age_ms", "tokens", "cancelled"],
                json!({
                    "node": count(),
                    "pid": {"type": "integer"},
                    "kind": string(),
                    "age_ms": count(),
                    "tokens": count(),
                    "cancelled": {"type": "boolean"},
                }),
            ),
            // `Activity::to_value` with the unit's id beside it.
            FeedKind::Activity => closed(
                &[
                    "id",
                    "task",
                    "ctx",
                    "phase",
                    "tool",
                    "round",
                    "tokens_in",
                    "tokens_out",
                    "started_ms",
                    "updated_ms",
                ],
                json!({
                    "id": string(),
                    "task": string_or_null(),
                    "ctx": string_or_null(),
                    "phase": string(),
                    "tool": string_or_null(),
                    "round": count(),
                    "tokens_in": count(),
                    "tokens_out": count(),
                    "started_ms": count(),
                    "updated_ms": count(),
                }),
            ),
            // The slim status the section diff pushes. The nested documents
            // (counters, budget, values) are the `status` op's own and are
            // described there; here they are objects.
            FeedKind::Status => closed(
                &[
                    "instance",
                    "model",
                    "version",
                    "draining",
                    "inbox_pending",
                    "counters",
                    "budget",
                    "store",
                    "values",
                    "skill_prefix",
                ],
                json!({
                    "instance": string(),
                    "model": string(),
                    "version": string(),
                    "draining": {"type": "boolean"},
                    "inbox_pending": count(),
                    "counters": {"type": "object"},
                    "budget": {"type": "object"},
                    "store": closed(
                        &["kind", "degraded"],
                        json!({"kind": string(), "degraded": {"type": "boolean"}}),
                    ),
                    "values": {"type": "object"},
                    "skill_prefix": string(),
                }),
            ),
            // The instance draining, or paused and resumed as a whole.
            FeedKind::Lifecycle => json!({
                "oneOf": [
                    closed(
                        &["draining", "reason"],
                        json!({"draining": {"const": true}, "reason": string()}),
                    ),
                    closed(
                        &["paused", "reason"],
                        json!({"paused": {"const": true}, "reason": string()}),
                    ),
                    closed(&["paused"], json!({"paused": {"const": false}})),
                ],
            }),
            // What moved, and whether a reload or an `admin.set` moved it.
            FeedKind::Config => closed(
                &["paths", "source"],
                json!({
                    "paths": {"type": "array", "items": string()},
                    "source": {"enum": ["reload", "admin.set"]},
                }),
            ),
            // The audit mirror: the audit record's own fields, and the sid
            // when the caller signed in with a session.
            FeedKind::Audit => closed(
                &["ts", "principal", "role", "action", "target", "outcome"],
                json!({
                    "ts": count(),
                    "principal": string_or_null(),
                    "role": string_or_null(),
                    "action": string(),
                    "target": true,
                    "outcome": string(),
                    "sid": string(),
                }),
            ),
            // From the FeedKind contract rather than from the call sites: the
            // launch grant pushes `launch` from a module no survey of today's
            // pushes can see. `client_id` and `scope` on every one, so an
            // operator's client can always say which device and what reach.
            FeedKind::Auth => {
                let mut s = closed(
                    &["event", "client_id", "scope"],
                    json!({
                        "event": {"enum": AUTH_EVENTS},
                        "client_id": string(),
                        "scope": {"enum": DeviceScope::ALL.iter().map(|s| s.as_str()).collect::<Vec<_>>()},
                        "user_code": string(),
                        "peer": string(),
                        "sid": string(),
                        "name": string(),
                    }),
                );
                s["allOf"] = json!([
                    // A sign-in waiting on, granted or refused by an operator
                    // is the code the operator reads off the device.
                    when_event(
                        &["pending", "approved", "denied"],
                        json!({"required": ["user_code"]})
                    ),
                    // A session ending names the session.
                    when_event(&["revoked"], json!({"required": ["sid"]})),
                    // A launch mints an operator session and names it.
                    when_event(
                        &["launch"],
                        json!({"required": ["sid"], "properties": {"scope": {"const": "operator"}}}),
                    ),
                ]);
                s
            }
        }
    }
}

/// The `event` values an `auth` event may carry.
pub const AUTH_EVENTS: &[&str] = &["pending", "approved", "denied", "revoked", "launch"];

/// Whether an event of kind `kind` carrying `data` is one the events extension
/// allows: `Err` names the kind, or each way the data misses its schema. What
/// the feed's push asserts in debug builds.
pub fn check_event(kind: &str, data: &Value) -> Result<FeedKind, String> {
    let k = FeedKind::of(kind).ok_or_else(|| format!("{kind:?} is not a FeedKind"))?;
    crate::jsonschema::validate(&k.data_schema(), data)
        .map(|()| k)
        .map_err(|errs| format!("{kind} data {data}: {}", crate::jsonschema::explain(&errs)))
}

/// The kinds a subscriber may receive: an operator every kind (`audit` only
/// while introspection is on), anyone else the kinds that reach everyone and
/// those that reach an owner. What the extended card lists.
pub fn kinds_for(is_operator: bool, introspection: bool) -> Vec<&'static str> {
    FeedKind::ALL
        .iter()
        .filter(|k| is_operator || k.audience() != Audience::Operator)
        .filter(|k| **k != FeedKind::Audit || introspection)
        .map(|k| k.as_str())
        .collect()
}

/// `SubscribeToEvents`'s params: at most a cursor, and nothing else. Strict,
/// so a member the method does not define is refused rather than ignored —
/// a client that misspells the cursor is told, not silently replayed from
/// the start.
pub fn params_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"fromSeq": {"type": "integer", "minimum": 0}},
        "additionalProperties": false,
    })
}

/// The events extension's schema bundle, published next to the extension's URI: a
/// frame's `result` at the root, the method's params and each kind's data
/// under `$defs`.
pub fn schema_bundle() -> Value {
    let kinds: serde_json::Map<String, Value> = FeedKind::ALL
        .iter()
        .map(|k| (k.as_str().to_string(), k.data_schema()))
        .collect();
    // An event's data is its kind's: one `if kind = K then data: K's schema`
    // per kind, so the bundle alone validates a whole frame.
    let per_kind: Vec<Value> = FeedKind::ALL
        .iter()
        .map(|k| {
            json!({
                "if": {"properties": {"kind": {"const": k.as_str()}}},
                "then": {"properties": {"data": {"$ref": format!("#/$defs/kinds/$defs/{}", k.as_str())}}},
            })
        })
        .collect();
    let seq = json!({"type": "integer", "minimum": 0});
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": schema_of(EVENTS_EXTENSION),
        "title": "agentd events",
        "description": format!(
            "The result of each {EVENTS_METHOD} frame: exactly one of hello, event or goodbye. \
             $defs/params is the method's params; $defs/kinds/$defs/<kind> is each kind's data."
        ),
        "$ref": "#/$defs/frame",
        "$defs": {
            "params": params_schema(),
            "frame": {
                "oneOf": [
                    closed(&["hello"], json!({"hello": {"$ref": "#/$defs/hello"}})),
                    closed(&["event"], json!({"event": {"$ref": "#/$defs/event"}})),
                    closed(&["goodbye"], json!({"goodbye": {"$ref": "#/$defs/goodbye"}})),
                ],
            },
            "hello": closed(
                &["seq", "resume", "resync", "introspection", "version"],
                json!({
                    "seq": seq,
                    "resume": seq,
                    "resync": {"type": "boolean"},
                    "introspection": {"type": "boolean"},
                    "version": {"type": "string", "description": "the agentd build, not a protocol version"},
                }),
            ),
            "event": {
                "type": "object",
                "required": ["seq", "ts", "kind", "data"],
                "properties": {
                    "seq": seq,
                    "ts": {"type": "integer", "minimum": 0, "description": "epoch milliseconds"},
                    "kind": {"enum": FeedKind::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>()},
                    "data": {"type": "object"},
                },
                "additionalProperties": false,
                "allOf": per_kind,
            },
            "goodbye": closed(
                &["seq", "reason"],
                json!({"seq": seq, "reason": {"enum": ["deadline", "revoked"]}}),
            ),
            "kinds": {"$defs": kinds},
        },
    })
}

/// An object with exactly `props`, of which `required` must be present.
fn closed(required: &[&str], props: Value) -> Value {
    json!({
        "type": "object",
        "required": required,
        "properties": props,
        "additionalProperties": false,
    })
}

/// `then` for `auth` events whose `event` is one of `events`.
fn when_event(events: &[&str], then: Value) -> Value {
    json!({
        "if": {"properties": {"event": {"enum": events}}},
        "then": then,
    })
}

fn string() -> Value {
    json!({"type": "string"})
}

fn string_or_null() -> Value {
    json!({"type": ["string", "null"]})
}

fn count() -> Value {
    json!({"type": "integer", "minimum": 0})
}

fn count_or_null() -> Value {
    json!({"type": ["integer", "null"], "minimum": 0})
}

/// The A2A 1.0 `Task` as the feed carries it. Only what a client keys on is
/// pinned; the object is the specification's, and its optional members
/// (`artifacts`, `history`, `metadata`) are the specification's to define.
fn task_schema() -> Value {
    json!({
        "type": "object",
        "required": ["id", "contextId", "status"],
        "properties": {
            "id": string(),
            "contextId": string(),
            "status": {
                "type": "object",
                "required": ["state"],
                "properties": {"state": {"type": "string", "pattern": "^TASK_STATE_"}},
            },
            "history": {"type": "array", "items": {"type": "object"}},
            "artifacts": {"type": "array", "items": {"type": "object"}},
            "metadata": {"type": "object"},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vocabulary is closed and every schema is complete: `ALL` holds
    /// every variant once, `as_str` and `of` round-trip, each data schema is a well-formed closed object schema, and the
    /// bundle names every kind in its enum and its `$defs`.
    #[test]
    fn feed_kinds_are_closed_and_schemas_complete() {
        let names: Vec<&str> = FeedKind::ALL.iter().map(|k| k.as_str()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "a kind twice: {names:?}");
        for k in FeedKind::ALL {
            assert_eq!(FeedKind::of(k.as_str()), Some(*k));
            let s = k.data_schema();
            crate::jsonschema::check_schema(&s).unwrap_or_else(|e| panic!("{k:?}: {e:?}"));
            assert!(
                !s.to_string().contains("$ref"),
                "{k:?} is not self-contained"
            );
            // Every alternative is closed: a field the code adds without the
            // schema is caught, not waved through.
            let alternatives: Vec<&Value> = match s.get("oneOf").and_then(Value::as_array) {
                Some(a) => a.iter().collect(),
                None => vec![&s],
            };
            for alt in alternatives {
                assert_eq!(alt["type"], "object", "{k:?}: {alt}");
                assert_eq!(alt["additionalProperties"], false, "{k:?}: {alt}");
            }
        }
        // Exhaustiveness in the other direction: a variant missing from ALL
        // would still have a name, and nothing else would notice.
        assert_eq!(FeedKind::ALL.len(), 18);

        let bundle = schema_bundle();
        crate::jsonschema::check_schema(&bundle).unwrap();
        assert_eq!(bundle["$id"], format!("{EVENTS_EXTENSION}/schema.json"));
        let listed: Vec<&str> = bundle["$defs"]["event"]["properties"]["kind"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(listed, names);
        for k in FeedKind::ALL {
            assert_eq!(
                bundle["$defs"]["kinds"]["$defs"][k.as_str()],
                k.data_schema()
            );
        }
        // Who sees what, from one table.
        assert_eq!(kinds_for(true, true), names);
        assert!(!kinds_for(true, false).contains(&"audit"));
        let user = kinds_for(false, true);
        for k in ["audit", "auth", "status", "step", "subagent", "child"] {
            assert!(!user.contains(&k), "{k} reaches a non-operator");
        }
        for k in [
            "task",
            "task.removed",
            "run",
            "conversation",
            "activity",
            "lifecycle",
            "config",
        ] {
            assert!(user.contains(&k), "{k} never reaches its owner");
        }
    }

    /// One real payload per kind — built by the code that builds it where
    /// that code can run without a daemon, else as a daemon pushed it — and,
    /// for `auth`, one per event value including the launch push: each
    /// passes its kind's schema, and the bundle accepts each as a frame.
    ///
    /// A schema that refuses a payload the daemon really pushes would trip
    /// the push assertion and bring a debug daemon down; this is where that
    /// shows first.
    #[test]
    fn a_captured_sample_of_every_kind_validates() {
        let samples = samples();
        for k in FeedKind::ALL {
            assert!(
                samples.iter().any(|(kind, _)| kind == k),
                "no sample of {k:?}"
            );
        }
        for (i, (kind, data)) in samples.iter().enumerate() {
            check_event(kind.as_str(), data).unwrap_or_else(|e| panic!("{e}"));
            let frame = json!({"event": {"seq": i + 1, "ts": 1_790_631_625_852u64,
                                          "kind": kind.as_str(), "data": data}});
            crate::jsonschema::validate(&schema_bundle(), &frame)
                .unwrap_or_else(|e| panic!("{kind:?} as a frame: {e:?}"));
        }
        let auth: Vec<&str> = samples
            .iter()
            .filter(|(k, _)| *k == FeedKind::Auth)
            .filter_map(|(_, d)| d["event"].as_str())
            .collect();
        for e in AUTH_EVENTS {
            assert!(auth.contains(e), "no `auth` sample with event {e}");
        }
        // The frames around the events.
        let bundle = schema_bundle();
        for frame in [
            json!({"hello": {"seq": 4, "resume": 0, "resync": false,
                             "introspection": true, "version": crate::VERSION}}),
            json!({"goodbye": {"seq": 9, "reason": "revoked"}}),
        ] {
            crate::jsonschema::validate(&bundle, &frame).unwrap_or_else(|e| panic!("{e:?}"));
        }
    }

    /// What the schemas refuse: an unknown kind, a field the
    /// schema does not name, an `auth` event missing what the contract
    /// requires — scope included, which the launch push must carry — and a
    /// launch that is not an operator's.
    #[test]
    fn a_payload_off_the_schema_is_refused() {
        assert!(check_event("no.such.kind", &json!({})).is_err());
        assert!(check_event("run.removed", &json!({"id": "r", "owner": "x"})).is_err());
        assert!(check_event("config", &json!({"paths": [], "source": "file"})).is_err());
        let launch = json!({"event": "launch", "sid": "ls_0123456789abcdef",
                            "client_id": "agentd-tui", "scope": "operator"});
        assert!(check_event("auth", &launch).is_ok());
        for drop in ["client_id", "scope", "sid"] {
            let mut v = launch.clone();
            v.as_object_mut().unwrap().remove(drop);
            assert!(check_event("auth", &v).is_err(), "launch without {drop}");
        }
        let mut user = launch.clone();
        user["scope"] = json!("user");
        assert!(check_event("auth", &user).is_err(), "a user launch");
        assert!(
            check_event(
                "auth",
                &json!({"event": "pending", "client_id": "c", "scope": "user"})
            )
            .is_err(),
            "a pending sign-in without its code"
        );
        assert!(
            check_event(
                "auth",
                &json!({"event": "no-such-event", "client_id": "c", "scope": "user"})
            )
            .is_err(),
            "an auth event the vocabulary does not name"
        );
        // The params: a cursor, and nothing else.
        let p = params_schema();
        assert!(crate::jsonschema::validate(&p, &json!({"fromSeq": 3})).is_ok());
        assert!(crate::jsonschema::validate(&p, &json!({})).is_ok());
        assert!(crate::jsonschema::validate(&p, &json!({"cursor": 3})).is_err());
        assert!(crate::jsonschema::validate(&p, &json!({"fromSeq": -1})).is_err());
    }

    /// The samples: real builders first.
    fn samples() -> Vec<(FeedKind, Value)> {
        use crate::runtime::activity::{Activity, Phase};
        let mut out = Vec::new();

        // `Run::summary` of a real run, fresh and finished.
        let wf = crate::engine::model::parse_workflow(&json!({
            "name": "greet",
            "steps": {"s": {"kind": "manual"}, "f": {"kind": "finish", "depends_on": ["s"], "output": "done"}},
        }))
        .expect("a workflow");
        let start = crate::engine::run::Start {
            node: "s".into(),
            payload: json!({}),
            ts: 0,
        };
        let mut run = crate::engine::run::RunState::new("greet-1", &wf, start, json!({}));
        out.push((FeedKind::Run, run.summary()));
        run.status = crate::engine::run::RunStatus::Completed;
        run.output = Some(json!("done"));
        run.finished = Some(1_790_631_626_178);
        run.task = Some("t-1".into());
        run.principal = Some("operator".into());
        out.push((FeedKind::Run, run.summary()));

        // `Contexts::status` of an owned conversation and an ownerless one.
        let mut contexts = crate::context::Contexts::new(128_000);
        contexts.conversation("c-1", Some("user:alice"));
        contexts.conversation("c-2", None);
        for c in contexts.status().as_array().unwrap() {
            out.push((FeedKind::Conversation, c.clone()));
        }

        // `Activity::to_value`, with the id the push adds, in each phase.
        for (phase, tool, task) in [
            (Phase::Thinking, None, Some("t-1")),
            (Phase::Tool, Some("subagent.run"), None),
            (Phase::Waiting, Some("ask_human"), Some("t-2")),
        ] {
            let a = Activity {
                task: task.map(str::to_string),
                ctx: Some("c-1".into()),
                phase,
                tool: tool.map(str::to_string),
                round: 1,
                tokens_in: 12,
                tokens_out: 4,
                started_ms: 1_790_631_625_858,
                updated_ms: 1_790_631_625_858,
            };
            let mut v = a.to_value();
            v["id"] = json!("1");
            out.push((FeedKind::Activity, v));
        }

        // The rest as a debug daemon pushed them (captured from a live feed:
        // a turn that delegated, a workflow run, pause and resume).
        out.extend([
            (
                FeedKind::Task,
                json!({"task": {
                    "id": "292d3149-7786-4d13-a994-8e7949b48cb0",
                    "contextId": "3a9d525f-c5b8-4f5d-bd41-9a10e1a641d7",
                    "status": {"state": "TASK_STATE_COMPLETED", "timestamp": "2026-09-28T21:40:26.178Z"},
                    "history": [{"contextId": "3a9d525f-c5b8-4f5d-bd41-9a10e1a641d7",
                                 "messageId": "015a695e-8acd-44b9-a918-a837389b7754",
                                 "parts": [{"text": "count for me"}], "role": "ROLE_USER",
                                 "taskId": "292d3149-7786-4d13-a994-8e7949b48cb0"}],
                    "artifacts": [{"artifactId": "292d3149.result", "parts": [{"text": "delegated and done"}]}],
                    "metadata": {super::super::ext::TASK_ANNOTATIONS_EXTENSION: {
                        "created": "2026-09-28T21:40:25.851Z",
                        "link": {"id": "3a9d525f-c5b8-4f5d-bd41-9a10e1a641d7", "kind": "turn"},
                        "principal": "operator"}},
                }}),
            ),
            (FeedKind::TaskRemoved, json!({"id": "292d3149"})),
            (FeedKind::RunRemoved, json!({"id": "greet-1"})),
            (
                FeedKind::Step,
                json!({"attempt": 1, "kind": "sleep", "phase": "start",
                       "run": "greet-01M3MZCH4MBN31C5JWJ2ABXMH0", "step": "w"}),
            ),
            (
                FeedKind::Step,
                json!({"attempt": 1, "err": null, "phase": "done", "run": "greet-01M3MZCH4MBN31C5JWJ2ABXMH0",
                       "status": "done", "step": "w", "tokens": 0}),
            ),
            (
                FeedKind::Step,
                json!({"attempt": 2, "err": "timed out", "phase": "done", "run": "r",
                       "status": "timeout", "step": "w", "tokens": 30}),
            ),
            (FeedKind::ConversationRemoved, json!({"id": "c-1"})),
            (
                FeedKind::Subagent,
                json!({"error": null, "handle": "sub-2", "mode": "sync", "status": "completed",
                       "tokens": 16, "updated": 1_790_631_625_868u64}),
            ),
            (FeedKind::SubagentRemoved, json!({"id": "sub-2"})),
            (
                FeedKind::Child,
                json!({"age_ms": 0, "cancelled": false, "kind": "turn:3a9d525f",
                       "node": 1, "pid": 3_572_696, "tokens": 0}),
            ),
            (FeedKind::ChildRemoved, json!({"id": "1"})),
            (FeedKind::ActivityRemoved, json!({"id": "1"})),
            (
                FeedKind::Status,
                json!({"budget": {"active": false, "events": 0,
                                  "instance": {"lifetime": null, "tactic": "wait", "windows": []},
                                  "reservations": 0, "scopes": {}, "waiting": {}},
                       "counters": {"runs_finished": 0, "runs_started": 0, "tokens_in": 0,
                                    "tokens_out": 0, "tool_calls": 0, "turns": 0},
                       "draining": false, "inbox_pending": 0, "instance": "cap", "model": "mock",
                       "skill_prefix": "@skill:", "store": {"degraded": false, "kind": "memory"},
                       "values": {"deploy.state": "green"}, "version": crate::VERSION}),
            ),
            (FeedKind::Lifecycle, json!({"draining": true, "reason": "signal"})),
            (FeedKind::Lifecycle, json!({"paused": true, "reason": "operator request"})),
            (FeedKind::Lifecycle, json!({"paused": false})),
            (
                FeedKind::Config,
                json!({"paths": ["a2a.introspection.enabled"], "source": "admin.set"}),
            ),
            (
                FeedKind::Config,
                json!({"paths": ["agent", "workflows"], "source": "reload"}),
            ),
            (
                FeedKind::Audit,
                json!({"action": "a2a.SendMessage", "outcome": "ok", "principal": "operator",
                       "role": "operator", "target": {"task": "292d3149"}, "ts": 1_790_631_625_852u64}),
            ),
            (
                FeedKind::Audit,
                json!({"action": "config.reload", "outcome": "ok", "principal": null, "role": null,
                       "target": null, "ts": 1_790_631_625_852u64, "sid": "ds_0123456789abcdef"}),
            ),
        ]);

        // `auth`: one per event value, from the builders that push them where
        // they are compiled, and the launch push as the contract gives it.
        #[cfg(feature = "a2a")]
        {
            use crate::a2a::oauth::{PendingView, auth_event};
            let view = PendingView {
                user_code: "WDJBMJHT".into(),
                client_id: "agentd-tui".into(),
                requested: DeviceScope::User,
                peer: Some(std::net::IpAddr::from([127, 0, 0, 1])),
                requested_ms: 0,
                expires_ms: 0,
            };
            // `Session::revoked_event`'s own skeleton, with what it adds.
            let mut revoked = auth_event("revoked", "agentd-tui", "user");
            revoked["sid"] = json!("ds_0123456789abcdef");
            revoked["name"] = json!("alice-laptop");
            out.extend(
                [
                    view.event("pending"),
                    view.approved_event("alice-laptop", DeviceScope::Operator),
                    view.event("denied"),
                    revoked,
                ]
                .map(|v| (FeedKind::Auth, v)),
            );
        }
        #[cfg(not(feature = "a2a"))]
        out.extend(
            [
                json!({"event": "pending", "client_id": "agentd-tui", "scope": "user",
                       "user_code": "WDJB-MJHT", "peer": "127.0.0.1"}),
                json!({"event": "approved", "client_id": "agentd-tui", "scope": "operator",
                       "user_code": "WDJB-MJHT", "name": "alice-laptop"}),
                json!({"event": "denied", "client_id": "agentd-tui", "scope": "user",
                       "user_code": "WDJB-MJHT"}),
                json!({"event": "revoked", "client_id": "agentd-tui", "scope": "user",
                       "sid": "ds_0123456789abcdef", "name": "alice-laptop"}),
            ]
            .map(|v| (FeedKind::Auth, v)),
        );
        out.push((
            FeedKind::Auth,
            json!({"event": "launch", "sid": "ls_0123456789abcdef",
                   "client_id": "agentd-tui", "scope": "operator"}),
        ));
        out
    }
}
