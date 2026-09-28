// SPDX-License-Identifier: AGPL-3.0-only
//! The **A2A transport binding**: the HTTPS listener that turns A2A requests
//! into runtime work, and the durable-task lifecycle behind it.
//!
//! Two halves meet here. The **transport** ([`A2aBridge`] plus `a2a::serve`'s
//! `Listener` and its `Auth`) runs on the framework's per-connection threads:
//! it resolves the caller to a [`Principal`], enforces the authorization
//! matrix, and posts each request to the single-writer loop as [`Event::A2a`],
//! blocking on a per-request oneshot. The **binding** (`impl Runtime`) runs on
//! the loop: it creates/advances durable [`Task`](crate::a2a::Task)s, routes natural-language
//! messages to conversation turns and command DataParts to the registry, and
//! answers `GetTask`/`ListTasks`/`CancelTask` and the operator admin family.
//! Reads that must not stall the loop — a blocking `SendMessage`, a stream —
//! are served by the transport thread polling a **shared task-snapshot map**
//! the loop keeps current.
//!
//! Identity: the listener reads the request's evidence — its bearer, its
//! verified client certificate, whether the peer is local, whether a browser
//! sent it — and the bridge's [`Resolver`] names the caller from it. The
//! resolver is also where the listener's posture lives, so a reload that
//! rebuilds the rules replaces the posture in the same swap.

use crate::a2a::principals::{Evidence, Resolution, SessionVerifier};
use crate::a2a::{Principal, Resolver};
use crate::runtime::events::Event;
use crate::runtime::reactor::Runtime;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::mpsc::{Sender, SyncSender, sync_channel};
use std::time::Duration;

mod admin;
/// The `auth.*` command ops: the operator's side of the device grant and the
/// session list, all on the operator floor.
mod auth_ops;
mod card;
mod commands;
mod feed;
mod introspection;
mod listener;
mod redact;
mod send;
mod tasks;

pub use feed::{FeedVis, SharedFeed};
pub(crate) use listener::{A2aServing, spawn_a2a_listener};
pub use send::command_op;
pub(crate) use send::{command_data, command_names_task};

use super::surface::{Active, Declaration, declared_when};
pub use super::surface::{COMMAND_EXTENSION, command_ops_of};

/// A2A error: no such task.
pub const TASK_NOT_FOUND: i64 = -32001;
/// A2A error: the operation is not supported over this surface.
pub const UNSUPPORTED_OPERATION: i64 = -32004;
/// The interface feed ring capacity: the replay window a reconnecting client
/// can resume across without a full re-bootstrap.
pub const FEED_RING: usize = 1024;

// ---- the request handed to the loop ----------------------------------------

/// A mutation/read posted to the single-writer loop, answered on `reply`.
#[derive(Debug)]
pub struct A2aRequest {
    pub method: String,
    pub params: Value,
    pub principal: Principal,
    /// The extensions the request activated. The runtime projects a task's
    /// annotations only while task-annotations/v1 is among them.
    pub active: Active,
    pub reply: SyncSender<Value>,
}

/// The transport's post-office into the loop + the shared view.
pub struct A2aBridge {
    events_tx: Sender<Event>,
    /// The observation feed — `None` unless `a2a.events.enabled`.
    feed: Option<Arc<SharedFeed>>,
    /// Swappable, so a reload can rebuild the principal rules without a
    /// restart. Read once per inbound request and written only by a reload,
    /// which is what an `RwLock` is for; the `Arc` inside keeps the read side
    /// to a clone rather than holding the lock across `resolve`.
    resolver: std::sync::RwLock<Arc<Resolver>>,
    pub request_timeout: Duration,
    pub stream_deadline: Duration,
}

impl A2aBridge {
    /// The listener's post office into the loop.
    pub fn new(events_tx: Sender<Event>, resolver: Resolver) -> Arc<A2aBridge> {
        Self::with_feed(events_tx, resolver, None)
    }

    /// [`A2aBridge::new`] with the observation feed attached.
    pub fn with_feed(
        events_tx: Sender<Event>,
        resolver: Resolver,
        feed: Option<Arc<SharedFeed>>,
    ) -> Arc<A2aBridge> {
        Arc::new(A2aBridge {
            events_tx,
            feed,
            resolver: std::sync::RwLock::new(Arc::new(resolver)),
            request_timeout: Duration::from_secs(120),
            stream_deadline: Duration::from_secs(600),
        })
    }

    /// Resolve one request's evidence under the rules in force now.
    ///
    /// The listener itself takes a [`resolver`](Self::resolver) snapshot
    /// instead, because it reads the posture from the same snapshot; this is
    /// the one-shot form for a caller that needs only the answer.
    pub fn resolve(
        &self,
        ev: &Evidence,
        unix: bool,
        sessions: Option<&dyn SessionVerifier>,
    ) -> Resolution {
        self.resolver().resolve(ev, unix, sessions)
    }

