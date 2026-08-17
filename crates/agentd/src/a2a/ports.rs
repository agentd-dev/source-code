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
//! Two ports are deliberately refusals rather than implementations. Task
//! *creation* and *status updates* are not things a caller may do out of band —
//! agentd's runtime owns when a task exists and what state it is in — and push
//! notifications are not implemented, which the agent card says out loud. Both
//! answer with the spec's own error for "not here" rather than a half-built
//! result.

use std::sync::Arc;

use a2a_rs::domain::{
    A2AError, ContextId, ListTasksParams, ListTasksResult, Message, Task as WireTask,
    TaskArtifactUpdateEvent, TaskId, TaskPushNotificationConfig, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};
use a2a_rs::port::{
    AsyncMessageHandler, AsyncNotificationManager, AsyncStreamingHandler, AsyncTaskLifecycle,
    AsyncTaskQuery, RequestContext, StreamingSubscriber,
};
use serde_json::{Value, json};

use crate::a2a::Principal;
use crate::runtime::a2a_server::A2aBridge;

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
    /// successful result that happens to contain an error.
    async fn call(&self, method: &str, params: Value, who: &Principal) -> Result<Value, A2AError> {
        let bridge = Arc::clone(&self.bridge);
        let method = method.to_string();
        let who = who.clone();
        let v = tokio::task::spawn_blocking(move || bridge.call(&method, params, who))
            .await
            .map_err(|e| A2AError::Internal(format!("the runtime call did not complete: {e}")))?;
        match v.get("_error") {
            Some(e) => Err(from_error_object(e)),
            None => Ok(v),
        }
    }

    /// The stream fan-out, for the reactor side to publish into.
    pub fn updates(&self) -> Arc<a2a_rs::adapter::InMemoryStreamingHandler> {
        Arc::clone(&self.updates)
    }
}

tokio::task_local! {
    /// Who is making the request currently being served.
    ///
    /// The spec's task ports (`get`, `cancel`, `list`) take no caller — they
    /// were drawn for a server whose store is not per-principal. agentd's is:
    /// a task belongs to whoever started it, and a non-operator may only see
    /// its own. So the caller travels out-of-band, scoped to the request's
    /// tokio task, exactly as it used to travel per-connection on a thread.
    ///
    /// Set once by the transport ([`crate::a2a::serve`]) around the whole
    /// dispatch. Unset means nobody is being served, which reads as anonymous —
    /// the role the authorization matrix refuses everything.
    static CALLER: Principal;
}

/// Run `f` with `who` as the caller for the duration of one request.
pub async fn with_caller<F, T>(who: Principal, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CALLER.scope(who, f).await
}

/// The caller of the request being served.
pub fn caller() -> Principal {
    CALLER
        .try_with(|p| p.clone())
        .unwrap_or_else(|_| Principal::anonymous())
}

/// The reactor's error, read back as the spec's error type.
///
/// The reactor marks a failed answer with an `_error` member rather than
/// returning a `Result`, because its reply channel carries one JSON value. The
/// codes it uses are already the spec's, so this is a mapping and not a
/// translation — and going through the typed error is what makes the protocol
/// layer emit the right JSON-RPC code instead of passing an error off as a
/// successful result that happens to contain one.
fn from_error_object(e: &Value) -> A2AError {
    let code = e.get("code").and_then(Value::as_i64).unwrap_or(-32603);
    let msg = e
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("internal error")
        .to_string();
    match code as i32 {
        -32001 => A2AError::TaskNotFound(msg),
        -32002 => A2AError::TaskNotCancelable(msg),
        -32003 => A2AError::PushNotificationNotSupported,
        -32004 => A2AError::UnsupportedOperation(msg),
        -32601 => A2AError::MethodNotFound(msg),
        -32602 => A2AError::InvalidParams(msg),
        _ => A2AError::Internal(msg),
    }
}

/// Read a `Task` out of a reactor reply, which may be the task itself or the
/// `{task}` envelope a send answers with.
fn task_from(v: Value) -> Result<WireTask, A2AError> {
    let body = match v.get("task") {
        Some(t) => t.clone(),
        None => v,
    };
    serde_json::from_value(body).map_err(A2AError::JsonParse)
}

