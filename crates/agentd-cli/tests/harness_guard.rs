// SPDX-License-Identifier: AGPL-3.0-only
//! **The test clients speak the specification, and only through the harness.**
//!
//! A suite that talks to the listener in shapes no real client sends proves
//! nothing about the clients that exist: it stays green while the server
//! accepts a spelling the spec never had, and it stays green when the server
//! stops accepting the spelling the spec does have. So the request shape lives
//! in one place — `common/mod.rs` for the e2e suites, `checks/util.rs` for the
//! conformance suite — and this file holds both ends of that:
//!
//! * the helpers are driven against a local capture server, which records what
//!   actually went over the wire: the content type and `A2A-Version` on every
//!   request, `ROLE_USER` and an explicit `returnImmediately` on every message,
//!   and a command marked as its extension in the message AND the header;
//! * the sources are scanned for request shapes built anywhere outside the two
//!   harness files — `blocking`, which is not an A2A 1.0 field, and a command
//!   DataPart typed out by hand.
//!
//! This file lives apart from `common/mod.rs` because that module is compiled
//! into every test binary, and these checks should run once, not once per binary.
//! It is excluded from its own scan: it has to name what it forbids.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::runtime::surface::COMMAND_EXTENSION;
use common::SendMessage;
use serde_json::{Value, json};

/// One request as the capture server received it.
#[derive(Debug, Clone)]
struct Captured {
    method: String,
    path: String,
    /// Names lowercased.
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("captured body is not JSON ({e}): {:?}", self.body))
    }
}

/// A listener that records every request and answers each with a JSON-RPC
/// success — or an error, for a request whose method is `Fail`, so the helpers
/// that insist on an error can be driven too.
struct Capture {
    addr: String,
    seen: Arc<Mutex<Vec<Captured>>>,
}

impl Capture {
    fn start() -> Capture {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind capture server");
        let addr = listener.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                if let Some(req) = read_request(conn.try_clone().unwrap()) {
                    let fail = serde_json::from_str::<Value>(&req.body)
                        .is_ok_and(|v| v["method"] == "Fail");
                    sink.lock().unwrap().push(req);
                    answer(conn, fail);
                }
            }
        });
        Capture { addr, seen }
    }

    /// Everything captured, once `n` requests have arrived.
    fn wait_for(&self, n: usize) -> Vec<Captured> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self.seen.lock().unwrap().clone();
            if seen.len() >= n {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "only {} of {n} requests arrived: {seen:#?}",
                seen.len()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The single request a helper just sent (the capture is cleared first).
    fn one(&self, send: impl FnOnce(&str)) -> Captured {
        self.seen.lock().unwrap().clear();
        send(&self.addr);
        let mut all = self.wait_for(1);
        assert_eq!(all.len(), 1, "one call, one request: {all:#?}");
        all.remove(0)
    }
}

