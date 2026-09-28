// SPDX-License-Identifier: AGPL-3.0-only
//! **A task belongs to a principal, and so does its event stream.**
//!
//! Two failure modes, one surface — the A2A listener's authorization — and both
//! are only visible from outside the process, which is why they are tested here
//! against the real binary rather than in a unit.
//!
//! ## Ownership does not stop at the task read
//!
//! Every task-facing port asks the reactor, and the reactor answers with the
//! ownership matrix already applied: another principal's task is "not found",
//! so its existence is not even disclosed. The *streaming* port must apply it
//! too — a2a-rs's fan-out is keyed by task id alone, so without an ownership
//! check naming somebody else's task id is enough to attach to it, and with a
//! `Last-Event-ID` to replay what it has already emitted: every transition, and
//! the result artifact that carries the agent's answer.
//!
//! The test therefore drives two *different* principals through the real
//! listener. The first assertion is that the subscribe is refused with the same
//! "not found" a read gets — a non-owner must not be able to tell the
//! difference between "not yours" and "does not exist". The second is the one
//! that matters: nothing is delivered. A refusal that still opened the stream
//! would pass a status-code check and leak the events anyway.
//!
//! ## A method name is remote input, and must never be leaked
//!
//! `principals::bare` must not lowercase the JSON-RPC `method` and `leak()` the
//! copy to hand back a `&'static str`. The name is attacker-chosen and
//! unbounded in length. One leak per request is an RSS climb driven with a
//! `curl` loop, so the flood test asserts the daemon's own RSS, because a leak
//! has no other observable: the requests all succeed (as errors), and only the
//! memory behind them differs.
//!
//! ## Who you are is a 401; what you may do is a 403
//!
//! A caller the listener cannot name gets HTTP 401 with a `WWW-Authenticate`
//! challenge before a byte of its body is parsed — a junk bearer included,
//! which once resolved to "anonymous" and ran the whole dispatch to be refused
//! with the spec's push-notification code. A named caller refused a method or a
//! command gets HTTP 403. Both are JSON, whatever the method promised.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::BufRead;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{SendMessage, a2a_open, a2a_post, a2a_post_within, rpc, rpc_as, rpc_body};

/// The bearers the two principals present. Literal here, `{{secret:…}}` in the
/// config — a bearer is a secret, and the config may only carry a reference.
const TOKEN_A: &str = "authz-token-for-principal-a";
const TOKEN_B: &str = "authz-token-for-principal-b";
const TOKEN_C: &str = "authz-token-for-principal-c";
const TOKEN_OP: &str = "authz-token-for-the-operator";

fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// A free loopback port (bind :0, read the port, drop). A tiny TOCTOU window —
/// agentd rebinds within milliseconds.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_ready(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a2a listener never became connectable"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct MockLlm {
    child: Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}
fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("authz-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("authz-mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    fn pid(&self) -> u32 {
        self.child.id()
    }
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        sigterm(self.child.id());
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

/// Spawn the daemon, handing it the two bearers through the environment — the
/// config names them by reference, so the secrets never touch a file.
fn spawn_daemon(config: &str) -> Daemon {
    let stderr_path = common::unique_path("a2a-authz-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .env("AGENTD_AUTHZ_TOKEN_A", TOKEN_A)
        .env("AGENTD_AUTHZ_TOKEN_B", TOKEN_B)
        .env("AGENTD_AUTHZ_TOKEN_C", TOKEN_C)
        .env("AGENTD_AUTHZ_TOKEN_OP", TOKEN_OP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd a2a daemon");
    Daemon { child, stderr_path }
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("a2a-authz", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

/// Two principals on one listener.
///
/// They differ by role as well as by bearer because a principal's *id* — the
/// thing a task records as its owner — is derived from the caller's identity,
/// and a plaintext bearer contributes none: two `user` rules would both resolve
/// to the same id and would not be two principals at all (see the note in the
/// summary). `user` and `agent` are two ids, and both roles may send, read and
/// subscribe, which is exactly the surface under test.
fn two_principal_config(llm: &str, port: u16) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: a2a-authz\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         \x20 principals:\n\
         \x20   - id: token-a\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_A}}}}\" }}\n\
         \x20     role: user\n\
         \x20   - id: token-b\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_B}}}}\" }}\n\
         \x20     role: agent\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

#[test]
fn one_principals_task_stream_is_not_readable_by_another() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "the private answer"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&two_principal_config(&llm.uri, port));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    // A starts a task and it settles, so the fan-out's replay buffer holds A's
    // transitions and its result artifact — the material a replay would leak.
    let send = SendMessage::text("hello").bearer(TOKEN_A).post(&addr);
    assert!(
        send.get("error").is_none(),
        "A's send should succeed: {send}"
    );
    let task = &send["result"]["task"];
    let task_id = task["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no task id in {send}"))
        .to_string();
    assert_eq!(
        task["status"]["state"], "TASK_STATE_COMPLETED",
        "A's task settled: {task}"
    );

    // The ports that already asked the reactor: not yours reads as not found,
    // so B cannot even confirm the id exists.
    let got = rpc_as(&addr, TOKEN_B, 2, "GetTask", json!({"id": task_id}));
    assert_eq!(got["error"]["code"], -32001, "B's GetTask: {got}");
    let cancelled = rpc_as(&addr, TOKEN_B, 3, "CancelTask", json!({"id": task_id}));
    assert_eq!(
        cancelled["error"]["code"], -32001,
        "B's cancel: {cancelled}"
    );

    // The surface under test. `Last-Event-ID: 0` asks for everything the task
    // has ever emitted, which is what makes this deterministic: a live
    // subscription would depend on catching a transition, but a replay is owed
    // the whole buffer the moment it attaches.
    // `a2a_post_within` bounds the read rather than the connection: a refused
    // subscribe closes at once, but an *unguarded* one answers with an SSE
    // stream that stays open, and the test has to be able to say what it
    // received from a stream that never ends.
    let subscribe = rpc_body(4, "SubscribeToTask", json!({"id": task_id}));
    let auth_b = format!("Bearer {TOKEN_B}");
    let reply = a2a_post_within(
        &addr,
        &subscribe,
        &[("Authorization", &auth_b), ("Last-Event-ID", "0")],
        Duration::from_secs(10),
    );
    let (status, body) = (reply.status, reply.body.as_str());

    // Nothing was delivered. This is the assertion that matters: a refusal that
    // still opened the stream would satisfy every other check here and hand B
    // the events anyway.
    assert!(
        !body.contains("data:"),
        "B received stream frames for A's task: {status} / {body}"
    );
    assert!(
        !body.contains(task_id.as_str()) || body.contains("error"),
        "B's response carried A's task id outside an error: {body}"
    );
    assert!(
        !body.contains("the private answer"),
        "B replayed A's result artifact: {body}"
    );

    // And it was refused as "not found" rather than "forbidden", so B cannot
    // learn from the refusal that the task is real.
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.contains("application/json")),
        "a refused subscribe answers with an error, not a stream: {reply:?}"
    );
    let refused: Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("non-JSON refusal ({e}): {status} {body:?}"));
    assert_eq!(
        refused["error"]["code"], -32001,
        "B's subscribe is refused as not-found: {refused}"
    );

    // The control: the same call, from the owner, does deliver. Without it a
    // subscribe that was broken for everybody would pass the assertions above.
    let auth_a = format!("Bearer {TOKEN_A}");
    let owner = a2a_post_within(
        &addr,
        &rpc_body(5, "SubscribeToTask", json!({"id": task_id})),
        &[("Authorization", &auth_a), ("Last-Event-ID", "0")],
        Duration::from_secs(10),
    );
    let (a_status, a_body) = (owner.status, owner.body);
    assert!(
        a_body.contains("data:") && a_body.contains(&task_id),
        "the owner still receives its own task's events: {a_status} / {a_body}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// A send that NAMES another principal's live task does not attach its caller
/// to that task's stream.
///
/// a2a-rs 0.10 reads the task a send names, then attaches the send's
/// subscription to that id, before the message is processed. Inside a send a
/// read that finds nothing is deliberately not a verdict (the send's own task
/// does not exist yet), so what keeps B off A's broadcast channel is that the
/// reactor answers B's send with a fresh task of B's own and the ledger never
/// records A's id as B's. Nothing else pins that: the subscribe test above
/// never goes through a send. A's task is held live by a slow model, so a
/// stream attached to it would carry A's transitions and its answer.
#[test]
fn a_send_naming_another_principals_live_task_streams_none_of_it() {
    let llm = spawn_mock_llm(&json!({
        "turns": [{"content": "B's own answer"}],
        "match": [{"when_contains": "the victim's question", "content": "the private answer", "delay_ms": 3000}],
    }));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&two_principal_config(&llm.uri, port));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    // A starts a task and does not wait for it: the model is still thinking.
    let started = SendMessage::text("the victim's question")
        .bearer(TOKEN_A)
        .return_immediately()
        .post(&addr);
    let a_task = &started["result"]["task"];
    let a_id = a_task["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no task id in {started}"))
        .to_string();
    let a_ctx = a_task["contextId"].as_str().unwrap_or_default().to_string();
    assert!(!a_ctx.is_empty(), "A's task has a context: {started}");
    let live = rpc_as(&addr, TOKEN_A, 2, "GetTask", json!({"id": a_id}));
    assert_ne!(
        live["result"]["status"]["state"], "TASK_STATE_COMPLETED",
        "A's task must still be live when B names it: {live}"
    );

    // B names A's task in a streaming send, and reads until the stream ends or
    // well past the moment A's answer is produced.
    let probe = SendMessage::text("probe from b")
        .bearer(TOKEN_B)
        .task(&a_id)
        .streaming();
    let headers = probe.headers();
    let extra: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut reader = a2a_open(&addr, &probe.body(3), &extra, Duration::from_secs(6));
    let mut body = String::new();
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => body.push_str(&line),
        }
    }

    assert!(
        !body.contains("the private answer"),
        "B streamed A's answer: {body}"
    );
    assert!(!body.contains(&a_ctx), "B learned A's context: {body}");
    // A's task id may come back only inside an error (B named it); no frame
    // that is a result may carry it.
    for data in body.lines().filter_map(|l| l.strip_prefix("data:")) {
        let frame: Value = serde_json::from_str(data.trim())
            .unwrap_or_else(|e| panic!("a non-JSON frame ({e}): {data}"));
        assert!(
            frame.get("error").is_some() || !frame.to_string().contains(&a_id),
            "B received a frame of A's task: {frame}"
        );
    }

    // The control: A's task did settle with the answer B must not have seen,
    // inside the window B was reading, and B's message is not in it.
    let end = Instant::now() + Duration::from_secs(15);
    let settled = loop {
        let got = rpc_as(&addr, TOKEN_A, 4, "GetTask", json!({"id": a_id}));
        if got["result"]["status"]["state"] == "TASK_STATE_COMPLETED" {
            break got;
        }
        assert!(Instant::now() < end, "A's task never settled: {got}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let settled = settled.to_string();
    assert!(
        settled.contains("the private answer"),
        "A's task carries its answer: {settled}"
    );
    assert!(
        !settled.contains("probe from b"),
        "B's message joined A's task: {settled}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// The RSS of a running process, in KiB.
fn rss_kb(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap_or_else(|e| panic!("read /proc/{pid}/status: {e}"));
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no VmRSS in /proc/{pid}/status"))
}

/// A daemon with no principals at all: loopback is the operator, so a request
/// with an unknown method is answered rather than logged as a denial. The admin
/// check that handles the method name runs either way — this only keeps the
/// measurement from being about the size of the log file.
fn loopback_config(port: u16) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: a2a-leak\n  instruction: You are a test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: https://127.0.0.1:9\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

#[test]
fn a_flood_of_distinct_method_names_does_not_grow_the_daemon() {
    /// Long enough that a leaked copy is unmistakable against allocator noise,
    /// short enough to stay well inside the request-body limit.
    const NAME: usize = 64_000;
    /// A leaked copy of every name would put ~15 MiB between the two readings,
    /// against the ~0.2 MiB a steady-state daemon actually moves — two orders of
    /// magnitude, which is why a threshold works here at all.
    const FLOOD: u64 = 250;

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&loopback_config(port));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let call = |n: u64| {
        let method = format!("{n:08}{}", "m".repeat(NAME));
        a2a_post(&addr, &rpc_body(n as i64, &method, json!({})), &[]).body
    };

    // Warm up first: the first requests grow the allocator's arenas, the
    // connection pools and the log buffers, and that growth is real and
    // bounded. The measurement starts once the daemon is in steady state.
    for n in 0..40 {
        let answer = call(n);
        assert!(
            answer.contains("error"),
            "an unknown method is refused, not served: {answer}"
        );
    }
    let before = rss_kb(daemon.pid());
    for n in 40..40 + FLOOD {
        call(n);
    }
    let after = rss_kb(daemon.pid());

    let growth = after.saturating_sub(before);
    let leaked = FLOOD * NAME as u64 / 1024;
    assert!(
        growth < leaked / 3,
        "RSS grew {growth} KiB over {FLOOD} requests \
         (a leaked copy of every method name would be ~{leaked} KiB): \
         {before} KiB → {after} KiB"
    );
    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// The error envelope and status of a refusal that must be JSON.
fn json_refusal(reply: &common::HttpReply) -> (u16, Value) {
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.contains("application/json")),
        "a refusal is JSON, never a stream: {reply:?}"
    );
    (reply.status, reply.json())
}

/// Every way a request can be refused for WHO sent it, and for what it asked.
///
/// - no credential, or one that matches nothing, is HTTP 401 with a challenge
///   (`invalid_token` when one was presented), code -31401, id null — the
///   body was never parsed;
/// - a named caller asking for a method its role may not call, or a command
///   its grants do not cover, is HTTP 403, code -31403, its id echoed, and a
///   bearer caller is told the token lacks the scope;
/// - the extended card is an authenticated read, so it gets the 401 too;
/// - a streaming send refused either way is JSON, not an SSE frame.
#[test]
fn credentials_are_challenged_with_401_and_roles_refused_with_403() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&two_principal_config(&llm.uri, port));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let unauthenticated = |reply: common::HttpReply, presented: bool, what: &str| {
        let challenge = reply.header("www-authenticate").map(str::to_string);
        let (status, v) = json_refusal(&reply);
        assert_eq!(status, 401, "{what}: {v}");
        assert_eq!(v["error"]["code"], -31401, "{what}: {v}");
        assert_eq!(v["id"], Value::Null, "{what}: the body was never read: {v}");
        assert_eq!(
            v["error"]["data"][0]["reason"], "UNAUTHENTICATED",
            "{what}: {v}"
        );
        let challenge = challenge.unwrap_or_else(|| panic!("{what}: no challenge"));
        assert!(
            challenge.starts_with("Bearer realm=\"agentd\""),
            "{what}: {challenge}"
        );
        assert_eq!(
            challenge.contains("error=\"invalid_token\""),
            presented,
            "{what}: {challenge}"
        );
    };

    // Nothing presented.
    unauthenticated(
        a2a_post(&addr, &rpc_body(1, "ListTasks", json!({})), &[]),
        false,
        "no credential",
    );
    // A bearer that matches nothing: a failed credential, not "anonymous".
    unauthenticated(
        a2a_post(
            &addr,
            &rpc_body(2, "GetTask", json!({"id": "t1"})),
            &[("Authorization", "Bearer not-a-real-token")],
        ),
        true,
        "junk bearer",
    );
    // A session token nobody issued.
    let reply = a2a_post(
        &addr,
        &rpc_body(3, "ListTasks", json!({})),
        &[("Authorization", "Bearer agentd_at_nobody")],
    );
    assert!(
        reply
            .header("www-authenticate")
            .is_some_and(|c| c.contains("unknown, expired or revoked")),
        "a dead session token is named as one: {reply:?}"
    );
    unauthenticated(reply, true, "dead session token");
    // The extended card is an authenticated read.
    unauthenticated(
        a2a_post(&addr, &rpc_body(4, "GetExtendedAgentCard", json!({})), &[]),
        false,
        "uncredentialed extended card",
    );
    // A streaming send is refused as JSON, not as a one-frame stream.
    let stream = SendMessage::text("hi").streaming();
    unauthenticated(
        a2a_post(&addr, &stream.body(5), &[]),
        false,
        "uncredentialed stream",
    );

    let forbidden = |reply: common::HttpReply, id: i64, what: &str| {
        let challenge = reply.header("www-authenticate").map(str::to_string);
        let (status, v) = json_refusal(&reply);
        assert_eq!(status, 403, "{what}: {v}");
        assert_eq!(v["error"]["code"], -31403, "{what}: {v}");
        assert_eq!(v["id"], json!(id), "{what}: the id is echoed: {v}");
        assert_eq!(
            v["error"]["data"][0]["reason"], "PERMISSION_DENIED",
            "{what}: {v}"
        );
        assert!(
            challenge.is_some_and(|c| c.contains("error=\"insufficient_scope\"")),
            "{what}: a bearer caller is told the scope is short"
        );
        v
    };
    let auth_b = format!("Bearer {TOKEN_B}");
    // A name outside the method table is not a method at all: -32601, as it
    // is for every caller, before authorization could make the answers
    // differ — never a 403 that would confirm the name means something.
    let reply = a2a_post(
        &addr,
        &rpc_body(6, "a2a.drainX", json!({})),
        &[("Authorization", &auth_b)],
    );
    assert_eq!(reply.status, 200, "an unknown method: {reply:?}");
    assert_eq!(reply.json()["error"]["code"], -32601, "{reply:?}");
    // A command the role is not granted: `plan.get` is a user op, not an
    // agent one. Blocking and streaming both answer with the JSON 403.
    let command = SendMessage::command("plan.get", json!({})).bearer(TOKEN_B);
    let headers = command.headers();
    let extra: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    forbidden(
        a2a_post(&addr, &command.body(7), &extra),
        7,
        "an ungranted command",
    );
    let streamed = command.clone().streaming();
    forbidden(
        a2a_post(&addr, &streamed.body(8), &extra),
        8,
        "an ungranted command, streamed",
    );
    // The runtime's own refusal — a workflow this user may not run, which only
    // the runtime can judge — travels with its status too, as JSON, even to
    // a caller that asked for a stream.
    let run = SendMessage::command("workflow.run", json!({"workflow": "nope"}))
        .bearer(TOKEN_A)
        .streaming();
    let headers = run.headers();
    let extra: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (status, v) = json_refusal(&a2a_post(&addr, &run.body(9), &extra));
    assert_eq!((status, &v["error"]["code"]), (403, &json!(-31403)), "{v}");
    assert_eq!(v["id"], 9, "{v}");

    // The denial is audited with the rule that named the caller.
    let log = daemon.stderr();
    assert!(
        log.lines().any(|l| l.contains("\"a2a.denied\"")
            && l.contains("\"rule\":\"token-b\"")
            && l.contains("\"status\":403")),
        "no a2a.denied line naming the rule:\n{log}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// The extended card needs a credential of a scheme the card DECLARES.
///
/// An `any` rule makes every uncredentialed caller somebody (`user:pub`), which
/// is enough to talk to the agent — but the spec makes the extended card an
/// authenticated read (§13.3), and a caller nobody authenticated is not that.
/// On a listener that declares a bearer, the uncredentialed caller is asked for
/// one; with the bearer, the card is served.
#[test]
fn extended_card_needs_a_declared_scheme_credential() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "hello pub"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&format!(
        "config_version: \"1\"\n\
         agent:\n  name: a2a-authz\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         \x20 bearer: \"{{{{secret:AGENTD_AUTHZ_TOKEN_A}}}}\"\n\
         \x20 principals:\n\
         \x20   - id: pub\n\
         \x20     match: {{ any: true }}\n\
         \x20     role: user\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n",
        llm = llm.uri
    ));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let card = a2a_post(&addr, &rpc_body(1, "GetExtendedAgentCard", json!({})), &[]);
    let (status, v) = json_refusal(&card);
    assert_eq!((status, &v["error"]["code"]), (401, &json!(-31401)), "{v}");

    let sent = SendMessage::text("hello").post(&addr);
    assert!(
        sent.get("error").is_none(),
        "an uncredentialed send under the any rule is served: {sent}"
    );
    // …as `user:pub`, the rule's principal — never the operator: an
    // operator-only op is refused, naming who asked.
    let config = SendMessage::command("config", json!({})).post(&addr);
    assert_eq!(config["error"]["code"], -31403, "{config}");
    assert!(
        config["error"]["message"]
            .as_str()
            .is_some_and(|m| m.ends_with("for user:pub")),
        "{config}"
    );

    let card = rpc_as(&addr, TOKEN_A, 2, "GetExtendedAgentCard", json!({}));
    assert!(
        card.get("error").is_none() && card["result"].is_object(),
        "with the bearer the card is served: {card}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// A browser is never the implicit operator — not even of a no-auth loopback
/// daemon that lists its origin.
///
/// Any page a local browser loads can make it POST to 127.0.0.1, and listing an
/// origin in `a2a.cors.origins` admits a UI, it does not vouch for the person
/// behind it. So the request that carries `Origin` is asked to sign in, and is
/// told how; the same request without it — a terminal, `curl` — is the
/// operator it always was.
#[test]
fn a_browser_origin_is_never_the_implicit_operator() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&loopback_config(port).replace(
        "  listen: ",
        "  cors:\n    origins: [\"http://127.0.0.1:4173\"]\n  listen: ",
    ));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let body = rpc_body(1, "ListTasks", json!({}));
    let browser = a2a_post(&addr, &body, &[("Origin", "http://127.0.0.1:4173")]);
    let (status, v) = json_refusal(&browser);
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["error"]["code"], -31401, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("browser requests must authenticate")),
        "the browser is told how to sign in: {v}"
    );
    assert_eq!(
        browser.header("access-control-allow-origin"),
        Some("http://127.0.0.1:4173"),
        "the listed origin can read the refusal"
    );

    let terminal = rpc(&addr, 2, "ListTasks", json!({}));
    assert!(
        terminal.get("error").is_none(),
        "the same request without Origin is the operator: {terminal}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// A reload REBUILDS the principal rules — proven by revocation taking effect.
///
/// Principals compile into a `Resolver` at startup. Until v1.4.0 the listener
/// held the one built at boot forever: an operator could demote a principal,
/// reload, watch `config.reloaded` report success, and still be serving the
/// old rules. v1.3.3 made that honest by refusing the reload; this makes it
/// work. The demotion is the case worth testing, because a revocation that
/// silently does not apply is the direction that costs you something.
#[test]
#[cfg(feature = "hot-reload")]
fn a_reload_demotes_a_principal_and_the_revocation_takes_effect() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "ok"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    // `plan.get` is a USER grant and not an AGENT one, so it is exactly the
    // privilege a demotion is meant to remove.
    let cfg_for = |role: &str| {
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: a2a-authz\n  instruction: You are a helpful test agent.\n  preflight: never\n\
             intelligence:\n  endpoints: {llm}\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             \x20 principals:\n\
             \x20   - id: token-a\n\
             \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_A}}}}\" }}\n\
             \x20     role: {role}\n\
             \x20   - id: token-b\n\
             \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_B}}}}\" }}\n\
             \x20     role: agent\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n",
            llm = llm.uri
        )
    };

    let cfg = write_config(&cfg_for("user"));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let plan_get = || {
        SendMessage::command("plan.get", json!({}))
            .bearer(TOKEN_A)
            .post(&addr)
    };

    // As a user, the command is not refused for want of a grant.
    let before = plan_get();
    // The authorization refusal is -31403, from whichever gate catches it
    // first: the listener's command-op gate rejects a command the role cannot
    // run before `a2a_command` is ever reached, so matching on the inner "not
    // granted" text would miss the real denial.
    let refused = |r: &Value| r["error"]["code"].as_i64() == Some(-31403);
    assert!(!refused(&before), "a user may plan.get: {before}");

    // Demote A to `agent` and reload.
    std::fs::write(&cfg, cfg_for("agent")).unwrap();
    unsafe { libc::kill(daemon.pid() as i32, libc::SIGHUP) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon.stderr().contains("\"event\":\"config.reloaded\"") {
        assert!(
            Instant::now() < deadline,
            "the daemon never reloaded:\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The revocation is live: the same bearer, the same method, now refused.
    let after = plan_get();
    assert!(
        refused(&after),
        "the demotion did not take effect — plan.get still granted: {after}"
    );

    assert!(daemon.alive(), "the daemon is still serving");
    std::fs::remove_file(&cfg).ok();
}

// ── Ownership of the objects a command names ────────────────────────────────
//
// A task is not the only thing a principal owns: the runs it starts and the
// subagents its turns spawn are its own too, and every op that names one — by
// a run id or a handle anyone can guess or read off a log — acts only for the
// owner. A non-owner is told the object does not exist, exactly as an unknown
// id is, so it cannot even learn which ids are real. The model driving a
// user's turn is held to the same line: it acts for that user and no further.

/// Two users on one listener. A owns what it starts; B is granted
/// `b_grants`, which for most tests is everything a user can be given, and
/// still owns nothing of A's. `waiter` parks on a signal so A's run stays
/// live; `all-clear` is the instance's retirement signal. `doors` runs from a
/// manual start anyone may use, and has a second, operator-only one; `later`
/// hands its owner's run to the model after a pause long enough to reload or
/// restart the daemon under it.
fn owners_config(llm: &str, port: u16, b_grants: &str) -> String {
    owners_config_on(llm, port, b_grants, "store:\n  kind: memory\n")
}

fn owners_config_on(llm: &str, port: u16, b_grants: &str, store: &str) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: a2a-owners\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         {store}\
         a2a:\n  listen: http://127.0.0.1:{port}\n  introspection:\n    enabled: true\n\
         \x20 principals:\n\
         \x20   - id: token-a\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_A}}}}\" }}\n\
         \x20     role: user\n\
         \x20     grants: [\"*\"]\n\
         \x20   - id: token-b\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_B}}}}\" }}\n\
         \x20     role: user\n\
         \x20     grants: {b_grants}\n\
         workflows:\n\
         \x20 - name: waiter\n    steps:\n      s: {{kind: manual}}\n      w: {{kind: wait, on: signal, signal: go, depends_on: [s]}}\n      f: {{kind: finish, depends_on: [w], output: released}}\n\
         \x20 - name: triage\n    steps:\n      s: {{kind: manual}}\n      w: {{kind: wait, on: signal, signal: never, depends_on: [s]}}\n      f: {{kind: finish, depends_on: [w]}}\n\
         \x20 - name: triage-kick\n    steps:\n      k: {{kind: signal, name: kick}}\n      f: {{kind: finish, depends_on: [k]}}\n\
         \x20 - name: deploy-kick\n    steps:\n      k: {{kind: signal, name: kick}}\n      f: {{kind: finish, depends_on: [k]}}\n\
         \x20 - name: deploy\n    steps:\n      s: {{kind: manual}}\n      w: {{kind: wait, on: signal, signal: never, depends_on: [s]}}\n      f: {{kind: finish, depends_on: [w]}}\n\
         \x20 - name: doors\n    steps:\n      s: {{kind: manual}}\n      o: {{kind: a2a, command: doors.open, roles: [operator]}}\n      f: {{kind: finish, depends_on: [s]}}\n\
         \x20 - name: later\n    steps:\n      s: {{kind: manual}}\n      n: {{kind: sleep, depends_on: [s], duration: 4s}}\n      a: {{kind: agent, depends_on: [n], instruction: \"LATER-STEP: run what you were asked\"}}\n      f: {{kind: finish, depends_on: [a]}}\n\
         lifecycle:\n  run_until: drained\n  until_signal: all-clear\n\
         observability:\n  log_level: info\n"
    )
}

