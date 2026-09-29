// SPDX-License-Identifier: AGPL-3.0-only
//! The **guarded local command runner** behind the `exec` internal tool —
//! compiled only under `--features exec`.
//!
//! agentd's default posture is **no local execution**: a compromised model must
//! not be able to reach the host it runs on. This runner exists only for
//! operators who explicitly opt in at two layers (the `exec` cargo feature and
//! `security.exec.enabled`), and it is defensive by construction:
//!
//! * **argv, never a shell** — `cmd` + `args` are passed to `execve` directly, so
//!   there is no shell metacharacter interpretation and no injection surface.
//! * **allow-list** — `argv[0]` must be listed in `security.exec.allow`; empty =
//!   deny all.
//! * **workdir confinement** — commands run in `security.exec.workdir`; a
//!   requested `cwd` is canonicalized and must resolve *inside* it (no `..`
//!   escape, no symlink escape).
//! * **timeout** — the child is killed past the (clamped) deadline.
//! * **output cap** — stdout+stderr are truncated to `max_output` bytes.
//! * **minimal env** — the child inherits ONLY the named `security.exec.env`
//!   variables; the agent's own environment (and its secrets) is never passed.
//!
//! The `exec` tool also carries the `sensitive` + `egress` trifecta tags, so the
//! Rule-of-Two gate refuses to grant it alongside untrusted input.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::supervisor::reap::WaitOutcome;

/// Resolve + confine the working directory. `workdir` must exist; a requested
/// `cwd` (relative to it, or absolute) must canonicalize to a path inside it.
pub(crate) fn resolve_cwd(workdir: &Path, req: Option<&str>) -> Result<PathBuf, String> {
    let base = workdir
        .canonicalize()
        .map_err(|e| format!("workdir {}: {e}", workdir.display()))?;
    let target = match req.filter(|c| !c.is_empty()) {
        None => base.clone(),
        Some(c) => {
            let p = Path::new(c);
            let joined = if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            };
            joined.canonicalize().map_err(|e| format!("cwd {c}: {e}"))?
        }
    };
    if !target.starts_with(&base) {
        return Err(format!(
            "cwd {} escapes workdir {}",
            target.display(),
            base.display()
        ));
    }
    Ok(target)
}

/// Run one allow-listed command and return `{stdout, stderr, exit_code,
/// timed_out}`. The caller has already checked the allow-list + resolved `cwd`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_command(
    cmd: &str,
    argv: &[String],
    cwd: &Path,
    stdin: Option<&str>,
    timeout: Duration,
    max_output: usize,
    env_pass: &[String],
) -> Result<Value, String> {
    let mut c = Command::new(cmd);
    c.args(argv)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A minimal environment: only the explicitly named variables (never the
    // agent's own env / secrets).
    c.env_clear();
    for k in env_pass {
        if let Ok(v) = std::env::var(k) {
            c.env(k, v);
        }
    }
    // Nor any descriptor the host left inheritable: a model-chosen command
    // gets its three pipes and nothing else, however agentd was started.
    crate::signals::pass_only_stdio(&mut c);
    // Routed from the fork on: a workflow step runs this on a `tool:exec`
    // thread while the reactor's tick reaps every exited child in the process,
    // so a plain `try_wait` here would lose the status to it.
    let mut child = crate::supervisor::reaper::spawn_owned(|| c.spawn())
        .map_err(|e| format!("spawn {cmd}: {e}"))?;

    // Feed stdin on a thread (so a child that writes before reading can't deadlock).
    if let Some(mut si) = child.take_stdin() {
        let input = stdin.unwrap_or("").as_bytes().to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&input);
            // dropping `si` closes the pipe (EOF)
        });
    }
    // Read stdout/stderr on threads, capped (avoids a full-pipe deadlock).
    let out = child.take_stdout();
    let err = child.take_stderr();
    let oh = out.map(|r| std::thread::spawn(move || read_capped(r, max_output)));
    let eh = err.map(|r| std::thread::spawn(move || read_capped(r, max_output)));

    // Wait with a deadline; past it, kill and still collect the exit, so a
    // timed-out command leaves no zombie behind.
    let deadline = Instant::now() + timeout;
    let status = match child.wait_until(Some(deadline)) {
        Ok(Some(s)) => Some(s),
        Ok(None) => {
            child.kill();
            child
                .wait_until(None)
                .map_err(|e| format!("wait {cmd}: {e}"))?;
            None
        }
        Err(e) => return Err(format!("wait {cmd}: {e}")),
    };

    let stdout = oh.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = eh.and_then(|h| h.join().ok()).unwrap_or_default();
    let timed_out = status.is_none();
    // A signal death has no exit code, as with `ExitStatus::code`.
    let exit_code = match status {
        Some(WaitOutcome::Exited(code)) => code,
        _ => -1,
    };
    Ok(json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": exit_code,
        "timed_out": timed_out,
    }))
}

