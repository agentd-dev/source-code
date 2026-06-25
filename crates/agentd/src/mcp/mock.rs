//! A minimal built-in MCP server, for tests and for operators kicking the
//! tyres on reactive setups. Hidden mode: `agentd --internal-mock-mcp <uri>`.
//!
//! It speaks stdio MCP (NDJSON JSON-RPC) and serves exactly one resource at
//! `<uri>`: `initialize` (advertising `resources.subscribe`), `resources/list`,
//! `resources/read`, and `resources/subscribe` — after which it emits one
//! `notifications/resources/updated{uri}` shortly after, so a reactive agent
//! has something to react to. Small enough to ship; invaluable for validating
//! reactivity by observation (RFC 0010, the M7 observe-suite).

use crate::json::{self, frame, Incoming, Request, Response};
use crate::wire::mcp::{method, PROTOCOL_VERSION};
use serde_json::json;
use std::io::{self, BufReader};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Out = Arc<Mutex<io::Stdout>>;

/// Run the mock server until stdin closes. `emit` controls whether it pushes a
/// `resources/updated` after a subscribe (off = test read-after-subscribe in
/// isolation). Returns the process exit code.
pub fn run(uri: &str, emit: bool) -> i32 {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let mut reader = BufReader::new(io::stdin());
    while let Ok(Some(bytes)) = frame::read_line(&mut reader) {
        if let Ok(Incoming::Request(req)) = serde_json::from_slice::<Incoming>(&bytes) {
            handle(&out, req, uri, emit);
        }
        // Notifications (e.g. notifications/initialized) need no reply.
    }
    0
}

fn handle(out: &Out, req: Request, uri: &str, emit: bool) {
    match req.method.as_str() {
        "initialize" => reply(
            out,
            Response::ok(
                req.id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"resources": {"subscribe": true, "listChanged": true}, "tools": {}},
                    "serverInfo": {"name": "agentd-mock", "version": crate::VERSION}
                }),
            ),
        ),
        "ping" => reply(out, Response::ok(req.id, json!({}))),
        "tools/list" => reply(out, Response::ok(req.id, json!({"tools": []}))),
        "resources/list" => {
            reply(out, Response::ok(req.id, json!({"resources": [{"uri": uri, "name": "mock"}]})))
        }
        "resources/read" => reply(
            out,
            Response::ok(
                req.id,
                json!({"contents": [{"uri": uri, "mimeType": "text/plain", "text": "the watched resource changed"}]}),
            ),
        ),
        "resources/unsubscribe" => reply(out, Response::ok(req.id, json!({}))),
        "resources/subscribe" => {
            reply(out, Response::ok(req.id, json!({})));
            // Drive the reactive loop: emit one update shortly after subscribe
            // (unless `emit` is off — used to test read-after-subscribe alone).
            if emit {
                let out = Arc::clone(out);
                let uri = uri.to_string();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(200));
                    let note = json::Notification::new(
                        method::NOTIFY_RESOURCES_UPDATED,
                        Some(json!({"uri": uri})),
                    );
                    let mut g = out.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = frame::write_line(&mut *g, &note);
                });
            }
        }
        other => reply(out, Response::err(req.id, json::METHOD_NOT_FOUND, format!("unsupported: {other}"))),
    }
}

fn reply(out: &Out, resp: Response) {
    let mut g = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = frame::write_line(&mut *g, &resp);
}