/// The document a command answered with: a read's Message data, or the JSON
/// DataPart artifact of the task a piece of work completed.
fn answer(v: &Value) -> Value {
    let r = &v["result"];
    if let Some(doc) = r["message"]["parts"][0].get("data") {
        return doc.clone();
    }
    r["task"]["artifacts"][0]["parts"][0]["data"].clone()
}

fn command_as(addr: &str, bearer: &str, op: &str, args: Value) -> Value {
    SendMessage::command(op, args)
        .bearer(bearer)
        .return_immediately()
        .post(addr)
}

/// The state of `waiter` run `run` as its owner A sees it: the run's status,
/// and its wait step's — `suspended` while it is parked on the signal.
fn run_state(addr: &str, run: &str) -> (String, String) {
    let v = command_as(addr, TOKEN_A, "run.get", json!({"run": run}));
    let r = &answer(&v)["run"];
    let status = r["status"]
        .as_str()
        .unwrap_or_else(|| panic!("A reads its own run: {v}"));
    let wait = r["steps"]["w"]["status"].as_str().unwrap_or("");
    (status.to_string(), wait.to_string())
}

/// Whether A's run is still where A left it: live, parked on its signal.
fn parked(addr: &str, run: &str) -> bool {
    run_state(addr, run) == ("running".to_string(), "suspended".to_string())
}

