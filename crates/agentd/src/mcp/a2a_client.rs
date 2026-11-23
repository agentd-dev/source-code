// SPDX-License-Identifier: AGPL-3.0-only
//! The A2A (Agent2Agent) **client** — agentd-as-A2A-client, the remote-A2A-agent
//! delegation backend. [feature: a2a]
//!
//! A coordinator can delegate an objective either to a LOCAL supervised subagent
//! (`subagent.spawn`) or to a REMOTE A2A agent. Both wear the same abstraction
//! (objective → distilled result); this module is the remote backend. It connects
//! to a declared peer (an [`A2aEndpoint`] — `https://host[:port]`, loopback
//! `http://` for dev, or `unix:///path` for a co-located one).
//!
//! **The protocol is the A2A SDK's.** Every A2A method this client sends
//! (`SendMessage`, `SendStreamingMessage`, `GetTask`) goes through the official
//! SDK's `A2AClient` over its `JsonRpcTransport` (`a2a-client-lf`): the SDK
//! writes the JSON-RPC envelope and the ProtoJSON, states `A2A-Version: 1.0` on
//! every request, parses the SSE stream and decodes every error into its
//! `A2AError`. What is agentd's is the **policy** below, and the `reqwest`
//! client the SDK's transport is handed: agentd's ring TLS configuration (the
//! bundled roots, the extra CA, the mTLS client identity), the unix socket of a
//! co-located peer, no proxy, one connection per request. The SDK is async;
//! every call runs on this module's own runtime and the blocking caller waits
//! for its answer, so a caller already inside a runtime never nests a second.
//!
//! A delegation is several requests, not one:
//!
//!   1. **read the peer's card** — `GET /.well-known/agent-card.json` on the
//!      configured origin, cached for the response's `max-age` (at most five
//!      minutes; one when it names none; five seconds for a peer on this
//!      host, whose port or socket may belong to a different child by the
//!      next delegation), per origin AND credential, and dropped whenever a
//!      delegation using it fails. The card says whether the peer speaks A2A
//!      1.0 on this kind of endpoint at all, under which tenant (that of the
//!      interface whose URL is the configured one, else the first), whether it
//!      streams, and which extensions it will not work without — a peer
//!      requiring one agentd does not implement is refused before anything is
//!      sent. A peer that serves no card is still dialled, streaming first.
//!      The read is a plain GET on the same `reqwest` client, not the SDK's
//!      `AgentCardResolver`: the resolver hands back the card alone, and the
//!      cache's lifetime is the response's `Cache-Control`.
//!   2. **send the objective** — `SendStreamingMessage` when the peer streams,
//!      whose frames carry the run to its end (working → artifact → terminal
//!      state); `SendMessage` with `returnImmediately` when it does not, or,
//!      once, when a stream is refused as unsupported (`-32004`). A peer that
//!      answers the streaming method with a plain JSON result is refused by
//!      the SDK ("expected streaming response"), and that is the answer.
//!   3. **recover by polling** — `GetTask` (~[`POLL_INTERVAL`] apart) whenever
//!      the answer is not yet in hand: a unary send, a stream that broke after
//!      the run started, a terminal frame whose artifact never arrived. The
//!      per-delegation deadline bounds it, so it never hangs, and a run that
//!      exists on the peer is never sent a second time.
//!   4. **return the result** — on COMPLETED, the task's artifacts (the
//!      **distillate**); a peer that answers with a message rather than a task
//!      has answered with that message; any other terminal state, a refusal or
//!      a transport failure is an error string.
//!
//! An A2A client does **not** send MCP `initialize` — A2A is its own surface, so
//! the client just calls the A2A methods. The requests are built from the
//! specification's own types ([`crate::a2a::peer`]) and handed to the SDK; its
//! answers are read back the same way.
//!
//! The card never chooses where agentd connects: the interface it selects
//! decides the protocol and the tenant, and the dial always goes to the URL the
//! operator configured. A card that names another host cannot redirect a
//! delegation, or the credentials it carries.
//!
//! Trust: agentd dials the peer over HTTP(S), presenting the peer client-auth
//! material on every request — a resolved **bearer/framing header** (static /
//! oauth2 device-login / SPIFFE JWT-SVID) and an **mTLS** client identity
//! (SPIFFE X.509-SVID). Nothing is signed per request: the SDK's interceptor
//! sees the method and the headers, never the URL or the body a signature
//! covers, so an A2A peer takes no AWS SigV4 (`kind: aws` is refused at
//! validation) and no AAuth signature.
//!
//! Trace: a hop a unit of work makes is a span of its own
//! ([`PeerAuth::trace`]), sent as `traceparent` and `tracestate` on every
//! request of the hop, so a peer that reads W3C Trace Context continues the
//! caller's trace. With the credential and `A2A-Extensions` (what the peer
//! requires, what the work offers, what the message is marked with) it rides
//! the SDK's `CallInterceptor`.
//!
//! The parent link: an instance child reports home ([`A2aEndpoint::Parent`])
//! through the channel it was handed at spawn — a door with no path, whose
//! every connection is a socket passed over `SCM_RIGHTS`. `reqwest` dials a
//! socket by path only, and its connector cannot be handed a stream of
//! agentd's (a connector layer must yield `reqwest`'s own `Conn`, which only
//! `reqwest` constructs), so that link stays on agentd's own transport
//! ([`parent`]): one `SendMessage` carrying an `_instance.result` or
//! `_instance.emit` report, which is all the parent accepts from a child.
//! Nothing else is dialled over it — no card, no stream, no poll.
//!
//! Paying: a task that asks to be paid (a2a-x402) will not go on by itself,
//! so a send or a delegation that may not pay ends there, saying so; one
//! that may ([`paying`]) hands the task back on the connection it asked on.

#[cfg(feature = "x402")]
mod paying;
#[cfg(feature = "x402")]
pub use paying::{PaidAnswer, PaidPeer, Paying, Reached, delegate_paying, send_paying};
mod parent;

use crate::a2a::peer::{self as a2a, PeerCard};
use crate::a2a::sdk_gaps;
use crate::a2a::wire::{from_wire, to_wire};
use crate::config::A2aEndpoint;
use crate::runtime::surface::{A2A_PROTOCOL_VERSION, COMMAND_EXTENSION};
use a2a_sdk_client::A2AClient;
use a2a_sdk_client::jsonrpc::JsonRpcTransport;
use a2a_sdk_client::middleware::CallInterceptor;
use a2a_sdk_client::transport::ServiceParams;
use a2a_types::{
    A2AError, GetTaskRequest, SendMessageRequest, StreamResponse, TaskState, error_code,
};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// Poll cadence between `GetTask` reads while a remote Task is in flight: short
/// enough that a finished task is picked up promptly, long enough that a long
/// run does not hammer the peer. Bounded above by the per-delegation deadline
/// the caller passes in.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Per-request bound — a single SendMessage/GetTask round trip, so a wedged
/// peer can't hang the connect/read. The overall delegation is separately
/// bounded by the caller's deadline.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a stream may stay silent before it is declared dead and the run
/// recovered over `GetTask`: agentd's listener writes a keep-alive comment
/// every ~15 s, so this is three missed beats.
const STREAM_QUIET: Duration = Duration::from_secs(45);

/// Where a peer publishes its card, on the configured origin.
const CARD_PATH: &str = "/.well-known/agent-card.json";

/// How long a card is trusted when the response names no `max-age`.
const CARD_TTL_DEFAULT: Duration = Duration::from_secs(60);

/// The longest a card is trusted whatever its `max-age` says, so a peer that
/// stops streaming or starts requiring an extension is noticed within minutes.
const CARD_TTL_MAX: Duration = Duration::from_secs(300);

/// The longest a card from THIS host is trusted: a loopback port or a socket
/// path changes hands as instance children come and go, and a card cached for
/// the child that held it last would be applied to the one that holds it now.
/// Re-reading a local card costs one local round trip.
const CARD_TTL_LOCAL: Duration = Duration::from_secs(5);

/// The bound on a card fetch. Discovery is one small GET; a peer that cannot
/// answer it promptly is dialled without its card rather than being allowed to
/// spend the delegation's budget on it.
const CARD_TIMEOUT: Duration = Duration::from_secs(5);

/// The extensions this client implements — the only ones it can honour when a
/// peer's card marks them `required`.
const CLIENT_EXTENSIONS: &[&str] = &[COMMAND_EXTENSION];

/// Poll cadence while the peer's task waits on a person to sign in: a wait of
/// minutes is not read ten times a second.
const AUTH_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What a delegation says while the peer's task waits on a person
/// (`TASK_STATE_AUTH_REQUIRED`, A2A §7.6): that it does, with the peer's own
/// words, and that it went on. agentd passes no credential to the peer: the
/// person authorizes it directly, out of band.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerWait {
    AuthRequired { task: String, text: String },
    Resumed { task: String },
}

/// How a delegation follows the peer's task beyond sending it.
#[derive(Default)]
pub struct Follow<'a> {
    /// Told each time the peer's task enters or leaves `AUTH_REQUIRED`.
    pub watch: Option<&'a (dyn Fn(PeerWait) + Send + Sync)>,
    /// How much longer than the delegation's own deadline a task waiting on
    /// a sign-in is followed; `ZERO` follows it no longer.
    pub auth_timeout: Duration,
    /// A task the peer already holds for this delegation — one a restart
    /// found waiting — followed instead of sending the objective again.
    pub resume: Option<String>,
}

/// The outcome of a remote A2A delegation: either the distillate (a COMPLETED
/// Task's terminal artifact text) or an error observation (a non-completed
/// terminal state, a transport failure, or the deadline). Maps straight onto the
/// `(observation, is_error)` tool-result shape the orchestrator returns.
pub enum DelegateOutcome {
    /// COMPLETED: the concatenated terminal-artifact text (may be empty if the
    /// remote completed with no artifact — still a success).
    Distillate(String),
    /// A non-success terminal state, a transport error, or the deadline — an
    /// observation the model sees as `isError`, never a crash.
    Error(String),
}

/// What a delegation sends, kept whole so the streaming attempt and the unary
/// fallback send the SAME message — one `messageId`, which a deduping peer
/// recognises as one message rather than two.
struct Objective<'a> {
    text: &'a str,
    // A typed command envelope (`{"op": …, …args}`) — sent as the DataPart
    // the peer's `a2a` start nodes match on, deterministically.
    command: Option<&'a Value>,
    output_contract: Option<&'a str>,
    message_id: String,
}

impl Objective<'_> {
    fn params(&self, tenant: Option<&str>, return_immediately: bool) -> Value {
        a2a::send_message_params_cmd(
            self.text,
            self.command,
            self.output_contract,
            &self.message_id,
            tenant,
            return_immediately,
        )
    }
}

/// Delegate `objective` to the remote A2A agent at `endpoint`, bounded by
/// `deadline`: read the peer's card, send, and follow the task to a terminal
/// state (see the module note for the sequence). Every failure path (a refusal
/// by the card, connect, write, read, RPC error, non-completed terminal,
/// deadline) returns [`DelegateOutcome::Error`] — never panics, never hangs
/// (the deadline is the hard backstop).
#[allow(clippy::too_many_arguments)]
pub fn delegate(
    endpoint: &A2aEndpoint,
    auth: PeerAuth,
    objective: &str,
    command: Option<&Value>,
    output_contract: Option<&str>,
    // See [`send`]: a stable id makes a retried delegation attach to the task
    // the first attempt created on a deduping peer, instead of starting a
    // second one.
    message_id: Option<&str>,
    deadline: Instant,
    follow: Follow<'_>,
) -> DelegateOutcome {
    match delegate_with(
        endpoint,
        auth,
        &[],
        objective,
        command,
        output_contract,
        message_id,
        deadline,
        &follow,
    ) {
        (_, Followed::Done(outcome)) => outcome,
        // A peer that will not go on until it is paid has answered a
        // delegation that does not pay.
        (_, Followed::Asked(task)) => {
            DelegateOutcome::Error(crate::a2a::x402::unpaid_error(&a2a::task_id_of(&task)))
        }
    }
}

