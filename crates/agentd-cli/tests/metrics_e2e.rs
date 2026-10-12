// SPDX-License-Identifier: AGPL-3.0-only
//! `/metrics` counts what happened, once, in the process that serves it.
//!
//! An agent's work happens in child processes — a turn worker per turn, a
//! process per subagent — and the scrape is the supervisor's. A series the
//! supervisor rendered with a value nothing wrote read as a healthy zero: a
//! model call made in a child, a bound that stopped a subagent, a refusal, a
//! stuck child killed, a reload refused. Each test here drives one kind of
//! event through a real daemon on the mock model and reads the series that
//! counts it from the supervisor's own scrape.
#![cfg(all(unix, feature = "metrics"))]

mod common;

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

/// A playbook for the mock model, in a file of its own.
fn playbook(body: Value) -> String {
    let path = common::unique_path("metrics-play", "json");
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

/// The value of `series` — a name, with its labels when it has any — in a
/// scrape; `None` when the scrape has no such sample.
fn sample(scrape: &str, series: &str) -> Option<u64> {
    scrape
        .lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

/// The sum of every sample of `family` that has labels.
fn family_total(scrape: &str, family: &str) -> u64 {
    let prefix = format!("{family}{{");
    scrape
        .lines()
        .filter(|l| l.starts_with(&prefix))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

/// A daemon on a configuration, its stderr in a file the test reads as it
/// goes; killed when the test ends, however it ends.
struct Daemon {
    child: Child,
    err_path: String,
    cfg_path: String,
}

impl Daemon {
    fn start(tag: &str, config: &str, env: &[(&str, &str)]) -> Daemon {
        let cfg_path = common::unique_path(tag, "yaml");
        std::fs::write(&cfg_path, config).expect("write config");
        let err_path = common::unique_path(tag, "log");
        let err = std::fs::File::create(&err_path).expect("stderr file");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
        cmd.args(["--config", &cfg_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn agentd");
        Daemon {
            child,
            err_path,
            cfg_path,
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    /// Wait for `pred` over the log, failing with `what` and the log.
    fn wait_for(&self, what: &str, secs: u64, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let log = self.log();
            if pred(&log) {
                return;
            }
            assert!(Instant::now() < deadline, "never saw {what}:\n{log}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The address the probe listener bound.
    fn addr(&self) -> String {
        self.wait_for("metrics.serving", 30, |log| {
            !events(log, "metrics.serving").is_empty()
        });
        events(&self.log(), "metrics.serving")[0]["addr"]
            .as_str()
            .expect("addr")
            .to_string()
    }

    fn scrape(&self) -> String {
        common::http_get(&self.addr(), "/metrics").body
    }

    /// Scrape until `pred` holds, failing with `what`, the scrape and the log.
    fn scrape_until(&self, what: &str, secs: u64, pred: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let scraped = self.scrape();
            if pred(&scraped) {
                return scraped;
            }
            assert!(
                Instant::now() < deadline,
                "never scraped {what}:\n{scraped}\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.cfg_path);
        let _ = std::fs::remove_file(&self.err_path);
    }
}

/// One run: a workflow step's turn, then a subagent its step bound stops —
/// both in child processes — and a resource subscription beside it. The run
/// counts once, by its status; the children's model calls, their
/// reachability and their loop steps reach the supervisor; the subagent's
/// bound counts; and the gauges show the tree and the work while they are
/// live.
#[test]
fn a_run_its_step_and_its_subagent_are_counted_in_the_supervisor() {
    let mock = common::spawn_mock_mcp(WATCHED, false);
    // The step answers after a while, so the run is seen in flight; the
    // subagent calls a tool on its one step, after a while, so it is seen
    // live, and its step bound ends it.
    let play = playbook(json!({
        "match": [{"when_contains": "SUB-TASK", "delay_ms": 2500,
                   "tool_calls": [{"name": "spin", "arguments": {}}]}],
        "turns": [{"content": "done", "delay_ms": 1500}]
    }));
    let endpoint = mock.uri();
    let daemon = Daemon::start(
        "metrics-run",
        &format!(
            "agent: {{ name: m }}\nstore: {{ kind: memory }}\n\
             observability: {{ log_level: info, metrics_addr: \"127.0.0.1:0\" }}\n\
             lifecycle: {{ run_until: drained }}\n\
             intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: m\n\
             \x20 budget: {{ lifetime_tokens: 1000000 }}\n\
             mcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     s: {{ kind: loop, max_iterations: 1 }}\n\
             \x20     work: {{ kind: agent, depends_on: [s], instruction: \"STEP-TASK\" }}\n\
             \x20     sub: {{ kind: subagent, depends_on: [work], mode: async, instruction: \"SUB-TASK\", limits: {{ steps: 1 }} }}\n\
             \x20     f: {{ kind: finish, depends_on: [sub], status: completed }}\n\
             \x20 - name: watch\n    steps:\n\
             \x20     on: {{ kind: subscribe, server: mock, uri: \"{WATCHED}\" }}\n\
             \x20     f: {{ kind: finish, depends_on: [on], status: completed }}\n"
        ),
        &[],
    );

    // The point-in-time gauges, at their highest while the work is live.
    let mut peak = std::collections::BTreeMap::<&str, u64>::new();
    let deadline = Instant::now() + Duration::from_secs(40);
    while events(&daemon.log(), "subagent.result").is_empty() {
        assert!(
            Instant::now() < deadline,
            "no subagent result:\n{}",
            daemon.log()
        );
        let scraped = daemon.scrape();
        for g in [
            "agent_inflight_reactions",
            "agent_active_subagents",
            "agent_tree_depth",
            "agent_tree_breadth",
        ] {
            let v = sample(&scraped, g).unwrap_or(0);
            let p = peak.entry(g).or_default();
            *p = (*p).max(v);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // A tick for the gauges to follow the child out.
    let scraped = daemon.scrape_until("the tree empty again", 10, |s| {
        sample(s, "agent_active_subagents") == Some(0)
    });
    let log = daemon.log();
    let result = events(&log, "subagent.result");
    assert_eq!(result[0]["status"], "exhausted_steps", "{log}");

    // The run, once, by its status (it was twice: a coarse projection, then
    // the precise one).
    assert_eq!(
        sample(&scraped, "agent_runs_total{status=\"completed\"}"),
        Some(1),
        "{scraped}"
    );
    assert_eq!(family_total(&scraped, "agent_runs_total"), 1, "{scraped}");
    assert_eq!(sample(&scraped, "agent_runs_completed_total"), Some(1));
    // The step's turn, under its own kind; no conversation turn ran.
    assert_eq!(
        sample(&scraped, "agent_turns_total{kind=\"step\"}"),
        Some(1),
        "{scraped}"
    );
    assert_eq!(
        sample(&scraped, "agent_turns_total{kind=\"root\"}"),
        Some(0)
    );
    // The children's model calls and loop steps, and what the last call
    // found of its endpoint: the supervisor makes none of these itself.
    assert!(
        sample(&scraped, "agent_intel_calls_total").is_some_and(|n| n >= 2),
        "{scraped}"
    );
    assert_eq!(sample(&scraped, "agent_intel_up"), Some(1), "{scraped}");
    assert!(
        sample(&scraped, "agent_loop_steps_total").is_some_and(|n| n >= 2),
        "{scraped}"
    );
    assert_eq!(family_total(&scraped, "agent_intel_errors_total"), 0);
    // The subagent's step bound stopped it.
    assert_eq!(
        sample(&scraped, "agent_limit_exceeded_total{limit=\"steps\"}"),
        Some(1),
        "{scraped}"
    );
    // The loop's one firing.
    assert_eq!(
        sample(&scraped, "agent_reactions_total"),
        Some(1),
        "{scraped}"
    );
    // The lifetime budget, less what the turn and the subagent spent.
    assert!(
        sample(&scraped, "agent_budget_tokens_remaining").is_some_and(|n| n > 0 && n < 1_000_000),
        "{scraped}"
    );
    // The resource subscription the `watch` start holds.
    assert_eq!(
        sample(&scraped, "agent_subscriptions_active{kind=\"resource\"}"),
        Some(1),
        "{scraped}"
    );
    // The run in flight while its step ran; one subagent, one deep, one wide,
    // while it lived.
    assert!(peak["agent_inflight_reactions"] >= 1, "{peak:?}");
    assert_eq!(peak["agent_active_subagents"], 1, "{peak:?}");
    assert_eq!(peak["agent_tree_depth"], 1, "{peak:?}");
    assert_eq!(peak["agent_tree_breadth"], 1, "{peak:?}");
    // What nothing writes is not on the surface.
    for gone in [
        "agent_pending_events",
        "agent_restarts_total",
        "agent_restarts_tripped_total",
    ] {
        assert!(
            !scraped.contains(&format!("# TYPE {gone} ")),
            "{gone} is rendered:\n{scraped}"
        );
    }
    let _ = std::fs::remove_file(&play);
}

/// Refusals count by the guard that refused, and the lifetime budget's end
/// counts once: a policy refuses a tool call, and a spent lifetime budget
/// refuses every turn after it.
#[test]
fn refusals_count_by_guard_and_a_spent_lifetime_budget_once() {
    let daemon = Daemon::start(
        "metrics-refuse",
        "agent: { name: r }\nstore: { kind: memory }\n\
         observability: { log_level: info, metrics_addr: \"127.0.0.1:0\" }\n\
         lifecycle: { run_until: drained }\n\
         intelligence:\n  endpoints: \"mock:final\"\n  model: m\n\
         \x20 budget: { lifetime_tokens: 1, lifetime_exhausted: refuse }\n\
         security:\n  policies:\n    - match: { tool: \"memory.set\" }\n      action: deny\n\
         workflows:\n  - name: deny\n    steps:\n\
         \x20     s: { kind: once }\n\
         \x20     m: { kind: memory.set, key: k, value: v, depends_on: [s] }\n\
         \x20     f: { kind: finish, depends_on: [m], status: completed }\n\
         \x20 - name: spend\n    steps:\n\
         \x20     s: { kind: loop, interval: 100ms }\n\
         \x20     t: { kind: agent, depends_on: [s], instruction: \"spend\" }\n\
         \x20     f: { kind: finish, depends_on: [t], status: completed }\n",
        &[],
    );
    let scraped = daemon.scrape_until("both refusals", 30, |s| {
        sample(s, "agent_refusals_total{reason=\"policy\"}") == Some(1)
            && sample(s, "agent_refusals_total{reason=\"budget\"}").is_some_and(|n| n >= 2)
    });
    assert_eq!(
        sample(
            &scraped,
            "agent_limit_exceeded_total{limit=\"tokens_lifetime\"}"
        ),
        Some(1),
        "once, however many turns it refuses:\n{scraped}"
    );
    assert!(
        sample(&scraped, "agent_budget_tokens_remaining").is_some_and(|n| n <= 1),
        "{scraped}"
    );
    assert!(
        sample(&scraped, "agent_runs_total{status=\"failed\"}").is_some_and(|n| n >= 1),
        "{scraped}"
    );
    assert_eq!(events(&daemon.log(), "budget.lifetime_exhausted").len(), 1);
}

/// A child that stops answering is killed by the liveness check, and the
/// kill counts.
#[test]
fn a_stuck_child_killed_by_the_liveness_check_counts() {
    let play = playbook(json!({"turns": [{"content": "late", "delay_ms": 20000}]}));
    let daemon = Daemon::start(
        "metrics-stuck",
        &format!(
            "agent: {{ name: s }}\nstore: {{ kind: memory }}\n\
             observability: {{ log_level: info, metrics_addr: \"127.0.0.1:0\" }}\n\
             lifecycle: {{ run_until: drained }}\n\
             intelligence: {{ endpoints: \"mock:file:{play}\", model: m }}\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     sub: {{ kind: subagent, depends_on: [s], mode: async, instruction: \"wait\" }}\n\
             \x20     f: {{ kind: finish, depends_on: [sub], status: completed }}\n"
        ),
        // Stuck after half a second without events or pongs.
        &[
            ("AGENTD_PROGRESS_TIMEOUT_MS", "500"),
            ("AGENTD_PONG_TIMEOUT_MS", "500"),
        ],
    );
    daemon.wait_for("the subagent", 30, |log| {
        !events(log, "subagent.spawn").is_empty()
    });
    let pid = events(&daemon.log(), "subagent.spawn")[0]["pid"]
        .as_i64()
        .expect("pid") as i32;
    let before = daemon.scrape();
    assert_eq!(
        sample(&before, "agent_subagent_stuck_kills_total{signal=\"kill\"}"),
        Some(0)
    );
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let scraped = daemon.scrape_until("the kill", 30, |s| {
        sample(s, "agent_subagent_stuck_kills_total{signal=\"kill\"}") == Some(1)
    });
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert!(!events(&daemon.log(), "child.unhealthy").is_empty());
    assert!(
        sample(
            &scraped,
            "agent_subagents_exited_total{status=\"cancelled\"}"
        )
        .is_some_and(|n| n >= 1),
        "{scraped}"
    );
    let _ = std::fs::remove_file(&play);
}

/// A supervisor loop that went quiet past the `/healthz` window counts a
/// stall when it ticks again.
#[test]
fn a_supervisor_that_stalled_counts_when_it_comes_back() {
    let daemon = Daemon::start(
        "metrics-stall",
        "agent: { name: t }\nstore: { kind: memory }\n\
         observability: { log_level: info, metrics_addr: \"127.0.0.1:0\" }\n\
         lifecycle: { run_until: drained }\n\
         intelligence: { endpoints: \"mock:final\", model: m }\n\
         workflows:\n  - name: w\n    steps:\n\
         \x20     s: { kind: manual }\n\
         \x20     f: { kind: finish, depends_on: [s], status: completed }\n",
        &[],
    );
    // Stalls count once the loop runs: a slow start is not one.
    daemon.wait_for("the loop", 30, |log| !events(log, "proc.ready").is_empty());
    let before = daemon.scrape();
    assert_eq!(sample(&before, "agent_reactor_stalls_total"), Some(0));
    // The whole process held past the window, as a wedged loop would be.
    unsafe { libc::kill(daemon.pid(), libc::SIGSTOP) };
    std::thread::sleep(Duration::from_millis(6_000));
    unsafe { libc::kill(daemon.pid(), libc::SIGCONT) };
    let scraped = daemon.scrape_until("the stall", 10, |s| {
        sample(s, "agent_reactor_stalls_total").is_some_and(|n| n >= 1)
    });
    assert_eq!(
        sample(&scraped, "agent_reactor_stalls_total"),
        Some(1),
        "{scraped}"
    );
}

/// A subagent a restarted supervisor finds unfinished in its store is spawned
/// again, and that counts as a restart.
#[test]
fn a_restored_subagent_spawned_again_counts_a_restart() {
    let play = playbook(json!({"turns": [{"content": "late", "delay_ms": 20000}]}));
    let store = common::unique_path("metrics-restore-store", "d");
    let config = |metrics: &str| {
        format!(
            "agent: {{ name: o }}\nstore: {{ kind: file, file: {{ path: {store} }} }}\n\
             observability: {{ log_level: info{metrics} }}\n\
             lifecycle: {{ run_until: drained }}\n\
             intelligence: {{ endpoints: \"mock:file:{play}\", model: m }}\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     sub: {{ kind: subagent, depends_on: [s], mode: async, durable: true, instruction: \"wait\" }}\n\
             \x20     f: {{ kind: finish, depends_on: [sub], status: completed }}\n"
        )
    };
    {
        let first = Daemon::start("metrics-restore", &config(""), &[]);
        // The run ends once its async subagent is spawned and on record.
        first.wait_for("the run", 30, |log| {
            !events(log, "subagent.spawn").is_empty() && !events(log, "run.done").is_empty()
        });
        // Killed with its subagent unfinished.
    }
    let second = Daemon::start(
        "metrics-restore",
        &config(", metrics_addr: \"127.0.0.1:0\""),
        &[],
    );
    second.wait_for("the respawn", 30, |log| {
        !events(log, "subagent.respawn").is_empty()
    });
    let scraped = second.scrape();
    assert_eq!(
        sample(
            &scraped,
            "agent_subagent_restarts_total{reason=\"restored\"}"
        ),
        Some(1),
        "{scraped}"
    );
    drop(second);
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_file(&play);
}

/// A model that cannot be reached: a subagent's failed calls reach the
/// supervisor by reason, `agent_intel_up` follows the last call from the up
/// it was, and the all-down a child reports is the gauge's as it is
/// `/readyz`'s.
#[test]
fn an_unreachable_model_counts_its_errors_and_its_outage() {
    let port = common::free_port();
    let daemon = Daemon::start(
        "metrics-down",
        &format!(
            "agent: {{ name: d }}\nstore: {{ kind: memory }}\n\
             observability: {{ log_level: info, metrics_addr: \"127.0.0.1:0\" }}\n\
             lifecycle: {{ run_until: drained }}\n\
             intelligence:\n  endpoints: \"mock:final\"\n  model: m\n\
             \x20 providers:\n    down: {{ endpoints: \"http://127.0.0.1:{port}\" }}\n\
             \x20 models:\n    gone: {{ model: g, provider: down }}\n\
             subagents: {{ defaults: {{ model: gone }} }}\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     ok: {{ kind: agent, depends_on: [s], instruction: \"say done\" }}\n\
             \x20     sub: {{ kind: subagent, depends_on: [ok], mode: warm, instruction: \"wait\" }}\n\
             \x20     f: {{ kind: finish, depends_on: [sub], status: completed }}\n"
        ),
        &[],
    );
    // The step's call found its endpoint up.
    daemon.scrape_until("the first call up", 30, |s| {
        sample(s, "agent_intel_up") == Some(1)
    });
    // The warm subagent backs off and tries again until every endpoint it
    // has is down, and says so.
    let scraped = daemon.scrape_until("the outage", 30, |s| {
        sample(s, "agent_intel_all_down") == Some(1)
    });
    assert_eq!(sample(&scraped, "agent_intel_up"), Some(0), "{scraped}");
    assert_eq!(sample(&scraped, "agent_ready"), Some(0), "{scraped}");
    // Three failed sweeps open the endpoint's breaker; the report that says
    // so carries the two before it, and the third goes up with the next frame.
    assert!(
        sample(&scraped, "agent_intel_errors_total{reason=\"unreachable\"}")
            .is_some_and(|n| n >= 2),
        "{scraped}"
    );
    assert!(
        sample(&scraped, "agent_intel_calls_total").is_some_and(|n| n >= 2),
        "{scraped}"
    );
}

/// Events that wait in the inbox behind a busy workflow show their age.
#[test]
fn events_waiting_in_the_inbox_show_their_age() {
    let daemon = Daemon::start(
        "metrics-lag",
        "agent: { name: l }\nstore: { kind: memory }\n\
         observability: { log_level: info, metrics_addr: \"127.0.0.1:0\" }\n\
         lifecycle: { run_until: drained }\n\
         intelligence: { endpoints: \"mock:final\", model: m }\n\
         workflows:\n  - name: w\n    concurrency: { max_runs: 1 }\n    steps:\n\
         \x20     s: { kind: schedule, every: 200ms }\n\
         \x20     n: { kind: sleep, depends_on: [s], duration: 30s }\n\
         \x20     f: { kind: finish, depends_on: [n], status: completed }\n",
        &[],
    );
    let scraped = daemon.scrape_until("a second's lag", 30, |s| {
        sample(s, "agent_reaction_lag_ms").is_some_and(|n| n >= 1_000)
    });
    assert!(
        sample(&scraped, "agent_inbox_pending").is_some_and(|n| n >= 1),
        "{scraped}"
    );
    assert!(
        sample(&scraped, "agent_reactions_total").is_some_and(|n| n >= 2),
        "every firing counts, queued or not:\n{scraped}"
    );
}

/// A reload that is refused — invalid, or needing a restart — counts as
/// `rejected`.
#[cfg(feature = "hot-reload")]
#[test]
fn a_refused_reload_counts_rejected() {
    let base = "agent: { name: h }\nstore: { kind: memory }\n\
         observability: { log_level: info, metrics_addr: \"127.0.0.1:0\" }\n\
         intelligence: { endpoints: \"mock:final\", model: m }\n\
         workflows:\n  - name: w\n    steps:\n\
         \x20     s: { kind: manual }\n\
         \x20     f: { kind: finish, depends_on: [s], status: completed }\n";
    let daemon = Daemon::start(
        "metrics-reload",
        &format!("{base}lifecycle: {{ run_until: drained }}\n"),
        &[],
    );
    daemon.scrape();
    let reload = |text: &str, event: &str, n: usize| {
        std::fs::write(&daemon.cfg_path, text).unwrap();
        unsafe { libc::kill(daemon.pid(), libc::SIGHUP) };
        daemon.wait_for(event, 20, |log| events(log, event).len() >= n);
    };
    reload("agent: [", "config.reload.invalid", 1);
    reload(
        &format!("{base}lifecycle: {{ run_until: idle }}\n"),
        "config.reload.restart_required",
        1,
    );
    let scraped = daemon.scrape_until("two rejections", 10, |s| {
        sample(s, "agent_config_reload_total{result=\"rejected\"}") == Some(2)
    });
    assert_eq!(
        sample(&scraped, "agent_config_reload_total{result=\"other\"}"),
        Some(0),
        "{scraped}"
    );
    assert_eq!(
        sample(&scraped, "agent_config_reload_total{result=\"applied\"}"),
        Some(0)
    );
}
