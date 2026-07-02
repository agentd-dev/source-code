// SPDX-License-Identifier: Apache-2.0
//! Black-box test of the opt-in cgroup-v2 active enforcement (`--cgroup auto`):
//! a once-mode run arms a per-run child cgroup, places its root subagent there,
//! completes normally, and removes the cgroup on the way out. The atomic
//! `cgroup.kill` teardown mechanism itself is unit-proven in
//! `supervisor::cgroup` (a `setsid` escapee is still reaped); here we prove the
//! *supervisor wiring* end-to-end with the real binary.
//!
//! Skips cleanly where the cgroup-v2 tree isn't writable (no root / no
//! delegation / off-cgroup) — the feature is never required.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The unit's own cgroup-v2 dir, from the `0::<path>` line of /proc/self/cgroup.
fn own_cgroup_dir() -> Option<PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = content.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    Some(Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/')))
}

/// Can we actually create + remove a child cgroup here? If not, skip the test.
fn cgroup_writable() -> bool {
    let Some(dir) = own_cgroup_dir() else {
        return false;
    };
    let probe = dir.join(format!(".e2e-probe-{}", std::process::id()));
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

fn start_mock_llm(exe: &str, sock: &Path) -> (Child, String) {
    let child = Command::new(exe)
        .args(["--internal-mock-llm", sock.to_str().unwrap(), "final"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !sock.exists() {
        assert!(Instant::now() < deadline, "mock-llm never announced");
        std::thread::sleep(Duration::from_millis(20));
    }
    let addr = std::fs::read_to_string(sock).expect("read mock-llm addr-file");
    (child, format!("http://{}", addr.trim()))
}

#[test]
fn once_mode_places_the_root_in_a_cgroup_and_removes_it_on_exit() {
    if !cgroup_writable() {
        eprintln!("skip: cgroup-v2 tree not writable on this host");
        return;
    }
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let llm_sock = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(exe, &llm_sock);

    let out = Command::new(exe)
        .args([
            "--instruction",
            "do a thing",
            "--intelligence",
            &intel,
            "--model",
            "m",
            "--cgroup",
            "auto",
            "--log-level",
            "info",
        ])
        .output()
        .expect("run agentd");

    let _ = llm.kill();
    let _ = llm.wait();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "run completed cleanly; stderr:\n{stderr}"
    );

    // The feature armed (parent cgroup resolved + writable)…
    assert!(
        stderr.contains("\"cgroup.enabled\""),
        "expected cgroup.enabled; stderr:\n{stderr}"
    );

    // …and the root subagent was placed in its per-run child cgroup. Parse the
    // event to recover the exact cgroup path so we can assert it was cleaned up.
    let placed = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["event"] == "cgroup.placed")
        .expect("a cgroup.placed event");
    assert_eq!(placed["ok"], true, "root placed into the cgroup: {placed}");
    let cgroup_path = placed["cgroup"].as_str().expect("cgroup path in the event");
    assert!(
        cgroup_path.contains("/run-"),
        "per-run cgroup name: {cgroup_path}"
    );

    // RAII cleanup: the per-run cgroup dir is gone once the supervisor dropped.
    assert!(
        !Path::new(cgroup_path).exists(),
        "the per-run cgroup {cgroup_path} should be removed on exit"
    );
}

/// Whether a manager cgroup directly under the cgroup-v2 root can delegate the
/// `pids` controller (the precondition for enforceable per-run limits). Probes +
/// cleans up. Returns the manager path to reuse, or `None` to skip.
fn root_delegating_manager() -> Option<PathBuf> {
    let mgr = Path::new("/sys/fs/cgroup").join(format!("agentd-e2e-limits-{}", std::process::id()));
    std::fs::create_dir(&mgr).ok()?;
    if std::fs::write(mgr.join("cgroup.subtree_control"), "+pids").is_err() {
        let _ = std::fs::remove_dir(&mgr);
        return None;
    }
    Some(mgr)
}

#[test]
fn once_mode_applies_hard_limits_when_the_parent_delegates_controllers() {
    let Some(mgr) = root_delegating_manager() else {
        eprintln!("skip: no cgroup parent that delegates controllers on this host");
        return;
    };
    // Ensure the manager dir is removed even if an assertion fails.
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::write(self.0.join("cgroup.kill"), "1");
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let _cleanup = Cleanup(mgr.clone());

    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let llm_sock = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(exe, &llm_sock);

    let out = Command::new(exe)
        .args([
            "--instruction",
            "do a thing",
            "--intelligence",
            &intel,
            "--model",
            "m",
            "--cgroup",
            mgr.to_str().unwrap(),
            "--cgroup-pids-max",
            "64",
            "--cgroup-memory-max",
            "256M",
            "--log-level",
            "info",
        ])
        .output()
        .expect("run agentd");

    let _ = llm.kill();
    let _ = llm.wait();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "run completed cleanly; stderr:\n{stderr}"
    );

    // The limits were accepted, normalized, and (since the parent delegates the
    // controllers) actually engaged — no `cgroup.limits_unavailable` warning.
    let enabled = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["event"] == "cgroup.enabled")
        .expect("a cgroup.enabled event");
    assert_eq!(enabled["pids_max"], "64", "pids.max normalized: {enabled}");
    assert_eq!(
        enabled["memory_max"],
        (256 * 1024 * 1024).to_string(),
        "memory.max normalized: {enabled}"
    );
    assert!(
        !stderr.contains("cgroup.limits_unavailable"),
        "controllers delegate here, so limits must engage; stderr:\n{stderr}"
    );
}
