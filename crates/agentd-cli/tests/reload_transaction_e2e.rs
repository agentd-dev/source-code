// SPDX-License-Identifier: AGPL-3.0-only
//! A reload applies completely or not at all.
//!
//! Three checks can only run against the servers and the tool registry a
//! reload builds: a workflow step naming a tool or server they lack, a `uri:`
//! workflow document read through a server the same reload adds, and the
//! registry build itself. They used to run after the new instruction, MCP
//! servers and registry were already live, so a reload they refused kept only
//! the running workflows and left everything else on the refused config. Now
//! the reload is staged beside the running state — the new servers dialed
//! into a set of their own, everything checked against it — and switched in
//! only when all of it passed.
//!
//! Each refusal below is followed by the same probe: put the original config
//! back and reload. Had the refused reload left any section applied, that
//! reload would report moving it back.
#![cfg(all(feature = "hot-reload", unix))]

mod common;

use serde_json::Value;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    err_path: String,
}

impl Daemon {
    fn spawn(cfg: &std::path::Path) -> Daemon {
        Daemon::spawn_env(cfg, &[])
    }
    fn spawn_env(cfg: &std::path::Path, env: &[(&str, &str)]) -> Daemon {
        let err_path = common::unique_path("reload-tx-daemon", "log");
        let errf = std::fs::File::create(&err_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg.to_string_lossy()])
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errf))
            .spawn()
            .expect("spawn daemon");
        Daemon { child, err_path }
    }
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }
    fn wait_for(&self, pred: impl Fn(&str) -> bool, what: &str, secs: u64) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let log = self.stderr();
            if pred(&log) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    /// Send SIGHUP and wait for the reload's outcome — the `nth` (0-based)
    /// `config.reloaded` or `config.reload.invalid` line of the whole log.
    fn reload(&self, nth: usize) -> String {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) };
        self.wait_for(|l| outcomes(l).len() > nth, "the reload's outcome", 20)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.err_path);
    }
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// Every reload outcome, in order. A refused reload logs one
/// `config.reload.invalid` line per error, so consecutive ones count once.
fn outcomes(log: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut last_invalid = false;
    for v in log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        if v["event"] == "config.reloaded" {
            out.push(v);
            last_invalid = false;
        } else if v["event"] == "config.reload.invalid" {
            if !last_invalid {
                out.push(v);
            }
            last_invalid = true;
        }
    }
    out
}

fn changed(v: &Value) -> Vec<String> {
    v["changed"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn done_with(log: &str, output: &str) -> usize {
    events(log, "run.done")
        .iter()
        .filter(|e| e["output"] == output)
        .count()
}

/// The server-side record of `tool` calls that reached a mock.
fn calls(mock: &common::MockMcp, tool: &str) -> usize {
    mock.log()
        .lines()
        .filter(|l| l.starts_with("MOCK_CALL ") && l.contains(&format!("\"name\":\"{tool}\"")))
        .count()
}

/// A scheduled workflow that calls server `a` on every tick: server a still
/// answering is this workflow's runs still completing.
const TICK: &str = "  - name: tick\n    steps:\n      s: {kind: schedule, every: 300ms}\n      c: {kind: mcp.tool, depends_on: [s], server: a, tool: mock.ops}\n      f: {kind: finish, depends_on: [c], status: completed, output: tick}\n";

/// A long-lived config: `servers` as (name, endpoint), each namespaced by its
/// name; `workflows` as YAML list items; `extra` any further sections.
fn config(instruction: &str, servers: &[(&str, &str)], workflows: &[&str], extra: &str) -> String {
    let mut out = format!(
        "agent:\n  name: tx\n  instruction: {instruction}\n\
         store:\n  kind: memory\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n"
    );
    if !servers.is_empty() {
        out.push_str("mcp:\n  servers:\n");
        for (name, endpoint) in servers {
            out.push_str(&format!(
                "    - {{name: {name}, endpoint: \"{endpoint}\", ns: {name}}}\n"
            ));
        }
    }
    if !workflows.is_empty() {
        out.push_str("workflows:\n");
        for w in workflows {
            out.push_str(w);
        }
    }
    out.push_str(extra);
    out
}

/// A refused reload: the refusal names `reason`, and the server the reload
/// dialed is closed rather than joined. Returns the log.
fn assert_refused(d: &Daemon, nth: usize, reason: &str, dialed: &str) -> String {
    let log = d.reload(nth);
    let outcome = &outcomes(&log)[nth];
    assert_eq!(
        outcome["event"], "config.reload.invalid",
        "the reload was not refused:\n{log}"
    );
    assert!(
        events(&log, "config.reload.invalid")
            .iter()
            .any(|e| e["error"].as_str().is_some_and(|m| m.contains(reason))),
        "no refusal naming {reason:?}:\n{log}"
    );
    assert!(
        events(&log, "mcp.disconnect")
            .iter()
            .any(|e| e["server"] == dialed && e["reason"] == "reload refused"),
        "the server the refused reload dialed was not closed:\n{log}"
    );
    assert!(
        events(&log, "mcp.connect")
            .iter()
            .all(|e| e["server"] != dialed),
        "the refused reload's server joined the running set:\n{log}"
    );
    log
}

/// After a refusal: server `a` still answers on the connection it had (its
/// workflow keeps completing), and restoring the original config reloads to
/// "nothing" — no section of the refused reload had applied — and closes no
/// connection the refused reload opened.
fn assert_nothing_applied(
    d: &Daemon,
    a: &common::MockMcp,
    cfg: &std::path::Path,
    original: &str,
    nth: usize,
    dialed: &str,
) {
    let log = d.stderr();
    let (ticks, a_calls) = (done_with(&log, "tick"), calls(a, "mock.ops"));
    d.wait_for(
        |l| done_with(l, "tick") >= ticks + 2,
        "server a answering on after the refusal",
        15,
    );
    assert!(calls(a, "mock.ops") >= a_calls + 2, "{}", a.log());

    std::fs::write(cfg, original).unwrap();
    let log = d.reload(nth + 1);
    let restored = &outcomes(&log)[nth + 1];
    assert_eq!(restored["event"], "config.reloaded", "{log}");
    assert_eq!(
        changed(restored),
        ["nothing"],
        "the refused reload had applied these sections:\n{log}"
    );
    assert!(
        events(&log, "mcp.disconnect")
            .iter()
            .all(|e| !(e["server"] == dialed && e["reason"] == "reload")),
        "the refused reload's server had been live:\n{log}"
    );
}

/// (a) A reload that adds server b and a workflow step naming a tool b does
/// not provide is refused: b's staged connection is closed, server a is
/// still connected and answering, and the instruction, servers, tools and
/// workflows are the running ones.
#[test]
fn a_step_naming_a_tool_the_new_server_lacks_refuses_the_reload_and_nothing_applies() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let b = common::spawn_mock_mcp("mock://b", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let original = config("Keep ticking.", &[("a", &a.uri())], &[TICK], "");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);

    let uses_b = "  - name: uses-b\n    steps:\n      s: {kind: manual}\n      x: {kind: tool, depends_on: [s], name: b.nosuch}\n      f: {kind: finish, depends_on: [x], status: completed}\n";
    std::fs::write(
        &cfg,
        config(
            "A different instruction.",
            &[("a", &a.uri()), ("b", &b.uri())],
            &[TICK, uses_b],
            "",
        ),
    )
    .unwrap();
    assert_refused(&d, 0, "tool \"b.nosuch\" is unknown", "b");
    assert_nothing_applied(&d, &a, &cfg, &original, 0, "b");
}

/// A reload that takes away a server a running workflow still calls is
/// refused the same way — the workflows are checked against the servers the
/// reload would run on even when the workflow entries did not change — and
/// the running connection to it stays.
#[test]
fn removing_a_server_a_workflow_calls_refuses_the_reload_and_its_connection_stays() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let b = common::spawn_mock_mcp("mock://b", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let original = config("Keep ticking.", &[("a", &a.uri())], &[TICK], "");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);

    // a is swapped for b; `tick` still calls a.
    std::fs::write(
        &cfg,
        config("Keep ticking.", &[("b", &b.uri())], &[TICK], ""),
    )
    .unwrap();
    assert_refused(&d, 0, "mcp server \"a\" is not connected", "b");
    assert_nothing_applied(&d, &a, &cfg, &original, 0, "b");
}

