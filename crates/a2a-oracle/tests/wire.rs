// SPDX-License-Identifier: Apache-2.0
//! **A second reader for the A2A specification.**
//!
//! agentd's A2A server is hand-written, and the risk that carries is not a loud
//! failure — the conformance suite covers those — but a *plausible* misreading:
//! a field named the way we assumed, a state spelled the way we guessed. Our own
//! tests cannot catch it, because they were written from the same reading of the
//! spec as the implementation. A peer built from the schema would simply fail to
//! parse us, in production, with no useful error.
//!
//! So this boots the real `agentd` binary, drives its real A2A listener over
//! JSON-RPC, and hands every response to [`a2a_rs`] — an unrelated Rust
//! implementation of the same specification, by a different author, with its own
//! types. Deserialization succeeding means two independent readings of the spec
//! agree on what went over the wire. Deserialization failing means one of us is
//! wrong, and the error says which field.
//!
//! Run it deliberately:
//! `cargo test --manifest-path crates/a2a-oracle/Cargo.toml`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use a2a_rs::domain::generated::StreamResponse;
use a2a_rs::domain::{AgentCard, ListTasksResult, Message, Task, TaskState};
use serde_json::{Value, json};

// ── booting the real thing ───────────────────────────────────────────────────

/// The `target/<profile>/` dir, derived from our own executable's location
/// (`.../target/<profile>/deps/<exe>`).
fn target_dir() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p
}

