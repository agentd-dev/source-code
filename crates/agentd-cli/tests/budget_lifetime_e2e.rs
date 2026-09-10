// SPDX-License-Identifier: AGPL-3.0-only
//! **A spent lifetime token budget ends the instance**
//! (`intelligence.budget.lifetime_exhausted`).
//!
//! `on_exhausted` governs a WINDOW, where the answer can be "wait" because the
//! window resets. A lifetime ceiling never resets, so the only question left is
//! what happens to the process — and the answer used to be nothing: every
//! admission failed and the daemon sat there refusing work until an operator
//! noticed a gauge. A budget that cannot end anything is not a budget.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A loop that spends its whole lifetime allowance on the first turn, so the
/// second admission trips the ceiling. `extra` goes inside `budget:`.
fn cfg(extra: &str) -> String {
    format!(
        "config_version: \"1\"\nagent: {{ name: broke }}\nstore: {{ kind: memory }}\n\
         intelligence:\n  endpoints: \"mock:final\"\n  model: mock\n\
         \x20 budget: {{ lifetime_tokens: 1{extra} }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 30s }}\n\
         observability: {{ log_level: info }}\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s: {{ kind: loop, interval: 100ms }}\n\
        \x20     t: {{ kind: agent, depends_on: [s], instruction: \"spend\" }}\n\
        \x20     f: {{ kind: finish, depends_on: [t], status: completed }}\n"
    )
}

/// Boot and wait up to `secs` for the process to exit ON ITS OWN. `None` is a
/// real answer, not a timeout failure: `refuse` is supposed to keep it up.
fn run(cfg: &str, secs: u64) -> (Option<i32>, String) {
    let path = common::unique_path("budget-life", "yaml");
    std::fs::write(&path, cfg).unwrap();
    let err_path = common::unique_path("budget-life", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", &path])
        .stdout(Stdio::null())
        .stderr(errf)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut code = None;
    while Instant::now() < deadline {
        if let Ok(Some(st)) = child.try_wait() {
            code = st.code();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    for p in [path, err_path] {
        let _ = std::fs::remove_file(&p);
    }
    (code, log)
}

/// The default: finish live work, exit 0, and let an orchestrator restart the
/// instance with a fresh window — which is what a lifetime budget is for.
#[test]
fn the_default_drains_and_exits_zero() {
    let (code, log) = run(&cfg(""), 25);
    let ev = events(&log, "budget.lifetime_exhausted");
    assert_eq!(ev.len(), 1, "logged once, not once per admission:\n{log}");
    assert_eq!(ev[0]["policy"], "drain", "{log}");
    assert!(
        ev[0]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("lifetime"),
        "{log}"
    );
    assert_eq!(code, Some(0), "a drain is a clean exit:\n{log}");
}

/// `exit` stops now, with the budget code rather than 0 — for a deployment
/// where overrunning the ceiling is a failure to notice, not a lifecycle.
#[test]
fn exit_stops_now_with_the_budget_code() {
    let (code, log) = run(&cfg(", lifetime_exhausted: exit"), 25);
    assert_eq!(
        events(&log, "budget.lifetime_exhausted")[0]["policy"],
        "exit",
        "{log}"
    );
    assert_eq!(code, Some(7), "exit::BUDGET:\n{log}");
}

/// `refuse` is the pre-1.15 behaviour, kept for an operator who would rather
/// inspect a stopped instance than lose it.
#[test]
fn refuse_keeps_the_instance_up() {
    let (code, log) = run(&cfg(", lifetime_exhausted: refuse"), 8);
    let ev = events(&log, "budget.lifetime_exhausted");
    assert_eq!(ev.len(), 1, "{log}");
    assert_eq!(ev[0]["policy"], "refuse", "{log}");
    // Still running when the wait expired — the whole contract of `refuse`.
    assert_eq!(code, None, "refuse must not end the instance:\n{log}");
    assert!(!log.contains("drain.start"), "and must not drain:\n{log}");
}
