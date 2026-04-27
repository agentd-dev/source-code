//! `shell.run` tool — spawn a local binary with argv-style args.
//!
//! No shell interpolation, no PATH lookup: the `command` field is a
//! literal path. Dynamic args come from the execution context as a
//! JSON array of strings — the runtime never concatenates them into
//! a shell command line, which keeps injection attacks off the
//! table by construction.
//!
//! Safety:
//!
//! 1. **Absolute-path only.** Relative paths are rejected at
//!    handler dispatch.
//! 2. **Canonicalised command.** `fs::canonicalize` before the
//!    policy check resolves symlinks; the allowlist matches the
//!    real inode target.
//! 3. **Policy gate.** `Policy::check_shell_run(canonical_path)`
//!    decides whether the command is allowed.
//! 4. **Output caps.** 64 KiB each on stdout and stderr; overflow is
//!    truncated with a `"truncated": true` marker.
//! 5. **Timeout.** Per-node `timeout_secs` (default 30). On deadline
//!    the child is SIGKILLed; output captured so far is returned.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::engine::{ExecutionContext, HandlerRegistry, NodeHandler, NodeOutcome};
use crate::error::{Error, Result};
use crate::tools::policy::{Decision, PolicyRef};
use crate::workflow::{Node, NodeKind};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub(crate) fn register(registry: &mut HandlerRegistry, policy: PolicyRef) {
    registry.register("shell_run", Box::new(ShellRunHandler { policy }));
}

pub struct ShellRunHandler {
    policy: PolicyRef,
}

impl NodeHandler for ShellRunHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::ShellRun {
            command,
            args_from,
            timeout_secs,
        } = &node.kind
        else {
            return Err(Error::Tool {
                tool: "shell_run".into(),
                reason: format!(
                    "handler for `shell_run` received node `{}` of kind `{}`",
                    node.id,
                    node.kind.name()
                ),
            });
        };

        // Absolute-path only, no PATH search.
        let cmd_path = PathBuf::from(command);
        if !cmd_path.is_absolute() {
            return Err(Error::Tool {
                tool: "shell_run".into(),
                reason: format!(
                    "shell_run requires an absolute path; got `{}`",
                    cmd_path.display()
                ),
            });
        }
        let canonical = std::fs::canonicalize(&cmd_path).map_err(|e| Error::Tool {
            tool: "shell_run".into(),
            reason: format!("resolve {}: {e}", cmd_path.display()),
        })?;

        // Policy gate on the resolved inode path.
        match self.policy.check_shell_run(&canonical) {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                return Err(Error::Policy(format!(
                    "shell_run `{}`: {reason}",
                    canonical.display()
                )));
            }
        }

        // Resolve optional args.
        let args = match args_from {
            Some(path) => {
                let raw = ctx.resolve_path(path).cloned().unwrap_or(Value::Null);
                match raw {
                    Value::Null => Vec::new(),
                    Value::Array(items) => items
                        .into_iter()
                        .map(|v| match v {
                            Value::String(s) => Ok(s),
                            other => Err(Error::Tool {
                                tool: "shell_run".into(),
                                reason: format!(
                                    "args element must be a string; got {}",
                                    super::value_type_name(&other)
                                ),
                            }),
                        })
                        .collect::<Result<Vec<_>>>()?,
                    other => {
                        return Err(Error::Tool {
                            tool: "shell_run".into(),
                            reason: format!(
                                "args_from `{path}` must resolve to an array of strings; got {}",
                                super::value_type_name(&other)
                            ),
                        });
                    }
                }
            }
            None => Vec::new(),
        };

        // Dry-run: surface intent, skip the spawn.
        if ctx.dry_run {
            return Ok(NodeOutcome::Continue {
                value: json!({
                    "command": canonical.display().to_string(),
                    "args": args,
                    "dry_run": true,
                }),
                branch: None,
            });
        }

        let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
        let outcome = run(&canonical, &args, timeout)?;

        let branch = if outcome.exit_code == 0 && outcome.signal.is_none() {
            None
        } else {
            Some("error".to_string())
        };

        Ok(NodeOutcome::Continue {
            value: json!({
                "command": canonical.display().to_string(),
                "args": args,
                "exit_code": outcome.exit_code,
                "signal": outcome.signal,
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
                "truncated": outcome.truncated,
                "timed_out": outcome.timed_out,
                "duration_ms": outcome.duration_ms,
            }),
            branch,
        })
    }
}