/// [`delegate`], its connection activating `offered` on every request
/// beyond what the peer's card requires, and handed back with what came of
/// it — `None` when the endpoint gave none.
#[allow(clippy::too_many_arguments)]
fn delegate_with(
    endpoint: &A2aEndpoint,
    auth: PeerAuth,
    offered: &[&str],
    objective: &str,
    command: Option<&Value>,
    output_contract: Option<&str>,
    message_id: Option<&str>,
    deadline: Instant,
    follow: &Follow<'_>,
) -> (Option<HttpConn>, Followed) {
    let ep = match HttpEp::of(endpoint) {
        Ok(ep) => ep,
        Err(e) => return (None, DelegateOutcome::Error(e).into()),
    };
    let mut conn = HttpConn::new(ep, auth);
    conn.offered = offered.iter().map(|u| u.to_string()).collect();
    let objective = Objective {
        text: objective,
        command,
        output_contract,
        message_id: message_id
            .map(str::to_string)
            .unwrap_or_else(mint_message_id),
    };
    let followed = delegate_on(&mut conn, &objective, deadline, follow);
    // A failed delegation drops the card it used, so the next one reads
    // the peer as it is now: the card may be why it failed — a peer
    // that changed its terms, or a local port that changed hands.
    if matches!(followed, Followed::Done(DelegateOutcome::Error(_))) {
        conn.forget_card();
    }
    (Some(conn), followed)
}

/// What following a peer's task came to: its outcome, or the task, as the
/// peer sent it, asking to be paid before it goes on (a2a-x402).
enum Followed {
    Done(DelegateOutcome),
    Asked(Value),
}

impl From<DelegateOutcome> for Followed {
    fn from(o: DelegateOutcome) -> Followed {
        Followed::Done(o)
    }
}

/// [`delegate`] over an open connection: the card, then the send — or, for
/// a task a restart found waiting, the poll of it alone.
fn delegate_on(
    conn: &mut HttpConn,
    objective: &Objective,
    deadline: Instant,
    follow: &Follow<'_>,
) -> Followed {
    // No card is not a refusal: the spec permits configuring a peer directly,
    // and a card is how a client DISCOVERS what a peer offers, not a
    // precondition for talking to it. Such a peer is dialled streaming first,
    // and a `-32004` still lands on the unary path below.
    let streams = match conn.card(deadline) {
        Some(card) => match conn.adopt(&card) {
            Ok(()) => card.streams(),
            Err(e) => return DelegateOutcome::Error(e).into(),
        },
        None => true,
    };
    if let Some(task_id) = &follow.resume {
        let tenant = conn.tenant.clone();
        return poll_task(conn, task_id, tenant.as_deref(), deadline, follow);
    }
    if !streams {
        return delegate_unary(conn, objective, deadline, follow);
    }
    match conn.call_streaming(objective, deadline) {
        Err(e) => DelegateOutcome::Error(e).into(),
        Ok(StreamOutcome::Done(outcome)) => outcome.into(),
        Ok(StreamOutcome::Recover(task_id)) => {
            let tenant = conn.tenant.clone();
            poll_task(conn, &task_id, tenant.as_deref(), deadline, follow)
        }
        // Nothing started: the peer refused the method, not the message. One
        // unary attempt with the same message, and no more — a peer that
        // refuses that too has answered.
        Ok(StreamOutcome::Unsupported) => delegate_unary(conn, objective, deadline, follow),
    }
}

/// The unary delegation: `SendMessage` with `returnImmediately`, then poll the
/// task it names. A `{message}` reply IS the answer — the peer tracked no work
/// — and a reply that is already terminal needs no poll.
fn delegate_unary(
    conn: &mut HttpConn,
    objective: &Objective,
    deadline: Instant,
    follow: &Follow<'_>,
) -> Followed {
    let tenant = conn.tenant.clone();
    let reply = match conn.call(
        "SendMessage",
        objective.params(tenant.as_deref(), true),
        deadline,
    ) {
        Ok(v) => v,
        Err(e) => return DelegateOutcome::Error(e).into(),
    };
    if let Some(a2a::Reply::Message(_)) = a2a::reply_of(&reply) {
        return DelegateOutcome::Distillate(a2a::reply_text_of(&reply)).into();
    }
    if crate::a2a::x402::asks(&reply) {
        return Followed::Asked(reply.get("task").cloned().unwrap_or(reply));
    }
    if let Some(outcome) = terminal_outcome(&reply) {
        return outcome.into();
    }
    let task_id = a2a::task_id_of(&reply);
    if task_id.is_empty() {
        return DelegateOutcome::Error("a2a: SendMessage reply named no task".into()).into();
    }
    poll_task(conn, &task_id, tenant.as_deref(), deadline, follow)
}

/// **Fire-and-forget**: one `SendMessage`, no task await — the `a2a.send` step.
///
/// The distinction from [`delegate`] is the whole point of having both. A
/// delegation is a request/response — it opens a stream, follows the task to a
/// terminal state, and hands back a distillate, which is right when the answer
/// is what you wanted. A send is a NOTIFICATION: the step is done once the peer
/// has accepted the message. That is what lets a workflow tell a peer something
/// and carry on, and it is why a send does not take an `output_contract` — there
/// is no output to shape. It is also why a send reads no card: it is one
/// request, `returnImmediately`, that any 1.0 peer accepts.
///
/// The message is the spec's own `ROLE_USER` message whatever shape `parts`
/// came in (see [`a2a::send_message_params_parts`]); a command part marks it
/// with the command extension and activates that extension in the header.
///
/// The peer's own reply, if there is one, arrives later on the conversation and
/// is picked up by `a2a.wait` (or a `wait {on: message}`), which is the async
/// half a delegation cannot express.
///
/// To [`A2aEndpoint::Parent`] the send goes through the instance child's
/// channel ([`parent`]); to any other peer, through the SDK.
///
/// Errors on connect / write / read / RPC error. Accepting is not the same as
/// succeeding: a peer that answers 200 and then fails internally is invisible
/// here by construction, which is what "fire and forget" means.
pub fn send(
    endpoint: &A2aEndpoint,
    auth: PeerAuth,
    parts: &Value,
    context: Option<&str>,
    // An explicit message id (the step's idempotency key): the A2A spec dedups
    // on `messageId`, so a retry presenting the same id is recognised as the
    // same send by a conforming peer — retry-safety with no new wire field.
    message_id: Option<&str>,
    deadline: Instant,
) -> Result<Value, String> {
    let message_id = message_id
        .map(str::to_string)
        .unwrap_or_else(mint_message_id);
    let params = a2a::send_message_params_parts(parts, context, &message_id, None)?;
    if matches!(endpoint, A2aEndpoint::Parent) {
        return parent::send(params, &auth, deadline);
    }
    let mut conn = HttpConn::new(HttpEp::of(endpoint)?, auth);
    conn.call("SendMessage", params, deadline)
}

/// What came of a message [`send`] delivered, as far as its answer goes:
/// the reply already ([`Sent::Answered`]), or a task still to be read for it
/// ([`Sent::Running`]).
pub enum Sent {
    Answered(DelegateOutcome),
    Running(Box<SentTask>),
}

/// A task a [`send`] started on the peer, read (`GetTask`) one look at a
/// time by whoever follows it to its answer, at the pace they choose.
pub struct SentTask {
    conn: HttpConn,
    params: Value,
    task_id: String,
}

/// The answer to a message [`send`] delivered, once the peer has one. A
/// `{message}` answer is the reply; a task is followed until it ends, or
/// until it stops to ask for input — its status message is the reply then,
/// as it is a person's to answer. The same distillate a delegation hands
/// back, for a send whose caller wants to see the answer too
/// (`subagent.send` to an instance child) without sending it a second time.
pub fn settle_sent(endpoint: &A2aEndpoint, auth: PeerAuth, sent: &Value) -> Sent {
    if let Some(a2a::Reply::Message(_)) = a2a::reply_of(sent) {
        return Sent::Answered(DelegateOutcome::Distillate(a2a::reply_text_of(sent)));
    }
    let task = sent.get("task").unwrap_or(sent);
    if let Some(outcome) = answered(task) {
        return Sent::Answered(outcome);
    }
    let task_id = a2a::task_id_of(sent);
    if task_id.is_empty() {
        return Sent::Answered(DelegateOutcome::Error(
            "a2a: SendMessage reply named no task".into(),
        ));
    }
    let ep = match HttpEp::of(endpoint) {
        Ok(ep) => ep,
        Err(e) => return Sent::Answered(DelegateOutcome::Error(e)),
    };
    Sent::Running(Box::new(SentTask {
        conn: HttpConn::new(ep, auth),
        params: a2a::get_task_params(&task_id, None),
        task_id,
    }))
}

/// A task's answer, when it has one: ended, or asking for input.
fn answered(task: &Value) -> Option<DelegateOutcome> {
    if a2a::task_state_of(task) == TaskState::InputRequired {
        return Some(DelegateOutcome::Distillate(a2a::status_text_of(task)));
    }
    terminal_outcome(task)
}

impl SentTask {
    /// One look at the task: its answer, or `None` while it runs. A read
    /// that failed is the answer: an error.
    pub fn look(&mut self, deadline: Instant) -> Option<DelegateOutcome> {
        match self.conn.call("GetTask", self.params.clone(), deadline) {
            Ok(t) => answered(&t),
            Err(e) => Some(DelegateOutcome::Error(e)),
        }
    }

    /// What a follower that gave up on the task says.
    pub fn still_running(&self) -> DelegateOutcome {
        DelegateOutcome::Error(format!(
            "a2a: no reply yet (task {} still running)",
            self.task_id
        ))
    }
}

/// One `method` call — `SendMessage` or `GetTask` — to the A2A peer at
/// `endpoint`, with `params`: the SDK's answer as the wire carries it, or why
/// there is none. What a thin client of a daemon on this host asks with
/// (`agentd --login company:<name>`).
pub fn call(
    endpoint: &A2aEndpoint,
    auth: PeerAuth,
    method: &str,
    params: Value,
    deadline: Instant,
) -> Result<Value, String> {
    let mut conn = HttpConn::new(HttpEp::of(endpoint)?, auth);
    conn.call(method, params, deadline)
}

/// How a streaming attempt resolved: a terminal outcome; a task id whose
/// terminal state must be RECOVERED over unary `GetTask` (a stream that broke,
/// went quiet or ran into the deadline after the run had started, or stopped
/// at a gate — the run exists on the peer already, so it is polled to a
/// conclusion and never re-sent); or a peer that refused the streaming method
/// itself before anything started.
enum StreamOutcome {
    Done(DelegateOutcome),
    Recover(String),
    Unsupported,
}

/// Poll `GetTask` until the task is terminal or the deadline passes — the
/// shared tail of the unary path and stream recovery.
///
/// A task in `TASK_STATE_AUTH_REQUIRED` waits on a person, who authorizes
/// the peer directly (A2A §7.6.3): it is not an end. It is said to
/// `follow.watch` with the peer's words when it starts and again when it
/// ends, polled less often meanwhile, and followed up to
/// `follow.auth_timeout` past the deadline.
///
/// A task in `TASK_STATE_INPUT_REQUIRED` that asks to be paid (a2a-x402)
/// will not go on by itself: it is handed back as it is.
fn poll_task<C: Caller>(
    conn: &mut C,
    task_id: &str,
    tenant: Option<&str>,
    deadline: Instant,
    follow: &Follow<'_>,
) -> Followed {
    let get_params = a2a::get_task_params(task_id, tenant);
    let mut deadline = deadline;
    let mut extended = false;
    let mut waiting = false;
    loop {
        if Instant::now() >= deadline {
            return DelegateOutcome::Error(if waiting {
                format!(
                    "a2a: the peer's task {task_id} still waits for a sign-in (auth-required), past the step's wait for it"
                )
            } else {
                format!("a2a: delegation to peer timed out (task {task_id} still running)")
            })
            .into();
        }
        let interval = if waiting {
            AUTH_POLL_INTERVAL
        } else {
            POLL_INTERVAL
        };
        // Never asleep past the deadline: the wait ends there, saying why.
        std::thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
        if Instant::now() >= deadline {
            continue;
        }
        let task = match conn.call("GetTask", get_params.clone(), deadline) {
            Ok(t) => t,
            Err(e) => return DelegateOutcome::Error(e).into(),
        };
        if crate::a2a::x402::asks(&task) {
            return Followed::Asked(task);
        }
        let auth = a2a::task_state_of(&task) == TaskState::AuthRequired;
        if auth && !waiting {
            waiting = true;
            if !extended {
                extended = true;
                deadline += follow.auth_timeout;
            }
            if let Some(w) = follow.watch {
                w(PeerWait::AuthRequired {
                    task: task_id.to_string(),
                    text: a2a::status_text_of(&task),
                });
            }
        } else if !auth && waiting {
            waiting = false;
            if let Some(w) = follow.watch {
                w(PeerWait::Resumed {
                    task: task_id.to_string(),
                });
            }
        }
        if let Some(outcome) = terminal_outcome(&task) {
            return outcome.into();
        }
    }
}

