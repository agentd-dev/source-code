//! MCP client over a stdio child process. RFC 0004.
//!
//! Spawns a server (`Command`), speaks newline-delimited JSON-RPC 2.0 over its
//! stdin/stdout, and routes traffic with the classic split: a dedicated
//! **reader thread** parses every inbound frame and either resolves a pending
//! request (by id) or hands a notification to the caller; request senders
//! block on a per-request channel with a timeout (the OLD runtime had *no*
//! MCP timeouts — a hung server wedged the node; we fix that here).
//!
//! v1 connects one server and implements the client subset from RFC 0004:
//! initialize + capability store, tools (list+call), resources (list+read),
//! subscribe/unsubscribe, ping. We declare **no** client capabilities and
//! answer server→client `ping`/`roots/list` minimally, rejecting `sampling`.

use crate::json::{self, frame, Id, Incoming, RpcError};
use crate::wire::mcp::{
    method, CallToolParams, CallToolResult, ClientCapabilities, Implementation, InitializeParams,
    InitializeResult, ListResourcesResult, ListToolsResult, ReadResourceParams, ReadResourceResult,
    Resource, ServerCapabilities, SubscribeParams, Tool, PROTOCOL_VERSION,
};
use serde::Serialize;
use serde_json::{json, Value};
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
            .stderr(Stdio::null()) // server logs go nowhere in v1; capture in M-later
            .spawn()
            .map_err(McpError::Spawn)?;

        let stdin = child.stdin.take().ok_or_else(|| McpError::Transport("no child stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Transport("no child stdout".into()))?;

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
            _reader: reader,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.caps
    }

    /// MCP lifecycle handshake: `initialize` → store capabilities →
    /// `notifications/initialized`.
    pub fn initialize(&mut self) -> Result<(), McpError> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "agentd".into(),
                version: crate::VERSION.into(),
                title: None,
            },
        };
        let result = self.request(method::INITIALIZE, Some(to_value(&params)))?;
        let init: InitializeResult = serde_json::from_value(result)
            .map_err(|e| McpError::Transport(format!("bad initialize result: {e}")))?;
        self.caps = init.capabilities;
        self.notify(method::INITIALIZED, None)?;
        Ok(())
    }

    /// `tools/list`, following cursor pagination to completion. Empty when the
    /// server doesn't advertise `tools`.
    pub fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        if !self.caps.supports_tools() {
            return Ok(Vec::new());
        }
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let page: ListToolsResult = self.request_as(method::TOOLS_LIST, params)?;
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
    pub fn call_tool(&self, name: &str, arguments: Option<Value>) -> Result<CallToolResult, McpError> {
        if !self.caps.supports_tools() {
            return Err(McpError::Capability(format!("server '{}' has no tools", self.name)));
        }
        let params = CallToolParams { name: name.to_string(), arguments };
        self.request_as(method::TOOLS_CALL, Some(to_value(&params)))
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
        let params = ReadResourceParams { uri: uri.to_string() };
        self.request_as(method::RESOURCES_READ, Some(to_value(&params)))
    }

    /// `resources/subscribe` — gated on the server advertising it (RFC 0004).
    pub fn subscribe(&self, uri: &str) -> Result<(), McpError> {
        if !self.caps.supports_subscribe() {
            return Err(McpError::Capability(format!(
                "server '{}' does not support resource subscriptions",
                self.name
            )));
        }
        self.request(method::RESOURCES_SUBSCRIBE, Some(to_value(&SubscribeParams { uri: uri.into() })))?;
        Ok(())
    }

    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        self.request(method::RESOURCES_UNSUBSCRIBE, Some(to_value(&SubscribeParams { uri: uri.into() })))?;
        Ok(())
    }

    /// Drain any notifications the reader thread has queued (e.g.
    /// `notifications/resources/updated`). The reactive router consumes these
    /// in M3; v1 callers may ignore them.
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
        let v = self.request(method, params)?;
        serde_json::from_value(v).map_err(|e| McpError::Transport(format!("bad {method} result: {e}")))
    }

    fn request(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let req = json::Request::new(id, method, params);
        if let Err(e) = write_msg(&self.writer, &req) {
            self.pending.lock().unwrap().remove(&id);
            return Err(McpError::Transport(e.to_string()));
        }

        match rx.recv_timeout(self.timeout) {
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
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(McpError::Transport(format!("server '{}' closed the connection", self.name)))
            }
        }
    }

    fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        write_msg(&self.writer, &json::Notification::new(method, params))
            .map_err(|e| McpError::Transport(e.to_string()))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Closing stdin (drop the writer's inner) signals shutdown; killing is
        // the backstop. The reader thread sees stdout EOF and exits. (The full
        // close-stdin → SIGTERM → SIGKILL ladder lands in M2/RFC 0003.)
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

fn to_value<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

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
        other => json::Response::err(req.id, json::METHOD_NOT_FOUND, format!("unsupported: {other}")),
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
    fn error_display() {
        let e = McpError::Timeout("tools/call on 'fs'".into());
        assert!(e.to_string().contains("timeout"));
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
