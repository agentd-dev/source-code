// SPDX-License-Identifier: AGPL-3.0-only
//! **agentd behind the A2A specification's ports.**
//!
//! [`a2a_rs`] models an A2A server as a handful of traits — a message handler, a
//! task lifecycle, a task query, a streaming handler, a notification manager, an
//! agent-card provider — and owns everything above them: method dispatch, the
//! typed request and response shapes, error codes, SSE framing, the blocking-send
//! rule. This module is the *below*: agentd's answers to those traits.
//!
//! The one structural fact to keep in mind is that agentd's runtime is a single
//! blocking reactor, and these traits are `async`. Every port here therefore
//! hands its work to [`A2aBridge`] — post an [`Event::A2a`] to the loop, wait for
//! the reply — on a blocking thread, and awaits that. The reactor stays
//! single-threaded and knows nothing about tokio; the protocol layer stays async
//! and knows nothing about the reactor.
//!
//! Two ports are deliberately refusals rather than implementations: task
//! *creation* and *status updates* are not things a caller may do out of band,
//! because agentd's runtime owns when a task exists and what state it is in.
//! They answer with the spec's own error for "not here" rather than a
//! half-built result.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use a2a_rs::domain::{
    A2AError, ContextId, ListTasksParams, ListTasksResult, Message, Task as WireTask,
    TaskArtifactUpdateEvent, TaskId, TaskPushNotificationConfig, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};
use a2a_rs::port::{
    AsyncMessageHandler, AsyncNotificationManager, AsyncStreamingHandler, AsyncTaskLifecycle,
    AsyncTaskQuery, RequestContext, StreamingSubscriber,
};
use futures_util::TryStreamExt;
use serde_json::{Value, json};

use crate::a2a::Principal;
use crate::a2a::principals::Via;
use crate::runtime::a2a_server::A2aBridge;
use crate::runtime::surface::Active;

/// Everything agentd supplies to the protocol layer, in one value.
///
/// One struct implements every port because they share one back end: the same
/// bridge into the same reactor. Splitting them would only mean cloning the
/// bridge four times.
pub struct RuntimePorts {
    bridge: Arc<A2aBridge>,
    /// The fan-out a2a-rs streams from. agentd's reactor broadcasts into it as
    /// tasks move; the protocol layer turns that into SSE.
    updates: Arc<a2a_rs::adapter::InMemoryStreamingHandler>,
}

impl RuntimePorts {
    pub fn new(
        bridge: Arc<A2aBridge>,
        updates: Arc<a2a_rs::adapter::InMemoryStreamingHandler>,
    ) -> RuntimePorts {
        RuntimePorts { bridge, updates }
    }

    /// Run one reactor round trip without blocking the async runtime.
    ///
    /// The reply is either a result value or agentd's JSON-RPC error object;
    /// the latter is turned back into the spec's error type so the protocol
    /// layer maps it to the right code, rather than being passed off as a
    /// successful result that happens to contain an error — and it is
    /// recorded on the request's [`RequestScope`], because the typed error
    /// cannot carry it whole (see [`record_error`]).
    ///
    /// An error is recognised under either spelling — the reactor's `_error`
    /// marker, or a plain JSON-RPC `error` member. Fail closed: no reply the
    /// runtime answers carries an `error` member as data, so one that does is a
    /// refusal, and reading it as a result would hand a caller whatever else
    /// happened to be in the object.
    async fn call(&self, method: &str, params: Value, who: &Principal) -> Result<Value, A2AError> {
        let v = self.call_raw(method, params, who).await?;
        match error_of(&v) {
            Some(e) => {
                record_error(e);
                Err(from_error_object(e))
            }
            None => Ok(v),
        }
    }

    /// [`Self::call`] without the reading: the reply as the runtime gave it.
    async fn call_raw(
        &self,
        method: &str,
        params: Value,
        who: &Principal,
    ) -> Result<Value, A2AError> {
        let bridge = Arc::clone(&self.bridge);
        let method = method.to_string();
        let who = who.clone();
        // What the request activated crosses with every call made for it, so
        // a task a2a-rs asks for is projected the way the caller asked to see
        // it — on the first frame of a stream as on a unary read.
        let active = serving().map_or(Active::NONE, |s| s.active);
        tokio::task::spawn_blocking(move || bridge.call(&method, params, who, active))
            .await
            .map_err(|e| A2AError::Internal(format!("the runtime call did not complete: {e}")))
    }

    /// The stream fan-out, for the reactor side to publish into.
    pub fn updates(&self) -> Arc<a2a_rs::adapter::InMemoryStreamingHandler> {
        Arc::clone(&self.updates)
    }
}

