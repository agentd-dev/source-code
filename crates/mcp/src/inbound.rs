// SPDX-License-Identifier: AGPL-3.0-only
//! **Server→client requests**: the half of MCP a client usually forgets.
//!
//! MCP is bidirectional. A server may send the client a *request* — not just a
//! notification — and the spec is unambiguous that the receiver answers:
//! `ping` MUST be responded to by either side, and a server that declared the
//! matching client capability may call `elicitation/create` (ask the human) or
//! `roots/list` (what may I operate on?).
//!
//! Until now this client dropped every inbound request on the floor. A server
//! that pinged us saw silence and was entitled to consider the connection dead;
//! a server that wanted to ask the operator a question had no way to.
//!
//! The rules this module encodes:
//!
//! * **Answer what we advertised, refuse what we did not.** A capability we do
//!   not declare gets `-32601 Method not found` rather than a half-answer, so a
//!   server can feature-detect by asking.
//! * **The host owns the human.** `elicitation/create` is delegated to a
//!   [`Handler`] the embedder supplies (agentd routes it to `ask_human`, whose
//!   gates already render in every attached client and survive a restart). The
//!   crate never invents an answer.
//! * **Decline is not an error.** The elicitation schema has three outcomes —
//!   `accept`, `decline`, `cancel` — and a user who says no is a successful
//!   response carrying `"decline"`, not a JSON-RPC error.

use crate::rpc::{self, Id, Response};
use serde_json::{Value, json};

/// A server→client request the host may be asked to answer.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// `elicitation/create` — the server needs input from the human operator.
    /// Carries the server's message and the requested-schema, verbatim.
    Elicit {
        message: String,
        requested_schema: Value,
    },
    /// `roots/list` — the server is asking which URI roots it may operate on.
    ListRoots,
}

/// What the host decided. Mirrors the spec's elicitation outcomes so a refusal
/// is expressible without inventing content.
#[derive(Debug, Clone)]
pub enum Answer {
    /// The user answered; `content` matches the requested schema.
    Accept(Value),
    /// The user actively refused. Not an error.
    Decline,
    /// The user dismissed it without deciding (or nothing could ask).
    Cancel,
    /// The roots this client exposes.
    Roots(Vec<Root>),
}

/// One entry of the `roots/list` result.
#[derive(Debug, Clone)]
pub struct Root {
    pub uri: String,
    pub name: Option<String>,
}

/// The host's answering surface. Implemented by the embedder; `None` anywhere
/// means "we did not advertise that capability", and the request is refused.
pub trait Handler: Send + Sync {
    /// Answer a server→client request. Returning `None` declines the capability
    /// itself (the caller turns that into `-32601`).
    fn handle(&self, req: Inbound) -> Option<Answer>;
}

/// Which client capabilities a [`Handler`] backs. Declared in the `initialize`
/// handshake (legacy) and in `_meta` client capabilities (modern), so a server
/// only calls what we can actually answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub elicitation: bool,
    pub roots: bool,
}

impl Capabilities {
    /// The JSON object a client advertises. `ping` is not a capability — it is
    /// unconditional — so it never appears here.
    pub fn to_json(self) -> Value {
        let mut caps = serde_json::Map::new();
        if self.elicitation {
            caps.insert("elicitation".into(), json!({}));
        }
        if self.roots {
            // `listChanged` stays false: we have no notification path for root
            // changes yet, and advertising one we do not send is worse than not
            // advertising it.
            caps.insert("roots".into(), json!({"listChanged": false}));
        }
        Value::Object(caps)
    }

    pub fn is_empty(self) -> bool {
        !self.elicitation && !self.roots
    }
}

/// Answer one inbound JSON-RPC request.
///
/// `ping` is answered unconditionally (spec MUST) even with no handler at all —
/// that is the whole point: a liveness probe must not depend on what the host
/// chose to implement.
pub fn answer(req: &rpc::Request, caps: Capabilities, handler: Option<&dyn Handler>) -> Response {
    let id = req.id.clone();
    match req.method.as_str() {
        // Both sides MUST respond; the result is an empty object.
        "ping" => Response::ok(id, json!({})),

        "elicitation/create" if caps.elicitation => {
            let params = req.params.clone().unwrap_or(Value::Null);
            let message = params
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let requested_schema = params
                .get("requestedSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            match handler.and_then(|h| {
                h.handle(Inbound::Elicit {
                    message,
                    requested_schema,
                })
            }) {
                Some(Answer::Accept(content)) => {
                    Response::ok(id, json!({"action": "accept", "content": content}))
                }
                Some(Answer::Decline) => Response::ok(id, json!({"action": "decline"})),
                // No handler, or nothing could ask: cancel is the honest answer.
                Some(Answer::Cancel) | None => Response::ok(id, json!({"action": "cancel"})),
                Some(Answer::Roots(_)) => Response::err(
                    id,
                    rpc::INTERNAL_ERROR,
                    "handler answered elicitation with roots",
                ),
            }
        }

        "roots/list" if caps.roots => match handler.and_then(|h| h.handle(Inbound::ListRoots)) {
            Some(Answer::Roots(roots)) => Response::ok(
                id,
                json!({
                    "roots": roots.iter().map(|r| match &r.name {
                        Some(n) => json!({"uri": r.uri, "name": n}),
                        None => json!({"uri": r.uri}),
                    }).collect::<Vec<_>>()
                }),
            ),
            _ => Response::ok(id, json!({"roots": []})),
        },

        // Anything we did not advertise — including elicitation/roots when the
        // capability is off. Feature detection by asking is legitimate, so this
        // is a clean refusal, not a fault.
        other => Response::err(
            id,
            rpc::METHOD_NOT_FOUND,
            format!("client does not implement {other}"),
        ),
    }
}

