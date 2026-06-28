// SPDX-License-Identifier: Apache-2.0
//! agentd as an MCP **server** (`--serve-mcp`): the JSON-RPC 2.0 + MCP protocol
//! it must speak to a peer. Every check connects a raw line-delimited client to
//! a live served daemon and asserts the wire contract.

use crate::{Category, Check, Harness, Outcome};
use serde_json::{Value, json};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "mcp-server/initialize-handshake",
            category: Category::McpServer,
            desc: "initialize returns protocolVersion + tools & resources capabilities",
            run: initialize_handshake,
        },
        Check {
            id: "mcp-server/jsonrpc-envelope",
            category: Category::McpServer,
            desc: "responses echo the request id and carry jsonrpc:2.0",
            run: jsonrpc_envelope,
        },
        Check {
            id: "mcp-server/tools-list-shape",
            category: Category::McpServer,
            desc: "tools/list returns tools[] each with a name and an inputSchema",
            run: tools_list_shape,
        },
        Check {
            id: "mcp-server/tools-list-core",
            category: Category::McpServer,
            desc: "tools/list advertises the core tools (status, subagent.spawn)",
            run: tools_list_core,
        },
        Check {
            id: "mcp-server/tools-call-status",
            category: Category::McpServer,
            desc: "tools/call status returns content + structuredContent state",
            run: tools_call_status,
        },
        Check {
            id: "mcp-server/unknown-method",
            category: Category::McpServer,
            desc: "an unknown method is a JSON-RPC METHOD_NOT_FOUND (-32601)",
            run: unknown_method,
        },
        Check {
            id: "mcp-server/invalid-params",
            category: Category::McpServer,
            desc: "a malformed tool call (missing required arg) is INVALID_PARAMS (-32602)",
            run: invalid_params,
        },
        Check {
            id: "mcp-server/unknown-tool",
            category: Category::McpServer,
            desc: "tools/call for an unknown tool is signalled as an error",
            run: unknown_tool,
        },
        Check {
            id: "mcp-server/resources-list",
            category: Category::McpServer,
            desc: "resources/list advertises agent://status",
            run: resources_list,
        },
        Check {
            id: "mcp-server/resources-read-status",
            category: Category::McpServer,
            desc: "resources/read agent://status returns its contents",
            run: resources_read_status,
        },
        Check {
            id: "mcp-server/run-listed",
            category: Category::McpServer,
            desc: "resources/list advertises the stable agent://run/<run_id> resource",
            run: run_listed,
        },
        Check {
            id: "mcp-server/resources-read-run",
            category: Category::McpServer,
            desc: "resources/read agent://run/<run_id> returns a body with run_id + mode",
            run: resources_read_run,
        },
        Check {
            id: "mcp-server/resources-read-unknown",
            category: Category::McpServer,
            desc: "resources/read of an unknown uri is RESOURCE_NOT_FOUND (-32002)",
            run: resources_read_unknown,
        },
        Check {
            id: "mcp-server/ping",
            category: Category::McpServer,
            desc: "ping returns a result",
            run: ping,
        },
        Check {
            id: "mcp-server/notification-no-response",
            category: Category::McpServer,
            desc: "a notification (no id) draws no response and doesn't desync the stream",
            run: notification_no_response,
        },
        Check {
            id: "mcp-server/malformed-json-survives",
            category: Category::McpServer,
            desc: "a non-JSON line doesn't crash the server; the next request still works",
            run: malformed_json_survives,
        },
    ]
}

// --- helpers ---------------------------------------------------------------

fn err_code(resp: &Value) -> Option<i64> {
    resp.get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_i64)
}

fn is_tool_error(resp: &Value) -> bool {
    // Either a JSON-RPC error, or a tool result flagged isError:true.
    resp.get("error").is_some() || resp["result"]["isError"] == json!(true)
}

// --- checks ----------------------------------------------------------------

fn initialize_handshake(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s.client().call("initialize", json!({}));
    let res = &r["result"];
    Outcome::require(
        res["protocolVersion"].is_string(),
        format!("no protocolVersion: {r}"),
    )
    .and(|| {
        Outcome::require(
            res["capabilities"]["tools"].is_object(),
            format!("no tools capability: {r}"),
        )
    })
    .and(|| {
        Outcome::require(
            res["capabilities"]["resources"].is_object(),
            format!("no resources capability: {r}"),
        )
    })
}

fn jsonrpc_envelope(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let id = s.client().next_id();
    let r = s.client().call("ping", json!({}));
    Outcome::require(r["jsonrpc"] == json!("2.0"), format!("jsonrpc != 2.0: {r}")).and(|| {
        Outcome::require(
            r["id"] == json!(id),
            format!("id not echoed (want {id}): {r}"),
        )
    })
}

fn tools_list_shape(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s.client().call("tools/list", json!({}));
    let Some(tools) = r["result"]["tools"].as_array() else {
        return Outcome::fail(format!("no tools array: {r}"));
    };
    if tools.is_empty() {
        return Outcome::fail("tools list is empty");
    }
    for t in tools {
        if !t["name"].is_string() {
            return Outcome::fail(format!("tool without a name: {t}"));
        }
        if !t["inputSchema"].is_object() {
            return Outcome::fail(format!("tool {} without an inputSchema", t["name"]));
        }
    }
    Outcome::note(format!("{} tools", tools.len()))
}

