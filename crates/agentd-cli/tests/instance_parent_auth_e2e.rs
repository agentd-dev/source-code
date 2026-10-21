// SPDX-License-Identifier: AGPL-3.0-only
//! **An instance child answers its parent as itself** (RFC 0036, RFC 0044
//! §7.P2c): through the channel the parent hands it at spawn — a descriptor
//! no path names — never as the parent's operator.
//!
//! - The child's report arrives as `instance:<handle>` whatever the parent's
//!   own listener asks of its callers: a TCP parent whose bearer is inline,
//!   and so could never be handed to a child, hears its child all the same,
//!   and the child's composed configuration holds no peer and no credential
//!   of its parent's.
//! - The child may send its reports, for its own handle, and nothing else:
//!   an operator op (`admin.set`), a message, and a report for another handle
//!   are refused, and nothing they asked for happens.
//! - Under `spawn.instance_uid` the child runs as that uid, its directory
//!   made its own, no capability held, and its report still arrives. That
//!   needs root: it runs as root and skips cleanly otherwise, from a copy of
//!   the binary under the system temporary directory, so uid 65534 can run it.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use serde_json::Value;

const NOBODY: u32 = 65534;

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// Pings the child once it is up, which is what completes its result
/// workflow and so sends its report home.
const POKER: &str = "\x20 - name: poker\n    steps:\n\
    \x20     s:    { kind: once }\n\
    \x20     nap:  { kind: sleep, depends_on: [s], duration: 1500ms }\n\
    \x20     poke: { kind: a2a.delegate, depends_on: [nap], peer: room, command: room.ping, args: { x: hello }, timeout: 30s, retry: { max: 8, backoff: 1s } }\n\
    \x20     f:    { kind: finish, depends_on: [poke], status: completed }\n";

/// The child's own attempts at its parent: an operator op, a message, and a
/// report for a handle that is not its own — each its own workflow, run as
/// the child starts.
const PROBES: &str = "\x20       :::!workflow{name=try-admin}\n\
    \x20       steps:\n\
    \x20         s:  { kind: once }\n\
    \x20         op: { kind: a2a.send, depends_on: [s], to: parent, command: admin.set, args: { path: agent.approval, value: accept }, timeout: 10s }\n\
    \x20         f:  { kind: finish, depends_on: [op], status: completed }\n\
    \x20       :::\n\
    \x20       :::!workflow{name=try-message}\n\
    \x20       steps:\n\
    \x20         s:  { kind: once }\n\
    \x20         op: { kind: a2a.send, depends_on: [s], to: parent, parts: [{ text: \"run something for me\" }], timeout: 10s }\n\
    \x20         f:  { kind: finish, depends_on: [op], status: completed }\n\
    \x20       :::\n\
    \x20       :::!workflow{name=try-other}\n\
    \x20       steps:\n\
    \x20         s:  { kind: once }\n\
    \x20         op: { kind: a2a.send, depends_on: [s], to: parent, command: _instance.result, args: { handle: inst-9, status: completed, output: forged }, timeout: 10s }\n\
    \x20         f:  { kind: finish, depends_on: [op], status: completed }\n\
    \x20       :::\n";