    /// The rules — and the posture built with them — in force now. One
    /// snapshot per request: a reload that lands mid-request cannot pair new
    /// rules with an old posture, because the two are one value.
    pub fn resolver(&self) -> Arc<Resolver> {
        self.resolver
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Install rebuilt principal rules — a reload of `a2a.principals`.
    ///
    /// Requests already in flight keep the rules they were resolved under;
    /// the next request uses these. That is the same "workers in flight keep
    /// what they were spawned with" rule the intelligence reload follows.
    pub fn set_resolver(&self, resolver: Resolver) {
        *self.resolver.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(resolver);
    }

    /// The observation feed, when `a2a.events.enabled` armed one.
    pub fn feed(&self) -> Option<Arc<SharedFeed>> {
        self.feed.clone()
    }

    /// The extensions the listener negotiates against: what it serves. The
    /// feed is armed at spawn and `a2a.events` is restart-only, so holding a
    /// feed is the same answer the card reads from the settings — and one
    /// the listener can give without a round trip to the loop.
    pub fn declared(&self) -> Vec<Declaration> {
        declared_when(self.feed.is_some())
    }

    /// Post a request to the loop and wait for its reply. Blocking — an async
    /// caller (the A2A ports) runs this on a blocking thread. `active` is what
    /// the request activated ([`Active::NONE`] for a call no request made).
    pub fn call(&self, method: &str, params: Value, principal: Principal, active: Active) -> Value {
        let (reply_tx, reply_rx) = sync_channel(1);
        let req = A2aRequest {
            method: method.to_string(),
            params,
            principal,
            active,
            reply: reply_tx,
        };
        if self.events_tx.send(Event::A2a(Box::new(req))).is_err() {
            return err_obj(rpc_internal(), "the runtime is shutting down");
        }
        reply_rx
            .recv_timeout(self.request_timeout)
            .unwrap_or_else(|_| err_obj(rpc_internal(), "the runtime did not answer in time"))
    }
}

// ---- the bridge's verbs ------------------------------------------------------

/// What the transport asks the runtime: the verbs of the bridge between them.
/// Not the spec's method names — push configs travel as `PushConfig*`, and
/// the public card is `PublicCard` because no wire method reads it — so
/// anything that classifies
/// a call the runtime answered (the audit mirror) reads THIS, not the spec's
/// vocabulary, which is how it once listed names that never arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb {
    SendMessage,
    GetTask,
    ListTasks,
    CancelTask,
    PushConfigSet,
    PushConfigGet,
    PushConfigList,
    PushConfigDelete,
    PublicCard,
    GetExtendedAgentCard,
}

impl Verb {
    /// Every verb, for the tests that hold a classification to all of them.
    #[cfg(test)]
    pub(crate) const ALL: &[Verb] = &[
        Verb::SendMessage,
        Verb::GetTask,
        Verb::ListTasks,
        Verb::CancelTask,
        Verb::PushConfigSet,
        Verb::PushConfigGet,
        Verb::PushConfigList,
        Verb::PushConfigDelete,
        Verb::PublicCard,
        Verb::GetExtendedAgentCard,
    ];

    /// The verb `method` names, if the runtime answers it. Matched exactly:
    /// the listener's route table has already refused every other spelling.
    pub(crate) fn of(method: &str) -> Option<Verb> {
        Some(match method {
            "SendMessage" | "SendStreamingMessage" => Verb::SendMessage,
            "GetTask" => Verb::GetTask,
            "ListTasks" => Verb::ListTasks,
            "CancelTask" => Verb::CancelTask,
            "PushConfigSet" => Verb::PushConfigSet,
            "PushConfigGet" => Verb::PushConfigGet,
            "PushConfigList" => Verb::PushConfigList,
            "PushConfigDelete" => Verb::PushConfigDelete,
            "PublicCard" => Verb::PublicCard,
            "GetExtendedAgentCard" => Verb::GetExtendedAgentCard,
            _ => return None,
        })
    }

    /// Whether the verb changes nothing a caller can observe. One exhaustive
    /// match, so a new verb cannot be left unclassified.
    pub(crate) fn reads(self) -> bool {
        match self {
            Verb::GetTask
            | Verb::ListTasks
            | Verb::PushConfigGet
            | Verb::PushConfigList
            | Verb::PublicCard
            | Verb::GetExtendedAgentCard => true,
            Verb::SendMessage | Verb::CancelTask | Verb::PushConfigSet | Verb::PushConfigDelete => {
                false
            }
        }
    }
}

// ---- wire helpers ----------------------------------------------------------

fn err_obj(code: i64, msg: &str) -> Value {
    json!({"_error": {"code": code, "message": msg}})
}

fn rpc_internal() -> i64 {
    ::mcp::rpc::INTERNAL_ERROR
}

// ---- the runtime binding (runs on the single-writer loop) -------------------

