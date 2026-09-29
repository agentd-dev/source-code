// SPDX-License-Identifier: AGPL-3.0-only
//! The **official SDK**, wrapped so the rest of agentd stays blocking.
//!
//! [`rmcp`] — the Rust SDK maintained alongside the protocol — owns the
//! handshake and every operation (tools, resources, prompts, completion,
//! subscriptions), so spec-tracking is inherited rather than hand-maintained.
//! [`crate::client::McpClient`] is the builder agentd holds; this is what it
//! connects.
//!
//! **Blocking on the outside.** agentd's runtime is blocking: the supervisor is
//! a single-threaded reactor and the turn worker is a straight-line state
//! machine. rmcp is async. Rather than colour the entire codebase, this facade
//! owns a private runtime (multi-threaded, for the reason given at
//! [`RmcpBuilder::connect`]) and blocks on it, exposing synchronous methods.
//! The runtime lives as long as the client and dies with it.
//!
//! **The protocol version is the SDK's to choose.** rmcp pins
//! `ProtocolVersion::LATEST` at `2025-11-25` even though the newer stateless
//! revision exists as a constant — that is upstream telling us what it is
//! actually ready to speak. Overriding it would mean asking a server for a
//! dialect the SDK may not fully implement, which is the opposite of why one
//! adopts an SDK. So this backend speaks whatever rmcp says is current, and
//! picks up the stateless revision automatically on the release that promotes
//! it. Everything version-dependent here (notably [`RmcpClient::subscribe`])
//! therefore branches on the *negotiated* version, compared against rmcp's own
//! constants.
//!
//! **No response cache.** SEP-2549 lets a client reuse a `resources/read` or
//! list result for the server's `ttlMs`, and serve an expired one when a
//! re-fetch fails; rmcp does both by default. agentd switches that off on every
//! connection. It reads because it needs the server's *current* answer — the
//! §7.7 freshness watch, notify-then-read, signing-key resolution — and a copy
//! from memory reported as a read is the "reports success, did nothing" defect:
//! a registry that was down, or that answered "not found" to a withdrawn
//! instruction, went on confirming it. So every read and list is one request on
//! the wire, and a failure is an `Err`, never an earlier answer. There is no knob
//! to turn it back on; `refresh`/`freshness` is the lever for registry load.

use crate::client::McpError;
use crate::inbound;
use crate::rpc;
use crate::wire::{Implementation, Prompt, ReadResourceResult, Resource, ServerCapabilities, Tool};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, ElicitRequestParams, ElicitResult,
    ElicitationAction, ElicitationCapability, Implementation as RmcpImpl, ProtocolVersion,
    ReadResourceRequestParams, SubscriptionFilter,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, ServiceExt};

/// Bridges rmcp's `ClientHandler` onto agentd: a server's elicitation reaches
/// the host that can answer it, and a server's notifications reach the queue the
/// reactor drains.
///
/// The notification half matters more than it looks. agentd is a *reactive*
/// daemon: `notifications/resources/updated` is what wakes a subscribed
/// workflow. A handler that accepted those and dropped them would leave the
/// agent idle forever, with nothing in any log to say why.
#[derive(Clone)]
struct Handler {
    info: ClientInfo,
    /// The host's elicitation answerer; `None` when the capability is not
    /// declared, which is also what the handshake told the server.
    elicitation: Option<Arc<dyn inbound::Handler>>,
    /// Where a server's notifications land until the reactor drains them.
    queue: Arc<Mutex<Vec<rpc::Notification>>>,
}

impl Handler {
    fn queue(&self, method: &str, params: Value) {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(rpc::Notification::new(method, Some(params)));
    }
}

