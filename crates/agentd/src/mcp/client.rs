//! MCP client trait, stdio transport, and test mock.
//!
//! The stdio client spawns the MCP server as a child process, sends
//! an `initialize` request + `notifications/initialized` notification
//! at the first call, then serves `tools/call` / `resources/read`
//! sequentially. A single process serves the whole workflow run.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::mcp::protocol::{
    ClientInfo, InitializeParams, InitializeResult, ResourcesReadParams, ResourcesReadResult,
    RpcNotification, RpcRequest, RpcResponse, ToolsCallParams, ToolsCallResult,
};

/// One bounded MCP operation. Synchronous — the engine blocks the
/// current node while waiting.
pub trait McpClient: Send + Sync {
    /// Invoke `tools/call` and return the raw result.
    fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolsCallResult>;

    /// Invoke `resources/read` and return the raw result.
    fn read_resource(&self, uri: &str) -> Result<ResourcesReadResult>;
}

pub type McpClientRef = Arc<dyn McpClient>;

// ---------------------------------------------------------------------------
// Hot-reloadable wrapper
// ---------------------------------------------------------------------------

/// [`McpClient`] implementation that holds its inner behind an
/// [`arc_swap::ArcSwap`] so SIGHUP can respawn the backing
/// `StdioMcpClient` (killing the old child, starting a new one)
/// without re-registering MCP handlers.
///
/// Swap semantics: the new `Box<dyn McpClient>` is stored
/// atomically. In-flight calls that already dereferenced the old
/// inner complete against it; the old `StdioMcpClient` is dropped
/// once the last snapshot goes away, at which point its child is
/// killed. New calls see the replacement.
pub struct ReloadableMcpClient {
    inner: arc_swap::ArcSwap<Box<dyn McpClient>>,
}

impl ReloadableMcpClient {
    pub fn new(initial: Box<dyn McpClient>) -> Self {
        Self {
            inner: arc_swap::ArcSwap::from_pointee(initial),
        }
    }

    pub fn swap(&self, next: Box<dyn McpClient>) {
        self.inner.store(Arc::new(next));
    }
}

impl McpClient for ReloadableMcpClient {
    fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolsCallResult> {
        self.inner.load().call_tool(name, arguments)
    }

    fn read_resource(&self, uri: &str) -> Result<ResourcesReadResult> {
        self.inner.load().read_resource(uri)
    }
}

#[cfg(test)]
mod reloadable_tests {
    use super::*;
    use crate::mcp::client::MockMcpClient;
    use crate::mcp::protocol::ToolsCallResult;

    #[test]
    fn swap_redirects_tool_calls_to_new_client() {
        let first = MockMcpClient::new();
        first.enqueue_tool(ToolsCallResult {
            content: vec![json!({"type":"text","text":"from-first"})],
            is_error: false,
            structured_content: None,
        });
        let reloadable = Arc::new(ReloadableMcpClient::new(Box::new(first)));
        let dyn_view: McpClientRef = reloadable.clone();

        let r = dyn_view.call_tool("x", Value::Null).unwrap();
        assert_eq!(r.content[0]["text"], "from-first");

        let second = MockMcpClient::new();
        second.enqueue_tool(ToolsCallResult {
            content: vec![json!({"type":"text","text":"from-second"})],
            is_error: false,
            structured_content: None,
        });
        reloadable.swap(Box::new(second));

        let r2 = dyn_view.call_tool("x", Value::Null).unwrap();
        assert_eq!(r2.content[0]["text"], "from-second");
    }
}

// ---------------------------------------------------------------------------
// Stdio client — persistent child process
// ---------------------------------------------------------------------------

const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

/// Spawns an MCP server as a child process and talks NDJSON JSON-RPC
/// over its stdin/stdout.
///
/// The child lives for the lifetime of the `StdioMcpClient`. On
/// drop, stdin is closed and the child is killed if still running.
/// The client serialises access to the pipe with a single Mutex —
/// MCP servers assume one in-flight request at a time.
pub struct StdioMcpClient {
    inner: Mutex<Inner>,
}

struct Inner {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    initialized: bool,
    label: String,
}

impl StdioMcpClient {
    /// Spawn the MCP server described by `command` + `args`. No
    /// initialize handshake yet — that happens lazily on the first
    /// real call so spawn cost isn't paid for unused clients.
    pub fn spawn(command: impl Into<PathBuf>, args: &[String]) -> Result<Self> {
        let command: PathBuf = command.into();
        let label = command.display().to_string();
        let mut child = Command::new(&command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Mcp(format!("spawn {}: {e}", command.display())))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Mcp("missing stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Mcp("missing stdout".into()))?;
        Ok(Self {
            inner: Mutex::new(Inner {
                child,
                stdin,
                stdout: BufReader::new(stdout),
                next_id: 1,
                initialized: false,
                label,
            }),
        })
    }

    fn rpc<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: P) -> Result<R> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| Error::Mcp("client mutex poisoned".into()))?;
        if !guard.initialized && method != "initialize" {
            initialize(&mut guard)?;
        }
        rpc_call(&mut guard, method, params)
    }
}