/// A's live objects: a run parked on the `go` signal, a warm subagent its
/// turn spawned, and the conversation that turn ran in.
struct Owned {
    run: String,
    handle: String,
    ctx: String,
}

/// Have A start a parked run and, through the model, a warm subagent.
fn a_owns_a_run_and_a_subagent(addr: &str, daemon: &Daemon) -> Owned {
    let started = command_as(addr, TOKEN_A, "workflow.run", json!({"workflow": "waiter"}));
    assert!(started.get("error").is_none(), "A starts a run: {started}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let run = loop {
        let v = command_as(addr, TOKEN_A, "workflow.status", json!({}));
        if let Some(r) = answer(&v)["runs"][0]["run"].as_str()
            && parked(addr, r)
        {
            break r.to_string();
        }
        assert!(Instant::now() < deadline, "A's run never parked: {v}");
        std::thread::sleep(Duration::from_millis(50));
    };

    let sent = SendMessage::text("start a warm helper")
        .bearer(TOKEN_A)
        .post(addr);
    let task = &sent["result"]["task"];
    assert_eq!(
        task["status"]["state"], "TASK_STATE_COMPLETED",
        "A's turn spawned its helper: {sent}"
    );
    let ctx = task["contextId"].as_str().unwrap().to_string();
    // The handle is read off the daemon's own log, as an operator would.
    let handle = daemon
        .stderr()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "subagent.spawn")
        .and_then(|v| v["handle"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("no subagent.spawn line:\n{}", daemon.stderr()));
    let st = command_as(addr, TOKEN_A, "subagent.status", json!({"handle": handle}));
    assert_eq!(answer(&st)["status"], "running", "A's helper is warm: {st}");
    Owned { run, handle, ctx }
}

