// SPDX-License-Identifier: Apache-2.0
//! MCP client over the **Streamable HTTP** transport. RFC 0004; RFC 0012 (no local
//! process spawn).
//!
//! One client connects one remote server (`https`/`http`/`unix`/`vsock`) and
//! implements the client subset from RFC 0004: initialize + capability store,
//! tools (list+call), resources (list+read), subscribe/unsubscribe, ping. We
//! declare **no** client capabilities.
//!
//! Each request is one POST of a JSON-RPC frame over a fresh connection (the
//! per-request socket timeout is the per-call bound); the response is
//! `application/json` or an SSE stream. Server→client notifications ride a
//! long-lived `GET` SSE stream, opened lazily on the first subscribe — a
//! background thread pumps them into a queue [`Self::drain_notifications`] serves.

use crate::http::{HttpError, HttpTransport, McpEndpoint};
use crate::inbound;
use crate::rpc::{self, RpcError};
use crate::wire::{
    CallToolResult, CompleteParams, CompleteResult, Era, GetPromptParams, GetPromptResult,
    Implementation, LATEST_MODERN_VERSION, ListResourceTemplatesResult, Prompt, ReadResourceResult,
    Resource, ResourceTemplate, ServerCapabilities, Task, Tool, as_task_result, method,
};
// The modern (stateless) request builders live alongside `wire` in the mcp crate.
use crate::modern;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Debug)]
pub enum McpError {
    Transport(String),
    /// A JSON-RPC error object from the server (protocol failure, distinct
    /// from a `tools/call` result with `isError: true`).
    Rpc(RpcError),
    /// No response within the per-request timeout.
    Timeout(String),
    /// The server doesn't advertise the capability the call needs.
    Capability(String),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::Transport(m) => write!(f, "mcp: transport: {m}"),
            McpError::Rpc(e) => write!(f, "mcp: rpc error {}: {}", e.code, e.message),
            McpError::Timeout(m) => write!(f, "mcp: timeout: {m}"),
            McpError::Capability(m) => write!(f, "mcp: capability: {m}"),
        }
    }
}
impl std::error::Error for McpError {}

type NotifQueue = Arc<Mutex<VecDeque<rpc::Notification>>>;

/// Routes an inbound JSON-RPC frame: a notification is queued for the reactor,
/// a **request** is answered and the response POSTed back.
///
/// MCP is bidirectional and this client used to be deaf in one direction —
/// every server→client request was dropped, including `ping`, which the spec
/// says both sides MUST answer. The router is shared by every path that can
/// receive a frame (the SSE event stream, the modern listen stream, and the
/// interleaved frames on any request's own response stream) so there is one
/// place that decides what an inbound frame means.
struct InboundRouter {
    http: Arc<HttpTransport>,
    queue: NotifQueue,
    caps: inbound::Capabilities,
    handler: Option<Arc<dyn inbound::Handler>>,
    timeout: Duration,
}

impl InboundRouter {
    fn route(&self, frame: Value) {
        if let Some(req) = inbound::as_request(&frame) {
            let resp = inbound::answer(&req, self.caps, self.handler.as_deref());
            // Best effort: a server that cannot take our answer is a server we
            // cannot help, and failing the caller's request over it would be
            // worse than the silence we are fixing.
            if let Ok(body) = serde_json::to_vec(&resp) {
                let _ = self.http.send(None, &body, self.timeout, &[], |_| {});
            }
            return;
        }
        queue_notification(&self.queue, frame);
    }
}