impl McpClient for StdioMcpClient {
    fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolsCallResult> {
        let params = ToolsCallParams { name, arguments };
        self.rpc("tools/call", params)
    }

    fn read_resource(&self, uri: &str) -> Result<ResourcesReadResult> {
        self.rpc("resources/read", ResourcesReadParams { uri })
    }
}

impl Drop for StdioMcpClient {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.inner.lock() {
            // Best-effort: close stdin, then wait briefly; kill on fallthrough.
            let _ = guard.stdin.flush();
            let _ = guard.child.kill();
            let _ = guard.child.wait();
        }
    }
}

fn initialize(inner: &mut Inner) -> Result<()> {
    let params = InitializeParams {
        protocol_version: MCP_PROTOCOL_VERSION,
        client_info: ClientInfo {
            name: "agentd",
            version: env!("CARGO_PKG_VERSION"),
        },
        capabilities: json!({}),
    };
    let _: InitializeResult = rpc_call(inner, "initialize", params)?;
    // Send the post-initialize notification (required by the spec).
    let note = RpcNotification {
        jsonrpc: "2.0",
        method: "notifications/initialized",
        params: json!({}),
    };
    let line = serde_json::to_vec(&note).map_err(Error::Json)?;
    inner
        .stdin
        .write_all(&line)
        .and_then(|()| inner.stdin.write_all(b"\n"))
        .and_then(|()| inner.stdin.flush())
        .map_err(|e| Error::Mcp(format!("send initialized: {e}")))?;
    inner.initialized = true;
    Ok(())
}

fn rpc_call<P: Serialize, R: DeserializeOwned>(
    inner: &mut Inner,
    method: &str,
    params: P,
) -> Result<R> {
    let id = inner.next_id;
    inner.next_id = inner.next_id.wrapping_add(1).max(1);

    let envelope = RpcRequest {
        jsonrpc: "2.0",
        id,
        method,
        params,
    };
    let line = serde_json::to_vec(&envelope).map_err(Error::Json)?;

    inner
        .stdin
        .write_all(&line)
        .and_then(|()| inner.stdin.write_all(b"\n"))
        .and_then(|()| inner.stdin.flush())
        .map_err(|e| Error::Mcp(format!("write request on {}: {e}", inner.label)))?;

    // Read one line — skip any notifications the server sends before
    // the response (servers may emit progress notifications).
    loop {
        let mut buf = String::new();
        let read = inner
            .stdout
            .read_line(&mut buf)
            .map_err(|e| Error::Mcp(format!("read response on {}: {e}", inner.label)))?;
        if read == 0 {
            return Err(Error::Mcp(format!(
                "{} closed the pipe unexpectedly",
                inner.label
            )));
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed: Value = serde_json::from_str(trimmed).map_err(Error::Json)?;
        // A response always has an `id`; notifications have `method` but no `id`.
        if parsed.get("id").is_some() {
            let rpc: RpcResponse<R> = serde_json::from_value(parsed).map_err(Error::Json)?;
            if let Some(err) = rpc.error {
                return Err(Error::Mcp(format!(
                    "{} error {code}: {msg}",
                    inner.label,
                    code = err.code,
                    msg = err.message
                )));
            }
            return rpc
                .result
                .ok_or_else(|| Error::Mcp(format!("{} response lacked `result`", inner.label)));
        }
        // Otherwise it's a notification — ignore and keep reading.
    }
}

// ---------------------------------------------------------------------------
// Mock client — test-only
// ---------------------------------------------------------------------------

/// In-process client with canned outputs. Records every call for
/// assertions.
#[derive(Debug, Default)]
pub struct MockMcpClient {
    tool_results: Mutex<Vec<Result<ToolsCallResult>>>,
    resource_results: Mutex<Vec<Result<ResourcesReadResult>>>,
    tool_calls: Mutex<Vec<(String, Value)>>,
    resource_calls: Mutex<Vec<String>>,
}

impl MockMcpClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue_tool(&self, result: ToolsCallResult) {
        self.tool_results.lock().unwrap().push(Ok(result));
    }

    pub fn enqueue_tool_error(&self, reason: &str) {
        self.tool_results
            .lock()
            .unwrap()
            .push(Err(Error::Mcp(reason.to_string())));
    }

    pub fn enqueue_resource(&self, result: ResourcesReadResult) {
        self.resource_results.lock().unwrap().push(Ok(result));
    }

    pub fn tool_calls(&self) -> Vec<(String, Value)> {
        self.tool_calls.lock().unwrap().clone()
    }

    pub fn resource_calls(&self) -> Vec<String> {
        self.resource_calls.lock().unwrap().clone()
    }
}

impl McpClient for MockMcpClient {
    fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolsCallResult> {
        self.tool_calls
            .lock()
            .unwrap()
            .push((name.to_string(), arguments));
        let mut q = self.tool_results.lock().unwrap();
        if q.is_empty() {
            return Err(Error::Mcp(format!(
                "MockMcpClient: no canned tool result for `{name}`"
            )));
        }
        q.remove(0)
    }

