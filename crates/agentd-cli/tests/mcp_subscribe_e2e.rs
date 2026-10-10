// SPDX-License-Identifier: AGPL-3.0-only
//! A resource subscription, end to end, through the two ways it used to go
//! quiet without a word.
//!
//! - **The server restarts.** It forgets the session, and every subscription
//!   the session held goes with it. The daemon learns of it from the `404` its
//!   notification stream's redial gets, re-dials, says so (`mcp.disconnect`
//!   then `mcp.connect`, both `reason: session_lost`), and subscribes again —
//!   counted on the server, and proved by a wake that fires a run on the new
//!   session.
//! - **The server offers no subscriptions.** A `subscribe` start that can never
//!   fire is `start.subscribe.unsupported` at error level, and a resource wait
//!   fails its step, instead of parking for ever — whether the server never
//!   offered them or came back from a restart without them.
//!
//! What a re-dial restores is read off the owners — the starts, the waits, the
//! instruction — through the code a reload's re-dial takes, and a subscribe it
//! makes that fails in a way that may pass is asked again.
//!
//! At 2026-07-28 there is no session to lose: a subscription is a
//! `subscriptions/listen` stream, and a restart closes it. The mock speaks
//! that revision for real — sessionless, every request held to the
//! revision's headers and `_meta` — and the tests that run against it say
//! what reached it: no `initialize`, nothing refused, no session, no `GET`.
#![cfg(unix)]

mod common;

use agentd::mcp::mock_http::{Lifecycle, Options};
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WATCHED: &str = "mock://watched";

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A daemon on `config`, its stderr in a file the test reads as it goes.
struct Daemon {
    child: Child,
    err_path: String,
    cfg_path: String,
}

impl Daemon {
    fn start(tag: &str, config: &str) -> Daemon {
        let cfg_path = common::unique_path(tag, "yaml");
        std::fs::write(&cfg_path, config).expect("write config");
        let err_path = common::unique_path(tag, "err");
        let err = std::fs::File::create(&err_path).expect("stderr file");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn agentd");
        Daemon {
            child,
            err_path,
            cfg_path,
        }
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
}

impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.err_path);
        let _ = std::fs::remove_file(&self.cfg_path);
    }
}

/// Whether one `subscriptions/listen` that reached the mock asked for every
/// URI in `uris` — logged once the mock has decided whether to refuse it.
fn listened_for(mock: &common::MockMcp, uris: &[&str]) -> bool {
    mock.log()
        .lines()
        .filter_map(|l| l.strip_prefix("MOCK_LISTEN "))
        .filter_map(|f| serde_json::from_str::<Value>(f).ok())
        .any(|f| {
            uris.iter().all(|u| {
                f["resourceSubscriptions"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x == u))
            })
        })
}

/// What a 2026-07-28 connection never sends the mock: an `initialize`, a
/// request the revision's rules refuse, a session, a `GET`.
fn assert_stateless_wire(mock: &common::MockMcp) {
    assert_eq!(mock.initializes(), 0, "{}", mock.log());
    assert_eq!(mock.refused(), 0, "{}", mock.log());
    assert_eq!(mock.sessions(), 0, "{}", mock.log());
    assert_eq!(mock.gets(), 0, "{}", mock.log());
}

