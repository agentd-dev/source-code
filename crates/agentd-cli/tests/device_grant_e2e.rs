// SPDX-License-Identifier: AGPL-3.0-only
//! **Signing in with the OAuth 2.0 device authorization grant.**
//!
//! What only shows from outside the process: a client asks the listener
//! origin for a code, an operator — and only an operator — approves it under
//! a name, the client's next poll is a session token, and that token is the
//! principal `user:<name>` until it is revoked. Then:
//!
//! * revocation reaches what the session already opened — an SDK stream, a
//!   blocking send, the observation feed — and by its own sid, so a sibling
//!   session of the same name keeps its own;
//! * ownership is by name, so a re-login reads and continues what the name
//!   started, and another name reads nothing of it;
//! * a device name and a configured rule id never become one principal, even
//!   across a restart, because the identity registry remembers who claimed a
//!   name first.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{HttpReply, SendMessage, a2a_open, rpc_body};

const OPS_TOKEN: &str = "device-grant-operator-bearer";
const CI_TOKEN: &str = "device-grant-ci-bearer";
const PEER_TOKEN: &str = "device-grant-peer-bearer";

// ---- the daemon -------------------------------------------------------------

struct Daemon {
    child: Child,
    stderr_path: String,
    cfg: String,
}

impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// SIGTERM, and wait for the exit — the next daemon on the same store
    /// needs the instance lock this one holds.
    fn stop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Wait for a log line containing every one of `needles`.
    fn wait_log(&self, needles: &[&str]) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let log = self.stderr();
            if let Some(line) = log.lines().find(|l| needles.iter().all(|n| l.contains(n))) {
                return line.to_string();
            }
            assert!(
                Instant::now() < deadline,
                "no line with {needles:?}:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.stderr_path);
        let _ = std::fs::remove_file(&self.cfg);
    }
}