/// A connected (and, after [`McpClient::initialize`], handshaken) remote MCP
/// server over Streamable HTTP.
pub struct McpClient {
    name: String,
    http: Arc<HttpTransport>,
    /// Notifications queued from two sources: those captured off a POST's SSE
    /// response, and the long-lived server→client `GET` SSE stream (`events`).
    notifications: NotifQueue,
    /// The background notification-stream thread, started lazily on first
    /// subscribe (the reactive push channel — a `GET` stream on legacy, a
    /// `subscriptions/listen` POST stream on modern).
    events: Mutex<Option<EventStreamHandle>>,
    /// The resource URIs subscribed to. On modern this is the filter the
    /// `subscriptions/listen` stream is (re)opened with; legacy subscribes
    /// Cached tool `inputSchema`s (name → schema) from the last `tools/list`, so a
    /// modern `tools/call` can mirror `x-mcp-header`-annotated params into
    /// `Mcp-Param-*` headers (transports §custom-headers). Populated only with
    /// tools whose annotations validate.
    tool_schemas: Mutex<HashMap<String, Value>>,
    next_id: AtomicI64,
    caps: ServerCapabilities,
    /// The protocol version negotiated at `initialize`/discovery; `None` until then.
    protocol_version: Option<String>,
    /// The protocol era established on connect: legacy (`initialize` handshake) or
    /// modern (stateless per-request `_meta`). Governs how every request is built.
    era: Era,
    timeout: Duration,
    /// When compiled with `rmcp-client`, the official SDK answers every
    /// operation instead of the hand-rolled path below. Held here rather than
    /// behind an enum so callers keep taking `&[McpClient]` and nothing else in
    /// the tree has to know which backend is live.
    #[cfg(feature = "rmcp-client")]
    rmcp: Option<crate::rmcp_client::RmcpClient>,
    /// The endpoint + headers the SDK backend needs to build its own transport.
    /// Only carried when that backend is compiled in.
    #[cfg(feature = "rmcp-client")]
    endpoint: String,
    #[cfg(feature = "rmcp-client")]
    extra_headers: Vec<(String, String)>,
    /// Server→client requests we advertise an ability to answer, and the host
    /// callback that answers them. `ping` is answered regardless.
    inbound_caps: inbound::Capabilities,
    inbound_handler: Option<Arc<dyn inbound::Handler>>,
    /// Stamped into every `tools/call` request's `params._meta` (e.g.
    /// `{"agent/run_id": …}`) so backing services can dedupe retries
    /// (RFC 0011 §idempotency).
    tool_meta: Option<Value>,
    /// The client identity sent in `initialize` (legacy) / every request's `_meta`
    /// (modern). Defaults to this crate's identity; the host overrides it via
    /// [`Self::with_client_info`] (agentd sets its own name + version).
    client_info: Implementation,
    /// The client capabilities advertised in the modern per-request `_meta` (e.g.
    /// the tasks extension). Defaults to `{}` (none); [`Self::with_tasks`] opts in.
    client_capabilities: Value,
}