/// The request being served, as the ports see it.
///
/// The spec's ports take no caller — they were drawn for a server whose store
/// is not per-principal. agentd's is: a task belongs to whoever started it, and
/// a non-operator may only see its own. So the request travels out-of-band,
/// scoped to its tokio task rather than passed down through the port
/// signatures — and so does what the caller meant by it, which a port cannot
/// tell from the arguments a2a-rs hands it.
#[derive(Clone)]
pub struct RequestScope {
    /// Who is calling.
    pub caller: Principal,
    /// Which evidence named them.
    pub via: Via,
    /// The first error object the runtime answered this request with, kept
    /// whole. The ports hand a2a-rs a typed [`A2AError`], and a2a-rs puts
    /// ITS rendering of that on the wire — its own message prefix, its own
    /// `ErrorInfo` under the domain `a2a-rs` — so a refusal agentd's runtime
    /// made (a draining agent, an op nobody may run) would reach the caller
    /// reworded, with the runtime's reason gone. The listener reads this back
    /// and puts the runtime's object on the wire instead
    /// (`serve::dispatch`'s fidelity filter), so a refusal is the same
    /// code, message and data whichever path answered it.
    pub error: Arc<Mutex<Option<Value>>>,
    /// The task the caller's message named (`message.taskId`), when it named
    /// one. a2a-rs hands [`AsyncMessageHandler::process_message`] a task id
    /// either way — the caller's, or one it generated because there was none —
    /// and the two mean opposite things: continue a task that must already
    /// exist, or create one under a server-chosen id. Only the listener saw
    /// which, so it says so here, and a caller-chosen id can never become the
    /// id of a new task.
    pub named_task: Option<String>,
    /// Whether the request is a `SendMessage`/`SendStreamingMessage`. A
    /// push config a2a-rs registers while serving one is the send's own
    /// (`configuration.taskPushNotificationConfig`), for a task that may not
    /// exist yet — see [`Self::pending_push`].
    pub in_send: bool,
    /// The push config a send carried inline. a2a-rs registers it BEFORE it
    /// processes the message, which for a new task is before the task
    /// exists, so the runtime could only answer "not found". It is held here
    /// instead and crosses with the message, and the runtime checks it and
    /// attaches it to the task the message creates or continues.
    pub pending_push: Arc<Mutex<Vec<TaskPushNotificationConfig>>>,
    /// The extensions the request activated (pipeline step 9). The ports
    /// pass it on every runtime call, which projects a task's annotations
    /// only while task-annotations is in it.
    pub active: Active,
}

impl RequestScope {
    /// A scope for `caller`, named by `via`, that activated `active`, with
    /// nothing recorded yet and no send in it (see [`Self::send`]).
    pub fn new(caller: Principal, via: Via, active: Active) -> RequestScope {
        RequestScope {
            caller,
            via,
            error: Arc::default(),
            named_task: None,
            in_send: false,
            pending_push: Arc::default(),
            active,
        }
    }

    /// This scope serving a send whose message named `named_task` (`None`
    /// when it named none).
    pub fn send(mut self, named_task: Option<String>) -> RequestScope {
        self.in_send = true;
        self.named_task = named_task;
        self
    }
}

tokio::task_local! {
    /// The request currently being served.
    ///
    /// Set once by the transport ([`crate::a2a::serve`]) around the whole
    /// dispatch. Unset means nobody is being served, which reads as anonymous —
    /// the role the authorization matrix refuses everything.
    static SCOPE: RequestScope;

    /// The tasks this request has *proved* its caller may watch.
    ///
    /// The spec's streaming port takes a task id and nothing else — no caller,
    /// no context — so the fan-out cannot tell one principal's task from
    /// another's, and attaching to an id is otherwise attaching to whatever
    /// that id names. Ownership lives in the reactor, and the reactor already
    /// answers the question on every task read, so the answer is recorded here
    /// as the request goes past and read back at the one place a subscription
    /// is made ([`SharedStreaming::combined_update_stream`]).
    ///
    /// Scoped alongside [`SCOPE`], per request; the entries never outlive it.
    static STREAMABLE: StreamAuthz;
}

/// One request's stream-authorization ledger: task id ⇒ may this caller see it.
///
/// Shared (`Arc`) rather than owned because a subscription outlives the request
/// that opened it — the SSE body is polled long after the handler returned — and
/// a send's verdict does not exist yet when its subscription is made.
#[derive(Clone, Default)]
struct StreamAuthz {
    seen: Arc<Mutex<HashMap<String, bool>>>,
}

impl StreamAuthz {
    /// Record the reactor's verdict on one task.
    ///
    /// Every read is a verdict, a send's included: a2a-rs reads only the task
    /// a send NAMES (to learn its context), and a named task must already
    /// exist and be the caller's — a send never creates a task under an id the
    /// caller chose. A new task's id is generated and never read before the
    /// message is processed, so its verdict comes from
    /// [`AsyncMessageHandler::process_message`].
    fn record(&self, task_id: &str, allowed: bool) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(task_id.to_string(), allowed);
        }
    }

    /// The verdict, or `None` for "this request has not asked yet".
    fn verdict(&self, task_id: &str) -> Option<bool> {
        self.seen
            .lock()
            .ok()
            .and_then(|seen| seen.get(task_id).copied())
    }
}

/// The ledger of the request being served. `None` means no request is — which
/// is nobody's subscription, and so grants nothing.
fn streamable() -> Option<StreamAuthz> {
    STREAMABLE.try_with(StreamAuthz::clone).ok()
}

/// Run `f` as one request's handling, in `scope`.
pub async fn with_request<F, T>(scope: RequestScope, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    SCOPE
        .scope(scope, STREAMABLE.scope(StreamAuthz::default(), f))
        .await
}

/// The caller of the request being served.
pub fn caller() -> Principal {
    SCOPE
        .try_with(|s| s.caller.clone())
        .unwrap_or_else(|_| Principal::anonymous())
}

/// Keep `error` — a runtime error object, `{code, message, data?}` — as the
/// answer to the request being served, unless one is kept already.
///
/// The first write wins because the first refusal is the one that ended the
/// request: a port that fails returns at once, and a2a-rs stops there. Outside
/// a request it is dropped; there is nobody to answer.
pub(crate) fn record_error(error: &Value) {
    let _ = SCOPE.try_with(|s| {
        if let Ok(mut kept) = s.error.lock()
            && kept.is_none()
        {
            *kept = Some(error.clone());
        }
    });
}

/// The request being served, when one is.
fn serving() -> Option<RequestScope> {
    SCOPE.try_with(RequestScope::clone).ok()
}

/// A reactor reply's error object, under either spelling (see
/// [`RuntimePorts::call`]).
pub(crate) fn error_of(v: &Value) -> Option<&Value> {
    v.get("_error").or_else(|| v.get("error"))
}

