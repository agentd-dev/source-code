// SPDX-License-Identifier: AGPL-3.0-only
//! **Admission: who is let in, how often, and who can be locked out.**
//!
//! Three properties of the listener that only show from outside the process:
//!
//! * a principal's declared rate is ADMISSION — a request over it is answered
//!   429 at the door and reaches nothing, rather than being turned into a
//!   refused result by the runtime;
//! * failed authentications are limited per source, and only failures are
//!   counted: past the limit the source's bearers are refused unchecked, so a
//!   guesser is slowed to the refill rate rather than told which guess was
//!   right — while nothing a web page makes a browser send (a refused origin,
//!   an uncredentialed request) is counted, or logged line for line, so it
//!   can neither lock the operator's own console out nor fill its log;
//! * the posture follows the rules: a reload that gives a no-auth loopback
//!   daemon its first principal ends the implicit operator on the next
//!   request.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{HttpReply, SendMessage, a2a_post, rpc_as, rpc_body};

const OPS_TOKEN: &str = "admission-operator-bearer";
const CI_TOKEN: &str = "admission-ci-bearer";

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
    let pb = common::unique_path("admission-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("admission-mock-llm", "addr");
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
    cfg: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
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
        let _ = std::fs::remove_file(&self.cfg);
    }
}

/// Start a daemon on a free loopback port, with `a2a_extra` appended under
/// `a2a:`; returns it and the authority it serves.
fn spawn(llm: &str, a2a_extra: &str) -> (Daemon, String) {
    let port = common::free_port();
    let cfg = common::unique_path("admission", "yaml");
    std::fs::write(&cfg, config(llm, port, a2a_extra)).unwrap();
    let stderr_path = common::unique_path("admission-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_ADMISSION_OPS", OPS_TOKEN)
        .env("AGENTD_ADMISSION_CI", CI_TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");
    let daemon = Daemon {
        child,
        stderr_path,
        cfg,
    };
    let addr = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(&addr).is_err() {
        assert!(
            Instant::now() < deadline,
            "the listener never came up:\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    (daemon, addr)
}

fn config(llm: &str, port: u16, a2a_extra: &str) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: admission\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{a2a_extra}\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

/// The rule the `ci` bearer matches, as `a2a.principals` YAML.
fn ci_rule(extra: &str) -> String {
    format!(
        "\x20 principals:\n\
         \x20   - id: ci\n\
         \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_ADMISSION_CI}}}}\" }}\n\
         \x20     role: user\n{extra}"
    )
}

fn json_of(reply: &HttpReply) -> Value {
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.contains("application/json")),
        "not JSON: {reply:?}"
    );
    reply.json()
}

fn tasks_seen_by_operator(addr: &str) -> usize {
    let v = rpc_as(addr, OPS_TOKEN, 90, "ListTasks", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    v["result"]["tasks"].as_array().map_or(0, Vec::len)
}

/// A declared rate is a limit on arrivals, applied before the request reaches
/// the runtime: the refused request creates no task, and it is answered 429
/// with a `Retry-After` and the `RATE_LIMITED` reason rather than as a result.
/// Operators are exempt.
#[test]
fn rate_limits_are_admission_not_results() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "ok"}]}));
    let (mut daemon, addr) = spawn(
        &llm.uri,
        &format!(
            "\x20 bearer: \"{{{{secret:AGENTD_ADMISSION_OPS}}}}\"\n{}",
            ci_rule("\x20     quotas: { rate: \"2/60s\" }\n")
        ),
    );

    for n in 0..2 {
        let sent = SendMessage::text("hello").bearer(CI_TOKEN).post(&addr);
        assert!(sent.get("error").is_none(), "send {n}: {sent}");
    }
    assert_eq!(tasks_seen_by_operator(&addr), 2);

    let over = SendMessage::text("one too many")
        .bearer(CI_TOKEN)
        .post_raw(&addr);
    assert_eq!(over.status, 429, "{over:?}");
    let retry: u64 = over
        .header("retry-after")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no Retry-After: {over:?}"));
    assert!(retry >= 1, "{retry}");
    let v = json_of(&over);
    assert_eq!(v["error"]["code"], -32603, "{v}");
    assert_eq!(v["error"]["data"][0]["reason"], "RATE_LIMITED", "{v}");
    assert_eq!(
        v["error"]["data"][0]["metadata"]["retryAfterSeconds"],
        json!(retry.to_string()),
        "{v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("rate limit for user:ci")),
        "{v}"
    );

    // Nothing reached the runtime: no third task. And the operator, who has
    // just made several requests of its own, was never limited.
    for _ in 0..5 {
        assert_eq!(
            tasks_seen_by_operator(&addr),
            2,
            "the refused send made a task"
        );
    }

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
}