/// The background notification-stream thread + its stop flag (RFC 0004 §GET SSE).
struct EventStreamHandle {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl McpClient {
    /// Connect to a remote MCP server over Streamable HTTP (RFC 0004). `endpoint`
    /// is `https://…` / `http://…` / `unix:/path` / `vsock:cid:port`. `headers`
    /// are caller-owned request headers (auth/framing — resolved secret values,
    /// never templates or logs). No process is spawned (RFC 0012). Call
    /// [`Self::initialize`] before any tool/resource call.
    pub fn connect(
        name: &str,
        endpoint: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<McpClient, McpError> {
        Self::connect_signed(name, endpoint, headers, timeout, None)
    }

    /// [`Self::connect`] with an optional per-request AAuth signer (RFC 0023) —
    /// every outbound request to this server is signed. `None` = unsigned (the
    /// `connect` default).
    pub fn connect_signed(
        name: &str,
        endpoint: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
        signer: Option<Arc<dyn crate::http::RequestSigner>>,
    ) -> Result<McpClient, McpError> {
        let ep = McpEndpoint::parse(endpoint)
            .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?;
        #[cfg(feature = "rmcp-client")]
        Ok(McpClient {
            name: name.to_string(),
            http: Arc::new(HttpTransport::new(ep, headers.clone()).with_signer(signer)),
            notifications: Arc::new(Mutex::new(VecDeque::new())),
            events: Mutex::new(None),
            tool_schemas: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            caps: ServerCapabilities::default(),
            protocol_version: None,
            // Established on connect; legacy is the safe default until then.
            era: Era::Legacy,
            timeout,
            #[cfg(feature = "rmcp-client")]
            rmcp: None,
            #[cfg(feature = "rmcp-client")]
            endpoint: endpoint.to_string(),
            #[cfg(feature = "rmcp-client")]
            extra_headers: headers,
            inbound_caps: inbound::Capabilities::default(),
            inbound_handler: None,
            tool_meta: None,
            client_info: Implementation {
                name: "agentd".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
            },
            client_capabilities: json!({}),
        })
    }

    /// Override the client identity sent to servers (name + version). agentd sets
    /// its own; other hosts of the `mcp` crate set theirs.
    pub fn with_client_info(mut self, info: Implementation) -> Self {
        self.client_info = info;
        self
    }

    /// Answer server→client **elicitation** requests through `handler`: a server
    /// may ask the operator a question mid-call and get a typed answer back.
    /// Declares the `elicitation` client capability, so a server only asks when
    /// we can actually deliver the question to a human.
    pub fn with_elicitation(mut self, handler: Arc<dyn inbound::Handler>) -> Self {
        self.inbound_caps.elicitation = true;
        self.inbound_handler = Some(handler);
        self
    }

    /// Answer `roots/list` through `handler` — the URI roots this client permits
    /// a server to operate on. Declares the `roots` capability.
    pub fn with_roots(mut self, handler: Arc<dyn inbound::Handler>) -> Self {
        self.inbound_caps.roots = true;
        self.inbound_handler = Some(handler);
        self
    }

    /// A router over this client's transport + inbound policy, cloneable into
    /// the background streams.
    fn router(&self) -> Arc<InboundRouter> {
        Arc::new(InboundRouter {
            http: Arc::clone(&self.http),
            queue: Arc::clone(&self.notifications),
            caps: self.inbound_caps,
            handler: self.inbound_handler.clone(),
            timeout: self.timeout,
        })
    }

    /// The full client capability object sent in the handshake / per-request
    /// `_meta`: the declared inbound capabilities merged with any extensions.
    fn declared_capabilities(&self) -> Value {
        let mut caps = self.client_capabilities.clone();
        let inbound = self.inbound_caps.to_json();
        match (caps.as_object_mut(), inbound.as_object()) {
            (Some(dst), Some(src)) => {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
                Value::Object(dst.clone())
            }
            _ => caps,
        }
    }

    /// Advertise support for the **tasks extension** (`io.modelcontextprotocol/
    /// tasks`) — a server may then return an async task handle from a supported
    /// request instead of blocking (poll it with [`Self::get_task`]).
    pub fn with_tasks(mut self) -> Self {
        self.client_capabilities = json!({
            "extensions": { crate::wire::TASKS_EXTENSION: {} }
        });
        self
    }

    /// Attach a mutual-TLS client identity (a mounted cert chain + key) for a
    /// `https://` endpoint. A no-op on non-TLS endpoints (the identity is only
    /// presented during the TLS handshake). RFC 0012 §3.7: the key never leaves
    /// the process (see [`net::tls`]).
    #[cfg(feature = "tls")]
    pub fn with_identity(mut self, identity: net::tls::ClientIdentity) -> Self {
        // The Arc is unshared here (called right after connect, before the event
        // thread), so get_mut succeeds; a no-op if it were somehow already shared.
        if let Some(h) = Arc::get_mut(&mut self.http) {
            h.set_identity(Some(identity));
        }
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.caps
    }

    /// Set the `_meta` stamped onto every `tools/call` (e.g. the run id, for
    /// retry dedup). Call after `initialize`. RFC 0011 §idempotency.
    pub fn set_tool_meta(&mut self, meta: Value) {
        self.tool_meta = Some(meta);
    }

    /// MCP lifecycle handshake: `initialize` → store capabilities →
    /// `notifications/initialized`. Uses the default per-request timeout.
    pub fn initialize(&mut self) -> Result<(), McpError> {
        self.initialize_within(self.timeout)
    }

    /// [`Self::initialize`] with a caller-supplied timeout for the `initialize`
    /// round-trip (the SHORT management bound, RFC 0016 §10). Used by the
    /// hot-reload re-handshake, which adds a server ON the reactor thread mid-loop:
    /// a slow-but-alive added server must not block the reactor (and starve the
    /// liveness heartbeat) for the full ~60s — a timeout is a contained
    /// `mcp.connect.fail` (the server is simply absent, RFC 0007 / RFC 0017 §5.3).
    pub fn initialize_within(&mut self, timeout: Duration) -> Result<(), McpError> {
        // The SDK owns the handshake and every operation after it — over *this*
        // connection's transport, so a request signer (AAuth's challenge loop,
        // AWS SigV4), an mTLS client identity and the SSRF guard all still
        // apply. Adopting the SDK cost none of them.
        #[cfg(feature = "rmcp-client")]
        {
            let mut b = crate::rmcp_client::RmcpBuilder::new(
                &self.name,
                &self.endpoint,
                self.extra_headers.clone(),
                timeout,
            )
            .with_http(Arc::clone(&self.http))
            .with_client_info(self.client_info.clone());
            if self.inbound_caps.elicitation
                && let Some(h) = &self.inbound_handler
            {
                b = b.with_elicitation(Arc::clone(h));
            }
            let c = b.connect()?;
            self.caps = c.capabilities().clone();
            self.protocol_version = c.protocol_version().map(str::to_string);
            // Report the era the SDK actually negotiated, not an assumption:
            // rmcp currently pins `LATEST` to a legacy revision, and callers
            // branch on this.
            self.era = c
                .protocol_version()
                .map(crate::version::era_of)
                .unwrap_or(Era::Legacy);
            self.rmcp = Some(c);
            Ok(())
        }
    }

    /// The protocol era established on connect (legacy handshake vs modern
    /// stateless). Governs how each request is built.
    pub fn era(&self) -> Era {
        self.era
    }

    /// The protocol version negotiated with the server (`None` before connect).
    /// Sent as `MCP-Protocol-Version` on every subsequent request.
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// `tools/list`, following cursor pagination to completion. Empty when the
    /// server doesn't advertise `tools`. Uses the default per-request timeout.
    pub fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        self.list_tools_within(self.timeout)
    }

