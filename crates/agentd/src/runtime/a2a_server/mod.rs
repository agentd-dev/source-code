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
/// The `auth.*` command ops. Empty until the device grant lands; declared now so
/// the units that fill it never have to edit this file.
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
pub(crate) use send::command_data;
pub use send::command_op;
use tasks::new_task_id;

pub use super::surface::{
    COMMAND_EXTENSION, EXTENSION_METHODS, EXTENSIONS, INTERFACE_EXTENSION, command_ops_of,
    extensions_of,
};

/// The JSON-RPC surface, defined in [`crate::runtime::surface`] so the
/// always-compiled `--capabilities` manifest can read it without the `a2a`
/// feature. Re-exported here because this is where the dispatch lives.
pub use crate::runtime::surface::{LOCAL_METHODS, METHODS};

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

    /// Post a request to the loop and wait for its reply. Blocking — an async
    /// caller (the A2A ports) runs this on a blocking thread.
    pub fn call(&self, method: &str, params: Value, principal: Principal) -> Value {
        self.call_loop(method, params, principal)
    }

    /// Post a request to the loop and wait for its reply.
    fn call_loop(&self, method: &str, params: Value, principal: Principal) -> Value {
        let (reply_tx, reply_rx) = sync_channel(1);
        let req = A2aRequest {
            method: method.to_string(),
            params,
            principal,
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
/// Not the spec's method names — push configs travel as `PushConfig*`, and a
/// send is preceded by a `NewTaskId` mint — so anything that classifies a
/// call the runtime answered (the audit mirror) reads THIS, not the spec's
/// vocabulary, which is how it once listed names that never arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb {
    SendMessage,
    NewTaskId,
    GetTask,
    ListTasks,
    CancelTask,
    PushConfigSet,
    PushConfigGet,
    PushConfigList,
    PushConfigDelete,
    GetAgentCard,
    GetExtendedAgentCard,
}

impl Verb {
    /// Every verb, for the tests that hold a classification to all of them.
    #[cfg(test)]
    pub(crate) const ALL: &[Verb] = &[
        Verb::SendMessage,
        Verb::NewTaskId,
        Verb::GetTask,
        Verb::ListTasks,
        Verb::CancelTask,
        Verb::PushConfigSet,
        Verb::PushConfigGet,
        Verb::PushConfigList,
        Verb::PushConfigDelete,
        Verb::GetAgentCard,
        Verb::GetExtendedAgentCard,
    ];

    /// The verb a bare method name is, if the runtime answers it.
    pub(crate) fn of(method: &str) -> Option<Verb> {
        Some(match method {
            "SendMessage" | "SendStreamingMessage" => Verb::SendMessage,
            "NewTaskId" => Verb::NewTaskId,
            "GetTask" => Verb::GetTask,
            "ListTasks" => Verb::ListTasks,
            "CancelTask" => Verb::CancelTask,
            "PushConfigSet" => Verb::PushConfigSet,
            "PushConfigGet" => Verb::PushConfigGet,
            "PushConfigList" => Verb::PushConfigList,
            "PushConfigDelete" => Verb::PushConfigDelete,
            "GetAgentCard" => Verb::GetAgentCard,
            "GetExtendedAgentCard" => Verb::GetExtendedAgentCard,
            _ => return None,
        })
    }

    /// Whether the verb changes nothing a caller can observe. A mint is
    /// plumbing the transport issues before every send, not a change. One
    /// exhaustive match, so a new verb cannot be left unclassified.
    pub(crate) fn reads(self) -> bool {
        match self {
            Verb::NewTaskId
            | Verb::GetTask
            | Verb::ListTasks
            | Verb::PushConfigGet
            | Verb::PushConfigList
            | Verb::GetAgentCard
            | Verb::GetExtendedAgentCard => true,
            Verb::SendMessage | Verb::CancelTask | Verb::PushConfigSet | Verb::PushConfigDelete => {
                false
            }
        }
    }
}

// ---- wire helpers ----------------------------------------------------------

/// Strip an optional `a2a.` prefix.
fn bare(m: &str) -> &str {
    m.strip_prefix("a2a.").unwrap_or(m)
}

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
            reply,
        } = req;
        // Index this caller's declared budget and labels the first time they
        // appear, so everything downstream can find them by id alone — the run
        // record, the MCP `_meta` and the audit line all carry the id, never
        // the whole principal. (The declared rate is admission, and the
        // listener applies it before a request reaches this loop.)
        self.note_principal(&principal);
        // The listener pre-mints the id of the task this request will create,
        // because the protocol layer subscribes to a task's updates before the
        // work starts. Whichever path creates it — a conversation turn or a
        // command — takes the id from here, so the caller is watching the task
        // it is actually given.
        self.reserved_task_id = params["message"]["taskId"]
            .as_str()
            .filter(|s| !s.is_empty() && !self.tasks.contains_key(*s))
            .map(str::to_string);
        let out = match Verb::of(bare(&method)) {
            Some(Verb::SendMessage) => self.a2a_send(&principal, &params),
            // The listener asks for the id a new task will have BEFORE
            // dispatching the send. The protocol layer subscribes to a task's
            // updates first and processes the message second, so that no
            // transition is missed — which means the id has to exist before the
            // work does. Minting stays here so one place owns the shape of a
            // task id (see `new_task_id`).
            Some(Verb::NewTaskId) => json!({"id": new_task_id()}),
            Some(Verb::GetTask) => self.a2a_get_task(&principal, &params),
            Some(Verb::ListTasks) => self.a2a_list_tasks(&principal, &params),
            Some(Verb::CancelTask) => self.a2a_cancel_task(&principal, &params),
            Some(Verb::PushConfigSet) => self.a2a_push_set(&principal, &params),
            Some(Verb::PushConfigGet) => self.a2a_push_get(&principal, &params),
            Some(Verb::PushConfigList) => self.a2a_push_list(&principal, &params),
            Some(Verb::PushConfigDelete) => self.a2a_push_delete(&principal, &params),
            Some(Verb::GetAgentCard) => self.a2a_agent_card(),
            Some(Verb::GetExtendedAgentCard) => self.a2a_extended_card(&principal),
            None => err_obj(
                UNSUPPORTED_OPERATION,
                &format!("unsupported method: {}", bare(&method)),
            ),
        };
        self.reserved_task_id = None;
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
            bare(&method),
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

    /// Every method agentd answers is spelled the way the SDK spells it, and
    /// the error codes peers branch on are the SDK's constants.
    ///
    /// These two assertions are what the `a2a-oracle` crate was really for.
    /// The rest of it booted the daemon and deserialized the replies with
    /// `a2a_rs` — which stopped proving anything the day the listener became
    /// `a2a_rs`'s own adapter: the same generated types on both ends of the
    /// round trip agree by construction. These do not need a daemon at all,
    /// and they still bind agentd's vocabulary to the crate generated from the
    /// A2A protobuf. The live behaviour the oracle also checked is covered by
    /// `a2a-conversation/protocol-errors-use-the-specified-codes`.
    #[test]
    fn our_method_names_and_error_codes_are_the_sdks() {
        use a2a_rs::adapter::transport::jsonrpc_wire::methods as m;
        let spec = [
            m::SEND_MESSAGE,
            m::SEND_STREAMING_MESSAGE,
            m::GET_TASK,
            m::LIST_TASKS,
            m::CANCEL_TASK,
            m::SUBSCRIBE_TO_TASK,
            m::CREATE_PUSH_CONFIG,
            m::GET_PUSH_CONFIG,
            m::LIST_PUSH_CONFIGS,
            m::DELETE_PUSH_CONFIG,
            m::GET_EXTENDED_AGENT_CARD,
        ];
        // Everything we dispatch is one of theirs, spelled identically — a
        // method we invented or misspelled is unreachable, and silently so.
        for name in METHODS {
            if *name == "SubscribeToEvents" {
                continue; // ours, declared as an extension rather than claimed
            }
            assert!(
                spec.contains(name),
                "agentd answers {name:?}, which is not an A2A method: {spec:?}"
            );
        }
        // …and every spec method is one we answer, so the card cannot promise
        // a surface the dispatcher lacks.
        for name in spec {
            assert!(
                METHODS.contains(&name),
                "the spec defines {name:?} and agentd does not answer it"
            );
        }
        assert_eq!(a2a_rs::domain::error::TASK_NOT_FOUND, -32001);
        assert_eq!(a2a_rs::domain::error::UNSUPPORTED_OPERATION, -32004);
    }

    /// Every method agentd answers is either an A2A method or DECLARED as an
    /// extension. The oracle checks the first half against an independent
    /// implementation of the spec; this checks the second, which is the half
    /// that rots — a method added without a declaration is a private protocol
    /// no peer can discover, and nothing else would notice.
    #[test]
    fn every_non_spec_method_is_declared_as_an_extension() {
        // The A2A JSON-RPC vocabulary (the oracle pins this against a2a-rs).
        const SPEC: &[&str] = &[
            "SendMessage",
            "SendStreamingMessage",
            "GetTask",
            "ListTasks",
            "CancelTask",
            "SubscribeToTask",
            "CreateTaskPushNotificationConfig",
            "GetTaskPushNotificationConfig",
            "ListTaskPushNotificationConfigs",
            "DeleteTaskPushNotificationConfig",
            "GetExtendedAgentCard",
        ];
        let undeclared: Vec<&&str> = METHODS
            .iter()
            .filter(|m| !SPEC.contains(m))
            .filter(|m| !EXTENSION_METHODS.iter().any(|(name, _)| name == *m))
            .collect();
        assert!(
            undeclared.is_empty(),
            "these methods are neither A2A nor declared under an extension: {undeclared:?}"
        );
        // …and every declaration names an extension this build can activate,
        // so a client that asks for it by URI is actually granted it.
        for (method, uri) in EXTENSION_METHODS {
            assert!(
                EXTENSIONS.contains(uri),
                "{method:?} is declared under {uri:?}, which is not in EXTENSIONS"
            );
        }
    }

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

    /// Callers may address a method with or without the historical `a2a.`
    /// prefix. (Frame construction and terminal classification moved to
    /// `a2a::wire` and to a2a-rs respectively.)
    #[test]
    fn a_method_may_be_addressed_with_or_without_the_prefix() {
        assert_eq!(bare("a2a.SendMessage"), "SendMessage");
        assert_eq!(bare("GetTask"), "GetTask");
    }
}
