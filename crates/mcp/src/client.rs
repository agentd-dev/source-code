// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP client agentd's runtime holds: one remote server over **Streamable
//! HTTP**, reached over a socket or not at all — no local process is spawned.
//!
//! [`McpClient`] is a connection *builder* until [`McpClient::initialize`]: it
//! collects the endpoint, the caller's headers, a request signer, an mTLS
//! identity and the elicitation handler, then hands them to the official SDK
//! ([`crate::rmcp_client`]), which runs the handshake and answers every
//! operation after it over agentd's own socket ([`crate::http`]).

use crate::http::{HttpTransport, McpEndpoint};
use crate::inbound;
use crate::rpc::{self, RpcError};
use crate::wire::{
    CallToolResult, CompleteResult, GetPromptResult, Implementation, Prompt, ReadResourceResult,
    Resource, ResourceTemplate, ServerCapabilities, Tool,
};
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
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

/// A connected (and, after [`McpClient::initialize`], handshaken) remote MCP
/// server over Streamable HTTP.
pub struct McpClient {
    name: String,
    http: Arc<HttpTransport>,
    caps: ServerCapabilities,
    /// The protocol version negotiated at `initialize`; `None` until then.
    protocol_version: Option<String>,
    timeout: Duration,
    /// The official SDK, which answers every operation. `None` before
    /// `initialize`; this type is a connection *builder* until then, and every
    /// operation on an unconnected client is a transport error rather than a
    /// panic.
    rmcp: Option<crate::rmcp_client::RmcpClient>,
    /// What the SDK needs to build its side of the connection. The socket
    /// itself is `http` above — that is how a request signer and an mTLS
    /// identity survive the SDK owning the protocol.
    endpoint: String,
    extra_headers: Vec<(String, String)>,
    /// The host callback that answers a server's `elicitation/create`. Present
    /// iff the `elicitation` client capability is declared, so a server only
    /// asks what the host can actually deliver to a human.
    elicitation: Option<Arc<dyn inbound::Handler>>,
    /// Stamped into every `tools/call` request's `params._meta` (e.g.
    /// `{"agent/run_id": …}`) so a backing service can recognize a retried call
    /// as the same logical operation and dedupe it rather than repeating a side
    /// effect.
    tool_meta: Option<Value>,
    /// The client identity sent in `initialize`. Defaults to this crate's
    /// identity; the host overrides it via [`Self::with_client_info`] (agentd
    /// sets its own name + version).
    client_info: Implementation,
}

