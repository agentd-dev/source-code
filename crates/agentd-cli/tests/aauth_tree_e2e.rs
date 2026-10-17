// SPDX-License-Identifier: AGPL-3.0-only
//! One AAuth identity for the whole process tree, from a verified provider.
//!
//! The daemon verifies the Agent Provider's metadata and fetches one agent
//! token when it starts, and keeps it fresh. A turn worker and a subagent are
//! handed that token, the key's file and what the tree knows of each server
//! with their spawn, so their MCP calls and model calls are signed with the
//! daemon's key and present its token, and the provider is asked for nothing
//! more; a token one child earns from a server is handed to the children
//! after it. An instance-tier child, a daemon of its own, is composed the
//! same provider and key, and never makes a key of its own; the links between
//! it and its parent are not signed. Metadata naming another issuer refuses
//! the start; a provider that does not answer at startup leaves every request
//! that needs it unsent until it answers.
//!
//! A mock provider records what it was asked, and a proxy in front of each
//! server records every request that reached it, so each signature is
//! checked against the daemon's public key as an AAuth server checks it.
#![cfg(all(unix, feature = "aauth"))]

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::aauth::AgentKey;
use agentd::mcp::mock_http::{Lifecycle, Options};
use serde_json::{Value, json};

/// The daemon's key, which every test writes to its key file.
const SEED: [u8; 32] = [21; 32];

/// One request as it arrived: method, path, headers (names lower-cased) and
/// body.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// Read one request off `conn`, with the bytes it arrived as.
fn read_request(conn: &TcpStream) -> Option<(Seen, Vec<u8>)> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut r = BufReader::new(conn.try_clone().ok()?);
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut raw = line.clone().into_bytes();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        raw.extend_from_slice(h.as_bytes());
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0usize);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok();
    raw.extend_from_slice(&body);
    let mut parts = line.split_whitespace();
    let seen = Seen {
        method: parts.next().unwrap_or_default().to_string(),
        path: parts.next().unwrap_or_default().to_string(),
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    Some((seen, raw))
}

/// A mock Agent Provider on `listener`. Its metadata names `issuer`, or its
/// own origin when `None`; it enrolls any key and issues `tok-<n>`, counting
/// from 1, that expire in an hour. Returns what it was asked, in order.
fn serve_provider(listener: TcpListener, issuer: Option<String>) -> Log {
    serve_provider_for(listener, issuer, 3600)
}

/// [`serve_provider`], its tokens expiring in `ttl` seconds.
fn serve_provider_for(listener: TcpListener, issuer: Option<String>, ttl: u64) -> Log {
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let issuer = issuer.unwrap_or(origin);
    let log = Log::default();
    let rec = Arc::clone(&log);
    std::thread::spawn(move || {
        let mut issued = 0;
        for mut c in listener.incoming().flatten() {
            let Some((req, _)) = read_request(&c) else {
                continue;
            };
            let body = if req.path == "/.well-known/aauth-agent.json" {
                json!({"issuer": issuer}).to_string()
            } else if req.path == "/enroll" {
                json!({"agent": "aauth:tree@apd.test"}).to_string()
            } else {
                issued += 1;
                json!({"agent_token": format!("tok-{issued}"), "expires_in": ttl}).to_string()
            };
            rec.lock().unwrap().push(req);
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    log
}

/// A proxy in front of `upstream` that records each request before passing
/// it on.
fn spawn_capture_proxy(upstream: &str) -> (String, Log) {
    spawn_issuing_proxy(upstream, None)
}

/// [`spawn_capture_proxy`], which answers the first `tools/call` of the tool
/// `issue` names with a `401` that issues the access token `acc-1` (AAuth
/// Case B) instead of passing it on.
fn spawn_issuing_proxy(upstream: &str, issue: Option<&'static str>) -> (String, Log) {
    let up = upstream.trim_start_matches("http://").to_string();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", l.local_addr().unwrap());
    let log = Log::default();
    let rec = Arc::clone(&log);
    let issued = Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (rec, up, issued) = (Arc::clone(&rec), up.clone(), Arc::clone(&issued));
            std::thread::spawn(move || {
                let Some((req, raw)) = read_request(&c) else {
                    return;
                };
                let call = serde_json::from_str::<Value>(&req.body).ok().filter(|b| {
                    b["method"] == "tools/call" && b["params"]["name"].as_str() == issue
                });
                rec.lock().unwrap().push(req);
                if call.is_some() && !issued.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let mut back = c;
                    let _ = write!(
                        back,
                        "HTTP/1.1 401 Unauthorized\r\nAAuth-Access: acc-1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    return;
                }
                let Ok(mut server) = TcpStream::connect(&up) else {
                    return;
                };
                let _ = server.write_all(&raw);
                let mut back = c;
                let _ = std::io::copy(&mut server, &mut back);
            });
        }
    });
    (uri, log)
}