fn tools_list_core(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s.client().call("tools/list", json!({}));
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    for want in ["status", "subagent.spawn"] {
        if !names.contains(&want) {
            return Outcome::fail(format!("missing core tool {want:?}; have {names:?}"));
        }
    }
    Outcome::pass()
}

fn tools_call_status(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s
        .client()
        .call("tools/call", json!({"name": "status", "arguments": {}}));
    let res = &r["result"];
    Outcome::require(
        res["content"][0]["type"] == json!("text"),
        format!("no text content: {r}"),
    )
    .and(|| {
        // structuredContent should carry live state (a run id / version / pid).
        let sc = &res["structuredContent"];
        Outcome::require(
            sc.is_object()
                && (sc.get("version").is_some()
                    || sc.get("run_id").is_some()
                    || sc.get("pid").is_some()),
            format!("status structuredContent lacks state: {r}"),
        )
    })
}

fn unknown_method(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s.client().call("no/such/method", json!({}));
    Outcome::require(
        err_code(&r) == Some(-32601),
        format!("want -32601, got: {r}"),
    )
}

fn invalid_params(h: &Harness) -> Outcome {
    let mut s = h.serve();
    // subagent.spawn requires a non-empty instruction → INVALID_PARAMS.
    let r = s.client().call(
        "tools/call",
        json!({"name": "subagent.spawn", "arguments": {}}),
    );
    Outcome::require(
        err_code(&r) == Some(-32602),
        format!("want -32602, got: {r}"),
    )
}

fn unknown_tool(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s.client().call(
        "tools/call",
        json!({"name": "no.such.tool", "arguments": {}}),
    );
    Outcome::require(
        is_tool_error(&r),
        format!("unknown tool not signalled as error: {r}"),
    )
}

fn resources_list(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s.client().call("resources/list", json!({}));
    let has = r["result"]["resources"]
        .as_array()
        .map(|a| a.iter().any(|res| res["uri"] == json!("agent://status")))
        .unwrap_or(false);
    Outcome::require(has, format!("agent://status not listed: {r}"))
}

fn resources_read_status(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s
        .client()
        .call("resources/read", json!({"uri": "agent://status"}));
    Outcome::require(
        r["result"]["contents"].is_array(),
        format!("no contents: {r}"),
    )
}

/// Read this daemon's run id out of `agent://status` (its body's `run_id`),
/// then form the `agent://run/<run_id>` uri the run resource is published under.
fn run_uri_of(s: &mut crate::harness::Served) -> Option<String> {
    let r = s
        .client()
        .call("resources/read", json!({"uri": "agent://status"}));
    let text = r["result"]["contents"][0]["text"].as_str()?;
    let body: Value = serde_json::from_str(text).ok()?;
    let run_id = body["run_id"].as_str()?;
    Some(format!("agent://run/{run_id}"))
}

fn run_listed(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let Some(uri) = run_uri_of(&mut s) else {
        return Outcome::fail("could not read run_id from agent://status");
    };
    let r = s.client().call("resources/list", json!({}));
    let has = r["result"]["resources"]
        .as_array()
        .map(|a| a.iter().any(|res| res["uri"] == json!(uri)))
        .unwrap_or(false);
    Outcome::require(has, format!("{uri} not listed: {r}"))
}

fn resources_read_run(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let Some(uri) = run_uri_of(&mut s) else {
        return Outcome::fail("could not read run_id from agent://status");
    };
    let r = s.client().call("resources/read", json!({"uri": uri}));
    let Some(text) = r["result"]["contents"][0]["text"].as_str() else {
        return Outcome::fail(format!("no run contents body: {r}"));
    };
    let Ok(body) = serde_json::from_str::<Value>(text) else {
        return Outcome::fail(format!("run body is not JSON: {text}"));
    };
    Outcome::require(
        body["run_id"].is_string(),
        format!("run body lacks run_id: {body}"),
    )
    .and(|| {
        Outcome::require(
            body["mode"].is_string(),
            format!("run body lacks mode: {body}"),
        )
    })
}

fn resources_read_unknown(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s
        .client()
        .call("resources/read", json!({"uri": "agent://no-such-thing"}));
    Outcome::require(
        err_code(&r) == Some(-32002),
        format!("want -32002, got: {r}"),
    )
}

fn ping(h: &Harness) -> Outcome {
    let mut s = h.serve();
    let r = s.client().call("ping", json!({}));
    Outcome::require(
        r.get("result").is_some(),
        format!("ping had no result: {r}"),
    )
}

fn notification_no_response(h: &Harness) -> Outcome {
    let mut s = h.serve();
    // A JSON-RPC notification has no id → no response is allowed. We send one,
    // then a normal request; the response we read back must be the request's
    // (id matched), proving the notification neither replied nor desynced.
    let _ = s
        .client()
        .raw(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string());
    let id = s.client().next_id();
    let r = s.client().call("ping", json!({}));
    Outcome::require(
        r["id"] == json!(id),
        format!("stream desynced after a notification: {r}"),
    )
}

fn malformed_json_survives(h: &Harness) -> Outcome {
    let mut s = h.serve();
    // A non-JSON line: the server may reply with a parse error or ignore it, but
    // it must not crash — the next valid request still gets a correct response.
    let _ = s.client().raw("this is not json at all");
    let id = s.client().next_id();
    let r = s.client().call("ping", json!({}));
    Outcome::require(
        r["id"] == json!(id) && r.get("result").is_some(),
        format!("server didn't recover after malformed input: {r}"),
    )
}
