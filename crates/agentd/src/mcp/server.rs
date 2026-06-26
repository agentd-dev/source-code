//! agentd serving its own MCP over a unix socket — composability. RFC 0005. [feature: serve-mcp]
//!
//! A peer (another agentd, an MCP client, or a driving harness) `initialize`s
//! against `--serve-mcp unix:PATH` and calls agentd's tools. Transport per RFC
//! §3.6: a **blocking `UnixListener`, thread-per-connection** (no async, no
//! mio) speaking the same NDJSON JSON-RPC codec as the MCP *client* (`json/`,
//! RFC 0004). v1 exposes a read-only `status` tool; the action tools
//! (`subagent.spawn` sync/async, `subagent.send/cancel/status`, RFC §3.2) build
//! on this transport next.

use crate::json::{self, frame, Id, Incoming, Request, Response};
use crate::obs::log::Logger;
use crate::subagent::protocol::SpawnPayload;
use crate::supervisor::reactor::{supervise_once, SuperviseResult};
use crate::wire::mcp::PROTOCOL_VERSION;
use serde_json::{json, Value};
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Cap on concurrent peer-driven `subagent.spawn` runs in flight (bounds a peer
/// spamming the socket; each run is also bounded by the base payload's limits).
const MAX_INFLIGHT_SPAWNS: usize = 4;
/// Cap on the result text returned to the peer (~chars).
const RESULT_CAP: usize = 4096;

/// Context for the served tools. `run_id`/`mode`/`started` back `status`; the
/// rest back `subagent.spawn` (a peer delegating work). Shared across
/// connections; the atomics enforce the concurrency cap + mint handles.
pub struct ServeCtx {
    run_id: String,
    mode: String,
    started: Instant,
    exe: PathBuf,
    base: SpawnPayload,
    drain_timeout: Duration,
    inflight: Arc<AtomicUsize>,
    counter: Arc<AtomicU64>,
}

impl ServeCtx {
    pub fn new(run_id: String, mode: String, exe: PathBuf, base: SpawnPayload, drain_timeout: Duration) -> ServeCtx {
        ServeCtx {
            run_id,
            mode,
            started: Instant::now(),
            exe,
            base,
            drain_timeout,
            inflight: Arc::new(AtomicUsize::new(0)),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// This agentd's own run/health state — the single source of truth for both
    /// the `status` tool and the `agentd://status` resource.
    fn status_body(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "mode": self.mode,
            "version": crate::VERSION,
            "pid": std::process::id(),
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "inflight_spawns": self.inflight.load(Ordering::Relaxed),
            "total_spawns": self.counter.load(Ordering::Relaxed),
        })
    }
}

/// RAII concurrency permit for a served spawn; releases the slot on drop.
struct SpawnGuard(Arc<AtomicUsize>);

impl SpawnGuard {
    fn acquire(slots: &Arc<AtomicUsize>, max: usize) -> Option<SpawnGuard> {
        if slots.fetch_add(1, Ordering::Relaxed) >= max {
            slots.fetch_sub(1, Ordering::Relaxed);
            None
        } else {
            Some(SpawnGuard(Arc::clone(slots)))
        }
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Bind `path` and serve the self-MCP on a background accept thread (one thread
/// per connection). Returns the bind error so the caller decides if it's fatal.
pub fn serve(path: &str, ctx: ServeCtx, log: Logger) -> std::io::Result<()> {
    // A stale socket from a crashed prior run would block the bind; clear it.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    log.info(
        "mcp.serving",
        json!({"path": path, "tools": ["status", "subagent.spawn"], "resources": ["agentd://status"]}),
    );
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
            let resp = dispatch(req, ctx, log);
            if frame::write_line(&mut write, &resp).is_err() {
                break; // peer hung up mid-reply
            }
        }
    }
    log.debug("mcp.disconnect", json!({"peer": "unix"}));
}

/// Route one JSON-RPC request to a response.
fn dispatch(req: Request, ctx: &ServeCtx, log: &Logger) -> Response {
    match req.method.as_str() {
        "initialize" => Response::ok(
            req.id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                // `resources: {}` (no subscribe yet) advertises the agentd://
                // read-only resource surface; presence is what a client gates on.
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "agentd", "version": crate::VERSION}
            }),
        ),
        "ping" => Response::ok(req.id, json!({})),
        "tools/list" => {
            Response::ok(req.id, json!({"tools": [status_tool_def(), spawn_tool_def()]}))
        }
        "tools/call" => tools_call(req, ctx, log),
        "resources/list" => Response::ok(req.id, json!({"resources": resource_list()})),
        "resources/read" => resources_read(req, ctx),
        other => Response::err(req.id, json::METHOD_NOT_FOUND, format!("unsupported method: {other}")),
    }
}