/// The client credential presented TO a peer: resolved bearer/framing headers
/// (secrets ALREADY materialized — never logged; this struct deliberately has no
/// `Debug` impl so a stray `{:?}` cannot print one) and/or an mTLS client
/// identity. All empty means an anonymous dial, which is only appropriate for a
/// loopback dev peer.
#[derive(Default)]
pub struct PeerAuth {
    /// Resolved header (name, value) pairs sent on every request. These are
    /// body-INDEPENDENT (a bearer / framing header), baked once at resolution.
    pub headers: Vec<(String, String)>,
    /// The mutual-TLS client identity presented during the handshake.
    pub identity: Option<crate::net::tls::ClientIdentity>,
    /// The span this hop is, when a unit of work makes it: sent as
    /// `traceparent` and `tracestate` on every request of the hop — the card
    /// read, the send, each poll — so an agentd peer continues the caller's
    /// trace. Not a credential: a card is never cached under it.
    pub trace: Option<crate::obs::trace::TraceContext>,
}

/// A one-in-flight-request-at-a-time A2A caller: `call(method, params)` → the
/// `result` Task value or an error string. Implemented by [`HttpConn`] (one
/// SDK call per call); the trait keeps [`poll_task`] testable against a
/// fixture.
trait Caller {
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, String>;
}

/// Map a `Task` value to a terminal [`DelegateOutcome`], or `None` if it is still
/// in flight (the client keeps polling). COMPLETED → the distillate; the other
/// terminal states → a descriptive error observation.
fn terminal_outcome(task: &Value) -> Option<DelegateOutcome> {
    let state = a2a::task_state_of(task);
    if !a2a::is_terminal(&state) {
        return None;
    }
    Some(match state {
        TaskState::Completed => DelegateOutcome::Distillate(a2a::artifact_text_of(task)),
        TaskState::Rejected => {
            DelegateOutcome::Error("a2a: remote agent rejected the objective".into())
        }
        TaskState::Canceled => DelegateOutcome::Error("a2a: remote task was canceled".into()),
        // Failed (and any other terminal mapped here) — the remote run did not
        // reach a clean conclusion.
        _ => DelegateOutcome::Error("a2a: remote task failed".into()),
    })
}

/// A resolved HTTP(S) or unix A2A peer endpoint.
struct HttpEp {
    /// The endpoint as the operator configured it — what the card's
    /// interfaces are matched against, and where a TCP peer is dialled.
    url: String,
    host: String,
    host_header: String,
    tls: bool,
    /// `unix:///path` peer: dial this socket instead of TCP — the co-located
    /// fast lane (no TLS handshake, no TCP stack; the kernel authenticates).
    socket: Option<String>,
}

impl HttpEp {
    /// The dial coordinates of `endpoint`. The parent link is not one: it
    /// carries an instance child's reports, which [`send`] puts through the
    /// channel, and nothing else.
    fn of(endpoint: &A2aEndpoint) -> Result<HttpEp, String> {
        match endpoint {
            A2aEndpoint::Https(url) => HttpEp::parse(url),
            A2aEndpoint::Parent => Err(
                "a2a: the parent link carries this instance's reports (`_instance.result`, `_instance.emit`) with a2a.send, and nothing else"
                    .to_string(),
            ),
        }
    }

    fn parse(url: &str) -> Result<HttpEp, String> {
        if let Some(path) = url
            .strip_prefix("unix://")
            .or_else(|| url.strip_prefix("unix:"))
        {
            if path.is_empty() {
                return Err(format!("a2a: unix peer needs a socket path: {url}"));
            }
            return Ok(HttpEp {
                url: url.to_string(),
                host: String::new(),
                host_header: "localhost".to_string(),
                tls: false,
                socket: Some(path.to_string()),
            });
        }
        let u = crate::net::http::Url::parse(url)
            .map_err(|e| format!("a2a: bad peer url {url}: {e}"))?;
        Ok(HttpEp {
            url: url.to_string(),
            host_header: u.host_header(),
            tls: u.is_tls(),
            host: u.host,
            socket: None,
        })
    }

    /// Whether the peer is on this host: a unix socket or a loopback address.
    fn local(&self) -> bool {
        self.socket.is_some() || crate::net::http::is_loopback_host(&self.host)
    }

    /// The origin a card is published on: the scheme and authority for TCP,
    /// the socket path for unix.
    fn origin(&self) -> String {
        match &self.socket {
            Some(socket) => format!("unix:{socket}"),
            None => format!(
                "{}://{}",
                if self.tls { "https" } else { "http" },
                self.host_header
            ),
        }
    }

    /// The URL the SDK's transport posts to: the configured one, or — over a
    /// unix socket, which the client dials whatever the URL says — the
    /// socket's root.
    fn rpc_url(&self) -> String {
        match &self.socket {
            Some(_) => "http://localhost/".to_string(),
            None => self.url.clone(),
        }
    }

    /// Where the card is read: [`CARD_PATH`] on the configured origin.
    fn card_url(&self) -> String {
        match &self.socket {
            Some(_) => format!("http://localhost{CARD_PATH}"),
            None => format!("{}{CARD_PATH}", self.origin()),
        }
    }
}

/// The cards read so far, by origin and credential ([`HttpConn::card_key`]),
/// with the instant each stops being trusted.
type CardCache = HashMap<String, (Instant, Arc<PeerCard>)>;

/// The process-wide [`CardCache`]. Process-wide because every delegation is
/// its own connection: a per-connection cache would re-read the card on every
/// call.
fn card_cache() -> &'static Mutex<CardCache> {
    static CARDS: OnceLock<Mutex<CardCache>> = OnceLock::new();
    CARDS.get_or_init(Mutex::default)
}

/// How long a card may be reused, from its response's `Cache-Control`:
/// `max-age` capped at [`CARD_TTL_MAX`] ([`CARD_TTL_LOCAL`] for a peer on this
/// host), [`CARD_TTL_DEFAULT`] when none is named, and not at all when the
/// peer says `no-store` or `no-cache`.
fn card_ttl_for(local: bool, cache_control: Option<&str>) -> Duration {
    let ttl = card_ttl(cache_control);
    if local { ttl.min(CARD_TTL_LOCAL) } else { ttl }
}

/// [`card_ttl_for`] a remote peer.
fn card_ttl(cache_control: Option<&str>) -> Duration {
    let mut ttl = CARD_TTL_DEFAULT;
    for directive in cache_control.unwrap_or_default().split(',') {
        let directive = directive.trim().to_ascii_lowercase();
        if directive == "no-store" || directive == "no-cache" {
            return Duration::ZERO;
        }
        if let Some(secs) = directive
            .strip_prefix("max-age=")
            .and_then(|n| n.trim_matches('"').parse::<u64>().ok())
        {
            ttl = Duration::from_secs(secs);
        }
    }
    ttl.min(CARD_TTL_MAX)
}

/// The runtime the SDK's client runs on: this module's own, process-wide, so
/// a blocking caller — a turn worker, a workflow step, a CLI — needs none of
/// its own, and one already inside a runtime never starts a second within it
/// (which panics).
fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RT: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("agentd-a2a-client")
            .build()
            .map_err(|e| format!("a2a: the client's runtime: {e}"))
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Run `fut` on `rt` and wait for it on this thread: spawned rather than
/// `block_on`, which panics on a thread already inside a runtime.
fn block<F>(rt: &tokio::runtime::Runtime, fut: F) -> Result<F::Output, String>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv()
        .map_err(|_| "a2a: the call ended without an answer".to_string())
}

/// A connector layer that notes a connection that could not be made — the
/// one failure that proves a request never left, which a payment's ledger
/// tells from one that left and had no answer ([`Failed::Unsent`]).
#[derive(Clone)]
struct NoteUnreached(Arc<AtomicBool>);

impl<S> tower::Layer<S> for NoteUnreached {
    type Service = Noted<S>;

    fn layer(&self, inner: S) -> Noted<S> {
        Noted {
            inner,
            unreached: Arc::clone(&self.0),
        }
    }
}

/// The connector [`NoteUnreached`] wraps.
#[derive(Clone)]
struct Noted<S> {
    inner: S,
    unreached: Arc<AtomicBool>,
}

impl<S, R> tower::Service<R> for Noted<S>
where
    S: tower::Service<R>,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let connecting = self.inner.call(req);
        let unreached = Arc::clone(&self.unreached);
        Box::pin(async move {
            let conn = connecting.await;
            if conn.is_err() {
                unreached.store(true, Ordering::SeqCst);
            }
            conn
        })
    }
}

/// The headers agentd adds to every request the SDK sends: the extensions it
/// activates, the hop's trace context and the peer credential. (`A2A-Version`
/// is the SDK's own.)
struct Present(Vec<(String, String)>);

#[async_trait::async_trait]
impl CallInterceptor for Present {
    async fn before(&self, _method: &str, params: &mut ServiceParams) -> Result<(), A2AError> {
        for (name, value) in &self.0 {
            params.entry(name.clone()).or_default().push(value.clone());
        }
        Ok(())
    }
}

/// Why an SDK call has no answer.
enum Failed {
    /// It never left: no connection could be made, or no request built.
    Unsent(String),
    /// The bound on the call ran out first.
    TimedOut,
    /// The SDK's error: the peer's own JSON-RPC error, or a reply it could
    /// not read (`INTERNAL_ERROR`).
    Peer(A2AError),
}

impl Failed {
    /// What a caller is told.
    fn say(&self, method: &str) -> String {
        match self {
            Failed::Unsent(e) => e.clone(),
            Failed::TimedOut => format!("a2a: {method}: timed out waiting for the peer"),
            Failed::Peer(e) => format!("a2a: {method} rpc error {}: {}", e.code, e.message),
        }
    }
}

/// An A2A peer as one delegation (or send, or payment) talks to it: the
/// endpoint, the credential, and the terms its card set — each call is one
/// SDK call on a `reqwest` client of its own, one connection per request.
struct HttpConn {
    ep: HttpEp,
    auth: PeerAuth,
    /// The tenant the selected card interface declares, echoed on every call.
    tenant: Option<String>,
    /// Extensions the peer requires (and this client implements), activated
    /// on every request rather than only on the ones that use them: a peer
    /// that requires an extension may refuse any request that lacks it.
    required: Vec<String>,
    /// Extensions this connection's work implements beyond
    /// [`CLIENT_EXTENSIONS`], activated on every request: a step that may pay
    /// offers a2a-x402.
    offered: Vec<String>,
}

impl HttpConn {
    fn new(ep: HttpEp, auth: PeerAuth) -> HttpConn {
        HttpConn {
            ep,
            auth,
            tenant: None,
            required: Vec::new(),
            offered: Vec::new(),
        }
    }

