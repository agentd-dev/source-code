// SPDX-License-Identifier: AGPL-3.0-only
//! MCP Events (RFC 0045 P2, SEP-3415), end to end: a binding polls its
//! server's occurrences into a declared stream, and the stream's consumers
//! run — through the built-in mock's Events surface, which holds every
//! request to the 2026-07-28 rules and records each one it answers.
//!
//! - The happy path: discovery (two pages), the cursor, one run per
//!   occurrence, the payload nested under `data.payload`.
//! - A restart catches up from the stored cursor and fires nothing twice.
//! - What the bridge refuses or flags: a repeated page, an oversize or a
//!   malformed occurrence, a payload that fails its schema, a `truncated`
//!   answer.
//! - Every refusal the RFC names that this build can meet, each said once
//!   with its reason, and no `events/*` request where none may be sent.
//! - The bounds: a server that always says `hasMore`, and the binding's own
//!   `rate`, which a poll's `maxEvents` follows.
#![cfg(all(unix, feature = "mcp-events"))]

mod common;

use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const REVISION: &str = "2026-07-28";

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
        Daemon::start_at(tag, &cfg_path)
    }

    fn start_at(tag: &str, cfg_path: &str) -> Daemon {
        let err_path = common::unique_path(tag, "err");
        let err = std::fs::File::create(&err_path).expect("stderr file");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", cfg_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn agentd");
        Daemon {
            child,
            err_path,
            cfg_path: cfg_path.to_string(),
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

    /// Stop it and wait for it to be gone.
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
        let _ = std::fs::remove_file(&self.cfg_path);
    }
}

/// The `n` of every finished `on_tick` run, in the order they finished.
fn ticks(log: &str) -> Vec<Value> {
    events(log, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "on_tick")
        .map(|e| e["output"].clone())
        .collect()
}

/// Every `events/poll` that reached the mock, as the params it was sent.
fn polls(mock: &common::MockMcp) -> Vec<Value> {
    mock.log()
        .lines()
        .filter_map(|l| l.strip_prefix("MOCK_EVENTS_POLL "))
        .filter_map(|p| serde_json::from_str(p).ok())
        .collect()
}

/// A daemon config: one server `mock` at `endpoint` (tagged, so no `rate` is
/// required unless `binding` sets one) with the `events:` entries in
/// `bindings`, a stream `ticks`, and a workflow `on_tick` that finishes with
/// each occurrence's `n`.
fn config(endpoint: &str, store: &str, bindings: &str) -> String {
    format!(
        "agent:\n  name: events\nstore:\n{store}streams:\n  ticks: {{}}\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\n      tags: {{\"*\": [sensitive]}}\n      events:\n{bindings}workflows:\n  - name: on_tick\n    steps:\n      s: {{kind: stream, stream: ticks, subject: tick, inputs: {{n: \"{{{{ payload.data.payload.n }}}}\"}}}}\n      f: {{kind: finish, depends_on: [s], status: completed, output: \"{{{{ inputs.n }}}}\"}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n  log_content: true\n"
    )
}

const MEMORY: &str = "  kind: memory\n";

fn tick_binding(extra: &str) -> String {
    format!(
        "        - name: mock.tick\n          arguments: {{topic: t1}}\n          delivery: [poll]\n          into: {{stream: ticks, subject: tick}}\n{extra}"
    )
}

/// Wait until the binding on `event` is armed AND its first poll has reached
/// the mock: the first poll asks from "now" [SEP L690], so an occurrence the
/// test appends before it is — rightly — never delivered.
fn ready(daemon: &Daemon, mock: &common::MockMcp, event: &str) {
    daemon.wait_for("the binding armed", |l| armed(l, event));
    daemon.wait_for("its first poll", |_| {
        polls(mock).iter().any(|p| p["name"] == event)
    });
}

fn armed(log: &str, event: &str) -> bool {
    events(log, "mcp.events.armed")
        .iter()
        .any(|e| e["event_type"] == event)
}

#[test]
fn a_binding_polls_its_occurrences_into_its_stream_and_each_starts_a_run() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let daemon = Daemon::start(
        "events-happy",
        &config(&mock.uri(), MEMORY, &tick_binding("")),
    );
    ready(&daemon, &mock, "mock.tick");
    mock.control("mock.event", json!({"topic": "t1", "count": 3}));
    daemon.wait_for("three runs", |l| ticks(l).len() >= 3);
    let log = daemon.log();
    let mut got: Vec<String> = ticks(&log).iter().map(|v| v.to_string()).collect();
    got.sort();
    assert_eq!(got, ["1", "2", "3"], "{log}");
    // Discovery followed the listing's pages, and saw both types.
    assert!(
        events(&log, "mcp.events.listed")
            .iter()
            .any(|e| e["server"] == "mock" && e["types"] == 2),
        "{log}"
    );
    let wire = mock.log();
    assert!(wire.contains("MOCK_EVENTS_LIST -"), "{wire}");
    assert!(
        wire.contains("MOCK_EVENTS_LIST 1"),
        "the second page: {wire}"
    );
    // Every poll carried the binding's arguments; the first asked from now.
    let sent = polls(&mock);
    assert!(
        sent.iter()
            .all(|p| p["arguments"] == json!({"topic": "t1"})),
        "{sent:?}"
    );
    assert_eq!(sent[0]["cursor"], Value::Null, "{sent:?}");
    // Nothing the 2026-07-28 rules refuse reached the server.
    assert_eq!(mock.refused(), 0, "{wire}");
    assert_eq!(mock.sessions(), 0, "{wire}");
    // The draft is said at load.
    assert!(
        events(&log, "config.warning").iter().any(|e| e["warning"]
            .as_str()
            .is_some_and(|w| w.contains("SEP-3415"))),
        "{log}"
    );
}