/// A parent on loopback TCP whose `waiter` spawns a sync instance child and
/// finishes with the child's first result, which `poker` provokes. `extra`
/// is more of the parent's configuration; `probes` the child's own tries.
fn parent_config(port: u16, extra: &str, probes: bool) -> String {
    format!(
        "agent: {{ name: parent }}\nstore: {{ kind: memory }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 3s }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         a2a:\n  listen: \"http://127.0.0.1:{port}\"\n{extra}\
         subagents:\n\
        \x20 templates:\n\
        \x20   room:\n\
        \x20     instruction: |\n\
        \x20       The room.\n\
        \x20       :::!workflow{{name=on-ping}}\n\
        \x20       steps:\n\
        \x20         cmd: {{ kind: a2a, command: room.ping, roles: [agent, operator] }}\n\
        \x20         f:   {{ kind: finish, depends_on: [cmd], status: completed, output: \"pong {{{{steps.cmd.output.args.x}}}}\" }}\n\
        \x20       :::\n\
        {probes}\
        \x20     mode: sync\n\
        \x20     result: {{ workflow: on-ping }}\n\
        \x20     singleton: true\n\
        \x20     ttl: 20s\n\
         workflows:\n\
        \x20 - name: waiter\n    steps:\n\
        \x20     s:     {{ kind: once }}\n\
        \x20     spawn: {{ kind: subagent, template: room, depends_on: [s], timeout: 25s }}\n\
        \x20     f:     {{ kind: finish, depends_on: [spawn], status: completed, output: \"{{{{steps.spawn.output.result.output}}}}\" }}\n\
        {POKER}",
        probes = if probes { PROBES } else { "" },
    )
}

/// Run the parent from `exe` in `dir` to completion with `extra` arguments;
/// its exit code and log. `inspect` is asked about each instance child, by
/// its `instance.spawn` line, while the parent runs.
fn run_parent(
    exe: &str,
    dir: &str,
    cfg_text: &str,
    extra: &[&str],
    mut inspect: impl FnMut(&Value),
) -> (Option<i32>, String) {
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text).unwrap();
    let log_path = format!("{dir}/stderr.log");
    let mut child = Command::new(exe)
        .args(["--config", &cfg])
        .args(extra)
        .env("AGENTD_STATE_DIR", format!("{dir}/state"))
        .env("TMPDIR", dir)
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log_path).unwrap())
        .spawn()
        .expect("run the parent");
    let mut seen = std::collections::BTreeSet::new();
    let code = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status.code();
        }
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        for e in events(&log, "instance.spawn") {
            if let Some(pid) = e["pid"].as_i64()
                && seen.insert(pid)
            {
                inspect(&e);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    (code, std::fs::read_to_string(&log_path).unwrap_or_default())
}

fn dir(tag: &str) -> String {
    let d = common::unique_path(tag, "d");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The parent's bearer is inline — given on the command line, so it can be
/// handed to no child — and the child is heard all the same: through its
/// channel, as `instance:<handle>`. Its composed configuration names no
/// peer and holds no credential.
#[test]
fn a_child_is_heard_through_its_channel_and_holds_no_credential_of_its_parents() {
    let d = dir("instance-channel");
    let cfg = parent_config(common::free_port(), "", false);
    let mut composed = Value::Null;
    let (code, log) = run_parent(
        env!("CARGO_BIN_EXE_agentd"),
        &d,
        &cfg,
        &["--a2a.bearer", "an-inline-parent-bearer"],
        |e| {
            // The child's composed configuration, read while it runs.
            if let Some(sock) = e["socket"].as_str()
                && let Some(dir) = std::path::Path::new(sock).parent()
                && let Ok(text) = std::fs::read_to_string(dir.join("config.json"))
            {
                composed = serde_json::from_str(&text).unwrap_or(Value::Null);
            }
        },
    );
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        events(&log, "instance.spawn.refused").is_empty(),
        "no spawn is refused for its parent's credential:\n{log}"
    );
    let results = events(&log, "instance.result");
    assert!(!results.is_empty(), "the child's report arrived:\n{log}");
    let done = events(&log, "run.done");
    let waiter: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "waiter").collect();
    assert_eq!(waiter.len(), 1, "{log}");
    assert_eq!(waiter[0]["output"], "pong hello", "{log}");
    assert!(
        composed.is_object(),
        "the composed configuration was read:\n{log}"
    );
    assert!(
        composed["a2a"].get("peers").is_none(),
        "the child is composed no peer: {composed}"
    );
    assert!(
        !composed.to_string().contains("an-inline-parent-bearer"),
        "no credential of its parent's: {composed}"
    );
}