/// A playbook whose root turn plays `turns`, while a warm helper — whose
/// prompt is the subagent default, not this agent's instruction — stands by.
fn owners_playbook(turns: Value) -> Value {
    json!({
        "turns": turns,
        "match": [
            {"when_contains": "You are agentd, an autonomous agent.", "content": "standing by"},
        ],
    })
}

/// A's turn: spawn a warm helper, then answer.
fn a_spawns_a_helper() -> Value {
    owners_playbook(json!([
        {"tool_calls": [{"name": "subagent.run", "arguments": {"instruction": "stand by for instructions", "mode": "warm"}}]},
        {"content": "helper started"},
    ]))
}

/// Every op that names a run, a subagent or a conversation answers a
/// non-owner exactly as it answers an id that does not exist: -32001, one
/// fixed message per kind of object, and the object untouched. B holds every
/// grant a user can hold, so nothing here is refused for want of one.
#[test]
fn every_command_op_that_names_an_object_is_owner_scoped() {
    let llm = spawn_mock_llm(&a_spawns_a_helper());
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&owners_config(&llm.uri, port, "[\"*\"]"));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);
    let a = a_owns_a_run_and_a_subagent(&addr, &daemon);

    let not_found = |op: &str, args: Value, msg: &str| {
        let v = command_as(&addr, TOKEN_B, op, args.clone());
        assert_eq!(v["error"]["code"], -32001, "B's {op} {args}: {v}");
        assert_eq!(v["error"]["message"], msg, "B's {op} {args}: {v}");
        v["error"].clone()
    };
    let cases = [
        ("workflow.status", json!({"run": a.run}), "no such run"),
        ("workflow.cancel", json!({"run": a.run}), "no such run"),
        (
            "workflow.signal",
            json!({"name": "go", "run": a.run}),
            "no such run",
        ),
        (
            "subagent.send",
            json!({"handle": a.handle, "message": "hijacked"}),
            "no such subagent",
        ),
        (
            "subagent.kill",
            json!({"handle": a.handle}),
            "no such subagent",
        ),
        (
            "subagent.status",
            json!({"handle": a.handle}),
            "no such subagent",
        ),
        (
            "subagent.get",
            json!({"handle": a.handle}),
            "no such subagent",
        ),
        ("run.get", json!({"run": a.run}), "no such run"),
        ("plan.get", json!({"id": a.ctx}), "no such conversation"),
        (
            "conversation.get",
            json!({"id": a.ctx}),
            "no such conversation",
        ),
    ];
    for (op, args, msg) in cases {
        let theirs = not_found(op, args.clone(), msg);
        // The same op on an id nobody has: the answers are identical, so B
        // cannot tell a real id of A's from a guess.
        let mut unknown = args.clone();
        for k in ["run", "handle", "id"] {
            if unknown.get(k).is_some() {
                unknown[k] = json!("does-not-exist");
            }
        }
        assert_eq!(not_found(op, unknown, msg), theirs, "{op}");
    }

    // A signal that names no run reaches only B's own runs — none here.
    let v = command_as(&addr, TOKEN_B, "workflow.signal", json!({"name": "go"}));
    // A DataPart's number is a `google.protobuf.Value` double on the wire.
    assert_eq!(
        answer(&v)["delivered"],
        0.0,
        "B's broadcast woke A's run: {v}"
    );
    // The instance's retirement signal, from a user, retires nothing.
    let v = command_as(
        &addr,
        TOKEN_B,
        "workflow.signal",
        json!({"name": "all-clear"}),
    );
    assert!(v.get("error").is_none(), "{v}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon
        .stderr()
        .contains("\"lifecycle.until_signal.refused\"")
    {
        assert!(
            Instant::now() < deadline,
            "no refusal logged:\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !daemon.stderr().contains("\"lifecycle.until_signal\""),
        "a user's signal began the drain:\n{}",
        daemon.stderr()
    );

    // Everything of A's is as A left it, and A can still use it.
    assert!(parked(&addr, &a.run), "A's run is untouched");
    let st = command_as(
        &addr,
        TOKEN_A,
        "subagent.status",
        json!({"handle": a.handle}),
    );
    assert_eq!(
        answer(&st)["status"],
        "running",
        "A's helper is untouched: {st}"
    );
    let v = command_as(
        &addr,
        TOKEN_A,
        "workflow.signal",
        json!({"name": "go", "run": a.run}),
    );
    assert_eq!(
        answer(&v)["delivered"],
        1.0,
        "the owner's signal lands: {v}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while run_state(&addr, &a.run).0 != "completed" {
        assert!(Instant::now() < deadline, "A's run never completed");
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// The model acts for whoever drives its turn. B cannot cancel A's run over
/// A2A; B asking the model to do it must fail the same way, or ownership is a
/// rule about which door a request uses.
#[test]
fn the_model_cannot_act_on_another_principals_objects() {
    let pb = common::unique_path("owners-playbook", "json");
    std::fs::write(&pb, a_spawns_a_helper().to_string()).unwrap();
    let llm = spawn_mock_llm_file(&pb);
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&owners_config(&llm.uri, port, "[\"*\"]"));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);
    let a = a_owns_a_run_and_a_subagent(&addr, &daemon);

    // B's turn: the model reaches for A's run and A's helper by id.
    std::fs::write(
        &pb,
        owners_playbook(json!([
            {"tool_calls": [
                {"name": "workflow.cancel", "arguments": {"run": a.run}},
                {"name": "workflow.signal", "arguments": {"name": "go", "run": a.run}},
                {"name": "subagent.kill", "arguments": {"handle": a.handle}},
            ]},
            {"content": "tried"},
        ]))
        .to_string(),
    )
    .unwrap();
    let sent = SendMessage::text("clean up everything")
        .bearer(TOKEN_B)
        .post(&addr);
    let task = &sent["result"]["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{sent}");
    let b_ctx = task["contextId"].as_str().unwrap().to_string();

    // Nothing of A's moved.
    std::thread::sleep(Duration::from_millis(500));
    assert!(parked(&addr, &a.run), "B's model cancelled or woke A's run");
    let st = command_as(
        &addr,
        TOKEN_A,
        "subagent.status",
        json!({"handle": a.handle}),
    );
    assert_eq!(
        answer(&st)["status"],
        "running",
        "B's model killed A's helper: {st}"
    );

    // And the model was told what B would have been told.
    let conv = command_as(&addr, TOKEN_B, "conversation.get", json!({"id": b_ctx}));
    let results: Vec<String> = answer(&conv)["conversation"]["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("B reads its own conversation: {conv}"))
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m.to_string())
        .collect();
    assert_eq!(results.len(), 3, "three tool results: {conv}");
    let run_ref = format!("no such run \\\"{}\\\"", a.run);
    let handle_ref = format!("no such subagent \\\"{}\\\"", a.handle);
    assert_eq!(
        results.iter().filter(|r| r.contains(&run_ref)).count(),
        2,
        "cancel and signal say the run does not exist: {results:?}"
    );
    assert!(
        results.iter().any(|r| r.contains(&handle_ref)),
        "kill says the subagent does not exist: {results:?}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_file(&pb).ok();
}

/// `grants: [workflow.run:triage*]` narrows what B may run, and neither
/// asking the model nor sending a signal widens it: the model's
/// `workflow.run` and a principal's signal starts ask the one predicate the
/// op and the card ask. What B's signal does start is B's own.
#[test]
fn a_narrowed_principal_cannot_run_a_workflow_through_the_model() {
    let llm = spawn_mock_llm(&json!({"turns": [
        {"tool_calls": [
            {"name": "workflow.run", "arguments": {"name": "deploy"}},
            {"name": "workflow.run", "arguments": {"name": "triage"}},
        ]},
        {"content": "started what I could"},
    ]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&owners_config(
        &llm.uri,
        port,
        "[\"workflow.run:triage*\", \"workflow.signal\"]",
    ));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let sent = SendMessage::text("run deploy and triage")
        .bearer(TOKEN_B)
        .post(&addr);
    assert_eq!(
        sent["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );
    // Every run B started is B's, so B's own listing is all of them.
    let deadline = Instant::now() + Duration::from_secs(10);
    let runs = loop {
        let v = command_as(&addr, TOKEN_B, "workflow.status", json!({}));
        let runs = answer(&v)["runs"].clone();
        if runs.as_array().is_some_and(|a| !a.is_empty()) {
            break runs;
        }
        assert!(
            Instant::now() < deadline,
            "the triage run never appeared: {v}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let names: Vec<&str> = runs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["workflow"].as_str())
        .collect();
    assert_eq!(names, ["triage"], "deploy ran through the model: {runs}");

    // `kick` starts both `*-kick` workflows for the runtime; from B it starts
    // only the one B may run, and that run is B's.
    let v = command_as(&addr, TOKEN_B, "workflow.signal", json!({"name": "kick"}));
    assert!(v.get("error").is_none(), "{v}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let v = command_as(&addr, TOKEN_B, "workflow.status", json!({}));
        let mine: Vec<String> = answer(&v)["runs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| r["workflow"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if mine.iter().any(|w| w == "triage-kick") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B's signal start is not B's: {v}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    for refused in ["deploy", "deploy-kick"] {
        assert!(
            daemon
                .stderr()
                .lines()
                .all(|l| !(l.contains("\"run.start\"")
                    && l.contains(&format!("\"workflow\":\"{refused}\"")))),
            "a {refused} run was started:\n{}",
            daemon.stderr()
        );
    }

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// The tool results of the root turn in conversation `ctx`, as `bearer`
/// (its owner) reads them back.
fn tool_results(addr: &str, bearer: &str, ctx: &str) -> Vec<String> {
    let conv = command_as(addr, bearer, "conversation.get", json!({"id": ctx}));
    answer(&conv)["conversation"]["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("the owner reads its conversation: {conv}"))
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m.to_string())
        .collect()
}

/// Send `text` as `bearer` and wait for the turn; the conversation's id.
fn turn_as(addr: &str, bearer: &str, text: &str) -> String {
    let sent = SendMessage::text(text).bearer(bearer).post(addr);
    let task = &sent["result"]["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{sent}");
    task["contextId"].as_str().unwrap().to_string()
}

/// What the model may list, and what it may change, when it acts for a
/// principal that is not the operator.
///
/// B's model lists runs and subagents and sees none of A's — the ids are
/// what every object-naming tool takes, so listing them would undo the
/// not-found answer the tools give. It cannot touch the instance's own
/// controls either: defining, redefining, deleting or disarming a workflow is
/// the operator's over A2A, and an armed workflow B's model defined would run
/// as the runtime, with no owner to check. Nor does it fire a start other than
/// the default one, which is all `may_run` judged. A's model, meanwhile, sees
/// what is A's: the filter narrows to the owner, not to nothing.
#[test]
fn the_model_lists_only_its_callers_objects_and_leaves_the_instance_alone() {
    let pb = common::unique_path("owners-playbook", "json");
    std::fs::write(&pb, a_spawns_a_helper().to_string()).unwrap();
    let llm = spawn_mock_llm_file(&pb);
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&owners_config(&llm.uri, port, "[\"*\"]"));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);
    let a = a_owns_a_run_and_a_subagent(&addr, &daemon);

    let definition = |name: &str| json!({"name": name, "steps": {"s": {"kind": "once"}, "f": {"kind": "finish", "depends_on": ["s"]}}});
    std::fs::write(
        &pb,
        owners_playbook(json!([
            {"tool_calls": [
                {"name": "workflow.status", "arguments": {}},
                {"name": "workflow.list", "arguments": {}},
                {"name": "subagent.list", "arguments": {}},
                {"name": "workflow.pause", "arguments": {"name": "waiter"}},
                {"name": "workflow.create", "arguments": {"definition": definition("b-made")}},
                {"name": "workflow.update", "arguments": {"name": "deploy", "definition": definition("deploy")}},
                {"name": "workflow.delete", "arguments": {"name": "deploy"}},
                {"name": "workflow.run", "arguments": {"name": "doors", "start": "o"}},
            ]},
            {"content": "tried"},
        ]))
        .to_string(),
    )
    .unwrap();
    let b_ctx = turn_as(&addr, TOKEN_B, "look around and rearrange things");
    let results = tool_results(&addr, TOKEN_B, &b_ctx);
    assert_eq!(results.len(), 8, "eight tool results: {results:?}");
    let all = results.join("\n");
    assert!(
        !all.contains(&a.run) && !all.contains(&a.handle),
        "B's model listed A's objects: {results:?}"
    );
    let instance_wide = "acts on the whole instance and is not permitted for user:token-b";
    assert_eq!(
        results.iter().filter(|r| r.contains(instance_wide)).count(),
        4,
        "pause-by-name, create, update and delete are refused: {results:?}"
    );
    assert!(
        results
            .iter()
            .any(|r| r.contains("may run \\\"doors\\\" only from its default start")),
        "the operator-only start is refused: {results:?}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !daemon
            .stderr()
            .lines()
            .any(|l| l.contains("\"run.start\"") && l.contains("\"workflow\":\"doors\"")),
        "a doors run was started:\n{}",
        daemon.stderr()
    );
    // What B's model tried to change is as it was: `deploy` still runs,
    // and `waiter` still takes a new run.
    for workflow in ["deploy", "waiter"] {
        let v = command_as(
            &addr,
            TOKEN_A,
            "workflow.run",
            json!({"workflow": workflow}),
        );
        assert!(v.get("error").is_none(), "{workflow} is intact: {v}");
    }

    // A's model sees A's run and A's helper.
    std::fs::write(
        &pb,
        owners_playbook(json!([
            {"tool_calls": [
                {"name": "workflow.status", "arguments": {}},
                {"name": "subagent.list", "arguments": {}},
            ]},
            {"content": "listed"},
        ]))
        .to_string(),
    )
    .unwrap();
    let a_ctx = turn_as(&addr, TOKEN_A, "what is mine?");
    let results = tool_results(&addr, TOKEN_A, &a_ctx);
    assert!(
        results[0].contains(&a.run),
        "A's model sees A's run: {results:?}"
    );
    assert!(
        results[1].contains(&a.handle),
        "A's model sees A's helper: {results:?}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_file(&pb).ok();
}

/// Whether the daemon's log records a run of `workflow` starting.
fn started(log: &str, workflow: &str) -> bool {
    log.lines()
        .any(|l| l.contains("\"run.start\"") && l.contains(&format!("\"workflow\":\"{workflow}\"")))
}

/// Wait for B's `later` run to finish: its model step has acted by then.
fn wait_later_done(daemon: &Daemon) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !daemon
        .stderr()
        .lines()
        .any(|l| l.contains("\"run.done\"") && l.contains("\"workflow\":\"later\""))
    {
        assert!(
            Instant::now() < deadline,
            "B's later run never finished:\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// B starts `later`, whose model step will ask to run `workflow` once the
/// pause is over.
fn b_starts_later(addr: &str, daemon: &Daemon) {
    let v = command_as(addr, TOKEN_B, "workflow.run", json!({"workflow": "later"}));
    assert!(v.get("error").is_none(), "B starts later: {v}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started(&daemon.stderr(), "later") {
        assert!(Instant::now() < deadline, "later never started");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A reload that narrows a principal narrows what the model does for it at
/// once — for work already in flight, before that principal calls again.
///
/// The model acts with the principal the runtime last indexed for an id. The
/// index was filled only when a request arrived, so a principal narrowed by
/// a reload kept its old grants in every turn and step already running for
/// it until it happened to call — and one whose rule was removed never does.
#[test]
#[cfg(feature = "hot-reload")]
fn a_reload_narrows_what_the_model_does_for_work_in_flight() {
    let llm = spawn_mock_llm(&json!({"turns": [
        {"tool_calls": [{"name": "workflow.run", "arguments": {"name": "deploy"}}]},
        {"content": "asked"},
    ]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&owners_config(&llm.uri, port, "[\"*\"]"));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);
    b_starts_later(&addr, &daemon);

    // Narrow B to `triage*` while its run sleeps, and make no request as B.
    std::fs::write(
        &cfg,
        owners_config(&llm.uri, port, "[\"workflow.run:triage*\"]"),
    )
    .unwrap();
    unsafe { libc::kill(daemon.pid() as i32, libc::SIGHUP) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon
        .stderr()
        .lines()
        .any(|l| l.contains("\"config.reloaded\"") && l.contains("a2a.principals"))
    {
        assert!(
            Instant::now() < deadline,
            "the daemon never reloaded:\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    wait_later_done(&daemon);
    assert!(
        !started(&daemon.stderr(), "deploy"),
        "B's model ran deploy with the grants the reload took away:\n{}",
        daemon.stderr()
    );
    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// Work restored across a restart acts for its owner with the rules in
/// force, before that owner has made a request of the new process.
///
/// Nothing indexed a principal until it called, so after a restart every
/// declared principal — the operator too — was unknown, and its restored
/// model steps failed closed: B's run could no longer run even what B may.
#[test]
fn restored_work_acts_for_its_owner_before_the_owner_calls_again() {
    let llm = spawn_mock_llm(&json!({"turns": [
        {"tool_calls": [{"name": "workflow.run", "arguments": {"name": "triage"}}]},
        {"content": "asked"},
    ]}));
    let dir = common::unique_path("authz-restart", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let store = format!(
        "store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n"
    );
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&owners_config_on(&llm.uri, port, "[\"*\"]", &store));

    let first = spawn_daemon(&cfg);
    wait_ready(&addr);
    b_starts_later(&addr, &first);
    // Mid-sleep: no drain, nothing finished.
    unsafe { libc::kill(first.pid() as i32, libc::SIGKILL) };
    drop(first);

    // The second life: nobody calls as B.
    let mut second = spawn_daemon(&cfg);
    wait_ready(&addr);
    wait_later_done(&second);
    assert!(
        started(&second.stderr(), "triage"),
        "B's restored run could not run what B may:\n{}",
        second.stderr()
    );
    assert!(second.alive(), "daemon still serving: {}", second.stderr());
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// `cfg` with one more principal: the operator, by its own bearer.
fn with_operator(cfg: &str) -> String {
    cfg.replacen(
        "  principals:\n",
        "  principals:\n    - id: op\n      match: { bearer_ref: \"{{secret:AGENTD_AUTHZ_TOKEN_OP}}\" }\n      role: operator\n",
        1,
    )
}

/// The ids of a status document's `section`.
fn ids_in(doc: &Value, section: &str, field: &str) -> Vec<String> {
    doc[section]
        .as_array()
        .unwrap_or_else(|| panic!("no {section} in {doc}"))
        .iter()
        .filter_map(|v| v[field].as_str().map(str::to_string))
        .collect()
}

/// `status` is granted to every named caller, so it answers each with what
/// that caller may see: the instance's facts, the workflows it may run, and
/// its OWN runs, conversations and activity — the same line the feed draws.
/// Another principal's run ids, conversation ids and subagent handles are
/// exactly what the owner-scoped ops refuse to confirm, so a `status` that
/// listed them would undo that; the instance's internals (subagents, budget,
/// the instruction, the store) are the operator's. The model, acting for a
/// caller, reads that caller's view. The operator still reads everything.
#[test]
fn status_is_scoped_to_the_caller() {
    let pb = common::unique_path("status-playbook", "json");
    std::fs::write(&pb, a_spawns_a_helper().to_string()).unwrap();
    let llm = spawn_mock_llm_file(&pb);
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&with_operator(&owners_config(&llm.uri, port, "[\"*\"]")));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);
    let a = a_owns_a_run_and_a_subagent(&addr, &daemon);

    // B's turn asks the model how the agent is doing.
    std::fs::write(
        &pb,
        owners_playbook(json!([
            {"tool_calls": [{"name": "status", "arguments": {}}]},
            {"content": "looked"},
        ]))
        .to_string(),
    )
    .unwrap();
    let b_ctx = turn_as(&addr, TOKEN_B, "how is the agent doing?");

    let status = |bearer: &str| {
        let v = command_as(&addr, bearer, "status", json!({}));
        assert!(v.get("error").is_none(), "status: {v}");
        answer(&v)
    };
    let (sa, sb, so) = (status(TOKEN_A), status(TOKEN_B), status(TOKEN_OP));

    // Each user sees its own work…
    assert_eq!(
        ids_in(&sa, "runs", "id"),
        std::slice::from_ref(&a.run),
        "{sa}"
    );
    assert!(ids_in(&sa, "conversations", "id").contains(&a.ctx), "{sa}");
    assert!(ids_in(&sb, "runs", "id").is_empty(), "{sb}");
    assert_eq!(
        ids_in(&sb, "conversations", "id"),
        std::slice::from_ref(&b_ctx),
        "{sb}"
    );
    for c in sb["conversations"].as_array().unwrap() {
        assert_eq!(c["contextId"], c["id"], "addressable by contextId: {c}");
    }
    // …and nothing of the other's, anywhere in the document.
    let (text_a, text_b) = (sa.to_string(), sb.to_string());
    for theirs in [&a.run, &a.ctx, &a.handle] {
        assert!(!text_b.contains(theirs.as_str()), "B sees {theirs}: {sb}");
    }
    assert!(!text_a.contains(&b_ctx), "A sees B's conversation: {sa}");
    for doc in [&sa, &sb] {
        for kept in [
            "instance",
            "uptime_ms",
            "draining",
            "paused",
            "model",
            "version",
            "skills",
            "skill_prefix",
            "values",
            "workflows",
            "activity",
        ] {
            assert!(doc.get(kept).is_some(), "{kept} is a fact: {doc}");
        }
        for internal in [
            "subagents",
            "children",
            "timers",
            "budget",
            "counters",
            "inbox_pending",
            "tools",
            "store",
            "instruction",
            "run_id",
            "job_shape",
        ] {
            assert!(doc.get(internal).is_none(), "{internal} leaked: {doc}");
        }
        for w in doc["workflows"].as_array().unwrap() {
            assert_eq!(
                w.as_object().map(|o| o.len()),
                Some(1),
                "a workflow is its name: {w}"
            );
        }
    }

    // The model acting for B was answered with B's view.
    let seen = tool_results(&addr, TOKEN_B, &b_ctx);
    assert_eq!(seen.len(), 1, "one status result: {seen:?}");
    assert!(
        seen[0].contains(&b_ctx),
        "B's model sees B's work: {seen:?}"
    );
    for theirs in [&a.run, &a.ctx, &a.handle] {
        assert!(
            !seen[0].contains(theirs.as_str()),
            "B's model sees {theirs}: {seen:?}"
        );
    }

    // The operator reads the whole instance.
    assert!(ids_in(&so, "runs", "id").contains(&a.run), "{so}");
    let convs = ids_in(&so, "conversations", "id");
    assert!(convs.contains(&a.ctx) && convs.contains(&b_ctx), "{so}");
    assert!(
        ids_in(&so, "subagents", "handle").contains(&a.handle),
        "{so}"
    );
    for internal in ["budget", "instruction", "counters", "store"] {
        assert!(
            so.get(internal).is_some(),
            "the operator reads {internal}: {so}"
        );
    }

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_file(&pb).ok();
}

/// What the extended card lists is exactly what `workflow.run` runs.
///
/// Both ask `Runtime::may_run` — the grants AND the default start's roles —
/// so a caller is never offered a workflow the op then refuses, nor refused
/// one it was never told of by a different answer than a stranger gets.
/// Every name off the caller's card is HTTP 403 with PERMISSION_DENIED, the
/// start-roles refusal the runtime makes included, carried out through the
/// a2a-rs path that serves a Task-reply command.
#[test]
fn listed_workflows_are_exactly_the_runnable_ones() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&format!(
        "config_version: \"1\"\n\
         agent:\n  name: a2a-runnable\n  instruction: You are a test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: https://127.0.0.1:9\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         \x20 principals:\n\
         \x20   - id: op\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_OP}}}}\" }}\n\
         \x20     role: operator\n\
         \x20   - id: token-a\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_A}}}}\" }}\n\
         \x20     role: user\n\
         \x20   - id: token-b\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_B}}}}\" }}\n\
         \x20     role: agent\n\
         \x20   - id: token-c\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_AUTHZ_TOKEN_C}}}}\" }}\n\
         \x20     role: agent\n\
         \x20     grants: [\"workflow.run:triage\"]\n\
         workflows:\n\
         \x20 - name: triage\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\n\
         \x20 - name: deploy\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\n\
         \x20 - name: user-only\n    steps:\n      s: {{kind: a2a, command: user.go, roles: [user]}}\n      f: {{kind: finish, depends_on: [s]}}\n\
         \x20 - name: ops-only\n    steps:\n      s: {{kind: a2a, command: ops.go, roles: [operator]}}\n      f: {{kind: finish, depends_on: [s]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    ));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let all = ["triage", "deploy", "user-only", "ops-only"];
    // Written out as well as compared, so the two sides cannot agree by
    // both being wrong (an empty card and a refusing op agree perfectly).
    let callers: [(&str, &str, &[&str]); 4] = [
        (
            "the default user",
            TOKEN_A,
            &["deploy", "triage", "user-only"],
        ),
        ("the default agent", TOKEN_B, &["deploy", "triage"]),
        ("an agent granted workflow.run:triage", TOKEN_C, &["triage"]),
        // The start's roles bind the operator too: `user-only` starts from an
        // a2a start that admits users alone.
        ("the operator", TOKEN_OP, &["deploy", "ops-only", "triage"]),
    ];
    for (who, bearer, expected) in callers {
        let card = rpc_as(&addr, bearer, 1, "GetExtendedAgentCard", json!({}));
        let mut listed: Vec<String> = card["result"]["skills"]
            .as_array()
            .unwrap_or_else(|| panic!("{who}: no extended card: {card}"))
            .iter()
            .filter(|s| {
                s["tags"]
                    .as_array()
                    .is_some_and(|t| t.contains(&json!("workflow")))
            })
            .filter_map(|s| {
                s["id"]
                    .as_str()?
                    .strip_prefix("workflow:")
                    .map(str::to_string)
            })
            .collect();
        listed.sort();
        assert_eq!(listed, expected, "{who}'s card");

        let mut runnable = Vec::new();
        for name in all {
            let reply = SendMessage::command("workflow.run", json!({"workflow": name}))
                .bearer(bearer)
                .return_immediately()
                .post_raw(&addr);
            let v = reply.json();
            if reply.status == 200 && v.get("error").is_none() {
                assert!(v["result"]["task"].is_object(), "{who} runs {name}: {v}");
                runnable.push(name.to_string());
                continue;
            }
            assert_eq!(reply.status, 403, "{who} runs {name}: {v}");
            assert_eq!(v["error"]["code"], -31403, "{who} runs {name}: {v}");
            assert_eq!(
                v["error"]["data"][0]["reason"], "PERMISSION_DENIED",
                "{who} runs {name}: {v}"
            );
        }
        runnable.sort();
        assert_eq!(runnable, listed, "{who}: what runs is what is listed");
    }

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    std::fs::remove_file(&cfg).ok();
}

/// A mock whose playbook is the file at `path`, re-read on every request, so
/// a test can script a later turn once it knows the ids that turn names.
fn spawn_mock_llm_file(path: &str) -> MockLlm {
    let addr_file = common::unique_path("authz-mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{path}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}