/// An MCP server at 2026-07-28 (it answers a `GET` with `405`, so AAuth
/// resource discovery is quick) behind a recording proxy.
fn spawn_mcp() -> (String, Log) {
    let upstream = common::mock_in_thread(Options {
        emit: false,
        hold_listen: false,
        lifecycle: Lifecycle::Discover,
        ..Options::new("mock://res")
    });
    spawn_capture_proxy(&upstream)
}

/// Standard base64, as the `Signature` value carries it.
fn b64_decode(s: &str) -> Vec<u8> {
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        bits = bits << 6 | v as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    out
}

/// Whether `req` is signed with the key whose public half is `public`:
/// the signature base rebuilt from what arrived, as an AAuth server does.
fn signed_by(req: &Seen, public: &[u8]) -> bool {
    let h = |name: &str| req.headers.get(name).cloned().unwrap_or_default();
    let input = h("signature-input");
    let Some(params) = input.strip_prefix("sig=") else {
        return false;
    };
    let mut base = format!(
        "\"@method\": {}\n\"@authority\": {}\n\"@path\": {}\n",
        req.method,
        h("host"),
        req.path
    );
    if params.contains("\"content-digest\"") {
        base += &format!("\"content-digest\": {}\n", h("content-digest"));
    }
    base += &format!(
        "\"signature-key\": {}\n\"@signature-params\": {params}",
        h("signature-key")
    );
    let sig = h("signature");
    let raw = sig.trim_start_matches("sig=:").trim_end_matches(':');
    agentd::aauth::verify_ed25519(public, base.as_bytes(), &b64_decode(raw)).is_ok()
}

/// The token a request presents as its key id.
fn presented(req: &Seen) -> String {
    req.headers
        .get("signature-key")
        .and_then(|k| k.strip_prefix("sig=jwt;jwt=\""))
        .map(|t| t.trim_end_matches('"').to_string())
        .unwrap_or_default()
}

/// The `tools/call` requests `log` holds, for the tool named `name`.
fn calls_to(log: &Log, name: &str) -> Vec<Seen> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|r| {
            serde_json::from_str::<Value>(&r.body).is_ok_and(|b| {
                b["method"] == "tools/call" && b["params"]["name"].as_str() == Some(name)
            })
        })
        .cloned()
        .collect()
}

/// What the provider was asked, as `METHOD /path`.
fn asked(log: &Log) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// The daemon's key, written to `dir/agent.key`; returns the path and the
/// public key.
fn write_key(dir: &std::path::Path) -> (String, Vec<u8>) {
    let key = AgentKey::from_seed(&SEED).unwrap();
    let path = dir.join("agent.key");
    key.persist(&path).unwrap();
    (path.display().to_string(), key.public_bytes().to_vec())
}