#[test]
fn a_restart_catches_up_from_the_stored_cursor_and_fires_nothing_twice() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let dir = tempfile::tempdir().unwrap();
    let store = format!("  kind: file\n  file: {{path: {}}}\n", dir.path().display());
    let cfg_path = common::unique_path("events-restart", "yaml");
    std::fs::write(&cfg_path, config(&mock.uri(), &store, &tick_binding(""))).unwrap();
    let mut first = Daemon::start_at("events-restart-1", &cfg_path);
    ready(&first, &mock, "mock.tick");
    mock.control("mock.event", json!({"topic": "t1", "count": 1}));
    // The cursor is written in the pass that appended, before the run fires.
    first.wait_for("the first run", |l| ticks(l).len() == 1);
    first.stop();

    // While it was down.
    mock.control("mock.event", json!({"topic": "t1", "count": 2}));
    let second = Daemon::start_at("events-restart-2", &cfg_path);
    second.wait_for("the two missed runs", |l| ticks(l).len() >= 2);
    std::thread::sleep(Duration::from_millis(1500));
    let log = second.log();
    let mut got: Vec<String> = ticks(&log).iter().map(|v| v.to_string()).collect();
    got.sort();
    assert_eq!(got, ["2", "3"], "caught up, nothing twice:\n{log}");
    assert!(
        events(&log, "mcp.events.armed")
            .iter()
            .any(|e| e["cursor_present"] == true),
        "armed from the stored cursor:\n{log}"
    );
}