/// A source that keeps failing to authenticate is slowed down — every bearer
/// it presents, not just the wrong ones.
///
/// Twenty-one junk bearers are answered 401 and the twenty-second 429, with a
/// `Retry-After`. From then on the source's bearers are refused unchecked:
/// the VALID bearer is refused too, because a limiter that still checked it
/// would answer a right guess 200 and a wrong one 429 — no slower for the
/// guesser, and an oracle besides. A request presenting nothing is not
/// refused by it (401, not 429), and the refused bearers are not counted: one
/// refill later the source is under the limit and the valid bearer is served.
#[test]
fn auth_failures_are_limited_per_source() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (mut daemon, addr) = spawn(
        &llm.uri,
        "\x20 bearer: \"{{secret:AGENTD_ADMISSION_OPS}}\"\n",
    );
    let body = rpc_body(1, "ListTasks", json!({}));
    let junk = [("Authorization", "Bearer guess")];

    for n in 1..=21 {
        let r = a2a_post(&addr, &body, &junk);
        assert_eq!(r.status, 401, "junk bearer #{n}: {r:?}");
    }
    let limited = a2a_post(&addr, &body, &junk);
    assert_eq!(limited.status, 429, "the 22nd failure: {limited:?}");
    assert!(limited.header("retry-after").is_some(), "{limited:?}");
    let v = json_of(&limited);
    assert_eq!(v["error"]["data"][0]["reason"], "RATE_LIMITED", "{v}");

    let valid = [("Authorization", format!("Bearer {OPS_TOKEN}"))];
    let valid: Vec<(&str, &str)> = valid.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let right_guess = a2a_post(&addr, &body, &valid);
    assert_eq!(
        right_guess.status, 429,
        "a throttled source's right guess is refused like its wrong ones: {right_guess:?}"
    );
    let nothing = a2a_post(&addr, &body, &[]);
    assert_eq!(
        nothing.status, 401,
        "a request presenting nothing is never throttled: {nothing:?}"
    );

    // One failure drains per three seconds. Had the two refused bearers been
    // counted, the source would still be over after one refill.
    std::thread::sleep(Duration::from_millis(3300));
    let ok = rpc_as(&addr, OPS_TOKEN, 2, "ListTasks", json!({}));
    assert!(
        ok.get("error").is_none(),
        "one refill later the valid bearer is served: {ok}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
}

/// Nothing a web page can make a browser send locks the console out, or
/// fills the log.
///
/// A page on another site POSTs with its own `Origin` (refused at the origin
/// gate), and a page on the listed UI origin POSTs without a credential
/// (refused 401) — hundreds of each, from 127.0.0.1, the same source the
/// operator's terminal uses. Neither is counted: had either been, the source
/// would be over the limit and the terminal's next bearer — a wrong one
/// here, which the limiter DOES count — would be refused 429 unchecked rather
/// than answered 401, and the operator's own bearer refused after it. And
/// the two hundred 401s leave a line or two in the log, not two hundred.
#[test]
fn browsers_cannot_lock_out_the_local_console() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (mut daemon, addr) = spawn(
        &llm.uri,
        "\x20 bearer: \"{{secret:AGENTD_ADMISSION_OPS}}\"\n\
         \x20 cors:\n    origins: [\"http://127.0.0.1:4173\"]\n",
    );
    let body = rpc_body(1, "ListTasks", json!({}));

    for n in 0..200 {
        let r = a2a_post(&addr, &body, &[("Origin", "https://evil.example")]);
        assert_eq!(r.status, 403, "foreign origin #{n}: {r:?}");
    }
    for n in 0..200 {
        let r = a2a_post(&addr, &body, &[("Origin", "http://127.0.0.1:4173")]);
        assert_eq!(r.status, 401, "uncredentialed browser #{n}: {r:?}");
    }

    let typo = a2a_post(&addr, &body, &[("Authorization", "Bearer typo")]);
    assert_eq!(
        typo.status, 401,
        "the browsers' requests put the source over the limit: {typo:?}"
    );
    let console = rpc_as(&addr, OPS_TOKEN, 2, "ListTasks", json!({}));
    assert!(
        console.get("error").is_none(),
        "the console was locked out: {console}"
    );

    let log = daemon.stderr();
    let lines = log
        .lines()
        .filter(|l| l.contains("\"event\":\"a2a.denied\""))
        .count();
    assert!(
        lines <= 3,
        "two hundred free refusals wrote {lines} log lines:\n{log}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
}

/// A reload that gives a no-auth loopback daemon its first principal ends the
/// implicit operator at once.
///
/// The posture is part of the resolver the reload swaps, so there is no copy
/// taken at spawn for the reload to miss: the same uncredentialed request that
/// was the operator a moment ago is now asked for a credential, and the new
/// rule's bearer is served as the principal it declares.
#[test]
#[cfg(feature = "hot-reload")]
fn adding_a_principal_ends_the_implicit_operator() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (mut daemon, addr) = spawn(&llm.uri, "");
    let before = common::rpc(&addr, 1, "ListTasks", json!({}));
    assert!(
        before.get("error").is_none(),
        "loopback with nothing configured is the operator: {before}"
    );

    let port = addr.rsplit_once(':').unwrap().1.parse().unwrap();
    std::fs::write(&daemon.cfg, config(&llm.uri, port, &ci_rule(""))).unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    let deadline = Instant::now() + Duration::from_secs(10);
    let reloaded = loop {
        let log = daemon.stderr();
        if let Some(line) = log
            .lines()
            .find(|l| l.contains("\"event\":\"config.reloaded\""))
        {
            break line.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never reloaded:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        reloaded.contains("a2a.principals"),
        "the reload reports the principals changed: {reloaded}"
    );

    let after = a2a_post(&addr, &rpc_body(2, "ListTasks", json!({})), &[]);
    assert_eq!(
        after.status, 401,
        "the implicit operator survived: {after:?}"
    );
    assert_eq!(json_of(&after)["error"]["code"], -31401);

    let ci = rpc_as(&addr, CI_TOKEN, 3, "ListTasks", json!({}));
    assert!(
        ci.get("error").is_none(),
        "the new rule's bearer is served: {ci}"
    );
    // …as the principal it declares: an operator-only op is refused, and the
    // refusal names who asked.
    let config = SendMessage::command("config", json!({}))
        .bearer(CI_TOKEN)
        .post(&addr);
    assert_eq!(config["error"]["code"], -31403, "{config}");
    assert!(
        config["error"]["message"]
            .as_str()
            .is_some_and(|m| m.ends_with("for user:ci")),
        "{config}"
    );

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
}