    /// `tools/list` with a caller-supplied per-request timeout (the SHORT
    /// management bound, RFC 0016 §10) instead of the default ~60s. Used by the
    /// reactor-thread management path (hot-reload re-handshake, claim coordination
    /// re-validation) so a slow-but-alive coordination server cannot outrun the
    /// liveness heartbeat. A timeout surfaces as the usual [`McpError::Timeout`],
    /// which the callers already treat as a best-effort failure. The timeout is
    /// applied to EACH page (each pagination round-trip is bounded), matching the
    /// per-request contract of [`Self::request_with_timeout`].
    pub fn list_tools_within(&self, _timeout: Duration) -> Result<Vec<Tool>, McpError> {
        #[cfg(feature = "rmcp-client")]
        let Some(c) = &self.rmcp else {
            return Err(McpError::Transport(
                "the MCP connection is not established".into(),
            ));
        };
        c.list_tools()
    }

    /// `tools/call`. The returned [`CallToolResult`] carries `isError` (a
    /// tool-domain failure observation) — distinct from an `Err` here, which
    /// is a transport/protocol failure (RFC 0004 §isError).
    pub fn call_tool(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<CallToolResult, McpError> {
        // The tool call is the hot path; the SDK owns the whole round trip
        // (including its own `_meta` handling) when it is the live backend.
        #[cfg(feature = "rmcp-client")]
        let Some(c) = &self.rmcp else {
            return Err(McpError::Transport(
                "the MCP connection is not established".into(),
            ));
        };
        let raw = c.call_tool_with_meta(name, arguments.clone(), None)?;
        serde_json::from_value(raw).map_err(|e| {
            McpError::Transport(format!("bad tools/call result on '{}': {e}", self.name))
        })
    }

    /// `tools/call` with **per-call** `_meta` merged on top of the persistent
    /// [`Self::set_tool_meta`] for this one call only — without mutating the
    /// stored meta. Used by the work-claim client (RFC 0019 §3 / RFC 0015 §5.6),
    /// where `agent/claim_key` is per-item and must ride the individual call,
    /// never the persistent stamp. `extra_meta` (an object) wins key-by-key over
    /// the persistent meta; a non-object `extra_meta` replaces it. The persistent
    /// meta is left untouched.
    pub fn call_tool_with_meta(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Value,
    ) -> Result<CallToolResult, McpError> {
        self.call_tool_with_meta_within(name, arguments, extra_meta, self.timeout)
    }

    /// `tools/call` with per-call `_meta` AND a caller-supplied per-request
    /// timeout (the SHORT management bound, RFC 0016 §10) instead of the default
    /// ~60s. Used by the reactor-thread lease management path (claim
    /// renew/ack/release) — a slow coordination server must not block the reactor
    /// past the liveness staleness window. Behaviour is otherwise identical to
    /// [`Self::call_tool_with_meta`]; a timeout surfaces as [`McpError::Timeout`],
    /// which the lease callers already treat as a best-effort failure. The data
    /// path (subagent tool calls) never uses this — it keeps the default timeout.
    pub fn call_tool_with_meta_within(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Value,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        #[cfg(feature = "rmcp-client")]
        let Some(c) = &self.rmcp else {
            return Err(McpError::Transport(
                "the MCP connection is not established".into(),
            ));
        };
        let _ = timeout; // the SDK owns its own per-request deadline
        let raw = c.call_tool_with_meta(name, arguments.clone(), Some(extra_meta.clone()))?;
        serde_json::from_value(raw).map_err(|e| {
            McpError::Transport(format!("bad tools/call result on '{}': {e}", self.name))
        })
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, McpError> {
        #[cfg(feature = "rmcp-client")]
        let Some(c) = &self.rmcp else {
            return Err(McpError::Transport(
                "the MCP connection is not established".into(),
            ));
        };
        c.list_resources()
    }

    /// `prompts/list`, following cursor pagination to completion. Empty when the
    /// server doesn't advertise `prompts`.
    pub fn list_prompts(&self) -> Result<Vec<Prompt>, McpError> {
        #[cfg(feature = "rmcp-client")]
        let Some(c) = &self.rmcp else {
            return Err(McpError::Transport(
                "the MCP connection is not established".into(),
            ));
        };
        c.list_prompts()
    }

    /// `prompts/get` — render the named prompt template with `arguments` (a flat
    /// string map). Gated on the server advertising `prompts`.
    pub fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<GetPromptResult, McpError> {
        if !self.caps.supports_prompts() {
            return Err(McpError::Capability(format!(
                "server '{}' has no prompts",
                self.name
            )));
        }
        let params = GetPromptParams {
            name: name.to_string(),
            arguments,
        };
        self.request_as(method::PROMPTS_GET, Some(to_value(&params)))
    }

    /// `completion/complete` — argument autocompletion for a prompt / resource-
    /// template `reference`. Gated on the server advertising `completions`.
    pub fn complete(&self, reference: Value, argument: Value) -> Result<CompleteResult, McpError> {
        if !self.caps.supports_completions() {
            return Err(McpError::Capability(format!(
                "server '{}' has no completions",
                self.name
            )));
        }
        let params = CompleteParams {
            reference,
            argument,
            context: None,
        };
        self.request_as(method::COMPLETION_COMPLETE, Some(to_value(&params)))
    }

    /// `resources/templates/list`, paginated. Empty when the server doesn't
    /// advertise `resources`.
    pub fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        if !self.caps.supports_resources() {
            return Ok(Vec::new());
        }
        let mut templates = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let page: ListResourceTemplatesResult =
                self.request_as(method::RESOURCES_TEMPLATES_LIST, params)?;
            templates.extend(page.resource_templates);
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(templates)
    }

    /// `ping` — a liveness round-trip (RFC 0004 §utilities). Returns `Ok(())` if
    /// the server answers within the default timeout.
    pub fn ping(&self) -> Result<(), McpError> {
        self.request_with_timeout(method::PING, None, self.timeout)?;
        Ok(())
    }

    // ---- tasks extension (io.modelcontextprotocol/tasks) ----

    /// If `result` is a task handle (`resultType: "task"`, the async shape a
    /// task-augmented request returns instead of blocking), parse it. Enable the
    /// extension with [`Self::with_tasks`]; poll the handle with [`Self::get_task`].
    pub fn as_task(&self, result: &Value) -> Option<Task> {
        as_task_result(result)
    }

    /// `tasks/get` — poll one async task's current state (the tasks extension).
    pub fn get_task(&self, task_id: &str) -> Result<Task, McpError> {
        self.request_as(method::TASKS_GET, Some(json!({ "taskId": task_id })))
    }

    /// `tasks/update` — supply `inputResponses` for a task in `input_required`
    /// (the MRTR fulfilment path). Acknowledged with an empty result.
    pub fn update_task(&self, task_id: &str, input_responses: Value) -> Result<(), McpError> {
        self.request_with_timeout(
            method::TASKS_UPDATE,
            Some(json!({ "taskId": task_id, "inputResponses": input_responses })),
            self.timeout,
        )?;
        Ok(())
    }

    /// `tasks/cancel` — request cancellation of a task (cooperative; the server may
    /// still reach a non-`cancelled` terminal state). Acknowledged with an empty
    /// result.
    pub fn cancel_task(&self, task_id: &str) -> Result<(), McpError> {
        self.request_with_timeout(
            method::TASKS_CANCEL,
            Some(json!({ "taskId": task_id })),
            self.timeout,
        )?;
        Ok(())
    }

    /// Poll `tasks/get` until the task reaches a terminal status or `deadline`,
    /// honoring the server's `pollIntervalMs` (bounded to a sane window). Returns
    /// the terminal [`Task`] (the caller reads `result`/`error`); a task that stops
    /// on `input_required` is returned so the caller can drive the MRTR loop.
    pub fn await_task(
        &self,
        task_id: &str,
        deadline: std::time::Instant,
    ) -> Result<Task, McpError> {
        loop {
            let task = self.get_task(task_id)?;
            if task.is_terminal() || task.needs_input() {
                return Ok(task);
            }
            if std::time::Instant::now() >= deadline {
                return Err(McpError::Timeout(format!(
                    "task '{task_id}' on '{}' did not finish before the deadline",
                    self.name
                )));
            }
            let poll = task.poll_interval_ms.unwrap_or(500).clamp(50, 5_000);
            std::thread::sleep(Duration::from_millis(poll));
        }
    }

    pub fn read_resource(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        self.read_resource_within(uri, self.timeout)
    }

    /// `resources/read` with a caller-supplied per-request timeout (the SHORT
    /// management bound, RFC 0016 §10) instead of the default ~60s. The reactor
    /// thread's notify-then-read (`read_current`) blocks on this; a slow-but-alive
    /// resource server must not outrun the liveness heartbeat. A timeout surfaces
    /// as [`McpError::Timeout`]; the level-triggered reactor treats a timed-out
    /// read exactly like any read failure (act on empty / skip), so a transient
    /// slow read is recovered on the next `updated` notification or re-read.
    pub fn read_resource_within(
        &self,
        uri: &str,
        _timeout: Duration,
    ) -> Result<ReadResourceResult, McpError> {
        #[cfg(feature = "rmcp-client")]
        let Some(c) = &self.rmcp else {
            return Err(McpError::Transport(
                "the MCP connection is not established".into(),
            ));
        };
        c.read_resource(uri)
    }

    /// `resources/subscribe` — gated on the server advertising it (RFC 0004).
    pub fn subscribe(&self, uri: &str) -> Result<(), McpError> {
        self.subscribe_within(uri, self.timeout)
    }

    /// [`Self::subscribe`] with a caller-supplied timeout (the SHORT management
    /// bound, RFC 0016 §10) — for the reactor-thread reload re-handshake, where a
    /// slow-but-alive server arming a subscription must not block the reactor.
    pub fn subscribe_within(&self, uri: &str, _timeout: Duration) -> Result<(), McpError> {
        #[cfg(feature = "rmcp-client")]
        let Some(c) = &self.rmcp else {
            return Err(McpError::Transport(
                "the MCP connection is not established".into(),
            ));
        };
        c.subscribe(uri)
    }

    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        self.unsubscribe_within(uri, self.timeout)
    }