/// The agentd:// resources this server exposes. v1: `agentd://status` (agentd's
/// own state). Per-served-subagent resources need a tracked async-spawn registry
/// (served spawns are sync today) — a follow-on.
fn resource_list() -> Value {
    json!([{
        "uri": "agentd://status",
        "name": "status",
        "description": "This agentd's run id, mode, version, pid, uptime, and spawn counts.",
        "mimeType": "application/json"
    }])
}

/// `resources/read` over the agentd:// scheme. A known URI returns a `contents`
/// body; an unknown/missing URI is a JSON-RPC INVALID_PARAMS error.
fn resources_read(req: Request, ctx: &ServeCtx) -> Response {
    let uri = req.params.as_ref().and_then(|p| p.get("uri")).and_then(Value::as_str).unwrap_or("");
    match crate::agentd_uri::AgentdResource::parse(uri) {
        Some(crate::agentd_uri::AgentdResource::Status) => Response::ok(
            req.id,
            json!({
                "contents": [{"uri": uri, "mimeType": "application/json", "text": ctx.status_body().to_string()}]
            }),
        ),
        // agentd://subagent/<handle> needs a tracked async-spawn registry (served
        // spawns are sync) — not available yet.
        _ => Response::err(req.id, json::INVALID_PARAMS, format!("unknown resource: {uri}")),
    }
}

