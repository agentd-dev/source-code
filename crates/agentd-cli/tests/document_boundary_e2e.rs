// SPDX-License-Identifier: AGPL-3.0-only
//! **A served document opens no route on the operator's listener without the
//! grant** (RFC 0042, the gap RFC 0045 §8 P0 item 13 found), end to end.
//!
//! `workflows` is the document's to write — that is what `:::!workflow` is
//! for — but a `webhook` start in one opens an inbound route on the
//! OPERATOR's webhook listener, answering whoever can reach it with the auth
//! the route itself names. `:::!endpoint`, which folds into exactly that
//! workflow, needs the `interface` grant; the same route written as a
//! workflow needed nothing. Now it needs the same grant, wherever the document
//! puts it: a `:::!workflow` block, a `:::!config` entry, or a `file:`
//! reference the document names. A subagent template's machinery opens none
//! at all — a child has no listener. Refused by `--validate-config`, at
//! startup, and on a reload, which keeps the running configuration.
#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;

/// A document whose machinery opens a route.
const ROUTE_DOC: &str = "You are the desk.\n\n\
     :::!workflow{name=intake}\n\
     steps:\n  hook: {kind: webhook, path: /in}\n  f: {kind: finish, depends_on: [hook], status: completed}\n\
     :::\n";

/// The same desk with machinery that opens nothing: a schedule keeps the
/// daemon up without a route.
const QUIET_DOC: &str = "You are the desk.\n\n\
     :::!workflow{name=tick}\n\
     steps:\n  s: {kind: schedule, every: 1h}\n  f: {kind: finish, depends_on: [s], status: completed}\n\
     :::\n";

/// An operator config around the instruction document `doc`, with a webhook
/// listener so the route's only possible refusal is the boundary.
fn config(doc: &Path, port: u16, extra: &str) -> String {
    format!(
        "agent:\n  name: boundary\n  instruction: {}\n{extra}\
         store: {{kind: memory}}\n\
         observability:\n  log_level: info\n\
         webhooks:\n  listen: http://127.0.0.1:{port}\n",
        doc.display()
    )
}

/// Run agentd with a scrubbed environment; `(exit code, stderr)`.
fn agentd(args: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    for (k, _) in std::env::vars() {
        if k.starts_with("AGENTD_") || k.starts_with("AGENT_") {
            cmd.env_remove(k);
        }
    }
    let out = cmd.args(args).output().expect("spawn agentd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn validate(cfg: &Path, body: &str) -> (i32, String) {
    std::fs::write(cfg, body).unwrap();
    agentd(&["--config", &cfg.to_string_lossy(), "--validate-config"])
}

/// Whether `err` names the workflow, its step and the document it came from,
/// and says what would open it. `err` may be a JSON-escaped log line.
fn names_the_route(err: &str, doc: &Path) -> bool {
    let err = err.replace("\\\"", "\"");
    err.contains("workflow \"intake\" step \"hook\"")
        && err.contains(&format!("from the instruction document {}", doc.display()))
        && err.contains("`interface` grant")
}

#[test]
fn a_document_webhook_start_is_refused_at_load() {
    let t = tempfile::tempdir().unwrap();
    let doc = t.path().join("desk.md");
    std::fs::write(&doc, ROUTE_DOC).unwrap();
    let cfg = t.path().join("agent.yaml");
    let (code, err) = validate(&cfg, &config(&doc, 9, ""));
    assert_eq!(code, 2, "{err}");
    assert!(names_the_route(&err, &doc), "{err}");

    // The operator's grant opens it — the refusal is the grant's, not a
    // blanket one.
    let (code, err) = validate(
        &cfg,
        &config(&doc, 9, "  document_capabilities: [interface]\n"),
    );
    assert_eq!(code, 0, "{err}");
}

/// The route the OPERATOR writes is theirs: the same workflow in the config,
/// beside a document with machinery of its own, loads with no grant.
#[test]
fn an_operator_configured_webhook_workflow_still_loads() {
    let t = tempfile::tempdir().unwrap();
    let doc = t.path().join("desk.md");
    std::fs::write(&doc, QUIET_DOC).unwrap();
    let cfg = t.path().join("agent.yaml");
    let (code, err) = validate(
        &cfg,
        &format!(
            "{}workflows:\n  - name: intake\n    steps:\n      hook: {{kind: webhook, path: /in}}\n      \
             f: {{kind: finish, depends_on: [hook], status: completed}}\n",
            config(&doc, 9, "")
        ),
    );
    assert_eq!(code, 0, "{err}");
}

/// A subagent template's document: its `:::!config` fragment's workflows
/// reach the child exactly as its `:::!workflow` blocks do, and a child has
/// no listener to serve a route on. Refused naming the template and the
/// workflow, rather than accepted here and refused by the child at every
/// spawn.
#[test]
fn a_template_document_route_is_refused_at_load() {
    let t = tempfile::tempdir().unwrap();
    let doc = t.path().join("desk.md");
    std::fs::write(&doc, QUIET_DOC).unwrap();
    let cfg = t.path().join("agent.yaml");
    let (code, err) = validate(
        &cfg,
        &format!(
            "{}subagents:\n  templates:\n    room:\n      instruction: |\n        Room.\n        \
             :::!config\n        workflows:\n          - name: intake\n            steps:\n              \
             hook: {{kind: webhook, path: /in}}\n              f: {{kind: finish, depends_on: [hook], status: completed}}\n        \
             :::\n",
            config(&doc, 9, "")
        ),
    );
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains(
            "subagents.templates.room: workflow 'intake' (its :::!config) step 'hook' is a `webhook` start: instance children have no webhook listener"
        ),
        "{err}"
    );
}