/// Build and locate the real binary, once for the whole test binary — every
/// test needs it and `cargo build` serializes on a lock anyway.
///
/// Two deliberate details. `a2a` is not a default feature, so it has to be
/// asked for: without it the listener simply never binds. And the build goes to
/// a target dir of its own — the main workspace's `target/debug/agentd` is
/// driven by the conformance suite and the CLI tests with a *different* feature
/// set, and whichever ran last would win.
fn agentd_bin() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../Cargo.toml")
            .canonicalize()
            .expect("locate the workspace manifest");
        let out = target_dir().join("agentd-under-test");
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "agentd-cli", "--features", "a2a"])
            .arg("--manifest-path")
            .arg(&workspace)
            .env("CARGO_TARGET_DIR", &out)
            .status()
            .expect("run cargo build");
        assert!(status.success(), "building agentd failed");
        let bin = out.join("debug/agentd");
        assert!(bin.exists(), "no agentd binary at {}", bin.display());
        bin
    })
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// A daemon that dies with the test, however the test ends.
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Boot agentd with a real A2A listener on loopback and wait for it to accept.
///
/// Plaintext loopback resolves to the `operator` principal, so the whole surface
/// is reachable without cert plumbing. `preflight: never` means the unreachable
/// intelligence endpoint is never dialled — every method exercised here is
/// answered by the protocol layer, not by a model.
fn boot() -> (Daemon, String, tempdir::TempDir) {
    let bin = agentd_bin();
    let tmp = tempdir::TempDir::new();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = tmp.path().join("agentd.yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"2\"\n\
             agent:\n  name: oracle\n  instruction: You are a test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: warn\n"
        ),
    )
    .expect("write config");

    let child = Command::new(bin)
        .args(["--config", cfg.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agentd");

    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(&addr).is_err() {
        assert!(Instant::now() < deadline, "a2a listener never came up");
        std::thread::sleep(Duration::from_millis(25));
    }
    (Daemon(child), addr, tmp)
}

/// A throwaway directory that cleans up after itself (no dev-dependency needed
/// for something this small).
mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    pub struct TempDir(PathBuf);
    impl TempDir {
        #[allow(clippy::new_without_default)]
        pub fn new() -> TempDir {
            let n = N.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("a2a-oracle-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).expect("mkdir temp");
            TempDir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

// ── driving it ───────────────────────────────────────────────────────────────

fn post(addr: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect a2a");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    loop {
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        if l.trim().is_empty() {
            break;
        }
    }
    let mut b = String::new();
    r.read_to_string(&mut b).unwrap();
    b
}

/// One JSON-RPC call; returns the whole envelope so a test can read either half.
fn rpc(addr: &str, id: i64, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let raw = post(addr, &body);
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("non-JSON A2A response ({e}): {raw:?}"))
}

fn result(addr: &str, id: i64, method: &str, params: Value) -> Value {
    let v = rpc(addr, id, method, params);
    assert!(v.get("error").is_none(), "{method} errored: {v}");
    v["result"].clone()
}

/// Parse with the other implementation, reporting the offending JSON on failure —
/// the error message *is* the deliverable when these fail.
fn cross_read<T: serde::de::DeserializeOwned>(what: &str, v: &Value) -> T {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| {
        panic!(
            "agentd's {what} does not satisfy an independent reading of the A2A spec.\n\
             a2a-rs says: {e}\n\
             what agentd sent:\n{}",
            serde_json::to_string_pretty(v).unwrap_or_default()
        )
    })
}

/// A DataPart command that completes without a model turn — so a task with a
/// real terminal status exists to inspect.
fn status_message(id: &str) -> Value {
    json!({"message": {"messageId": id, "role": "user",
           "parts": [{"data": {"agentd": {"op": "status"}}}]}})
}

// ── the checks ───────────────────────────────────────────────────────────────

#[test]
fn the_agent_card_parses_as_an_agent_card() {
    let (_d, addr, _t) = boot();
    let card = result(&addr, 1, "GetAgentCard", json!({}));
    let theirs: AgentCard = cross_read("AgentCard", &card);

    // The fields a peer actually routes on.
    assert!(!theirs.name.is_empty(), "card has no name: {card}");
    assert!(
        !theirs.version.is_empty(),
        "a peer pins behaviour to the version: {card}"
    );
    assert!(
        theirs.capabilities.streaming.unwrap_or(false),
        "agentd streams; the card must say so: {card}"
    );
}

#[test]
fn a_task_we_return_parses_as_a_task() {
    let (_d, addr, _t) = boot();
    let sent = result(&addr, 1, "SendMessage", status_message("m1"));
    let wire = if sent.get("task").is_some() {
        sent["task"].clone()
    } else {
        sent.clone()
    };
    let theirs: Task = cross_read("Task", &wire);

    assert_eq!(
        theirs.status.state,
        TaskState::TASK_STATE_COMPLETED,
        "a status command is terminal: {wire}"
    );
    assert!(!theirs.id.as_str().is_empty(), "task has no id: {wire}");
    assert!(
        !theirs.context_id.as_str().is_empty(),
        "a task must carry the context that groups it: {wire}"
    );

    // …and the same task read back through GetTask, which is a different code
    // path in agentd (durable store → wire) and could drift from SendMessage's.
    let got = result(&addr, 2, "GetTask", json!({"id": theirs.id.as_str()}));
    let reread: Task = cross_read("GetTask Task", &got);
    assert_eq!(reread.id.as_str(), theirs.id.as_str());
    assert_eq!(reread.status.state, theirs.status.state);
}

#[test]
fn the_task_list_parses_as_a_task_list() {
    let (_d, addr, _t) = boot();
    result(&addr, 1, "SendMessage", status_message("m1"));
    result(&addr, 2, "SendMessage", status_message("m2"));

    let listed = result(&addr, 3, "ListTasks", json!({}));
    let theirs: ListTasksResult = cross_read("ListTasksResult", &listed);
    assert!(
        theirs.tasks.len() >= 2,
        "both tasks should be enumerable: {listed}"
    );
    for t in &theirs.tasks {
        assert!(!t.id.as_str().is_empty());
    }
}

#[test]
fn the_message_we_accept_is_the_message_their_client_sends() {
    // The inbound direction. a2a-rs's own client serializes a Message this way;
    // if agentd cannot take it, no peer built on that crate can talk to us.
    let (_d, addr, _t) = boot();
    let theirs = Message::user_text("say hello".to_string(), "m-inbound".to_string());
    let wire = serde_json::to_value(&theirs).expect("serialize their Message");

    let v = rpc(&addr, 1, "SendMessage", json!({"message": wire.clone()}));
    assert!(
        v.get("error").is_none(),
        "agentd rejected a message produced by an independent A2A client: {v}\n\
         the message was:\n{}",
        serde_json::to_string_pretty(&wire).unwrap_or_default()
    );
}

#[test]
fn our_terminal_statuses_are_spelled_the_way_the_spec_spells_them() {
    // The likeliest place to drift, and the quietest when it does: a state a
    // peer cannot parse is an interop failure with no error message. Checked
    // against their enum rather than our own constants.
    for wire in [
        "TASK_STATE_SUBMITTED",
        "TASK_STATE_WORKING",
        "TASK_STATE_INPUT_REQUIRED",
        "TASK_STATE_COMPLETED",
        "TASK_STATE_FAILED",
        "TASK_STATE_CANCELED",
        "TASK_STATE_REJECTED",
        "TASK_STATE_AUTH_REQUIRED",
    ] {
        let parsed: TaskState = serde_json::from_value(json!(wire))
            .unwrap_or_else(|e| panic!("agentd emits {wire:?}, which they cannot read: {e}"));
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            json!(wire),
            "{wire} does not round-trip"
        );
    }
}