#[test]
fn what_the_bridge_refuses_flags_or_dedupes_is_said_and_counted_right() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let daemon = Daemon::start(
        "events-bridge",
        &config(&mock.uri(), MEMORY, &tick_binding("")),
    );
    ready(&daemon, &mock, "mock.tick");
    // A payload that fails its schema is appended, flagged.
    mock.control("mock.event", json!({"topic": "t1", "bad_schema": true}));
    daemon.wait_for("the flagged run", |l| ticks(l).len() == 1);
    // An oversize and a malformed one are rejected, by cause.
    mock.control("mock.event", json!({"topic": "t1", "oversize": true}));
    mock.control("mock.event", json!({"topic": "t1", "malformed": true}));
    // The next page is answered twice: its occurrence is appended once.
    mock.control("mock.event", json!({"topic": "t1"}));
    mock.control("mock.events_flags", json!({"repeat": true}));
    daemon.wait_for("both rejections and the next run", |l| {
        events(l, "mcp.events.rejected").len() >= 2 && ticks(l).len() >= 2
    });
    std::thread::sleep(Duration::from_millis(2500));
    let log = daemon.log();
    assert_eq!(
        ticks(&log).len(),
        2,
        "the repeated page fired nothing twice:\n{log}"
    );
    let causes: Vec<String> = events(&log, "mcp.events.rejected")
        .iter()
        .map(|e| e["cause"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(causes.contains(&"oversize".to_string()), "{causes:?}");
    assert!(causes.contains(&"malformed".to_string()), "{causes:?}");
    assert!(
        events(&log, "mcp.events.schema_mismatch")
            .iter()
            .any(|e| e["event_id"] == "evt-t1-1"),
        "{log}"
    );
    // A trimmed log answers an older cursor `truncated`: a gap, said.
    mock.control("mock.event_trim", json!({"topic": "t1"}));
    mock.control("mock.event", json!({"topic": "t1"}));
    daemon.wait_for("the gap", |l| {
        events(l, "mcp.events.gap")
            .iter()
            .any(|e| e["cause"] == "truncated")
    });
}

/// A page answered twice — a server's retry, a client's re-poll after a
/// crash — is appended once: the second sight of each occurrence is a
/// duplicate, counted as one, and nothing reaches the stream twice.
#[cfg(feature = "metrics")]
#[test]
fn a_page_answered_twice_is_appended_once() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let cfg = config(&mock.uri(), MEMORY, &tick_binding("")).replace(
        "observability:\n  log_level: info\n",
        "observability:\n  log_level: info\n  metrics_addr: 127.0.0.1:0\n",
    );
    let daemon = Daemon::start("events-dedup", &cfg);
    ready(&daemon, &mock, "mock.tick");
    mock.control("mock.event", json!({"topic": "t1", "count": 2}));
    daemon.wait_for("two runs", |l| ticks(l).len() == 2);
    mock.control("mock.events_flags", json!({"repeat": true}));
    let addr = events(&daemon.log(), "metrics.serving")[0]["addr"]
        .as_str()
        .unwrap()
        .to_string();
    let want = "agent_mcp_events_total{server=\"mock\",event=\"mock.tick\",mode=\"poll\",outcome=\"duplicate\"} 2";
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut scraped = String::new();
    while Instant::now() < deadline && !scraped.contains(want) {
        scraped = common::http_get(&addr, "/metrics").body;
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(scraped.contains(want), "{scraped}");
    assert!(
        scraped.contains("agent_mcp_events_total{server=\"mock\",event=\"mock.tick\",mode=\"poll\",outcome=\"appended\"} 2"),
        "{scraped}"
    );
    assert!(
        scraped.contains("agent_subscriptions_active{kind=\"event\",mode=\"poll\"} 1"),
        "{scraped}"
    );
    assert_eq!(ticks(&daemon.log()).len(), 2);
}

#[test]
fn every_refusal_is_said_once_with_its_reason() {
    let modern = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let bare = common::spawn_mock_mcp_with(REVISION, "mock://res", false, &["--events", "none"]);
    let legacy = common::spawn_mock_mcp_at("2025-11-25", "mock://res", false);
    let binding = |name: &str, args: &str| {
        format!(
            "        - name: {name}\n          arguments: {args}\n          delivery: [poll]\n          into: {{stream: ticks, subject: tick}}\n"
        )
    };
    let server = |name: &str, endpoint: &str, list: &str| {
        format!(
            "    - name: {name}\n      endpoint: {endpoint}/mcp\n      tags: {{\"*\": [sensitive]}}\n      events:\n{list}"
        )
    };
    let modern_list = [
        binding("nope.never", "{}"),
        binding("mock.webhook_only", "{}"),
        binding("mock.tick", "{stray: 1}"),
        binding("mock.tick", "{topic: forbidden}"),
    ]
    .concat();
    let cfg = format!(
        "agent:\n  name: refusals\nstore:\n  kind: memory\nstreams:\n  ticks: {{}}\nmcp:\n  servers:\n{}{}{}lifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n",
        server("modern", &modern.uri(), &modern_list),
        server("bare", &bare.uri(), &binding("mock.tick", "{}")),
        server("legacy", &legacy.uri(), &binding("mock.tick", "{}")),
    );
    let daemon = Daemon::start("events-refusals", &cfg);
    let want = [
        ("modern", "nope.never", "unknown_event"),
        ("modern", "mock.webhook_only", "no_common_mode"),
        ("modern", "mock.tick", "invalid_arguments"),
        ("modern", "mock.tick", "forbidden"),
        ("bare", "mock.tick", "no_capability"),
        ("legacy", "mock.tick", "revision"),
    ];
    daemon.wait_for("every refusal", |l| {
        let refused = events(l, "mcp.events.refused");
        want.iter().all(|(s, e, r)| {
            refused
                .iter()
                .any(|x| x["server"] == *s && x["event_type"] == *e && x["reason"] == *r)
        })
    });
    std::thread::sleep(Duration::from_millis(2500));
    let log = daemon.log();
    let refused = events(&log, "mcp.events.refused");
    assert_eq!(refused.len(), want.len(), "each said once:\n{log}");
    assert!(
        refused.iter().all(|e| e["level"] == "error"),
        "at error level: {refused:?}"
    );
    // Where Events is not offered, nothing of it is sent [SEP L94].
    for (name, mock) in [("bare", &bare), ("legacy", &legacy)] {
        assert!(
            !mock.log().contains("MOCK_EVENTS_"),
            "{name} was sent events/*:\n{}",
            mock.log()
        );
    }
}

#[test]
fn the_sketchs_top_level_declaration_is_read_too() {
    let mock =
        common::spawn_mock_mcp_with(REVISION, "mock://res", false, &["--events", "top-level"]);
    let daemon = Daemon::start(
        "events-top-level",
        &config(&mock.uri(), MEMORY, &tick_binding("")),
    );
    ready(&daemon, &mock, "mock.tick");
}

#[test]
fn a_server_that_always_says_has_more_is_not_polled_in_a_loop() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    mock.control("mock.events_flags", json!({"always_more": true}));
    let daemon = Daemon::start(
        "events-has-more",
        &config(&mock.uri(), MEMORY, &tick_binding("")),
    );
    ready(&daemon, &mock, "mock.tick");
    mock.control("mock.event", json!({"topic": "t1", "count": 30}));
    daemon.wait_for("the backlog drained", |l| ticks(l).len() >= 30);
    let at = polls(&mock).len();
    std::thread::sleep(Duration::from_secs(3));
    // Nothing new: each poll waits its interval (a floor of 1 s), however
    // often the server says `hasMore`.
    let more = polls(&mock).len() - at;
    assert!(more <= 4, "{more} polls in 3 s with nothing new");
}