fn read_request(s: TcpStream) -> Option<Captured> {
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut l = String::new();
        if r.read_line(&mut l).ok()? == 0 || l.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some(Captured {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn answer(mut s: TcpStream, fail: bool) {
    let body = if fail {
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32000, "message": "asked to"}})
    } else {
        json!({"jsonrpc": "2.0", "id": 1, "result": {"name": "agentd"}})
    }
    .to_string();
    let _ = write!(
        s,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Every POST helper sends `Content-Type: application/json` and the
/// `A2A-Version` the spec defines. Each helper is driven on its own, so a
/// helper that grew its own request writer — and left a header behind — is
/// named by the failure.
#[test]
fn helpers_send_version_and_content_type() {
    let cap = Capture::start();
    let feed = Arc::new(Mutex::new(None));

    type Call = Box<dyn Fn(&str)>;
    let hold = Arc::clone(&feed);
    let calls: Vec<(&str, Call)> = vec![
        (
            "a2a_post",
            Box::new(|a| drop(common::a2a_post(a, "{}", &[]))),
        ),
        (
            "try_a2a_post",
            Box::new(|a| drop(common::try_a2a_post(a, "{}", &[]).unwrap())),
        ),
        (
            "a2a_post_within",
            Box::new(|a| {
                drop(common::a2a_post_within(
                    a,
                    "{}",
                    &[],
                    Duration::from_secs(5),
                ))
            }),
        ),
        (
            "rpc",
            Box::new(|a| drop(common::rpc(a, 1, "GetTask", json!({})))),
        ),
        (
            "rpc_as",
            Box::new(|a| drop(common::rpc_as(a, "t", 1, "GetTask", json!({})))),
        ),
        (
            "rpc_result",
            Box::new(|a| drop(common::rpc_result(a, 1, "GetTask", json!({})))),
        ),
        (
            "rpc_error",
            Box::new(|a| drop(common::rpc_error(a, 1, "Fail", json!({})))),
        ),
        ("send_text", Box::new(|a| drop(common::send_text(a, "hi")))),
        (
            "send_command",
            Box::new(|a| drop(common::send_command(a, "status", json!({})))),
        ),
        (
            "SendMessage::result",
            Box::new(|a| drop(SendMessage::text("hi").task("t").result(a))),
        ),
        (
            "SendMessage::try_post_raw (streaming)",
            Box::new(|a| {
                drop(
                    SendMessage::command("status", json!({}))
                        .streaming()
                        .try_post_raw(a)
                        .unwrap(),
                )
            }),
        ),
        (
            "subscribe_feed",
            Box::new(move |a| {
                // The stream is only opened, never read: keep it alive until
                // the capture has seen the request.
                *hold.lock().unwrap() = Some(common::subscribe_feed(a, 0, Duration::from_secs(5)));
            }),
        ),
    ];

    for (name, call) in &calls {
        let req = cap.one(call);
        assert_eq!(
            (req.method.as_str(), req.path.as_str()),
            ("POST", "/"),
            "{name}: {req:?}"
        );
        assert_eq!(
            req.header("content-type"),
            Some("application/json"),
            "{name} must send the JSON content type: {req:?}"
        );
        assert_eq!(
            req.header("a2a-version"),
            Some(common::A2A_VERSION),
            "{name} must state the protocol version: {req:?}"
        );
    }
    assert_eq!(common::A2A_VERSION, "1.0");

    // The card is not a JSON-RPC method: it is a GET of the well-known path.
    let req = cap.one(|a| drop(common::get_card(a)));
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("GET", "/.well-known/agent-card.json"),
        "{req:?}"
    );
}

/// What a message says about itself: `ROLE_USER` and an explicit
/// `returnImmediately` on every one, and a command marked as the extension it
/// belongs to — listed in the message and activated by the header, the two
/// halves a server can check independently.
#[test]
fn command_messages_are_marked() {
    let cap = Capture::start();

    let req = cap.one(|a| drop(common::send_command(a, "status", json!({"run": "r-1"}))));
    let body = req.json();
    let message = &body["params"]["message"];
    assert_eq!(body["method"], "SendMessage", "{body}");
    assert_eq!(message["role"], "ROLE_USER", "{body}");
    assert_eq!(
        message["extensions"],
        json!([COMMAND_EXTENSION]),
        "a command lists its extension: {body}"
    );
    assert_eq!(
        req.header("a2a-extensions"),
        Some(COMMAND_EXTENSION),
        "a command activates its extension: {req:?}"
    );
    assert_eq!(
        message["parts"],
        json!([{"data": {"agentd": {"op": "status", "run": "r-1"}}}]),
        "the op and its args ride one DataPart: {body}"
    );
    assert_eq!(
        body["params"]["configuration"]["returnImmediately"], false,
        "{body}"
    );
    assert!(
        message["taskId"].is_null(),
        "a command names no task: {body}"
    );
    assert!(
        message["messageId"].as_str().is_some_and(|m| !m.is_empty()),
        "{body}"
    );

    // Natural language is not a command: no mark, no header — activating an
    // extension the request does not use would be a claim it is not making.
    let req = cap.one(|a| drop(common::send_text(a, "hello")));
    let body = req.json();
    let message = &body["params"]["message"];
    assert_eq!(message["role"], "ROLE_USER", "{body}");
    assert_eq!(message["parts"], json!([{"text": "hello"}]), "{body}");
    assert!(message.get("extensions").is_none(), "{body}");
    assert_eq!(req.header("a2a-extensions"), None, "{req:?}");
    assert_eq!(
        body["params"]["configuration"]["returnImmediately"], false,
        "the default is stated, never left to the server: {body}"
    );

    // The knobs land where the spec puts them, and the bearer in its header.
    let req = cap.one(|a| {
        drop(
            SendMessage::text("reply")
                .task("t-9")
                .context("c-9")
                .return_immediately()
                .bearer("tok")
                .result(a),
        )
    });
    let body = req.json();
    assert_eq!(body["params"]["message"]["taskId"], "t-9", "{body}");
    assert_eq!(body["params"]["message"]["contextId"], "c-9", "{body}");
    assert_eq!(
        body["params"]["configuration"]["returnImmediately"], true,
        "{body}"
    );
    assert_eq!(req.header("authorization"), Some("Bearer tok"), "{req:?}");

    // No two messages share an id.
    let a = cap.one(|a| drop(common::send_text(a, "one"))).json();
    let b = cap.one(|a| drop(common::send_text(a, "two"))).json();
    assert_ne!(
        a["params"]["message"]["messageId"], b["params"]["message"]["messageId"],
        "a message id names ONE message"
    );
}

/// Every `.rs` file under `dir`, recursively.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Request shapes built outside the harness.
///
/// * `"blocking"` — not an A2A 1.0 field; the spec's is `returnImmediately`,
///   its inverse;
/// * a command DataPart typed out by hand — it would carry neither the
///   extension mark nor the activation header, so it only works against a
///   server that checks neither. Only the two harness files may build one. The
///   `"agentd":` key is refused whatever follows it: `op` need not come first,
///   and the value may be a variable.
#[test]
fn only_the_harness_builds_requests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = root.join("tests");
    let conformance = root.join("../agentd-conformance/src");
    let harnesses = [
        tests.join("common/mod.rs"),
        conformance.join("checks/util.rs"),
    ];
    let mut files = Vec::new();
    sources(&tests, &mut files);
    sources(&conformance, &mut files);
    assert!(
        files.len() > 50,
        "the scan must actually see the suites: {files:?}"
    );

    let mut found = Vec::new();
    for file in files {
        if file.file_name().is_some_and(|n| n == "harness_guard.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        // Whitespace is not significant in either JSON or a `json!` literal,
        // so it is removed before matching: `"agentd": {"op"` and
        // `"agentd":{"op"` are the same shape.
        let flat: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let name = file.display().to_string();
        if text.contains("\"blocking\"") {
            found.push(format!(
                "{name}: \"blocking\" — say `returnImmediately` (SendMessage::return_immediately)"
            ));
        }
        let is_harness = harnesses
            .iter()
            .any(|h| h.canonicalize().ok() == file.canonicalize().ok());
        if !is_harness && flat.contains("\"agentd\":") {
            found.push(format!(
                "{name}: a hand-built command DataPart — use SendMessage::command"
            ));
        }
    }
    assert!(
        found.is_empty(),
        "request shapes built outside the harness:\n{}",
        found.join("\n")
    );
}

/// A daemon killed when the test lets go of it.
#[cfg(all(unix, feature = "a2a"))]
struct Spawned(std::process::Child, String);
#[cfg(all(unix, feature = "a2a"))]
impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
    }
}