    /// [`Self::unsubscribe`] with a caller-supplied timeout (the SHORT management
    /// bound, RFC 0016 §10) — for the reactor-thread reload reconcile + the drain
    /// unsubscribe, both best-effort: a slow server here must not block the reactor
    /// or the drain past the liveness window / drain budget.
    pub fn unsubscribe_within(&self, uri: &str, _timeout: Duration) -> Result<(), McpError> {
        #[cfg(feature = "rmcp-client")]
        let Some(c) = &self.rmcp else {
            return Err(McpError::Transport(
                "the MCP connection is not established".into(),
            ));
        };
        c.unsubscribe(uri)
    }

    /// Drain any notifications queued since the last drain (e.g.
    /// `notifications/resources/updated`). The reactive router
    /// (`triggers/mode.rs`) drains these between runs to drive re-reactions.
    pub fn drain_notifications(&self) -> Vec<rpc::Notification> {
        #[cfg(feature = "rmcp-client")]
        let Some(c) = &self.rmcp else {
            return Vec::new();
        };
        c.drain_notifications()
    }

    // ---- internals ----

    fn request_as<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<T, McpError> {
        self.request_as_within(method, params, self.timeout)
    }

    fn request_as_within<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<T, McpError> {
        let v = self.request_with_timeout(method, params, timeout)?;
        serde_json::from_value(v)
            .map_err(|e| McpError::Transport(format!("bad {method} result: {e}")))
    }

