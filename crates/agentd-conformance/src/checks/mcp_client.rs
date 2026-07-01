// SPDX-License-Identifier: Apache-2.0
//! agentd as an MCP **client**: the requests it sends a backing server during
//! the handshake + discovery + subscribe. A `confmcp` reference server records
//! every request agentd makes; the checks assert the client side of the spec.

use crate::{Category, Check, Harness, Outcome};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "mcp-client/initialize-sent",
            category: Category::McpClient,
            desc: "the client opens with initialize carrying protocolVersion + clientInfo",
            run: initialize_sent,
        },
        Check {
            id: "mcp-client/initialized-notification",
            category: Category::McpClient,
            desc: "the client sends notifications/initialized after the handshake",
            run: initialized_notification,
        },
        Check {
            id: "mcp-client/tools-list-discovered",
            category: Category::McpClient,
            desc: "the client discovers server tools via tools/list",
            run: tools_list_discovered,
        },
        Check {
            id: "mcp-client/subscribe-sent",
            category: Category::McpClient,
            desc: "a reactive client subscribes to the resource it watches",
            run: subscribe_sent,
        },
    ]
}

/// The recorded client requests, captured once and shared by every check in the
/// family (one reactive run, not one per check).
fn records(h: &Harness) -> &'static [Value] {
    static RECORDS: OnceLock<Vec<Value>> = OnceLock::new();
    RECORDS.get_or_init(|| record_client_requests(h))
}

/// Run a reactive agentd against the recording `confmcp` server. The daemon
/// handshakes + subscribes; confmcp then pushes an update, firing a reaction
/// whose subagent performs `tools/list` + `resources/read`. We wait until that
/// discovery is recorded (or time out), then return every recorded request.
fn record_client_requests(h: &Harness) -> Vec<Value> {
    let tmp = h.tempdir();
    let rec = tmp.path().join("rec.jsonl");
    let sock = tmp.path().join("confmcp.sock");
    let uri = "file:///conf-watch.json";

    // Launch confmcp as a Streamable HTTP MCP server on a unix socket; agentd
    // connects to it (v2.0.0 — no stdio spawn).
    let mut confmcp = std::process::Command::new(h.confmcp())
        .arg(&sock)
        .arg(&rec)
        .arg(uri)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn confmcp");
    let sock_deadline = Instant::now() + Duration::from_secs(5);
    while !sock.exists() {
        if Instant::now() >= sock_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let mcp = format!("ref=unix:{}", sock.display());
    let daemon = h.spawn(&[
        "--mode",
        "reactive",
        "--subscribe",
        uri,
        "--instruction",
        "stand by",
        "--intelligence",
        "unix:/nonexistent/agentd-conf.sock",
        "--mcp",
        &mcp,
        "--log-level",
        "warn",
    ]);

    // The reaction's subagent issues tools/list — the last of the client's
    // discovery path to land. Wait for it (or give up after a generous window).
    let deadline = Instant::now() + Duration::from_secs(12);
    let reqs = loop {
        let reqs = read_records(&rec);
        let saw_subscribe = reqs.iter().any(|r| r["method"] == "resources/subscribe");
        let saw_tools = reqs.iter().any(|r| r["method"] == "tools/list");
        if (saw_subscribe && saw_tools) || Instant::now() >= deadline {
            break reqs;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(daemon);
    let _ = confmcp.kill();
    let _ = confmcp.wait();
    reqs
}

fn read_records(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn find<'a>(reqs: &'a [Value], method: &str) -> Option<&'a Value> {
    reqs.iter().find(|r| r["method"] == method)
}

fn initialize_sent(h: &Harness) -> Outcome {
    let reqs = records(h);
    let Some(init) = find(reqs, "initialize") else {
        return Outcome::fail(format!(
            "no initialize sent; recorded {} requests",
            reqs.len()
        ));
    };
    let p = &init["params"];
    Outcome::require(
        p["protocolVersion"].is_string(),
        format!("initialize without protocolVersion: {init}"),
    )
    .and(|| {
        Outcome::require(
            p["clientInfo"].is_object(),
            format!("initialize without clientInfo: {init}"),
        )
    })
    .and(|| {
        Outcome::require(
            p["capabilities"].is_object(),
            format!("initialize without capabilities: {init}"),
        )
    })
}

fn initialized_notification(h: &Harness) -> Outcome {
    let reqs = records(h);
    let note = find(reqs, "notifications/initialized");
    Outcome::require(
        note.is_some(),
        "client never sent notifications/initialized".to_string(),
    )
    .and(|| {
        // A notification carries no id.
        Outcome::require(
            note.unwrap().get("id").is_none(),
            "notifications/initialized carried an id".to_string(),
        )
    })
}

fn tools_list_discovered(h: &Harness) -> Outcome {
    let reqs = records(h);
    Outcome::require(
        find(reqs, "tools/list").is_some(),
        "client never sent tools/list".to_string(),
    )
}

fn subscribe_sent(h: &Harness) -> Outcome {
    let reqs = records(h);
    let Some(sub) = find(reqs, "resources/subscribe") else {
        return Outcome::fail("client never subscribed to its watched resource".to_string());
    };
    Outcome::require(
        sub["params"]["uri"] == "file:///conf-watch.json",
        format!("subscribed to the wrong uri: {sub}"),
    )
}
