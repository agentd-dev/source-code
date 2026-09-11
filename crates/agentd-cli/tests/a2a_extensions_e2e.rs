// SPDX-License-Identifier: AGPL-3.0-only
//! **Everything agentd speaks beyond core A2A is reachable the way the spec
//! provides for**, proven against the real listener.
//!
//! Three claims, in the order a peer meets them:
//!
//! 1. the agent card DECLARES each extension, with a versioned URI, so a client
//!    discovers the surface instead of reading our documentation;
//! 2. the `A2A-Extensions` handshake works — a client lists what it means to
//!    activate and the response echoes what actually was, which is the rule the
//!    spec states for the header;
//! 3. the operator admin family is reachable as an ordinary `SendMessage` with
//!    a command DataPart, so a client that has never heard of agentd can drain
//!    this instance — and a non-operator still cannot, whatever its grants.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const OPERATOR: &str = "ext-e2e-operator-token";
const USER: &str = "ext-e2e-user-token";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// POST, returning `(status line, headers, body)` — the headers matter here.
fn post(addr: &str, body: &str, extra: &[(&str, &str)]) -> (String, String, String) {
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let mut head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    while Instant::now() < deadline {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => panic!("read: {e}"),
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = match text.find("\r\n\r\n") {
        Some(i) => (text[..i].to_string(), text[i + 4..].to_string()),
        None => (text.clone(), String::new()),
    };
    (
        head.lines().next().unwrap_or("").to_string(),
        head.to_ascii_lowercase(),
        body,
    )
}

fn rpc(addr: &str, bearer: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let auth = format!("Bearer {bearer}");
    let (status, _, body) = post(addr, &body, &[("Authorization", &auth)]);
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("non-JSON reply ({e}) to {method}: {status} {body:?}"))
}

/// A command DataPart — the A2A-shaped way to invoke an operation.
fn command(op: &str, args: Value) -> Value {
    let mut data = json!({"op": op});
    if let (Some(d), Some(a)) = (data.as_object_mut(), args.as_object()) {
        for (k, v) in a {
            d.insert(k.clone(), v.clone());
        }
    }
    json!({"message": {"role": "ROLE_USER", "messageId": "m-1",
                       "parts": [{"data": {"agentd": data}}]}})
}

struct Daemon {
    child: Child,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_ready(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            std::thread::sleep(Duration::from_millis(200));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("listener never came up on {addr}");
}

fn boot() -> (Daemon, String) {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = common::unique_path("a2a-ext", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: a2a-ext\n  instruction: You are a test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             \x20 principals:\n\
             \x20   - match: {{ bearer_ref: \"{{{{secret:AGENTD_EXT_OPERATOR}}}}\" }}\n\
             \x20     role: operator\n\
             \x20   - match: {{ bearer_ref: \"{{{{secret:AGENTD_EXT_USER}}}}\" }}\n\
             \x20     role: user\n\
             \x20     grants: [\"*\"]\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n"
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_EXT_OPERATOR", OPERATOR)
        .env("AGENTD_EXT_USER", USER)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    wait_ready(&addr);
    (Daemon { child }, addr)
}

/// The card declares what agentd speaks beyond core A2A, and the commands are
/// skills — so discovery needs no agentd-specific knowledge.
#[test]
fn the_card_declares_the_extensions_and_the_commands_are_skills() {
    let (_d, addr) = boot();
    let card = rpc(&addr, OPERATOR, "GetAgentCard", json!({}))["result"].clone();

    let exts = card["capabilities"]["extensions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let uris: Vec<&str> = exts.iter().filter_map(|e| e["uri"].as_str()).collect();
    assert!(
        uris.contains(&"https://agentd.dev/a2a/ext/command/v1"),
        "the command vocabulary is declared: {uris:?}"
    );
    // Every declaration carries what the spec asks of an AgentExtension.
    for e in &exts {
        assert!(e["uri"].is_string(), "an extension needs a uri: {e}");
        assert!(
            e["description"].as_str().is_some_and(|d| !d.is_empty()),
            "an extension needs a description: {e}"
        );
        // `required` is a plain proto3 `bool`, so a conformant wire OMITS it
        // when false and a peer reads absence as "not required" — which is the
        // A2A extensions spec's own default. Absent or `false`, never `true`:
        // no extension agentd declares may be a precondition.
        assert_ne!(
            e["required"],
            json!(true),
            "no extension may be required: {e}"
        );
    }

    // The ops ride the declaration, so a peer can enumerate them…
    let command_ext = exts
        .iter()
        .find(|e| e["uri"] == "https://agentd.dev/a2a/ext/command/v1")
        .expect("the command extension");
    let ops: Vec<&str> = command_ext["params"]["ops"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for want in ["status", "workflow.run", "admin.drain"] {
        assert!(ops.contains(&want), "{want} is declared: {ops:?}");
    }

    // …and each is also a skill, which is A2A's own "what can I ask for".
    let skills: Vec<&str> = card["skills"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["id"].as_str()).collect())
        .unwrap_or_default();
    assert!(skills.contains(&"admin.drain"), "skills: {skills:?}");
}

/// The `A2A-Extensions` handshake: the client asks, the response reports what
/// was actually activated. An unknown URI is simply not echoed.
#[test]
fn the_extension_header_is_echoed_with_what_was_activated() {
    let (_d, addr) = boot();
    let body =
        json!({"jsonrpc": "2.0", "id": 1, "method": "GetAgentCard", "params": {}}).to_string();
    let auth = format!("Bearer {OPERATOR}");
    let (_, head, _) = post(
        &addr,
        &body,
        &[
            ("Authorization", &auth),
            (
                "A2A-Extensions",
                "https://agentd.dev/a2a/ext/command/v1, https://example.invalid/nope/v1",
            ),
        ],
    );
    assert!(
        head.contains("a2a-extensions: https://agentd.dev/a2a/ext/command/v1"),
        "the activated extension is echoed, and only it:\n{head}"
    );
    assert!(
        !head.contains("example.invalid"),
        "an extension we do not implement is not claimed as activated:\n{head}"
    );

    // A request that activates nothing gets no header — silence, not an empty one.
    let (_, head, _) = post(&addr, &body, &[("Authorization", &auth)]);
    assert!(!head.contains("a2a-extensions:"), "{head}");
}

/// The operator family, reached with a stock `SendMessage` — and refused for a
/// non-operator even with `grants: ["*"]`.
#[test]
fn admin_is_a_command_datapart_and_stays_operator_only() {
    let (_d, addr) = boot();

    // A `user` with a wildcard grant is still not an operator.
    let refused = rpc(
        &addr,
        USER,
        "SendMessage",
        command("admin.pause", json!({})),
    );
    assert!(
        refused["error"].is_object() || refused["result"]["status"]["state"] == "TASK_STATE_FAILED",
        "a non-operator must not pause the instance: {refused}"
    );

    // The operator can, through the same ordinary method.
    let paused = rpc(
        &addr,
        OPERATOR,
        "SendMessage",
        command("admin.pause", json!({"reason": "e2e"})),
    );
    let body = serde_json::to_string(&paused).unwrap();
    assert!(
        body.contains("paused"),
        "the admin op answers as a task: {body}"
    );

    let resumed = rpc(
        &addr,
        OPERATOR,
        "SendMessage",
        command("admin.resume", json!({})),
    );
    assert!(
        serde_json::to_string(&resumed).unwrap().contains("running"),
        "and resume brings it back: {resumed}"
    );
}
