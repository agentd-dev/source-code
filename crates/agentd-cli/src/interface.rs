// SPDX-License-Identifier: AGPL-3.0-only
//! The **`agentd tui` / `agentd ui` passthrough** (RFC 0032 §8): run the daemon
//! AND its display client as one command. The daemon runs exactly as `agentd
//! -c …` would — plus a forced `interface.enabled` (the subcommand IS the
//! opt-in) — while a child process (`agentd-tui` / `agentd-ui`, separate
//! Node projects) gets the real terminal and speaks A2A to the loopback
//! listener. Lifetimes are tied: the client exiting drains the daemon
//! (SIGTERM to self); the daemon exiting kills the client.
//!
//! Terminal ownership: an interactive TUI and a JSON-lines-logging daemon
//! cannot share a tty. The original stdio fds are saved for the child and the
//! daemon's stdout/stderr are redirected to a log file (path printed before
//! the switch, `AGENTD_INTERFACE_LOG` overrides).

#![cfg(unix)]

use agentd::config::v2::{self, Ask};
use agentd::exit;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the watcher waits for the A2A listener to become connectable
/// before giving up and draining the daemon.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Run `agentd <tui|ui> …`: load the config with the interface forced on, run
/// the daemon on this thread, and drive the display client alongside it.
pub fn run(sub: &str, args: &[String], env: &[(String, String)]) -> i32 {
    // Split the passthrough's own flags from the daemon's.
    let mut daemon_args: Vec<String> = Vec::new();
    let mut debug = false;
    let mut open = true;
    for a in args {
        match a.as_str() {
            "--debug" => debug = true,
            "--no-open" => open = false,
            other => daemon_args.push(other.to_string()),
        }
    }
    // The subcommand IS the interface opt-in; flags survive a SIGHUP reload
    // (which re-reads argv), unlike an in-memory settings mutation.
    daemon_args.push("--interface.enabled".into());
    daemon_args.push("true".into());
    if debug {
        daemon_args.push("--interface.debug".into());
        daemon_args.push("true".into());
    }

    let (loaded, ask) = match v2::load(&daemon_args, env) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e:?}");
            return exit::USAGE;
        }
    };
    if ask != Ask::Run {
        eprintln!(
            "agentd {sub}: combine with a runnable configuration (asks like --help/--validate-config run without the subcommand)"
        );
        return exit::USAGE;
    }

    // The client dials the daemon's own A2A listener.
    let endpoint = match client_endpoint(&loaded.settings) {
        Ok(ep) => ep,
        Err(e) => {
            eprintln!(
                "agentd {sub}: {e}\n  hint: add to the config:\n    a2a:\n      listen: http://127.0.0.1:8420"
            );
            return exit::USAGE;
        }
    };
    // A configured server bearer must be handed to the client (the listener
    // requires it even from loopback once set).
    let envmap = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let bearer = match &loaded.settings.a2a.bearer {
        Some(b) => match agentd::sec::secret::resolve(&b.0, &envmap) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("agentd {sub}: a2a.bearer: {e}");
                return exit::USAGE;
            }
        },
        None => None,
    };

    // Save the real terminal for the child, then point the daemon's own
    // stdout/stderr at a log file.
    let log_path = std::env::var("AGENTD_INTERFACE_LOG").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join(format!("agentd-{sub}-{}.log", std::process::id()))
            .display()
            .to_string()
    });
    eprintln!("agentd {sub}: endpoint {endpoint} · daemon logs → {log_path}");
    let tty = match redirect_daemon_output(&log_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("agentd {sub}: cannot redirect daemon output: {e}");
            return exit::USAGE;
        }
    };

    // The watcher: wait for the listener, spawn the client on the saved tty,
    // and drain the daemon when the client exits.
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let watcher = spawn_watcher(
        sub.to_string(),
        endpoint,
        bearer,
        debug,
        open,
        tty,
        Arc::clone(&child_slot),
        Arc::clone(&done),
    );

    let code = agentd::runtime::run(&loaded, &daemon_args, env);

    // The daemon is down: reap the client (graceful first) and let the
    // watcher wind down.
    done.store(true, Ordering::Relaxed);
    if let Some(mut child) = child_slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = watcher.join();
    code
}