    /// Send one JSON-RPC request over a fresh HTTP connection and return the
    /// matching response (`timeout` is the socket connect+read bound). The
    /// default-timeout callers delegate here with `self.timeout`; the reactor-
    /// thread management path passes the SHORT bound (RFC 0016 §10) so a slow-but-
    /// alive server cannot block the reactor past the liveness window.
    fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // In the MODERN era, every request carries per-request `_meta` and the
        // Mcp-Method / Mcp-Name routing headers; legacy sends plain params.
        let (params, routing) = if self.era == Era::Modern {
            let mut p = params.unwrap_or_else(|| json!({}));
            let version = self
                .protocol_version
                .as_deref()
                .unwrap_or(LATEST_MODERN_VERSION);
            modern::inject_client_meta(
                &mut p,
                version,
                &self.client_info,
                &self.declared_capabilities(),
            );
            let mut routing: Vec<(String, String)> = modern::routing_headers(method, &p)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            // x-mcp-header (transports §custom-headers): mirror `tools/call` params
            // annotated in the tool's cached inputSchema into `Mcp-Param-*` headers.
            if method == method::TOOLS_CALL
                && let Some(name) = p.get("name").and_then(Value::as_str)
            {
                let schema = self
                    .tool_schemas
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(name)
                    .cloned();
                if let Some(schema) = schema {
                    let args = p.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    routing.extend(modern::param_headers(&schema, &args));
                }
            }
            (Some(p), routing)
        } else {
            (params, Vec::new())
        };
        let refs: Vec<(&str, &str)> = routing
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let req = rpc::Request::new(id, method, params);
        let body = serde_json::to_vec(&req)
            .map_err(|e| McpError::Transport(format!("encode {method}: {e}")))?;
        let router = self.router();
        let msg = self
            .http
            .send(Some(id), &body, timeout, &refs, |n| router.route(n))
            .map_err(|e| http_err(&self.name, method, e))?
            .ok_or_else(|| {
                McpError::Transport(format!("no response to {method} on '{}'", self.name))
            })?;
        let resp: rpc::Response = serde_json::from_value(msg).map_err(|e| {
            McpError::Transport(format!("bad {method} response on '{}': {e}", self.name))
        })?;
        match resp.error {
            Some(err) => Err(McpError::Rpc(err)),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Stop the notification thread: set its stop flag; it wakes within
        // EVENT_READ_TIMEOUT (its read bound) and exits. The per-request
        // connections open+close themselves, so there is nothing else to reap.
        if let Some(ev) = self
            .events
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            ev.stop.store(true, Ordering::SeqCst);
            let _ = ev.handle.join();
        }
    }
}