fn declined() -> ElicitResult {
    ElicitResult::new(ElicitationAction::Decline)
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientInfo {
        self.info.clone()
    }

    async fn create_elicitation(
        &self,
        params: ElicitRequestParams,
        _ctx: rmcp::service::RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        // Only the form flavour asks for structured input; a URL elicitation
        // has nothing for `ask_human` to answer, so it is declined honestly.
        let (message, requested_schema) = match &params {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => (
                message.clone(),
                serde_json::to_value(requested_schema).unwrap_or_else(|_| json!({})),
            ),
            _ => return Ok(declined()),
        };
        let Some(handler) = &self.elicitation else {
            return Ok(declined());
        };
        let answer = handler.handle(inbound::Inbound::Elicit {
            message,
            requested_schema,
        });
        Ok(match answer {
            Some(inbound::Answer::Accept(content)) => {
                ElicitResult::new(ElicitationAction::Accept).with_content(content)
            }
            Some(inbound::Answer::Decline) => declined(),
            // Nothing could ask: cancel is the honest outcome, and it is not an
            // error.
            _ => ElicitResult::new(ElicitationAction::Cancel),
        })
    }

    // ---- notifications: the reactive wake path ----

    async fn on_resource_updated(
        &self,
        params: rmcp::model::ResourceUpdatedNotificationParam,
        _ctx: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.queue(
            "notifications/resources/updated",
            serde_json::to_value(&params).unwrap_or_else(|_| json!({})),
        );
    }

    async fn on_resource_list_changed(&self, _ctx: rmcp::service::NotificationContext<RoleClient>) {
        self.queue("notifications/resources/list_changed", json!({}));
    }

    async fn on_tool_list_changed(&self, _ctx: rmcp::service::NotificationContext<RoleClient>) {
        self.queue("notifications/tools/list_changed", json!({}));
    }

    async fn on_prompt_list_changed(&self, _ctx: rmcp::service::NotificationContext<RoleClient>) {
        self.queue("notifications/prompts/list_changed", json!({}));
    }

    // Upstream marks logging notifications deprecated, but a server that still
    // sends them is better heard than silently ignored.
    #[allow(deprecated)]
    async fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _ctx: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.queue(
            "notifications/message",
            serde_json::to_value(&params).unwrap_or_else(|_| json!({})),
        );
    }

    async fn on_progress(
        &self,
        params: rmcp::model::ProgressNotificationParam,
        _ctx: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.queue(
            "notifications/progress",
            serde_json::to_value(&params).unwrap_or_else(|_| json!({})),
        );
    }
}

/// A blocking MCP client backed by the official SDK.
pub struct RmcpClient {
    name: String,
    rt: tokio::runtime::Runtime,
    service: RunningService<RoleClient, Handler>,
    caps: ServerCapabilities,
    /// The revision the handshake settled on, kept as rmcp's own type so the
    /// version-dependent branches compare it the way rmcp does.
    protocol_version: Option<ProtocolVersion>,
    tool_meta: Option<Value>,
    notifications: Arc<Mutex<Vec<rpc::Notification>>>,
    /// Every URI the host asked for; one `listen` subscription covers them all.
    uris: Mutex<std::collections::BTreeSet<String>>,
    /// The task pumping that subscription into `notifications`.
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Builder state, so the host can declare capabilities before connecting (the
/// handshake carries them, so they cannot be added afterwards).
pub struct RmcpBuilder {
    name: String,
    endpoint: String,
    headers: Vec<(String, String)>,
    timeout: Duration,
    client_info: Implementation,
    elicitation: Option<Arc<dyn inbound::Handler>>,
    /// agentd's authenticated socket. Present whenever the connection carries a
    /// credential the SDK's own client could not (a request signer, an mTLS
    /// identity); absent only in tests that dial a bare loopback server.
    http: Option<Arc<crate::http::HttpTransport>>,
}

impl RmcpBuilder {
    pub fn new(
        name: &str,
        endpoint: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Self {
        RmcpBuilder {
            name: name.to_string(),
            endpoint: endpoint.to_string(),
            headers,
            timeout,
            client_info: Implementation {
                name: "agentd".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
            },
            elicitation: None,
            http: None,
        }
    }

    /// Use agentd's socket for this connection — the one carrying its request
    /// signer and mTLS identity.
    pub fn with_http(mut self, http: Arc<crate::http::HttpTransport>) -> Self {
        self.http = Some(http);
        self
    }

    pub fn with_client_info(mut self, info: Implementation) -> Self {
        self.client_info = info;
        self
    }

    /// Declare `elicitation` and route it to `handler`, the host seam that
    /// reaches `ask_human`.
    pub fn with_elicitation(mut self, handler: Arc<dyn inbound::Handler>) -> Self {
        self.elicitation = Some(handler);
        self
    }

    /// Connect and run the `initialize` handshake.
    pub fn connect(self) -> Result<RmcpClient, McpError> {
        // A *multi-threaded* runtime, deliberately. The SDK runs the transport
        // and the service dispatch as background tasks, and a server pushing a
        // notification (`resources/updated` — agentd's reactive wake) has to be
        // heard between our calls, not only during one. On a current-thread
        // runtime those tasks advance only inside `block_on`, so a daemon that
        // was idling — exactly when a wake matters — would never receive it.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("agentd-mcp")
            .build()
            .map_err(|e| {
                McpError::Transport(format!("mcp server '{}': runtime: {e}", self.name))
            })?;

        let mut config =
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                self.endpoint.clone(),
            );
        for (k, v) in &self.headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(k.as_bytes()),
                http::HeaderValue::from_str(v),
            ) {
                config.custom_headers.insert(name, value);
            }
        }

        let mut caps = ClientCapabilities::default();
        if self.elicitation.is_some() {
            caps.elicitation = Some(ElicitationCapability::new());
        }

        // Notifications arrive on the `listen` subscription, not through the
        // handler — rmcp routes one or the other, never both.
        let notifications: Arc<Mutex<Vec<rpc::Notification>>> = Arc::default();
        let mut implementation = RmcpImpl::new(
            self.client_info.name.clone(),
            self.client_info.version.clone(),
        );
        implementation.title = self.client_info.title.clone();

        let handler = Handler {
            queue: Arc::clone(&notifications),
            // `ClientInfo::new` already carries `ProtocolVersion::default()`,
            // i.e. rmcp's `LATEST`. Left explicit so it is obvious this is a
            // decision (follow the SDK) and not an omission.
            info: ClientInfo::new(caps, implementation)
                .with_protocol_version(ProtocolVersion::default()),
            elicitation: self.elicitation.clone(),
        };

        let name = self.name.clone();
        // The SDK speaks the protocol; agentd supplies the socket, so a
        // connection keeps its signer and its mTLS identity.
        let socket = match &self.http {
            Some(h) => Arc::clone(h),
            None => Arc::new(crate::http::HttpTransport::new(
                crate::http::McpEndpoint::parse(&self.endpoint)
                    .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?,
                self.headers.clone(),
            )),
        };
        let client = crate::rmcp_transport::AgentdHttp::new(socket, self.timeout);
        let service = rt
            .block_on(async move {
                let transport = StreamableHttpClientTransport::with_client(client, config);
                handler.serve(transport).await
            })
            .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?;
        // Before any request can be issued (`initialize` itself is not cached):
        // the cache is per peer and the peer only exists once `serve` returns.
        // With it on, a read inside the server's `ttlMs` never reaches the
        // server, and a failed read past it is answered with the expired copy —
        // so a dead or refusing registry would keep "confirming" freshness.
        rt.block_on(
            service
                .peer()
                .set_response_cache_config(rmcp::ClientCacheConfig::disabled()),
        );

        let info = service.peer_info();
        let protocol_version = info.as_ref().map(|i| i.protocol_version.clone());
        let info_json = info
            .as_ref()
            .and_then(|i| serde_json::to_value(i.as_ref()).ok());
        let caps = server_capabilities(info_json.as_ref());

        Ok(RmcpClient {
            name: self.name,
            rt,
            service,
            caps,
            protocol_version,
            tool_meta: None,
            notifications,
            uris: Mutex::new(std::collections::BTreeSet::new()),
            pump: Mutex::new(None),
        })
    }
}