/// (b) A `uri:` workflow document read through a server the same reload adds,
/// which does not serve it, refuses the reload with nothing applied.
#[test]
fn a_uri_workflow_the_new_server_cannot_serve_refuses_the_reload_and_nothing_applies() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let b = common::spawn_mock_mcp("mock://b", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let original = config("Keep ticking.", &[("a", &a.uri())], &[TICK], "");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);

    let from_b = "  - name: from-b\n    uri: mcp://b/workflows://missing.yaml\n";
    std::fs::write(
        &cfg,
        config(
            "A different instruction.",
            &[("a", &a.uri()), ("b", &b.uri())],
            &[TICK, from_b],
            "",
        ),
    )
    .unwrap();
    let log = assert_refused(&d, 0, "workflow uri mcp://b/workflows://missing.yaml", "b");
    // It was read through b — the staged server — not refused for want of
    // one: the reload dialed b and asked it.
    assert_eq!(b.reads("workflows://missing.yaml"), 1, "{log}");
    assert_nothing_applied(&d, &a, &cfg, &original, 0, "b");
}

/// (c) A tool registry that fails to build refuses the reload with nothing
/// applied — not the tools section alone.
#[test]
fn a_registry_that_fails_to_build_refuses_the_reload_and_nothing_applies() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let b = common::spawn_mock_mcp("mock://b", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let original = config("Keep ticking.", &[("a", &a.uri())], &[TICK], "");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);

    std::fs::write(
        &cfg,
        config(
            "A different instruction.",
            &[("a", &a.uri()), ("b", &b.uri())],
            &[TICK],
            "tools:\n  disabled: [b.no-such-tool]\n",
        ),
    )
    .unwrap();
    assert_refused(
        &d,
        0,
        "tools.disabled names an unknown tool \"b.no-such-tool\"",
        "b",
    );
    assert_nothing_applied(&d, &a, &cfg, &original, 0, "b");
}