/// Start `agentd` on `cfg` (already written), without waiting for it.
fn start(cfg: &str) -> Daemon {
    let stderr_path = common::unique_path("device-grant-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .env("AGENTD_DG_OPS", OPS_TOKEN)
        .env("AGENTD_DG_CI", CI_TOKEN)
        .env("AGENTD_DG_PEER", PEER_TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");
    Daemon {
        child,
        stderr_path,
        cfg: cfg.to_string(),
    }
}

/// Start `agentd` on `yaml` and wait until `addr` accepts connections.
fn spawn(yaml: &str, addr: &str) -> Daemon {
    let cfg = common::unique_path("device-grant", "yaml");
    std::fs::write(&cfg, yaml).unwrap();
    let mut daemon = start(&cfg);
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(addr).is_err() {
        assert!(
            daemon.alive() && Instant::now() < deadline,
            "the listener never came up:\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    daemon
}

/// A playbook: a message containing `SLOWPLEASE` is answered eight seconds
/// late, so its task stays WORKING; anything else is answered at once.
fn playbook() -> String {
    let path = common::unique_path("device-grant-playbook", "json");
    std::fs::write(
        &path,
        json!({
            "turns": [{"content": "ok"}],
            "match": [{"when_contains": "SLOWPLEASE", "delay_ms": 8000, "content": "late"}],
        })
        .to_string(),
    )
    .unwrap();
    path
}

/// A daemon config: `listen`, then `a2a_extra` under `a2a:`, `store`, and a
/// `waiter` workflow a user may start that parks on a signal.
fn config(listen: &str, a2a_extra: &str, store: &str) -> String {
    format!(
        "\
         agent:\n  name: device-grant\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n\
         {store}\
         a2a:\n  listen: {listen}\n  bearer: \"{{{{secret:AGENTD_DG_OPS}}}}\"\n\
         \x20 device_grant:\n    enabled: true\n    scopes: [user, operator]\n\
         \x20 events:\n    enabled: true\n\
         {a2a_extra}\
         workflows:\n\
         \x20 - name: waiter\n    steps:\n      s: {{kind: manual}}\n      w: {{kind: wait, on: signal, signal: go, depends_on: [s]}}\n      f: {{kind: finish, depends_on: [w]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  audit:\n    sink: [log]\n",
        play = playbook()
    )
}

const MEMORY: &str = "store:\n  kind: memory\n";

/// A user-role rule holding every grant a user can hold.
const CI_RULE: &str = "\x20 principals:\n\
     \x20   - id: ci\n\
     \x20     match: { bearer_ref: \"{{secret:AGENTD_DG_CI}}\" }\n\
     \x20     role: user\n\
     \x20     grants: [\"*\"]\n";

/// A loopback daemon on a fresh port: it and its authority.
fn loopback(a2a_extra: &str, store: &str) -> (Daemon, String) {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let daemon = spawn(
        &config(&format!("http://127.0.0.1:{port}"), a2a_extra, store),
        &addr,
    );
    (daemon, addr)
}

// ---- the grant, from the client's side --------------------------------------

/// A form POST to `path`, as an OAuth client sends one.
fn form_post(addr: &str, path: &str, body: &str) -> HttpReply {
    let mut s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok();
    parse(&raw)
}

fn parse(raw: &[u8]) -> HttpReply {
    let text = String::from_utf8_lossy(raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    HttpReply {
        status,
        headers: lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect(),
        body: body.to_string(),
    }
}

/// Ask for a code: `(device_code, user_code, the whole answer)`.
fn device_code(addr: &str, form: &str) -> (String, String, Value) {
    let r = form_post(addr, "/oauth2/device_authorization", form);
    assert_eq!(r.status, 200, "{r:?}");
    let v = r.json();
    (
        v["device_code"].as_str().unwrap().to_string(),
        v["user_code"].as_str().unwrap().to_string(),
        v,
    )
}

fn poll(addr: &str, device_code: &str, client: &str) -> HttpReply {
    form_post(
        addr,
        "/oauth2/token",
        &format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}&client_id={client}"
        ),
    )
}

/// The document a command answered with: a read's Message data, or the JSON
/// DataPart artifact of the task the work completed as.
fn answer(v: &Value) -> Value {
    let r = &v["result"];
    if let Some(doc) = r["message"]["parts"][0].get("data") {
        return doc.clone();
    }
    r["task"]["artifacts"][0]["parts"][0]["data"].clone()
}

/// Run `op` as `bearer`, answered as soon as its task exists: a command that
/// starts work that waits (a parked run) must not hold the call open.
fn command_as(addr: &str, bearer: &str, op: &str, args: Value) -> Value {
    SendMessage::command(op, args)
        .bearer(bearer)
        .return_immediately()
        .post(addr)
}

/// Approve `user_code` as `name`, as the operator; the `approved` document.
fn approve(addr: &str, user_code: &str, name: &str) -> Value {
    let v = command_as(
        addr,
        OPS_TOKEN,
        "auth.device.approve",
        json!({"user_code": user_code, "as": name}),
    );
    assert!(
        v.get("error").is_none(),
        "approve {user_code} as {name}: {v}"
    );
    answer(&v)["approved"].clone()
}

/// A session approved as `name`: `(token, sid)`.
fn session(addr: &str, name: &str) -> (String, String) {
    let before = sids(addr);
    let (dc, uc, _) = device_code(addr, "client_id=e2e");
    approve(addr, &uc, name);
    let r = poll(addr, &dc, "e2e");
    assert_eq!(r.status, 200, "{r:?}");
    let token = r.json()["access_token"].as_str().unwrap().to_string();
    let sid = sids(addr)
        .into_iter()
        .find(|s| !before.contains(s))
        .expect("the new session is listed");
    (token, sid)
}

/// Every live session's sid, as the operator lists them.
fn sids(addr: &str) -> Vec<String> {
    let v = command_as(addr, OPS_TOKEN, "auth.sessions", json!({}));
    answer(&v)["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("auth.sessions: {v}"))
        .iter()
        .map(|s| s["sid"].as_str().unwrap().to_string())
        .collect()
}

fn revoke(addr: &str, target: Value) -> u64 {
    let v = command_as(addr, OPS_TOKEN, "auth.sessions.revoke", target);
    // A task's result crosses as a protobuf Struct, whose numbers are
    // doubles: `1` arrives as `1.0`.
    answer(&v)["revoked"]
        .as_f64()
        .map(|n| n as u64)
        .unwrap_or_else(|| panic!("revoke: {v}"))
}

/// The 401 a dead session token gets.
fn assert_revoked(r: &HttpReply) {
    assert_eq!(r.status, 401, "{r:?}");
    let www = r.header("www-authenticate").unwrap_or_default();
    assert!(www.contains("error=\"invalid_token\""), "{www}");
    assert!(www.contains("unknown, expired or revoked"), "{www}");
}

/// Open `SubscribeToTask` on `task` as `bearer` and read its first frame.
fn subscribe(addr: &str, bearer: &str, task: &str) -> BufReader<TcpStream> {
    let auth = format!("Bearer {bearer}");
    let mut s = a2a_open(
        addr,
        &rpc_body(9, "SubscribeToTask", json!({"id": task})),
        &[("Authorization", &auth)],
        Duration::from_secs(30),
    );
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            s.read_line(&mut line).unwrap() > 0,
            "the stream never opened"
        );
        assert!(!line.contains("\"error\""), "{line}");
        if line.starts_with("data:") {
            return s;
        }
    }
}

/// How long `stream` takes to end, reading and discarding what it sends.
fn time_to_end(stream: &mut BufReader<TcpStream>) -> Duration {
    let at = Instant::now();
    let mut line = String::new();
    loop {
        line.clear();
        match stream.read_line(&mut line) {
            Ok(0) => return at.elapsed(),
            Ok(_) => {}
            Err(e) => panic!("the stream did not end: {e}"),
        }
    }
}

// ---- the tests --------------------------------------------------------------

/// The whole grant, in nine steps: a code, a pending poll, a user refused
/// the approval (403), an approval without `as` refused (-32602), the
/// operator's approval, a single-use redemption, a session audited as its
/// principal with its sid, a feed stream ended by revocation, and the dead
/// token's 401.
#[test]
fn device_grant_end_to_end() {
    let (mut daemon, addr) = loopback(CI_RULE, MEMORY);

    // 1. A code, pointing at the listener's own verification page.
    let (dc, uc, v) = device_code(&addr, "client_id=tui");
    assert_eq!(
        v["verification_uri"],
        format!("http://{addr}/oauth2/device"),
        "{v}"
    );
    assert_eq!(v["interval"], 5);
    let pending = command_as(&addr, OPS_TOKEN, "auth.device.pending", json!({}));
    assert_eq!(answer(&pending)["pending"][0]["user_code"], uc, "{pending}");
    // 2. Nobody has approved it.
    let r = poll(&addr, &dc, "tui");
    assert_eq!(
        (r.status, r.json()["error"].clone()),
        (400, json!("authorization_pending"))
    );
    // 3. A user holding every grant may not approve anything.
    let r = SendMessage::command("auth.device.approve", json!({"user_code": uc, "as": "ci"}))
        .bearer(CI_TOKEN)
        .post_raw(&addr);
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(r.json()["error"]["code"], -31403);
    // 4. `as` is required.
    let v = command_as(
        &addr,
        OPS_TOKEN,
        "auth.device.approve",
        json!({"user_code": uc}),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    // 5. The operator approves it as alice.
    let approved = approve(&addr, &uc, "alice");
    assert_eq!(approved["principal"], "user:alice", "{approved}");
    assert_eq!(approved["existing"], false);
    assert_eq!(approved["client_id"], "tui");
    // 6. Redeemed once — after the interval the last poll set.
    std::thread::sleep(Duration::from_millis(5100));
    let r = poll(&addr, &dc, "tui");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let token = r.json()["access_token"].as_str().unwrap().to_string();
    assert!(token.starts_with("agentd_at_"));
    let again = poll(&addr, &dc, "tui");
    assert_eq!(
        again.json()["error"],
        "invalid_grant",
        "the code is single-use"
    );
    // 7. The token is user:alice, and the audit names it with its sid.
    let status = command_as(&addr, &token, "status", json!({}));
    assert!(status.get("error").is_none(), "{status}");
    let (_, sid) = {
        let listed = command_as(&addr, OPS_TOKEN, "auth.sessions", json!({}));
        let s = answer(&listed)["sessions"][0].clone();
        assert_eq!(s["principal"], "user:alice", "{listed}");
        assert_eq!(s["name"], "alice");
        assert_eq!(s["kind"], "device");
        (token.clone(), s["sid"].as_str().unwrap().to_string())
    };
    let line = daemon.wait_log(&["\"audit\"", "\"user:alice\"", &format!("\"sid\":\"{sid}\"")]);
    assert!(!line.contains(&token), "a token reached the log: {line}");
    // A refusal of the session is logged with its sid too: siblings share
    // the principal id, and only the sid says which of them was refused.
    let refused = SendMessage::command("config", json!({}))
        .bearer(&token)
        .post_raw(&addr);
    assert_eq!(refused.status, 403, "{refused:?}");
    daemon.wait_log(&[
        "\"a2a.denied\"",
        "\"user:alice\"",
        &format!("\"sid\":\"{sid}\""),
    ]);
    // 8. A feed stream the session holds ends with goodbye{revoked}.
    let auth = format!("Bearer {token}");
    let mut feed = a2a_open(
        &addr,
        &rpc_body(77, common::feed_method(), json!({"fromSeq": 0})),
        &[
            ("Authorization", &auth),
            ("A2A-Extensions", &common::feed_extensions()),
        ],
        Duration::from_secs(10),
    );
    let mut hello = String::new();
    while !hello.contains("hello") {
        hello.clear();
        assert!(feed.read_line(&mut hello).unwrap() > 0, "no hello");
    }
    assert_eq!(revoke(&addr, json!({"sid": sid})), 1);
    let mut goodbye = None;
    common::read_frames(&mut feed, |v| {
        if v["result"].get("goodbye").is_some() {
            goodbye = Some(v["result"]["goodbye"].clone());
            return false;
        }
        true
    });
    assert_eq!(goodbye.expect("a goodbye")["reason"], "revoked");
    daemon.wait_log(&["\"auth.session.revoked\"", &sid]);
    // 9. The token is dead.
    let r = SendMessage::command("status", json!({}))
        .bearer(&token)
        .post_raw(&addr);
    assert_revoked(&r);
    assert!(daemon.alive(), "{}", daemon.stderr());
}

/// Revocation reaches what a2a-rs is already serving: a session-token
/// `SubscribeToTask` on a WORKING task closes within 200 ms of the revoke,
/// and a blocking `SendMessage` still waiting on its turn is answered with
/// the dead token's 401.
#[test]
fn revocation_ends_sdk_streams() {
    let (mut daemon, addr) = loopback("", MEMORY);
    let (token, sid) = session(&addr, "alice");
    let started = SendMessage::text("SLOWPLEASE one")
        .bearer(&token)
        .return_immediately()
        .result(&addr);
    let task = started["task"]["id"].as_str().unwrap().to_string();
    let mut stream = subscribe(&addr, &token, &task);

    let (addr2, token2) = (addr.clone(), token.clone());
    let blocked = std::thread::spawn(move || {
        SendMessage::text("SLOWPLEASE two")
            .bearer(&token2)
            .post_raw(&addr2)
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(revoke(&addr, json!({"sid": sid})), 1);
    let took = time_to_end(&mut stream);
    assert!(
        took <= Duration::from_millis(200),
        "the stream outlived its session by {took:?}"
    );
    let r = blocked.join().unwrap();
    assert_revoked(&r);
    assert!(daemon.alive(), "{}", daemon.stderr());
}

/// Two sessions approved as alice are one principal, but each is revoked by
/// its own sid: ending the first closes the first's stream and leaves the
/// second's open, and the second keeps working.
#[test]
fn revoking_one_session_leaves_its_siblings() {
    let (mut daemon, addr) = loopback("", MEMORY);
    let (one, one_sid) = session(&addr, "alice");
    let (two, _) = session(&addr, "alice");
    let run = command_as(&addr, &one, "workflow.run", json!({"workflow": "waiter"}));
    let task = run["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("alice runs the waiter: {run}"))
        .to_string();
    let mut first = subscribe(&addr, &one, &task);
    let mut second = subscribe(&addr, &two, &task);
    second
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(600)))
        .unwrap();

    assert_eq!(revoke(&addr, json!({"sid": one_sid})), 1);
    let took = time_to_end(&mut first);
    assert!(took <= Duration::from_millis(200), "{took:?}");
    let mut line = String::new();
    match second.read_line(&mut line) {
        Err(e) => assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "{e}"
        ),
        Ok(n) => assert!(n > 0, "the sibling's stream ended with the revoked one"),
    }
    let v = command_as(&addr, &two, "workflow.status", json!({}));
    assert!(v.get("error").is_none(), "the sibling still works: {v}");
    let r = SendMessage::command("status", json!({}))
        .bearer(&one)
        .post_raw(&addr);
    assert_revoked(&r);
    assert!(daemon.alive(), "{}", daemon.stderr());
}

/// Ownership is by name, not by session: alice's run and conversation are
/// read and continued by a later session approved as alice after the first
/// is revoked, and a session approved as bob is told neither exists.
#[test]
fn ownership_survives_re_login() {
    let (mut daemon, addr) = loopback("", MEMORY);
    let (first, first_sid) = session(&addr, "alice");
    let run = command_as(&addr, &first, "workflow.run", json!({"workflow": "waiter"}));
    assert!(run.get("error").is_none(), "{run}");
    let runs = command_as(&addr, &first, "workflow.status", json!({}));
    let run_id = answer(&runs)["runs"][0]["run"]
        .as_str()
        .unwrap_or_else(|| panic!("alice's run: {runs}"))
        .to_string();
    let said = SendMessage::text("hello").bearer(&first).result(&addr);
    let ctx = said["task"]["contextId"].as_str().unwrap().to_string();
    assert_eq!(revoke(&addr, json!({"sid": first_sid})), 1);

    let (again, _) = session(&addr, "alice");
    let status = command_as(&addr, &again, "workflow.status", json!({"run": run_id}));
    assert_eq!(answer(&status)["runs"][0]["run"], run_id, "{status}");
    let plan = command_as(&addr, &again, "plan.get", json!({"id": ctx}));
    assert!(
        plan.get("error").is_none(),
        "alice reads her conversation: {plan}"
    );
    let continued = SendMessage::text("and again")
        .context(&ctx)
        .bearer(&again)
        .result(&addr);
    assert_eq!(continued["task"]["contextId"], ctx.as_str());
    assert_eq!(continued["task"]["status"]["state"], "TASK_STATE_COMPLETED");

    let (bob, _) = session(&addr, "bob");
    let v = command_as(&addr, &bob, "workflow.status", json!({"run": run_id}));
    assert_eq!(v["error"]["code"], -32001, "{v}");
    let v = command_as(&addr, &bob, "plan.get", json!({"id": ctx}));
    assert_eq!(v["error"]["code"], -32001, "{v}");
    assert!(daemon.alive(), "{}", daemon.stderr());
}

/// A signed-in session is served through a flood of junk bearers from its own
/// source.
///
/// The launched console and every device on the host present their session
/// from 127.0.0.1, which any local process can push over the failure limit.
/// Past it, a configured bearer is still refused 429 unchecked — it may be a
/// weak secret, and checking it would be an oracle — and so is a session
/// token that names nobody; a live session is checked and served, because a
/// 256-bit token agentd minted gives a guesser nothing to learn from
/// "valid, or 429".
#[test]
fn a_live_session_is_served_past_the_failure_limit() {
    let (mut daemon, addr) = loopback("", MEMORY);
    let (token, _) = session(&addr, "alice");
    let body = rpc_body(1, "ListTasks", json!({}));
    let bearer = |t: &str| format!("Bearer {t}");

    for n in 1..=21 {
        let r = common::a2a_post(&addr, &body, &[("Authorization", "Bearer guess")]);
        assert_eq!(r.status, 401, "junk bearer #{n}: {r:?}");
    }
    let limited = common::a2a_post(&addr, &body, &[("Authorization", "Bearer guess")]);
    assert_eq!(limited.status, 429, "the 22nd failure: {limited:?}");

    let served = common::a2a_post(&addr, &body, &[("Authorization", &bearer(&token))]);
    assert_eq!(
        served.status, 200,
        "the signed-in console was locked out: {served:?}"
    );
    assert!(served.json().get("error").is_none(), "{served:?}");

    let forged = format!("agentd_at_{}", "0".repeat(64));
    let r = common::a2a_post(&addr, &body, &[("Authorization", &bearer(&forged))]);
    assert_eq!(
        r.status, 429,
        "a session token naming nobody is refused unchecked: {r:?}"
    );
    let r = common::a2a_post(&addr, &body, &[("Authorization", &bearer(OPS_TOKEN))]);
    assert_eq!(
        r.status, 429,
        "a configured bearer is refused unchecked: {r:?}"
    );
    assert!(daemon.alive(), "{}", daemon.stderr());
}

/// `as` is required and well formed, never a reserved name and never a name
/// a configured rule holds — of any role, in any case; a second approval of
/// one name says so.
#[test]
fn approve_names_are_required_and_well_formed() {
    // An agent-role rule, spelled with capitals: it is `agent:Peer-Bot`, so
    // only the check against the rules in force stands between it and a
    // device approved as `peer-bot`.
    let rules = format!(
        "{CI_RULE}\
         \x20   - id: Peer-Bot\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_DG_PEER}}}}\" }}\n\
         \x20     role: agent\n"
    );
    let (mut daemon, addr) = loopback(&rules, MEMORY);
    let (_, uc, _) = device_code(&addr, "client_id=e2e");
    let long = "a".repeat(65);
    let mut bad: Vec<Value> = vec![
        json!({"user_code": uc}),
        json!({"user_code": uc, "as": "Alice"}),
        json!({"user_code": uc, "as": "-a"}),
        json!({"user_code": uc, "as": "a b"}),
        json!({"user_code": uc, "as": long}),
    ];
    for r in agentd::runtime::surface::RESERVED_APPROVAL_NAMES {
        bad.push(json!({"user_code": uc, "as": r}));
    }
    for args in &bad {
        let v = command_as(&addr, OPS_TOKEN, "auth.device.approve", args.clone());
        assert_eq!(v["error"]["code"], -32602, "{args}: {v}");
    }
    for (name, rule) in [("ci", "ci"), ("peer-bot", "Peer-Bot")] {
        let v = command_as(
            &addr,
            OPS_TOKEN,
            "auth.device.approve",
            json!({"user_code": uc, "as": name}),
        );
        assert_eq!(v["error"]["code"], -32602, "{name}: {v}");
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("principal rule") && msg.contains(&format!("id {rule})")),
            "the refusal names the rule: {v}"
        );
    }
    // Nothing above approved it.
    let pending = command_as(&addr, OPS_TOKEN, "auth.device.pending", json!({}));
    assert_eq!(answer(&pending)["pending"][0]["user_code"], uc, "{pending}");

    assert_eq!(approve(&addr, &uc, "alice")["existing"], false);
    let (_, uc2, _) = device_code(&addr, "client_id=e2e");
    assert_eq!(approve(&addr, &uc2, "alice")["existing"], true);
    assert!(daemon.alive(), "{}", daemon.stderr());
}

/// On a wildcard bind a loopback peer is somebody a same-host proxy relayed,
/// not the operator: an uncredentialed request from 127.0.0.1 is a 401. The
/// operator approves codes with `a2a.bearer`, over TLS, and the issuer is the
/// advertised origin.
#[test]
fn wildcard_listener_has_no_loopback_operator() {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/../net/tests/fixtures");
    let port = common::free_port();
    let origin = format!("https://localhost:{port}");
    let yaml = config(&format!("https://0.0.0.0:{port}"), "", MEMORY).replace(
        "  bearer:",
        &format!(
            "  url: {origin}\n  tls:\n    cert: {fixtures}/server.pem\n    key: {fixtures}/server.key\n  bearer:"
        ),
    );
    let addr = format!("127.0.0.1:{port}");
    let mut daemon = spawn(&yaml, &addr);
    let ca = std::fs::read(format!("{fixtures}/ca.pem")).unwrap();
    let https = |method: &str, path: &str, headers: &[(&str, &str)], body: &str| {
        let tcp = TcpStream::connect(&addr).unwrap();
        let mut tls = agentd::net::tls::connect_with_ca(tcp, "localhost", &ca, None)
            .unwrap_or_else(|e| panic!("TLS to {addr}: {e}"));
        agentd::net::http::send(
            &mut tls,
            "localhost",
            method,
            path,
            headers,
            body.as_bytes(),
        )
        .unwrap_or_else(|e| panic!("{method} {path}: {e}"))
    };
    let a2a = |body: &str, headers: &[(&str, &str)]| {
        let mut h = vec![
            ("Content-Type", "application/json"),
            ("A2A-Version", common::A2A_VERSION),
        ];
        h.extend_from_slice(headers);
        https("POST", "/", &h, body)
    };
    let form = [("Content-Type", "application/x-www-form-urlencoded")];

    let r = a2a(&rpc_body(1, "ListTasks", json!({})), &[]);
    assert_eq!(r.status, 401, "{}", r.body_str());

    let meta = https("GET", "/.well-known/oauth-authorization-server", &[], "");
    let meta: Value = serde_json::from_slice(&meta.body).unwrap();
    assert_eq!(meta["issuer"], origin.as_str(), "{meta}");

    let r = https(
        "POST",
        "/oauth2/device_authorization",
        &form,
        "client_id=e2e",
    );
    let code: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(code["verification_uri"], format!("{origin}/oauth2/device"));
    let approval = SendMessage::command(
        "auth.device.approve",
        json!({"user_code": code["user_code"], "as": "alice"}),
    )
    .bearer(OPS_TOKEN);
    let headers = approval.headers();
    let headers: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let r = a2a(&approval.body(2), &headers);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(answer(&v)["approved"]["principal"], "user:alice", "{v}");

    let r = https(
        "POST",
        "/oauth2/token",
        &form,
        &format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={}&client_id=e2e",
            code["device_code"].as_str().unwrap()
        ),
    );
    assert_eq!(r.status, 200, "{}", r.body_str());
    let token: Value = serde_json::from_slice(&r.body).unwrap();
    let auth = format!("Bearer {}", token["access_token"].as_str().unwrap());
    let r = a2a(
        &rpc_body(3, "ListTasks", json!({})),
        &[("Authorization", &auth)],
    );
    assert_eq!(r.status, 200, "{}", r.body_str());
    assert!(daemon.alive(), "{}", daemon.stderr());
}

/// agentd's own OAuth client — the one `agentd login` drives against other
/// authorization servers — signs in against agentd's device server: RFC 8414
/// discovery, the device authorization (with a parameter this server does
/// not know, which it ignores), and the poll.
#[test]
#[cfg(feature = "oauth")]
fn agentd_oauth_client_interoperates_with_its_own_device_server() {
    use agentd::auth::oauth2::{
        OAuth2Params, PollOutcome, discover, poll_device_once, start_device,
    };
    let (mut daemon, addr) = loopback("", MEMORY);
    let t = Duration::from_secs(10);
    let found = discover(&format!("http://{addr}"), t).expect("discovery");
    assert_eq!(
        found.issuer.as_deref(),
        Some(format!("http://{addr}").as_str())
    );
    let params = OAuth2Params {
        token_url: found.token_endpoint.expect("a token endpoint"),
        device_authorization_url: found.device_authorization_endpoint,
        authorization_url: None,
        client_id: "agentd-cli".into(),
        client_secret: None,
        scopes: vec![],
        audience: Some("https://elsewhere.example".into()),
    };
    let device = start_device(&params, t).expect("a code");
    assert_eq!(device.interval, 5);
    match poll_device_once(&params, &device.device_code, t).expect("a poll") {
        PollOutcome::Pending => {}
        other => panic!("before the approval: {other:?}"),
    }
    approve(&addr, &device.user_code, "cli-user");
    std::thread::sleep(Duration::from_millis(5100));
    let tokens = match poll_device_once(&params, &device.device_code, t).expect("a poll") {
        PollOutcome::Token(t) => t,
        other => panic!("after the approval: {other:?}"),
    };
    assert_eq!(tokens.token_type.as_deref(), Some("Bearer"));
    assert_eq!(tokens.scope.as_deref(), Some("user"));
    assert!(tokens.refresh_token.is_none());
    let v = command_as(&addr, &tokens.access_token, "status", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert!(daemon.alive(), "{}", daemon.stderr());
}

/// A device name and a configured rule id never share a principal, whichever
/// claimed the name first and whatever happened in between — the registry is
/// durable, so the claim outlives the sessions and the restart.
#[test]
fn a_device_name_and_a_rule_id_never_share_a_principal() {
    let dir = common::unique_path("device-grant-store", "dir");
    let store = format!(
        "store:\n  kind: file\n  file:\n    path: {dir}\n  checkpoint:\n    debounce_ms: 0\n"
    );
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let listen = format!("http://127.0.0.1:{port}");
    let rule = |id: &str, role: &str, secret: &str| {
        format!(
            "\x20 principals:\n\
             \x20   - id: {id}\n\
             \x20     match: {{ bearer_ref: \"{{{{secret:{secret}}}}}\" }}\n\
             \x20     role: {role}\n"
        )
    };

    // (a) alice is approved; after a restart a user-role rule `alice` is a
    // configuration error that names her.
    let mut d = spawn(&config(&listen, "", &store), &addr);
    let (_, uc, _) = device_code(&addr, "client_id=e2e");
    approve(&addr, &uc, "alice");
    d.stop();
    drop(d);
    let cfg = common::unique_path("device-grant", "yaml");
    std::fs::write(
        &cfg,
        config(&listen, &rule("alice", "user", "AGENTD_DG_CI"), &store),
    )
    .unwrap();
    let mut refused = start(&cfg);
    // Bounded: a daemon that wrongly starts must fail this, not hang it.
    let deadline = Instant::now() + Duration::from_secs(15);
    let code = loop {
        if let Some(status) = refused.child.try_wait().unwrap() {
            break status.code();
        }
        assert!(
            Instant::now() < deadline,
            "a user-role rule naming an approved device started:\n{}",
            refused.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(code, Some(2), "startup must refuse:\n{}", refused.stderr());
    let log = refused.stderr();
    assert!(log.contains("\"identity.collision\""), "{log}");
    assert!(log.contains("alice"), "{log}");
    drop(refused);

    // (b) a rule ci-bot, then a restart without it: its name is still its.
    let mut d = spawn(
        &config(&listen, &rule("ci-bot", "user", "AGENTD_DG_CI"), &store),
        &addr,
    );
    d.stop();
    drop(d);
    let mut d = spawn(&config(&listen, "", &store), &addr);
    let (_, uc, _) = device_code(&addr, "client_id=e2e");
    let v = command_as(
        &addr,
        OPS_TOKEN,
        "auth.device.approve",
        json!({"user_code": uc, "as": "ci-bot"}),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("was a configured principal id"),
        "{v}"
    );

    // (c) a reload may not declare alice as a user either; an agent may be.
    #[cfg(feature = "hot-reload")]
    {
        let before = common::a2a_post(&addr, &rpc_body(1, "ListTasks", json!({})), &[]);
        assert_eq!(before.status, 401);
        std::fs::write(
            &d.cfg,
            config(&listen, &rule("alice", "user", "AGENTD_DG_CI"), &store),
        )
        .unwrap();
        unsafe { libc::kill(d.child.id() as i32, libc::SIGHUP) };
        d.wait_log(&["\"config.reload.principals\""]);
        d.wait_log(&["\"identity.collision\"", "user:alice"]);
        let as_alice = common::rpc_as(&addr, CI_TOKEN, 2, "ListTasks", json!({}));
        assert_eq!(
            as_alice["error"]["code"], -31401,
            "the refused rule is not in force: {as_alice}"
        );
        let after = common::a2a_post(&addr, &rpc_body(3, "ListTasks", json!({})), &[]);
        assert_eq!(after.status, before.status, "answered as before");

        std::fs::write(
            &d.cfg,
            config(&listen, &rule("alice", "agent", "AGENTD_DG_PEER"), &store),
        )
        .unwrap();
        unsafe { libc::kill(d.child.id() as i32, libc::SIGHUP) };
        d.wait_log(&["\"config.reloaded\"", "a2a.principals"]);
        let as_agent = common::rpc_as(&addr, PEER_TOKEN, 4, "ListTasks", json!({}));
        assert!(
            as_agent.get("error").is_none(),
            "an agent-role alice is accepted: {as_agent}"
        );
    }
    assert!(d.alive(), "{}", d.stderr());
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The display clients check an approval name before sending it, with a
/// hand copy of the daemon's pattern. It must be the daemon's.
#[test]
fn the_clients_approval_name_is_the_daemons() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../interface/src/client/composer.ts"
    );
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("{path}: {e} — this guard is monorepo-only and expects the interface client beside the crates")
    });
    let decl = "export const APPROVAL_NAME = /";
    let rest = &src[src.find(decl).expect("composer.ts declares APPROVAL_NAME") + decl.len()..];
    let pattern = &rest[..rest.find("/;").expect("a regex literal")];
    assert_eq!(
        pattern,
        agentd::runtime::surface::APPROVAL_NAME_PATTERN,
        "interface/src/client/composer.ts APPROVAL_NAME must be the daemon's approval-name pattern"
    );
}

// ---- the launch grant -------------------------------------------------------

/// What the launch tests hand the in-process daemon they start: its config,
/// where to publish the launch code, and — for a web UI's launch — the
/// origin the slot carries.
const LAUNCH_CFG_ENV: &str = "AGENTD_E2E_LAUNCH_CFG";
const LAUNCH_CODE_ENV: &str = "AGENTD_E2E_LAUNCH_CODE";
const LAUNCH_ORIGIN_ENV: &str = "AGENTD_E2E_LAUNCH_ORIGIN";

/// Not a test of its own: the daemon the launch tests start, as this test
/// binary re-run with `--ignored --exact`.
///
/// A launch slot is installed only in the process that runs the daemon —
/// nothing on the command line or in a file can install one — so the daemon
/// runs here, in-process, the way the launcher runs it, and in a process of
/// its own so its child reaper and signal handlers are nobody else's. Built as
/// a test, it is a debug build: the feed's schema assertion is compiled in.
#[test]
#[ignore = "the in-process daemon the launch tests start"]
fn launch_daemon() {
    use agentd::a2a::oauth::{LaunchBind, LaunchSlot};
    let (Ok(cfg), Ok(code_path)) = (
        std::env::var(LAUNCH_CFG_ENV),
        std::env::var(LAUNCH_CODE_ENV),
    ) else {
        return;
    };
    let args = vec!["--config".to_string(), cfg];
    let env: Vec<(String, String)> = std::env::vars().collect();
    let (loaded, _) =
        agentd::config::settings::load(&args, &env).unwrap_or_else(|e| panic!("config: {e:?}"));
    let origin = std::env::var(LAUNCH_ORIGIN_ENV).ok();
    let slot = std::sync::Arc::new(LaunchSlot::new(origin.as_deref()).unwrap());
    let code = match &origin {
        Some(o) => slot.issue(LaunchBind::Origin(o.clone()), "agentd-ui"),
        None => slot.issue(LaunchBind::NoOrigin, "agentd-tui"),
    }
    .unwrap();
    // Published whole: the test reads it once the file exists.
    let staged = format!("{code_path}.tmp");
    std::fs::write(&staged, &code).unwrap();
    std::fs::rename(&staged, &code_path).unwrap();
    let rc = agentd::runtime::run_with(
        &loaded,
        &args,
        &env,
        agentd::runtime::RunOpts { launch: Some(slot) },
    );
    let _ = std::fs::remove_file(&code_path);
    std::process::exit(rc);
}

/// A launch exchange pushes the feed's `auth` `launch` event — the sid, the
/// client and the operator scope the FeedKind contract requires — and an
/// operator's subscriber receives it: the debug build's schema assertion in
/// the feed's push did not fire. The session is an operator that lists as a
/// launch, and neither the code nor the token reaches the log.
/// Start [`launch_daemon`] on a fresh loopback port with `a2a_extra` under
/// `a2a:`, its slot carrying `origin` (a web UI's launch) or none (a terminal
/// client's): the daemon, its authority, and the launch code it minted.
fn launched(a2a_extra: &str, origin: Option<&str>) -> (Daemon, String, String) {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = common::unique_path("launch-daemon", "yaml");
    std::fs::write(
        &cfg,
        config(&format!("http://127.0.0.1:{port}"), a2a_extra, MEMORY),
    )
    .unwrap();
    let code_path = common::unique_path("launch-code", "txt");
    let stderr_path = common::unique_path("launch-daemon", "log");
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "launch_daemon",
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(LAUNCH_CFG_ENV, &cfg)
    .env(LAUNCH_CODE_ENV, &code_path)
    .env_remove(LAUNCH_ORIGIN_ENV)
    .env("AGENTD_DG_OPS", OPS_TOKEN)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()));
    if let Some(o) = origin {
        cmd.env(LAUNCH_ORIGIN_ENV, o);
    }
    let child = cmd.spawn().expect("spawn the in-process daemon");
    let mut daemon = Daemon {
        child,
        stderr_path,
        cfg,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(&addr).is_err() || !std::path::Path::new(&code_path).exists() {
        assert!(
            daemon.alive() && Instant::now() < deadline,
            "the launched daemon never came up:\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let code = std::fs::read_to_string(&code_path).unwrap();
    (daemon, addr, code)
}

#[test]
fn the_launch_event_matches_the_feed_schema() {
    let (mut daemon, addr, code) = launched("", None);

    // An operator watches the feed from before the exchange.
    let ops = format!("Bearer {OPS_TOKEN}");
    let mut feed = a2a_open(
        &addr,
        &rpc_body(77, common::feed_method(), json!({"fromSeq": 0})),
        &[
            ("Authorization", &ops),
            ("A2A-Extensions", &common::feed_extensions()),
        ],
        Duration::from_secs(10),
    );
    let mut hello = String::new();
    while !hello.contains("hello") {
        hello.clear();
        assert!(feed.read_line(&mut hello).unwrap() > 0, "no hello");
    }

    let grant = agentd::runtime::surface::launch::LAUNCH_GRANT_TYPE
        .replace(':', "%3A")
        .replace('/', "%2F");
    let r = form_post(
        &addr,
        "/oauth2/token",
        &format!("grant_type={grant}&code={code}&client_id=agentd-tui"),
    );
    assert_eq!(r.status, 200, "{r:?}\n{}", daemon.stderr());
    let v = r.json();
    assert_eq!(v["scope"], "operator", "{v}");
    assert!(v.get("expires_in").is_none(), "a terminal session: {v}");
    let token = v["access_token"].as_str().unwrap().to_string();

    let mut launch = None;
    common::read_frames(&mut feed, |f| {
        let ev = &f["result"]["event"];
        if ev["kind"] == "auth" && ev["data"]["event"] == "launch" {
            launch = Some(ev["data"].clone());
            return false;
        }
        true
    });
    let launch = launch.unwrap_or_else(|| {
        panic!(
            "no launch event reached the operator's feed:\n{}",
            daemon.stderr()
        )
    });
    let sid = launch["sid"].as_str().expect("the sid").to_string();
    assert!(sid.starts_with("ls_"), "{launch}");
    assert_eq!(
        launch,
        json!({"event": "launch", "sid": sid, "client_id": "agentd-tui", "scope": "operator"})
    );

    // The session is the operator's, and lists as a launch.
    let listed = command_as(&addr, &token, "auth.sessions", json!({}));
    let rows = answer(&listed)["sessions"].clone();
    let row = rows
        .as_array()
        .and_then(|r| r.iter().find(|s| s["sid"] == sid.as_str()))
        .unwrap_or_else(|| panic!("the launch session is listed: {listed}"));
    assert_eq!(
        (&row["kind"], &row["approved_by"], &row["principal"]),
        (&json!("launch"), &json!("launcher"), &json!("operator"))
    );
    let line = daemon.wait_log(&["\"auth.launch.exchanged\"", &sid]);
    assert!(line.contains("\"via\":\"code\""), "{line}");
    let log = daemon.stderr();
    assert!(
        !log.contains(code.trim()),
        "the launch code reached the log"
    );
    assert!(!log.contains(&token), "the token reached the log");
    // Spent: a second exchange is refused.
    let again = form_post(
        &addr,
        "/oauth2/token",
        &format!("grant_type={grant}&code={code}&client_id=agentd-tui"),
    );
    assert_eq!(again.status, 400, "{again:?}");
    assert!(daemon.alive(), "{}", daemon.stderr());
}

/// A preflight of `POST /` from `origin`: the status.
#[cfg(feature = "hot-reload")]
fn preflight(addr: &str, origin: &str) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(
        s,
        "OPTIONS / HTTP/1.1\r\nHost: x\r\nOrigin: {origin}\r\n\
         Access-Control-Request-Method: POST\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok();
    raw.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

/// A reload that edits `a2a.cors.origins` swaps the configured origins and
/// keeps the UI the launcher started, which is in no file a reload re-reads.
/// Swapping in the file's list alone would lock the operator's own tab out
/// on the first CORS edit — and report the reload a success.
#[cfg(feature = "hot-reload")]
#[test]
fn a_cors_reload_keeps_the_launched_origin() {
    const UI: &str = "http://127.0.0.1:4555";
    const ONE: &str = "https://one.example";
    const TWO: &str = "https://two.example";
    let origins = |o: &str| format!("\x20 cors:\n    origins: [\"{o}\"]\n");
    let (mut daemon, addr, _code) = launched(&origins(ONE), Some(UI));
    assert_eq!(preflight(&addr, UI), 204, "{}", daemon.stderr());
    assert_eq!(preflight(&addr, ONE), 204);
    assert_eq!(preflight(&addr, TWO), 403);

    let listen = format!("http://{addr}");
    std::fs::write(&daemon.cfg, config(&listen, &origins(TWO), MEMORY)).unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    daemon.wait_log(&["\"config.reloaded\"", "a2a.cors.origins"]);
    assert_eq!(preflight(&addr, TWO), 204, "the edit is in force");
    assert_eq!(preflight(&addr, ONE), 403, "and the removal");
    assert_eq!(
        preflight(&addr, UI),
        204,
        "the launched UI is still admitted"
    );
    assert!(daemon.alive(), "{}", daemon.stderr());
}
