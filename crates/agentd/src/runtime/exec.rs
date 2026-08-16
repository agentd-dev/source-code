// SPDX-License-Identifier: Apache-2.0
//! The **guarded local command runner** behind the `exec` internal tool
//! (RFC 0028 §exec) — compiled only under `--features exec`.
//!
//! agentd's posture is **no local execution** (RFC 0012); this runner exists only
//! for operators who explicitly opt in, and it is defensive by construction:
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
    let mut child = c.spawn().map_err(|e| format!("spawn {cmd}: {e}"))?;

    // Feed stdin on a thread (so a child that writes before reading can't deadlock).
    if let Some(mut si) = child.stdin.take() {
        let input = stdin.unwrap_or("").as_bytes().to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&input);
            // dropping `si` closes the pipe (EOF)
        });
    }
    // Read stdout/stderr on threads, capped (avoids a full-pipe deadlock).
    let out = child.stdout.take();
    let err = child.stderr.take();
    let oh = out.map(|r| std::thread::spawn(move || read_capped(r, max_output)));
    let eh = err.map(|r| std::thread::spawn(move || read_capped(r, max_output)));

    // Wait with a deadline; kill on timeout.
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(e) => return Err(format!("wait {cmd}: {e}")),
        }
    };

    let stdout = oh.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = eh.and_then(|h| h.join().ok()).unwrap_or_default();
    let timed_out = status.is_none();
    let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);
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
