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
    let Some(dir) = own_cgroup_dir() else { return false };
    let probe = dir.join(format!(".e2e-probe-{}", std::process::id()));
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

fn start_mock_llm(exe: &str, sock: &Path) -> Child {
    let child = Command::new(exe)
        .args(["--internal-mock-llm", sock.to_str().unwrap(), "final"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !sock.exists() {
        assert!(Instant::now() < deadline, "mock-llm never bound");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

#[test]
fn once_mode_places_the_root_in_a_cgroup_and_removes_it_on_exit() {
    if !cgroup_writable() {
        eprintln!("skip: cgroup-v2 tree not writable on this host");
        return;
    }
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let llm_sock = dir.path().join("llm.sock");
    let mut llm = start_mock_llm(exe, &llm_sock);

    let out = Command::new(exe)
        .args([
            "--instruction",
            "do a thing",
            "--intelligence",
            &format!("unix:{}", llm_sock.display()),
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
    assert_eq!(out.status.code(), Some(0), "run completed cleanly; stderr:\n{stderr}");

    // The feature armed (parent cgroup resolved + writable)…
    assert!(stderr.contains("\"cgroup.enabled\""), "expected cgroup.enabled; stderr:\n{stderr}");

    // …and the root subagent was placed in its per-run child cgroup. Parse the
    // event to recover the exact cgroup path so we can assert it was cleaned up.
    let placed = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["event"] == "cgroup.placed")
        .expect("a cgroup.placed event");
    assert_eq!(placed["ok"], true, "root placed into the cgroup: {placed}");
    let cgroup_path = placed["cgroup"].as_str().expect("cgroup path in the event");
    assert!(cgroup_path.contains("/run-"), "per-run cgroup name: {cgroup_path}");

    // RAII cleanup: the per-run cgroup dir is gone once the supervisor dropped.
    assert!(
        !Path::new(cgroup_path).exists(),
        "the per-run cgroup {cgroup_path} should be removed on exit"
    );
}