/// (d) A valid reload that adds and removes servers and workflows applies all
/// of it and reports every section: the new server is dialed and its
/// workflow runs on it, the removed server is closed, the unchanged one keeps
/// its connection, the removed workflow unloads, and the workflow tools stay
/// registered on the rebuilt registry.
#[test]
fn a_valid_reload_with_server_and_workflow_changes_applies_everything() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let b = common::spawn_mock_mcp("mock://b", false);
    let c = common::spawn_mock_mcp("mock://c", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let probe = "  - name: probe\n    tool: {name: probe.run, mode: async}\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], status: completed}\n";
    let old = "  - name: old\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], status: completed}\n";
    std::fs::write(
        &cfg,
        config(
            "Keep ticking.",
            &[("a", &a.uri()), ("c", &c.uri())],
            &[TICK, probe, old],
            "",
        ),
    )
    .unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);
    assert_eq!(events(&d.stderr(), "registry.workflow_tools").len(), 1);

    let on_b = "  - name: on-b\n    steps:\n      s: {kind: schedule, every: 300ms}\n      c: {kind: mcp.tool, depends_on: [s], server: b, tool: mock.ops}\n      f: {kind: finish, depends_on: [c], status: completed, output: on-b}\n";
    std::fs::write(
        &cfg,
        config(
            "A different instruction.",
            &[("a", &a.uri()), ("b", &b.uri())],
            &[TICK, probe, on_b],
            "",
        ),
    )
    .unwrap();
    let log = d.reload(0);
    let outcome = &outcomes(&log)[0];
    assert_eq!(outcome["event"], "config.reloaded", "{log}");
    let changed = changed(outcome);
    for section in ["agent.instruction", "mcp", "tools", "workflows"] {
        assert!(
            changed.iter().any(|c| c == section),
            "{section} not reported: {changed:?}\n{log}"
        );
    }
    let connects: Vec<Value> = events(&log, "mcp.connect");
    assert!(
        connects
            .iter()
            .any(|e| e["server"] == "b" && e["reason"] == "reload"),
        "{log}"
    );
    assert!(
        connects
            .iter()
            .all(|e| !(e["server"] == "a" && e["reason"] == "reload")),
        "the unchanged server was re-dialed:\n{log}"
    );
    assert!(
        events(&log, "mcp.disconnect")
            .iter()
            .any(|e| e["server"] == "c" && e["reason"] == "reload"),
        "{log}"
    );
    assert!(
        events(&log, "workflow.unloaded")
            .iter()
            .any(|e| e["workflow"] == "old" && e["reason"] == "removed"),
        "{log}"
    );
    assert_eq!(
        events(&log, "registry.workflow_tools")
            .iter()
            .filter(|e| e["tool"] == "probe.run")
            .count(),
        2,
        "the rebuilt registry dropped the workflow tool:\n{log}"
    );
    // The new workflow runs on the new server, and the old one keeps going.
    let log = d.wait_for(
        |l| done_with(l, "on-b") >= 2,
        "the new workflow running on the new server",
        15,
    );
    assert!(calls(&b, "mock.ops") >= 2, "{}", b.log());
    let ticks = done_with(&log, "tick");
    d.wait_for(|l| done_with(l, "tick") >= ticks + 2, "a still ticking", 15);
}

/// (f) A run in flight on a server the reload removes: the call already made
/// finishes on the connection it was made on, and a later step that needs
/// the server fails — `mcp server "a" is not connected` — rather than
/// reaching a connection the configuration no longer names.
#[test]
fn a_run_in_flight_on_a_removed_server_finishes_its_call_and_fails_its_next() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let slow = "  - name: slow\n    steps:\n      s: {kind: once}\n      c1: {kind: mcp.tool, depends_on: [s], server: a, tool: mock.slow, args: {ms: 2500}}\n      c2: {kind: mcp.tool, depends_on: [c1], server: a, tool: mock.ops}\n      f: {kind: finish, depends_on: [c2], status: completed, output: slow}\n";
    std::fs::write(&cfg, config("Work.", &[("a", &a.uri())], &[slow], "")).unwrap();
    let d = Daemon::spawn(&cfg);
    let deadline = Instant::now() + Duration::from_secs(15);
    while calls(&a, "mock.slow") == 0 {
        assert!(Instant::now() < deadline, "the slow call never reached a");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Server a and the workflow go; the run keeps its pinned definition.
    std::fs::write(&cfg, config("Work.", &[], &[], "")).unwrap();
    let log = d.reload(0);
    let outcome = &outcomes(&log)[0];
    assert_eq!(outcome["event"], "config.reloaded", "{log}");
    assert!(changed(outcome).iter().any(|c| c == "mcp"), "{log}");

    let log = d.wait_for(
        |l| {
            events(l, "run.done")
                .iter()
                .any(|e| e["workflow"] == "slow")
        },
        "the slow run to finish",
        15,
    );
    let step = |id: &str| {
        events(&log, "step.done")
            .into_iter()
            .find(|e| e["step"] == id)
            .unwrap_or_else(|| panic!("no step.done for {id}:\n{log}"))
    };
    assert_eq!(step("c1")["status"], "done", "{log}");
    assert_eq!(step("c2")["status"], "failed", "{log}");
    assert!(
        step("c2")["err"]
            .as_str()
            .is_some_and(|e| e.contains("mcp server \"a\" is not connected")),
        "{log}"
    );
    let run = events(&log, "run.done")
        .into_iter()
        .find(|e| e["workflow"] == "slow")
        .unwrap();
    assert_ne!(run["status"], "completed", "{log}");
    assert_eq!(calls(&a, "mock.ops"), 0, "{}", a.log());
}