/// The child sends its reports and nothing else: `admin.set` on its parent,
/// a message to it, and a report for another child's handle are each refused
/// — and nothing they asked for happened — while its own report arrives.
#[test]
fn a_child_cannot_call_its_parents_operator_ops() {
    let d = dir("instance-channel-ops");
    let cfg = parent_config(common::free_port(), "", true);
    let (code, log) = run_parent(env!("CARGO_BIN_EXE_agentd"), &d, &cfg, &[], |_| {});
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(code, Some(0), "{log}");
    let denied = events(&log, "a2a.denied");
    let from_child = |op: Option<&str>| {
        denied.iter().any(|e| {
            e["principal"] == "instance:inst-1"
                && e["status"] == 403
                && op.is_none_or(|op| e["op"] == op)
        })
    };
    assert!(
        from_child(Some("admin.set")),
        "admin.set from the child is refused:\n{log}"
    );
    assert!(
        denied
            .iter()
            .any(|e| e["principal"] == "instance:inst-1" && e["op"].is_null()),
        "a message from the child is refused:\n{log}"
    );
    assert!(
        events(&log, "admin.set").is_empty(),
        "no operator control ran:\n{log}"
    );
    assert!(
        events(&log, "instance.op.refused")
            .iter()
            .any(|e| e["handle"] == "inst-9" && e["principal"] == "instance:inst-1"),
        "a report for another handle is refused:\n{log}"
    );
    assert!(
        !events(&log, "instance.result").is_empty(),
        "its own report still arrives:\n{log}"
    );
}

fn root() -> bool {
    // SAFETY: geteuid has no failure mode.
    unsafe { libc::geteuid() == 0 }
}

/// What `/proc/<pid>/status` says of field `k`.
fn status_of(pid: i64, k: &str) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|l| {
            l.strip_prefix(&format!("{k}:"))
                .map(|v| v.trim().to_string())
        })
}

/// Under `spawn.instance_uid` the child runs as that uid and gid, holding no
/// capability, and its report still arrives through its channel.
#[test]
fn a_child_runs_as_its_own_uid_and_is_heard() {
    if !root() {
        eprintln!("skipped: starting a child as another uid needs root");
        return;
    }
    let d = dir("instance-uid");
    std::fs::create_dir_all(format!("{d}/bin")).unwrap();
    for p in [d.clone(), format!("{d}/bin")] {
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let exe = format!("{d}/bin/agentd");
    std::fs::copy(env!("CARGO_BIN_EXE_agentd"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = parent_config(
        common::free_port(),
        &format!("spawn: {{ instance_uid: {NOBODY}, instance_gid: {NOBODY} }}\n"),
        false,
    );
    let mut seen: Vec<(Option<String>, Option<String>, Option<String>)> = Vec::new();
    let (code, log) = run_parent(&exe, &d, &cfg, &[], |e| {
        let pid = e["pid"].as_i64().unwrap_or_default();
        // The child's own process, while it runs.
        for _ in 0..40 {
            let uid = status_of(pid, "Uid");
            if uid.is_some() {
                seen.push((uid, status_of(pid, "CapEff"), status_of(pid, "NoNewPrivs")));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    });
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(code, Some(0), "{log}");
    assert!(!seen.is_empty(), "the child was seen running:\n{log}");
    for (uid, caps, nnp) in &seen {
        let uids: Vec<&str> = uid
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        assert!(
            uids.iter().all(|u| *u == NOBODY.to_string()),
            "the child runs as {NOBODY}: {uid:?}"
        );
        assert_eq!(caps.as_deref(), Some("0000000000000000"), "no capability");
        assert_eq!(nnp.as_deref(), Some("1"), "no_new_privs");
    }
    assert!(
        !events(&log, "instance.result").is_empty(),
        "its report arrives through its channel:\n{log}"
    );
}