#[test]
fn a_binding_rate_paces_intake_and_each_poll_asks_for_what_it_may_take() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let daemon = Daemon::start(
        "events-rate",
        &config(
            &mock.uri(),
            MEMORY,
            &tick_binding("          rate: 2/60s\n"),
        ),
    );
    ready(&daemon, &mock, "mock.tick");
    mock.control("mock.event", json!({"topic": "t1", "count": 10}));
    daemon.wait_for("the first two runs", |l| ticks(l).len() >= 2);
    std::thread::sleep(Duration::from_secs(3));
    let log = daemon.log();
    assert_eq!(ticks(&log).len(), 2, "paced, not drained:\n{log}");
    assert!(
        polls(&mock)
            .iter()
            .all(|p| p["maxEvents"].as_u64().is_some_and(|n| n <= 2)),
        "{:?}",
        polls(&mock)
    );
    assert!(
        events(&log, "mcp.events.paused")
            .iter()
            .any(|e| e["cause"] == "rate"),
        "{log}"
    );
}

#[test]
fn a_failure_that_may_pass_backs_off_and_resumes_from_the_cursor() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let daemon = Daemon::start(
        "events-backoff",
        &config(&mock.uri(), MEMORY, &tick_binding("")),
    );
    ready(&daemon, &mock, "mock.tick");
    mock.control(
        "mock.events_fail",
        json!({"code": -32025, "times": 2,
        "data": {"limit": "polls"}}),
    );
    mock.control("mock.event", json!({"topic": "t1"}));
    daemon.wait_for("the run after the failures", |l| ticks(l).len() == 1);
    let log = daemon.log();
    let fails = events(&log, "mcp.events.poll.fail");
    assert!(fails.len() >= 2, "{log}");
    assert!(
        fails
            .iter()
            .all(|e| e["code"] == -32025 && e["level"] == "warn"),
        "{fails:?}"
    );
    assert!(
        events(&log, "mcp.events.refused").is_empty(),
        "transient, not refused:\n{log}"
    );
}

#[cfg(feature = "hot-reload")]
#[test]
fn editing_a_binding_or_adding_one_dials_nothing() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let daemon = Daemon::start(
        "events-reload",
        &config(&mock.uri(), MEMORY, &tick_binding("")),
    );
    ready(&daemon, &mock, "mock.tick");
    let discovers = mock.discovers();
    // A rate on the first, and a second binding.
    let two = format!(
        "{}        - name: mock.tick\n          arguments: {{topic: t2}}\n          delivery: [poll]\n          into: {{stream: ticks, subject: tick}}\n",
        tick_binding("          rate: 5/1s\n")
    );
    std::fs::write(&daemon.cfg_path, config(&mock.uri(), MEMORY, &two)).unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    daemon.wait_for("the reload applied", |l| {
        events(l, "config.reloaded")
            .iter()
            .any(|e| e["changed"].to_string().contains("mcp.events"))
    });
    daemon.wait_for("the second binding polling", |_| {
        polls(&mock).iter().any(|p| p["arguments"]["topic"] == "t2")
    });
    assert_eq!(mock.discovers(), discovers, "no re-dial:\n{}", mock.log());
    let log = daemon.log();
    assert_eq!(
        events(&log, "mcp.connect").len(),
        1,
        "one connection, kept:\n{log}"
    );
    // The first binding kept its subscription and goes on.
    mock.control("mock.event", json!({"topic": "t1"}));
    daemon.wait_for("a run on the edited binding", |l| !ticks(l).is_empty());
}

#[test]
fn a_server_that_ignores_max_events_is_still_held_to_the_rate() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    mock.control("mock.events_flags", json!({"ignore_max": true}));
    let daemon = Daemon::start(
        "events-ignore-max",
        &config(
            &mock.uri(),
            MEMORY,
            &tick_binding("          rate: 2/60s\n"),
        ),
    );
    ready(&daemon, &mock, "mock.tick");
    mock.control("mock.event", json!({"topic": "t1", "count": 10}));
    daemon.wait_for("the paced pause", |l| {
        events(l, "mcp.events.paused")
            .iter()
            .any(|e| e["cause"] == "rate")
    });
    std::thread::sleep(Duration::from_secs(2));
    let log = daemon.log();
    assert_eq!(
        ticks(&log).len(),
        2,
        "two of the ten the page carried:\n{log}"
    );
}