/// A `url:` workflow source that fails to fetch is named by scheme, host and
/// path — at startup and in a reload's refusal — and the fetch error is
/// scrubbed the same way: a credential in the URL's userinfo or query reaches
/// neither the log nor the refusal.
#[test]
fn a_failing_url_source_never_logs_the_credential_in_its_url() {
    let url = "http://ops:hunter2pw@127.0.0.1:9/defs/tick.yaml?token=s3cr3tvalue";
    let leaked = |log: &str| log.contains("hunter2pw") || log.contains("s3cr3tvalue");
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let entry = format!("  - name: remote\n    url: \"{url}\"\n    allow_private: true\n");

    // At startup: the definitions are refused, exit 2.
    std::fs::write(&cfg, config("Work.", &[], &[&entry], "")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg.to_string_lossy()])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{log}");
    assert!(
        log.contains("workflow url http://127.0.0.1:9/defs/tick.yaml"),
        "{log}"
    );
    assert!(
        !leaked(&log),
        "the URL's credential reached the log:\n{log}"
    );

    // In a reload's refusal.
    let original = config(
        "Work.",
        &[],
        &[
            "  - name: idle\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], status: completed}\n",
        ],
        "",
    );
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| !events(l, "proc.ready").is_empty(), "the daemon", 15);
    std::fs::write(&cfg, config("Work.", &[], &[&entry], "")).unwrap();
    let log = d.reload(0);
    assert_eq!(outcomes(&log)[0]["event"], "config.reload.invalid", "{log}");
    assert!(
        log.contains("workflow url http://127.0.0.1:9/defs/tick.yaml"),
        "{log}"
    );
    assert!(
        !leaked(&log),
        "the URL's credential reached the log:\n{log}"
    );
}

/// A resource instruction is read through the servers the reload would run
/// on, so one served by a server the same reload adds applies — it used to
/// be read before that server was dialed, fail, and leave the old text while
/// the reload reported `agent.instruction`. One nothing serves refuses the
/// reload, as it refuses a start.
#[test]
fn a_resource_instruction_is_read_through_the_server_the_reload_adds() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let b = common::spawn_mock_mcp("mock://b", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let original = config("Keep ticking.", &[("a", &a.uri())], &[TICK], "");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);

    // Nothing serves it: b is not configured.
    std::fs::write(
        &cfg,
        config(
            "mcp://b/mock://instruction",
            &[("a", &a.uri())],
            &[TICK],
            "",
        ),
    )
    .unwrap();
    let log = d.reload(0);
    assert_eq!(outcomes(&log)[0]["event"], "config.reload.invalid", "{log}");
    assert!(
        events(&log, "config.reload.invalid")
            .iter()
            .any(|e| e["error"]
                .as_str()
                .is_some_and(|m| m.contains("agent.instruction mcp://b/mock://instruction"))),
        "{log}"
    );

    // b arrives in the same reload: the instruction is read through it.
    std::fs::write(
        &cfg,
        config(
            "mcp://b/mock://instruction",
            &[("a", &a.uri()), ("b", &b.uri())],
            &[TICK],
            "",
        ),
    )
    .unwrap();
    let log = d.reload(1);
    let outcome = &outcomes(&log)[1];
    assert_eq!(outcome["event"], "config.reloaded", "{log}");
    assert!(
        changed(outcome).iter().any(|c| c == "agent.instruction"),
        "{log}"
    );
    assert!(
        events(&log, "instruction.loaded")
            .iter()
            .any(|e| e["server"] == "b" && e["uri"] == "mock://instruction"),
        "{log}"
    );
    assert_eq!(b.reads("mock://instruction"), 1, "{}", b.log());
}

/// An intelligence token file that cannot be read refuses the reload, as it
/// refuses a start; the new endpoint used to go live with the old
/// credential.
#[test]
fn an_unreadable_intelligence_token_refuses_the_reload() {
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let intel = |endpoint: &str, token_file: &str| {
        format!("intelligence:\n  endpoints: {endpoint}\n  model: mock\n{token_file}")
    };
    let original = config("Work.", &[], &[], &intel("http://127.0.0.1:9", ""));
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| !events(l, "proc.ready").is_empty(), "the daemon", 15);

    let missing = t.path().join("no-such-token");
    std::fs::write(
        &cfg,
        config(
            "Work.",
            &[],
            &[],
            &intel(
                "http://127.0.0.1:10",
                &format!("  token_file: {}\n", missing.display()),
            ),
        ),
    )
    .unwrap();
    let log = d.reload(0);
    assert_eq!(outcomes(&log)[0]["event"], "config.reload.invalid", "{log}");
    assert!(
        events(&log, "config.reload.invalid")
            .iter()
            .any(|e| e["error"]
                .as_str()
                .is_some_and(|m| m.contains("intelligence token"))),
        "{log}"
    );
    std::fs::write(&cfg, &original).unwrap();
    let log = d.reload(1);
    assert_eq!(changed(&outcomes(&log)[1]), ["nothing"], "{log}");
}

/// A step naming a tool only the server the reload adds provides is checked
/// against the registry the reload builds — not the running one, which lacks
/// it — so the reload applies and the step runs on the new server.
#[test]
fn a_step_naming_a_tool_only_the_new_server_has_applies_and_runs_on_it() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let b = common::spawn_mock_mcp("mock://b", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(
        &cfg,
        config("Keep ticking.", &[("a", &a.uri())], &[TICK], ""),
    )
    .unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);

    let uses_b = "  - name: uses-b\n    steps:\n      s: {kind: schedule, every: 300ms}\n      x: {kind: tool, depends_on: [s], name: b.mock.ops}\n      f: {kind: finish, depends_on: [x], status: completed, output: uses-b}\n";
    std::fs::write(
        &cfg,
        config(
            "Keep ticking.",
            &[("a", &a.uri()), ("b", &b.uri())],
            &[TICK, uses_b],
            "",
        ),
    )
    .unwrap();
    let log = d.reload(0);
    assert_eq!(outcomes(&log)[0]["event"], "config.reloaded", "{log}");
    d.wait_for(|l| done_with(l, "uses-b") >= 1, "the step running on b", 15);
    assert!(calls(&b, "mock.ops") >= 1, "{}", b.log());
}