// ---------------------------------------------------------------------------
// Spawn + capture + timeout
// ---------------------------------------------------------------------------

struct ShellOutcome {
    exit_code: i32,
    signal: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
    timed_out: bool,
    duration_ms: u64,
}

fn run(command: &Path, args: &[String], timeout: Duration) -> Result<ShellOutcome> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Curated environment — no PATH inheritance surprises.
        // Handlers that actually need env vars go through the
        // `read_env` node + build their own command line.
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("LANG", "C.UTF-8");

    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| Error::Tool {
        tool: "shell_run".into(),
        reason: format!("spawn {}: {e}", command.display()),
    })?;

    // Read stdout + stderr in background threads so a chatty child
    // doesn't block the deadline check.
    let stdout_pipe = child.stdout.take().expect("stdout piped above");
    let stderr_pipe = child.stderr.take().expect("stderr piped above");
    let out_handle = thread::spawn(move || read_capped(stdout_pipe));
    let err_handle = thread::spawn(move || read_capped(stderr_pipe));

    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    timed_out = true;
                    // Loop again to collect the exit status.
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                return Err(Error::Tool {
                    tool: "shell_run".into(),
                    reason: format!("wait: {e}"),
                });
            }
        }
    }
    let status = child.wait().map_err(|e| Error::Tool {
        tool: "shell_run".into(),
        reason: format!("final wait: {e}"),
    })?;

    let (stdout, stdout_trunc) = out_handle.join().unwrap_or_else(|_| (Vec::new(), false));
    let (stderr, stderr_trunc) = err_handle.join().unwrap_or_else(|_| (Vec::new(), false));

    // On Unix, extract the signal when no exit code is present.
    #[cfg(unix)]
    let (exit_code, signal) = {
        use std::os::unix::process::ExitStatusExt;
        (status.code().unwrap_or(-1), status.signal())
    };
    #[cfg(not(unix))]
    let (exit_code, signal) = (status.code().unwrap_or(-1), None);

    Ok(ShellOutcome {
        exit_code,
        signal,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: stdout_trunc || stderr_trunc,
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Read a pipe into a buffer, capped at `MAX_OUTPUT_BYTES`. Returns
/// (bytes, was_truncated).
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
                    // Keep draining so the child isn't blocked on a
                    // full pipe, but discard bytes past the cap.
                    continue;
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{RunOptions, TriggerMeta};
    use crate::tools::policy::{Decision, Policy, allow_all};
    use std::sync::Arc;

    fn ctx(input: Value) -> ExecutionContext {
        ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(input),
            &RunOptions::default(),
        )
    }

    fn node(command: &str, args_from: Option<&str>, timeout_secs: Option<u64>) -> Node {
        Node {
            id: "n".into(),
            kind: NodeKind::ShellRun {
                command: command.into(),
                args_from: args_from.map(Into::into),
                timeout_secs,
            },
        }
    }

    #[test]
    fn runs_true_cleanly() {
        let h = ShellRunHandler {
            policy: allow_all(),
        };
        let mut c = ctx(json!({}));
        let out = h.handle(&node("/bin/true", None, None), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, branch } => {
                assert_eq!(value["exit_code"], 0);
                assert!(branch.is_none(), "clean exit = no error branch");
                assert_eq!(value["stdout"], "");
                assert_eq!(value["timed_out"], false);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn non_zero_exit_sets_error_branch() {
        let h = ShellRunHandler {
            policy: allow_all(),
        };
        let mut c = ctx(json!({}));
        let out = h.handle(&node("/bin/false", None, None), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, branch } => {
                assert_ne!(value["exit_code"], 0);
                assert_eq!(branch.as_deref(), Some("error"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn captures_stdout() {
        let h = ShellRunHandler {
            policy: allow_all(),
        };
        let mut c = ctx(json!({ "args": ["hello world"] }));
        let out = h
            .handle(&node("/bin/echo", Some("trigger.args"), None), &mut c)
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["exit_code"], 0);
                assert_eq!(value["stdout"].as_str().unwrap().trim(), "hello world");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn relative_path_rejected() {
        let h = ShellRunHandler {
            policy: allow_all(),
        };
        let mut c = ctx(json!({}));
        let err = h.handle(&node("true", None, None), &mut c).unwrap_err();
        assert!(format!("{err}").contains("absolute path"));
    }

    #[test]
    fn nonexistent_path_errors_at_canonicalize() {
        let h = ShellRunHandler {
            policy: allow_all(),
        };
        let mut c = ctx(json!({}));
        let err = h
            .handle(
                &node("/definitely/not/a/real/binary/path", None, None),
                &mut c,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("resolve"));
    }

    #[test]
    fn policy_deny_blocks_execution() {
        struct NoShell;
        impl Policy for NoShell {
            fn check_shell_run(&self, _: &Path) -> Decision {
                Decision::Deny("nope".into())
            }
        }
        let h = ShellRunHandler {
            policy: Arc::new(NoShell),
        };
        let mut c = ctx(json!({}));
        let err = h
            .handle(&node("/bin/true", None, None), &mut c)
            .unwrap_err();
        assert!(format!("{err}").contains("nope"));
    }

    #[test]
    fn dry_run_does_not_spawn() {
        let h = ShellRunHandler {
            policy: allow_all(),
        };
        let mut c = ctx(json!({}));
        c.dry_run = true;
        // Canonicalize still runs, so we need a real command path. `/bin/true`
        // exists on any Linux system with coreutils.
        let out = h.handle(&node("/bin/true", None, None), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["dry_run"], true);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn args_non_string_element_rejected() {
        let h = ShellRunHandler {
            policy: allow_all(),
        };
        let mut c = ctx(json!({ "args": ["ok", 42] }));
        let err = h
            .handle(&node("/bin/echo", Some("trigger.args"), None), &mut c)
            .unwrap_err();
        assert!(format!("{err}").contains("must be a string"));
    }

    #[test]
    fn timeout_kills_long_running_child() {
        // `/bin/sleep 5` with a 1-second timeout.
        let h = ShellRunHandler {
            policy: allow_all(),
        };
        let mut c = ctx(json!({ "args": ["5"] }));
        let out = h
            .handle(&node("/bin/sleep", Some("trigger.args"), Some(1)), &mut c)
            .unwrap();
        match out {
            NodeOutcome::Continue { value, branch } => {
                assert_eq!(value["timed_out"], true);
                assert_eq!(branch.as_deref(), Some("error"));
                // Elapsed should be roughly the timeout, not 5s.
                let ms = value["duration_ms"].as_u64().unwrap();
                assert!(ms < 3_000, "timed-out child took too long: {ms}ms");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn env_clear_is_effective() {
        // `/usr/bin/env` prints the environment. With env_clear +
        // explicit PATH + LANG, output should contain exactly those
        // two (PATH + LANG). We don't assert order — just absence of
        // anything the test runner might have leaked.
        let h = ShellRunHandler {
            policy: allow_all(),
        };
        let mut c = ctx(json!({}));
        // Not every distro carries /usr/bin/env at that exact path;
        // skip the assertion if we can't canonicalize it.
        if std::fs::canonicalize("/usr/bin/env").is_err() {
            return;
        }
        let out = h.handle(&node("/usr/bin/env", None, None), &mut c).unwrap();
        if let NodeOutcome::Continue { value, .. } = out {
            let stdout = value["stdout"].as_str().unwrap();
            assert!(stdout.contains("LANG=C.UTF-8"), "stdout:\n{stdout}");
            // No `HOME=` should leak from the test harness.
            assert!(!stdout.contains("HOME="), "stdout:\n{stdout}");
        }
    }
}
