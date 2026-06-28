// SPDX-License-Identifier: Apache-2.0
//! MCP client over a stdio child process. RFC 0004.
//!
//! Spawns a server (`Command`), speaks newline-delimited JSON-RPC 2.0 over its
//! stdin/stdout, and routes traffic with the classic split: a dedicated
//! **reader thread** parses every inbound frame and either resolves a pending
//! request (by id) or hands a notification to the caller; request senders
//! block on a per-request channel with a timeout (the OLD runtime had *no*
//! MCP timeouts — a hung server wedged the node; we fix that here).
//!
//! Each client connects one server and implements the client subset from RFC 0004:
//! initialize + capability store, tools (list+call), resources (list+read),
//! subscribe/unsubscribe, ping. We declare **no** client capabilities and
//! answer server→client `ping`/`roots/list` minimally, rejecting `sampling`.

use crate::json::{self, Id, Incoming, RpcError, frame};
use crate::wire::mcp::{
    CallToolResult, ClientCapabilities, Implementation, InitializeParams, InitializeResult,
    ListResourcesResult, ListToolsResult, PROTOCOL_VERSION, ReadResourceParams, ReadResourceResult,
    Resource, ServerCapabilities, SubscribeParams, Tool, method,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Debug)]
pub enum McpError {
    Spawn(std::io::Error),
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
            McpError::Spawn(e) => write!(f, "mcp: spawn failed: {e}"),
            McpError::Transport(m) => write!(f, "mcp: transport: {m}"),
            McpError::Rpc(e) => write!(f, "mcp: rpc error {}: {}", e.code, e.message),
            McpError::Timeout(m) => write!(f, "mcp: timeout: {m}"),
            McpError::Capability(m) => write!(f, "mcp: capability: {m}"),
        }
    }
}
impl std::error::Error for McpError {}

type Pending = Arc<Mutex<HashMap<i64, Sender<json::Response>>>>;
type SharedWriter = Arc<Mutex<ChildStdin>>;

/// A connected (and, after [`McpClient::initialize`], handshaken) MCP server.
pub struct McpClient {
    name: String,
    child: Option<Child>,
    writer: SharedWriter,
    pending: Pending,
    notifications: Mutex<Receiver<json::Notification>>,
    next_id: AtomicI64,
    caps: ServerCapabilities,
    timeout: Duration,
    /// Stamped into every `tools/call` request's `params._meta` (e.g.
    /// `{"agent/run_id": …}`) so backing services can dedupe retries
    /// (RFC 0011 §idempotency).
    tool_meta: Option<Value>,
    _reader: JoinHandle<()>,
}