/// A document's `file:` reference is the document's machinery one hop away:
/// a local file, so `--validate-config` reads it and refuses it as the start
/// does — under the grant, it loads.
#[test]
fn a_document_reference_to_a_route_is_refused_at_startup() {
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join("intake.yaml");
    std::fs::write(
        &file,
        "name: intake\nsteps:\n  hook: {kind: webhook, path: /in}\n  \
         f: {kind: finish, depends_on: [hook], status: completed}\n",
    )
    .unwrap();
    let doc = t.path().join("desk.md");
    std::fs::write(
        &doc,
        format!(
            "You are the desk.\n\n:::!config\nworkflows:\n  - name: intake\n    file: {}\n:::\n",
            file.display()
        ),
    )
    .unwrap();
    let cfg = t.path().join("agent.yaml");
    let port = common::free_port();

    // The JSON line escapes the quotes, a refusal while the start loads its
    // configuration does not; read both as written.
    let names_it = |err: &str| {
        let err = err.replace("\\\"", "\"");
        err.contains(&format!(
            "workflow \"intake\" step \"hook\" from the instruction document {}",
            doc.display()
        )) && err.contains("`interface` grant")
    };
    let (code, err) = validate(&cfg, &config(&doc, port, ""));
    assert_eq!(
        code, 2,
        "a local reference is read by --validate-config: {err}"
    );
    assert!(names_it(&err), "{err}");
    let (code, err) = start(&cfg, |_| false);
    assert_eq!(code, Some(2), "{err}");
    assert!(names_it(&err), "{err}");

    std::fs::write(
        &cfg,
        config(&doc, port, "  document_capabilities: [interface]\n"),
    )
    .unwrap();
    let (code, err) = start(&cfg, |log| {
        log.lines()
            .any(|l| l.contains("\"workflow.loaded\"") && l.contains("\"intake\""))
    });
    assert_eq!(code, None, "under the grant the reference loads:\n{err}");
}