/// The endpoint a local client dials, derived from `a2a.listen`: same scheme
/// and port, with a wildcard bind host rewritten to loopback.
fn client_endpoint(s: &v2::Settings) -> Result<String, String> {
    let listen = s
        .a2a
        .listen
        .as_deref()
        .ok_or("the interface needs an A2A listener (a2a.listen is not set)")?;
    let agentd::config::ServeTarget::Http { bind, tls } =
        agentd::config::ServeTarget::parse(listen).map_err(|e| format!("a2a.listen: {e:?}"))?;
    let (host, port) = split_authority(&bind);
    if port == "0" {
        return Err("a2a.listen uses an ephemeral port (:0); the client needs a fixed one".into());
    }
    let host = match host {
        "0.0.0.0" | "::" | "[::]" | "" => "127.0.0.1",
        h => h,
    };
    let scheme = if tls { "https" } else { "http" };
    Ok(if host.contains(':') && !host.starts_with('[') {
        format!("{scheme}://[{host}]:{port}")
    } else {
        format!("{scheme}://{host}:{port}")
    })
}

/// Split `host:port` (bracketed IPv6 kept intact).
fn split_authority(bind: &str) -> (&str, &str) {
    if let Some(rest) = bind.strip_prefix('[')
        && let Some((h, p)) = rest.split_once(']')
    {
        return (h, p.strip_prefix(':').unwrap_or(""));
    }
    match bind.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => (bind, ""),
    }
}

/// The saved terminal fds (stdin/stdout/stderr as they were at startup).
struct Tty {
    stdin: OwnedFd,
    stdout: OwnedFd,
    stderr: OwnedFd,
}

/// Duplicate the terminal fds, then point the daemon's fd 1/2 at `log_path`
/// (append). The returned [`Tty`] hands the real terminal to the client child.
fn redirect_daemon_output(log_path: &str) -> Result<Tty, String> {
    let dup = |fd: i32| -> Result<OwnedFd, String> {
        let d = unsafe { libc::dup(fd) };
        if d < 0 {
            return Err(format!("dup({fd}) failed"));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(d) })
    };
    let tty = Tty {
        stdin: dup(0)?,
        stdout: dup(1)?,
        stderr: dup(2)?,
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| format!("{log_path}: {e}"))?;
    use std::os::fd::AsRawFd;
    for target in [1, 2] {
        if unsafe { libc::dup2(file.as_raw_fd(), target) } < 0 {
            return Err(format!("dup2 onto fd {target} failed"));
        }
    }
    Ok(tty)
}