/// A reactor reply, split into a result or the spec's error, recording
/// nothing — for a reply read outside a request.
#[cfg(test)]
fn reply_of(v: Value) -> Result<Value, A2AError> {
    match error_of(&v) {
        Some(e) => Err(from_error_object(e)),
        None => Ok(v),
    }
}

/// The reactor's error, read back as the spec's error type.
///
/// The reactor marks a failed answer with an `_error` member rather than
/// returning a `Result`, because its reply channel carries one JSON value. The
/// codes it uses are already the spec's, so this is a mapping and not a
/// translation — and going through the typed error is what makes the protocol
/// layer emit the right JSON-RPC code instead of passing an error off as a
/// successful result that happens to contain one.
///
/// The variants are the ones a2a-rs itself branches on (a subscribe treats
/// "not found" differently from any other failure, for one). Every other code
/// — agentd's own `-31401`/`-31403`, a spec code this table has no variant
/// for — travels as [`A2AError::JsonRpc`] with its number and `data` intact,
/// rather than collapsing into an internal error that would tell the caller
/// the wrong thing.
pub(crate) fn from_error_object(e: &Value) -> A2AError {
    use crate::a2a::errors as code;
    let n = e
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or(code::INTERNAL_ERROR);
    let msg = e
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("internal error")
        .to_string();
    match n {
        code::TASK_NOT_FOUND => A2AError::TaskNotFound(msg),
        code::TASK_NOT_CANCELABLE => A2AError::TaskNotCancelable(msg),
        code::PUSH_NOTIFICATION_NOT_SUPPORTED => A2AError::PushNotificationNotSupported,
        code::UNSUPPORTED_OPERATION => A2AError::UnsupportedOperation(msg),
        code::CONTENT_TYPE_NOT_SUPPORTED => A2AError::ContentTypeNotSupported(msg),
        code::EXTENDED_AGENT_CARD_NOT_CONFIGURED => {
            A2AError::AuthenticatedExtendedCardNotConfigured
        }
        code::METHOD_NOT_FOUND => A2AError::MethodNotFound(msg),
        code::INVALID_PARAMS => A2AError::InvalidParams(msg),
        other => A2AError::JsonRpc {
            code: i32::try_from(other).unwrap_or(code::INTERNAL_ERROR as i32),
            message: msg,
            data: e.get("data").cloned(),
        },
    }
}

/// Read a `Task` out of a reactor reply, which may be the task itself or the
/// `{task}` envelope a send answers with.
///
/// A task with no id is not a task. The spec's type defaults every field, so
/// `null` — or any object that is not a task at all — deserializes into an
/// empty one; accepting it would answer a caller with a blank `Task` and call
/// that success. Refused instead, as the internal error it is.
fn task_from(v: Value) -> Result<WireTask, A2AError> {
    let body = match v.get("task") {
        Some(t) => t.clone(),
        None => v,
    };
    let t: WireTask = serde_json::from_value(body).map_err(A2AError::JsonParse)?;
    if t.id.is_empty() {
        return Err(A2AError::Internal(
            "the runtime answered without a task".to_string(),
        ));
    }
    Ok(t)
}

#[async_trait::async_trait]
impl AsyncMessageHandler for RuntimePorts {
    /// A message becomes runtime work: a conversation turn, or — when it carries
    /// agentd's command DataPart — a registry action. Which one it is, and the
    /// durable task that results, is the reactor's decision; this only carries
    /// the message across, with what the caller meant by it.
    ///
    /// `task_id` is never empty: a2a-rs generates one when the message named
    /// none, and it has already subscribed to that id. So the reactor is told
    /// which it is (`newTask`) — a new task takes exactly this id, and a named
    /// one must already exist — and a push config the send carried inline
    /// crosses with it (see [`RequestScope::pending_push`]).
    async fn process_message(
        &self,
        task_id: &str,
        message: &Message,
        ctx: &RequestContext,
    ) -> Result<WireTask, A2AError> {
        // The context carries the same principal; the task-local is the one
        // source, so a port that has no context reads the same value.
        let _ = ctx;
        let who = caller();
        let scope = serving();
        let named = scope.as_ref().and_then(|s| s.named_task.clone());
        let mut params = json!({
            "message": serde_json::to_value(message).map_err(A2AError::JsonParse)?,
            "taskId": task_id,
            "newTask": named.is_none(),
        });
        let push = scope
            .as_ref()
            .and_then(|s| s.pending_push.lock().ok()?.pop());
        if let Some(push) = push {
            params["push"] = serde_json::to_value(push).map_err(A2AError::JsonParse)?;
        }
        // The task that answers must be the task a2a-rs is watching: it
        // subscribed to `task_id` before this call, and a send answered with
        // any other task would settle — or stream — someone else's.
        let task = same_task(
            task_id,
            task_from(self.call("SendMessage", params, &who).await?)?,
        )?;
        // The task the reactor made or continued for this message is the
        // caller's — and it is the only task this send authorizes. See
        // [`STREAMABLE`].
        if let Some(seen) = streamable() {
            seen.record(&task.id, true);
        }
        Ok(task)
    }
}

#[async_trait::async_trait]
impl AsyncTaskLifecycle for RuntimePorts {
    async fn create(&self, _id: &TaskId, _context_id: &ContextId) -> Result<WireTask, A2AError> {
        // A task exists because the runtime started work, never because a caller
        // asked for an empty one. `SendMessage` is the way in.
        Err(A2AError::UnsupportedOperation(
            "agentd creates tasks from messages; there is no out-of-band create".to_string(),
        ))
    }