    /// The headers every request carries beyond the SDK's own: the
    /// extensions it activates (those the peer requires, those the work
    /// offers, plus `marked` — the ones the message itself is marked with),
    /// the hop's W3C trace context, and the peer credential.
    fn request_headers(&self, marked: &[String]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut active: Vec<&str> = self.required.iter().map(String::as_str).collect();
        for uri in self.offered.iter().chain(marked) {
            if !active.contains(&uri.as_str()) {
                active.push(uri);
            }
        }
        if !active.is_empty() {
            out.push(("A2A-Extensions".to_string(), active.join(", ")));
        }
        if let Some(t) = &self.auth.trace {
            out.extend(t.headers());
        }
        out.extend(self.auth.headers.iter().cloned());
        out
    }

    /// The `reqwest` client one request goes out on: agentd's TLS
    /// configuration (with the mTLS identity, when there is one), the peer's
    /// socket for a unix peer, no proxy, no connection kept, and a read
    /// bound of `quiet` when one is given. With it, the flag its connector
    /// raises when no connection could be made.
    fn http(&self, quiet: Option<Duration>) -> Result<(reqwest::Client, Arc<AtomicBool>), String> {
        let tls = crate::net::tls::client_config_for(self.auth.identity.as_ref())
            .map_err(|e| format!("a2a: tls to peer {}: {e}", self.ep.origin()))?;
        let unreached = Arc::new(AtomicBool::new(false));
        let mut builder = reqwest::Client::builder()
            .tls_backend_preconfigured((*tls).clone())
            .no_proxy()
            .pool_max_idle_per_host(0)
            .connector_layer(NoteUnreached(Arc::clone(&unreached)));
        if let Some(quiet) = quiet {
            builder = builder.read_timeout(quiet);
        }
        if let Some(socket) = &self.ep.socket {
            #[cfg(unix)]
            {
                builder = builder.unix_socket(std::path::PathBuf::from(socket));
            }
            #[cfg(not(unix))]
            return Err(format!("a2a: unix peer {socket} needs a unix host"));
        }
        let client = builder
            .build()
            .map_err(|e| format!("a2a: http client for peer {}: {e}", self.ep.origin()))?;
        Ok((client, unreached))
    }

    /// The SDK's client for one call, presenting [`Self::request_headers`].
    fn sdk(
        &self,
        marked: &[String],
        quiet: Option<Duration>,
    ) -> Result<(A2AClient<JsonRpcTransport>, Arc<AtomicBool>), String> {
        let (http, unreached) = self.http(quiet)?;
        let present: Arc<dyn CallInterceptor> = Arc::new(Present(self.request_headers(marked)));
        let client = A2AClient::new(JsonRpcTransport::new(http, self.ep.rpc_url()))
            .with_interceptors(vec![present]);
        Ok((client, unreached))
    }

    /// One unary SDK call, bounded by [`request_timeout`].
    fn unary<T, F, Fut>(&self, marked: &[String], deadline: Instant, call: F) -> Result<T, Failed>
    where
        F: FnOnce(A2AClient<JsonRpcTransport>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, A2AError>> + Send + 'static,
        T: Send + 'static,
    {
        let rt = runtime().map_err(Failed::Unsent)?;
        let (sdk, unreached) = self.sdk(marked, None).map_err(Failed::Unsent)?;
        let bound = request_timeout(deadline);
        let out = block(
            rt,
            async move { tokio::time::timeout(bound, call(sdk)).await },
        )
        .map_err(|e| Failed::Peer(A2AError::internal(e)))?;
        match out {
            Err(_) => Err(Failed::TimedOut),
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) if unreached.load(Ordering::SeqCst) => Err(Failed::Unsent(format!(
                "a2a: cannot reach peer {}: {}",
                self.ep.origin(),
                e.message
            ))),
            Ok(Err(e)) => Err(Failed::Peer(e)),
        }
    }

    /// `method` with `params` (as the wire carries them) through the SDK:
    /// its answer, as the wire carries it.
    fn exchange(&self, method: &str, params: Value, deadline: Instant) -> Result<Value, Failed> {
        // The header activates what the message is marked with, and nothing
        // else of the message's own: the mark is the one source.
        let marked = a2a::extensions_of(&params);
        let params = sdk_gaps::outbound(params);
        let unreadable = |e: String| Failed::Unsent(format!("a2a: encode {method}: {e}"));
        let answer = match method {
            "SendMessage" => {
                let req: SendMessageRequest = from_wire(params).map_err(unreadable)?;
                self.unary(&marked, deadline, move |sdk| async move {
                    sdk.send_message(&req).await.map(|r| to_wire(&r))
                })?
            }
            "GetTask" => {
                let req: GetTaskRequest = from_wire(params).map_err(unreadable)?;
                self.unary(&marked, deadline, move |sdk| async move {
                    sdk.get_task(&req).await.map(|t| to_wire(&t))
                })?
            }
            other => {
                return Err(Failed::Unsent(format!(
                    "a2a: {other} is not a method this client sends"
                )));
            }
        };
        Ok(sdk_gaps::whole_numbers(answer))
    }

    /// What this connection's card is cached under — `None` when it is not
    /// cached at all.
    ///
    /// The origin, AND the credential it is read with: a gated peer may show
    /// different callers different cards, so two peers configured on one
    /// origin with different bearers never share one. A card read under a
    /// client certificate is not cached: that identity cannot be told apart
    /// here, and one extra GET is cheaper than handing one identity's card to
    /// another.
    fn card_key(&self) -> Option<String> {
        if self.auth.identity.is_some() {
            return None;
        }
        let mut credential = String::new();
        for (k, v) in &self.auth.headers {
            credential.push_str(&k.to_ascii_lowercase());
            credential.push(':');
            credential.push_str(v);
            credential.push('\n');
        }
        Some(format!(
            "{} {}",
            self.ep.origin(),
            crate::sha::sha256_hex(credential.as_bytes())
        ))
    }

    /// The peer's card: from the cache while it is fresh, otherwise fetched
    /// from the configured origin. `None` when the peer serves none (or none
    /// this client can read), which the caller treats as "dial it anyway".
    fn card(&self, deadline: Instant) -> Option<Arc<PeerCard>> {
        let key = self.card_key();
        let now = Instant::now();
        if let Some(key) = &key
            && let Ok(cache) = card_cache().lock()
            && let Some((until, card)) = cache.get(key)
            && *until > now
        {
            return Some(Arc::clone(card));
        }
        let (card, ttl) = self.fetch_card(deadline)?;
        let card = Arc::new(card);
        if let Some(key) = key
            && !ttl.is_zero()
            && let Ok(mut cache) = card_cache().lock()
        {
            // Expired entries go on every insert, so a daemon that talks to
            // many short-lived instance children does not keep all their cards.
            cache.retain(|_, (until, _)| *until > now);
            cache.insert(key, (now + ttl, Arc::clone(&card)));
        }
        Some(card)
    }

    /// Drop this connection's cached card, so the next delegation reads it
    /// afresh.
    fn forget_card(&self) {
        if let Some(key) = self.card_key()
            && let Ok(mut cache) = card_cache().lock()
        {
            cache.remove(&key);
        }
    }

