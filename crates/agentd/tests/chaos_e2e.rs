// SPDX-License-Identifier: Apache-2.0
//! Chaos / reliability E2E (M7): prove a supervision guarantee by observing a
//! real process tree under failure — **the tree leaks no process**. RFC 0003.
//!
//! Scenario: kill the *supervisor* out from under a live, supervised subagent.
//! `PR_SET_PDEATHSIG(SIGKILL)` must collapse the subagent with it — no orphaned
//! agent keeps running (and burning intelligence) after its supervisor is gone.
//! (The bounded kill ladder, liveness 2×2 classifier, and cap refusals are
//! covered by the `supervisor::{kill,liveness}` + `orchestrator_spawn` unit/
//! integration tests; the SIGTERM drain ladder by `daemon_modes`.)
#![cfg(target_os = "linux")]

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_agentd")
}

fn signal(pid: u32, sig: i32) {
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

/// Whether `pid` is a *running* process (exists and is not a reaped zombie).
/// A zombie means the process died (PDEATHSIG worked) and is merely awaiting
/// reap — it is not an orphan still doing work.
fn running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false; // gone (reaped)
    };
    // "pid (comm) state ppid …" — comm may contain ')', so read after the last one.
    let Some((_, after)) = stat.rsplit_once(')') else {
        return false;
    };
    after.split_whitespace().next().unwrap_or("Z") != "Z"
}

/// PIDs whose parent (PPID = `/proc/<pid>/stat` field after `comm`) is `parent`.
fn children_of(parent: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((_, after)) = stat.rsplit_once(')') else {
            continue;
        };
        let f: Vec<&str> = after.split_whitespace().collect();
        // f = [state, ppid, …]
        if f.get(1).and_then(|p| p.parse::<u32>().ok()) == Some(parent) {
            out.push(pid);
        }
    }
    out
}

fn poll_until(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    cond()
}

/// Start the mock LLM in its `slow` script (holds each response ~5s), waiting
/// until it binds the socket.
fn start_slow_llm(socket: &Path) -> Child {
    let c = Command::new(exe())
        .args(["--internal-mock-llm", socket.to_str().unwrap(), "slow"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn slow mock-llm");
    assert!(
        poll_until(Duration::from_secs(3), || socket.exists()),
        "mock-llm never bound"
    );
    c
}

#[test]
fn killing_the_supervisor_collapses_the_subagent_no_orphan() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.sock");
    let mut llm = start_slow_llm(&sock);

    let intel = format!("unix:{}", sock.display());
    // once-mode: the root subagent connects intelligence and then blocks ~5s
    // reading the slow model response — alive and supervised the whole time.
    let mut sup = Command::new(exe())
        .args([
            "--mode",
            "once",
            "--instruction",
            "x",
            "--intelligence",
            &intel,
            "--log-level",
            "error",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agentd supervisor");
    let sup_pid = sup.id();

    // Wait for the root subagent (a child process of the supervisor) to come up.
    let mut sub_pid = 0u32;
    let found = poll_until(Duration::from_secs(3), || {
        match children_of(sup_pid).first() {
            Some(&p) => {
                sub_pid = p;
                running(p)
            }
            None => false,
        }
    });
    assert!(found, "no live subagent appeared under the supervisor");

    // Collapse the tree from the top: SIGKILL the supervisor (no graceful drain,
    // no chance to run its own kill ladder — only PDEATHSIG can save us).
    signal(sup_pid, libc::SIGKILL);
    let _ = sup.wait();

    // The kernel SIGKILLs the subagent when its parent dies; it then reparents
    // to the subreaper/init and is reaped. Either way it stops running.
    let collapsed = poll_until(Duration::from_secs(5), || !running(sub_pid));

    signal(llm.id(), libc::SIGKILL);
    let _ = llm.wait();

    assert!(
        collapsed,
        "subagent {sub_pid} kept running after its supervisor died — PDEATHSIG leaked a process"
    );
}

#[test]
fn a_wedged_subagent_is_detected_stuck_and_force_killed_within_budget() {
    // Short liveness timeouts (env knobs) + a short drain budget so a frozen
    // subagent is classified Stuck and force-killed within a couple of seconds
    // instead of the production 120 s. The slow LLM keeps the subagent in its
    // model call; SIGSTOP then freezes its control thread too, so it stops
    // answering pings — the genuine "wedged, not busy" condition.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.sock");
    let mut llm = start_slow_llm(&sock);

    let intel = format!("unix:{}", sock.display());
    let mut sup = Command::new(exe())
        .args([
            "--mode",
            "once",
            "--instruction",
            "x",
            "--intelligence",
            &intel,
            "--drain-timeout",
            "1s",
            "--log-level",
            "info",
        ])
        .env("AGENTD_PROGRESS_TIMEOUT_MS", "400")
        .env("AGENTD_PONG_TIMEOUT_MS", "400")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentd supervisor");
    let sup_pid = sup.id();
    let mut err = sup.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        err.read_to_string(&mut s).ok();
        s
    });

    // Find the live subagent, then freeze it (control thread included).
    let mut sub_pid = 0u32;
    let found = poll_until(Duration::from_secs(3), || {
        match children_of(sup_pid).first() {
            Some(&p) => {
                sub_pid = p;
                running(p)
            }
            None => false,
        }
    });
    assert!(found, "no live subagent appeared under the supervisor");
    signal(sub_pid, libc::SIGSTOP);

    // The supervisor must classify it Stuck (no pongs) and force the kill ladder
    // within the drain budget — i.e. agentd *exits* rather than hang forever.
    let exited = poll_until(Duration::from_secs(8), || {
        sup.try_wait().ok().flatten().is_some()
    });
    let status = sup.try_wait().ok().flatten();
    let sub_gone = !running(sub_pid);

    signal(sub_pid, libc::SIGKILL); // cleanup if anything is still around
    signal(llm.id(), libc::SIGKILL);
    let _ = llm.wait();
    let out = reader.join().unwrap_or_default();

    assert!(
        exited,
        "supervisor hung on a wedged subagent instead of force-killing it:\n{out}"
    );
    assert!(
        out.contains(r#""event":"subagent.stuck""#),
        "the wedged subagent was not classified Stuck:\n{out}"
    );
    assert!(sub_gone, "the wedged subagent was not killed");
    // KillReason::Stuck maps to exit 124 (the deadline/stuck class).
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(124),
        "a stuck-killed run should exit 124:\n{out}"
    );
}
