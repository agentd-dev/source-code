//! agentd serving its own MCP over a unix socket — composability. RFC 0005. [feature: serve-mcp]
//!
//! A peer (another agentd, an MCP client, or a driving harness) `initialize`s
//! against `--serve-mcp unix:PATH` and calls agentd's tools. Transport per RFC
//! §3.6: a **blocking `UnixListener`, thread-per-connection** (no async, no
//! mio) speaking the same NDJSON JSON-RPC codec as the MCP *client* (`json/`,
//! RFC 0004). v1 exposes a read-only `status` tool; the action tools
//! (`subagent.spawn` sync/async, `subagent.send/cancel/status`, RFC §3.2) build
//! on this transport next.

use crate::json::{self, frame, Incoming, Request, Response};
use crate::obs::log::Logger;
use crate::wire::mcp::PROTOCOL_VERSION;
use serde_json::{json, Value};
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

/// Read-only context the served tools report. Shared (read-only) across
/// connections; richer action context (a spawn-payload template) lands with the
/// `subagent.*` tools.
pub struct ServeCtx {
    pub run_id: String,
    pub mode: String,
    pub started: Instant,
}

/// Bind `path` and serve the self-MCP on a background accept thread (one thread
/// per connection). Returns the bind error so the caller decides if it's fatal.
pub fn serve(path: &str, ctx: ServeCtx, log: Logger) -> std::io::Result<()> {
    // A stale socket from a crashed prior run would block the bind; clear it.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    log.info("mcp.serving", json!({"path": path, "tools": ["status"]}));
    let ctx = Arc::new(ctx);
    thread::Builder::new()
        .name("serve-mcp".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let ctx = Arc::clone(&ctx);
                let log = log.clone();
                // One blocking thread per peer connection (RFC §3.6).
                thread::Builder::new()
                    .name("serve-mcp-conn".into())
                    .spawn(move || handle_conn(stream, &ctx, &log))
                    .ok();
            }
        })?;
    Ok(())
}

fn handle_conn(stream: UnixStream, ctx: &ServeCtx, log: &Logger) {
    let mut write = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    log.info("mcp.connect", json!({"peer": "unix"}));
    let mut reader = BufReader::new(stream);
    while let Ok(Some(bytes)) = frame::read_line(&mut reader) {
        // Requests get a reply; notifications (initialized, …) do not.
        if let Ok(Incoming::Request(req)) = serde_json::from_slice::<Incoming>(&bytes) {
            let resp = dispatch(req, ctx);
            if frame::write_line(&mut write, &resp).is_err() {
                break; // peer hung up mid-reply
            }
        }
    }
    log.debug("mcp.disconnect", json!({"peer": "unix"}));
}

/// Route one JSON-RPC request to a response. Pure given `ctx`.
fn dispatch(req: Request, ctx: &ServeCtx) -> Response {
    match req.method.as_str() {
        "initialize" => Response::ok(
            req.id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "agentd", "version": crate::VERSION}
            }),
        ),
        "ping" => Response::ok(req.id, json!({})),
        "tools/list" => Response::ok(req.id, json!({"tools": [status_tool_def()]})),
        "tools/call" => tools_call(req, ctx),
        other => Response::err(req.id, json::METHOD_NOT_FOUND, format!("unsupported method: {other}")),
    }
}

fn tools_call(req: Request, ctx: &ServeCtx) -> Response {
    let name = req.params.as_ref().and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or("");
    match name {
        "status" => {
            let body = json!({
                "run_id": ctx.run_id,
                "mode": ctx.mode,
                "version": crate::VERSION,
                "pid": std::process::id(),
                "uptime_ms": ctx.started.elapsed().as_millis() as u64,
            });
            // CallToolResult: human text + a structured payload (MCP 2025-11-25).
            Response::ok(
                req.id,
                json!({
                    "content": [{"type": "text", "text": body.to_string()}],
                    "structuredContent": body,
                    "isError": false
                }),
            )
        }
        other => Response::err(req.id, json::INVALID_PARAMS, format!("unknown tool: {other}")),
    }
}

fn status_tool_def() -> Value {
    json!({
        "name": "status",
        "description": "Report this agentd's run id, mode, version, pid and uptime (ms).",
        "inputSchema": {"type": "object", "properties": {}}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Id;

    fn ctx() -> ServeCtx {
        ServeCtx { run_id: "r1".into(), mode: "reactive".into(), started: Instant::now() }
    }

    fn req(method: &str, params: Option<Value>) -> Request {
        Request::new(Id::Num(1), method, params)
    }

    #[test]
    fn initialize_declares_tools_capability() {
        let r = dispatch(req("initialize", None), &ctx());
        let v = r.result.expect("ok");
        assert_eq!(v["protocolVersion"], PROTOCOL_VERSION);
        assert!(v["capabilities"]["tools"].is_object());
        assert_eq!(v["serverInfo"]["name"], "agentd");
    }

    #[test]
    fn tools_list_advertises_status() {
        let r = dispatch(req("tools/list", None), &ctx());
        let tools = r.result.expect("ok")["tools"].clone();
        assert_eq!(tools[0]["name"], "status");
    }

    #[test]
    fn status_call_returns_structured_state() {
        let r = dispatch(req("tools/call", Some(json!({"name": "status"}))), &ctx());
        let v = r.result.expect("ok");
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["run_id"], "r1");
        assert_eq!(v["structuredContent"]["mode"], "reactive");
    }

    #[test]
    fn unknown_tool_and_method_are_errors() {
        let bad_tool = dispatch(req("tools/call", Some(json!({"name": "ghost"}))), &ctx());
        assert!(bad_tool.error.is_some());
        let bad_method = dispatch(req("frobnicate", None), &ctx());
        assert!(bad_method.error.is_some());
    }
}
