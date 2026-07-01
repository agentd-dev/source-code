// SPDX-License-Identifier: Apache-2.0
//! `confmcp <unix-socket> <record-file> [resource-uri]` — a minimal, spec-correct
//! **Streamable HTTP** MCP server that agentd connects to as a client (over
//! `unix:<socket>`), used by the MCP-client conformance family. It answers the
//! handshake + discovery + resource methods and **appends every request it
//! receives** (one JSON object per line) to `<record-file>` so the suite can
//! assert exactly what agentd's client sent. Independent of the agentd library.

use agentd_conformance::mcp_http_server::serve;
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::Write;

fn main() {
    let mut args = std::env::args().skip(1);
    let socket = args
        .next()
        .expect("usage: confmcp <unix-socket> <record-file> [uri]");
    let record_path = args
        .next()
        .expect("usage: confmcp <unix-socket> <record-file> [uri]");
    let uri = args
        .next()
        .unwrap_or_else(|| "file:///conf-watch.json".to_string());

    serve(&socket, move |req, notifier| {
        record(&record_path, req);
        let method = req["method"].as_str().unwrap_or("");
        // Requests have an id; notifications don't and get no reply.
        let id = req.get("id").cloned()?;

        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"resources": {"subscribe": true, "listChanged": true}, "tools": {}},
                "serverInfo": {"name": "confmcp", "version": "1.0.0"}
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": [
                {"name": "echo", "description": "echo the input", "inputSchema": {"type": "object"}}
            ]}),
            "resources/list" => json!({"resources": [{"uri": uri, "name": "watched"}]}),
            "resources/read" => {
                json!({"contents": [{"uri": uri, "mimeType": "text/plain", "text": "conf content"}]})
            }
            "resources/unsubscribe" => json!({}),
            "resources/subscribe" => {
                // After a subscribe, push one resources/updated (on the GET SSE
                // stream) so a reactive agent fires a reaction — that reaction's
                // subagent performs tools/list, making the client's discovery
                // path observable in the record.
                let notifier = notifier.clone();
                let uri = uri.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    notifier.push(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/resources/updated",
                        "params": {"uri": uri}
                    }));
                });
                json!({})
            }
            _ => {
                return Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "method not found"}
                }));
            }
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    });
}

/// Append the request (serialized, one per line) to the record file.
fn record(path: &str, req: &Value) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{req}");
    }
}