fn run(dir: &std::path::Path, config: &str) -> (Option<i32>, String) {
    let cfg = dir.join("agentd.yaml");
    std::fs::write(&cfg, config).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env("AGENTD_STATE_DIR", dir.join("state"))
        .output()
        .expect("run agentd");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A turn worker's MCP call and the call of the subagent it delegates to
/// are signed with the daemon's key and present the daemon's agent token,
/// as are the model calls each makes; the provider was asked for one
/// metadata document, one enrollment and one token for the whole tree.
#[test]
fn a_turn_and_its_subagent_sign_as_the_daemon_with_its_one_token() {
    let dir = tempfile::tempdir().unwrap();
    let (key_file, public) = write_key(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = format!("http://{}", listener.local_addr().unwrap());
    let ap = serve_provider(listener, None);
    let (mcp, seen) = spawn_mcp();
    let playbook = dir.path().join("playbook.json");
    std::fs::write(
        &playbook,
        json!({
            "turns": [
                {"tool_calls": [{"name": "mock.ops", "arguments": {}}]},
                {"tool_calls": [{"name": "subagent.run", "arguments": {
                    "instruction": "look something up", "mode": "sync"}}]},
                {"content": "delegated and done"}
            ],
            "match": [
                {"when_contains": "Result for zebra-child", "content": "child done"},
                {"when_contains": "You are agentd, an autonomous agent.",
                 "tool_calls": [{"name": "mock.search.query", "arguments": {"query": "zebra-child"}}]}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let llm = agentd::intel::mock::inprocess(&format!("file:{}", playbook.display())).unwrap();
    let (model, model_seen) = spawn_capture_proxy(&format!("http://{llm}"));
    let (code, stderr) = run(
        dir.path(),
        &format!(
            "agent: {{name: tree, preflight: never}}\n\
             store: {{kind: memory}}\n\
             intelligence: {{endpoints: \"{model}\", model: mock}}\n\
             mcp: {{servers: [{{name: mock, endpoint: \"{mcp}\"}}]}}\n\
             security: {{aauth: {{provider: \"{provider}\", key_file: \"{key_file}\"}}}}\n\
             workflows:\n\
             \x20 - name: work\n    steps:\n      s: {{kind: once}}\n      a: {{kind: agent, depends_on: [s], instruction: \"list and delegate\"}}\n      f: {{kind: finish, depends_on: [a]}}\n\
             lifecycle: {{run_until: idle, idle_grace: 500ms}}\n\
             observability: {{log_level: info}}\n"
        ),
    );
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let from_turn = calls_to(&seen, "mock.ops");
    let from_subagent = calls_to(&seen, "search.query");
    assert_eq!(from_turn.len(), 1, "the turn's call:\n{stderr}");
    assert_eq!(from_subagent.len(), 1, "the subagent's call:\n{stderr}");
    for (who, call) in [("turn", &from_turn[0]), ("subagent", &from_subagent[0])] {
        assert!(
            signed_by(call, &public),
            "{who}: not the daemon's key: {call:?}"
        );
        assert_eq!(presented(call), "tok-1", "{who}: not the daemon's token");
    }
    let model_calls = model_seen.lock().unwrap().clone();
    assert!(model_calls.len() >= 4, "{model_calls:?}");
    for call in &model_calls {
        assert!(signed_by(call, &public), "a model call: {call:?}");
        assert_eq!(presented(call), "tok-1");
    }
    // Every MCP request that reached the server was signed so; the only
    // unsigned ones are the GETs of its public discovery document.
    for req in seen.lock().unwrap().iter().filter(|r| r.method == "POST") {
        assert!(signed_by(req, &public), "{req:?}");
        assert_eq!(presented(req), "tok-1", "{req:?}");
    }
    assert_eq!(
        asked(&ap),
        [
            "GET /.well-known/aauth-agent.json",
            "POST /enroll",
            "POST /agent-token"
        ],
        "one verification, enrollment and token for the tree:\n{stderr}"
    );
}

/// An instance-tier child is a daemon of its own: it is composed the
/// daemon's provider and key file, verifies and enrolls as it starts, and
/// its MCP calls are signed with the same key.
#[cfg(feature = "a2a")]
#[test]
fn an_instance_child_signs_with_the_daemons_key() {
    let dir = tempfile::tempdir().unwrap();
    let (key_file, public) = write_key(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = format!("http://{}", listener.local_addr().unwrap());
    let ap = serve_provider(listener, None);
    let (mcp, seen) = spawn_mcp();
    let socket = dir.path().join("parent.sock");
    let (code, stderr) = run(
        dir.path(),
        &format!(
            "agent: {{name: tree}}\n\
             store: {{kind: memory}}\n\
             intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
             a2a: {{listen: \"unix://{socket}\"}}\n\
             security: {{aauth: {{provider: \"{provider}\", key_file: \"{key_file}\"}}}}\n\
             subagents:\n\
             \x20 templates:\n\
             \x20   probe:\n\
             \x20     instruction: |\n\
             \x20       The probe.\n\
             \x20       :::!mcp{{name=kb}}\n\
             \x20       endpoint: \"{mcp}\"\n\
             \x20       :::\n\
             \x20       :::!workflow{{name=probe}}\n\
             \x20       steps:\n\
             \x20         s: {{kind: once}}\n\
             \x20         call: {{kind: mcp.tool, depends_on: [s], server: kb, tool: state.list, args: {{key: x}}}}\n\
             \x20         f: {{kind: finish, depends_on: [call], status: completed, output: listed}}\n\
             \x20       :::\n\
             \x20     mode: sync\n\
             \x20     result: {{workflow: probe}}\n\
             \x20     ttl: 20s\n\
             workflows:\n\
             \x20 - name: spawner\n    steps:\n      s: {{kind: once}}\n      spawn: {{kind: subagent, template: probe, depends_on: [s], timeout: 30s}}\n      f: {{kind: finish, depends_on: [spawn], status: completed, output: \"{{{{steps.spawn.output.result.output}}}}\"}}\n\
             lifecycle: {{run_until: idle, idle_grace: 1500ms}}\n\
             observability: {{log_level: info}}\n",
            socket = socket.display()
        ),
    );
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert!(
        !events(&stderr, "instance.result").is_empty(),
        "the child's run reported home:\n{stderr}"
    );
    let calls = calls_to(&seen, "state.list");
    assert_eq!(calls.len(), 1, "{:?}", seen.lock().unwrap());
    assert!(signed_by(&calls[0], &public), "{:?}", calls[0]);
    // The child fetched its own token, for the same key.
    assert_eq!(presented(&calls[0]), "tok-2");
    let enrolls: Vec<String> = ap
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path == "/enroll")
        .map(|r| r.headers["signature-key"].clone())
        .collect();
    assert_eq!(enrolls.len(), 2, "the daemon's and the child's");
    assert_eq!(enrolls[0], enrolls[1], "both presented the one key");
}

/// Metadata that names another issuer refuses the start, before the key is
/// enrolled with whoever served it or anything is dialed.
#[test]
fn provider_metadata_naming_another_issuer_refuses_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let (key_file, _) = write_key(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = format!("http://{}", listener.local_addr().unwrap());
    let ap = serve_provider(listener, Some("https://apd.elsewhere.example".into()));
    let (mcp, seen) = spawn_mcp();
    let (code, stderr) = run(
        dir.path(),
        &format!(
            "agent: {{name: poisoned, instruction: Work.}}\n\
             store: {{kind: memory}}\n\
             intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
             mcp: {{servers: [{{name: kb, endpoint: \"{mcp}\"}}]}}\n\
             security: {{aauth: {{provider: \"{provider}\", key_file: \"{key_file}\"}}}}\n"
        ),
    );
    assert_eq!(code, Some(2), "stderr:\n{stderr}");
    let exit = events(&stderr, "proc.exit");
    assert!(
        exit.iter().any(|e| e["err"]
            .as_str()
            .is_some_and(|m| m.contains("does not match configured provider")
                && m.contains("host-poisoning"))),
        "stderr:\n{stderr}"
    );
    assert_eq!(asked(&ap), ["GET /.well-known/aauth-agent.json"]);
    assert!(seen.lock().unwrap().is_empty(), "nothing was dialed");
}

/// A provider that does not answer at startup does not stop the daemon, and
/// lets nothing out unsigned: the server's connect and the step's calls fail
/// unsent, and the log says so. When the provider answers, its metadata is
/// verified first, the server joins on signed requests, and the step's call
/// goes out signed.
#[test]
fn a_provider_down_at_startup_signs_nothing_until_it_answers() {
    let dir = tempfile::tempdir().unwrap();
    let (key_file, public) = write_key(dir.path());
    let port = common::free_port();
    let provider = format!("http://127.0.0.1:{port}");
    let (mcp, seen) = spawn_mcp();
    let cfg = dir.path().join("agentd.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{name: late}}\n\
             store: {{kind: memory}}\n\
             intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
             mcp: {{servers: [{{name: kb, endpoint: \"{mcp}\"}}]}}\n\
             security: {{aauth: {{provider: \"{provider}\", key_file: \"{key_file}\"}}}}\n\
             workflows:\n\
             \x20 - name: call\n    steps:\n      s: {{kind: once}}\n      call: {{kind: mcp.tool, depends_on: [s], server: kb, tool: state.list, args: {{key: x}}, retry: {{max: 8, backoff: 300ms}}}}\n      f: {{kind: finish, depends_on: [call], status: completed}}\n\
             lifecycle: {{run_until: idle, idle_grace: 500ms}}\n\
             observability: {{log_level: info}}\n"
        ),
    )
    .unwrap();
    let log_path = dir.path().join("stderr.log");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log_path).unwrap())
        .spawn()
        .expect("spawn agentd");
    let read = || std::fs::read_to_string(&log_path).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(20);
    // The server's connect at boot needed the identity, and was refused.
    while !read().contains("\"mcp.connect.fail\"") {
        assert!(
            Instant::now() < deadline && daemon.try_wait().unwrap().is_none(),
            "no mcp.connect.fail:\n{}",
            read()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let refused = events(&read(), "mcp.connect.fail");
    assert!(
        refused[0]["err"]
            .as_str()
            .is_some_and(|e| e.contains("aauth: provider metadata")),
        "{refused:?}"
    );
    assert!(
        seen.lock().unwrap().iter().all(|r| r.method == "GET"),
        "nothing but public discovery reached the server: {:?}",
        seen.lock().unwrap()
    );
    let ap = serve_provider(TcpListener::bind(("127.0.0.1", port)).unwrap(), None);
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = daemon.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = daemon.kill();
            panic!("the daemon did not finish:\n{}", read());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stderr = read();
    assert_eq!(status.code(), Some(0), "stderr:\n{stderr}");
    let unavailable = events(&stderr, "auth.unavailable");
    assert_eq!(unavailable[0]["target"], "aauth", "{stderr}");
    assert!(
        unavailable[0]["err"]
            .as_str()
            .is_some_and(|e| e.starts_with("provider metadata")),
        "{stderr}"
    );
    assert!(
        events(&stderr, "auth.recovered")
            .iter()
            .any(|e| e["target"] == "aauth"),
        "{stderr}"
    );
    assert!(!events(&stderr, "mcp.joined").is_empty(), "{stderr}");
    assert_eq!(
        asked(&ap),
        [
            "GET /.well-known/aauth-agent.json",
            "POST /enroll",
            "POST /agent-token"
        ],
        "verified before it was used"
    );
    let calls = calls_to(&seen, "state.list");
    assert_eq!(calls.len(), 1, "{:?}", seen.lock().unwrap());
    for req in seen.lock().unwrap().iter().filter(|r| r.method == "POST") {
        assert!(signed_by(req, &public), "{req:?}");
        assert_eq!(presented(req), "tok-1", "{req:?}");
    }
}

/// A token a server issues one child is the tree's: a turn worker earns an
/// access token (Case B) on its call, hands it up, and the subagent it then
/// delegates to is handed it, so the subagent's call presents it without
/// earning one of its own.
#[test]
fn a_token_one_child_earns_is_presented_by_the_children_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let (key_file, _) = write_key(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = format!("http://{}", listener.local_addr().unwrap());
    let _ap = serve_provider(listener, None);
    let upstream = common::mock_in_thread(Options {
        emit: false,
        hold_listen: false,
        lifecycle: Lifecycle::Discover,
        ..Options::new("mock://res")
    });
    let (mcp, seen) = spawn_issuing_proxy(&upstream, Some("mock.ops"));
    let playbook = dir.path().join("playbook.json");
    std::fs::write(
        &playbook,
        json!({
            "turns": [
                {"tool_calls": [{"name": "mock.ops", "arguments": {}}]},
                {"tool_calls": [{"name": "subagent.run", "arguments": {
                    "instruction": "look something up", "mode": "sync"}}]},
                {"content": "delegated and done"}
            ],
            "match": [
                {"when_contains": "Result for zebra-child", "content": "child done"},
                {"when_contains": "You are agentd, an autonomous agent.",
                 "tool_calls": [{"name": "mock.search.query", "arguments": {"query": "zebra-child"}}]}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let llm = agentd::intel::mock::inprocess(&format!("file:{}", playbook.display())).unwrap();
    let (code, stderr) = run(
        dir.path(),
        &format!(
            "agent: {{name: tree, preflight: never}}\n\
             store: {{kind: memory}}\n\
             intelligence: {{endpoints: \"http://{llm}\", model: mock}}\n\
             mcp: {{servers: [{{name: mock, endpoint: \"{mcp}\"}}]}}\n\
             security: {{aauth: {{provider: \"{provider}\", key_file: \"{key_file}\"}}}}\n\
             workflows:\n\
             \x20 - name: work\n    steps:\n      s: {{kind: once}}\n      a: {{kind: agent, depends_on: [s], instruction: \"list and delegate\"}}\n      f: {{kind: finish, depends_on: [a]}}\n\
             lifecycle: {{run_until: idle, idle_grace: 500ms}}\n\
             observability: {{log_level: info}}\n"
        ),
    );
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let access = |r: &Seen| r.headers.get("authorization").cloned().unwrap_or_default();
    let from_turn = calls_to(&seen, "mock.ops");
    assert_eq!(from_turn.len(), 2, "issued, then retried:\n{stderr}");
    assert_eq!(access(&from_turn[0]), "", "the turn earned it");
    assert_eq!(access(&from_turn[1]), "AAuth acc-1");
    let from_subagent = calls_to(&seen, "search.query");
    assert_eq!(from_subagent.len(), 1, "{stderr}");
    assert_eq!(
        access(&from_subagent[0]),
        "AAuth acc-1",
        "the subagent was handed it"
    );
}

/// The links between a parent and its instance child are not signed, so
/// they do not wait on the provider: with it down, the child still reports
/// home, and `subagent.send` and `a2a.send` reach the child.
#[cfg(feature = "a2a")]
#[test]
fn the_trees_own_links_do_not_wait_on_the_provider() {
    let dir = tempfile::tempdir().unwrap();
    let (key_file, _) = write_key(dir.path());
    let provider = format!("http://127.0.0.1:{}", common::free_port());
    let socket = dir.path().join("parent.sock");
    let (code, stderr) = run(
        dir.path(),
        &format!(
            "agent: {{name: tree}}\n\
             store: {{kind: memory}}\n\
             intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
             a2a: {{listen: \"unix://{socket}\"}}\n\
             security: {{aauth: {{provider: \"{provider}\", key_file: \"{key_file}\"}}}}\n\
             subagents:\n\
             \x20 templates:\n\
             \x20   probe:\n\
             \x20     instruction: |\n\
             \x20       The probe.\n\
             \x20       :::!workflow{{name=probe}}\n\
             \x20       steps:\n\
             \x20         s: {{kind: once}}\n\
             \x20         f: {{kind: finish, depends_on: [s], status: completed, output: probed}}\n\
             \x20       :::\n\
             \x20     mode: sync\n\
             \x20     result: {{workflow: probe}}\n\
             \x20     ttl: 20s\n\
             workflows:\n\
             \x20 - name: spawner\n    steps:\n      s: {{kind: once}}\n      spawn: {{kind: subagent, template: probe, depends_on: [s], timeout: 15s}}\n      say: {{kind: tool, depends_on: [spawn], name: subagent.send, args: {{handle: \"{{{{steps.spawn.output.handle}}}}\", message: \"hello\"}}}}\n      tell: {{kind: a2a.send, depends_on: [say], to: \"{{{{steps.spawn.output.handle}}}}\", parts: \"hello again\"}}\n      f: {{kind: finish, depends_on: [tell], status: completed, output: \"{{{{steps.spawn.output.result.output}}}}\"}}\n\
             lifecycle: {{run_until: idle, idle_grace: 1500ms}}\n\
             observability: {{log_level: info}}\n",
            socket = socket.display()
        ),
    );
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert!(
        events(&stderr, "auth.unavailable")
            .iter()
            .any(|e| e["target"] == "aauth"),
        "the provider was down:\n{stderr}"
    );
    assert!(
        !events(&stderr, "instance.result").is_empty(),
        "the child reported home:\n{stderr}"
    );
    let sent = events(&stderr, "instance.send");
    assert!(
        !sent.is_empty() && sent.iter().all(|e| e["err"].is_null()),
        "subagent.send reached the child:\n{stderr}"
    );
}

/// A daemon that signs nothing past the instant its agent token stops being
/// good still holds one to hand the children it spawns then: it refreshes the
/// token ahead of that instant, in the background.
#[test]
fn an_idle_daemon_keeps_its_token_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let (key_file, _) = write_key(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = format!("http://{}", listener.local_addr().unwrap());
    // Good for two seconds, and due a refresh one second in.
    let ap = serve_provider_for(listener, None, 62);
    let (code, stderr) = run(
        dir.path(),
        &format!(
            "agent: {{name: idle}}\n\
             store: {{kind: memory}}\n\
             intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
             security: {{aauth: {{provider: \"{provider}\", key_file: \"{key_file}\"}}}}\n\
             workflows:\n\
             \x20 - name: nap\n    steps:\n      s: {{kind: once}}\n      n: {{kind: sleep, depends_on: [s], duration: 3500ms}}\n      f: {{kind: finish, depends_on: [n]}}\n\
             lifecycle: {{run_until: idle, idle_grace: 200ms}}\n"
        ),
    );
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let tokens = asked(&ap)
        .iter()
        .filter(|r| *r == "POST /agent-token")
        .count();
    assert!(tokens >= 3, "refreshed while idle: {:?}", asked(&ap));
    assert_eq!(
        asked(&ap)[..2],
        ["GET /.well-known/aauth-agent.json", "POST /enroll"],
        "verified and enrolled once"
    );
}

/// A daemon its parent composed signs with the key its parent named, and
/// never makes one: a key file that is gone refuses its start.
#[test]
fn an_instance_child_never_makes_a_key_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("agent.key");
    let cfg = dir.path().join("agentd.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{name: child}}\n\
             store: {{kind: memory}}\n\
             intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
             security: {{aauth: {{provider: \"http://127.0.0.1:{}\", key_file: \"{}\"}}}}\n\
             lifecycle: {{run_until: idle, idle_grace: 200ms}}\n",
            common::free_port(),
            key_file.display()
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env("AGENTD_STATE_DIR", dir.path().join("state"))
        .env(agentd::supervisor::reap::INSTANCE_CHILD_ENV, "1")
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(
        events(&stderr, "proc.exit")
            .iter()
            .any(|e| e["err"].as_str().is_some_and(|m| m.contains("read key"))),
        "stderr:\n{stderr}"
    );
    assert!(!key_file.exists(), "no key was made");
}
