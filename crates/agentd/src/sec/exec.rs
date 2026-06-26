//! The gated `exec` self-tool — the one non-MCP capability. RFC 0005 §exec,
//! RFC 0012 §exec.
//!
//! Off by default; exposed only when `--enable-exec` is set. It runs a local
//! command **argv-style** (no shell, no PATH lookup, no interpolation — so
//! command injection is off the table by construction). The strongest leg of
//! the lethal trifecta, so it is deliberately the most constrained tool:
//!
//! - `argv[0]` must be an **absolute path** to an existing, executable file
//!   (otherwise the call fails as an observation — the model adapts).
//! - The child runs with a **scrubbed environment** (curated PATH, no inherited
//!   vars), `stdin` closed, output capped.
//! - It is its **own process group**; on a timeout the whole group is
//!   `killpg`'d (catching any grandchildren it spawned) — the same teardown
//!   primitive as the supervisor's kill ladder (RFC 0003).
//!
//! Salvaged/adapted from the retired `tools/shell.rs::run()`.

use crate::supervisor::kill::kill_group;
use crate::wire::intel::ToolDef;
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Default per-call wall-clock bound; the child's group is killed past it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const CURATED_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: i32,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
}

/// Run `argv` (its first element is the absolute path) with a timeout. Returns `Err` for a
/// validation/spawn failure (a recoverable observation); `Ok` for a command
/// that *ran* — its exit code is data the model interprets.
pub fn run(argv: &[String], timeout: Duration) -> Result<ExecResult, String> {
    let prog = argv.first().ok_or_else(|| "exec: empty argv".to_string())?;
    validate_program(prog)?;

    let mut cmd = Command::new(prog);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Scrubbed env: no inherited vars, curated PATH (the binary is an
        // absolute path anyway; PATH is for anything it execs).
        .env_clear()
        .env("PATH", CURATED_PATH)
        .env("LANG", "C.UTF-8");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0); // own group → killpg reaches grandchildren
                Ok(())
            });
        }
    }

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("exec: spawn {prog}: {e}"))?;
    let pgid = child.id() as i32;

    // Drain stdout/stderr on threads so a chatty child can't deadlock the wait.
    let out_pipe = child.stdout.take().expect("stdout piped");
    let err_pipe = child.stderr.take().expect("stderr piped");
    let out_h = thread::spawn(move || read_capped(out_pipe));
    let err_h = thread::spawn(move || read_capped(err_pipe));

    let mut timed_out = false;
    // Capture the exit status from `try_wait` itself — no redundant final
    // `child.wait()`, which would error with ECHILD if anything else in the
    // process reaped the child first (defensive: exec runs on a subagent's single
    // loop thread today, with no concurrent reaper, but this keeps it robust).
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    kill_group(pgid); // SIGKILL the whole group
                    let _ = child.kill();
                    timed_out = true;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(format!("exec: wait: {e}")),
        }
    };

    let (stdout, out_trunc) = out_h.join().unwrap_or_else(|_| (Vec::new(), false));
    let (stderr, err_trunc) = err_h.join().unwrap_or_else(|_| (Vec::new(), false));

    #[cfg(unix)]
    let (exit_code, signal) = {
        use std::os::unix::process::ExitStatusExt;
        (status.code().unwrap_or(-1), status.signal())
    };
    #[cfg(not(unix))]
    let (exit_code, signal) = (status.code().unwrap_or(-1), None);

    Ok(ExecResult {
        exit_code,
        signal,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: out_trunc || err_trunc,
        timed_out,
    })
}