impl Runtime {
    /// Handle one A2A request (posted by the transport). Never blocks: work
    /// that takes time (a turn, a run) starts here and is polled by the caller.
    pub(crate) fn on_a2a_request(&mut self, req: A2aRequest) {
        let A2aRequest {
            method,
            params,
            principal,
            active,
            reply,
        } = req;
        // What this request activated, for every task it projects — however
        // deep in the handler the projection is made. Cleared with the
        // reserved id below, so nothing outside a request is ever annotated
        // for a caller who did not ask.
        self.a2a_active = active;
        // Index this caller's declared budget and labels the first time they
        // appear, so everything downstream can find them by id alone — the run
        // record, the MCP `_meta` and the audit line all carry the id, never
        // the whole principal. (The declared rate is admission, and the
        // listener applies it before a request reaches this loop.)
        self.note_principal(&principal);
        // A send that creates a task creates it under the id a2a-rs generated
        // for it, because the protocol layer subscribed to that id before the
        // work started. Whichever path creates it — a conversation turn or a
        // command — takes the id from here, so the caller is watching the task
        // it is actually given. Only a new task's id is reserved: the id a
        // caller named is for a task that must already exist (see `a2a_send`).
        self.reserved_task_id = (params["newTask"] == json!(true))
            .then(|| params["taskId"].as_str())
            .flatten()
            .filter(|s| !s.is_empty() && !self.tasks.contains_key(*s))
            .map(str::to_string);
        let out = match Verb::of(&method) {
            Some(Verb::SendMessage) => self.a2a_send(&principal, &params),
            Some(Verb::GetTask) => self.a2a_get_task(&principal, &params),
            Some(Verb::ListTasks) => self.a2a_list_tasks(&principal, &params),
            Some(Verb::CancelTask) => self.a2a_cancel_task(&principal, &params),
            Some(Verb::PushConfigSet) => self.a2a_push_set(&principal, &params),
            Some(Verb::PushConfigGet) => self.a2a_push_get(&principal, &params),
            Some(Verb::PushConfigList) => self.a2a_push_list(&principal, &params),
            Some(Verb::PushConfigDelete) => self.a2a_push_delete(&principal, &params),
            Some(Verb::PublicCard) => self.a2a_agent_card(),
            Some(Verb::GetExtendedAgentCard) => self.a2a_extended_card(&principal),
            None => err_obj(
                UNSUPPORTED_OPERATION,
                &format!("unsupported method: {method}"),
            ),
        };
        self.reserved_task_id = None;
        self.a2a_active = Active::NONE;
        // Audit every A2A call: who (principal + role), what (method + command
        // op), and the outcome. This is the record of who authorized what, so
        // it is emitted for refusals as well as successes.
        let op = params.get("message").and_then(command_op);
        let outcome = if out.get("_error").is_some() {
            "error"
        } else {
            "ok"
        };
        let target = out["task"]["id"]
            .as_str()
            .map(|id| json!({"task": id}))
            .unwrap_or(Value::Null);
        let request_id = params["message"]["messageId"].as_str();
        self.audit_a2a(
            &method,
            op.as_deref(),
            &principal,
            outcome,
            target,
            request_id,
        );
        let _ = reply.send(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "a2a")]
    #[test]
    fn mtls_san_resolves_to_the_matched_principal_role() {
        // The client-cert SAN/subject drives the principal: a SPIFFE URI SAN
        // matches a `san` rule, and that rule's role is the caller's.
        use crate::a2a::principals::{CertId, Via};

        let resolver = Resolver::build(
            &serde_json::from_value(json!({
                "principals": [
                    {"match": {"san": "spiffe://corp/ops/*"}, "role": "operator"},
                    {"match": {"san": "spiffe://corp/team/*"}, "role": "user", "grants": ["knowledge.*"]},
                ]
            }))
            .unwrap(),
            &|_| None,
        )
        .unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let bridge = A2aBridge::new(tx, resolver);
        let with_san = |san: &str| Evidence {
            cert: Some(CertId {
                subject: None,
                sans: vec![san.into()],
            }),
            ..Default::default()
        };

        // A SPIFFE X.509-SVID (empty subject; identity in the URI SAN) under the
        // team trust path → the user role, labelled by its SAN.
        let Resolution::Named(p, Via::Cert) =
            bridge.resolve(&with_san("spiffe://corp/team/alice"), false, None)
        else {
            panic!("the team SVID named nobody");
        };
        assert_eq!(p.role, crate::config::v2::Role::User);
        assert_eq!(p.id, "user:san=spiffe://corp/team/alice");
        // A cert under the ops path → operator (a different rule).
        let Resolution::Named(op, _) =
            bridge.resolve(&with_san("spiffe://corp/ops/root"), false, None)
        else {
            panic!("the ops SVID named nobody");
        };
        assert!(op.is_operator());
        // A cert matching NO rule, with principals configured, has no role:
        // declaring any principal rule turns the allowlist on.
        assert_eq!(
            bridge.resolve(&with_san("spiffe://other/x"), false, None),
            Resolution::NoRole
        );
    }
}