/// Map a [`HttpError`] onto the client's error domain, folding socket timeouts
/// into [`McpError::Timeout`] so the management-timeout callers (which treat a
/// timeout as a best-effort failure) behave identically across the request path.
fn http_err(name: &str, method: &str, e: HttpError) -> McpError {
    use std::io::ErrorKind;
    match e {
        HttpError::Connect(io) | HttpError::Http(io) => match io.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => {
                McpError::Timeout(format!("{method} on '{name}'"))
            }
            _ => McpError::Transport(format!("{method} on '{name}': {io}")),
        },
        HttpError::Status(code, _) => {
            McpError::Transport(format!("{method} on '{name}': server returned HTTP {code}"))
        }
        HttpError::Unsupported(m) => McpError::Transport(m),
        HttpError::NoResponse => {
            McpError::Transport(format!("{method} on '{name}': no JSON-RPC response"))
        }
    }
}

/// Queue a raw notification Value captured off an HTTP response or the GET SSE
/// stream (a JSON-RPC message with no matching request id). Non-notification
/// frames (e.g. a server→client request) that don't deserialize are dropped — v1
/// declares no client capabilities, so there is nothing to answer.
fn queue_notification(queue: &Mutex<VecDeque<rpc::Notification>>, n: Value) {
    if let Ok(note) = serde_json::from_value::<rpc::Notification>(n) {
        queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(note);
    }
}