/// Start the daemon on `cfg` until it exits — `(Some(code), log)` — or until
/// `up` sees what it waits for in the log, when it is stopped: `(None, log)`.
fn start(cfg: &Path, up: impl Fn(&str) -> bool) -> (Option<i32>, String) {
    let log = cfg.with_extension("log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg.to_string_lossy()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("spawn agentd");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if let Some(status) = child.try_wait().unwrap() {
            return (
                status.code(),
                std::fs::read_to_string(&log).unwrap_or_default(),
            );
        }
        if up(&text) {
            let _ = child.kill();
            let _ = child.wait();
            return (None, text);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("neither refused nor up in time:\n{text}");
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
}

/// A daemon whose stderr log a test reads, stopped on drop.
#[cfg(feature = "hot-reload")]
struct Daemon {
    child: std::process::Child,
    log: std::path::PathBuf,
}

#[cfg(feature = "hot-reload")]
impl Daemon {
    fn spawn(cfg: &Path) -> Daemon {
        let log = cfg.with_extension("log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg.to_string_lossy()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("spawn daemon");
        Daemon { child, log }
    }
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn events(&self, name: &str) -> Vec<serde_json::Value> {
        self.log()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["event"] == name)
            .collect()
    }
    fn wait_for(&self, pred: impl Fn(&Daemon) -> bool, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !pred(self) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}:\n{}",
                self.log()
            );
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
    fn sighup(&self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) };
    }
}

#[cfg(feature = "hot-reload")]
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while std::time::Instant::now() < deadline && !matches!(self.child.try_wait(), Ok(Some(_)))
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A reload that brings a route into the document is refused, naming it; and
/// restoring the document then reloads to "nothing", so no section of the
/// refused reload had applied.
#[cfg(feature = "hot-reload")]
#[test]
fn a_reload_introducing_a_document_route_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let doc = t.path().join("desk.md");
    std::fs::write(&doc, QUIET_DOC).unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, config(&doc, common::free_port(), "")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |d| {
            d.events("workflow.loaded")
                .iter()
                .any(|e| e["name"] == "tick")
        },
        "the document's workflow to load",
    );

    std::fs::write(&doc, ROUTE_DOC).unwrap();
    d.sighup();
    d.wait_for(
        |d| !d.events("config.reload.invalid").is_empty(),
        "the refusal",
    );
    assert!(
        d.events("config.reload.invalid").iter().any(|e| e["error"]
            .as_str()
            .is_some_and(|m| names_the_route(m, &doc))),
        "the refusal does not name the route and the document:\n{}",
        d.log()
    );
    assert!(d.events("config.reloaded").is_empty(), "{}", d.log());

    std::fs::write(&doc, QUIET_DOC).unwrap();
    d.sighup();
    d.wait_for(
        |d| !d.events("config.reloaded").is_empty(),
        "the restoring reload",
    );
    assert_eq!(
        d.events("config.reloaded")[0]["changed"],
        serde_json::json!(["nothing"]),
        "the refused reload had applied:\n{}",
        d.log()
    );
}

/// The same for a subagent template a reload adds: the route its `:::!config`
/// fragment declares is refused by the reload, which keeps the running
/// configuration.
#[cfg(all(feature = "hot-reload", feature = "a2a"))]
#[test]
fn a_reload_introducing_a_template_route_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let doc = t.path().join("desk.md");
    std::fs::write(&doc, QUIET_DOC).unwrap();
    let cfg = t.path().join("agent.yaml");
    let base = config(&doc, common::free_port(), "");
    std::fs::write(&cfg, &base).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |d| {
            d.events("workflow.loaded")
                .iter()
                .any(|e| e["name"] == "tick")
        },
        "the document's workflow to load",
    );

    std::fs::write(
        &cfg,
        format!(
            "{base}subagents:\n  templates:\n    room:\n      instruction: |\n        Room.\n        \
             :::!config\n        workflows:\n          - name: intake\n            steps:\n              \
             hook: {{kind: webhook, path: /in}}\n              f: {{kind: finish, depends_on: [hook], status: completed}}\n        \
             :::\n"
        ),
    )
    .unwrap();
    d.sighup();
    d.wait_for(
        |d| !d.events("config.reload.invalid").is_empty(),
        "the refusal",
    );
    assert!(
        d.events("config.reload.invalid").iter().any(|e| e["error"]
            .as_str()
            .is_some_and(|m| m.contains(
                "subagents.templates.room: workflow 'intake' (its :::!config) step 'hook' is a `webhook` start"
            ))),
        "{}",
        d.log()
    );
    assert!(d.events("config.reloaded").is_empty(), "{}", d.log());
}