/// Translate rmcp's negotiated server capabilities into ours.
///
/// Deliberately via JSON rather than field-by-field: both sides are the same
/// wire shape, so a round trip is exact today and does not break the day rmcp
/// adds a capability we have not heard of.
fn server_capabilities(info: Option<&serde_json::Value>) -> ServerCapabilities {
    info.and_then(|v| v.get("capabilities"))
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default()
}

fn rpc_err(name: &str, op: &str, e: impl std::fmt::Display) -> McpError {
    McpError::Transport(format!("mcp server '{name}': {op}: {e}"))
}

impl RmcpClient {
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.caps
    }

    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_ref().map(ProtocolVersion::as_str)
    }

    pub fn set_tool_meta(&mut self, meta: Value) {
        self.tool_meta = Some(meta);
    }

    /// Convert an rmcp value into our wire type. Both sides are the same JSON
    /// shape, so this is exact — and it does not need updating when rmcp adds a
    /// field we do not model.
    fn convert<T: serde::de::DeserializeOwned>(
        &self,
        v: &impl serde::Serialize,
        what: &str,
    ) -> Result<T, McpError> {
        let json = serde_json::to_value(v).map_err(|e| rpc_err(&self.name, what, e))?;
        serde_json::from_value(json).map_err(|e| rpc_err(&self.name, what, e))
    }

    pub fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        let res = self
            .rt
            .block_on(self.service.list_all_tools())
            .map_err(|e| rpc_err(&self.name, "tools/list", e))?;
        self.convert(&res, "tools/list")
    }

    /// `_meta` (run id, idempotency key) rides on the arguments object, which is
    /// where the wire carries it.
    pub fn call_tool_with_meta(
        &self,
        name: &str,
        args: Option<Value>,
        extra_meta: Option<Value>,
    ) -> Result<Value, McpError> {
        let mut arguments = match args {
            Some(Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        if let Some(m) = merge_meta(self.tool_meta.as_ref(), extra_meta) {
            arguments.insert("_meta".into(), m);
        }
        let param = CallToolRequestParams::new(name.to_string()).with_arguments(arguments);
        let res = self
            .rt
            .block_on(self.service.call_tool(param))
            .map_err(|e| rpc_err(&self.name, &format!("tools/call {name}"), e))?;
        serde_json::to_value(&res).map_err(|e| rpc_err(&self.name, "tools/call", e))
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, McpError> {
        let res = self
            .rt
            .block_on(self.service.list_all_resources())
            .map_err(|e| rpc_err(&self.name, "resources/list", e))?;
        self.convert(&res, "resources/list")
    }

    pub fn read_resource(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let res = self
            .rt
            .block_on(
                self.service
                    .read_resource(ReadResourceRequestParams::new(uri.to_string())),
            )
            .map_err(|e| rpc_err(&self.name, &format!("resources/read {uri}"), e))?;
        self.convert(&res, "resources/read")
    }

    pub fn list_prompts(&self) -> Result<Vec<Prompt>, McpError> {
        let res = self
            .rt
            .block_on(self.service.list_all_prompts())
            .map_err(|e| rpc_err(&self.name, "prompts/list", e))?;
        self.convert(&res, "prompts/list")
    }

    pub fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<crate::wire::GetPromptResult, McpError> {
        let mut params = rmcp::model::GetPromptRequestParams::new(name);
        params.arguments = arguments.and_then(|a| a.as_object().cloned());
        let res = self
            .rt
            .block_on(self.service.get_prompt(params))
            .map_err(|e| rpc_err(&self.name, &format!("prompts/get {name}"), e))?;
        self.convert(&res, "prompts/get")
    }

    pub fn complete(
        &self,
        reference: Value,
        argument: Value,
    ) -> Result<crate::wire::CompleteResult, McpError> {
        // Both halves are protocol-typed by the SDK: `ref` is a tagged union
        // over prompt/resource references, and a shape it does not recognise is
        // refused here rather than on the wire.
        let r#ref = serde_json::from_value(reference)
            .map_err(|e| rpc_err(&self.name, "completion/complete ref", e))?;
        let argument = serde_json::from_value(argument)
            .map_err(|e| rpc_err(&self.name, "completion/complete argument", e))?;
        let res = self
            .rt
            .block_on(
                self.service
                    .complete(rmcp::model::CompleteRequestParams::new(r#ref, argument)),
            )
            .map_err(|e| rpc_err(&self.name, "completion/complete", e))?;
        self.convert(&res, "completion/complete")
    }

    pub fn list_resource_templates(&self) -> Result<Vec<crate::wire::ResourceTemplate>, McpError> {
        let res = self
            .rt
            .block_on(self.service.list_all_resource_templates())
            .map_err(|e| rpc_err(&self.name, "resources/templates/list", e))?;
        self.convert(&res, "resources/templates/list")
    }

    /// Subscribe to a resource, by whichever mechanism the negotiated revision
    /// actually defines.
    ///
    /// The revisions disagree: up to 2025-11-25 a client calls
    /// `resources/subscribe`, and from 2026-07-28 on `subscriptions/listen`
    /// replaces it (rmcp marks the former deprecated *for those versions
    /// only*). Because this backend speaks whatever revision the SDK
    /// negotiates, the choice must be read off the negotiated version rather
    /// than hard-coded — calling the wrong one leaves the host with a
    /// subscription the server never honours.
    ///
    /// From 2026-07-28 on, one subscription covers every tracked URI: adding a
    /// URI reopens it with the widened filter, and its notifications pump into
    /// the queue the host drains.
    pub fn subscribe(&self, uri: &str) -> Result<(), McpError> {
        {
            let mut uris = self.uris.lock().unwrap_or_else(|e| e.into_inner());
            if !uris.insert(uri.to_string()) {
                return Ok(()); // already covered by the live subscription
            }
        }
        self.relisten()
    }

    #[allow(deprecated)]
    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        {
            let mut uris = self.uris.lock().unwrap_or_else(|e| e.into_inner());
            if !uris.remove(uri) {
                return Ok(());
            }
        }
        // Before 2026-07-28 the server holds a per-URI subscription, so it
        // needs an explicit `resources/unsubscribe` — narrowing a filter would
        // not reach it. From then on, reopening the listen with the narrowed
        // filter is the cancellation.
        if !self.listens() {
            return self
                .rt
                .block_on(
                    self.service
                        .unsubscribe(rmcp::model::UnsubscribeRequestParams::new(uri.to_string())),
                )
                .map_err(|e| rpc_err(&self.name, &format!("resources/unsubscribe {uri}"), e));
        }
        self.relisten()
    }

    /// (Re)open the single subscription covering every tracked URI, and pump its
    /// notifications into the drain queue on a background task.
    fn relisten(&self) -> Result<(), McpError> {
        // Revisions before 2026-07-28 have no `subscriptions/listen`; the
        // per-URI `resources/subscribe` is the correct call there, and is
        // marked deprecated only relative to the newer revisions.
        if !self.listens() {
            return self.subscribe_each();
        }
        let uris: Vec<String> = self
            .uris
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        // Dropping the previous handle cancels the previous listen.
        *self.pump.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if uris.is_empty() {
            return Ok(());
        }

        let mut filter = SubscriptionFilter::builder().resources_list_changed();
        for u in &uris {
            filter = filter.resource_subscription(u.clone());
        }
        let filter = filter.build();

        let peer = self.service.peer().clone();
        let mut subscription = self
            .rt
            .block_on(peer.listen(filter))
            .map_err(|e| rpc_err(&self.name, "subscriptions/listen", e))?;

        let queue = Arc::clone(&self.notifications);
        let handle = self.rt.spawn(async move {
            while let Ok(Some(note)) = subscription.next().await {
                if let Ok(v) = serde_json::to_value(&note)
                    && let Ok(n) = serde_json::from_value::<rpc::Notification>(v)
                {
                    queue.lock().unwrap_or_else(|e| e.into_inner()).push(n);
                }
            }
        });
        *self.pump.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(())
    }

    /// Does the negotiated revision define `subscriptions/listen`? Every
    /// revision from 2026-07-28 on does — compared with `>=` exactly as rmcp's
    /// own client decides the same question (`service/client.rs`), so a later
    /// revision rmcp negotiates keeps the listen path rather than falling back
    /// to a method that revision removed.
    fn listens(&self) -> bool {
        self.protocol_version
            .as_ref()
            .is_some_and(|v| *v >= ProtocolVersion::V_2026_07_28)
    }

    /// Per-URI subscription: one `resources/subscribe` per URI. Notifications
    /// arrive through the handler's channel rather than a subscription handle.
    #[allow(deprecated)]
    fn subscribe_each(&self) -> Result<(), McpError> {
        let uris: Vec<String> = self
            .uris
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        for uri in uris {
            self.rt
                .block_on(
                    self.service
                        .subscribe(rmcp::model::SubscribeRequestParams::new(uri.clone())),
                )
                .map_err(|e| rpc_err(&self.name, &format!("resources/subscribe {uri}"), e))?;
        }
        Ok(())
    }

    /// Drain notifications the handler queued: take what has arrived, leave
    /// the queue empty.
    pub fn drain_notifications(&self) -> Vec<rpc::Notification> {
        std::mem::take(&mut *self.notifications.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Merge the persistent tool `_meta` with a per-call overlay; the overlay wins.
fn merge_meta(base: Option<&Value>, extra: Option<Value>) -> Option<Value> {
    match (base, extra) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(e)) => Some(e),
        (Some(b), Some(e)) => {
            let mut m = b.as_object().cloned().unwrap_or_default();
            if let Some(eo) = e.as_object() {
                for (k, v) in eo {
                    m.insert(k.clone(), v.clone());
                }
            }
            Some(Value::Object(m))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_overlay_wins_without_mutating_the_base() {
        let base = json!({"agent/run_id": "r1", "traceparent": "tp"});
        let merged = merge_meta(Some(&base), Some(json!({"traceparent": "tp2", "k": 1}))).unwrap();
        assert_eq!(merged["agent/run_id"], "r1");
        assert_eq!(merged["traceparent"], "tp2");
        assert_eq!(merged["k"], 1);
        assert_eq!(base["traceparent"], "tp");
        assert!(merge_meta(None, None).is_none());
    }

    #[test]
    fn the_sdk_still_pins_a_revision_before_the_listen_one() {
        // The backend deliberately asks for `ProtocolVersion::default()` —
        // rmcp's `LATEST`, currently 2025-11-25 — so against a server that
        // echoes it, subscriptions take the per-URI path. This records that
        // the listen revision exists and still sorts after `LATEST`, so the day
        // rmcp promotes it this test is the tripwire.
        assert_eq!(ProtocolVersion::default(), ProtocolVersion::LATEST);
        assert!(ProtocolVersion::LATEST < ProtocolVersion::V_2026_07_28);
    }
}