    /// One `GET` of the card, with the version and the credential every
    /// request carries: a gated peer gates its card too. On the same client
    /// the SDK's calls go out on; the response's `Cache-Control` is read for
    /// how long the card is kept.
    fn fetch_card(&self, deadline: Instant) -> Option<(PeerCard, Duration)> {
        let bound = request_timeout(deadline).min(CARD_TIMEOUT);
        let rt = runtime().ok()?;
        let (http, _) = self.http(None).ok()?;
        let mut headers = vec![
            ("A2A-Version".to_string(), A2A_PROTOCOL_VERSION.to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        headers.extend(self.request_headers(&[]));
        let url = self.ep.card_url();
        let (body, cache_control) = block(rt, async move {
            let read = async move {
                let mut req = http.get(url);
                for (name, value) in &headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                let resp = req.send().await.ok()?;
                if resp.status() != reqwest::StatusCode::OK {
                    return None;
                }
                let cache_control = resp
                    .headers()
                    .get(reqwest::header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let body = resp.bytes().await.ok()?;
                Some((body, cache_control))
            };
            tokio::time::timeout(bound, read).await.ok().flatten()
        })
        .ok()
        .flatten()?;
        let card = PeerCard::parse(&body)?;
        Some((
            card,
            card_ttl_for(self.ep.local(), cache_control.as_deref()),
        ))
    }

    /// Take the card's terms, or refuse them: an interface this client speaks
    /// for this kind of endpoint (its tenant is echoed from here on), and no
    /// required extension this client does not implement.
    fn adopt(&mut self, card: &PeerCard) -> Result<(), String> {
        let interface = card.select_interface(self.ep.socket.is_some(), &self.ep.url)?;
        let mut implemented = CLIENT_EXTENSIONS.to_vec();
        implemented.extend(self.offered.iter().map(String::as_str));
        if let Some(uri) = card.requires_unknown_extension(&implemented) {
            if crate::a2a::x402::EXTENSIONS.contains(&uri) {
                return Err(format!(
                    "a2a: peer requires the extension {uri} (a2a-x402: it charges), and this step has no payment field"
                ));
            }
            return Err(format!(
                "a2a: peer requires the extension {uri}, which agentd does not implement"
            ));
        }
        self.tenant = interface.tenant.clone().filter(|t| !t.is_empty());
        self.required = card.required_extensions().map(str::to_string).collect();
        Ok(())
    }

    /// One `SendStreamingMessage` through the SDK, its frames — working →
    /// (artifact) → final — followed to a terminal outcome. Returns
    /// `Recover(task_id)` when the terminal state must instead be fetched over
    /// unary GetTask: the stream broke, went quiet past [`STREAM_QUIET`] or
    /// ran into the deadline after the run had started, or the task stopped at
    /// a gate. `Unsupported` is a peer that refused the method (`-32004`)
    /// before any task existed. `Err` is a stream that never opened or ended
    /// before naming a task.
    fn call_streaming(
        &mut self,
        objective: &Objective,
        deadline: Instant,
    ) -> Result<StreamOutcome, String> {
        let params = objective.params(self.tenant.as_deref(), false);
        let marked = a2a::extensions_of(&params);
        let req: SendMessageRequest = from_wire(sdk_gaps::outbound(params))
            .map_err(|e| format!("a2a: encode streaming send: {e}"))?;
        let quiet = deadline
            .saturating_duration_since(Instant::now())
            .min(STREAM_QUIET)
            .max(Duration::from_millis(1));
        let rt = runtime()?;
        let (sdk, unreached) = self.sdk(&marked, Some(quiet))?;
        let origin = self.ep.origin();
        let until = tokio::time::Instant::from_std(deadline);
        block(rt, async move {
            let events =
                match tokio::time::timeout_at(until, sdk.send_streaming_message(&req)).await {
                    Err(_) => return Err("a2a: deadline while streaming".to_string()),
                    Ok(Ok(events)) => events,
                    Ok(Err(e)) if e.code == error_code::UNSUPPORTED_OPERATION => {
                        return Ok(StreamOutcome::Unsupported);
                    }
                    Ok(Err(e)) if unreached.load(Ordering::SeqCst) => {
                        return Err(format!("a2a: cannot reach peer {origin}: {}", e.message));
                    }
                    Ok(Err(e)) => {
                        return Err(format!(
                            "a2a: streaming rpc error {}: {}",
                            e.code, e.message
                        ));
                    }
                };
            follow_stream(events, until).await
        })?
    }
}

/// Follow a stream's frames to a [`StreamOutcome`]: statusUpdate frames
/// carry the lifecycle (a terminal state ends it), a completed run's
/// artifactUpdate frames carry the distillate before that — possibly in
/// chunks, possibly as several artifacts, which read as one answer in order.
/// The reads stop at `until`: a peer's keep-alive comments would otherwise
/// hold a quiet stream open past it.
async fn follow_stream(
    mut events: a2a_sdk_client::BoxStream<'static, Result<StreamResponse, A2AError>>,
    until: tokio::time::Instant,
) -> Result<StreamOutcome, String> {
    let mut task_id: Option<String> = None;
    let mut artifacts: Vec<(String, String)> = Vec::new();
    loop {
        let next = match tokio::time::timeout_at(until, events.next()).await {
            Ok(next) => next,
            Err(_) => {
                return match task_id {
                    Some(tid) => Ok(StreamOutcome::Recover(tid)),
                    None => Err("a2a: deadline while streaming".into()),
                };
            }
        };
        let result = match next {
            Some(Ok(frame)) => sdk_gaps::whole_numbers(to_wire(&frame)),
            // The end of the stream: the run may well be alive server-side —
            // recover over GetTask when we know which task it is.
            None => {
                return match task_id {
                    Some(tid) => Ok(StreamOutcome::Recover(tid)),
                    None => Err("a2a: stream ended before any frame".into()),
                };
            }
            Some(Err(e)) => {
                if task_id.is_none() && e.code == error_code::UNSUPPORTED_OPERATION {
                    return Ok(StreamOutcome::Unsupported);
                }
                // A read that broke or went quiet, or a frame the SDK could
                // not read: the run exists, and GetTask says how it is.
                if e.code == error_code::INTERNAL_ERROR
                    && let Some(tid) = task_id
                {
                    return Ok(StreamOutcome::Recover(tid));
                }
                return Ok(StreamOutcome::Done(DelegateOutcome::Error(format!(
                    "a2a: streaming rpc error {}: {}",
                    e.code, e.message
                ))));
            }
        };
        if let Some(update) = result.get("statusUpdate") {
            if let Some(tid) = update.get("taskId").and_then(Value::as_str) {
                task_id = Some(tid.to_string());
            }
            let state = update
                .pointer("/status/state")
                .and_then(Value::as_str)
                .unwrap_or("");
            // A task waiting on someone keeps its stream open (A2A 1.0
            // closes a stream only at a terminal state), but nothing will
            // arrive on it until that someone answers: the wait is the
            // poll's, up to the deadline.
            if waits_on_someone(state)
                && let Some(tid) = &task_id
            {
                return Ok(StreamOutcome::Recover(tid.clone()));
            }
            // Terminate on a terminal task STATE: an A2A stream closes once
            // the task is terminal. agentd emits no `final` flag of its own,
            // and a conformant peer signals termination by the state plus
            // closing the stream, so the state is the only signal to trust.
            if a2a::is_terminal(
                &serde_json::from_value(json!(state)).unwrap_or(TaskState::Unspecified),
            ) {
                let outcome = match state {
                    "TASK_STATE_COMPLETED" if !artifacts.is_empty() => DelegateOutcome::Distillate(
                        artifacts
                            .iter()
                            .map(|(_, t)| t.as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    // Completed but no artifact frame reached us (some
                    // peers order the final status first): the task is
                    // real — recover the artifacts over unary GetTask
                    // instead of calling a finished delegation an error.
                    "TASK_STATE_COMPLETED" => {
                        return Ok(match &task_id {
                            Some(tid) => StreamOutcome::Recover(tid.clone()),
                            None => StreamOutcome::Done(DelegateOutcome::Error(
                                "a2a: remote completed without a distillate artifact".into(),
                            )),
                        });
                    }
                    "TASK_STATE_REJECTED" => {
                        DelegateOutcome::Error("a2a: remote agent rejected the objective".into())
                    }
                    "TASK_STATE_CANCELLED" | "TASK_STATE_CANCELED" => {
                        DelegateOutcome::Error("a2a: remote task was canceled".into())
                    }
                    other => DelegateOutcome::Error(format!("a2a: remote task ended {other}")),
                };
                return Ok(StreamOutcome::Done(outcome));
            }
        } else if let Some(update) = result.get("artifactUpdate") {
            if let Some((artifact_id, append, text)) = a2a::artifact_update_of(update) {
                match artifacts.iter_mut().find(|(a, _)| *a == artifact_id) {
                    Some((_, so_far)) if append => so_far.push_str(&text),
                    Some((_, so_far)) => *so_far = text,
                    None => artifacts.push((artifact_id, text)),
                }
            }
        }
        // A stream that opens with a message is answered by it: the peer
        // tracked no task, and the stream ends there.
        else if let Some(a2a::Reply::Message(_)) =
            a2a::reply_of(&result).filter(|_| task_id.is_none())
        {
            return Ok(StreamOutcome::Done(DelegateOutcome::Distillate(
                a2a::reply_text_of(&result),
            )));
        }
        // A full-Task frame (some peers stream the initial Task) is benign:
        // capture its id and keep reading — unless the task is already
        // waiting on someone, which the poll waits out.
        else if !a2a::task_id_of(&result).is_empty() {
            let tid = a2a::task_id_of(&result);
            let state = result
                .pointer("/task/status/state")
                .or_else(|| result.pointer("/status/state"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if waits_on_someone(state) {
                return Ok(StreamOutcome::Recover(tid));
            }
            task_id = Some(tid);
        }
    }
}

/// Whether a task in `state` (its proto name) is waiting on someone outside it.
fn waits_on_someone(state: &str) -> bool {
    matches!(
        state,
        "TASK_STATE_INPUT_REQUIRED" | "TASK_STATE_AUTH_REQUIRED"
    )
}

impl Caller for HttpConn {
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, String> {
        self.exchange(method, params, deadline)
            .map_err(|f| f.say(method))
    }
}

/// Mint a per-delegation `messageId` (the A2A `Message.messageId`). agentd has no
/// ULID dependency; time-plus-counter is unique enough for one client's run.
pub(crate) fn mint_message_id() -> String {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("a2a-msg-{millis:x}-{n:x}")
}

/// The per-request bound: capped by [`REQUEST_TIMEOUT`] but never longer than
/// the time left to the delegation deadline (and never zero — a tiny floor so
/// the call can at least attempt).
fn request_timeout(deadline: Instant) -> Duration {
    let remaining = deadline.saturating_duration_since(Instant::now());
    remaining.min(REQUEST_TIMEOUT).max(Duration::from_millis(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::thread;

    fn task(id: &str, state: TaskState, artifact: Option<&str>) -> Value {
        let mut t = json!({
            "id": id,
            "contextId": format!("ctx-{id}"),
            "status": { "state": state, "timestamp": "1970-01-01T00:00:00.000Z" },
        });
        if let Some(text) = artifact {
            t["artifacts"] =
                json!([{ "artifactId": format!("{id}.distillate"), "parts": [{ "text": text }] }]);
        }
        t
    }

    /// One request a fixture peer received.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        path: String,
        /// Names lowercased.
        headers: Vec<(String, String)>,
        body: Value,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }

        /// The JSON-RPC method, or `GET <path>` for a plain GET.
        fn call(&self) -> String {
            match self.body["method"].as_str() {
                Some(m) => m.to_string(),
                None => format!("{} {}", self.method, self.path),
            }
        }
    }

    /// How a fixture peer answers one request.
    enum Answer {
        /// One JSON-RPC response (`{"result": …}` or `{"error": …}`); the
        /// version and the request's id are filled in.
        Rpc(Value),
        /// An SSE stream of such responses, after a keep-alive comment the
        /// client must skip.
        Sse(Vec<Value>),
        /// The same, held open afterwards — a keep-alive comment every 50 ms
        /// for the duration — as a peer holds the stream of a task that has
        /// not ended.
        SseHeld(Vec<Value>, Duration),
        /// A plain HTTP response.
        Http(u16, Vec<(&'static str, String)>, String),
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    fn read_request<S: Read>(s: &mut S) -> Option<Seen> {
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).ok()?;
        let mut words = line.split_whitespace();
        let (method, path) = (words.next()?.to_string(), words.next()?.to_string());
        let mut headers = Vec::new();
        let mut len = 0usize;
        loop {
            let mut l = String::new();
            if r.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                if k == "content-length" {
                    len = v.parse().unwrap_or(0);
                }
                headers.push((k, v));
            }
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).ok()?;
        Some(Seen {
            method,
            path,
            headers,
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        })
    }

    fn envelope(req: &Seen, mut response: Value) -> String {
        response["jsonrpc"] = json!("2.0");
        response["id"] = req.body["id"].clone();
        response.to_string()
    }

    fn write_answer<S: Write>(s: &mut S, req: &Seen, answer: Answer) {
        if let Answer::SseHeld(frames, hold) = answer {
            let mut head = String::from(
                "HTTP/1.1 200 X\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: keep-alive\n\n",
            );
            for f in frames {
                head.push_str(&format!("data: {}\n\n", envelope(req, f)));
            }
            let _ = s.write_all(head.as_bytes());
            let _ = s.flush();
            let until = Instant::now() + hold;
            while Instant::now() < until {
                thread::sleep(Duration::from_millis(50));
                if s.write_all(b": keep-alive\n\n")
                    .and_then(|()| s.flush())
                    .is_err()
                {
                    return;
                }
            }
            return;
        }
        let (status, content_type, extra, body) = match answer {
            Answer::Rpc(r) => (200, "application/json", Vec::new(), envelope(req, r)),
            Answer::Sse(frames) => {
                let mut body = String::from(": keep-alive\n\n");
                for f in frames {
                    body.push_str(&format!("data: {}\n\n", envelope(req, f)));
                }
                (200, "text/event-stream", Vec::new(), body)
            }
            Answer::Http(status, headers, body) => (status, "application/json", headers, body),
            Answer::SseHeld(..) => unreachable!("written above"),
        };
        let mut head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let _ = s.write_all(head.as_bytes());
        let _ = s.write_all(body.as_bytes());
        let _ = s.flush();
    }

    /// Record one request, then answer it — in that order, so a test that
    /// inspects the log after the client returns sees every request it made.
    fn handle<S: Read + Write>(
        mut s: S,
        handler: &mut dyn FnMut(&Seen) -> Answer,
        log: &Mutex<Vec<Seen>>,
    ) {
        if let Some(req) = read_request(&mut s) {
            let answer = handler(&req);
            log.lock().unwrap().push(req.clone());
            write_answer(&mut s, &req, answer);
        }
    }

    /// A loopback-TCP A2A peer whose every answer is `handler`'s; returns its
    /// URL and the log of what it was sent.
    fn serve(mut handler: impl FnMut(&Seen) -> Answer + Send + 'static) -> (String, Log) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let log: Log = Arc::default();
        let seen = Arc::clone(&log);
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(s) = conn else { continue };
                s.set_read_timeout(Some(Duration::from_secs(2))).ok();
                handle(s, &mut handler, &seen);
            }
        });
        (url, log)
    }

    /// [`serve`] on a unix socket; the URL is `unix://<path>`.
    fn serve_unix(mut handler: impl FnMut(&Seen) -> Answer + Send + 'static) -> (String, Log) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "agentd-a2a-client-{}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let log: Log = Arc::default();
        let seen = Arc::clone(&log);
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(s) = conn else { continue };
                s.set_read_timeout(Some(Duration::from_secs(2))).ok();
                handle(s, &mut handler, &seen);
            }
        });
        (format!("unix://{}", path.display()), log)
    }

    /// A peer with no card: the well-known path is a 404.
    fn no_card() -> Answer {
        Answer::Http(404, Vec::new(), String::new())
    }

    fn is_card_get(req: &Seen) -> bool {
        req.method == "GET" && req.path == CARD_PATH
    }

    /// Where a fixture card points its interfaces: a port nothing listens
    /// on. A delegation that succeeds against such a card dialled the URL
    /// it was configured with, not the one the card named.
    const ELSEWHERE: &str = "http://127.0.0.1:1/";

    fn card_of(interfaces: Value, streaming: bool, extensions: Value) -> Value {
        json!({
            "name": "fixture",
            "description": "a fixture peer",
            "version": "1",
            "supportedInterfaces": interfaces,
            "capabilities": {"streaming": streaming, "extensions": extensions},
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": [],
        })
    }

    fn interface(binding: &str, version: &str, tenant: &str) -> Value {
        json!({"url": ELSEWHERE, "protocolBinding": binding, "protocolVersion": version, "tenant": tenant})
    }

    /// A non-streaming JSON-RPC 1.0 card.
    fn unary_card() -> Value {
        card_of(json!([interface("JSONRPC", "1.0", "")]), false, json!([]))
    }

    /// The card, marked uncacheable so no test's card outlives it.
    fn serve_card(card: &Value) -> Answer {
        Answer::Http(
            200,
            vec![("Cache-Control", "no-store".to_string())],
            card.to_string(),
        )
    }

    /// A tiny loopback-TCP **HTTP** A2A server fixture whose card says it
    /// does not stream: answers each `POST` with one JSON-RPC reply drawn
    /// from the canned queue (the send first, then the GetTask replies),
    /// clamp-repeating the LAST so a never-terminal task keeps the client
    /// polling until its deadline. (A peer that answers the streaming method
    /// with plain JSON is refused by the SDK, so a unary peer says so on its
    /// card.) A task answers `SendMessage` as the spec's `{"task": …}` and
    /// `GetTask` bare.
    fn serve_http_fixture(replies: Vec<Value>) -> String {
        let mut idx = 0usize;
        serve(move |req| {
            if is_card_get(req) {
                return serve_card(&unary_card());
            }
            let mut result = replies[idx.min(replies.len() - 1)].clone();
            idx += 1;
            if req.body["method"] == "SendMessage" {
                result = json!({ "task": result });
            }
            Answer::Rpc(json!({ "result": result }))
        })
        .0
    }

    /// Like [`serve_http_fixture`] but answers every call with a JSON-RPC
    /// error object (to exercise the peer-error path).
    fn serve_http_error_fixture() -> String {
        serve(|req| {
            if is_card_get(req) {
                return no_card();
            }
            Answer::Rpc(json!({"error": {"code": -32602, "message": "bad params"}}))
        })
        .0
    }

    /// An SSE A2A fixture with no card: answers the FIRST call with a
    /// `text/event-stream` of the given frames (each a StreamResponse result),
    /// then — for recovery tests — every LATER call with unary replies from
    /// `unary` (clamp-repeating the last; an HTTP 500 when there are none, so
    /// a poll that should not happen fails the delegation).
    fn serve_sse_fixture(frames: Vec<Value>, unary: Vec<Value>) -> String {
        let mut first = true;
        let mut idx = 0usize;
        serve(move |req| {
            if is_card_get(req) {
                return no_card();
            }
            if std::mem::take(&mut first) {
                return Answer::Sse(frames.iter().map(|f| json!({ "result": f })).collect());
            }
            if unary.is_empty() {
                return Answer::Http(500, Vec::new(), String::new());
            }
            let result = unary[idx.min(unary.len() - 1)].clone();
            idx += 1;
            Answer::Rpc(json!({ "result": result }))
        })
        .0
    }

    /// A peer whose task waits at a gate keeps its stream open (A2A 1.0
    /// closes one only at a terminal state). The client does not sit on it:
    /// it recovers over GetTask at once and polls — here straight to the
    /// answer.
    #[test]
    fn a_peer_task_at_a_gate_is_polled_not_waited_on() {
        let mut first = true;
        let (url, _) = serve(move |req| {
            if is_card_get(req) {
                return no_card();
            }
            if std::mem::take(&mut first) {
                return Answer::SseHeld(
                    vec![
                        json!({"result": status_frame("g-1", "TASK_STATE_WORKING", false)}),
                        json!({"result": status_frame("g-1", "TASK_STATE_INPUT_REQUIRED", false)}),
                    ],
                    Duration::from_secs(30),
                );
            }
            Answer::Rpc(
                json!({"result": task("g-1", TaskState::Completed, Some("after the gate"))}),
            )
        });
        let ep = A2aEndpoint::parse(&url).unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(20);
        match delegate(
            &ep,
            PeerAuth::default(),
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "after the gate"),
            DelegateOutcome::Error(e) => panic!("expected the polled answer: {e}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited on a held stream: {:?}",
            started.elapsed()
        );
    }

    /// A stream kept alive by comments alone still ends at the delegation's
    /// deadline.
    #[test]
    fn a_stream_held_open_by_keep_alives_ends_at_the_deadline() {
        let mut first = true;
        let (url, _) = serve(move |req| {
            if is_card_get(req) {
                return no_card();
            }
            if std::mem::take(&mut first) {
                return Answer::SseHeld(
                    vec![json!({"result": status_frame("k-1", "TASK_STATE_WORKING", false)})],
                    Duration::from_secs(30),
                );
            }
            Answer::Rpc(json!({"result": task("k-1", TaskState::Working, None)}))
        });
        let ep = A2aEndpoint::parse(&url).unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(2);
        let outcome = delegate(
            &ep,
            PeerAuth::default(),
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "read past the deadline: {:?}",
            started.elapsed()
        );
        assert!(matches!(outcome, DelegateOutcome::Error(_)), "unfinished");
    }

    fn status_frame(id: &str, state: &str, _is_final: bool) -> Value {
        // agentd emits no `final` flag (the A2A proto has none); the client
        // terminates on the terminal task STATE. The bool argument is ignored —
        // it only makes the call sites below read as the lifecycle they describe.
        json!({"statusUpdate": {"taskId": id, "contextId": "ctx", "status": {"state": state}}})
    }

    #[test]
    fn delegate_consumes_an_sse_stream_to_the_distillate_without_polling() {
        let url = serve_sse_fixture(
            vec![
                status_frame("s-1", "TASK_STATE_WORKING", false),
                json!({"artifactUpdate": {"taskId": "s-1", "contextId": "ctx", "artifact": {"artifactId": "s-1.distillate", "parts": [{"text": "streamed answer"}]}, "lastChunk": true}}),
                status_frame("s-1", "TASK_STATE_COMPLETED", true),
            ],
            Vec::new(), // NO unary replies — any poll would fail
        );
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(
            &ep,
            PeerAuth::default(),
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "streamed answer"),
            DelegateOutcome::Error(e) => panic!("expected streamed distillate: {e}"),
        }
    }

    #[test]
    fn a_broken_stream_recovers_over_get_task() {
        // The stream dies after WORKING (no final frame); the client recovers the
        // terminal Task over unary GetTask instead of erroring or re-sending.
        let url = serve_sse_fixture(
            vec![status_frame("s-2", "TASK_STATE_WORKING", false)],
            vec![task("s-2", TaskState::Completed, Some("recovered answer"))],
        );
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(
            &ep,
            PeerAuth::default(),
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "recovered answer"),
            DelegateOutcome::Error(e) => panic!("expected recovery: {e}"),
        }
    }