    async fn get(&self, id: &TaskId, history_length: Option<u32>) -> Result<WireTask, A2AError> {
        let who = caller();
        let got = self
            .call("GetTask", json!({"id": id.as_str()}), &who)
            .await
            .and_then(task_from)
            .and_then(|t| same_task(id.as_str(), t));
        // The reactor answers a read with the ownership matrix already applied —
        // somebody else's task is "not found", so existence is not disclosed —
        // which makes this verdict exactly the one a subscription needs. A
        // `SubscribeToTask` reads the task before it attaches, and so does a
        // send that names one, so recording it here is what lets the attach
        // refuse. See [`STREAMABLE`]. Only the task that was asked for is
        // proof: a reply naming any other id grants nothing, so a wrong answer
        // can never open a stream.
        if let Some(seen) = streamable() {
            seen.record(id.as_str(), got.is_ok());
        }
        let mut t = got?;
        if let Some(n) = history_length {
            t = t.with_limited_history(Some(n));
        }
        Ok(t)
    }

    async fn update_status(
        &self,
        _id: &TaskId,
        _state: TaskState,
        _message: Option<Message>,
    ) -> Result<WireTask, A2AError> {
        // The runtime owns state. A caller that wants a task stopped cancels it.
        Err(A2AError::UnsupportedOperation(
            "task state follows the work; it is not settable from outside".to_string(),
        ))
    }

    async fn cancel(&self, id: &TaskId) -> Result<WireTask, A2AError> {
        let who = caller();
        let v = self
            .call("CancelTask", json!({"id": id.as_str()}), &who)
            .await?;
        same_task(id.as_str(), task_from(v)?)
    }