#[test]
fn a_schema_changed_in_place_lets_the_cursor_go_and_says_the_gap() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let daemon = Daemon::start(
        "events-schema-changed",
        &config(&mock.uri(), MEMORY, &tick_binding("")),
    );
    ready(&daemon, &mock, "mock.tick");
    mock.control("mock.event", json!({"topic": "t1"}));
    daemon.wait_for("the first run", |l| ticks(l).len() == 1);
    // Answered so three times: each is a gap and a listing, and the polls
    // between them wait the backoff the failures earn (1 s, then 2 s).
    mock.control(
        "mock.events_fail",
        json!({"code": -32026, "times": 3,
        "data": {"feature": "payloadSchema", "reason": "schema_changed"}}),
    );
    let gaps = |l: &str| {
        events(l, "mcp.events.gap")
            .iter()
            .filter(|e| e["cause"] == "schema_changed")
            .count()
    };
    daemon.wait_for("the first gap", |l| gaps(l) >= 1);
    let first = Instant::now();
    daemon.wait_for("the third gap", |l| gaps(l) >= 3);
    assert!(
        first.elapsed() >= Duration::from_millis(2500),
        "backed off, not looped: {:?}",
        first.elapsed()
    );
    // Re-armed from now each time: the first poll asked from now, so did
    // each after a change, and the last of them was answered — after which
    // the binding polls from the fresh cursor, and what comes next arrives.
    daemon.wait_for("polling from the fresh cursor", |_| {
        let sent = polls(&mock);
        sent.iter().filter(|p| p["cursor"].is_null()).count() >= 4
            && sent.last().is_some_and(|p| !p["cursor"].is_null())
    });
    mock.control("mock.event", json!({"topic": "t1"}));
    daemon.wait_for("the run after the change", |l| ticks(l).len() == 2);
}

#[cfg(feature = "hot-reload")]
#[test]
fn a_binding_added_on_a_refused_server_is_refused_at_once() {
    let bare = common::spawn_mock_mcp_with(REVISION, "mock://res", false, &["--events", "none"]);
    let daemon = Daemon::start(
        "events-refused-add",
        &config(&bare.uri(), MEMORY, &tick_binding("")),
    );
    daemon.wait_for("the server refused", |l| {
        events(l, "mcp.events.refused")
            .iter()
            .any(|e| e["reason"] == "no_capability")
    });
    let two = format!(
        "{}        - name: mock.tick\n          arguments: {{topic: t2}}\n          delivery: [poll]\n          into: {{stream: ticks, subject: tick}}\n",
        tick_binding("")
    );
    std::fs::write(&daemon.cfg_path, config(&bare.uri(), MEMORY, &two)).unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    daemon.wait_for("the added binding refused", |l| {
        events(l, "mcp.events.refused")
            .iter()
            .filter(|e| e["reason"] == "no_capability")
            .count()
            >= 2
    });
    assert!(!bare.log().contains("MOCK_EVENTS_"), "{}", bare.log());
}

#[test]
fn a_type_listed_later_is_armed_when_the_server_says_its_list_changed() {
    // The standing listen asks for `eventsListChanged`; the notification it
    // brings is taken off the stream for the loop — rmcp's own listen would
    // have ended on it — and the server is listed again at once, not at the
    // next 15-minute re-list.
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let later = "        - name: mock.later\n          delivery: [poll]\n          into: {stream: ticks, subject: tick}\n";
    let daemon = Daemon::start("events-list-changed", &config(&mock.uri(), MEMORY, later));
    daemon.wait_for("the type refused as unknown", |l| {
        events(l, "mcp.events.refused")
            .iter()
            .any(|e| e["reason"] == "unknown_event")
    });
    let asked = |m: &common::MockMcp| {
        m.log()
            .lines()
            .filter_map(|l| l.strip_prefix("MOCK_LISTEN "))
            .filter_map(|f| serde_json::from_str::<Value>(f).ok())
            .any(|f| f["eventsListChanged"] == true)
    };
    daemon.wait_for("a listen that asks for eventsListChanged", |_| asked(&mock));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let told = mock.control("mock.events_changed", json!({}))["told"]
            .as_u64()
            .unwrap_or(0);
        if told > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no listen took the notification:\n{}",
            mock.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    daemon.wait_for("the type armed", |l| armed(l, "mock.later"));
    let log = daemon.log();
    assert!(
        events(&log, "mcp.events.list_changed")
            .iter()
            .any(|e| e["server"] == "mock"),
        "{log}"
    );
    assert!(
        !events(&log, "mcp.listen.ended")
            .iter()
            .any(|e| e["level"] == "warn"),
        "the standing listen outlived the notification:\n{log}"
    );
}

/// A config with one granted event type on `mock` (and no binding): `extra`
/// goes under the grant, `workflows` is the workflows section's body.
fn granted(endpoint: &str, store: &str, grant_extra: &str, workflows: &str) -> String {
    format!(
        "agent:\n  name: runtime\nstore:\n{store}streams:\n  ticks: {{}}\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\n      tags: {{\"*\": [sensitive]}}\n      event_grants:\n        - name: mock.tick\n          delivery: [poll]\n          into: {{stream: ticks, subject: tick}}\n{grant_extra}workflows:\n{workflows}lifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n  log_content: true\n"
    )
}