/// A reload that disables a tool a workflow step calls is refused — with no
/// `mcp` change at all, so only the tools section moves what the workflows
/// are checked against — and nothing of it applies: the tool stays enabled.
#[test]
fn disabling_a_tool_a_step_calls_refuses_the_reload_and_nothing_applies() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let calls_a = "  - name: calls-a\n    steps:\n      s: {kind: manual}\n      x: {kind: tool, depends_on: [s], name: a.mock.ops}\n      f: {kind: finish, depends_on: [x], status: completed}\n";
    let original = config("Keep ticking.", &[("a", &a.uri())], &[TICK, calls_a], "");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);

    std::fs::write(
        &cfg,
        config(
            "Keep ticking.",
            &[("a", &a.uri())],
            &[TICK, calls_a],
            "tools:\n  disabled: [a.mock.ops]\n",
        ),
    )
    .unwrap();
    let log = d.reload(0);
    assert_eq!(
        outcomes(&log)[0]["event"],
        "config.reload.invalid",
        "the reload was not refused:\n{log}"
    );
    assert!(
        events(&log, "config.reload.invalid")
            .iter()
            .any(|e| e["error"]
                .as_str()
                .is_some_and(|m| m.contains("tool \"a.mock.ops\" is unknown, disabled"))),
        "{log}"
    );
    std::fs::write(&cfg, &original).unwrap();
    let log = d.reload(1);
    assert_eq!(changed(&outcomes(&log)[1]), ["nothing"], "{log}");
}

/// A reload that changes only the workflows — adding one that registers a
/// tool — rebuilds the registry, so the new workflow tool is there. Kept, the
/// registry would carry the old set's tools and the new one would be missing.
#[test]
fn a_reload_adding_a_tool_workflow_alone_registers_its_tool() {
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let probe = "  - name: probe\n    tool: {name: probe.run, mode: async}\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], status: completed}\n";
    let probe2 = "  - name: probe2\n    tool: {name: probe2.run, mode: async}\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], status: completed}\n";
    std::fs::write(&cfg, config("Work.", &[], &[probe], "")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| !events(l, "proc.ready").is_empty(), "the daemon", 15);

    std::fs::write(&cfg, config("Work.", &[], &[probe, probe2], "")).unwrap();
    let log = d.reload(0);
    let outcome = &outcomes(&log)[0];
    assert_eq!(outcome["event"], "config.reloaded", "{log}");
    assert!(changed(outcome).iter().any(|c| c == "tools"), "{log}");
    assert!(
        events(&log, "registry.workflow_tools")
            .iter()
            .any(|e| e["tool"] == "probe2.run"),
        "the new workflow's tool was not registered:\n{log}"
    );
}

/// Skills are discovered through the servers the reload would run on, so a
/// skills source on a server the same reload adds is found.
#[test]
fn skills_from_a_server_the_reload_adds_are_discovered() {
    let b = common::spawn_mock_mcp("mock://b", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, config("Work.", &[], &[], "")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| !events(l, "proc.ready").is_empty(), "the daemon", 15);

    std::fs::write(
        &cfg,
        config(
            "Work.",
            &[("b", &b.uri())],
            &[],
            "skills:\n  sources:\n    - {server: b, discover: resources}\n",
        ),
    )
    .unwrap();
    let log = d.reload(0);
    let outcome = &outcomes(&log)[0];
    assert_eq!(outcome["event"], "config.reloaded", "{log}");
    assert!(
        changed(outcome).iter().any(|c| c == "skills"),
        "b's skills were not discovered: {:?}\n{log}",
        changed(outcome)
    );
}

