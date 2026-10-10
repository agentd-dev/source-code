// SPDX-License-Identifier: AGPL-3.0-only
//! MCP Events webhook delivery (RFC 0045 §5.6.3, SEP-3415), end to end over
//! HTTPS: the built-in mock subscribes agentd's webhook listener, verifies the
//! endpoint with a signed challenge, and POSTs signed occurrences — retrying
//! a refused one and abandoning it after its window, as a server does.
//!
//! - The handshake, a signed delivery taken into the stream, and what a third
//!   party gets (no echo, no event); an oversize body answered `413` unread.
//! - An outage longer than the server's retry window: the re-arm replays it.
//! - A refused delivery holds the cursor, so what was abandoned is replayed.
//! - A suspended subscription reported on refresh is re-armed from the cursor.
//! - The `gap` and `terminated` envelopes.
#![cfg(all(unix, feature = "mcp-events", feature = "a2a"))]

mod common;

use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const REVISION: &str = "2026-07-28";

fn fixtures() -> String {
    format!("{}/../net/tests/fixtures", env!("CARGO_MANIFEST_DIR"))
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// The mock's `MOCK_HOOK_<what>` lines, their JSON.
fn hook_lines(mock: &common::MockMcp, what: &str) -> Vec<Value> {
    let marker = format!("MOCK_HOOK_{what} ");
    mock.log()
        .lines()
        .filter_map(|l| l.strip_prefix(&marker))
        .filter_map(|j| serde_json::from_str(j).ok())
        .collect()
}

fn done(log: &str) -> Vec<Value> {
    events(log, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "on_tick")
        .collect()
}

fn outputs(log: &str) -> Vec<i64> {
    let mut out: Vec<i64> = done(log)
        .iter()
        .filter_map(|r| {
            r["output"]
                .as_i64()
                .or_else(|| r["output"].as_str()?.parse().ok())
        })
        .collect();
    out.sort();
    out
}

struct Daemon {
    child: Child,
    err_path: String,
}

impl Daemon {
    fn start(tag: &str, cfg_path: &str) -> Daemon {
        let err_path = common::unique_path(tag, "err");
        let err = std::fs::File::create(&err_path).expect("stderr file");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", cfg_path])
            .env("HOOK_AUTH", "Bearer hooks-test")
            .env("HOOK_AUTH_2", "Bearer hooks-test-2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn agentd");
        Daemon { child, err_path }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    fn wait_for(&self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let log = self.log();
            if pred(&log) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn armed(&self) -> usize {
        events(&self.log(), "mcp.events.armed")
            .iter()
            .filter(|e| e["mode"] == "webhook")
            .count()
    }

    fn stop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.err_path);
    }
}

/// [`Daemon::wait_for`], with the mock's side in the failure.
fn wait(daemon: &Daemon, mock: &common::MockMcp, what: &str, pred: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let log = daemon.log();
        if pred(&log) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}:\n{log}\n--- mock ---\n{}",
            mock.log()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn mock() -> common::MockMcp {
    let ca = format!("{}/ca.pem", fixtures());
    common::spawn_mock_mcp_with(REVISION, "mock://res", false, &["--hook-ca", &ca])
}

/// A webhook binding on `mock`'s `mock.tick` (topic t1), its listener on an
/// HTTPS loopback port with the test PKI, a file store in `dir`, and a
/// workflow that finishes with each occurrence's `n`. `extra` goes under the
/// binding.
fn config(mock: &common::MockMcp, port: u16, dir: &std::path::Path, extra: &str) -> String {
    let fx = fixtures();
    format!(
        "agent:\n  name: hooks\nstore:\n  kind: file\n  file: {{path: {}}}\nstreams:\n  ticks: {{}}\nwebhooks:\n  listen: https://127.0.0.1:{port}\n  url: https://127.0.0.1:{port}\n  tls: {{cert: {fx}/server.pem, key: {fx}/server.key}}\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}/mcp\n      headers: {{Authorization: \"{{{{secret:HOOK_AUTH}}}}\"}}\n      tags: {{\"*\": [sensitive]}}\n      events:\n        - name: mock.tick\n          arguments: {{topic: t1}}\n          delivery: [webhook]\n          into: {{stream: ticks, subject: tick}}\n{extra}workflows:\n  - name: on_tick\n    steps:\n      s: {{kind: stream, stream: ticks, subject: tick, inputs: {{n: \"{{{{ payload.data.payload.n }}}}\"}}}}\n      f: {{kind: finish, depends_on: [s], status: completed, output: \"{{{{ inputs.n }}}}\"}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n  log_content: true\n",
        dir.display(),
        mock.uri(),
    )
}

fn write(cfg: &str) -> String {
    let path = common::unique_path("hooks", "yaml");
    std::fs::write(&path, cfg).unwrap();
    path
}

#[test]
fn a_webhook_binding_is_verified_subscribed_and_takes_only_signed_deliveries() {
    let mock = mock();
    let dir = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let cfg = write(&config(&mock, port, dir.path(), ""));
    let daemon = Daemon::start("hooks-happy", &cfg);
    daemon.wait_for("the binding armed by webhook", |_| daemon.armed() == 1);
    assert!(mock.log().contains("MOCK_HOOK_VERIFIED"), "{}", mock.log());
    let sub = &hook_lines(&mock, "SUBSCRIBE")[0];
    assert_eq!(
        sub["url"],
        format!("https://127.0.0.1:{port}/hooks/_mcp/")
            + sub["url"].as_str().unwrap().rsplit('/').next().unwrap()
    );
    // A signed delivery is appended and starts a run; 200 only once kept.
    mock.control("mock.event", json!({"topic": "t1"}));
    daemon.wait_for("its run", |l| outputs(l) == [1]);
    assert_eq!(
        hook_lines(&mock, "DELIVERED")[0]["status"],
        200,
        "{}",
        mock.log()
    );
    // A third party: neither its delivery nor its challenge is taken.
    let forged = mock.control("mock.hook_forge", json!({}));
    let answers = forged["answers"][0].clone();
    assert_eq!(answers["delivery"], 401, "{forged}");
    assert_eq!(answers["challenge"], 401, "{forged}");
    // Over the size profile: refused before it is read, final.
    mock.control("mock.event", json!({"topic": "t1", "oversize": true}));
    mock.control("mock.event", json!({"topic": "t1"}));
    daemon.wait_for("the next run", |l| outputs(l) == [1, 3]);
    let rejected = hook_lines(&mock, "REJECTED");
    assert_eq!(rejected.len(), 1, "{}", mock.log());
    assert_eq!(rejected[0]["status"], 413);
    let log = daemon.log();
    assert!(
        events(&log, "mcp.events.hook.denied")
            .iter()
            .all(|e| e["reason"] == "bad_signature"),
        "{log}"
    );
    assert_eq!(outputs(&log), [1, 3], "the forged event never ran:\n{log}");
}

#[test]
fn an_outage_longer_than_the_retry_window_is_replayed_by_the_rearm() {
    let mock = mock();
    mock.control("mock.hook_policy", json!({"attempts": 2, "window_ms": 300}));
    let dir = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let cfg = write(&config(&mock, port, dir.path(), ""));
    let mut first = Daemon::start("hooks-outage-1", &cfg);
    first.wait_for("armed", |_| first.armed() == 1);
    mock.control("mock.event", json!({"topic": "t1"}));
    first.wait_for("the first run", |l| outputs(l) == [1]);
    first.stop();
    // Down: the server gives the delivery up, and its watermark moves past.
    mock.control("mock.event", json!({"topic": "t1"}));
    let deadline = Instant::now() + Duration::from_secs(30);
    while hook_lines(&mock, "ABANDONED").is_empty() {
        assert!(Instant::now() < deadline, "{}", mock.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    // Back: a fresh subscription from the stored cursor replays it.
    let second = Daemon::start("hooks-outage-2", &cfg);
    second.wait_for("the abandoned occurrence's run", |l| outputs(l) == [2]);
    assert!(
        hook_lines(&mock, "UNSUBSCRIBE")
            .iter()
            .any(|u| u["found"] == true),
        "an arm is a fresh subscription:\n{}",
        mock.log()
    );
}

#[test]
fn a_refused_delivery_holds_the_cursor_until_it_is_taken() {
    let mock = mock();
    mock.control("mock.hook_policy", json!({"attempts": 3, "window_ms": 600}));
    let dir = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let cfg = write(&config(&mock, port, dir.path(), "          rate: 1/4s\n"));
    let mut first = Daemon::start("hooks-refused-1", &cfg);
    first.wait_for("armed", |_| first.armed() == 1);
    // One token: the first is taken, the second refused (429) until the
    // server abandons it; the third, later, is taken — and its watermark,
    // past the abandoned one, is not kept.
    mock.control("mock.event", json!({"topic": "t1", "count": 2}));
    first.wait_for("the first run", |l| outputs(l) == [1]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while hook_lines(&mock, "ABANDONED").is_empty() {
        assert!(Instant::now() < deadline, "{}", mock.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        hook_lines(&mock, "FAILED")
            .iter()
            .any(|f| f["status"] == 429),
        "{}",
        mock.log()
    );
    std::thread::sleep(Duration::from_secs(4));
    mock.control("mock.event", json!({"topic": "t1"}));
    first.wait_for("the third run", |l| outputs(l) == [1, 3]);
    first.stop();
    // The re-arm starts from before the refused one: it runs, nothing twice.
    let second = Daemon::start("hooks-refused-2", &cfg);
    second.wait_for("the refused occurrence's run", |l| outputs(l) == [2]);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(outputs(&second.log()), [2], "{}", second.log());
}

#[test]
fn a_suspension_reported_on_refresh_rearms_from_the_cursor() {
    let mock = mock();
    let dir = tempfile::tempdir().unwrap();
    let port = common::free_port();
    // A 2-second lease: refreshed every 1.6 seconds.
    let cfg = write(&config(&mock, port, dir.path(), "          ttl: 2s\n"));
    let daemon = Daemon::start("hooks-suspended", &cfg);
    daemon.wait_for("armed", |_| daemon.armed() == 1);
    mock.control("mock.hook_suspend", json!({}));
    mock.control("mock.event", json!({"topic": "t1"}));
    wait(&daemon, &mock, "the suspension seen on refresh", |l| {
        !events(l, "mcp.events.delivery_suspended").is_empty()
    });
    daemon.wait_for("the held occurrence's run", |l| outputs(l) == [1]);
    let log = daemon.log();
    let s = &events(&log, "mcp.events.delivery_suspended")[0];
    assert_eq!(s["last_error"], "http_5xx", "{log}");
    assert!(daemon.armed() >= 2, "re-armed fresh:\n{log}");
}

#[test]
fn the_gap_and_terminated_envelopes_are_taken_and_said() {
    let mock = mock();
    let dir = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let cfg = write(&config(&mock, port, dir.path(), ""));
    let daemon = Daemon::start("hooks-envelopes", &cfg);
    daemon.wait_for("armed", |_| daemon.armed() == 1);
    let gap = mock.control("mock.hook_gap", json!({}));
    assert_eq!(gap["answers"][0], 200, "{gap}");
    daemon.wait_for("the gap said", |l| {
        events(l, "mcp.events.gap")
            .iter()
            .any(|e| e["cause"] == "truncated")
    });
    let end = mock.control("mock.hook_terminate", json!({"code": -32024}));
    assert_eq!(end["answers"][0], 200, "{end}");
    daemon.wait_for("refused as forbidden", |l| {
        events(l, "mcp.events.refused")
            .iter()
            .any(|e| e["reason"] == "forbidden")
    });
    let log = daemon.log();
    assert_eq!(
        events(&log, "mcp.events.terminated")[0]["code"],
        -32024,
        "{log}"
    );
}

#[cfg(feature = "hot-reload")]
#[test]
fn a_credential_change_unsubscribes_on_the_old_connection_and_removal_lets_go() {
    let mock = mock();
    let dir = tempfile::tempdir().unwrap();
    let port = common::free_port();
    // The credential is a reference, so a reload can name another one.
    let cfg_path = write(&config(&mock, port, dir.path(), ""));
    let daemon = Daemon::start("hooks-reload", &cfg_path);
    daemon.wait_for("armed", |_| daemon.armed() == 1);
    // A new credential: the subscription made under the old principal is
    // let go on the old connection, and a new one is made.
    let second = config(&mock, port, dir.path(), "").replace("HOOK_AUTH", "HOOK_AUTH_2");
    std::fs::write(&cfg_path, &second).unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    wait(&daemon, &mock, "re-armed under the new credential", |_| {
        daemon.armed() == 2
    });
    assert!(
        hook_lines(&mock, "UNSUBSCRIBE")
            .iter()
            .any(|u| u["found"] == true),
        "the old principal's subscription was let go:\n{}",
        mock.log()
    );
    let held = mock.control("mock.hook_state", json!({}));
    assert_eq!(
        held["subscriptions"].as_object().unwrap().len(),
        1,
        "{held}"
    );
    mock.control("mock.event", json!({"topic": "t1"}));
    wait(&daemon, &mock, "a run under the new subscription", |l| {
        outputs(l) == [1]
    });
    // Removed: unsubscribed at once.
    let without = second.replace(
        "      events:\n        - name: mock.tick\n          arguments: {topic: t1}\n          delivery: [webhook]\n          into: {stream: ticks, subject: tick}\n",
        "",
    );
    std::fs::write(&cfg_path, without).unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let held = mock.control("mock.hook_state", json!({}));
        if held["subscriptions"].as_object().unwrap().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "{held}\n{}", daemon.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_rearm_that_cannot_let_go_of_the_old_subscription_fails_and_retries() {
    let mock = mock();
    mock.control("mock.hook_policy", json!({"attempts": 2, "window_ms": 300}));
    let dir = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let cfg = write(&config(&mock, port, dir.path(), ""));
    let mut first = Daemon::start("hooks-unsub-1", &cfg);
    first.wait_for("armed", |_| first.armed() == 1);
    mock.control("mock.event", json!({"topic": "t1"}));
    first.wait_for("the first run", |l| outputs(l) == [1]);
    first.stop();
    mock.control("mock.event", json!({"topic": "t1"}));
    let deadline = Instant::now() + Duration::from_secs(30);
    while hook_lines(&mock, "ABANDONED").is_empty() {
        assert!(Instant::now() < deadline, "{}", mock.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    // The re-arm's unsubscribe fails once: subscribing anyway would refresh
    // the live subscription, whose cursor is a no-op — nothing replayed.
    mock.control("mock.hook_policy", json!({"fail_unsubscribe": 1}));
    let second = Daemon::start("hooks-unsub-2", &cfg);
    wait(&second, &mock, "the abandoned occurrence's run", |l| {
        outputs(l) == [2]
    });
    assert!(
        mock.log().contains("MOCK_HOOK_UNSUBSCRIBE_FAILED"),
        "{}",
        mock.log()
    );
    assert!(
        !events(&second.log(), "mcp.events.arm.fail").is_empty(),
        "{}",
        second.log()
    );
}

#[test]
fn a_mode_the_server_will_not_deliver_in_stays_passed_over_at_the_next_listing() {
    let mock = mock();
    mock.control("mock.hook_policy", json!({"refuse": true}));
    let dir = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let cfg = write(
        &config(&mock, port, dir.path(), "")
            .replace("delivery: [webhook]", "delivery: [webhook, poll]"),
    );
    let daemon = Daemon::start("hooks-demoted", &cfg);
    wait(&daemon, &mock, "moved to poll", |l| {
        events(l, "mcp.events.mode_changed")
            .iter()
            .any(|e| e["from"] == "webhook" && e["to"] == "poll")
    });
    let refused = mock.log().matches("MOCK_HOOK_REFUSED").count();
    // The server says its types changed: listed again, the binding keeps
    // polling rather than trying webhook once more.
    mock.control("mock.events_changed", json!({}));
    wait(&daemon, &mock, "listed again", |l| {
        events(l, "mcp.events.listed").len() >= 2
    });
    // Polling from its first answer on: an occurrence after it is taken.
    wait(&daemon, &mock, "polled", |_| {
        mock.log().contains("MOCK_EVENTS_POLL")
    });
    mock.control("mock.event", json!({"topic": "t1"}));
    wait(&daemon, &mock, "a run by poll", |l| outputs(l) == [1]);
    let log = daemon.log();
    assert_eq!(events(&log, "mcp.events.mode_changed").len(), 1, "{log}");
    assert_eq!(mock.log().matches("MOCK_HOOK_REFUSED").count(), refused);
}