/// A workflow that waits on its own subscription to `topic` and finishes
/// with the occurrence's `n`.
fn waiter(name: &str, topic: &str) -> String {
    format!(
        "  - name: {name}\n    steps:\n      s: {{kind: once}}\n      w: {{kind: wait, on: event, depends_on: [s], timeout: 120s, subscribe: {{server: mock, event: mock.tick, arguments: {{topic: {topic}}}}}}}\n      f: {{kind: finish, depends_on: [w], status: completed, output: \"{{{{ steps.w.output.data.payload.n }}}}\"}}\n"
    )
}

fn done(log: &str, workflow: &str) -> Vec<Value> {
    events(log, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == workflow)
        .collect()
}

#[test]
fn a_wait_that_subscribes_resolves_on_its_own_occurrence_and_lets_go() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let wfs = format!("{}{}", waiter("one", "w1"), waiter("two", "w2"));
    let daemon = Daemon::start("events-wait-sub", &granted(&mock.uri(), MEMORY, "", &wfs));
    daemon.wait_for("both subscriptions polling", |_| {
        let p = polls(&mock);
        ["w1", "w2"]
            .iter()
            .all(|t| p.iter().any(|x| x["arguments"]["topic"] == *t))
    });
    let log = daemon.log();
    let subs = events(&log, "mcp.events.subscribed");
    assert_eq!(subs.len(), 2, "{log}");
    assert!(
        subs.iter().all(|e| e["owner"]["kind"] == "wait"),
        "{subs:?}"
    );
    // Only w1's occurrence: the run on w2 does not wake on it, though both
    // read the same stream.
    mock.control("mock.event", json!({"topic": "w1"}));
    daemon.wait_for("the first wait resolved", |l| !done(l, "one").is_empty());
    std::thread::sleep(Duration::from_millis(1500));
    let log = daemon.log();
    assert_eq!(done(&log, "one")[0]["output"], 1, "{log}");
    assert!(done(&log, "two").is_empty(), "the other wait woke:\n{log}");
    // Its subscription is let go with the wait; the other stays.
    daemon.wait_for("the first subscription let go", |l| {
        events(l, "mcp.events.unsubscribed")
            .iter()
            .any(|e| e["reason"] == "wait_left")
    });
    let at = polls(&mock)
        .iter()
        .filter(|p| p["arguments"]["topic"] == "w1")
        .count();
    std::thread::sleep(Duration::from_secs(3));
    let after = polls(&mock)
        .iter()
        .filter(|p| p["arguments"]["topic"] == "w1")
        .count();
    assert_eq!(at, after, "w1 is no longer polled");
    assert!(
        polls(&mock)
            .iter()
            .filter(|p| p["arguments"]["topic"] == "w2")
            .count()
            > 2,
        "w2 is still polled"
    );
}

#[test]
fn a_grants_max_is_free_again_the_moment_a_wait_leaves() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    // Two waits in a row under `max: 1`: the second subscribes as the first
    // resolves, not once a sweep has noticed it left.
    let wf = "  - name: pair\n    steps:\n      s: {kind: once}\n      a: {kind: wait, on: event, depends_on: [s], timeout: 120s, subscribe: {server: mock, event: mock.tick, arguments: {topic: m1}}}\n      b: {kind: wait, on: event, depends_on: [a], timeout: 120s, subscribe: {server: mock, event: mock.tick, arguments: {topic: m2}}}\n      f: {kind: finish, depends_on: [b], status: completed, output: \"{{ steps.b.output.data.payload.n }}\"}\n";
    let daemon = Daemon::start(
        "events-max",
        &granted(&mock.uri(), MEMORY, "          max: 1\n", wf),
    );
    let polled = |topic: &str| {
        polls(&mock)
            .iter()
            .any(|p| p["arguments"]["topic"] == topic && !p["cursor"].is_null())
    };
    daemon.wait_for("the first wait polling", |_| polled("m1"));
    mock.control("mock.event", json!({"topic": "m1"}));
    daemon.wait_for("the second wait polling", |_| polled("m2"));
    mock.control("mock.event", json!({"topic": "m2"}));
    daemon.wait_for("the run done", |l| !done(l, "pair").is_empty());
    let log = daemon.log();
    assert_eq!(done(&log, "pair")[0]["status"], "completed", "{log}");
    assert_eq!(done(&log, "pair")[0]["output"], 1, "{log}");
}