/// `common::spawn_bound` hands a test the authority its daemon's own
/// `a2a.listen` line names, and retries a daemon whose bind lost the race —
/// never the probed port. Forced here: the first attempt is aimed at a port
/// this test is holding, so that daemon exits on its bind, and a helper that
/// trusted the probe would hand the test this test's own socket as the agent.
#[test]
#[cfg(all(unix, feature = "a2a"))]
fn spawn_bound_retries_a_lost_bind_and_reports_the_daemons_own_authority() {
    let squatter = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap().port();
    let mut attempts = 0;
    let (_daemon, addr) = common::spawn_bound(|port| {
        attempts += 1;
        let port = if attempts == 1 { taken } else { port };
        let cfg = common::unique_path("spawn-bound", "yaml");
        std::fs::write(
            &cfg,
            format!(
                "agent: {{name: spawn-bound, preflight: never}}\n\
                 intelligence: {{endpoints: \"https://127.0.0.1:9\", model: mock}}\n\
                 store: {{kind: memory}}\n\
                 a2a: {{listen: \"http://127.0.0.1:{port}\"}}\n\
                 lifecycle: {{run_until: drained}}\n\
                 observability: {{log_level: info}}\n"
            ),
        )
        .unwrap();
        let log = common::unique_path("spawn-bound", "log");
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("spawn agentd");
        (Spawned(child, log.clone()), log)
    });
    assert_eq!(attempts, 2, "the lost bind was retried once");
    assert_ne!(
        addr,
        format!("127.0.0.1:{taken}"),
        "the test was handed the squatter's port"
    );
    assert_eq!(common::get_card(&addr)["name"], "spawn-bound");
    drop(squatter);
}

/// A no-credential loopback daemon logs a `config.warning` that starts with
/// `a2a.listen:` BEFORE it binds. Reading that text as a failed bind made a
/// healthy daemon look dead whenever the poll landed between the two lines,
/// so the bound authority is waited for until the daemon actually exits.
#[test]
fn a_listen_warning_before_the_bind_is_not_a_failed_bind() {
    let log = common::unique_path("try-bound", "log");
    std::fs::write(
        &log,
        "{\"event\":\"config.warning\",\"warning\":\"a2a.listen: no credential is configured\"}\n",
    )
    .unwrap();
    let writer = {
        let log = log.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            writeln!(
                f,
                "{{\"event\":\"a2a.listen\",\"bound\":\"127.0.0.1:4242\"}}"
            )
            .unwrap();
        })
    };
    let bound = common::try_a2a_bound(&log, Duration::from_secs(10));
    writer.join().unwrap();
    std::fs::remove_file(&log).ok();
    assert_eq!(bound.as_deref(), Some("127.0.0.1:4242"));
}