/// Read a stream to a `String`, capping the retained bytes at `cap` (the rest is
/// drained but discarded, so the child never blocks on a full pipe).
fn read_capped<R: Read>(mut r: R, cap: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_workdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentd-exec-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn runs_an_allowed_command_and_captures_output() {
        let wd = tmp_workdir("echo");
        let cwd = resolve_cwd(&wd, None).unwrap();
        let out = run_command(
            "echo",
            &["hello".into(), "world".into()],
            &cwd,
            None,
            Duration::from_secs(5),
            4096,
            &[],
        )
        .unwrap();
        assert_eq!(out["stdout"], "hello world\n");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["timed_out"], false);
        std::fs::remove_dir_all(&wd).ok();
    }

    #[test]
    fn stdin_is_delivered_and_output_capped() {
        let wd = tmp_workdir("cat");
        let cwd = resolve_cwd(&wd, None).unwrap();
        let out = run_command(
            "cat",
            &[],
            &cwd,
            Some("abcdefghij"),
            Duration::from_secs(5),
            4, // cap
            &[],
        )
        .unwrap();
        assert_eq!(out["stdout"], "abcd", "output is capped at 4 bytes");
        std::fs::remove_dir_all(&wd).ok();
    }

    #[test]
    fn a_slow_command_is_killed_at_the_timeout() {
        let wd = tmp_workdir("sleep");
        let cwd = resolve_cwd(&wd, None).unwrap();
        let out = run_command(
            "sleep",
            &["5".into()],
            &cwd,
            None,
            Duration::from_millis(200),
            4096,
            &[],
        )
        .unwrap();
        assert_eq!(out["timed_out"], true, "killed at the deadline");
        std::fs::remove_dir_all(&wd).ok();
    }

    /// A timed-out command is reported `timed_out`, and by the time the run
    /// returns its child is killed and reaped: no process, not even a zombie,
    /// is left under the pid.
    #[test]
    fn a_timed_out_command_is_killed_and_reaped() {
        let wd = tmp_workdir("reaped");
        let cwd = resolve_cwd(&wd, None).unwrap();
        let started = Instant::now();
        let out = run_command(
            "sh",
            &["-c".into(), "echo $$; exec sleep 30".into()],
            &cwd,
            None,
            Duration::from_millis(300),
            4096,
            &["PATH".into()],
        )
        .unwrap();
        std::fs::remove_dir_all(&wd).ok();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "killed at the deadline, not left to run: {out}"
        );
        assert_eq!(out["timed_out"], true, "{out}");
        assert_eq!(out["exit_code"], -1, "{out}");
        let pid: i32 = out["stdout"].as_str().unwrap().trim().parse().unwrap();
        // A zombie still answers signal 0; only a reaped pid is ESRCH.
        let alive = unsafe { libc::kill(pid, 0) };
        assert!(
            alive == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "pid {pid} is still there (running or a zombie)"
        );
    }

    const REAPER_CHILD: &str = "AGENTD_EXEC_REAPER_CHILD";

    /// A workflow's `exec` step runs on its own thread while the daemon's
    /// reactor reaps every exited child in the process: every run still
    /// reports its own command's exit. Run in a child test process, because a
    /// tight `reap_and_dispatch` loop would reap the children of every other
    /// test in this binary.
    #[test]
    fn exec_keeps_its_exit_status_under_a_ticking_reaper() {
        if std::env::var_os(REAPER_CHILD).is_some() {
            return runs_under_a_ticking_reaper();
        }
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::exec::tests::exec_keeps_its_exit_status_under_a_ticking_reaper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(REAPER_CHILD, "1")
            .output()
            .expect("spawn the test binary");
        let log = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "the child failed:\n{log}");
        // The filter matched the test, so the child really ran it.
        assert!(log.contains("1 passed"), "the child ran nothing:\n{log}");
    }

    fn runs_under_a_ticking_reaper() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let reaper = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    crate::supervisor::reaper::reap_and_dispatch();
                    std::thread::yield_now();
                }
            })
        };
        let wd = tmp_workdir("ticking");
        let cwd = resolve_cwd(&wd, None).unwrap();
        for i in 0..200 {
            let out = run_command(
                "echo",
                &[format!("run-{i}")],
                &cwd,
                None,
                Duration::from_secs(10),
                4096,
                &[],
            )
            .unwrap_or_else(|e| panic!("run {i}: {e}"));
            assert_eq!(out["exit_code"], 0, "run {i}: {out}");
            assert_eq!(out["stdout"], format!("run-{i}\n"), "run {i}: {out}");
            assert_eq!(out["timed_out"], false, "run {i}: {out}");
        }
        stop.store(true, Ordering::Relaxed);
        reaper.join().unwrap();
        std::fs::remove_dir_all(&wd).ok();
    }

    /// An embedder's process never ran the binary's start-up marking, so a
    /// descriptor its host left inheritable is still inheritable here: the
    /// exec tool's child holds none of it, while a plain spawn of the same
    /// command does — the stray was there to leak.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_exec_child_holds_no_descriptor_the_host_left_open() {
        use crate::signals::stray_fd::{StrayPipe, listing, lock};
        let _g = lock();
        let stray = StrayPipe::open();
        let plain = listing(Command::new("ls").args(["-l", "/proc/self/fd"]));
        assert!(stray.held_in(&plain), "a plain spawn inherits it: {plain}");

        let wd = tmp_workdir("fds");
        let cwd = resolve_cwd(&wd, None).unwrap();
        let out = run_command(
            "ls",
            &["-l".into(), "/proc/self/fd".into()],
            &cwd,
            None,
            Duration::from_secs(5),
            64 * 1024,
            &[],
        )
        .unwrap();
        std::fs::remove_dir_all(&wd).ok();
        let fds = out["stdout"].as_str().unwrap();
        assert!(fds.contains("pipe:["), "the listing ran: {out}");
        assert!(!stray.held_in(fds), "the exec child holds the stray: {fds}");
    }

    #[test]
    fn cwd_cannot_escape_the_workdir() {
        let wd = tmp_workdir("confine");
        assert!(resolve_cwd(&wd, Some("../..")).is_err());
        assert!(resolve_cwd(&wd, Some("/etc")).is_err());
        // A subdir inside is fine.
        std::fs::create_dir_all(wd.join("sub")).unwrap();
        assert!(resolve_cwd(&wd, Some("sub")).is_ok());
        std::fs::remove_dir_all(&wd).ok();
    }

    #[test]
    fn env_is_minimal() {
        // A var NOT in env_pass must be absent in the child.
        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("AGENTD_EXEC_SECRET", "leak") };
        let wd = tmp_workdir("env");
        let cwd = resolve_cwd(&wd, None).unwrap();
        let out = run_command(
            "env",
            &[],
            &cwd,
            None,
            Duration::from_secs(5),
            8192,
            &["PATH".into()],
        )
        .unwrap();
        let s = out["stdout"].as_str().unwrap();
        assert!(
            !s.contains("AGENTD_EXEC_SECRET"),
            "secret env not inherited: {s}"
        );
        unsafe { std::env::remove_var("AGENTD_EXEC_SECRET") };
        std::fs::remove_dir_all(&wd).ok();
    }
}