fn to_value<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn error_display() {
        let e = McpError::Timeout("tools/call on 'fs'".into());
        assert!(e.to_string().contains("timeout"));
    }

    #[test]
    fn http_err_folds_socket_timeout_into_timeout_variant() {
        use std::io::{Error, ErrorKind};
        let e = http_err(
            "fs",
            "tools/call",
            HttpError::Http(Error::new(ErrorKind::WouldBlock, "read timed out")),
        );
        assert!(matches!(e, McpError::Timeout(_)), "got {e:?}");
        // A non-2xx HTTP status is a transport error, not a timeout.
        let e = http_err("fs", "initialize", HttpError::Status(503, Vec::new()));
        assert!(matches!(e, McpError::Transport(_)), "got {e:?}");
    }

    #[test]
    fn queue_notification_enqueues_notifications_and_drops_others() {
        let q = Mutex::new(VecDeque::new());
        // A real notification is queued.
        queue_notification(
            &q,
            json!({"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"x"}}),
        );
        // A response frame (has id, no method) is not a notification → dropped.
        queue_notification(&q, json!({"jsonrpc":"2.0","id":1,"result":{}}));
        let drained: Vec<_> = q.lock().unwrap().drain(..).collect();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].method, "notifications/resources/updated");
    }

    #[test]
    fn connect_rejects_a_bad_endpoint() {
        // McpClient isn't Debug, so match the Result rather than unwrap_err().
        match McpClient::connect("bad", "ftp://nope/", Vec::new(), Duration::from_secs(1)) {
            Err(McpError::Transport(_)) => {}
            Err(other) => panic!("expected a Transport error, got {other:?}"),
            Ok(_) => panic!("expected connect to reject an unsupported scheme"),
        }
    }

    /// A unix listener that ACCEPTS a connection but never replies — an alive-but-
    /// silent server, to prove the per-request timeout governs (not a hang).
    fn spawn_silent_server() -> (String, std::thread::JoinHandle<()>) {
        let path = std::env::temp_dir().join(format!(
            "agentd-mcp-silent-{}-{}.sock",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind silent server");
        let handle = std::thread::spawn(move || {
            // Accept connections and hold them open, reading forever (never reply).
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 256];
                    while let Ok(n) = stream.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                    }
                });
            }
        });
        (format!("unix:{}", path.display()), handle)
    }

    #[test]
    fn management_timeout_bounds_a_call_on_a_silent_server() {
        // The server accepts but never replies; a request with the SHORT management
        // bound must return a Timeout fast — the per-call timeout, not a hang.
        let (endpoint, _srv) = spawn_silent_server();
        let client = McpClient::connect("silent", &endpoint, Vec::new(), Duration::from_secs(60))
            .expect("connect");

        let short = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let r = client.request_with_timeout("ping", None, short);
        let elapsed = started.elapsed();
        assert!(
            matches!(r, Err(McpError::Timeout(_))),
            "expected a Timeout within the short bound, got {r:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the short per-call timeout must govern (took {elapsed:?})"
        );
    }

    #[test]
    fn write_read_smoke_for_unix_stream() {
        // Guard that the test transport helpers are wired (a trivial round-trip),
        // so a future refactor of spawn_silent_server fails loudly here.
        let path = std::env::temp_dir().join(format!("agentd-smoke-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let p2 = path.clone();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.write_all(b"hi");
        });
        let mut c = UnixStream::connect(&p2).unwrap();
        let mut buf = [0u8; 2];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
        h.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