/// Wait for the listener, spawn the display client on the saved tty, and
/// SIGTERM the daemon (graceful drain) when the client exits.
#[allow(clippy::too_many_arguments)]
fn spawn_watcher(
    sub: String,
    endpoint: String,
    bearer: Option<String>,
    debug: bool,
    open: bool,
    tty: Tty,
    child_slot: Arc<Mutex<Option<Child>>>,
    done: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("interface-client".into())
        .spawn(move || {
            // 1. Wait until the A2A listener accepts connections.
            let authority = endpoint
                .split_once("://")
                .map(|(_, a)| a.to_string())
                .unwrap_or_else(|| endpoint.clone());
            let deadline = Instant::now() + READY_TIMEOUT;
            loop {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                if std::net::TcpStream::connect(&authority).is_ok() {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = tty_println(
                        &tty,
                        &format!("agentd {sub}: the A2A listener never became reachable; shutting down"),
                    );
                    unsafe { libc::raise(libc::SIGTERM) };
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // 2. Spawn the client on the real terminal.
            let bin_env = if sub == "tui" { "AGENTD_TUI_BIN" } else { "AGENTD_UI_BIN" };
            let bin = std::env::var(bin_env).unwrap_or_else(|_| format!("agentd-{sub}"));
            let mut cmd = Command::new(&bin);
            cmd.arg("--endpoint")
                .arg(&endpoint)
                .env("AGENTD_ENDPOINT", &endpoint)
                .stdin(Stdio::from(tty.stdin.try_clone().expect("clone tty")))
                .stdout(Stdio::from(tty.stdout.try_clone().expect("clone tty")))
                .stderr(Stdio::from(tty.stderr.try_clone().expect("clone tty")));
            if let Some(b) = &bearer {
                cmd.env("AGENTD_BEARER", b);
            }
            if debug {
                cmd.arg("--debug");
            }
            if sub == "ui" && open {
                cmd.arg("--open");
            }
            let child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tty_println(
                        &tty,
                        &format!(
                            "agentd {sub}: cannot start {bin:?}: {e}\n  install it: npm install -g @agentd/{sub}  (or set {bin_env})"
                        ),
                    );
                    unsafe { libc::raise(libc::SIGTERM) };
                    return;
                }
            };
            let pid = child.id();
            *child_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
            // 3. Wait for the client to exit, then drain the daemon.
            loop {
                if done.load(Ordering::Relaxed) {
                    return; // the daemon beat us to it; main reaps the child
                }
                let exited = child_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                    .map(|c| matches!(c.try_wait(), Ok(Some(_))))
                    .unwrap_or(true);
                if exited {
                    let _ = tty_println(
                        &tty,
                        &format!("agentd {sub}: client (pid {pid}) exited; draining the daemon"),
                    );
                    unsafe { libc::raise(libc::SIGTERM) };
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
        .expect("spawn interface watcher")
}

/// Print a line to the SAVED terminal (the daemon's own stderr is redirected).
fn tty_println(tty: &Tty, msg: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    // Write via a borrowed duplicate so the saved fd stays open.
    let dup = unsafe { libc::dup(tty.stderr.as_raw_fd()) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut f = unsafe { std::fs::File::from_raw_fd(dup) };
    writeln!(f, "{msg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(yaml_listen: Option<&str>) -> v2::Settings {
        let doc = match yaml_listen {
            Some(l) => serde_json::json!({"a2a": {"listen": l}}),
            None => serde_json::json!({}),
        };
        serde_json::from_value(doc).unwrap()
    }

    #[test]
    fn client_endpoint_rewrites_wildcards_and_refuses_ephemeral() {
        assert_eq!(
            client_endpoint(&settings(Some("http://127.0.0.1:8420"))).unwrap(),
            "http://127.0.0.1:8420"
        );
        assert_eq!(
            client_endpoint(&settings(Some("https://0.0.0.0:9443"))).unwrap(),
            "https://127.0.0.1:9443"
        );
        // A wildcard IPv6 bind (https — plaintext non-loopback is refused by
        // ServeTarget itself) also lands on loopback.
        assert_eq!(
            client_endpoint(&settings(Some("https://[::]:8080"))).unwrap(),
            "https://127.0.0.1:8080"
        );
        // `:0` never reaches the ephemeral guard — ServeTarget::parse already
        // refuses it (a listener needs an explicit port); either way: an error.
        assert!(client_endpoint(&settings(Some("http://127.0.0.1:0"))).is_err());
        assert!(
            client_endpoint(&settings(None))
                .unwrap_err()
                .contains("a2a.listen")
        );
    }

    #[test]
    fn authority_split_handles_ipv6() {
        assert_eq!(split_authority("127.0.0.1:80"), ("127.0.0.1", "80"));
        assert_eq!(split_authority("[::1]:9000"), ("::1", "9000"));
    }
}
