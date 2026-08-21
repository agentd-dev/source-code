// SPDX-License-Identifier: Apache-2.0
//! `--prompt-missing` end to end, on a REAL pseudo-terminal: a config whose
//! workflow references `{{secret:…}}` that is not in the environment
//!
//!   1. WITHOUT the flag: refused at admission (exit 2), naming the secret —
//!      fail-closed is the default, prompting is the opt-in.
//!   2. WITH the flag: the daemon asks on `/dev/tty` (echo OFF — the typed
//!      value must not appear on the terminal), the entered value resolves the
//!      reference, and the workflow's HTTP request carries it — proof the
//!      prompted value reached the point of use, not just the validator.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SECRET_NAME: &str = "PM_E2E_API_KEY";
const SECRET_VALUE: &str = "s3cr3t-fr0m-tty";

/// A loopback endpoint that records the `x-key` header of the one request it
/// serves.
fn spawn_key_recorder() -> (u16, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let seen = slot.clone();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut reader = BufReader::new(stream);
            let mut start = String::new();
            if reader.read_line(&mut start).is_err() || start.is_empty() {
                continue;
            }
            let mut len = 0usize;
            let mut key = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    break;
                }
                if line.trim().is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                } else if lower.starts_with("x-key:")
                    && let Some((_, v)) = line.split_once(':')
                {
                    key = Some(v.trim().to_string());
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).ok();
            *seen.lock().unwrap() = key;
            let mut s = reader.into_inner();
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            );
            let _ = s.flush();
        }
    });
    (port, slot)
}

fn config(port: u16) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: pm\n\
         workflows:\n  - name: w\n    steps:\n\
         \x20     start: {{kind: once}}\n\
         \x20     call: {{kind: http, depends_on: [start], method: POST, url: \"http://127.0.0.1:{port}/k\", allow_private: true, headers: {{x-key: \"{{{{secret:{SECRET_NAME}}}}}\"}}}}\n\
         \x20     done: {{kind: finish, depends_on: [call], status: completed}}\n\
         lifecycle:\n  run_until: idle\n  idle_grace: 1s\n\
         observability:\n  log_level: info\n"
    )
}

#[test]
fn without_the_flag_a_missing_secret_is_refused_at_admission() {
    let cfg = common::unique_path("agentd-pm-refuse", "yaml");
    std::fs::write(&cfg, config(1)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env_remove(SECRET_NAME)
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "fail-closed:\n{stderr}");
    assert!(
        stderr.contains(SECRET_NAME),
        "the refusal names the secret:\n{stderr}"
    );
    let _ = std::fs::remove_file(&cfg);
}

#[test]
fn with_the_flag_the_daemon_prompts_on_the_tty_echo_off_and_uses_the_value() {
    let (port, seen) = spawn_key_recorder();
    let cfg = common::unique_path("agentd-pm-prompt", "yaml");
    std::fs::write(&cfg, config(port)).unwrap();

    // A real pty: the daemon's `/dev/tty` must answer, and must not echo.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master >= 0, "posix_openpt failed");
    unsafe {
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
    }
    let slave_path = unsafe {
        let p = libc::ptsname(master);
        assert!(!p.is_null(), "ptsname failed");
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    };
    // Non-blocking master: if the prompt never comes, the deadline fails the
    // test instead of a read() parking it forever.
    unsafe {
        let fl = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }

    let err_path = common::unique_path("agentd-pm-prompt", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(["--config", &cfg, "--prompt-missing"])
        .env_remove(SECRET_NAME)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf));
    let slave_c = std::ffi::CString::new(slave_path).unwrap();
    unsafe {
        cmd.pre_exec(move || {
            // New session, then open the pty slave: the first tty a session
            // leader opens becomes its controlling terminal, which is exactly
            // what `/dev/tty` resolves to.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = libc::open(slave_c.as_ptr(), libc::O_RDWR);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn agentd on a pty");

    // Read the prompt off the master; answer it; keep collecting output.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut tty_out = String::new();
    let mut answered = false;
    let mut buf = [0u8; 512];
    loop {
        assert!(
            Instant::now() < deadline,
            "no prompt appeared on the tty; saw: {tty_out:?}; stderr:\n{}",
            std::fs::read_to_string(&err_path).unwrap_or_default()
        );
        let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            tty_out.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !answered && tty_out.contains(SECRET_NAME) {
            let line = format!("{SECRET_VALUE}\n");
            let w = unsafe { libc::write(master, line.as_ptr().cast(), line.len()) };
            assert!(w > 0, "writing the answer to the pty failed");
            answered = true;
        }
        if answered && matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        if answered && Instant::now() >= deadline {
            break;
        }
    }
    let status = child.wait().expect("wait agentd");
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    unsafe {
        libc::close(master);
    }
    assert_eq!(status.code(), Some(0), "the run completed:\n{stderr}");
    // The prompt asked for THIS secret…
    assert!(tty_out.contains(SECRET_NAME), "prompt text: {tty_out:?}");
    // …did not echo the typed value back (termios ECHO off)…
    assert!(
        !tty_out.contains(SECRET_VALUE),
        "the secret was echoed to the terminal: {tty_out:?}"
    );
    // …and the value reached the point of use: the outbound request header.
    assert_eq!(
        seen.lock().unwrap().as_deref(),
        Some(SECRET_VALUE),
        "the prompted value was used by the workflow:\n{stderr}"
    );
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&err_path);
}
