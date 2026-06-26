//! `confmcp <record-file> [resource-uri]` — a minimal, spec-correct MCP server
//! that agentd connects to as a client, used by the MCP-client conformance
//! family. It speaks stdio NDJSON JSON-RPC, answers the handshake + discovery +
//! resource methods, and **appends every request it receives** (verbatim JSON,
//! one per line) to `<record-file>` so the suite can assert exactly what agentd's
//! client sent. Independent of the agentd library on purpose.

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};

fn main() {
    let mut args = std::env::args().skip(1);
    let record_path = args.next().expect("usage: confmcp <record-file> [uri]");
    let uri = args.next().unwrap_or_else(|| "file:///conf-watch.json".to_string());

    let stdin = io::stdin();
    let stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        record(&record_path, &line);

        let method = req["method"].as_str().unwrap_or("");
        let id = req.get("id").cloned();
        // Requests have an id; notifications don't and get no reply.
        let Some(id) = id else { continue };

        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"resources": {"subscribe": true, "listChanged": true}, "tools": {}},
                "serverInfo": {"name": "confmcp", "version": "1.0.0"}
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": [
                {"name": "echo", "description": "echo the input", "inputSchema": {"type": "object"}}
            ]}),
            "resources/list" => json!({"resources": [{"uri": uri, "name": "watched"}]}),
            "resources/read" => json!({"contents": [{"uri": uri, "mimeType": "text/plain", "text": "conf content"}]}),
            "resources/subscribe" | "resources/unsubscribe" => json!({}),
            _ => {
                reply(&stdout, json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}}));
                continue;
            }
        };
        reply(&stdout, json!({"jsonrpc": "2.0", "id": id, "result": result}));

        // After a subscribe, push one resources/updated so a reactive agent fires
        // a reaction — that reaction's subagent is what performs tools/list, so
        // this is how the client's discovery path becomes observable.
        if method == "resources/subscribe" {
            let uri = uri.clone();
            std::thread::spawn(move || {
                let stdout = io::stdout();
                std::thread::sleep(std::time::Duration::from_millis(150));
                let note = json!({"jsonrpc": "2.0", "method": "notifications/resources/updated", "params": {"uri": uri}});
                reply(&stdout, note);
            });
        }
    }
}

fn record(path: &str, line: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn reply(stdout: &io::Stdout, msg: Value) {
    let mut w = stdout.lock();
    let _ = writeln!(w, "{msg}");
    let _ = w.flush();
}