#[test]
fn a_wait_whose_subscription_is_refused_fails_instead_of_waiting() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    // The mock answers topic `forbidden` with -32024: the wait fails at once,
    // not at its two-minute timeout.
    let daemon = Daemon::start(
        "events-wait-refused",
        &granted(&mock.uri(), MEMORY, "", &waiter("denied", "forbidden")),
    );
    daemon.wait_for("the run done", |l| !done(l, "denied").is_empty());
    let log = daemon.log();
    let run = &done(&log, "denied")[0];
    assert_eq!(run["status"], "failed", "{log}");
    assert!(
        run["err"].to_string().contains("its subscription ended")
            || log.contains("wait subscribe: its subscription ended"),
        "{log}"
    );
    assert!(
        events(&log, "mcp.events.refused")
            .iter()
            .any(|e| e["reason"] == "forbidden"),
        "{log}"
    );
}

#[test]
fn a_wait_the_grant_no_longer_allows_fails_on_reload_and_at_restart() {
    // Reload: the grant now fixes an argument the subscription does not carry.
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let daemon = Daemon::start(
        "events-narrowed",
        &granted(&mock.uri(), MEMORY, "", &waiter("one", "q1")),
    );
    daemon.wait_for("the wait polling", |_| {
        polls(&mock).iter().any(|p| p["arguments"]["topic"] == "q1")
    });
    std::fs::write(
        &daemon.cfg_path,
        granted(
            &mock.uri(),
            MEMORY,
            "          fixed: {region: eu}\n",
            &waiter("one", "q1"),
        ),
    )
    .unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    daemon.wait_for("the run done", |l| !done(l, "one").is_empty());
    let log = daemon.log();
    assert_eq!(done(&log, "one")[0]["status"], "failed", "{log}");
    assert!(log.contains("no longer allows it"), "{log}");
    assert!(
        events(&log, "mcp.events.unsubscribed")
            .iter()
            .any(|e| e["reason"] == "grant_narrowed"),
        "{log}"
    );

    // Restart: the same edit made while the daemon was down.
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let dir = tempfile::tempdir().unwrap();
    let store = format!("  kind: file\n  file: {{path: {}}}\n", dir.path().display());
    let cfg_path = common::unique_path("events-narrowed-restart", "yaml");
    std::fs::write(
        &cfg_path,
        granted(&mock.uri(), &store, "", &waiter("one", "q2")),
    )
    .unwrap();
    let mut first = Daemon::start_at("events-narrowed-restart-1", &cfg_path);
    first.wait_for("the wait polling", |_| {
        polls(&mock)
            .iter()
            .any(|p| p["arguments"]["topic"] == "q2" && !p["cursor"].is_null())
    });
    first.stop();
    std::fs::write(
        &cfg_path,
        granted(
            &mock.uri(),
            &store,
            "          fixed: {region: eu}\n",
            &waiter("one", "q2"),
        ),
    )
    .unwrap();
    let second = Daemon::start_at("events-narrowed-restart-2", &cfg_path);
    second.wait_for("the run done", |l| !done(l, "one").is_empty());
    let log = second.log();
    assert_eq!(done(&log, "one")[0]["status"], "failed", "{log}");
    assert!(log.contains("no longer allows it"), "{log}");
}

#[test]
fn subscribing_twice_from_one_run_holds_once() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let wf = "  - name: twice\n    steps:\n      s: {kind: once}\n      t1: {kind: tool, depends_on: [s], name: events.subscribe, args: {server: mock, event: mock.tick, arguments: {topic: tr}}}\n      t2: {kind: tool, depends_on: [t1], name: events.subscribe, args: {server: mock, event: mock.tick, arguments: {topic: tr}}}\n      u: {kind: tool, depends_on: [t2], name: events.unsubscribe, args: {subscription: \"{{ steps.t2.output.subscription }}\"}}\n      f: {kind: finish, depends_on: [u], status: completed}\n";
    let daemon = Daemon::start(
        "events-twice",
        &granted(&mock.uri(), MEMORY, "          tools: true\n", wf),
    );
    daemon.wait_for("the run done", |l| !done(l, "twice").is_empty());
    // Past a sweep: a second hold left behind would be let go as the run
    // ended.
    std::thread::sleep(Duration::from_secs(6));
    let log = daemon.log();
    assert_eq!(done(&log, "twice")[0]["status"], "completed", "{log}");
    assert_eq!(events(&log, "mcp.events.subscribed").len(), 1, "{log}");
    let released: Vec<Value> = events(&log, "mcp.events.unsubscribed");
    assert_eq!(released.len(), 1, "{log}");
    assert_eq!(released[0]["reason"], "unsubscribed", "{log}");
}