/// Classify a raw inbound JSON-RPC frame. A frame with an `id` AND a `method` is
/// a request we must answer; with a `method` and no `id` it is a notification;
/// anything else (a response to something we sent) is not ours to route here.
pub fn as_request(v: &Value) -> Option<rpc::Request> {
    if v.get("method").is_some() && v.get("id").is_some() {
        serde_json::from_value::<rpc::Request>(v.clone()).ok()
    } else {
        None
    }
}

/// The JSON-RPC id of a frame, for logging a dropped/failed answer.
pub fn frame_id(v: &Value) -> Option<Id> {
    serde_json::from_value::<Id>(v.get("id")?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Yes(Answer);
    impl Handler for Yes {
        fn handle(&self, _req: Inbound) -> Option<Answer> {
            Some(self.0.clone())
        }
    }
    struct No;
    impl Handler for No {
        fn handle(&self, _req: Inbound) -> Option<Answer> {
            None
        }
    }

    fn req(method: &str, params: Value) -> rpc::Request {
        rpc::Request::new(1, method, Some(params))
    }

    #[test]
    fn ping_is_answered_even_with_no_handler_or_capabilities() {
        // The liveness probe must not depend on what the host implements —
        // this is the gap that let a server consider us dead.
        let r = answer(&req("ping", json!({})), Capabilities::default(), None);
        assert_eq!(r.result, Some(json!({})));
        assert!(r.error.is_none());
    }

    #[test]
    fn elicitation_maps_the_three_outcomes() {
        let caps = Capabilities {
            elicitation: true,
            roots: false,
        };
        let p = json!({"message": "Which environment?", "requestedSchema": {"type": "object"}});

        let accept = Yes(Answer::Accept(json!({"env": "staging"})));
        let r = answer(&req("elicitation/create", p.clone()), caps, Some(&accept));
        assert_eq!(r.result.as_ref().unwrap()["action"], "accept");
        assert_eq!(r.result.unwrap()["content"]["env"], "staging");

        // A refusal is a SUCCESSFUL response carrying `decline`, not an error.
        let decline = Yes(Answer::Decline);
        let r = answer(&req("elicitation/create", p.clone()), caps, Some(&decline));
        assert!(r.error.is_none());
        assert_eq!(r.result.unwrap()["action"], "decline");

        // Nothing could ask ⇒ cancel.
        let r = answer(&req("elicitation/create", p.clone()), caps, Some(&No));
        assert_eq!(r.result.unwrap()["action"], "cancel");
        let r = answer(&req("elicitation/create", p), caps, None);
        assert_eq!(r.result.unwrap()["action"], "cancel");
    }

    #[test]
    fn an_undeclared_capability_is_refused_not_half_answered() {
        // A server may probe by calling; the honest answer is method-not-found.
        let none = Capabilities::default();
        let r = answer(&req("elicitation/create", json!({})), none, Some(&No));
        assert_eq!(r.error.as_ref().unwrap().code, rpc::METHOD_NOT_FOUND);
        let r = answer(&req("roots/list", json!({})), none, Some(&No));
        assert_eq!(r.error.as_ref().unwrap().code, rpc::METHOD_NOT_FOUND);
        // And anything we simply do not implement.
        let r = answer(&req("sampling/createMessage", json!({})), none, None);
        assert_eq!(r.error.unwrap().code, rpc::METHOD_NOT_FOUND);
    }

    #[test]
    fn roots_are_listed_when_declared() {
        let caps = Capabilities {
            elicitation: false,
            roots: true,
        };
        let h = Yes(Answer::Roots(vec![Root {
            uri: "file:///work".into(),
            name: Some("workspace".into()),
        }]));
        let r = answer(&req("roots/list", json!({})), caps, Some(&h));
        let roots = &r.result.unwrap()["roots"];
        assert_eq!(roots[0]["uri"], "file:///work");
        assert_eq!(roots[0]["name"], "workspace");
    }

    #[test]
    fn capabilities_serialize_to_what_we_can_actually_answer() {
        assert_eq!(Capabilities::default().to_json(), json!({}));
        assert!(Capabilities::default().is_empty());
        let both = Capabilities {
            elicitation: true,
            roots: true,
        };
        assert_eq!(
            both.to_json(),
            json!({"elicitation": {}, "roots": {"listChanged": false}})
        );
        // `ping` is unconditional and never advertised as a capability.
        assert!(both.to_json().get("ping").is_none());
    }

    #[test]
    fn request_classification_separates_requests_from_notifications() {
        assert!(as_request(&json!({"jsonrpc":"2.0","id":1,"method":"ping"})).is_some());
        // No id ⇒ a notification, not ours to answer.
        assert!(as_request(&json!({"jsonrpc":"2.0","method":"notifications/x"})).is_none());
        // A response to something we sent.
        assert!(as_request(&json!({"jsonrpc":"2.0","id":1,"result":{}})).is_none());
    }
}
