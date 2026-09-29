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
        let err_path = common::unique_path("reload-tx-daemon", "log");
        let errf = std::fs::File::create(&err_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg.to_string_lossy()])
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