#[test]
fn a_wait_subscribing_under_no_grant_is_refused_at_load() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    // A grant on another type: mock.tick has none.
    let cfg = granted(&mock.uri(), MEMORY, "", &waiter("w", "t")).replace(
        "        - name: mock.tick\n          delivery: [poll]",
        "        - name: mock.other\n          delivery: [poll]",
    );
    let path = common::unique_path("events-no-grant", "yaml");
    std::fs::write(&path, &cfg).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--validate-config", "--config", &path])
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stderr).to_string() + &String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("which no `event_grants` entry grants"),
        "{text}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_fixed_argument_cannot_be_set_at_runtime() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let cfg = granted(
        &mock.uri(),
        MEMORY,
        "          fixed: {topic: pinned}\n",
        &waiter("w", "mine"),
    );
    let daemon = Daemon::start("events-fixed", &cfg);
    daemon.wait_for("the step refused", |l| {
        events(l, "run.done").iter().any(|e| {
            e["workflow"] == "w"
                && e["err"]
                    .as_str()
                    .is_some_and(|m| m.contains("fixed by the operator's grant"))
        })
    });
    assert!(
        polls(&mock).is_empty(),
        "nothing subscribed:\n{}",
        mock.log()
    );
}

#[test]
fn the_events_tools_subscribe_list_deliver_and_unsubscribe() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let wf = "  - name: model\n    steps:\n      s: {kind: once}\n      l: {kind: tool, depends_on: [s], name: events.list, args: {}}\n      t: {kind: tool, depends_on: [l], name: events.subscribe, args: {server: mock, event: mock.tick, arguments: {topic: tt}}}\n      w: {kind: wait, on: event, depends_on: [t], stream: ticks, subject: tick, timeout: 120s}\n      u: {kind: tool, depends_on: [w], name: events.unsubscribe, args: {subscription: \"{{ steps.t.output.subscription }}\"}}\n      f: {kind: finish, depends_on: [u], status: completed, output: {n: \"{{ steps.w.output.data.payload.n }}\", listed: \"{{ steps.l.output.types }}\"}}\n";
    let daemon = Daemon::start(
        "events-tools",
        &granted(
            &mock.uri(),
            MEMORY,
            "          tools: true\n          lifetime: 1h\n",
            wf,
        ),
    );
    daemon.wait_for("the tool's subscription polling", |_| {
        polls(&mock).iter().any(|p| p["arguments"]["topic"] == "tt")
    });
    mock.control("mock.event", json!({"topic": "tt"}));
    daemon.wait_for("the run done", |l| !done(l, "model").is_empty());
    let log = daemon.log();
    let run = &done(&log, "model")[0];
    assert_eq!(run["status"], "completed", "{log}");
    assert_eq!(run["output"]["n"], 1, "{run}");
    assert!(
        run["output"]["listed"].to_string().contains("mock.tick"),
        "{run}"
    );
    let subscribed = events(&log, "mcp.events.subscribed");
    assert!(
        subscribed.iter().any(|e| e["owner"]["kind"] == "tool"),
        "{log}"
    );
    assert!(
        events(&log, "mcp.events.unsubscribed")
            .iter()
            .any(|e| e["reason"] == "unsubscribed"),
        "{log}"
    );
}

#[test]
fn a_waits_subscription_survives_a_restart() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let dir = tempfile::tempdir().unwrap();
    let store = format!("  kind: file\n  file: {{path: {}}}\n", dir.path().display());
    let cfg_path = common::unique_path("events-wait-restart", "yaml");
    std::fs::write(
        &cfg_path,
        granted(&mock.uri(), &store, "", &waiter("one", "r1")),
    )
    .unwrap();
    let mut first = Daemon::start_at("events-wait-restart-1", &cfg_path);
    first.wait_for("the wait's subscription polling", |_| {
        polls(&mock)
            .iter()
            .any(|p| p["arguments"]["topic"] == "r1" && !p["cursor"].is_null())
    });
    first.stop();
    // While it was down.
    mock.control("mock.event", json!({"topic": "r1"}));
    let second = Daemon::start_at("events-wait-restart-2", &cfg_path);
    second.wait_for("the wait resolved after the restart", |l| {
        !done(l, "one").is_empty()
    });
    assert_eq!(
        done(&second.log(), "one")[0]["output"],
        1,
        "{}",
        second.log()
    );
}

#[test]
fn the_events_tools_exist_only_where_a_grant_offers_them() {
    let mock = common::spawn_mock_mcp_at(REVISION, "mock://res", false);
    let wf = "  - name: model\n    steps:\n      s: {kind: once}\n      l: {kind: tool, depends_on: [s], name: events.list, args: {}}\n      f: {kind: finish, depends_on: [l], status: completed}\n";
    // A grant without `tools: true`.
    let path = common::unique_path("events-no-tools", "yaml");
    std::fs::write(&path, granted(&mock.uri(), MEMORY, "", wf)).unwrap();
    // Bounded: were the tool offered, the run would succeed and the daemon
    // would keep serving — the test must fail, not hang.
    let log = common::unique_path("events-no-tools", "log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let status = status.unwrap_or_else(|| panic!("agentd kept running: {text}"));
    assert_ne!(status.code(), Some(0), "{text}");
    assert!(
        text.contains("events.list") && text.contains("unknown, disabled or not granted"),
        "{text}"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&log);
}