impl McpClient {
    /// Spawn `command` (argv) as a stdio MCP server and start the reader
    /// thread. Call [`Self::initialize`] before any tool/resource call.
    pub fn spawn(name: &str, command: &[String], timeout: Duration) -> Result<McpClient, McpError> {
        let (prog, args) = command.split_first().ok_or_else(|| {
            McpError::Transport(format!("mcp server '{name}' has an empty command"))
        })?;
        let mut child = Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // discarded by design — servers own their logging; capture is deferred v2 surface
            .spawn()
            .map_err(McpError::Spawn)?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("no child stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("no child stdout".into()))?;

        let writer: SharedWriter = Arc::new(Mutex::new(stdin));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, notif_rx) = mpsc::channel();

        let reader = {
            let pending = Arc::clone(&pending);
            let writer = Arc::clone(&writer);
            let name = name.to_string();
            std::thread::Builder::new()
                .name(format!("mcp-reader:{name}"))
                .spawn(move || reader_loop(BufReader::new(stdout), pending, writer, notif_tx))
                .map_err(|e| McpError::Transport(format!("spawn reader thread: {e}")))?
        };

        Ok(McpClient {
            name: name.to_string(),
            child: Some(child),
            writer,
            pending,
            notifications: Mutex::new(notif_rx),
            next_id: AtomicI64::new(1),
            caps: ServerCapabilities::default(),
            timeout,
            tool_meta: None,
            _reader: reader,
        })
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
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "agentd".into(),
                version: crate::VERSION.into(),
                title: None,
            },
        };
        let result =
            self.request_with_timeout(method::INITIALIZE, Some(to_value(&params)), timeout)?;
        let init: InitializeResult = serde_json::from_value(result)
            .map_err(|e| McpError::Transport(format!("bad initialize result: {e}")))?;
        self.caps = init.capabilities;
        self.notify(method::INITIALIZED, None)?;
        Ok(())
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
    pub fn list_tools_within(&self, timeout: Duration) -> Result<Vec<Tool>, McpError> {
        if !self.caps.supports_tools() {
            return Ok(Vec::new());
        }
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let page: ListToolsResult =
                self.request_as_within(method::TOOLS_LIST, params, timeout)?;
            tools.extend(page.tools);
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(tools)
    }

    /// `tools/call`. The returned [`CallToolResult`] carries `isError` (a
    /// tool-domain failure observation) — distinct from an `Err` here, which
    /// is a transport/protocol failure (RFC 0004 §isError).
    pub fn call_tool(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<CallToolResult, McpError> {
        if !self.caps.supports_tools() {
            return Err(McpError::Capability(format!(
                "server '{}' has no tools",
                self.name
            )));
        }
        let params = build_call_params(name, arguments, self.tool_meta.as_ref());
        self.request_as(method::TOOLS_CALL, Some(params))
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
        if !self.caps.supports_tools() {
            return Err(McpError::Capability(format!(
                "server '{}' has no tools",
                self.name
            )));
        }
        let merged = merge_meta(self.tool_meta.as_ref(), extra_meta);
        let params = build_call_params(name, arguments, Some(&merged));
        self.request_as_within(method::TOOLS_CALL, Some(params), timeout)
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, McpError> {
        if !self.caps.supports_resources() {
            return Ok(Vec::new());
        }
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let page: ListResourcesResult = self.request_as(method::RESOURCES_LIST, params)?;
            resources.extend(page.resources);
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(resources)
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
        timeout: Duration,
    ) -> Result<ReadResourceResult, McpError> {
        let params = ReadResourceParams {
            uri: uri.to_string(),
        };
        self.request_as_within(method::RESOURCES_READ, Some(to_value(&params)), timeout)
    }

    /// `resources/subscribe` — gated on the server advertising it (RFC 0004).
    pub fn subscribe(&self, uri: &str) -> Result<(), McpError> {
        self.subscribe_within(uri, self.timeout)
    }

    /// [`Self::subscribe`] with a caller-supplied timeout (the SHORT management
    /// bound, RFC 0016 §10) — for the reactor-thread reload re-handshake, where a
    /// slow-but-alive server arming a subscription must not block the reactor.
    pub fn subscribe_within(&self, uri: &str, timeout: Duration) -> Result<(), McpError> {
        if !self.caps.supports_subscribe() {
            return Err(McpError::Capability(format!(
                "server '{}' does not support resource subscriptions",
                self.name
            )));
        }
        self.request_with_timeout(
            method::RESOURCES_SUBSCRIBE,
            Some(to_value(&SubscribeParams { uri: uri.into() })),
            timeout,
        )?;
        Ok(())
    }

    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        self.unsubscribe_within(uri, self.timeout)
    }

    /// [`Self::unsubscribe`] with a caller-supplied timeout (the SHORT management
    /// bound, RFC 0016 §10) — for the reactor-thread reload reconcile + the drain
    /// unsubscribe, both best-effort: a slow server here must not block the reactor
    /// or the drain past the liveness window / drain budget.
    pub fn unsubscribe_within(&self, uri: &str, timeout: Duration) -> Result<(), McpError> {
        self.request_with_timeout(
            method::RESOURCES_UNSUBSCRIBE,
            Some(to_value(&SubscribeParams { uri: uri.into() })),
            timeout,
        )?;
        Ok(())
    }

    /// Drain any notifications the reader thread has queued (e.g.
    /// `notifications/resources/updated`). The reactive router
    /// (`triggers/mode.rs`) drains these between runs to drive re-reactions.
    pub fn drain_notifications(&self) -> Vec<json::Notification> {
        let rx = self.notifications.lock().unwrap();
        rx.try_iter().collect()
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

    /// Send one JSON-RPC request and block up to `timeout` for the matching
    /// response. The default-timeout `request` delegates here with `self.timeout`;
    /// the reactor-thread management path passes the SHORT bound (RFC 0016 §10) so
    /// a slow-but-alive server cannot block the reactor past the liveness window.
    fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        // Fail FAST if the reader thread has already exited (the server's stdout
        // hit EOF / a read error). Without this, a request after the reader is gone
        // registers a pending sender nothing will ever resolve, so `recv_timeout`
        // blocks for the FULL per-request timeout (up to 60s) — which can wedge a
        // best-effort drain/reload unsubscribe. A dead reader means the connection
        // is gone; surface it immediately as a transport error (RFC 0004).
        if self._reader.is_finished() {
            return Err(McpError::Transport(format!(
                "server '{}' connection is closed (reader exited)",
                self.name
            )));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let req = json::Request::new(id, method, params);
        if let Err(e) = write_msg(&self.writer, &req) {
            self.pending.lock().unwrap().remove(&id);
            return Err(McpError::Transport(e.to_string()));
        }

        match rx.recv_timeout(timeout) {
            Ok(resp) => match resp.error {
                Some(err) => Err(McpError::Rpc(err)),
                None => Ok(resp.result.unwrap_or(Value::Null)),
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap().remove(&id);
                // Best-effort cancel so the server can stop work (RFC 0004).
                let _ = self.notify(
                    method::NOTIFY_CANCELLED,
                    Some(json!({ "requestId": id, "reason": "timeout" })),
                );
                Err(McpError::Timeout(format!("{method} on '{}'", self.name)))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(McpError::Transport(format!(
                "server '{}' closed the connection",
                self.name
            ))),
        }
    }

    fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        write_msg(&self.writer, &json::Notification::new(method, params))
            .map_err(|e| McpError::Transport(e.to_string()))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Closing stdin signals shutdown; child.kill() is the backstop. The
        // reader thread sees stdout EOF and exits. (MCP server children are
        // reaped here directly; the supervisor kill-ladder in supervisor/kill.rs
        // governs subagent process groups, not these stdio servers.)
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn write_msg<T: Serialize>(w: &SharedWriter, msg: &T) -> std::io::Result<()> {
    let mut guard = w.lock().unwrap_or_else(|e| e.into_inner());
    frame::write_line(&mut *guard, msg)
}

/// Build `tools/call` params — `{name, arguments?, _meta?}` — injecting the
/// client's `_meta` (run id, etc.) for retry dedup. Pure. RFC 0011 §idempotency.
fn build_call_params(name: &str, arguments: Option<Value>, meta: Option<&Value>) -> Value {
    let mut p = serde_json::Map::new();
    p.insert("name".into(), Value::String(name.to_string()));
    if let Some(args) = arguments {
        p.insert("arguments".into(), args);
    }
    if let Some(m) = meta {
        p.insert("_meta".into(), m.clone());
    }
    Value::Object(p)
}

fn to_value<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Merge `extra` over the persistent `base` meta for a single call, without
/// mutating either. When both are objects, `extra` wins key-by-key (a shallow
/// merge — the claim contract's keys are flat); a non-object `extra` replaces
/// `base` wholesale; a `None`/non-object `base` yields `extra`. Pure.
fn merge_meta(base: Option<&Value>, extra: Value) -> Value {
    match (base, &extra) {
        (Some(Value::Object(b)), Value::Object(e)) => {
            let mut m = b.clone();
            for (k, v) in e {
                m.insert(k.clone(), v.clone());
            }
            Value::Object(m)
        }
        _ => extra,
    }
}

// Safe because agentd only ever mints numeric ids (`next_id: AtomicI64`): a
// string-id response cannot match a pending request and is dropped (the caller
// times out), which cannot happen for our own requests. A server echoing a
// non-numeric id is out of spec.
fn id_num(id: &Id) -> Option<i64> {
    match id {
        Id::Num(n) => Some(*n),
        Id::Str(_) => None,
    }
}

/// The reader thread: parse every inbound frame and dispatch. Exits on EOF or
/// a fatal read error, after which pending requests see `Disconnected`.
fn reader_loop(
    mut reader: BufReader<std::process::ChildStdout>,
    pending: Pending,
    writer: SharedWriter,
    notif_tx: Sender<json::Notification>,
) {
    loop {
        match frame::read_line(&mut reader) {
            Ok(Some(bytes)) => match serde_json::from_slice::<Incoming>(&bytes) {
                Ok(Incoming::Response(resp)) => {
                    let tx = id_num(&resp.id).and_then(|id| pending.lock().unwrap().remove(&id));
                    if let Some(tx) = tx {
                        let _ = tx.send(resp);
                    }
                }
                Ok(Incoming::Notification(n)) => {
                    let _ = notif_tx.send(n);
                }
                Ok(Incoming::Request(req)) => answer_server_request(&writer, req),
                Err(_) => { /* unparseable frame — skip, keep the stream moving */ }
            },
            Ok(None) => break, // clean EOF
            Err(_) => break,   // read error
        }
    }
    // Let blocked callers fall through to Disconnected.
    pending.lock().unwrap().clear();
}

/// Answer the few server→client requests v1 expects. We declare no client
/// capabilities, so `roots/list` is empty and `sampling/createMessage` is
/// refused; `ping` is answered (RFC 0004 §declare-no-client-caps).
fn answer_server_request(writer: &SharedWriter, req: json::Request) {
    let resp = match req.method.as_str() {
        "ping" => json::Response::ok(req.id, json!({})),
        "roots/list" => json::Response::ok(req.id, json!({ "roots": [] })),
        other => json::Response::err(
            req.id,
            json::METHOD_NOT_FOUND,
            format!("unsupported: {other}"),
        ),
    };
    let _ = write_msg(writer, &resp);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_num_extracts_numeric() {
        assert_eq!(id_num(&Id::Num(5)), Some(5));
        assert_eq!(id_num(&Id::Str("x".into())), None);
    }

    #[test]
    fn call_params_inject_meta_for_idempotency() {
        let meta = json!({"agent/run_id": "r1"});
        let p = build_call_params("send_email", Some(json!({"to": "x"})), Some(&meta));
        assert_eq!(p["name"], "send_email");
        assert_eq!(p["arguments"]["to"], "x");
        assert_eq!(p["_meta"]["agent/run_id"], "r1");

        // no meta / no args → those keys are absent
        let p2 = build_call_params("noop", None, None);
        assert_eq!(p2["name"], "noop");
        assert!(p2.get("_meta").is_none());
        assert!(p2.get("arguments").is_none());
    }

    #[test]
    fn merge_meta_overlays_extra_without_mutating_base() {
        // Per-call claim_key rides on top of the persistent run_id stamp.
        let base = json!({"agent/run_id": "r1", "traceparent": "tp"});
        let merged = merge_meta(
            Some(&base),
            json!({"agent/claim_key": "ck", "traceparent": "tp2"}),
        );
        assert_eq!(merged["agent/run_id"], "r1"); // persistent key preserved
        assert_eq!(merged["agent/claim_key"], "ck"); // per-call key added
        assert_eq!(merged["traceparent"], "tp2"); // extra wins on conflict
        // The base is untouched.
        assert_eq!(base["traceparent"], "tp");
        // No persistent base → the extra is the meta.
        let only = merge_meta(None, json!({"agent/claim_key": "ck"}));
        assert_eq!(only["agent/claim_key"], "ck");
    }

    #[test]
    fn error_display() {
        let e = McpError::Timeout("tools/call on 'fs'".into());
        assert!(e.to_string().contains("timeout"));
    }

    #[test]
    fn management_timeout_bounds_a_call_on_a_non_responding_server() {
        // A server that is ALIVE but never replies: `sleep` reads nothing and
        // writes nothing, so its stdout never produces a frame, the reader thread
        // stays blocked on the read (alive — so the fast-fail-on-dead-reader path
        // does NOT trip), and a request blocks until its per-request timeout. We
        // spawn with the DEFAULT 60s timeout but issue the request with the SHORT
        // management bound, proving the per-call timeout — not the default —
        // governs: the call returns ~200ms even though the default is 60s.
        let mut client = McpClient::spawn(
            "hung",
            &["sleep".to_string(), "3600".to_string()],
            Duration::from_secs(60),
        )
        .expect("spawn the hung server");

        let short = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let r = client.request_with_timeout("ping", None, short);
        let elapsed = started.elapsed();

        match r {
            Err(McpError::Timeout(_)) => {}
            other => panic!("expected a Timeout within the short bound, got {other:?}"),
        }
        // Returned within the short bound (generous slack for CI), NOT the 60s
        // default. If the default had governed this would be ~60s.
        assert!(
            elapsed < Duration::from_secs(5),
            "the short per-call timeout must govern, not the 60s default (took {elapsed:?})"
        );
        // The public management entry points thread the same short bound through.
        // `initialize_within` issues a real request, so it also times out fast.
        let started = std::time::Instant::now();
        let r = client.initialize_within(short);
        assert!(
            matches!(r, Err(McpError::Timeout(_))),
            "initialize_within must honour the short bound: {r:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "initialize_within must return within the short bound"
        );
    }

    #[test]
    fn management_timeout_const_is_under_the_liveness_window() {
        // The pinned invariant (RFC 0016 §10): the reactor-thread management-call
        // timeout MUST stay under the `/healthz` + health-file liveness staleness
        // window, or a single management call could itself age the heartbeat past
        // the threshold — the starvation the short bound exists to prevent. The
        // authority is the compile-time `const _: () = assert!(...)` in `obs::health`
        // (it fails the BUILD if the invariant is broken); this test documents the
        // relationship where a reader of the client sees it, via the runtime
        // `Duration` accessor (not a const-foldable comparison).
        let mgmt = crate::obs::health::management_timeout();
        let window = Duration::from_millis(crate::obs::health::LIVENESS_STALE_AFTER_MS);
        assert!(
            mgmt < window,
            "management timeout {mgmt:?} must be under the liveness window {window:?}"
        );
        assert_eq!(
            mgmt,
            Duration::from_millis(crate::obs::health::MANAGEMENT_TIMEOUT_MS),
        );
    }

    #[test]
    fn spawn_failure_is_reported() {
        // A command that does not exist must surface as a Spawn error, not a
        // panic.
        let result = McpClient::spawn(
            "nope",
            &["/nonexistent/agentd-mcp-xyz".to_string()],
            Duration::from_secs(1),
        );
        match result {
            Err(McpError::Spawn(_)) => {}
            Err(other) => panic!("expected Spawn error, got {other}"),
            Ok(_) => panic!("expected spawn to fail"),
        }
    }
}
