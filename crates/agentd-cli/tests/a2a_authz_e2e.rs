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
//! copy to hand back a `&'static str`. The name is attacker-chosen, unbounded in
//! length, and reached *before* the caller is known to be anybody: an
//! `Authorization: Bearer <junk>` header resolves to the anonymous principal
//! rather than a 401, and every request passes the admin check on its way to
//! being refused. One leak per request is an RSS climb driven from off the box
//! with a `curl` loop. The second test asserts the daemon's own RSS, because a
//! leak has no other observable: the requests all succeed (as errors), and only
//! the memory behind them differs.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::BufRead;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{SendMessage, a2a_open, a2a_post, a2a_post_within, rpc_as, rpc_body};

/// The bearers the two principals present. Literal here, `{{secret:…}}` in the
/// config — a bearer is a secret, and the config may only carry a reference.
const TOKEN_A: &str = "authz-token-for-principal-a";
const TOKEN_B: &str = "authz-token-for-principal-b";

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

/// The leak-prone path is reached *before* the caller is anybody: a junk bearer
/// is not a 401, it is an anonymous principal refused several checks later
/// — after the admin check has already run. Asserted separately because the
/// flood above measures the daemon that treats its caller as the operator, and
/// the claim that matters for exposure is about the caller who has no
/// credentials at all.
#[test]
fn an_unauthenticated_caller_still_reaches_the_admin_check() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&two_principal_config(&llm.uri, port));
    let mut daemon = spawn_daemon(&cfg);
    wait_ready(&addr);

    let refused = rpc_as(&addr, "not-a-real-token", 1, "GetTask", json!({"id": "t1"}));
    assert_eq!(
        refused["error"]["code"], -32003,
        "a junk bearer is refused by the matrix, not by a 401: {refused}"
    );
    // The same request with the header the transport would reject outright —
    // there is none, which is the point: the listener has no credential to
    // require here, so every one of these requests runs the whole dispatch.
    let status = a2a_post(
        &addr,
        &rpc_body(2, "a2a.drainX", json!({})),
        &[("Authorization", "Bearer also-junk")],
    )
    .status;
    assert!(
        status == 200,
        "the request is dispatched and answered, not rejected at the door: {status}"
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
    // The authorization refusal is code -32003, from whichever gate catches it
    // first: the method-level `may(SendMessage, Some(op))` check rejects a
    // command the role cannot run before `a2a_command` is ever reached, so
    // matching on the inner "not granted" text would miss the real denial.
    let refused = |r: &Value| r["error"]["code"].as_i64() == Some(-32003);
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