/// A server is unchanged only when everything its dial reads is: a changed
/// credential or timeout re-dials it, where comparing the endpoint, headers
/// and AAuth flag alone kept the old connection — the old credential — while
/// the reload reported `mcp`. A change to its tags alone, which only the
/// registry reads, keeps the live connection.
#[test]
fn a_changed_credential_or_timeout_redials_the_server_and_its_tags_do_not() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let server = |extra: &str| {
        let mcp = format!(
            "mcp:\n  servers:\n    - {{name: a, endpoint: \"{}\", ns: a{extra}}}\n",
            a.uri()
        );
        config("Keep ticking.", &[], &[TICK], &mcp)
    };
    let redials = |log: &str| {
        events(log, "mcp.connect")
            .iter()
            .filter(|e| e["server"] == "a" && e["reason"] == "reload")
            .count()
    };
    std::fs::write(
        &cfg,
        server(", timeout: 30s, auth: {kind: static, token: \"{{secret:TX_TOKEN_ONE}}\"}"),
    )
    .unwrap();
    let d = Daemon::spawn_env(
        &cfg,
        &[("TX_TOKEN_ONE", "token-one"), ("TX_TOKEN_TWO", "token-two")],
    );
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);

    // The credential is rotated.
    std::fs::write(
        &cfg,
        server(", timeout: 30s, auth: {kind: static, token: \"{{secret:TX_TOKEN_TWO}}\"}"),
    )
    .unwrap();
    let log = d.reload(0);
    assert_eq!(outcomes(&log)[0]["event"], "config.reloaded", "{log}");
    assert_eq!(
        redials(&log),
        1,
        "a rotated credential kept the old connection:\n{log}"
    );

    // The timeout changes.
    std::fs::write(
        &cfg,
        server(", timeout: 31s, auth: {kind: static, token: \"{{secret:TX_TOKEN_TWO}}\"}"),
    )
    .unwrap();
    let log = d.reload(1);
    assert_eq!(outcomes(&log)[1]["event"], "config.reloaded", "{log}");
    assert_eq!(
        redials(&log),
        2,
        "a changed timeout kept the old connection:\n{log}"
    );

    // Only the tags change.
    std::fs::write(
        &cfg,
        server(", timeout: 31s, auth: {kind: static, token: \"{{secret:TX_TOKEN_TWO}}\"}, tags: {\"*\": [sensitive]}"),
    )
    .unwrap();
    let log = d.reload(2);
    assert_eq!(outcomes(&log)[2]["event"], "config.reloaded", "{log}");
    assert_eq!(
        redials(&log),
        2,
        "a tags-only change re-dialed the server:\n{log}"
    );
    let ticks = done_with(&log, "tick");
    d.wait_for(|l| done_with(l, "tick") >= ticks + 2, "a still ticking", 15);
}

/// A subscription lives on its connection. A reload that re-dials a server
/// subscribes again, on the new connection, what the old one carried: the
/// `subscribe` start of a workflow the reload left unchanged, and the
/// resource a suspended `wait` waits on. Counted where it lands — at the
/// server.
#[test]
fn a_redialed_server_is_subscribed_again_for_unchanged_starts_and_waits() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let watch = "  - name: watch\n    steps:\n      s: {kind: subscribe, server: a, uri: \"mock://a\"}\n      f: {kind: finish, depends_on: [s], status: completed}\n";
    // The wait watches a resource of its own, so each is counted apart: a
    // connection tracks a URI once, whoever subscribed it.
    let waits = "  - name: waits\n    steps:\n      s: {kind: once}\n      w: {kind: wait, depends_on: [s], on: resource, server: a, uri: \"mock://waited\", timeout: 120s}\n      f: {kind: finish, depends_on: [w], status: completed}\n";
    let server = |headers: &str| {
        let mcp = format!(
            "mcp:\n  servers:\n    - {{name: a, endpoint: \"{}\", ns: a{headers}}}\n",
            a.uri()
        );
        config("Work.", &[], &[watch, waits], &mcp)
    };
    std::fs::write(&cfg, server("")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |_| a.subscribes("mock://a") >= 1 && a.subscribes("mock://waited") >= 1,
        "the start and the wait subscribing",
        15,
    );
    // (The per-URI protocol re-sends every URI a connection tracks with each
    // new one, so the counts only ever say "more than before".)
    let (start0, wait0) = (a.subscribes("mock://a"), a.subscribes("mock://waited"));

    std::fs::write(&cfg, server(", headers: {X-Generation: \"2\"}")).unwrap();
    let log = d.reload(0);
    assert_eq!(outcomes(&log)[0]["event"], "config.reloaded", "{log}");
    assert!(
        events(&log, "mcp.connect")
            .iter()
            .any(|e| e["server"] == "a" && e["reason"] == "reload"),
        "{log}"
    );
    let log = d.wait_for(
        |_| a.subscribes("mock://a") > start0 && a.subscribes("mock://waited") > wait0,
        "both subscribed again on the new connection",
        15,
    );
    assert!(
        events(&log, "wait.resubscribed")
            .iter()
            .any(|e| e["server"] == "a" && e["uri"] == "mock://waited"),
        "{log}"
    );
}

/// A reload that removes a server takes the subscriptions on it with it. A
/// suspended wait on that server can never be woken, and fails loudly — as it
/// does at boot — rather than parking with nothing in any log.
#[test]
fn a_wait_on_a_server_the_reload_removes_fails() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let b = common::spawn_mock_mcp("mock://b", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let waits = "  - name: waits\n    steps:\n      s: {kind: once}\n      w: {kind: wait, depends_on: [s], on: resource, server: b, uri: \"mock://waited\", timeout: 120s}\n      f: {kind: finish, depends_on: [w], status: completed}\n";
    std::fs::write(
        &cfg,
        config(
            "Work.",
            &[("a", &a.uri()), ("b", &b.uri())],
            &[TICK, waits],
            "",
        ),
    )
    .unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |_| b.subscribes("mock://waited") >= 1,
        "the wait subscribing on b",
        15,
    );

    // b goes, and the workflow that named it; the run keeps its definition.
    std::fs::write(&cfg, config("Work.", &[("a", &a.uri())], &[TICK], "")).unwrap();
    let log = d.reload(0);
    assert_eq!(outcomes(&log)[0]["event"], "config.reloaded", "{log}");
    let log = d.wait_for(
        |l| {
            events(l, "run.done")
                .iter()
                .any(|e| e["workflow"] == "waits" && e["status"] == "failed")
        },
        "the wait on the removed server to fail",
        15,
    );
    assert!(
        events(&log, "wait.resubscribe.fail")
            .iter()
            .any(|e| e["level"] == "error"
                && e["server"] == "b"
                && e["reason"] == "reload"
                && e["err"]
                    .as_str()
                    .is_some_and(|m| m.contains("no longer configured"))),
        "{log}"
    );
}