#[test]
fn a_server_that_forgets_the_session_is_re_dialed_and_the_subscription_restored() {
    let mock = common::spawn_mock_mcp(WATCHED, true);
    let endpoint = mock.uri();
    let daemon = Daemon::start(
        "mcp-session-lost",
        &format!(
            "agent:\n  name: session-lost\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: watch\n    steps:\n      s: {{kind: subscribe, server: mock, uri: \"{WATCHED}\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );

    // Subscribed, and the mock's one push after a subscribe fired a run.
    daemon.wait_for("the first wake to fire a run", |log| {
        !events(log, "run.done").is_empty()
    });
    assert_eq!(mock.subscribes(WATCHED), 1);

    // The server restarts: its session, and the subscription with it, gone.
    mock.control("mock.restart", json!({}));

    daemon.wait_for("a run fired by a wake on the new session", |log| {
        events(log, "run.done").len() >= 2
    });
    let log = daemon.log();
    let lost = events(&log, "mcp.disconnect");
    assert!(
        lost.iter()
            .any(|e| e["server"] == "mock" && e["reason"] == "session_lost"),
        "the loss is said:\n{log}"
    );
    let back = events(&log, "mcp.connect");
    assert!(
        back.iter()
            .any(|e| e["server"] == "mock" && e["reason"] == "session_lost"),
        "the re-dial is said:\n{log}"
    );
    // Both connections — at boot and the re-dial — say the revision they
    // negotiated.
    assert_eq!(back.len(), 2, "{log}");
    for c in &back {
        assert_eq!(c["protocol"], "2025-11-25", "{log}");
    }
    assert!(
        events(&log, "start.subscribe.armed")
            .iter()
            .any(|e| e["uri"] == WATCHED && e["reason"] == "session_lost"),
        "the subscription is restored:\n{log}"
    );
    // Counted where it lands: the server saw the subscription made again.
    assert_eq!(mock.subscribes(WATCHED), 2, "{}", mock.log());
}

#[test]
fn a_server_without_subscriptions_is_a_loud_failure_not_a_silent_park() {
    // At either revision: no `resources.subscribe` in the answer to the
    // handshake or to `server/discover`.
    for lifecycle in [Lifecycle::Initialize, Lifecycle::Discover] {
        let endpoint = common::mock_in_thread(Options {
            subscribe: false,
            lifecycle,
            ..Options::new(WATCHED)
        });
        let daemon = Daemon::start(
            "mcp-nosub",
            &format!(
                "agent:\n  name: nosub\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: watch\n    steps:\n      s: {{kind: subscribe, server: mock, uri: \"{WATCHED}\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\n  - name: waits\n    steps:\n      s: {{kind: once}}\n      w: {{kind: wait, on: resource, server: mock, uri: \"{WATCHED}\", depends_on: [s]}}\n      f: {{kind: finish, depends_on: [w], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
            ),
        );

        daemon.wait_for("the start to be refused and the wait to fail", |log| {
            !events(log, "start.subscribe.unsupported").is_empty()
                && log.contains("wait resource: subscribe")
        });
        let log = daemon.log();
        let refused = &events(&log, "start.subscribe.unsupported")[0];
        assert_eq!(refused["level"], "error", "{lifecycle:?}: {log}");
        assert_eq!(refused["uri"], WATCHED, "{lifecycle:?}: {log}");
        assert!(
            events(&log, "start.subscribe.armed").is_empty(),
            "{lifecycle:?}: nothing was armed:\n{log}"
        );
        assert!(
            log.contains("resources.subscribe"),
            "{lifecycle:?}: the wait's failure names the missing capability:\n{log}"
        );
    }
}

#[test]
fn a_subscribe_start_wakes_on_a_2026_07_28_server() {
    let mock = common::spawn_mock_mcp_at("2026-07-28", WATCHED, true);
    let endpoint = mock.uri();
    let daemon = Daemon::start(
        "mcp-listen-wake",
        &format!(
            "agent:\n  name: listen-wake\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: watch\n    steps:\n      s: {{kind: subscribe, server: mock, uri: \"{WATCHED}\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );
    daemon.wait_for("the listen's update to fire a run", |log| {
        !events(log, "run.done").is_empty()
    });
    let log = daemon.log();
    assert!(
        events(&log, "mcp.connect")
            .iter()
            .any(|e| e["server"] == "mock" && e["protocol"] == "2026-07-28"),
        "the connection says the revision it reached:\n{log}"
    );
    assert!(
        events(&log, "start.subscribe.armed")
            .iter()
            .any(|e| e["uri"] == WATCHED),
        "{log}"
    );
    // Reached through `server/discover`, subscribed through a listen.
    assert!(mock.discovers() >= 1, "{}", mock.log());
    assert!(listened_for(&mock, &[WATCHED]), "{}", mock.log());
    assert_stateless_wire(&mock);
}

/// A URI no owner wants any more is let go of, on the server too, while
/// what a start still watches stays held: before, the set a connection held
/// only grew, and the server went on notifying for URIs nothing read.
#[test]
fn a_uri_no_owner_wants_any_more_is_let_go_of() {
    released("2025-11-25");
}

#[test]
fn a_uri_no_owner_wants_any_more_is_let_go_of_at_2026_07_28() {
    released("2026-07-28");
}

fn released(protocol_version: &str) {
    let mock = common::spawn_mock_mcp_at(protocol_version, WATCHED, false);
    let endpoint = mock.uri();
    let daemon = Daemon::start(
        "mcp-release",
        &format!(
            "agent:\n  name: release\n  instruction: mcp://mock/mock://instruction\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: watch\n    steps:\n      s: {{kind: subscribe, server: mock, uri: \"{WATCHED}\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\n  - name: waits\n    steps:\n      s: {{kind: once}}\n      w: {{kind: wait, on: resource, server: mock, uri: \"mock://waited\", depends_on: [s], timeout: 1s}}\n      f: {{kind: finish, depends_on: [w], status: completed}}\n  - name: parks\n    steps:\n      s: {{kind: once}}\n      w: {{kind: wait, on: resource, server: mock, uri: \"mock://parked\", depends_on: [s], timeout: 120s}}\n      f: {{kind: finish, depends_on: [w], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );
    daemon.wait_for("the timed-out wait's URI to be let go of", |log| {
        events(log, "mcp.unsubscribed")
            .iter()
            .any(|e| e["uri"] == "mock://waited" && e["reason"] == "unowned")
    });
    // Still wanted: the start's URI, the parked wait's, the instruction's.
    let kept = [WATCHED, "mock://parked", "mock://instruction"];
    let log = daemon.log();
    assert!(
        !events(&log, "mcp.unsubscribed")
            .iter()
            .any(|e| kept.iter().any(|k| e["uri"] == *k)),
        "a URI an owner still wants was let go of:\n{log}"
    );
    let wire = mock.log();
    if protocol_version == "2026-07-28" {
        // The listen is opened again for what is still wanted, and only that.
        let last = wire
            .lines()
            .filter_map(|l| l.strip_prefix("MOCK_LISTEN "))
            .filter_map(|f| serde_json::from_str::<Value>(f).ok())
            .next_back()
            .expect("a listen");
        let mut held: Vec<&str> = last["resourceSubscriptions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        held.sort_unstable();
        let mut want = kept.to_vec();
        want.sort_unstable();
        assert_eq!(held, want, "{wire}");
        assert_stateless_wire(&mock);
    } else {
        assert!(wire.contains("MOCK_UNSUBSCRIBE mock://waited"), "{wire}");
        for k in kept {
            assert!(!wire.contains(&format!("MOCK_UNSUBSCRIBE {k}")), "{wire}");
        }
    }
}

/// A workflow a reload retires lets go of its start's URI only when no other
/// owner wants it: a wait still parked on the same URI keeps it held.
#[cfg(feature = "hot-reload")]
#[test]
fn a_retired_start_keeps_a_uri_a_parked_wait_still_wants() {
    let mock = common::spawn_mock_mcp(WATCHED, false);
    let endpoint = mock.uri();
    let parks = format!(
        "  - name: parks\n    steps:\n      s: {{kind: once}}\n      w: {{kind: wait, on: resource, server: mock, uri: \"{WATCHED}\", depends_on: [s], timeout: 120s}}\n      f: {{kind: finish, depends_on: [w], status: completed}}\n"
    );
    let config = |watch: bool| {
        format!(
            "agent:\n  name: retire\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n{}{parks}lifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n",
            if watch {
                format!(
                    "  - name: watch\n    steps:\n      s: {{kind: subscribe, server: mock, uri: \"{WATCHED}\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\n"
                )
            } else {
                String::new()
            }
        )
    };
    let daemon = Daemon::start("mcp-retire-keep", &config(true));
    daemon.wait_for("the start armed and the wait parked", |log| {
        !events(log, "start.subscribe.armed").is_empty()
            && events(log, "step.start")
                .iter()
                .any(|e| e["kind"] == "wait" && e["step"] == "w")
    });
    std::fs::write(&daemon.cfg_path, config(false)).unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    daemon.wait_for("the start's workflow retired", |log| {
        events(log, "workflow.unloaded")
            .iter()
            .any(|e| e["workflow"] == "watch")
    });
    // Past a sweep of what no owner wants.
    std::thread::sleep(Duration::from_secs(6));
    let log = daemon.log();
    assert!(events(&log, "workflow.unsubscribed").is_empty(), "{log}");
    assert!(events(&log, "mcp.unsubscribed").is_empty(), "{log}");
    assert!(
        !mock.log().contains(&format!("MOCK_UNSUBSCRIBE {WATCHED}")),
        "{}",
        mock.log()
    );
}

/// The two workflows the restart tests watch with: a `subscribe` start on
/// [`WATCHED`] and a `wait {on: resource}` on `mock://waited`, each counted
/// apart at the server.
fn watch_and_wait(endpoint: &str, name: &str) -> String {
    format!(
        "agent:\n  name: {name}\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: watch\n    steps:\n      s: {{kind: subscribe, server: mock, uri: \"{WATCHED}\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\n  - name: waits\n    steps:\n      s: {{kind: once}}\n      w: {{kind: wait, on: resource, server: mock, uri: \"mock://waited\", depends_on: [s], timeout: 120s}}\n      f: {{kind: finish, depends_on: [w], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
    )
}

#[test]
fn a_server_back_from_a_restart_without_subscriptions_fails_the_waits_and_says_the_start() {
    // The re-dial restores through the code boot and a reload take, so a
    // server that came back without `resources.subscribe` gets the answer
    // it gets there — not a warning and a wait parked for ever.
    let mock = common::spawn_mock_mcp(WATCHED, false);
    let daemon = Daemon::start("mcp-lost-cap", &watch_and_wait(&mock.uri(), "lostcap"));
    daemon.wait_for("the start and the wait to subscribe", |_| {
        mock.subscribes(WATCHED) >= 1 && mock.subscribes("mock://waited") >= 1
    });

    mock.control("mock.restart", json!({"subscribe": false}));

    daemon.wait_for("the wait to fail and the start to be refused", |log| {
        !events(log, "start.subscribe.unsupported").is_empty()
            && events(log, "run.done")
                .iter()
                .any(|e| e["workflow"] == "waits" && e["status"] == "failed")
    });
    let log = daemon.log();
    assert!(
        events(&log, "wait.resubscribe.fail")
            .iter()
            .any(|e| e["level"] == "error"
                && e["reason"] == "session_lost"
                && e["uri"] == "mock://waited"
                && e["err"]
                    .as_str()
                    .is_some_and(|m| m.contains("resources.subscribe"))),
        "the wait's failure names the lost capability:\n{log}"
    );
    let refused = &events(&log, "start.subscribe.unsupported")[0];
    assert_eq!(refused["level"], "error", "{log}");
    assert_eq!(refused["uri"], WATCHED, "{log}");
}

#[test]
fn a_re_subscribe_the_restarted_server_refuses_is_asked_again() {
    // A server still warming up refuses the first subscribe on the new
    // session. Said once and left, the start never fired again.
    let mock = common::spawn_mock_mcp(WATCHED, true);
    let daemon = Daemon::start("mcp-resub-retry", &watch_and_wait(&mock.uri(), "retry"));
    daemon.wait_for(
        "the first wake to fire a run, and the wait to subscribe",
        |log| !events(log, "run.done").is_empty() && mock.subscribes("mock://waited") >= 1,
    );

    // The start's re-subscribe and the wait's are both refused.
    mock.control("mock.refuse_subscribe", json!({"count": 2}));
    mock.control("mock.restart", json!({}));

    daemon.wait_for("a run fired by a wake on the retried subscription", |log| {
        events(log, "run.done").len() >= 2
            && events(log, "wait.resubscribed")
                .iter()
                .any(|e| e["reason"] == "retry")
    });
    let log = daemon.log();
    assert!(
        events(&log, "wait.resubscribe.fail")
            .iter()
            .any(|e| e["level"] == "warn" && e["uri"] == "mock://waited"),
        "{log}"
    );
    assert_eq!(mock.subscribes("mock://waited"), 3, "{}", mock.log());
    assert!(
        events(&log, "start.subscribe.fail")
            .iter()
            .any(|e| e["uri"] == WATCHED),
        "the refusal is said:\n{log}"
    );
    assert!(
        events(&log, "start.subscribe.armed")
            .iter()
            .any(|e| e["uri"] == WATCHED && e["reason"] == "retry"),
        "the start is armed again by the retry:\n{log}"
    );
    // At the server: the first subscribe, the refused one, the retry.
    assert_eq!(mock.subscribes(WATCHED), 3, "{}", mock.log());
}

/// Whether a run of `workflow` started after the last `mcp.listen.resumed`
/// line: a wake the re-opened listen delivered.
fn started_after_the_resume(log: &str, workflow: &str) -> bool {
    let lines: Vec<Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let Some(resumed) = lines
        .iter()
        .rposition(|e| e["event"] == "mcp.listen.resumed")
    else {
        return false;
    };
    lines[resumed..]
        .iter()
        .any(|e| e["event"] == "run.start" && e["workflow"] == workflow)
}

#[test]
fn a_re_subscribe_the_restarted_server_refuses_is_asked_again_at_2026_07_28() {
    // At 2026-07-28 a restart has no session to lose: it closes the listen
    // that carries every subscription, and the connection listens again. A
    // server still warming up refuses the first re-listens; they are asked
    // again until one is acknowledged, and the start fires on it.
    let mock = common::spawn_mock_mcp_at("2026-07-28", WATCHED, true);
    let daemon = Daemon::start(
        "mcp-relisten-retry",
        &watch_and_wait(&mock.uri(), "relisten"),
    );
    daemon.wait_for(
        "the first wake to fire a run, and the wait to be in the listen",
        |log| {
            !events(log, "run.done").is_empty() && listened_for(&mock, &[WATCHED, "mock://waited"])
        },
    );

    mock.control("mock.refuse_subscribe", json!({"count": 2}));
    mock.control("mock.restart", json!({}));

    daemon.wait_for("a run fired by a wake on the listen opened again", |log| {
        started_after_the_resume(log, "watch")
    });
    let log = daemon.log();
    assert!(
        events(&log, "mcp.listen.ended")
            .iter()
            .any(|e| e["level"] == "warn"
                && e["reason"]
                    .as_str()
                    .is_some_and(|r| r.contains("subscribe refused"))),
        "a refused re-listen is said, and asked again:\n{log}"
    );
    assert_stateless_wire(&mock);
}

#[test]
fn a_resource_instruction_is_read_again_after_its_server_restarts() {
    // The publisher may have changed the instruction while the server was
    // down — often why it restarted. Subscribing it again is not enough: an
    // update sent before the new subscription existed reached nobody.
    let mock = common::spawn_mock_mcp(WATCHED, false);
    let endpoint = mock.uri();
    let daemon = Daemon::start(
        "mcp-instruction-redial",
        &format!(
            "agent:\n  name: instructed\n  instruction: mcp://mock/mock://instruction\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );
    daemon.wait_for("the instruction to be read and subscribed", |_| {
        mock.subscribes("mock://instruction") >= 1
    });
    assert_eq!(mock.reads("mock://instruction"), 1, "{}", mock.log());

    mock.control("mock.restart", json!({}));

    daemon.wait_for("the instruction read again, changed", |log| {
        !events(log, "instruction.updated").is_empty()
    });
    assert_eq!(mock.reads("mock://instruction"), 2, "{}", mock.log());
    assert_eq!(mock.subscribes("mock://instruction"), 2, "{}", mock.log());
}

#[test]
fn a_listen_stream_that_ends_is_said_and_a_narrowed_one_fails_the_wait_it_dropped() {
    // At 2026-07-28 one `subscriptions/listen` stream carries every
    // subscription. A restart closes it: the daemon says the end and listens
    // again. Then the server acknowledges without the waited URI. rmcp drops
    // every update outside an acknowledgment, so the URI is said dropped,
    // asked for again, and — the server still leaving it out — the wait on it
    // fails rather than parking for ever.
    let mock = common::spawn_mock_mcp_at("2026-07-28", WATCHED, true);
    let daemon = Daemon::start("mcp-listen", &watch_and_wait(&mock.uri(), "listener"));
    daemon.wait_for(
        "the start to fire and the wait to be in the listen",
        |log| {
            !events(log, "run.done").is_empty() && listened_for(&mock, &[WATCHED, "mock://waited"])
        },
    );

    mock.control("mock.restart", json!({}));

    daemon.wait_for("the listen to end and be opened again", |log| {
        !events(log, "mcp.listen.resumed").is_empty()
    });
    let log = daemon.log();
    let ended = &events(&log, "mcp.listen.ended")[0];
    assert_eq!(ended["server"], "mock", "{log}");
    assert_eq!(ended["level"], "warn", "a restart is no idle cut:\n{log}");
    assert!(ended["retry_ms"].as_u64().is_some(), "{log}");
    assert!(
        mock.log()
            .lines()
            .any(|l| l == "MOCK_LISTEN_CLOSED restart"),
        "{}",
        mock.log()
    );

    mock.control("mock.narrow", json!({"uri": "mock://waited"}));
    mock.control("mock.restart", json!({}));

    daemon.wait_for("the wait on the dropped URI to fail", |log| {
        events(log, "run.done")
            .iter()
            .any(|e| e["workflow"] == "waits" && e["status"] == "failed")
    });
    let log = daemon.log();
    assert!(
        events(&log, "mcp.listen.narrowed")
            .iter()
            .any(|e| e["dropped"] == json!(["mock://waited"])),
        "the narrowing is said:\n{log}"
    );
    assert!(
        events(&log, "wait.resubscribe.fail")
            .iter()
            .any(|e| e["level"] == "error"
                && e["reason"] == "retry"
                && e["err"]
                    .as_str()
                    .is_some_and(|m| m.contains("acknowledged the listen without"))),
        "the retry met the refusal, and the wait was told:\n{log}"
    );
    assert_stateless_wire(&mock);
}

#[test]
fn an_instruction_on_a_server_without_subscriptions_is_said_once_not_on_every_refresh() {
    // Such a server is followed by the refresh re-read alone. Said when the
    // instruction is first read from it — not as a warning on every re-read.
    let endpoint = format!(
        "{}/mcp",
        common::mock_in_thread(Options {
            emit: false,
            subscribe: false,
            ..Options::new(WATCHED)
        })
    );
    let daemon = Daemon::start(
        "mcp-nosub-instruction",
        &format!(
            "agent:\n  name: refreshed\n  instruction:\n    mcp: \"mcp://mock/mock://instruction\"\n    refresh: 300ms\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}\nworkflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );
    daemon.wait_for("a few refresh re-reads", |log| {
        events(log, "instruction.loaded").len() >= 4
    });
    let log = daemon.log();
    assert!(
        events(&log, "instruction.subscribe.fail").is_empty(),
        "no warning per re-read:\n{log}"
    );
    assert_eq!(
        events(&log, "instruction.subscribe.unsupported").len(),
        1,
        "said once:\n{log}"
    );
}

#[test]
fn a_tool_rmcp_drops_is_logged_once_by_the_daemon() {
    // At 2026-07-28 the MCP client drops, without a word, a tool whose
    // `x-mcp-header` annotation it rejects. The daemon says it — once, though
    // boot drains twice (after the connect's priming list and after the
    // catalogue's own list).
    let mock = common::spawn_mock_mcp_at("2026-07-28", WATCHED, true);
    let endpoint = mock.uri();
    let daemon = Daemon::start(
        "mcp-dropped",
        &format!(
            "agent:\n  name: dropped\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );
    daemon.wait_for("the connection", |log| {
        !events(log, "mcp.connect").is_empty()
    });
    let log = daemon.log();
    let dropped = events(&log, "mcp.tool.dropped");
    assert_eq!(dropped.len(), 1, "{log}");
    assert_eq!(dropped[0]["level"], "warn", "{log}");
    assert_eq!(dropped[0]["server"], "mock", "{log}");
    assert_eq!(dropped[0]["tool"], "mock.nested", "{log}");
    assert_eq!(
        dropped[0]["reason"], "invalid x-mcp-header annotation",
        "{log}"
    );
    assert_stateless_wire(&mock);
}