    #[test]
    fn a_final_failed_stream_frame_is_a_terminal_error() {
        let url = serve_sse_fixture(
            vec![
                status_frame("s-3", "TASK_STATE_WORKING", false),
                status_frame("s-3", "TASK_STATE_FAILED", true),
            ],
            Vec::new(),
        );
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(
            &ep,
            PeerAuth::default(),
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Error(e) => assert!(e.contains("FAILED"), "{e}"),
            DelegateOutcome::Distillate(s) => panic!("expected error, got: {s}"),
        }
    }

    #[test]
    fn delegate_over_http_send_then_poll_returns_the_distillate() {
        // Send → WORKING; GetTask → WORKING then COMPLETED — over HTTP.
        let url = serve_http_fixture(vec![
            task("h-1", TaskState::Working, None),
            task("h-1", TaskState::Working, None),
            task("h-1", TaskState::Completed, Some("http distilled answer")),
        ]);
        let ep = A2aEndpoint::parse(&url).expect("parse https endpoint");
        assert!(matches!(ep, A2aEndpoint::Https(_)));
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(
            &ep,
            PeerAuth::default(),
            "do the work",
            None,
            Some("one line"),
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "http distilled answer"),
            DelegateOutcome::Error(e) => panic!("expected distillate, got error: {e}"),
        }
    }

    #[test]
    fn delegate_presents_the_peer_auth_headers() {
        // The resolved bearer header must be on the wire — on the card read as
        // on the send, since both go to the same configured origin.
        let (url, log) = serve(|req| {
            if is_card_get(req) {
                return serve_card(&unary_card());
            }
            // Reply terminal immediately so the client stops after one call.
            let result = task("h-a", TaskState::Completed, Some("authed"));
            Answer::Rpc(json!({ "result": { "task": result } }))
        });
        let ep = A2aEndpoint::parse(&url).unwrap();
        let auth = PeerAuth {
            headers: vec![("authorization".into(), "Bearer sekrit-token".into())],
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(
            &ep,
            auth,
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "authed"),
            DelegateOutcome::Error(e) => panic!("unexpected error: {e}"),
        }
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        for req in &seen {
            assert_eq!(
                req.header("authorization"),
                Some("Bearer sekrit-token"),
                "the bearer header was presented to the peer on {}",
                req.call()
            );
        }
    }

    #[test]
    fn delegate_over_http_send_message_already_terminal_skips_polling() {
        // A blocking peer returns COMPLETED straight from the send.
        let url = serve_http_fixture(vec![task("h-t", TaskState::Completed, Some("immediate"))]);
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(
            &ep,
            PeerAuth::default(),
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "immediate"),
            DelegateOutcome::Error(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn delegate_over_http_surfaces_a_failed_task() {
        let url = serve_http_fixture(vec![task("h-2", TaskState::Failed, None)]);
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(matches!(
            delegate(
                &ep,
                PeerAuth::default(),
                "obj",
                None,
                None,
                None,
                deadline,
                Follow::default()
            ),
            DelegateOutcome::Error(_)
        ));
    }

    #[test]
    fn delegate_over_http_deadline_while_polling_is_a_timeout() {
        // The task never terminates (WORKING repeats) → give up on the deadline.
        let url = serve_http_fixture(vec![task("h-w", TaskState::Working, None)]);
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        match delegate(
            &ep,
            PeerAuth::default(),
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Error(e) => assert!(e.contains("timed out"), "got: {e}"),
            DelegateOutcome::Distillate(s) => panic!("expected timeout, got: {s}"),
        }
    }

    #[test]
    fn a_deadline_that_runs_out_inside_a_call_is_a_timeout() {
        // SendMessage is answered at once; every later call (GetTask) is held
        // past the deadline, so the deadline always expires mid-read — which
        // must read as a timeout, not as whatever the socket reports.
        let mut first = true;
        let url = serve(move |req| {
            if is_card_get(req) {
                return serve_card(&unary_card());
            }
            let working = task("h-d", TaskState::Working, None);
            if std::mem::take(&mut first) {
                return Answer::Rpc(json!({ "result": { "task": working } }));
            }
            thread::sleep(Duration::from_millis(900));
            Answer::Rpc(json!({ "result": working }))
        })
        .0;
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_millis(400);
        match delegate(
            &ep,
            PeerAuth::default(),
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Error(e) => assert!(e.contains("timed out"), "got: {e}"),
            DelegateOutcome::Distillate(s) => panic!("expected timeout, got: {s}"),
        }
    }

    #[test]
    fn delegate_over_http_surfaces_a_peer_rpc_error() {
        let url = serve_http_error_fixture();
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        match delegate(
            &ep,
            PeerAuth::default(),
            "obj",
            None,
            None,
            None,
            deadline,
            Follow::default(),
        ) {
            DelegateOutcome::Error(e) => assert!(e.contains("rpc error"), "got: {e}"),
            DelegateOutcome::Distillate(s) => panic!("expected error, got: {s}"),
        }
    }

    /// The outbound half of A2A 1.0, case by case: what agentd sends a peer,
    /// and what it reads from the peer's card before it sends anything.
    mod outbound_protocol {
        use super::*;
        use crate::runtime::surface::UNIX_BINDING;

        fn result(v: Value) -> Answer {
            Answer::Rpc(json!({ "result": v }))
        }

        fn unsupported() -> Value {
            json!({"error": {"code": crate::a2a::errors::UNSUPPORTED_OPERATION, "message": "streaming is not supported"}})
        }

        /// A peer with `card` whose sends answer `send` and whose polls answer
        /// a completed task.
        fn peer(card: Option<Value>, send: impl Fn() -> Answer + Send + 'static) -> (String, Log) {
            serve(move |req| {
                if is_card_get(req) {
                    return card.as_ref().map_or_else(no_card, serve_card);
                }
                match req.body["method"].as_str() {
                    Some("GetTask") => result(task("t-1", TaskState::Completed, Some("done"))),
                    _ => send(),
                }
            })
        }

        fn working() -> Answer {
            result(json!({"task": task("t-1", TaskState::Working, None)}))
        }

        fn delegated(url: &str) -> DelegateOutcome {
            let ep = A2aEndpoint::parse(url).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            delegate(
                &ep,
                PeerAuth::default(),
                "obj",
                None,
                None,
                None,
                deadline,
                Follow::default(),
            )
        }

        fn calls(log: &Log) -> Vec<String> {
            log.lock().unwrap().iter().map(Seen::call).collect()
        }

        fn requests(log: &Log, method: &str) -> Vec<Seen> {
            log.lock()
                .unwrap()
                .iter()
                .filter(|r| r.call() == method)
                .cloned()
                .collect()
        }

        /// Every request — the card read, the send, the poll — states the
        /// protocol version.
        fn assert_versioned(log: &Log) {
            for req in log.lock().unwrap().iter() {
                assert_eq!(
                    req.header("a2a-version"),
                    Some(A2A_PROTOCOL_VERSION),
                    "{} carried no A2A-Version: {:?}",
                    req.call(),
                    req.headers
                );
            }
        }

        fn answer_of(outcome: DelegateOutcome) -> String {
            match outcome {
                DelegateOutcome::Distillate(s) => s,
                DelegateOutcome::Error(e) => panic!("expected an answer, got: {e}"),
            }
        }

        fn error_of(outcome: DelegateOutcome) -> String {
            match outcome {
                DelegateOutcome::Error(e) => e,
                DelegateOutcome::Distillate(s) => panic!("expected a refusal, got: {s}"),
            }
        }

        #[test]
        fn every_request_states_the_protocol_version() {
            let (url, log) = peer(Some(unary_card()), working);
            assert_eq!(answer_of(delegated(&url)), "done");
            assert_eq!(
                calls(&log),
                [
                    format!("GET {CARD_PATH}"),
                    "SendMessage".into(),
                    "GetTask".into()
                ]
            );
            assert_versioned(&log);

            let (url, log) = peer(None, || {
                Answer::Sse(vec![
                    json!({"result": {"message": {"role": "ROLE_AGENT", "messageId": "r", "parts": [{"text": "hi"}]}}}),
                ])
            });
            assert_eq!(answer_of(delegated(&url)), "hi");
            assert_eq!(
                calls(&log),
                [format!("GET {CARD_PATH}"), "SendStreamingMessage".into()]
            );
            assert_versioned(&log);
        }

        /// `send` is one typed `ROLE_USER` message with `returnImmediately`,
        /// and reads no card. A command part marks the message with the command
        /// extension and activates it in the header; plain text does neither.
        #[test]
        fn a_send_is_a_typed_user_message_that_returns_immediately() {
            let (url, log) = peer(Some(unary_card()), working);
            let ep = A2aEndpoint::parse(&url).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);

            send(
                &ep,
                PeerAuth::default(),
                &json!("hello"),
                Some("conv-1"),
                Some("m-1"),
                deadline,
            )
            .expect("a plain send is accepted");
            let cmd = json!([{"data": {"agentd": {"op": "stream.forwarded", "seq": 7}}}]);
            send(&ep, PeerAuth::default(), &cmd, None, Some("m-2"), deadline)
                .expect("a command send is accepted");

            assert_eq!(
                calls(&log),
                ["SendMessage", "SendMessage"],
                "a send reads no card"
            );
            assert_versioned(&log);
            let sent = requests(&log, "SendMessage");

            let plain = &sent[0].body["params"];
            assert_eq!(plain["message"]["role"], "ROLE_USER");
            assert_eq!(plain["message"]["messageId"], "m-1");
            assert_eq!(plain["message"]["contextId"], "conv-1");
            assert_eq!(plain["message"]["parts"], json!([{"text": "hello"}]));
            assert_eq!(plain["configuration"]["returnImmediately"], true);
            assert!(plain["message"].get("extensions").is_none(), "{plain}");
            assert_eq!(sent[0].header("a2a-extensions"), None);

            let command = &sent[1].body["params"];
            assert_eq!(command["message"]["role"], "ROLE_USER");
            assert_eq!(command["message"]["extensions"], json!([COMMAND_EXTENSION]));
            // The document travels as written, but for its whole numbers,
            // which the released SDK writes as doubles (`7.0`) and agentd's
            // listener reads back as integers (`sdk_gaps`; 90e2876 upstream).
            assert_eq!(
                sdk_gaps::whole_numbers(command["message"]["parts"].clone()),
                cmd,
                "the document travels as written"
            );
            assert_eq!(
                command["message"]["parts"][0]["data"]["agentd"]["seq"],
                json!(7.0)
            );
            assert_eq!(sent[1].header("a2a-extensions"), Some(COMMAND_EXTENSION));
        }

        /// A delegation's command is marked on the message and activated in
        /// the header, on the streaming send as on the unary one.
        #[test]
        fn a_delegated_command_is_marked_and_activated() {
            let (url, log) = peer(Some(unary_card()), working);
            let ep = A2aEndpoint::parse(&url).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let cmd = json!({"op": "room.ping", "x": "hello"});
            answer_of(delegate(
                &ep,
                PeerAuth::default(),
                "",
                Some(&cmd),
                None,
                None,
                deadline,
                Follow::default(),
            ));
            let sent = requests(&log, "SendMessage");
            assert_eq!(
                sent[0].body["params"]["message"]["extensions"],
                json!([COMMAND_EXTENSION])
            );
            assert_eq!(sent[0].header("a2a-extensions"), Some(COMMAND_EXTENSION));
            // The poll carries no message, so it activates nothing.
            assert_eq!(requests(&log, "GetTask")[0].header("a2a-extensions"), None);
        }

        #[test]
        fn a_card_is_reused_for_its_max_age_and_no_longer_than_five_minutes() {
            let card = unary_card();
            let (url, log) = serve(move |req| {
                if is_card_get(req) {
                    return Answer::Http(
                        200,
                        vec![("Cache-Control", "public, max-age=60".to_string())],
                        card.to_string(),
                    );
                }
                result(json!({"task": task("t-1", TaskState::Completed, Some("done"))}))
            });
            assert_eq!(answer_of(delegated(&url)), "done");
            assert_eq!(answer_of(delegated(&url)), "done");
            assert_eq!(
                calls(&log),
                [
                    format!("GET {CARD_PATH}"),
                    "SendMessage".into(),
                    "SendMessage".into()
                ],
                "the second delegation used the cached card"
            );

            assert_eq!(card_ttl(None), Duration::from_secs(60));
            assert_eq!(card_ttl(Some("public")), Duration::from_secs(60));
            assert_eq!(
                card_ttl(Some("public, max-age=10")),
                Duration::from_secs(10)
            );
            assert_eq!(card_ttl(Some("max-age=86400")), Duration::from_secs(300));
            assert_eq!(card_ttl(Some("no-store")), Duration::ZERO);
            assert_eq!(card_ttl(Some("max-age=60, no-cache")), Duration::ZERO);
        }

        /// A card listing one interface per tenant delivers under the tenant
        /// of the interface the operator configured — not the one listed
        /// first — while the dial still goes to the configured URL.
        #[test]
        fn the_configured_interface_decides_the_tenant() {
            // The fixture's URL is known only once it is bound, so the card
            // names it through a slot filled right after.
            let me: Arc<Mutex<String>> = Arc::default();
            let named = Arc::clone(&me);
            let (url, log) = serve(move |req| {
                if is_card_get(req) {
                    let mine = named.lock().unwrap().clone();
                    return serve_card(&card_of(
                        json!([
                            interface("JSONRPC", "1.0", "t-first"),
                            {"url": format!("{mine}/"), "protocolBinding": "JSONRPC",
                             "protocolVersion": "1.0", "tenant": "t-mine"},
                        ]),
                        false,
                        json!([]),
                    ));
                }
                match req.body["method"].as_str() {
                    Some("GetTask") => result(task("t-1", TaskState::Completed, Some("done"))),
                    _ => working(),
                }
            });
            *me.lock().unwrap() = url.clone();
            assert_eq!(answer_of(delegated(&url)), "done");
            assert_eq!(
                requests(&log, "SendMessage")[0].body["params"]["tenant"],
                "t-mine"
            );
            assert_eq!(
                requests(&log, "GetTask")[0].body["params"]["tenant"],
                "t-mine"
            );
        }

        /// A delegation that fails drops the card it used: the next one reads
        /// the peer as it is now, although the card said it could be kept.
        #[test]
        fn a_failed_delegation_drops_its_card() {
            let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = Arc::clone(&reads);
            let (url, log) = serve(move |req| {
                if is_card_get(req) {
                    // First read: a peer requiring an extension nobody knows.
                    // After that: the peer as it now is.
                    let n = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let extensions = if n == 0 {
                        json!([{"uri": "https://example.com/ext/unknown", "required": true}])
                    } else {
                        json!([])
                    };
                    let card = card_of(json!([interface("JSONRPC", "1.0", "")]), false, extensions);
                    return Answer::Http(
                        200,
                        vec![("Cache-Control", "max-age=60".to_string())],
                        card.to_string(),
                    );
                }
                result(json!({"task": task("t-1", TaskState::Completed, Some("done"))}))
            });
            let e = error_of(delegated(&url));
            assert!(e.contains("https://example.com/ext/unknown"), "{e}");
            assert_eq!(answer_of(delegated(&url)), "done");
            assert_eq!(
                reads.load(std::sync::atomic::Ordering::SeqCst),
                2,
                "the refused card was read again: {:?}",
                calls(&log)
            );
        }

        /// Two callers of one origin with different credentials never share
        /// a card: a gated peer may show them different ones.
        #[test]
        fn a_card_is_cached_per_credential() {
            let card = unary_card();
            let (url, log) = serve(move |req| {
                if is_card_get(req) {
                    return Answer::Http(
                        200,
                        vec![("Cache-Control", "max-age=60".to_string())],
                        card.to_string(),
                    );
                }
                result(json!({"task": task("t-1", TaskState::Completed, Some("done"))}))
            });
            let as_bearer = |b: &str| {
                let ep = A2aEndpoint::parse(&url).unwrap();
                let auth = PeerAuth {
                    headers: vec![("Authorization".into(), format!("Bearer {b}"))],
                    ..Default::default()
                };
                let deadline = Instant::now() + Duration::from_secs(5);
                answer_of(delegate(
                    &ep,
                    auth,
                    "obj",
                    None,
                    None,
                    None,
                    deadline,
                    Follow::default(),
                ))
            };
            as_bearer("alice");
            as_bearer("alice");
            as_bearer("bob");
            let reads = calls(&log).iter().filter(|c| c.starts_with("GET")).count();
            assert_eq!(reads, 2, "one read per credential: {:?}", calls(&log));
        }

        /// A card from this host is kept for seconds, not minutes: the port or
        /// socket it came from may hold another child by the next delegation.
        #[test]
        fn a_local_card_is_kept_for_seconds() {
            assert_eq!(card_ttl_for(true, Some("max-age=60")), CARD_TTL_LOCAL);
            assert_eq!(
                card_ttl_for(false, Some("max-age=60")),
                Duration::from_secs(60)
            );
            assert_eq!(card_ttl_for(true, Some("no-store")), Duration::ZERO);
            assert!(HttpEp::parse("http://127.0.0.1:8080").unwrap().local());
            assert!(HttpEp::parse("unix:///run/a.sock").unwrap().local());
            assert!(!HttpEp::parse("https://agent.example").unwrap().local());
        }

        /// The first interface at JSON-RPC 1.x.0 decides the tenant, which is
        /// echoed on every call; its URL decides nothing — the dial goes to the
        /// configured peer, although the card names a dead port.
        #[test]
        fn the_first_json_rpc_1_0_interface_decides_the_tenant_not_the_dial() {
            let card = card_of(
                json!([
                    interface("GRPC", "1.0", "grpc-tenant"),
                    interface("JSONRPC", "0.3.0", "old-tenant"),
                    interface(UNIX_BINDING, "1.0", "unix-tenant"),
                    interface("JSONRPC", "1.0.1", "t-1"),
                    interface("JSONRPC", "1.0", "t-2"),
                ]),
                false,
                json!([]),
            );
            let (url, log) = peer(Some(card), working);
            assert_eq!(answer_of(delegated(&url)), "done");
            assert_versioned(&log);
            assert_eq!(
                requests(&log, "SendMessage")[0].body["params"]["tenant"],
                "t-1"
            );
            assert_eq!(requests(&log, "GetTask")[0].body["params"]["tenant"], "t-1");

            // A card with no 1.0 JSON-RPC interface is a peer this client
            // cannot speak to: refused, with what it did offer.
            let card = card_of(
                json!([
                    interface("JSONRPC", "0.3.0", ""),
                    interface("GRPC", "1.0", "")
                ]),
                true,
                json!([]),
            );
            let (url, log) = peer(Some(card), working);
            let e = error_of(delegated(&url));
            assert!(e.contains("JSONRPC@0.3.0") && e.contains("GRPC@1.0"), "{e}");
            assert_eq!(
                calls(&log),
                [format!("GET {CARD_PATH}")],
                "nothing was sent"
            );
        }

        #[test]
        fn a_required_extension_agentd_does_not_implement_is_refused_before_sending() {
            let unknown = "https://example.test/ext/unknown/v1";
            let card = card_of(
                json!([interface("JSONRPC", "1.0", "")]),
                true,
                json!([
                    {"uri": COMMAND_EXTENSION},
                    {"uri": unknown, "required": true},
                ]),
            );
            let (url, log) = peer(Some(card), working);
            let e = error_of(delegated(&url));
            assert!(e.contains(unknown), "{e}");
            assert_eq!(
                calls(&log),
                [format!("GET {CARD_PATH}")],
                "nothing was sent"
            );

            // A required extension agentd DOES implement is honoured: activated
            // on every request, the poll included.
            let card = card_of(
                json!([interface("JSONRPC", "1.0", "")]),
                false,
                json!([{"uri": COMMAND_EXTENSION, "required": true}]),
            );
            let (url, log) = peer(Some(card), working);
            assert_eq!(answer_of(delegated(&url)), "done");
            for call in ["SendMessage", "GetTask"] {
                assert_eq!(
                    requests(&log, call)[0].header("a2a-extensions"),
                    Some(COMMAND_EXTENSION),
                    "{call}"
                );
            }
        }

        /// A card that does not say it streams gets `SendMessage` with
        /// `returnImmediately`, then `GetTask` — never the streaming method it
        /// would have to refuse.
        #[test]
        fn a_peer_that_does_not_stream_is_sent_unary_and_polled() {
            let (url, log) = peer(Some(unary_card()), working);
            assert_eq!(answer_of(delegated(&url)), "done");
            assert!(requests(&log, "SendStreamingMessage").is_empty());
            let sent = requests(&log, "SendMessage");
            assert_eq!(sent.len(), 1);
            assert_eq!(
                sent[0].body["params"]["configuration"]["returnImmediately"],
                true
            );
            assert_eq!(sent[0].body["params"]["message"]["role"], "ROLE_USER");
            assert_eq!(requests(&log, "GetTask")[0].body["params"]["id"], "t-1");
        }

        /// A stream refused as unsupported — as a JSON answer or as the first
        /// SSE frame — started nothing, so the same message goes once more,
        /// unary. A peer that refuses that too has answered: no third try.
        #[test]
        fn an_unsupported_stream_falls_back_to_one_unary_send() {
            for refusal in [Answer::Rpc(unsupported()), Answer::Sse(vec![unsupported()])] {
                let mut refusal = Some(refusal);
                let (url, log) = serve(move |req| {
                    if is_card_get(req) {
                        return no_card();
                    }
                    match req.body["method"].as_str() {
                        Some("SendStreamingMessage") => refusal.take().unwrap(),
                        _ => result(
                            json!({"task": task("t-1", TaskState::Completed, Some("unary"))}),
                        ),
                    }
                });
                assert_eq!(answer_of(delegated(&url)), "unary");
                assert_eq!(
                    calls(&log),
                    [
                        format!("GET {CARD_PATH}"),
                        "SendStreamingMessage".into(),
                        "SendMessage".into()
                    ]
                );
                assert_versioned(&log);
                let seen = log.lock().unwrap().clone();
                assert_eq!(
                    seen[1].body["params"]["message"]["messageId"],
                    seen[2].body["params"]["message"]["messageId"],
                    "the fallback sends the same message"
                );
            }

            let (url, log) = peer(None, || Answer::Rpc(unsupported()));
            let e = error_of(delegated(&url));
            assert!(e.contains("-32004"), "{e}");
            assert_eq!(
                requests(&log, "SendMessage").len(),
                1,
                "one unary retry, no more"
            );
        }

        /// Without a card the peer is still dialled — streaming first.
        #[test]
        fn a_peer_without_a_card_is_streamed_to_first() {
            let (url, log) = peer(None, || {
                Answer::Sse(vec![
                    json!({"result": status_frame("t-1", "TASK_STATE_WORKING", false)}),
                    json!({"result": {"artifactUpdate": {"taskId": "t-1", "artifact": {"artifactId": "a", "parts": [{"text": "part one"}]}}}}),
                    json!({"result": {"artifactUpdate": {"taskId": "t-1", "append": true, "artifact": {"artifactId": "a", "parts": [{"text": ", part two"}]}}}}),
                    json!({"result": {"artifactUpdate": {"taskId": "t-1", "artifact": {"artifactId": "b", "parts": [{"data": {"n": 1}}]}}}}),
                    json!({"result": status_frame("t-1", "TASK_STATE_COMPLETED", true)}),
                ])
            });
            assert_eq!(answer_of(delegated(&url)), "part one, part two\n{\"n\":1}");
            assert_eq!(
                calls(&log),
                [format!("GET {CARD_PATH}"), "SendStreamingMessage".into()]
            );
        }

        /// A peer may answer with a message instead of a task; that message is
        /// the answer, and there is no task to poll.
        #[test]
        fn a_message_reply_is_the_answer() {
            let message = json!({"message": {"role": "ROLE_AGENT", "messageId": "r", "parts": [{"data": {"runs": []}}]}});
            let reply = message.clone();
            let (url, log) = peer(Some(unary_card()), move || result(reply.clone()));
            assert_eq!(answer_of(delegated(&url)), "{\"runs\":[]}");
            assert!(
                requests(&log, "GetTask").is_empty(),
                "a message names no task to poll"
            );

            // A peer with no card is streamed to, and answers on its stream.
            let reply = message.clone();
            let (url, log) = peer(None, move || {
                Answer::Sse(vec![json!({ "result": reply.clone() })])
            });
            assert_eq!(answer_of(delegated(&url)), "{\"runs\":[]}");
            assert!(requests(&log, "GetTask").is_empty());
        }

        /// A peer that answers the streaming method with a plain JSON result
        /// is refused by the SDK: the answer is an error, not a poll of a
        /// task the client cannot name.
        #[test]
        fn a_plain_json_answer_to_a_stream_is_refused() {
            let (url, log) = peer(None, working);
            let e = error_of(delegated(&url));
            assert!(e.contains("expected streaming response"), "{e}");
            assert_eq!(
                calls(&log),
                [format!("GET {CARD_PATH}"), "SendStreamingMessage".into()],
                "nothing was sent again"
            );
        }

        /// A unix peer is read by the binding a unix socket declares — and only
        /// by that one: an HTTP peer's unix interface is not one agentd can
        /// dial, and a unix socket is never JSON-RPC-over-HTTP by URL.
        #[test]
        fn a_unix_peer_is_read_by_its_declared_binding() {
            let unix_only = card_of(
                json!([{"url": "unix:///elsewhere.sock", "protocolBinding": UNIX_BINDING, "protocolVersion": "1.0"}]),
                false,
                json!([]),
            );
            let (url, log) = {
                let card = unix_only.clone();
                serve_unix(move |req| {
                    if is_card_get(req) {
                        return serve_card(&card);
                    }
                    result(
                        json!({"task": task("t-1", TaskState::Completed, Some("over the socket"))}),
                    )
                })
            };
            assert_eq!(answer_of(delegated(&url)), "over the socket");
            assert_eq!(
                calls(&log),
                [format!("GET {CARD_PATH}"), "SendMessage".into()]
            );
            assert_versioned(&log);

            let (url, log) = {
                let card = unary_card();
                serve_unix(move |req| {
                    if is_card_get(req) {
                        return serve_card(&card);
                    }
                    result(json!({"task": task("t-1", TaskState::Completed, Some("x"))}))
                })
            };
            let e = error_of(delegated(&url));
            assert!(e.contains(UNIX_BINDING), "{e}");
            assert_eq!(calls(&log), [format!("GET {CARD_PATH}")]);

            let (url, _) = peer(Some(unix_only), working);
            let e = error_of(delegated(&url));
            assert!(e.contains("no JSONRPC interface"), "{e}");
        }

        /// A peer's task waiting on its person (`AUTH_REQUIRED`) is no end:
        /// the delegation is told when the wait starts — in the peer's words
        /// — and when it ends, follows it `auth_timeout` past its own
        /// deadline, and answers with what the task completes with. Resumed,
        /// it polls that task alone and sends the work no second time.
        #[test]
        fn a_task_waiting_on_its_person_is_followed_to_its_end() {
            use std::sync::atomic::{AtomicU64, Ordering};
            let waiting = || {
                let mut t = task("t-1", TaskState::AuthRequired, None);
                t["status"]["message"] = json!({"messageId": "m", "role": "ROLE_AGENT",
                    "parts": [{"text": "sign in at https://peer.example/auth"}]});
                t
            };
            let polls = Arc::new(AtomicU64::new(0));
            let p = Arc::clone(&polls);
            let (url, log) = serve(move |req| {
                if is_card_get(req) {
                    return serve_card(&unary_card());
                }
                match req.body["method"].as_str() {
                    Some("GetTask") if p.fetch_add(1, Ordering::SeqCst) < 2 => result(waiting()),
                    Some("GetTask") => {
                        result(task("t-1", TaskState::Completed, Some("after the sign-in")))
                    }
                    _ => result(json!({"task": waiting()})),
                }
            });
            let seen = Mutex::new(Vec::new());
            let watch = |w: PeerWait| seen.lock().unwrap().push(w);
            let ep = A2aEndpoint::parse(&url).unwrap();
            // Shorter than the wait: the sign-in is what extends it.
            let deadline = Instant::now() + Duration::from_millis(500);
            let follow = Follow {
                watch: Some(&watch),
                auth_timeout: Duration::from_secs(10),
                resume: None,
            };
            let out = delegate(
                &ep,
                PeerAuth::default(),
                "obj",
                None,
                None,
                None,
                deadline,
                follow,
            );
            assert_eq!(answer_of(out), "after the sign-in");
            assert_eq!(
                *seen.lock().unwrap(),
                [
                    PeerWait::AuthRequired {
                        task: "t-1".into(),
                        text: "sign in at https://peer.example/auth".into()
                    },
                    PeerWait::Resumed { task: "t-1".into() }
                ]
            );
            let sent = requests(&log, "SendMessage").len();
            let resumed = Follow {
                resume: Some("t-1".into()),
                ..Follow::default()
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            let out = delegate(
                &ep,
                PeerAuth::default(),
                "obj",
                None,
                None,
                None,
                deadline,
                resumed,
            );
            assert_eq!(answer_of(out), "after the sign-in");
            assert_eq!(requests(&log, "SendMessage").len(), sent);

            // With no time for a sign-in, the wait ends the delegation,
            // saying so.
            polls.store(0, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_millis(300);
            let e = error_of(delegate(
                &ep,
                PeerAuth::default(),
                "obj",
                None,
                None,
                None,
                deadline,
                Follow::default(),
            ));
            assert!(e.contains("still waits for a sign-in"), "{e}");
        }
    }
}