/// The resource instruction's server, with the instruction itself unchanged:
/// a reload that removes it is refused, as a start with that configuration
/// is; one that re-dials it reads the instruction again through the new
/// connection and subscribes it there, so the publisher's updates keep
/// arriving.
#[test]
fn the_instructions_server_cannot_be_removed_and_a_redial_resubscribes_it() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let b = common::spawn_mock_mcp("mock://b", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let instruction = "mcp://b/mock://instruction";
    let with_b = |headers: &str| {
        let mcp = format!(
            "mcp:\n  servers:\n    - {{name: a, endpoint: \"{}\", ns: a}}\n    - {{name: b, endpoint: \"{}\", ns: b{headers}}}\n",
            a.uri(),
            b.uri()
        );
        config(instruction, &[], &[TICK], &mcp)
    };
    let original = with_b("");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick") >= 1, "a first tick", 15);
    assert_eq!(b.reads("mock://instruction"), 1, "{}", b.log());
    assert_eq!(b.subscribes("mock://instruction"), 1, "{}", b.log());

    // b goes; the instruction does not change.
    std::fs::write(&cfg, config(instruction, &[("a", &a.uri())], &[TICK], "")).unwrap();
    let log = d.reload(0);
    assert_eq!(
        outcomes(&log)[0]["event"],
        "config.reload.invalid",
        "a reload removing the instruction's server applied:\n{log}"
    );
    assert!(
        events(&log, "config.reload.invalid")
            .iter()
            .any(|e| e["error"]
                .as_str()
                .is_some_and(|m| m.contains("agent.instruction mcp://b/mock://instruction"))),
        "{log}"
    );

    // b is re-dialed with a new header.
    std::fs::write(&cfg, with_b(", headers: {X-Generation: \"2\"}")).unwrap();
    let log = d.reload(1);
    let outcome = &outcomes(&log)[1];
    assert_eq!(outcome["event"], "config.reloaded", "{log}");
    assert!(
        !changed(outcome).iter().any(|c| c == "agent.instruction"),
        "the same text was reported as a changed instruction:\n{log}"
    );
    assert_eq!(b.reads("mock://instruction"), 2, "{}", b.log());
    assert_eq!(
        b.subscribes("mock://instruction"),
        2,
        "the instruction was not subscribed on the new connection:\n{}",
        b.log()
    );
}

/// A reload that replaces a workflow for an edit that leaves its schedule as
/// it was keeps the schedule's deadline. Re-armed from now, every edit
/// pushed the next run back a whole period: this `every: 4s` workflow,
/// edited each second, would not run until four seconds after the last edit.
#[test]
fn an_edit_that_leaves_a_schedule_alone_keeps_its_deadline() {
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let sched = |n: usize| {
        format!(
            "  - name: sched\n    steps:\n      s: {{kind: schedule, every: 4s}}\n      f: {{kind: finish, depends_on: [s], status: completed, output: sched-{n}}}\n"
        )
    };
    std::fs::write(&cfg, config("Work.", &[], &[&sched(0)], "")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |l| {
            events(l, "start.schedule.armed")
                .iter()
                .any(|e| e["workflow"] == "sched")
        },
        "the schedule armed",
        15,
    );
    let armed = Instant::now();
    for n in 1..=3 {
        std::thread::sleep(Duration::from_millis(900));
        std::fs::write(&cfg, config("Work.", &[], &[&sched(n)], "")).unwrap();
        let log = d.reload(n - 1);
        assert_eq!(outcomes(&log)[n - 1]["event"], "config.reloaded", "{log}");
    }
    d.wait_for(
        |l| {
            events(l, "run.done")
                .iter()
                .any(|e| e["workflow"] == "sched")
        },
        "the first scheduled run",
        15,
    );
    let took = armed.elapsed();
    assert!(
        took < Duration::from_millis(6_000),
        "the edits postponed the schedule: first run {took:?} after it was armed\n{}",
        d.stderr()
    );
}

/// A document that resolves to something other than a mapping — here a
/// `file:` holding a bare string — refuses the reload naming its source, and
/// the daemon lives on: on a reload, a panic over it took the daemon down.
#[test]
fn a_file_that_is_not_a_definition_refuses_the_reload_and_the_daemon_lives() {
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let scalar = t.path().join("scalar.yaml");
    std::fs::write(&scalar, "just a sentence\n").unwrap();
    let original = config("Work.", &[], &[TICK_LOCAL], "");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| done_with(l, "tick-local") >= 1, "a first tick", 15);

    let entry = format!("  - name: scalar\n    file: {}\n", scalar.display());
    std::fs::write(&cfg, config("Work.", &[], &[TICK_LOCAL, &entry], "")).unwrap();
    let log = d.reload(0);
    assert_eq!(outcomes(&log)[0]["event"], "config.reload.invalid", "{log}");
    assert!(
        events(&log, "config.reload.invalid")
            .iter()
            .any(|e| e["error"]
                .as_str()
                .is_some_and(|m| m.contains(&format!("workflow file {}", scalar.display())))),
        "{log}"
    );
    let ticks = done_with(&log, "tick-local");
    d.wait_for(
        |l| done_with(l, "tick-local") >= ticks + 2,
        "the daemon ticking on",
        15,
    );
}