    async fn exists(&self, id: &TaskId) -> Result<bool, A2AError> {
        match self.get(id, None).await {
            Ok(_) => Ok(true),
            Err(A2AError::TaskNotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// The task a reply is about must be the task that was asked about. Fail
/// closed: an answer for some other id is an internal error, never a result.
fn same_task(want: &str, t: WireTask) -> Result<WireTask, A2AError> {
    if t.id == want {
        Ok(t)
    } else {
        Err(A2AError::Internal(format!(
            "the runtime answered for task {:?}, not {want:?}",
            t.id
        )))
    }
}

#[async_trait::async_trait]
impl AsyncTaskQuery for RuntimePorts {
    /// One page of the tasks the caller may see. The request crosses whole —
    /// every filter, the page and the projection are the reactor's to apply,
    /// because only the reactor knows who owns what, and a parameter dropped
    /// here would be a filter silently not applied.
    async fn list(&self, params: &ListTasksParams) -> Result<ListTasksResult, A2AError> {
        let who = caller();
        let req = serde_json::to_value(params).map_err(A2AError::JsonParse)?;
        let v = self.call("ListTasks", req, &who).await?;
        serde_json::from_value(v).map_err(A2AError::JsonParse)
    }
}

/// Push notifications: a caller registers a webhook and is told about its task
/// instead of watching it.
///
/// The register/read/delete half is here; the *delivery* half is
/// [`StreamSink::push`], fired from the reactor where every transition passes.
/// Both are refused unless `a2a.push.enabled` — the URL comes from a peer, so
/// making the request at all is the operator's decision (see
/// [`crate::a2a::push`]).
#[async_trait::async_trait]
impl AsyncNotificationManager for RuntimePorts {
    async fn set_config(
        &self,
        config: &TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        // A send's inline config: held for the message, which carries it to
        // the runtime (see [`RequestScope::pending_push`]). Nothing is
        // registered unless the message is accepted, so a refused send leaves
        // no webhook behind on a task it never reached.
        if let Some(scope) = serving().filter(|s| s.in_send) {
            if let Ok(mut pending) = scope.pending_push.lock() {
                pending.push(config.clone());
            }
            return Ok(config.clone());
        }
        let who = caller();
        // The spec's request is the config itself, and it crosses as it came.
        let params = serde_json::to_value(config).map_err(A2AError::JsonParse)?;
        let v = self.call("PushConfigSet", params, &who).await?;
        serde_json::from_value(v).map_err(A2AError::JsonParse)
    }

    async fn get_config(
        &self,
        params: &a2a_rs::domain::GetTaskPushNotificationConfigParams,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let who = caller();
        // a2a-rs names the task `id` and the config `pushNotificationConfigId`;
        // the runtime takes the spec's own request spelling, `{taskId, id}`.
        // An absent config id arrives as empty, and the runtime refuses it.
        let req = json!({
            "taskId": params.id,
            "id": params.push_notification_config_id.clone().unwrap_or_default(),
        });
        let v = self.call("PushConfigGet", req, &who).await?;
        serde_json::from_value(v).map_err(A2AError::JsonParse)
    }

    async fn list_configs(
        &self,
        params: &a2a_rs::domain::ListTaskPushNotificationConfigsParams,
    ) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
        // a2a-rs hands this port the task id alone — the request's `pageSize`
        // and `pageToken` do not reach it, and its answer can carry no
        // `nextPageToken` — which is why the listener answers the wire method
        // itself (`serve::dispatch`) and a caller's paging never comes here.
        // Whatever still does gets a listing that is complete rather than
        // silently short: the port walks every page and answers with all of
        // them.
        let who = caller();
        let mut out: Vec<TaskPushNotificationConfig> = Vec::new();
        let mut token = String::new();
        loop {
            let req = json!({
                "taskId": params.id,
                "pageSize": crate::a2a::wire::MAX_PAGE_SIZE,
                "pageToken": token,
            });
            let v = self.call("PushConfigList", req, &who).await?;
            let page: Vec<TaskPushNotificationConfig> =
                serde_json::from_value(v["configs"].clone()).map_err(A2AError::JsonParse)?;
            out.extend(page);
            match v["nextPageToken"].as_str() {
                Some("") => return Ok(out),
                // A cursor that does not move would loop forever.
                Some(next) if next != token => token = next.to_string(),
                _ => {
                    return Err(A2AError::Internal(
                        "the runtime's push-config listing did not advance".to_string(),
                    ));
                }
            }
        }
    }

    async fn delete_config(
        &self,
        params: &a2a_rs::domain::DeleteTaskPushNotificationConfigParams,
    ) -> Result<(), A2AError> {
        let who = caller();
        let req = json!({
            "taskId": params.id,
            "id": params.push_notification_config_id,
        });
        self.call("PushConfigDelete", req, &who).await?;
        Ok(())
    }
}

/// The streaming half: a2a-rs's own in-memory fan-out, shared.
///
/// agentd adds one thing to it — authorization. Every subscriber, replay buffer
/// and stream-termination rule is the protocol layer's, and the reactor
/// publishes transitions in through [`StreamSink`]; but the fan-out is keyed by
/// task id alone, and agentd's tasks belong to principals. Attaching is
/// therefore gated on the same ownership the task reads enforce (see
/// [`STREAMABLE`]) — without that gate, naming another principal's task id
/// would be enough to watch its transitions and, with a `Last-Event-ID`, to
/// replay its result artifact.
///
/// The type exists at all because the adapter takes the handler by value while
/// the reactor needs a handle to the same one.
pub struct SharedStreaming(pub Arc<a2a_rs::adapter::InMemoryStreamingHandler>);

#[async_trait::async_trait]
impl AsyncStreamingHandler for SharedStreaming {
    async fn add_status_subscriber(
        &self,
        task_id: &str,
        subscriber: Box<dyn StreamingSubscriber<TaskStatusUpdateEvent> + Send + Sync>,
    ) -> Result<String, A2AError> {
        self.0.add_status_subscriber(task_id, subscriber).await
    }
    async fn add_artifact_subscriber(
        &self,
        task_id: &str,
        subscriber: Box<dyn StreamingSubscriber<TaskArtifactUpdateEvent> + Send + Sync>,
    ) -> Result<String, A2AError> {
        self.0.add_artifact_subscriber(task_id, subscriber).await
    }
    async fn remove_subscription(&self, subscription_id: &str) -> Result<(), A2AError> {
        self.0.remove_subscription(subscription_id).await
    }
    async fn remove_task_subscribers(&self, task_id: &str) -> Result<(), A2AError> {
        self.0.remove_task_subscribers(task_id).await
    }
    async fn get_subscriber_count(&self, task_id: &str) -> Result<usize, A2AError> {
        self.0.get_subscriber_count(task_id).await
    }
    async fn broadcast_status_update(
        &self,
        task_id: &str,
        update: TaskStatusUpdateEvent,
    ) -> Result<(), A2AError> {
        self.0.broadcast_status_update(task_id, update).await
    }
    async fn broadcast_artifact_update(
        &self,
        task_id: &str,
        update: TaskArtifactUpdateEvent,
    ) -> Result<(), A2AError> {
        self.0.broadcast_artifact_update(task_id, update).await
    }
    async fn status_update_stream(
        &self,
        task_id: &str,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<TaskStatusUpdateEvent, A2AError>> + Send>,
        >,
        A2AError,
    > {
        self.0.status_update_stream(task_id).await
    }
    async fn artifact_update_stream(
        &self,
        task_id: &str,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<TaskArtifactUpdateEvent, A2AError>> + Send>,
        >,
        A2AError,
    > {
        self.0.artifact_update_stream(task_id).await
    }
    /// The one place a subscription is made — every streaming method in the
    /// protocol layer arrives here — and so the one place ownership is checked.
    ///
    /// The two ways in reach it from opposite directions, which is why the
    /// answer is given in two ways:
    ///
    /// * A **subscribe** (`SubscribeToTask`) reads the task first, so the
    ///   verdict is already in. A caller that may not read the task may not
    ///   watch it either, and it is refused with the same "not found" the read
    ///   gave it — a non-owner must not learn from the difference that the task
    ///   exists.
    /// * A **send** attaches *before* the message is processed, deliberately, so
    ///   that a task settling immediately cannot be missed. A send that named
    ///   its task has had it read already, like a subscribe. A new task's id
    ///   was generated a moment ago and names nothing yet, so the subscription
    ///   is made anyway and the verdict applied at the first poll, by which
    ///   time the send has recorded the task it created under that id.
    ///   Anything still unproved by then delivers nothing.
    async fn combined_update_stream(
        &self,
        task_id: &str,
        from_event_id: Option<u64>,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<a2a_rs::port::SeqEvent, A2AError>> + Send>,
        >,
        A2AError,
    > {
        let seen = streamable();
        if seen.as_ref().and_then(|s| s.verdict(task_id)) == Some(false) {
            return Err(A2AError::TaskNotFound(task_id.to_string()));
        }
        let inner = self
            .0
            .combined_update_stream(task_id, from_event_id)
            .await?;
        if seen.as_ref().and_then(|s| s.verdict(task_id)) == Some(true) {
            return Ok(inner);
        }
        let id = task_id.to_string();
        Ok(Box::pin(
            futures_util::stream::once(async move {
                match seen.and_then(|s| s.verdict(&id)) {
                    Some(true) => Ok(inner),
                    _ => Err(A2AError::TaskNotFound(id)),
                }
            })
            .try_flatten(),
        ))
    }
}

/// Where the reactor publishes a task transition so subscribers see it.
///
/// The reactor is synchronous and the fan-out is async, so this holds a handle
/// to the runtime that owns the listener and drives one short task per event.
/// It is the only place the two directions meet.
pub struct StreamSink {
    updates: Arc<a2a_rs::adapter::InMemoryStreamingHandler>,
    /// The replay log under `updates` — the same one: a clone shares it. The
    /// fan-out gives no handle to its log, and forgetting a task needs one.
    events: a2a_rs::adapter::InMemoryEventLog,
    handle: tokio::runtime::Handle,
    log: crate::obs::log::Logger,
}

impl StreamSink {
    /// A sink publishing into `updates`, which must fan out over `events`
    /// (`StreamingFanout::over(events.clone())`).
    pub fn new(
        updates: Arc<a2a_rs::adapter::InMemoryStreamingHandler>,
        events: a2a_rs::adapter::InMemoryEventLog,
        handle: tokio::runtime::Handle,
        log: crate::obs::log::Logger,
    ) -> StreamSink {
        StreamSink {
            updates,
            events,
            handle,
            log,
        }
    }

    /// Drop everything the stream side holds for a task retention dropped:
    /// its replay ring — up to 256 events, its result artifact among them —
    /// and its channel. The in-memory log keeps a finished task's ring until
    /// told otherwise, so without this `store.retention.tasks` would bound
    /// the task map and leave the larger half of every task in memory.
    pub fn forget(&self, task_id: &str) {
        let updates = Arc::clone(&self.updates);
        let events = self.events.clone();
        let id = task_id.to_string();
        self.handle.spawn(async move {
            use a2a_rs::port::AsyncEventLog;
            let _ = events.discard(&id).await;
            let _ = updates.remove_task_subscribers(&id).await;
        });
    }

    /// Publish a task's status as it now is.
    pub fn status(&self, task: &crate::a2a::tasks::Task) {
        let ev = crate::a2a::wire::status_event(task);
        self.spawn_status(task.id.clone(), ev);
    }

    /// Deliver this task's state to every webhook registered on it, as the
    /// `StreamResponse` the spec says a webhook receives (see
    /// [`crate::a2a::wire::push_body`]).
    ///
    /// Best-effort and off the reactor: a webhook that is down, slow, or now
    /// pointing somewhere it should not must not affect the task it is
    /// reporting on. Each delivery is one blocking POST on the blocking pool.
    pub fn push(&self, task: &crate::a2a::tasks::Task, allow_private: bool) {
        let event = crate::a2a::wire::push_body(task);
        for target in &task.push {
            let target = target.clone();
            let event = event.clone();
            let task_id = task.id.clone();
            let log = self.log.clone();
            self.handle.spawn(async move {
                let outcome = tokio::task::spawn_blocking(move || {
                    crate::a2a::push::deliver(&target, &event, allow_private)
                })
                .await;
                if let Ok(Err(e)) = outcome {
                    log.warn(
                        "a2a.push.failed",
                        serde_json::json!({"task": task_id, "err": e}),
                    );
                }
            });
        }
    }

    /// Publish a delivered artifact.
    pub fn artifact(&self, task_id: &str, context_id: &str, artifact: a2a_rs::domain::Artifact) {
        let ev = crate::a2a::wire::artifact_event(task_id, context_id, artifact, true);
        let updates = Arc::clone(&self.updates);
        let id = task_id.to_string();
        self.handle.spawn(async move {
            let _ = updates.broadcast_artifact_update(&id, ev).await;
        });
    }

    fn spawn_status(&self, id: String, ev: TaskStatusUpdateEvent) {
        let updates = Arc::clone(&self.updates);
        self.handle.spawn(async move {
            let _ = updates.broadcast_status_update(&id, ev).await;
        });
    }
}

/// The `TaskStatus` a status event carries, for callers that want to inspect one
/// before publishing (the reactor logs on terminal transitions).
pub fn status_of(ev: &TaskStatusUpdateEvent) -> &TaskStatus {
    &ev.status
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read that finds nothing is a refusal, in a send as anywhere: a send
    /// reads only the task it names, and a named task that is not there is
    /// not the send's to create.
    #[tokio::test]
    async fn a_failed_read_refuses_in_a_send_too() {
        let ports =
            ports_answering(json!({"_error": {"code": -32001, "message": "task not found"}}));
        let id: TaskId = "task-named".parse().unwrap();
        for s in [scope(), scope().send(Some("task-named".into()))] {
            let error = Arc::clone(&s.error);
            let verdict = with_request(s, async {
                assert!(ports.get(&id, None).await.is_err());
                streamable().and_then(|l| l.verdict("task-named"))
            })
            .await;
            assert_eq!(verdict, Some(false));
            assert!(error.lock().unwrap().is_some(), "the refusal is kept");
        }
    }

    /// What the caller meant crosses with the message: a new task under the
    /// id a2a-rs generated, or the task the caller named — and the push config
    /// the send carried, which is held rather than registered on a task that
    /// does not exist yet.
    #[tokio::test]
    async fn a_send_crosses_with_its_intent_and_its_inline_push() {
        let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let seen = Arc::clone(&calls);
        let ports = ports_with(move |req| {
            seen.lock()
                .unwrap()
                .push((req.method.clone(), req.params.clone()));
            let id = req.params["taskId"].as_str().unwrap_or("task-x");
            json!({"task": {"id": id, "contextId": "c"}})
        });
        let message = Message {
            message_id: "m-1".into(),
            ..Default::default()
        };
        let push = TaskPushNotificationConfig {
            task_id: "task-gen".into(),
            url: "https://hooks.example/x".into(),
            ..Default::default()
        };

        let ctx = RequestContext::anonymous();
        with_request(scope().send(None), async {
            let held = ports.set_config(&push).await.expect("held");
            assert_eq!(held.url, push.url);
            let t = ports.process_message("task-gen", &message, &ctx).await;
            assert_eq!(t.expect("the new task").id, "task-gen");
            // The task it created is the one this send may watch.
            assert_eq!(streamable().and_then(|l| l.verdict("task-gen")), Some(true));
        })
        .await;
        with_request(scope().send(Some("task-named".into())), async {
            let t = ports.process_message("task-named", &message, &ctx).await;
            assert_eq!(t.expect("the named task").id, "task-named");
        })
        .await;
        // Outside a send, a config is a registration like any other.
        with_request(scope(), async {
            let _ = ports.set_config(&push).await;
        })
        .await;

        let calls = calls.lock().unwrap().clone();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(methods, ["SendMessage", "SendMessage", "PushConfigSet"]);
        let (new, named) = (&calls[0].1, &calls[1].1);
        assert_eq!(new["taskId"], "task-gen");
        assert_eq!(new["newTask"], true);
        assert_eq!(new["push"]["url"], "https://hooks.example/x", "{new}");
        assert_eq!(named["taskId"], "task-named");
        assert_eq!(named["newTask"], false);
        assert!(named.get("push").is_none(), "{named}");
    }

    /// The task a send is answered with is the task a2a-rs subscribed to, or
    /// the send fails: any other would settle — or stream — somebody else's.
    #[tokio::test]
    async fn a_send_answered_with_another_task_fails() {
        let ports = ports_answering(json!({"task": {"id": "task-other", "contextId": "c"}}));
        let got = with_request(scope().send(None), async {
            let got = ports
                .process_message(
                    "task-gen",
                    &Message::default(),
                    &RequestContext::anonymous(),
                )
                .await;
            (got, streamable().and_then(|l| l.verdict("task-other")))
        })
        .await;
        assert!(got.0.is_err(), "{:?}", got.0);
        assert_eq!(got.1, None, "the wrong task is not proved");
    }

    /// What the request activated reaches the runtime with every call the
    /// ports make for it — a2a-rs's task reads included — and a call made
    /// outside a request activates nothing.
    #[tokio::test]
    async fn the_requests_activation_crosses_with_every_call() {
        use crate::runtime::surface::{
            Declaration, Ext, SpecMethod, TASK_ANNOTATIONS_EXTENSION, negotiate,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let ports = ports_with(move |req| {
            log.lock().unwrap().push(req.active);
            json!({"id": "task-1", "contextId": "c"})
        });
        let active = negotiate(
            &[TASK_ANNOTATIONS_EXTENSION.to_string()],
            &[Declaration {
                ext: Ext::TaskAnnotations,
                required: false,
            }],
            crate::runtime::surface::Route::Spec(SpecMethod::GetTask),
        )
        .unwrap();
        assert!(active.contains(Ext::TaskAnnotations));
        let id: TaskId = "task-1".parse().unwrap();
        let s = RequestScope::new(Principal::anonymous(), Via::Implicit, active);
        with_request(s, async { ports.get(&id, None).await.unwrap() }).await;
        ports.get(&id, None).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), [active, Active::NONE]);
    }

    /// Ports over a stand-in reactor that answers each call with `answer`.
    fn ports_with(
        answer: impl Fn(&crate::runtime::a2a_server::A2aRequest) -> Value + Send + 'static,
    ) -> RuntimePorts {
        let resolver =
            crate::a2a::Resolver::build(&crate::config::settings::A2a::default(), &|_| None)
                .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(crate::runtime::events::Event::A2a(req)) = rx.recv() {
                let _ = req.reply.send(answer(&req));
            }
        });
        RuntimePorts::new(
            A2aBridge::new(tx, resolver),
            Arc::new(a2a_rs::adapter::InMemoryStreamingHandler::new()),
        )
    }

    /// Ports over a stand-in reactor that answers every call with `reply`.
    fn ports_answering(reply: Value) -> RuntimePorts {
        ports_with(move |_| reply.clone())
    }

    /// An anonymous request's scope.
    fn scope() -> RequestScope {
        RequestScope::new(Principal::anonymous(), Via::Implicit, Active::NONE)
    }

    /// The runtime's error object is kept exactly as the runtime answered it —
    /// code, message and data — because the typed error a2a-rs receives can
    /// carry none of the data and rewords the message. The first one wins: it
    /// is the refusal that ended the request.
    #[tokio::test]
    async fn a_runtime_refusal_is_kept_whole_and_the_first_wins() {
        let refusal = json!({"code": -32603, "message": "the agent is draining",
            "data": [{"reason": "DRAINING", "domain": "agentd.dev"}]});
        let ports = ports_answering(json!({"_error": refusal}));
        let s = scope();
        let kept = Arc::clone(&s.error);
        let id: TaskId = "task-1".parse().unwrap();
        with_request(s, async {
            assert!(ports.cancel(&id).await.is_err());
            // A second refusal in the same request does not replace it.
            record_error(&json!({"code": -32001, "message": "later"}));
        })
        .await;
        assert_eq!(kept.lock().unwrap().clone(), Some(refusal));

        // Outside a request there is nobody to answer, and nothing is kept.
        record_error(&json!({"code": -32001, "message": "nobody"}));
    }

    /// a2a-rs hands the listing port no page parameters and has nowhere to put
    /// a next-page token, so the port walks the runtime's pages itself: the
    /// answer is every config, never a first page passed off as all of them.
    #[tokio::test]
    async fn a_push_config_listing_is_never_silently_short() {
        let cfg = |id: &str| json!({"taskId": "t", "id": id, "url": "https://h.example/x"});
        let (a, b) = (cfg("p-a"), cfg("p-b"));
        let ports = ports_with(move |req| match req.params["pageToken"].as_str() {
            Some("") => json!({"configs": [a], "nextPageToken": "page-2"}),
            Some("page-2") => json!({"configs": [b], "nextPageToken": ""}),
            other => json!({"_error": {"code": -32602, "message": format!("{other:?}")}}),
        });
        let params = a2a_rs::domain::ListTaskPushNotificationConfigsParams {
            id: "t".into(),
            metadata: None,
        };
        let all = ports.list_configs(&params).await.expect("every page");
        let ids: Vec<&str> = all.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["p-a", "p-b"]);

        // A cursor that does not move is an error, not an endless loop.
        let stuck = ports_answering(json!({"configs": [], "nextPageToken": "again"}));
        assert!(stuck.list_configs(&params).await.is_err());
    }

    /// Every way a reply can fail to be the answer it claims is an error, and
    /// every code crosses as the code it is.
    #[tokio::test]
    async fn fail_closed_and_codes() {
        // An error under either spelling is an error, never a result.
        for v in [
            json!({"_error": {"code": -32001, "message": "task not found"}}),
            json!({"error": {"code": -32001, "message": "task not found"}}),
        ] {
            assert!(
                matches!(reply_of(v.clone()), Err(A2AError::TaskNotFound(_))),
                "{v}"
            );
        }
        assert!(reply_of(json!({"tasks": []})).is_ok());

        // A task with no id is not a task — `null` included, which the spec's
        // all-defaults type would otherwise read as an empty success.
        for v in [
            Value::Null,
            json!({}),
            json!({"task": null}),
            json!({"x": 1}),
        ] {
            assert!(task_from(v.clone()).is_err(), "{v}");
        }
        let t = task_from(json!({"task": {"id": "task-1", "contextId": "c"}})).unwrap();
        assert_eq!(t.id, "task-1");
        assert!(same_task("task-2", t).is_err());

        // The codes a2a-rs branches on come back as their variants…
        let code = |c: i64| from_error_object(&json!({"code": c, "message": "m"}));
        assert!(matches!(code(-32002), A2AError::TaskNotCancelable(_)));
        assert!(matches!(
            code(-32003),
            A2AError::PushNotificationNotSupported
        ));
        assert!(matches!(code(-32005), A2AError::ContentTypeNotSupported(_)));
        assert!(matches!(
            code(-32007),
            A2AError::AuthenticatedExtendedCardNotConfigured
        ));
        assert!(matches!(code(-32602), A2AError::InvalidParams(_)));
        // …and every other code keeps its number and its data rather than
        // collapsing into an internal error.
        match from_error_object(&json!({"code": -31403, "message": "no", "data": [1]})) {
            A2AError::JsonRpc {
                code,
                message,
                data,
            } => {
                assert_eq!(code, -31403);
                assert_eq!(message, "no");
                assert_eq!(data, Some(json!([1])));
            }
            other => panic!("-31403 must stay itself, got {other:?}"),
        }
    }

    /// A read proves the caller may watch a task only when the reply is that
    /// task. A runtime answering for some other id — a bug, a mix-up — must
    /// not open a stream on the id that was asked about.
    #[tokio::test]
    async fn a_read_answering_for_another_task_proves_nothing() {
        let ports = ports_answering(json!({"id": "task-other", "contextId": "c"}));
        let asked: TaskId = "task-asked".parse().unwrap();
        let (got, verdict) = with_request(scope(), async {
            let got = ports.get(&asked, None).await;
            (got, streamable().and_then(|s| s.verdict("task-asked")))
        })
        .await;
        assert!(got.is_err(), "the wrong task is not an answer");
        assert_eq!(verdict, Some(false));

        let ports = ports_answering(json!({"id": "task-asked", "contextId": "c"}));
        let verdict = with_request(scope(), async {
            ports.get(&asked, None).await.expect("the task asked for");
            streamable().and_then(|s| s.verdict("task-asked"))
        })
        .await;
        assert_eq!(verdict, Some(true));
    }

    /// A task retention forgets leaves nothing on the stream side: its replay
    /// ring (every status and its result, which a `Last-Event-ID` would
    /// otherwise still hand back) and its channel go, and every other task
    /// keeps its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forgotten_task_leaves_no_stream_state() {
        use a2a_rs::port::AsyncEventLog;
        let events = a2a_rs::adapter::InMemoryEventLog::new();
        let updates = Arc::new(a2a_rs::adapter::StreamingFanout::over(events.clone()));
        let log = crate::obs::log::Logger::new(
            crate::obs::log::LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: crate::obs::log::Comp::Supervisor,
                pid: std::process::id(),
                trace_id: None,
            },
            crate::obs::log::Level::Error,
        );
        let sink = StreamSink::new(
            Arc::clone(&updates),
            events.clone(),
            tokio::runtime::Handle::current(),
            log,
        );
        let finished = |id: &str| {
            let mut t = crate::a2a::tasks::Task::new(
                id,
                "c",
                Some("u"),
                crate::a2a::tasks::Link::Turn { ctx: "c".into() },
            );
            t.set_result(json!("done"));
            t.transition(crate::a2a::tasks::State::Completed, None);
            t
        };
        let held = |id: &'static str| {
            let events = events.clone();
            let updates = Arc::clone(&updates);
            async move {
                (
                    events.replay(id, 0).await.unwrap().events.len(),
                    updates.get_subscriber_count(id).await.unwrap(),
                )
            }
        };
        // Someone is still watching each, so each has a channel to drop.
        let _watch_1 = updates.start_task_streaming("t-1", None).await.unwrap();
        let _watch_2 = updates.start_task_streaming("t-2", None).await.unwrap();
        for id in ["t-1", "t-2"] {
            sink.status(&finished(id));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while held("t-1").await.0 == 0 || held("t-2").await.0 == 0 {
            assert!(std::time::Instant::now() < deadline, "never published");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(held("t-1").await.1, 1, "watched");

        sink.forget("t-1");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while held("t-1").await != (0, 0) {
            assert!(
                std::time::Instant::now() < deadline,
                "t-1 is still held: {:?}",
                held("t-1").await
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(held("t-2").await, (1, 1), "t-2 keeps its own");
    }
}