fn validate_program(prog: &str) -> Result<(), String> {
    let path = Path::new(prog);
    if !path.is_absolute() {
        return Err(format!("exec: '{prog}' must be an absolute path"));
    }
    let meta = std::fs::metadata(path).map_err(|_| format!("exec: '{prog}' does not exist"))?;
    if !meta.is_file() {
        return Err(format!("exec: '{prog}' is not a file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(format!("exec: '{prog}' is not executable"));
        }
    }
    Ok(())
}

fn read_capped<R: Read>(mut r: R) -> (Vec<u8>, bool) {
    let mut buf = Vec::with_capacity(4 * 1024);
    let mut chunk = [0u8; 8 * 1024];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = MAX_OUTPUT_BYTES.saturating_sub(buf.len());
                if room == 0 {
                    truncated = true;
                    continue; // keep draining so the child isn't blocked
                }
                let take = n.min(room);
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
}

/// Format an [`ExecResult`] as the observation fed back to the model.
pub fn format_result(r: &ExecResult) -> String {
    let mut s = String::new();
    if r.timed_out {
        s.push_str("[timed out — process group killed]\n");
    }
    s.push_str(&format!("exit_code: {}\n", r.exit_code));
    if let Some(sig) = r.signal {
        s.push_str(&format!("killed_by_signal: {sig}\n"));
    }
    if !r.stdout.is_empty() {
        s.push_str(&format!("stdout:\n{}\n", r.stdout));
    }
    if !r.stderr.is_empty() {
        s.push_str(&format!("stderr:\n{}\n", r.stderr));
    }
    if r.truncated {
        s.push_str("[output truncated at 64 KiB]\n");
    }
    s
}

/// The `exec` tool definition (advertised only when exec is enabled).
pub fn tool_def() -> ToolDef {
    ToolDef {
        name: "exec".into(),
        description: "Run a local command directly (no shell). 'argv' is an array of strings; \
            argv[0] MUST be the absolute path to an existing executable and the rest are its \
            arguments. The environment is scrubbed. Returns the exit code, stdout, and stderr."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "absolute-path executable followed by its arguments"
                }
            },
            "required": ["argv"]
        }),
    }
}

/// Parse + run an `exec` tool call. Returns `(observation, is_error)` for the
/// loop. `is_error` is true only for a validation/spawn failure; a command that
/// ran (any exit code) is a normal observation.
pub fn handle_call(args: &Value, timeout: Duration) -> (String, bool) {
    let argv: Vec<String> = match args.get("argv").and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => {
            return (
                "error: exec requires 'argv' (an array of strings)".into(),
                true,
            );
        }
    };
    if argv.is_empty() {
        return ("error: exec 'argv' must not be empty".into(), true);
    }
    match run(&argv, timeout) {
        Ok(r) => (format_result(&r), false),
        Err(e) => (e, true),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn runs_echo_and_captures_stdout() {
        let r = run(&["/bin/echo".into(), "hello".into()], DEFAULT_TIMEOUT).unwrap();
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("hello"));
        assert!(!r.timed_out);
    }

    #[test]
    fn relative_path_is_rejected() {
        let err = run(&["echo".into()], DEFAULT_TIMEOUT).unwrap_err();
        assert!(err.contains("absolute path"));
    }

    #[test]
    fn nonexistent_is_rejected() {
        let err = run(&["/nonexistent/agentd-xyz".into()], DEFAULT_TIMEOUT).unwrap_err();
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn timeout_kills_and_flags() {
        // /bin/sleep 5 with a 200ms bound → killed, timed_out true.
        let sleep = if Path::new("/bin/sleep").exists() {
            "/bin/sleep"
        } else {
            "/usr/bin/sleep"
        };
        let r = run(&[sleep.into(), "5".into()], Duration::from_millis(200)).unwrap();
        assert!(r.timed_out, "expected timeout");
    }

    #[test]
    fn handle_call_missing_argv_is_error() {
        let (_msg, err) = handle_call(&json!({}), DEFAULT_TIMEOUT);
        assert!(err);
    }

    #[test]
    fn format_includes_exit_and_stdout() {
        let r = ExecResult {
            exit_code: 0,
            signal: None,
            stdout: "hi".into(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
        };
        let s = format_result(&r);
        assert!(s.contains("exit_code: 0"));
        assert!(s.contains("hi"));
    }
}
