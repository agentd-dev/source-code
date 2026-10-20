// SPDX-License-Identifier: AGPL-3.0-only
//! Workers under a uid of their own (RFC 0044 §7.P2b): with `spawn.uid`
//! and `spawn.gid` a real turn worker — and an `exec` command — runs as
//! that uid and gid, with no supplementary group, and still in the Landlock
//! domain that denies the audit chain. The worker's own process says who it
//! is (`AGENTD_TEST_WORKER_ID`) and what it could open
//! (`AGENTD_TEST_WORKER_OPEN`), the test hooks of a debug build.
//!
//! Dropping to another uid needs root: these run as root and skip cleanly
//! otherwise, where `--validate-config` refuses the same configuration
//! naming why. The daemon runs from a copy of the binary under the system
//! temporary directory, so the worker uid (65534, `nobody`) can run it.
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use serde_json::Value;

const NOBODY: u32 = 65534;

fn root() -> bool {
    // SAFETY: geteuid has no failure mode.
    unsafe { libc::geteuid() == 0 }
}

/// A directory under the system temporary directory every uid may enter,
/// holding a copy of the binary every uid may run.
fn sandbox(tag: &str) -> (String, String) {
    let dir = common::unique_path(tag, "d");
    std::fs::create_dir_all(format!("{dir}/bin")).unwrap();
    for d in [dir.clone(), format!("{dir}/bin")] {
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let exe = format!("{dir}/bin/agentd");
    std::fs::copy(env!("CARGO_BIN_EXE_agentd"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, exe)
}

fn events<'a>(log: &'a str, name: &'a str) -> impl Iterator<Item = Value> + 'a {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(move |e| e["event"] == name)
}

#[test]
fn a_turn_worker_runs_as_the_configured_uid_inside_its_landlock_domain() {
    if !root() {
        eprintln!("skipped: starting a worker as another uid needs root");
        return;
    }
    let (dir, exe) = sandbox("worker-uid");
    let public = format!("{dir}/public.txt");
    std::fs::write(&public, "public").unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o644)).unwrap();
    let chain = format!("{dir}/audit/audit.log");
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{ name: uid, preflight: never }}\n\
             intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
             store: {{ kind: file, file: {{ path: \"{dir}/state\" }} }}\n\
             spawn: {{ uid: {NOBODY}, gid: {NOBODY} }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 500ms }}\n\
             observability: {{ log_level: info, audit: {{ chain: {{ path: \"{chain}\" }} }} }}\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     t: {{ kind: agent, depends_on: [s], instruction: \"say done\" }}\n\
             \x20     f: {{ kind: finish, depends_on: [t], status: completed }}\n"
        ),
    )
    .unwrap();
    let out = Command::new(&exe)
        .args(["--config", &cfg])
        .env("AGENTD_TEST_WORKER_ID", "1")
        .env("AGENTD_TEST_WORKER_OPEN", format!("{chain}:{public}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert!(
        events(&log, "proc.worker").any(|e| e["uid"] == NOBODY),
        "{log}"
    );
    let ids: Vec<Value> = events(&log, "test.worker_id").collect();
    assert!(!ids.is_empty(), "no worker said who it is:\n{log}");
    for id in &ids {
        for k in ["uid", "euid", "gid", "egid"] {
            assert_eq!(id[k], NOBODY, "the worker's {k}: {id}");
        }
        let groups = id["groups"].as_array().cloned().unwrap_or_default();
        assert!(
            groups.iter().all(|g| *g == NOBODY),
            "no supplementary group of agentd's: {id}"
        );
    }
    let opened: Vec<(String, bool)> = events(&log, "test.worker_open")
        .map(|e| {
            (
                e["path"].as_str().unwrap_or_default().to_string(),
                e["opened"] == true,
            )
        })
        .collect();
    assert!(
        opened.iter().any(|(p, ok)| *p == public && *ok),
        "the worker reads what its uid may: {opened:?}\n{log}"
    );
    if agentd::sec::landlock::abi().is_some() {
        assert!(
            opened.iter().any(|(p, ok)| *p == chain && !ok),
            "Landlock still denies the chain: {opened:?}"
        );
    }
    let done: Vec<Value> = events(&log, "run.done").collect();
    assert_eq!(
        done.first().map(|d| d["status"].clone()),
        Some("completed".into()),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An `exec` command runs as the worker uid too.
#[cfg(feature = "exec")]
#[test]
fn an_exec_command_runs_as_the_configured_uid() {
    if !root() {
        eprintln!("skipped: starting a command as another uid needs root");
        return;
    }
    let (dir, exe) = sandbox("worker-uid-exec");
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{ name: uid, instruction: x, preflight: never }}\n\
             intelligence: {{ endpoints: \"mock:final\", model: m }}\n\
             store: {{ kind: memory }}\n\
             spawn: {{ uid: {NOBODY}, gid: {NOBODY} }}\n\
             security:\n  exec:\n    enabled: true\n    allow: [id]\n    workdir: {dir}\n    timeout: 10s\n\
             workflows:\n  - name: run\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     run: {{ kind: tool, depends_on: [s], name: exec, args: {{ cmd: id, args: [\"-u\"] }} }}\n\
             \x20     done: {{ kind: finish, depends_on: [run], output: \"{{{{steps.run.output}}}}\" }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 500ms }}\n\
             observability: {{ log_level: info, log_content: true }}\n"
        ),
    )
    .unwrap();
    let out = Command::new(&exe)
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let done: Vec<Value> = events(&log, "run.done").collect();
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["output"]["stdout"], format!("{NOBODY}\n"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Where agentd cannot start a worker as another uid — any uid but root,
/// holding neither capability — the configuration is refused at load,
/// naming why.
#[test]
fn a_worker_uid_agentd_cannot_drop_to_is_refused_at_load() {
    if root() {
        eprintln!("skipped: root can drop to any uid");
        return;
    }
    let dir = common::unique_path("worker-uid-refused", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{ name: uid }}\nintelligence: {{ endpoints: \"mock:final\", model: m }}\n\
             spawn: {{ uid: {NOBODY}, gid: {NOBODY} }}\n"
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg, "--validate-config"])
        .output()
        .expect("run");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("without CAP_SETUID and CAP_SETGID"),
        "the refusal names why: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
