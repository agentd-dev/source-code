// SPDX-License-Identifier: AGPL-3.0-only
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

use a2a_rs::domain::generated::{ListTasksResponse, StreamResponse};
use a2a_rs::domain::{AgentCard, Message, Task, TaskState};
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
    // `free_port` binds a port, closes it, and hands back the number — so
    // between that and the daemon binding, anything on the machine may take it.
    // With thirteen tests each starting a daemon in parallel, that is not
    // theoretical: it is the flake that made this job red intermittently, and
    // it presented as a `ConnectionRefused` several lines later with no clue
    // why. Retrying the whole boot is the honest fix; the port cannot be
    // reserved without holding it, and holding it is what stops the daemon
    // binding it.
    for attempt in 1..=4 {
        match try_boot() {
            Ok(v) => return v,
            Err(why) if attempt < 4 => {
                eprintln!("oracle: boot attempt {attempt} failed ({why}); retrying");
            }
            Err(why) => panic!("agentd would not start after 4 attempts: {why}"),
        }
    }
    unreachable!()
}

fn try_boot() -> Result<(Daemon, String, tempdir::TempDir), String> {
    let bin = agentd_bin();
    let tmp = tempdir::TempDir::new();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = tmp.path().join("agentd.yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: oracle\n  instruction: You are a test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n  push:\n    enabled: true\n    allow_private: true\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: warn\n"
        ),
    )
    .map_err(|e| format!("write config: {e}"))?;

    // Keep the daemon's stderr. It used to go to /dev/null, so when the daemon
    // failed to start the test reported `ConnectionRefused` from a request
    // several lines later and the actual reason — the port taken, the config
    // refused — was gone. A harness that discards the only explanation turns a
    // five-minute fix into an afternoon.
    let logp = tmp.path().join("agentd.log");
    let errf = std::fs::File::create(&logp).expect("create daemon log");
    let mut child = Command::new(bin)
        .args(["--config", cfg.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // A daemon that EXITED will never accept a connection, so waiting the
        // full twenty seconds for it is just a slower way to say the same
        // thing — badly. Check liveness first and report what it said.
        if let Ok(Some(status)) = child.try_wait() {
            let log = std::fs::read_to_string(&logp).unwrap_or_default();
            return Err(format!("exited before serving ({status}); stderr:\n{log}"));
        }
        if TcpStream::connect(&addr).is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let log = std::fs::read_to_string(&logp).unwrap_or_default();
            return Err(format!("listener never came up; stderr:\n{log}"));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok((Daemon(child), addr, tmp))
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

/// A plain GET, for the well-known discovery path.
fn fetch(addr: &str, path: &str) -> Value {
    let mut s = TcpStream::connect(addr).expect("connect a2a");
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
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
    serde_json::from_str(&b).unwrap_or_else(|e| panic!("non-JSON at {path} ({e}): {b:?}"))
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

/// A card that parses and names no endpoint is the quietest possible failure:
/// the peer reads it happily and has nowhere to send anything. Discovery is the
/// *point* of the card, so the interface is the field to assert.
#[test]
fn the_card_tells_a_peer_where_to_send_things() {
    let (_d, addr, _t) = boot();
    let card = result(&addr, 1, "GetAgentCard", json!({}));
    let theirs: AgentCard = cross_read("AgentCard", &card);

    let iface = theirs
        .supported_interfaces
        .first()
        .unwrap_or_else(|| panic!("the card names no interface, so a peer cannot dial it: {card}"));
    assert!(!iface.url.is_empty(), "an interface with no url: {card}");
    assert_eq!(
        iface.protocol_binding,
        a2a_rs::domain::PROTOCOL_BINDING_JSONRPC,
        "the binding a peer would choose: {card}"
    );

    // …and the same card is served unauthenticated at the well-known path,
    // which is how a peer finds it before it has credentials.
    let served = fetch(&addr, "/.well-known/agent-card.json");
    let published: AgentCard = cross_read("well-known AgentCard", &served);
    assert_eq!(published.name, theirs.name);
    assert_eq!(
        published.supported_interfaces.len(),
        theirs.supported_interfaces.len(),
        "the published card and the RPC card must be the same document"
    );
}

/// Every capability the card claims must be exercisable, and every one it
/// disclaims must be refused. A card is a promise a peer plans against.
#[test]
fn the_card_claims_exactly_what_the_server_does() {
    let (_d, addr, _t) = boot();
    let card = result(&addr, 1, "GetAgentCard", json!({}));
    let theirs: AgentCard = cross_read("AgentCard", &card);

    // Push notifications are off in this fixture, so the card must say so and
    // the method must refuse rather than half-serve.
    if !theirs
        .capabilities
        .as_option()
        .and_then(|c| c.push_notifications)
        .unwrap_or(false)
    {
        let sent = result(&addr, 2, "SendMessage", status_message("m1"));
        let task = sent.get("task").cloned().unwrap_or(sent);
        let id = task["id"].as_str().unwrap_or_default();
        let v = rpc(
            &addr,
            3,
            "CreateTaskPushNotificationConfig",
            json!({"taskId": id, "url": "https://hooks.example/x"}),
        );
        assert!(
            v["error"]["code"].as_i64().is_some(),
            "a disclaimed capability must be refused, not silently accepted: {v}"
        );
    }
}

/// Cancellation, which a peer reaches for when it changes its mind — and the
/// answer has to be a `Task`, not an acknowledgement.
#[test]
fn cancelling_answers_with_the_task_in_a_state_they_can_read() {
    let (_d, addr, _t) = boot();
    let sent = result(&addr, 1, "SendMessage", status_message("m1"));
    let task: Task = cross_read("Task", &sent.get("task").cloned().unwrap_or(sent));

    let v = rpc(&addr, 2, "CancelTask", json!({"id": task.id.as_str()}));
    match v.get("error") {
        // A finished task cannot be cancelled, and the spec has a code for
        // exactly that — which is a better answer than pretending.
        Some(e) => assert_eq!(
            e["code"], -32002,
            "an uncancelable task is TaskNotCancelable: {v}"
        ),
        None => {
            let back: Task = cross_read("CancelTask Task", &v["result"]);
            assert_eq!(back.id.as_str(), task.id.as_str());
        }
    }

    // And a task that does not exist is TaskNotFound either way.
    let v = rpc(&addr, 3, "CancelTask", json!({"id": "nope"}));
    assert_eq!(v["error"]["code"], -32001, "{v}");
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
    assert!(!theirs.id.is_empty(), "task has no id: {wire}");
    assert!(
        !theirs.context_id.is_empty(),
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
    // The *generated* response type, not the hand-written mirror: proto3 JSON
    // omits a field at its default value, so a strict struct with no defaults
    // would reject a legal empty page. This is the type a peer built from the
    // schema actually deserializes into.
    let theirs: ListTasksResponse = cross_read("ListTasksResponse", &listed);
    assert!(
        theirs.tasks.len() >= 2,
        "both tasks should be enumerable: {listed}"
    );
    for t in &theirs.tasks {
        assert!(!t.id.is_empty());
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

/// A stream a client can come back to. Each frame carries an event id, which is
/// what a client echoes as `Last-Event-ID` after a disconnect — without them a
/// dropped connection means starting over, and for a long task that means
/// missing the answer.
#[test]
fn stream_frames_carry_the_id_a_client_resumes_from() {
    let (_d, addr, _t) = boot();
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "SendStreamingMessage",
        "params": status_message("s1")
    })
    .to_string();
    let raw = post(&addr, &body);

    // A streaming method answers with a stream — whatever the message turned
    // out to contain. A caller that asked for SSE and received a JSON body
    // cannot parse it.
    let mut ids = 0usize;
    let mut frames = 0usize;
    for line in raw.lines() {
        if line.starts_with("id:") {
            ids += 1;
        }
        if line.starts_with("data:") {
            frames += 1;
        }
    }
    assert!(
        frames > 0,
        "a streaming send must answer as a stream: {raw}"
    );
    assert!(
        ids > 0,
        "no frame carried an event id, so a disconnected client cannot resume: {raw}"
    );
}

/// The push-notification surface, read back through their types. A caller
/// registers a webhook with a `TaskPushNotificationConfig`; what comes back has
/// to be one.
#[test]
fn a_push_config_round_trips_as_their_type() {
    use a2a_rs::domain::TaskPushNotificationConfig;

    let (_d, addr, _t) = boot();
    let sent = result(&addr, 1, "SendMessage", status_message("m1"));
    let task: Task = cross_read("Task", &sent.get("task").cloned().unwrap_or(sent));

    let created = result(
        &addr,
        2,
        "CreateTaskPushNotificationConfig",
        // The request *is* the config, flat — as the spec's message defines it.
        json!({"taskId": task.id.as_str(), "url": "https://127.0.0.1:9/hook", "token": "t"}),
    );
    let theirs: TaskPushNotificationConfig = cross_read("TaskPushNotificationConfig", &created);
    assert_eq!(theirs.task_id, task.id.as_str());
    assert!(!theirs.url.is_empty());

    let listed = result(
        &addr,
        3,
        "ListTaskPushNotificationConfigs",
        json!({"taskId": task.id.as_str()}),
    );
    let configs = listed["configs"]
        .as_array()
        .unwrap_or_else(|| panic!("a listing carries its configs: {listed}"));
    assert_eq!(configs.len(), 1, "{listed}");
    let _: TaskPushNotificationConfig = cross_read("listed config", &configs[0]);
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
        // `SubscribeToEvents` is agentd's own interface feed for display clients, declared
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

    let v = rpc(&addr, 1, "GetTask", json!({"id": "does-not-exist"}));
    assert_eq!(
        v["error"]["code"],
        a2a_rs::domain::error::TASK_NOT_FOUND,
        "an unknown task must be TaskNotFound, not a generic failure: {v}"
    );

    let v = rpc(&addr, 2, "NoSuchMethod", json!({}));
    assert_eq!(
        v["error"]["code"], -32601,
        "an unknown method is JSON-RPC MethodNotFound: {v}"
    );
}