#[test]
fn the_frames_we_stream_parse_as_the_events_they_claim_to_be() {
    // Streaming builds its frames independently of the `Task` projection, so it
    // is the half most likely to drift — and the half a peer consumes without
    // ever calling GetTask.
    let (_d, addr, _t) = boot();
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "SendStreamingMessage",
        "params": status_message("s1")
    })
    .to_string();
    let raw = post(&addr, &body);

    // The payload is a proto `oneof` — `statusUpdate` / `artifactUpdate` /
    // `task` / `message` — so parsing the whole envelope checks the union tag
    // and the event body in one go.
    let mut frames = 0usize;
    for line in raw.lines() {
        let payload = line.strip_prefix("data:").unwrap_or(line).trim();
        if !payload.starts_with('{') {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        let result = &frame["result"];
        if !result.is_object() {
            continue;
        }
        let theirs: StreamResponse = cross_read("StreamResponse", result);
        assert!(
            theirs.payload.is_some(),
            "a stream frame carried no recognisable payload: {result}"
        );
        frames += 1;
    }
    assert!(
        frames > 0,
        "a streaming send should emit update frames: {raw}"
    );
}

#[test]
fn every_method_we_answer_is_spelled_the_way_the_spec_spells_it() {
    // Their JSON-RPC binding is the vocabulary a peer's client will call. Ours
    // must be a subset of it, spelled identically — a method we invented, or
    // one we misspelled, is unreachable and silently so.
    use a2a_rs::adapter::transport::jsonrpc_wire::methods as m;
    let theirs = [
        m::SEND_MESSAGE,
        m::SEND_STREAMING_MESSAGE,
        m::GET_TASK,
        m::LIST_TASKS,
        m::CANCEL_TASK,
        m::SUBSCRIBE_TO_TASK,
        m::CREATE_PUSH_CONFIG,
        m::GET_PUSH_CONFIG,
        m::LIST_PUSH_CONFIGS,
        m::DELETE_PUSH_CONFIG,
        m::GET_EXTENDED_AGENT_CARD,
    ];
    for name in agentd::runtime::a2a_server::METHODS {
        // `SubscribeToEvents` is agentd's own RFC 0032 interface feed, declared
        // as an extension rather than claimed as A2A.
        if *name == "SubscribeToEvents" {
            continue;
        }
        assert!(
            theirs.contains(name),
            "agentd answers {name:?}, which is not an A2A method: {theirs:?}"
        );
    }
}

#[test]
fn the_error_codes_peers_branch_on_are_the_specified_ones() {
    let (_d, addr, _t) = boot();

    // Independently sourced: their constants, not ours.
    assert_eq!(a2a_rs::domain::error::TASK_NOT_FOUND, -32001);
    assert_eq!(a2a_rs::domain::error::UNSUPPORTED_OPERATION, -32004);

    let v = rpc(&addr, 1, "GetTask", json!({"name": "tasks/does-not-exist"}));
    assert_eq!(
        v["error"]["code"], a2a_rs::domain::error::TASK_NOT_FOUND,
        "an unknown task must be TaskNotFound, not a generic failure: {v}"
    );

    let v = rpc(&addr, 2, "NoSuchMethod", json!({}));
    assert_eq!(
        v["error"]["code"], -32601,
        "an unknown method is JSON-RPC MethodNotFound: {v}"
    );
}