fn tools_call(req: Request, ctx: &ServeCtx, log: &Logger) -> Response {
    let name = req.params.as_ref().and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or("");
    match name {
        "status" => {
            let body = ctx.status_body();
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
        "subagent.spawn" => handle_spawn(req, ctx, log),
        other => Response::err(req.id, json::INVALID_PARAMS, format!("unknown tool: {other}")),
    }
}

/// A peer delegates a task to agentd (RFC 0005 §3.2, sync). Build a fresh root
/// run from the daemon's payload template + the request, supervise it, and
/// return the distilled outcome. Bad params → a JSON-RPC error; a cap/scope
/// refusal or a run failure → `isError:true` inside a successful result (so the
/// caller's model adapts), never a crash.
///
/// **Trust boundary:** anyone able to connect to the `--serve-mcp` socket can
/// run instructions with this agentd's intelligence + tool scope. The operator
/// gates access via the socket's filesystem permissions; `async`/`detach` and
/// the spawn-payload's `limits`/`context_seed` overrides land with M3.
fn handle_spawn(req: Request, ctx: &ServeCtx, log: &Logger) -> Response {
    let id = req.id.clone();
    let args = req.params.as_ref().and_then(|p| p.get("arguments")).cloned().unwrap_or(json!({}));
    let instruction = args.get("instruction").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if instruction.is_empty() {
        // Malformed call (missing required param) → JSON-RPC error (RFC §3.2).
        return Response::err(id, json::INVALID_PARAMS, "subagent.spawn requires a non-empty 'instruction'");
    }
    // Concurrency cap → refused as a tool result, never a crash.
    let _permit = match SpawnGuard::acquire(&ctx.inflight, MAX_INFLIGHT_SPAWNS) {
        Some(g) => g,
        None => {
            return tool_error(id, format!("spawn refused: {MAX_INFLIGHT_SPAWNS} concurrent served spawns in flight"));
        }
    };
    let n = ctx.counter.fetch_add(1, Ordering::Relaxed);
    let handle = format!("served.{n}");
    let payload = build_served_payload(&ctx.base, &args, &handle);
    log.info("mcp.spawn", json!({"handle": handle, "servers": payload.mcp_servers.len()}));
    match supervise_once(ctx.exe.clone(), &payload, ctx.drain_timeout, log.clone()) {
        Ok(SuperviseResult::Completed(o)) => Response::ok(
            id,
            json!({
                "content": [{"type": "text", "text": distill(&o.result)}],
                "structuredContent": {
                    "handle": handle, "status": o.status.as_str(), "partial": o.partial, "result": o.result
                },
                "isError": false
            }),
        ),
        Ok(SuperviseResult::Failed(e)) => tool_error(id, format!("subagent failed: {e}")),
        Ok(SuperviseResult::Killed(r)) => tool_error(id, format!("subagent terminated: {r:?}")),
        Err(e) => tool_error(id, format!("subagent could not start: {e}")),
    }
}

/// Build a served run's payload from the daemon's template + the request. The
/// child's depth is minted here (a fresh root, not read from the request); the
/// `tool_scope` only ever narrows the daemon's server set (RFC 0005 §3.2). Pure.
fn build_served_payload(base: &SpawnPayload, args: &Value, handle: &str) -> SpawnPayload {
    let mut p = base.clone();
    p.instruction = args.get("instruction").and_then(Value::as_str).unwrap_or("").trim().to_string();
    p.output_contract = args.get("output_contract").map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    if let Some(names) = args.get("tool_scope").and_then(Value::as_array) {
        let wanted: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
        p.mcp_servers.retain(|s| wanted.contains(&s.name.as_str()));
    }
    p.telemetry.agent_id = handle.to_string();
    p.telemetry.agent_path = handle.to_string();
    p.depth = 0; // a fresh root run triggered by the peer
    p
}

/// A tool-domain failure: `isError:true` inside a *successful* JSON-RPC result.
fn tool_error(id: Id, msg: String) -> Response {
    Response::ok(id, json!({"content": [{"type": "text", "text": msg}], "isError": true}))
}

/// Cap a result value to text for the `content` block (the full value is also in
/// `structuredContent`).
fn distill(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() <= RESULT_CAP {
        s
    } else {
        s.chars().take(RESULT_CAP).collect::<String>() + "…[truncated]"
    }
}

fn status_tool_def() -> Value {
    json!({
        "name": "status",
        "description": "Report this agentd's run id, mode, version, pid and uptime (ms).",
        "inputSchema": {"type": "object", "properties": {}}
    })
}

fn spawn_tool_def() -> Value {
    json!({
        "name": "subagent.spawn",
        "description": "Delegate a task to this agentd: it runs a fresh agent (its own intelligence \
            + tool scope) and returns the distilled result. Give an 'instruction' and optionally an \
            'output_contract' and a 'tool_scope' (a subset of this agentd's MCP server names). Sync: \
            the call blocks until the run finishes.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "instruction": {"type": "string", "description": "the task for the spawned agent"},
                "output_contract": {"type": "string", "description": "exactly what the agent should return"},
                "tool_scope": {"type": "array", "items": {"type": "string"}, "description": "subset of MCP server names to grant"}
            },
            "required": ["instruction"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerSpec;
    use crate::obs::log::{Comp, Level, LogCtx};
    use crate::subagent::protocol::{IntelConfig, Limits, Telemetry};

    fn base() -> SpawnPayload {
        SpawnPayload {
            instruction: "standing".into(),
            output_contract: None,
            context_seed: Vec::new(),
            intelligence: IntelConfig { uri: "unix:/x".into(), token: None, model: None },
            mcp_servers: vec![
                McpServerSpec { name: "fs".into(), command: vec!["a".into()], tags: Vec::new() },
                McpServerSpec { name: "db".into(), command: vec!["b".into()], tags: Vec::new() },
            ],
            limits: Limits { max_steps: 10, max_tokens: 1000, deadline_ms: 1000, max_depth: 4 },
            telemetry: Telemetry {
                run_id: "r1".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                trace_id: None,
                log_level: "error".into(),
                log_content: false,
            },
            depth: 0,
            enable_exec: false,
            warm: false,
        }
    }

    fn ctx() -> ServeCtx {
        ServeCtx::new("r1".into(), "reactive".into(), "agentd".into(), base(), Duration::from_secs(5))
    }

    fn log() -> Logger {
        Logger::new(
            LogCtx {
                run_id: "r1".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: 0,
                trace_id: None,
            },
            Level::Error,
        )
    }

    fn req(method: &str, params: Option<Value>) -> Request {
        Request::new(Id::Num(1), method, params)
    }

    #[test]
    fn initialize_declares_tools_capability() {
        let r = dispatch(req("initialize", None), &ctx(), &log());
        let v = r.result.expect("ok");
        assert_eq!(v["protocolVersion"], PROTOCOL_VERSION);
        assert!(v["capabilities"]["tools"].is_object());
        assert_eq!(v["serverInfo"]["name"], "agentd");
    }

    #[test]
    fn tools_list_advertises_status_and_spawn() {
        let r = dispatch(req("tools/list", None), &ctx(), &log());
        let tools = r.result.expect("ok")["tools"].clone();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"status"));
        assert!(names.contains(&"subagent.spawn"));
    }

    #[test]
    fn status_call_returns_structured_state() {
        let r = dispatch(req("tools/call", Some(json!({"name": "status"}))), &ctx(), &log());
        let v = r.result.expect("ok");
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["run_id"], "r1");
        assert_eq!(v["structuredContent"]["mode"], "reactive");
    }

    #[test]
    fn initialize_declares_resources_capability() {
        let r = dispatch(req("initialize", None), &ctx(), &log());
        let v = r.result.expect("ok");
        assert!(v["capabilities"]["resources"].is_object(), "resources capability advertised");
    }

    #[test]
    fn resources_list_advertises_agentd_status() {
        let r = dispatch(req("resources/list", None), &ctx(), &log());
        let resources = r.result.expect("ok")["resources"].clone();
        let uris: Vec<&str> = resources.as_array().unwrap().iter().filter_map(|x| x["uri"].as_str()).collect();
        assert!(uris.contains(&"agentd://status"), "agentd://status listed: {uris:?}");
    }

    #[test]
    fn resources_read_status_returns_a_contents_body() {
        let r = dispatch(req("resources/read", Some(json!({"uri": "agentd://status"}))), &ctx(), &log());
        let v = r.result.expect("ok");
        let entry = &v["contents"][0];
        assert_eq!(entry["uri"], "agentd://status");
        assert_eq!(entry["mimeType"], "application/json");
        // the text is the JSON status body
        let body: Value = serde_json::from_str(entry["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["run_id"], "r1");
        assert_eq!(body["mode"], "reactive");
    }

    #[test]
    fn resources_read_unknown_uri_is_an_error() {
        let r = dispatch(req("resources/read", Some(json!({"uri": "agentd://ghost"}))), &ctx(), &log());
        assert!(r.error.is_some(), "unknown agentd:// uri → JSON-RPC error");
        let bad = dispatch(req("resources/read", Some(json!({"uri": "file:///x"}))), &ctx(), &log());
        assert!(bad.error.is_some(), "non-agentd uri → JSON-RPC error");
    }

    #[test]
    fn unknown_tool_and_method_are_errors() {
        let bad_tool = dispatch(req("tools/call", Some(json!({"name": "ghost"}))), &ctx(), &log());
        assert!(bad_tool.error.is_some());
        let bad_method = dispatch(req("frobnicate", None), &ctx(), &log());
        assert!(bad_method.error.is_some());
    }

    #[test]
    fn build_served_payload_sets_instruction_and_narrows_scope() {
        let p = build_served_payload(&base(), &json!({"instruction": "do x", "tool_scope": ["fs"]}), "served.0");
        assert_eq!(p.instruction, "do x");
        assert_eq!(p.mcp_servers.len(), 1); // narrowed to the requested subset
        assert_eq!(p.mcp_servers[0].name, "fs");
        assert_eq!(p.telemetry.agent_path, "served.0"); // handle minted here
        assert_eq!(p.depth, 0);
    }

    #[test]
    fn spawn_missing_instruction_is_a_jsonrpc_error() {
        // Malformed params → JSON-RPC error (not an isError result); never reaches a real spawn.
        let r = dispatch(
            req("tools/call", Some(json!({"name": "subagent.spawn", "arguments": {}}))),
            &ctx(),
            &log(),
        );
        assert!(r.error.is_some(), "missing instruction → JSON-RPC error");
    }
}