    fn read_resource(&self, uri: &str) -> Result<ResourcesReadResult> {
        self.resource_calls.lock().unwrap().push(uri.to_string());
        let mut q = self.resource_results.lock().unwrap();
        if q.is_empty() {
            return Err(Error::Mcp(format!(
                "MockMcpClient: no canned resource for `{uri}`"
            )));
        }
        q.remove(0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Child-process transport tests drive real fds — Unix-only by
// construction; the mock-path coverage rides along since CI runs
// the suite on Linux + macOS.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::io::{FromRawFd, IntoRawFd};

    #[test]
    fn mock_records_and_returns_canned_tool_result() {
        let c = MockMcpClient::new();
        c.enqueue_tool(ToolsCallResult {
            content: vec![json!({"type":"text","text":"ok"})],
            is_error: false,
            structured_content: None,
        });
        let res = c.call_tool("do-thing", json!({"x": 1})).unwrap();
        assert_eq!(res.content[0]["text"], "ok");
        assert_eq!(
            c.tool_calls(),
            vec![("do-thing".to_string(), json!({"x": 1}))]
        );
    }

    #[test]
    fn mock_errors_on_empty_queue() {
        let c = MockMcpClient::new();
        let err = c.call_tool("do", json!({})).unwrap_err();
        assert!(format!("{err}").contains("no canned"));
    }

    #[test]
    fn mock_resource_round_trip() {
        let c = MockMcpClient::new();
        c.enqueue_resource(ResourcesReadResult {
            contents: vec![json!({"uri":"docs://x","text":"hello"})],
        });
        let r = c.read_resource("docs://x").unwrap();
        assert_eq!(r.contents[0]["text"], "hello");
        assert_eq!(c.resource_calls(), vec!["docs://x".to_string()]);
    }

    /// End-to-end stdio client against a tiny python-less "server"
    /// implemented as a shell loop that echoes canned responses.
    /// We use `/bin/sh` to read one request per line and emit one
    /// response per line — verifies the NDJSON framing end to end.
    #[test]
    fn stdio_client_initializes_and_calls_tool_against_fake_server() {
        // The fake server handles exactly three lines:
        //   1) initialize → success
        //   2) notifications/initialized → no reply (notification)
        //   3) tools/call → success with one content block
        // `sh` here is a simple state machine driven by line number.
        let script = r#"
            set -u
            line_no=0
            while IFS= read -r line; do
                line_no=$((line_no + 1))
                case "$line_no" in
                    1)
                        # initialize response
                        id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26"}}\n' "$id"
                        ;;
                    2)
                        # initialized notification, no response
                        :
                        ;;
                    3)
                        id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id"
                        exit 0
                        ;;
                esac
            done
        "#;

        let client = StdioMcpClient::spawn(
            PathBuf::from("/bin/sh"),
            &["-c".to_string(), script.to_string()],
        )
        .expect("spawn sh-based fake MCP server");

        let res = client.call_tool("ping", json!({})).expect("tools/call");
        assert_eq!(res.content[0]["text"], "pong");
        assert!(!res.is_error);
    }

    #[test]
    fn stdio_client_surfaces_rpc_error() {
        let script = r#"
            set -u
            line_no=0
            while IFS= read -r line; do
                line_no=$((line_no + 1))
                case "$line_no" in
                    1)
                        id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26"}}\n' "$id"
                        ;;
                    2) : ;;
                    3)
                        id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32010,"message":"denied"}}\n' "$id"
                        exit 0
                        ;;
                esac
            done
        "#;

        let client = StdioMcpClient::spawn(
            PathBuf::from("/bin/sh"),
            &["-c".to_string(), script.to_string()],
        )
        .expect("spawn");

        let err = client.call_tool("bad", json!({})).unwrap_err();
        assert!(format!("{err}").contains("-32010"));
        assert!(format!("{err}").contains("denied"));
    }

    #[test]
    fn spawn_missing_binary_errors() {
        // `unwrap_err` requires `Debug` on the `Ok` variant, but
        // `StdioMcpClient` holds a `Child` we don't want to derive on.
        // Pattern-match instead.
        match StdioMcpClient::spawn(PathBuf::from("/definitely/not/a/real/binary/path"), &[]) {
            Ok(_) => panic!("expected spawn to fail on a missing binary"),
            Err(e) => assert!(format!("{e}").contains("spawn")),
        }
    }

    // Keep BufReader / BufRead / Write / FromRawFd / IntoRawFd in
    // scope even if no specific test mentions them — future test
    // additions often need them, and hiding them here keeps the
    // module's imports grouped.
    #[allow(dead_code)]
    fn _unused_io_imports() {
        let _ = BufReader::new(std::io::empty());
        let _: fn(&mut std::io::Empty) -> std::io::Result<&[u8]> = BufRead::fill_buf;
        let _: fn(&mut std::io::Sink) -> std::io::Result<()> = Write::flush;
        let _: unsafe fn(i32) -> std::fs::File = <std::fs::File as FromRawFd>::from_raw_fd;
        let _: fn(std::fs::File) -> i32 = <std::fs::File as IntoRawFd>::into_raw_fd;
    }
}