/// A scheduled workflow that needs no server.
const TICK_LOCAL: &str = "  - name: tick-local\n    steps:\n      s: {kind: schedule, every: 300ms}\n      f: {kind: finish, depends_on: [s], status: completed, output: tick-local}\n";

/// A definition the agent stored at runtime cannot veto the operator: a
/// reload that disables the tool it names applies — the tool IS disabled —
/// and the stored definition is left out, said out loud, and kept; it loads
/// again once the tool is back. And the agent cannot store one naming a tool
/// this agent does not have in the first place.
#[test]
fn a_stored_definition_cannot_veto_a_reload_that_disables_its_tool() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let play = t.path().join("play.json");
    let def = |name: &str, tool: &str| {
        serde_json::json!({"name": name, "steps": {
            "s": {"kind": "manual"},
            "x": {"kind": "tool", "depends_on": ["s"], "name": tool},
            "f": {"kind": "finish", "depends_on": ["x"], "status": "completed"}}})
    };
    let create = |d: serde_json::Value| serde_json::json!({"tool_calls": [{"name": "workflow.create", "arguments": {"definition": d}}]});
    std::fs::write(
        &play,
        serde_json::json!({"turns": [
            create(def("mine", "a.mock.ops")),
            create(def("bogus", "nosuch.tool")),
            {"echo_tool_result": true}
        ]})
        .to_string(),
    )
    .unwrap();
    let with = |extra: &str| {
        format!(
            "agent:\n  name: tx\n  prompt: go\n  instruction: Keep ticking.\n\
             intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
             store:\n  kind: memory\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n  log_content: true\n\
             mcp:\n  servers:\n    - {{name: a, endpoint: \"{}\", ns: a}}\n\
             workflows:\n{TICK}{extra}",
            play.display(),
            a.uri()
        )
    };
    let original = with("");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    let log = d.wait_for(
        |l| !events(l, "turn.reply").is_empty(),
        "the agent's turn",
        20,
    );
    assert!(
        events(&log, "workflow.defined")
            .iter()
            .any(|e| e["name"] == "mine"),
        "{log}"
    );
    assert!(
        events(&log, "workflow.defined")
            .iter()
            .all(|e| e["name"] != "bogus"),
        "a definition naming a tool this agent lacks was stored:\n{log}"
    );
    assert!(
        events(&log, "turn.reply").iter().any(|e| e["text"]
            .as_str()
            .is_some_and(|m| m.contains("tool \"nosuch.tool\" is unknown"))),
        "the refusal did not say why:\n{log}"
    );

    std::fs::write(&cfg, with("tools:\n  disabled: [a.mock.ops]\n")).unwrap();
    let log = d.reload(0);
    let outcome = &outcomes(&log)[0];
    assert_eq!(
        outcome["event"], "config.reloaded",
        "a stored definition vetoed the operator's reload:\n{log}"
    );
    assert!(changed(outcome).iter().any(|c| c == "tools"), "{log}");
    assert!(
        events(&log, "workflow.stored.invalid")
            .iter()
            .any(|e| e["name"] == "mine"),
        "{log}"
    );
    assert!(
        events(&log, "workflow.unloaded")
            .iter()
            .any(|e| e["workflow"] == "mine" && e["reason"] == "removed"),
        "{log}"
    );

    std::fs::write(&cfg, &original).unwrap();
    let log = d.reload(1);
    assert_eq!(outcomes(&log)[1]["event"], "config.reloaded", "{log}");
    assert!(
        events(&log, "workflow.loaded")
            .iter()
            .filter(|e| e["name"] == "mine" && e["source"] == "store")
            .count()
            == 1,
        "the stored definition did not load again:\n{log}"
    );
}

/// A `tool` step naming a workflow's tool is refused on every path. A start
/// refuses it (the workflow tools are registered after the check), and so
/// does a reload that rebuilds the registry; a reload that kept the registry
/// — which already held the workflow tools — accepted it, applying a
/// configuration the next start refuses.
#[test]
fn a_step_naming_a_workflow_tool_is_refused_by_a_reload_that_keeps_the_registry() {
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let probe = "  - name: probe\n    tool: {name: probe.run, mode: async}\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], status: completed}\n";
    let original = config("Work.", &[], &[probe], "");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|l| !events(l, "proc.ready").is_empty(), "the daemon", 15);

    // No `tool:` block moves, so the registry is kept.
    let calls_probe = "  - name: calls-probe\n    steps:\n      s: {kind: manual}\n      x: {kind: tool, depends_on: [s], name: probe.run}\n      f: {kind: finish, depends_on: [x], status: completed}\n";
    std::fs::write(&cfg, config("Work.", &[], &[probe, calls_probe], "")).unwrap();
    let log = d.reload(0);
    assert_eq!(
        outcomes(&log)[0]["event"],
        "config.reload.invalid",
        "the reload accepted what a start refuses:\n{log}"
    );
    assert!(
        events(&log, "config.reload.invalid")
            .iter()
            .any(|e| e["error"]
                .as_str()
                .is_some_and(|m| m.contains("tool \"probe.run\" is a workflow's tool"))),
        "{log}"
    );
    std::fs::write(&cfg, &original).unwrap();
    let log = d.reload(1);
    assert_eq!(changed(&outcomes(&log)[1]), ["nothing"], "{log}");
}