#[async_trait::async_trait]
impl AsyncMessageHandler for RuntimePorts {
    /// A message becomes runtime work: a conversation turn, or — when it carries
    /// agentd's command DataPart — a registry action. Which one it is, and the
    /// durable task that results, is the reactor's decision; this only carries
    /// the message across.
    ///
    /// `task_id` is empty for a new task (the caller did not name one), and the
    /// reactor mints the id in that case.
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
        let mut params =
            json!({"message": serde_json::to_value(message).map_err(A2AError::JsonParse)?});
        if !task_id.is_empty() {
            params["taskId"] = json!(task_id);
        }
        task_from(self.call("SendMessage", params, &who).await?)
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
        let mut t = task_from(
            self.call("GetTask", json!({"id": id.as_str()}), &who)
                .await?,
        )?;
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
        task_from(
            self.call("CancelTask", json!({"id": id.as_str()}), &who)
                .await?,
        )
    }

    async fn exists(&self, id: &TaskId) -> Result<bool, A2AError> {
        match self.get(id, None).await {
            Ok(_) => Ok(true),
            Err(A2AError::TaskNotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[async_trait::async_trait]
impl AsyncTaskQuery for RuntimePorts {
    /// Every task the caller may see. Ownership filtering happens in the
    /// reactor, which is the only place that knows who owns what.
    async fn list(&self, params: &ListTasksParams) -> Result<ListTasksResult, A2AError> {
        let who = caller();
        let mut req = json!({});
        if let Some(c) = &params.context_id {
            req["contextId"] = json!(c);
        }
        let v = self.call("ListTasks", req, &who).await?;
        serde_json::from_value(v).map_err(A2AError::JsonParse)
    }
}

#[async_trait::async_trait]
impl AsyncNotificationManager for RuntimePorts {
    async fn set_config(
        &self,
        _config: &TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::PushNotificationNotSupported)
    }
    async fn get_config(
        &self,
        _params: &a2a_rs::domain::GetTaskPushNotificationConfigParams,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::PushNotificationNotSupported)
    }
    async fn list_configs(
        &self,
        _params: &a2a_rs::domain::ListTaskPushNotificationConfigsParams,
    ) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
        Err(A2AError::PushNotificationNotSupported)
    }
    async fn delete_config(
        &self,
        _params: &a2a_rs::domain::DeleteTaskPushNotificationConfigParams,
    ) -> Result<(), A2AError> {
        Err(A2AError::PushNotificationNotSupported)
    }
}

/// The streaming half: a2a-rs's own in-memory fan-out, shared.
///
/// agentd has nothing to add to it — the reactor publishes transitions in
/// through [`StreamSink`], and every subscriber, replay buffer and
/// stream-termination rule is the protocol layer's. This exists only because the
/// adapter takes the handler by value while the reactor needs a handle to the
/// same one.
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
        self.0.combined_update_stream(task_id, from_event_id).await
    }
}

/// Where the reactor publishes a task transition so subscribers see it.
///
/// The reactor is synchronous and the fan-out is async, so this holds a handle
/// to the runtime that owns the listener and drives one short task per event.
/// It is the only place the two directions meet.
pub struct StreamSink {
    updates: Arc<a2a_rs::adapter::InMemoryStreamingHandler>,
    handle: tokio::runtime::Handle,
}

impl StreamSink {
    pub fn new(
        updates: Arc<a2a_rs::adapter::InMemoryStreamingHandler>,
        handle: tokio::runtime::Handle,
    ) -> StreamSink {
        StreamSink { updates, handle }
    }

    /// Publish a status transition.
    pub fn status(
        &self,
        task_id: &str,
        context_id: &str,
        state: TaskState,
        message: Option<&str>,
        at_ms: u64,
    ) {
        let ev = crate::a2a::wire::status_event(task_id, context_id, state, message, at_ms);
        self.spawn_status(task_id.to_string(), ev);
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