impl McpClient {
    /// Connect to a remote MCP server over Streamable HTTP. `endpoint` is
    /// `https://…` or `http://…`. `headers` are caller-owned request headers
    /// (auth/framing — already-resolved secret values, never templates, and
    /// never logged). No process is spawned. Call [`Self::initialize`] before
    /// any tool/resource call.
    pub fn connect(
        name: &str,
        endpoint: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<McpClient, McpError> {
        Self::connect_signed(name, endpoint, headers, timeout, None)
    }

    /// [`Self::connect`] with an optional per-request AAuth signer — every
    /// outbound request to this server is then signed, including the long-lived
    /// notification stream. `None` = unsigned (the `connect` default).
    pub fn connect_signed(
        name: &str,
        endpoint: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
        signer: Option<Arc<dyn crate::http::RequestSigner>>,
    ) -> Result<McpClient, McpError> {
        let ep = McpEndpoint::parse(endpoint)
            .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?;
        Ok(McpClient {
            name: name.to_string(),
            http: Arc::new(HttpTransport::new(ep, headers.clone()).with_signer(signer)),
            caps: ServerCapabilities::default(),
            protocol_version: None,
            timeout,
            rmcp: None,
            endpoint: endpoint.to_string(),
            extra_headers: headers,
            elicitation: None,
            tool_meta: None,
            client_info: Implementation {
                name: "agentd".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
            },
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
        self.elicitation = Some(handler);
        self
    }

    /// Attach a mutual-TLS client identity (a mounted cert chain + key) for a
    /// `https://` endpoint. A no-op on non-TLS endpoints (the identity is only
    /// presented during the TLS handshake). The private key is read from a
    /// mounted file and never leaves the process — it is not logged, rendered
    /// into an error, or copied onto the wire (see [`net::tls`]).
    #[cfg(feature = "tls")]
    pub fn with_identity(mut self, identity: net::tls::ClientIdentity) -> Self {
        // The Arc is unshared here (called right after connect, before the SDK
        // holds the socket), so get_mut succeeds; a no-op if it were somehow
        // already shared.
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

    /// Set the `_meta` stamped onto every `tools/call` (e.g. the run id), so a
    /// server can recognize a retried call and dedupe its side effect. Call
    /// after `initialize`.
    pub fn set_tool_meta(&mut self, meta: Value) {
        self.tool_meta = Some(meta);
    }

    /// MCP lifecycle handshake: `initialize` → store capabilities →
    /// `notifications/initialized`. Uses the default per-request timeout.
    pub fn initialize(&mut self) -> Result<(), McpError> {
        self.initialize_within(self.timeout)
    }

    /// [`Self::initialize`] with a caller-supplied timeout for the `initialize`
    /// round-trip — the SHORT management bound. Used by the hot-reload
    /// re-handshake, which adds a server ON the reactor thread mid-loop: a
    /// slow-but-alive added server must not block the reactor (and starve the
    /// liveness heartbeat) for the full default bound. A timeout is contained,
    /// reported as `mcp.connect.fail`, and the server is simply treated as
    /// absent rather than failing the reload.
    pub fn initialize_within(&mut self, timeout: Duration) -> Result<(), McpError> {
        // The SDK owns the handshake and every operation after it — over *this*
        // connection's transport, so a request signer (AAuth's challenge loop,
        // AWS SigV4) and an mTLS client identity still apply. Adopting the SDK
        // cost neither of them.
        let mut b = crate::rmcp_client::RmcpBuilder::new(
            &self.name,
            &self.endpoint,
            self.extra_headers.clone(),
            timeout,
        )
        .with_http(Arc::clone(&self.http))
        .with_client_info(self.client_info.clone());
        if let Some(h) = &self.elicitation {
            b = b.with_elicitation(Arc::clone(h));
        }
        let c = b.connect()?;
        self.caps = c.capabilities().clone();
        self.protocol_version = c.protocol_version().map(str::to_string);
        self.rmcp = Some(c);
        Ok(())
    }

    /// The protocol version negotiated with the server (`None` before connect).
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// The SDK connection, or the error every operation on an unconnected
    /// client reports.
    fn sdk(&self) -> Result<&crate::rmcp_client::RmcpClient, McpError> {
        self.rmcp
            .as_ref()
            .ok_or_else(|| McpError::Transport("the MCP connection is not established".into()))
    }

    /// `tools/list`, following cursor pagination to completion.
    pub fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        self.sdk()?.list_tools()
    }

    /// `tools/call`. The returned [`CallToolResult`] carries `isError` (a
    /// tool-domain failure the model sees as an observation) — distinct from an
    /// `Err` here, which is a transport/protocol failure and fails the call.
    pub fn call_tool(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<CallToolResult, McpError> {
        // The tool call is the hot path; the SDK owns the whole round trip
        // (including its own `_meta` handling).
        let raw = self.sdk()?.call_tool_with_meta(name, arguments, None)?;
        serde_json::from_value(raw).map_err(|e| {
            McpError::Transport(format!("bad tools/call result on '{}': {e}", self.name))
        })
    }

    /// `tools/call` with **per-call** `_meta` merged on top of the persistent
    /// [`Self::set_tool_meta`] for this one call only — without mutating the
    /// stored meta. Used by the work-claim client, where `agent/claim_key`
    /// identifies one work item and must ride only that call — stamping it
    /// persistently would attach one item's key to every later call.
    /// `extra_meta` (an object) wins key-by-key over
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
    /// timeout — the SHORT management bound rather than the long data-path
    /// default. Used by the reactor-thread lease management path (claim
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
        let c = self.sdk()?;
        let _ = timeout; // the SDK owns its own per-request deadline
        let raw = c.call_tool_with_meta(name, arguments.clone(), Some(extra_meta.clone()))?;
        serde_json::from_value(raw).map_err(|e| {
            McpError::Transport(format!("bad tools/call result on '{}': {e}", self.name))
        })
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, McpError> {
        self.sdk()?.list_resources()
    }

    /// `prompts/list`, following cursor pagination to completion.
    pub fn list_prompts(&self) -> Result<Vec<Prompt>, McpError> {
        self.sdk()?.list_prompts()
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
        self.sdk()?.get_prompt(name, arguments)
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
        self.sdk()?.complete(reference, argument)
    }

    /// `resources/templates/list`, paginated. Empty when the server doesn't
    /// advertise `resources`.
    pub fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        if !self.caps.supports_resources() {
            return Ok(Vec::new());
        }
        // The SDK walks the cursor; pagination is its problem, not ours.
        self.sdk()?.list_resource_templates()
    }

    /// `resources/read`. Always one request to the server: the SDK's response
    /// cache is disabled at connect, so an unreachable server or a JSON-RPC error
    /// is an `Err`, never an earlier answer.
    pub fn read_resource(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        self.sdk()?.read_resource(uri)
    }

    /// Subscribe to `uri`'s updates, by whichever mechanism the negotiated
    /// revision defines (see [`crate::rmcp_client::RmcpClient::subscribe`]).
    pub fn subscribe(&self, uri: &str) -> Result<(), McpError> {
        self.sdk()?.subscribe(uri)
    }

    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        self.sdk()?.unsubscribe(uri)
    }

    /// Drain any notifications queued since the last drain (e.g.
    /// `notifications/resources/updated`). The reactive router
    /// (`triggers/mode.rs`) drains these between runs to drive re-reactions.
    pub fn drain_notifications(&self) -> Vec<rpc::Notification> {
        match &self.rmcp {
            Some(c) => c.drain_notifications(),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn error_display() {
        let e = McpError::Timeout("tools/call on 'fs'".into());
        assert!(e.to_string().contains("timeout"));
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

    #[test]
    fn an_operation_before_initialize_is_an_error_not_a_panic() {
        let c = McpClient::connect(
            "early",
            "http://127.0.0.1:1/mcp",
            Vec::new(),
            Duration::from_secs(1),
        )
        .expect("connect only parses");
        assert!(matches!(c.list_tools(), Err(McpError::Transport(_))));
        assert!(c.drain_notifications().is_empty());
    }

    /// A loopback TCP listener that ACCEPTS a connection but never replies — an
    /// alive-but-silent server, to prove the per-request timeout governs (not a
    /// hang).
    fn spawn_silent_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind silent server");
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
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
        format!("http://{addr}/mcp")
    }

    #[test]
    fn management_timeout_bounds_a_handshake_on_a_silent_server() {
        // The server accepts but never replies; a handshake with the SHORT
        // management bound must fail fast — the per-call timeout, not a hang
        // (the default here is a minute, so only the short bound can end it).
        let endpoint = spawn_silent_server();
        let mut client =
            McpClient::connect("silent", &endpoint, Vec::new(), Duration::from_secs(60))
                .expect("connect");

        let short = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let r = client.initialize_within(short);
        let elapsed = started.elapsed();
        assert!(r.is_err(), "a silent server cannot complete a handshake");
        assert!(
            elapsed < Duration::from_secs(5),
            "the short per-call timeout must govern (took {elapsed:?})"
        );
    }
}
