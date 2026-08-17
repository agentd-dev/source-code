// SPDX-License-Identifier: AGPL-3.0-only
//! The **official SDK** as an alternative client backend (`--features rmcp-client`).
//!
//! agentd's own MCP client is hand-rolled, for the same reason everything else
//! here is: the default build has three external dependencies and a sub-millisecond
//! start. That is a real property, and it is also a real bet — a bet that we
//! track the specification correctly by ourselves.
//!
//! This module is the other side of that bet, for deployments that would rather
//! inherit spec-tracking from upstream than own it: the same operations, served
//! by [`rmcp`], the Rust SDK maintained with the protocol. Turning it on costs
//! ~77 additional crates including tokio, so it is opt-in and the hand-rolled
//! client stays the default.
//!
//! **Blocking on the outside.** agentd has no async runtime: the supervisor is a
//! single-threaded reactor and the turn worker is a straight-line state machine,
//! both blocking. rmcp is async. Rather than colour the entire codebase, this
//! facade owns a private current-thread runtime and blocks on it, exposing the
//! same synchronous methods the native client does. The runtime lives as long as
//! the client and dies with it.
//!
//! **The protocol version is ours to choose.** rmcp's `ProtocolVersion::LATEST`
//! is the conservative `2025-11-25`; the newer stateless revision is available as
//! a constant. We ask for the newest revision this library knows
//! ([`crate::version::LATEST_MODERN_VERSION`]) and let the server negotiate down,
//! which is the whole point of the handshake.

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

/// The host's inbound policy, shared with the rmcp handler.
#[derive(Clone)]
struct Inbound {
    caps: inbound::Capabilities,
    handler: Option<Arc<dyn inbound::Handler>>,
}

/// Bridges rmcp's `ClientHandler` onto our host-facing [`inbound::Handler`], so
/// a server's elicitation reaches the same place under either backend.
#[derive(Clone)]
struct Handler {
    info: ClientInfo,
    inbound: Inbound,
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
        if !self.inbound.caps.elicitation {
            return Ok(declined());
        }
        let answer = self.inbound.handler.as_ref().and_then(|h| {
            h.handle(inbound::Inbound::Elicit {
                message,
                requested_schema,
            })
        });
        Ok(match answer {
            Some(inbound::Answer::Accept(content)) => {
                ElicitResult::new(ElicitationAction::Accept).with_content(content)
            }
            Some(inbound::Answer::Decline) => declined(),
            // No handler, nothing to ask, or a roots answer to an elicitation:
            // cancel is the honest outcome, and it is not an error.
            _ => ElicitResult::new(ElicitationAction::Cancel),
        })
    }
}

/// A blocking MCP client backed by the official SDK.
pub struct RmcpClient {
    name: String,
    rt: tokio::runtime::Runtime,
    service: RunningService<RoleClient, Handler>,
    caps: ServerCapabilities,
    protocol_version: Option<String>,
    timeout: Duration,
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
    inbound: Inbound,
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
            inbound: Inbound {
                caps: inbound::Capabilities::default(),
                handler: None,
            },
        }
    }

    pub fn with_client_info(mut self, info: Implementation) -> Self {
        self.client_info = info;
        self
    }

    /// Declare `elicitation` and route it to `handler` — the same host seam the
    /// native backend uses, so `ask_human` is reached identically either way.
    pub fn with_elicitation(mut self, handler: Arc<dyn inbound::Handler>) -> Self {
        self.inbound.caps.elicitation = true;
        self.inbound.handler = Some(handler);
        self
    }

    /// Connect and run the `initialize` handshake.
    pub fn connect(self) -> Result<RmcpClient, McpError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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
        if self.inbound.caps.elicitation {
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
            // Ask for the newest revision this library knows, NOT rmcp's
            // conservative `LATEST` — the server negotiates down if it speaks
            // an older one, which is what the handshake is for.
            info: ClientInfo::new(caps, implementation)
                .with_protocol_version(ProtocolVersion::V_2026_07_28),
            inbound: self.inbound.clone(),
        };

        let name = self.name.clone();
        let service = rt
            .block_on(async move {
                let transport = StreamableHttpClientTransport::from_config(config);
                handler.serve(transport).await
            })
            .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?;

        let info = service.peer_info();
        let protocol_version = info.as_ref().map(|i| i.protocol_version.to_string());
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
            timeout: self.timeout,
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
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.caps
    }

    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
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

    pub fn call_tool(&self, name: &str, args: Option<Value>) -> Result<Value, McpError> {
        self.call_tool_with_meta(name, args, None)
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

    /// Subscribe to a resource.
    ///
    /// Under `2026-07-28` this is `subscriptions/listen`, not the legacy
    /// `resources/subscribe` — rmcp deprecates the latter for exactly the
    /// version we negotiate. One subscription covers every URI we track, so a
    /// new URI reopens it with the widened filter and the old handle is
    /// dropped; its notifications pump into the queue the host drains.
    pub fn subscribe(&self, uri: &str) -> Result<(), McpError> {
        {
            let mut uris = self.uris.lock().unwrap_or_else(|e| e.into_inner());
            if !uris.insert(uri.to_string()) {
                return Ok(()); // already covered by the live subscription
            }
        }
        self.relisten()
    }

    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        {
            let mut uris = self.uris.lock().unwrap_or_else(|e| e.into_inner());
            if !uris.remove(uri) {
                return Ok(());
            }
        }
        self.relisten()
    }

    /// (Re)open the single subscription covering every tracked URI, and pump its
    /// notifications into the drain queue on a background task.
    fn relisten(&self) -> Result<(), McpError> {
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

    /// Drain notifications the handler queued (same contract as the native
    /// client: take what has arrived, leave the queue empty).
    pub fn drain_notifications(&self) -> Vec<rpc::Notification> {
        std::mem::take(&mut *self.notifications.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// The configured per-request timeout, for parity with the native client.
    pub fn timeout(&self) -> Duration {
        self.timeout
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
    fn we_ask_for_the_newest_revision_we_know_not_rmcps_conservative_default() {
        // rmcp's ProtocolVersion::LATEST is the older stable; asking for it
        // would silently give up the stateless dialect this crate supports.
        let ours = ProtocolVersion::V_2026_07_28;
        assert_eq!(ours.to_string(), crate::version::LATEST_MODERN_VERSION);
        assert_ne!(ours.to_string(), ProtocolVersion::LATEST.to_string());
    }
}
