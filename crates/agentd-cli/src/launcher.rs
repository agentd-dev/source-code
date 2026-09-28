// SPDX-License-Identifier: AGPL-3.0-only
//! The **`agentd tui` / `agentd ui` launcher**: run the daemon and one display
//! client as one command, and hand the client nothing but its endpoint.
//!
//! The daemon runs exactly as `agentd <args>` would — the launcher adds no
//! config key, flag or variable, so what an operator sees under the launcher is
//! what they get without it. The client (`agentd-tui` / `agentd-ui`, or the
//! binary `AGENTD_TUI_BIN` / `AGENTD_UI_BIN` names) gets the argv
//! [`LAUNCH_CONTRACT`](agentd::runtime::surface::launch::LAUNCH_CONTRACT) lists and the launcher's environment minus every
//! variable the config loader reads and every `{{secret:NAME}}` the loaded
//! settings reference. It never gets `a2a.bearer`: a display client that holds
//! the daemon's root credential is one more place to steal it from, and the
//! web UI used to serve it to any page that asked.
//!
//! Lifetimes are tied: the client exiting drains the daemon (SIGTERM to self);
//! the daemon exiting sends the client SIGTERM, then SIGKILL.
//!
//! Terminal ownership: an interactive TUI and a JSON-lines-logging daemon
//! cannot share a tty. The original stdio fds are saved for the client and the
//! daemon's stdout/stderr go to `--daemon-log`.
//!
//! Descriptor hygiene: every fd the launcher creates is close-on-exec in this
//! process, because the daemon spawns processes of its own — the exec tool,
//! instances, subagents — and none of them may inherit the operator's
//! terminal or the UI's socket. The one fd a client is meant to have reaches
//! descriptor 3 only in that client, between fork and exec.

use agentd::config::v2::{self, Ask};
use agentd::exit;
use agentd::runtime::surface::launch::{
    DEFAULT_UI_PORT, LAUNCH_FD, LAUNCHER_DOCS, LaunchClient, REMOVED_LAUNCHER_FLAGS, launch_client,
    launch_endpoint,
};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the watcher waits for the A2A listener to become connectable
/// before giving up and draining the daemon.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a client gets to exit on SIGTERM before it is killed.
const CLIENT_GRACE: Duration = Duration::from_secs(3);

/// The launcher's own flags, split from the daemon's.
#[derive(Debug, Clone, PartialEq)]
struct Options {
    daemon_log: Option<String>,
    /// `agentd ui` only: the loopback port the web UI is served on (0 = any).
    port: u16,
    /// `agentd ui` only: open the page in a browser.
    open: bool,
}

/// Split the launcher's flags from the daemon's, refusing a removed one by
/// name. Everything that is not the launcher's is passed on untouched, in
/// order — the daemon loads exactly the arguments it would have been given.
fn split_args(sub: &str, args: &[String]) -> Result<(Options, Vec<String>), String> {
    let mut opts = Options {
        daemon_log: None,
        port: DEFAULT_UI_PORT,
        open: true,
    };
    let mut daemon = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some((flag, hint)) = REMOVED_LAUNCHER_FLAGS.iter().find(|(f, _)| f == a) {
            return Err(format!(
                "agentd {sub} {flag} was removed in agentd {}: {hint}",
                v2::KEYS_REMOVED_IN
            ));
        }
        match a.as_str() {
            "--daemon-log" => {
                let path = it
                    .next()
                    .ok_or_else(|| format!("agentd {sub}: --daemon-log needs a path"))?;
                opts.daemon_log = Some(path.clone());
            }
            "--port" if sub == "ui" => {
                opts.port = it
                    .next()
                    .and_then(|p| p.parse().ok())
                    .ok_or_else(|| format!("agentd {sub}: --port needs a port number"))?;
            }
            "--no-open" if sub == "ui" => opts.open = false,
            other => daemon.push(other.to_string()),
        }
    }
    Ok((opts, daemon))
}

/// Run `agentd <tui|ui> …`.
pub fn run(sub: &str, args: &[String], env: &[(String, String)]) -> i32 {
    let Some(client) = launch_client(sub) else {
        eprintln!("agentd {sub}: not a launcher subcommand");
        return exit::USAGE;
    };
    // Before any configuration is read: a stale flag is the operator's to fix,
    // and loading a config first would bury that under whatever it says.
    let (opts, daemon_args) = match split_args(sub, args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return exit::USAGE;
        }
    };
    let inv = match crate::load_invocation(&daemon_args, env) {
        Ok(inv) => inv,
        Err(code) => return code,
    };
    if inv.ask != Ask::Run {
        eprintln!(
            "agentd {sub}: combine with a runnable configuration (asks like --help/--validate-config run without the subcommand)"
        );
        return exit::USAGE;
    }

    // What the client can use, decided before anything is spawned.
    let endpoint = match launch_endpoint(&inv.loaded.settings) {
        Ok(ep) => ep,
        Err(cause) => {
            eprintln!(
                "agentd {sub}: {cause}\n  hint: start the daemon with `agentd -c …` and run `agentd-{sub} --endpoint <url>` against it"
            );
            return exit::USAGE;
        }
    };
    let child_env = client_env(&inv.loaded, &inv.env);
    // `agentd ui` binds the web UI's socket itself, on loopback, so no other
    // local process can hold the port and be handed whatever the browser opens
    // there. std opens it close-on-exec.
    let ui_listener = if sub == "ui" {
        match std::net::TcpListener::bind(("127.0.0.1", opts.port)) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("agentd {sub}: cannot bind 127.0.0.1:{}: {e}", opts.port);
                return exit::USAGE;
            }
        }
    } else {
        None
    };
    let ui_url = ui_listener
        .as_ref()
        .and_then(|l| l.local_addr().ok())
        .map(|a| format!("http://127.0.0.1:{}/", a.port()));

    let log_path = opts
        .daemon_log
        .clone()
        .unwrap_or_else(|| default_log_path(sub, env));
    match &ui_url {
        Some(url) => {
            eprintln!("agentd {sub}: endpoint {endpoint} · web UI {url} · daemon logs → {log_path}")
        }
        None => eprintln!("agentd {sub}: endpoint {endpoint} · daemon logs → {log_path}"),
    }
    let tty = match redirect_daemon_output(&log_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("agentd {sub}: cannot open the daemon log: {e}");
            return exit::USAGE;
        }
    };

    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));
    let watcher = spawn_watcher(Watch {
        client: *client,
        endpoint,
        env: child_env,
        ui_listener,
        open_url: ui_url.filter(|_| opts.open),
        tty,
        child_slot: Arc::clone(&child_slot),
        done: Arc::clone(&done),
        failed: Arc::clone(&failed),
    });

    agentd::state::record_config_digest(&inv.loaded.settings);
    let code = agentd::runtime::run(&inv.loaded, &inv.args, &inv.env);

    // The daemon is down: reap the client (graceful first) and let the
    // watcher wind down.
    done.store(true, Ordering::Relaxed);
    if let Some(mut child) = child_slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + CLIENT_GRACE;
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
    // A client that never started is a failed launch, however cleanly the
    // daemon then drained.
    if failed.load(Ordering::Relaxed) && code == exit::SUCCESS {
        return exit::GENERIC;
    }
    code
}

/// The client's environment: the launcher's own, minus every variable the
/// config loader reads, every `{{secret:NAME}}` the loaded settings name, and
/// `AGENTD_BEARER`. Nothing is added.
///
/// The promise is narrow on purpose — never `a2a.bearer`, never a resolved
/// config secret — because that is what the scrub can keep: credentials read
/// by code other than the loader (the AWS chain) pass through, and a same-uid
/// process can read `/proc/<ppid>/environ` anyway.
fn client_env(loaded: &v2::Loaded, env: &[(String, String)]) -> Vec<(String, String)> {
    let mut scrub: std::collections::HashSet<String> =
        v2::consumed_env_names(env).into_iter().collect();
    scrub.extend(agentd::sec::secret::secret_env_names(&loaded.doc));
    // The name the pre-1.17 launcher handed the bearer over in: a client that
    // still reads it must find nothing there.
    scrub.insert("AGENTD_BEARER".to_string());
    env.iter()
        .filter(|(k, _)| !scrub.contains(k))
        .cloned()
        .collect()
}

/// `$XDG_RUNTIME_DIR/agentd-<sub>-<pid>.log`, else the same name in `$TMPDIR`.
/// The runtime dir is the user's own (0700); the temp dir is shared, which is
/// why the log is opened so that nothing already there can be written through.
fn default_log_path(sub: &str, env: &[(String, String)]) -> String {
    let dir = env
        .iter()
        .find(|(k, v)| k == "XDG_RUNTIME_DIR" && !v.is_empty())
        .map(|(_, v)| std::path::PathBuf::from(v))
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("agentd-{sub}-{}.log", std::process::id()))
        .display()
        .to_string()
}

/// The saved terminal fds (stdin/stdout/stderr as they were at startup).
struct Tty {
    stdin: OwnedFd,
    stdout: OwnedFd,
    stderr: OwnedFd,
}

/// Save the terminal for the client, then point the daemon's fd 1/2 at a
/// FRESH log file.
///
/// The saved fds are close-on-exec duplicates (`F_DUPFD_CLOEXEC`), so only the
/// client — which gets them as its stdio — inherits the terminal; a process
/// the daemon spawns does not. The log is created `O_EXCL | O_NOFOLLOW`, mode
/// 0600: in a shared temp dir, a file or symlink planted at the predictable
/// name would otherwise receive the daemon's log, or have it written through
/// to wherever the link points.
fn redirect_daemon_output(log_path: &str) -> Result<Tty, String> {
    let dup = |fd: i32| -> Result<OwnedFd, String> {
        let d = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if d < 0 {
            return Err(format!(
                "saving fd {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(d) })
    };
    let tty = Tty {
        stdin: dup(0)?,
        stdout: dup(1)?,
        stderr: dup(2)?,
    };
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(log_path)
        .map_err(|e| format!("{log_path}: {e} (the log must be a new file)"))?;
    for target in [1, 2] {
        if unsafe { libc::dup2(file.as_raw_fd(), target) } < 0 {
            return Err(format!("dup2 onto fd {target} failed"));
        }
    }
    Ok(tty)
}

/// What the watcher thread needs.
struct Watch {
    client: LaunchClient,
    endpoint: String,
    env: Vec<(String, String)>,
    ui_listener: Option<std::net::TcpListener>,
    open_url: Option<String>,
    tty: Tty,
    child_slot: Arc<Mutex<Option<Child>>>,
    done: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

/// The authority to probe for readiness: the endpoint's, with the scheme's
/// default port when it names none.
fn dial_authority(endpoint: &str) -> String {
    let (scheme, rest) = endpoint.split_once("://").unwrap_or(("http", endpoint));
    let rest = rest.trim_end_matches('/');
    let has_port = match rest.rsplit_once(':') {
        Some((host, _)) => !host.is_empty() && (!host.starts_with('[') || host.ends_with(']')),
        None => false,
    };
    if has_port {
        rest.to_string()
    } else {
        format!("{rest}:{}", if scheme == "https" { 443 } else { 80 })
    }
}

/// The client's command: its binary, exactly the contract's argv, the
/// scrubbed environment, and its stdio.
fn client_command(w: &Watch) -> Result<(Command, String), String> {
    let bin = std::env::var(w.client.bin_env).unwrap_or_else(|_| w.client.bin.to_string());
    let mut cmd = Command::new(&bin);
    cmd.env_clear().envs(w.env.iter().map(|(k, v)| (k, v)));
    let clone = |fd: &OwnedFd| fd.try_clone().map_err(|e| format!("terminal fd: {e}"));
    for flag in w.client.argv {
        cmd.arg(flag);
        match *flag {
            agentd::runtime::surface::launch::ENDPOINT_FLAG => {
                cmd.arg(&w.endpoint);
            }
            agentd::runtime::surface::launch::LISTEN_FD_FLAG => {
                cmd.arg(LAUNCH_FD.to_string());
                let listener = w
                    .ui_listener
                    .as_ref()
                    .ok_or("the web UI's listener was never bound")?;
                inherit_as_launch_fd(&mut cmd, listener.as_raw_fd());
            }
            other => {
                return Err(format!(
                    "the launch contract names {other}, which this launcher cannot supply"
                ));
            }
        }
    }
    // The web UI reads no terminal input; the launcher keeps the terminal.
    let stdin = if w.ui_listener.is_some() {
        Stdio::null()
    } else {
        Stdio::from(clone(&w.tty.stdin)?)
    };
    cmd.stdin(stdin)
        .stdout(Stdio::from(clone(&w.tty.stdout)?))
        .stderr(Stdio::from(clone(&w.tty.stderr)?));
    Ok((cmd, bin))
}

/// Deliver `src` to the client as descriptor [`LAUNCH_FD`], and only to it.
///
/// `src` is close-on-exec here, so no other child of this process can inherit
/// it. In the client, between fork and exec, `dup2` puts a copy at 3 without
/// the flag. When `src` already IS 3, `dup2` onto itself is a no-op that
/// leaves close-on-exec set, so the flag is cleared explicitly instead.
fn inherit_as_launch_fd(cmd: &mut Command, src: i32) {
    unsafe {
        cmd.pre_exec(move || {
            let rc = if src == LAUNCH_FD {
                libc::fcntl(LAUNCH_FD, libc::F_SETFD, 0)
            } else {
                libc::dup2(src, LAUNCH_FD)
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Wait for the listener, spawn the display client on the saved terminal, and
/// SIGTERM the daemon (graceful drain) when the client exits.
fn spawn_watcher(mut w: Watch) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("launcher-client".into())
        .spawn(move || {
            let sub = w.client.sub;
            let give_up = |w: &Watch, msg: &str| {
                let _ = tty_println(&w.tty, msg);
                w.failed.store(true, Ordering::Relaxed);
                unsafe { libc::raise(libc::SIGTERM) };
            };
            // 1. Wait until the A2A listener accepts connections.
            let authority = dial_authority(&w.endpoint);
            let deadline = Instant::now() + READY_TIMEOUT;
            loop {
                if w.done.load(Ordering::Relaxed) {
                    return;
                }
                if std::net::TcpStream::connect(&authority).is_ok() {
                    break;
                }
                if Instant::now() >= deadline {
                    give_up(
                        &w,
                        &format!("agentd {sub}: the A2A listener never became reachable; shutting down"),
                    );
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // 2. Spawn the client.
            let spawned = client_command(&w).and_then(|(mut cmd, bin)| {
                cmd.spawn().map_err(|e| {
                    format!(
                        "agentd {sub}: cannot start {bin:?}: {e}\n  set {} to the display client's path — see {LAUNCHER_DOCS}",
                        w.client.bin_env
                    )
                })
            });
            // The client holds its copy now; this process never needs the
            // UI's socket again, and a later child must not inherit it.
            w.ui_listener = None;
            let child = match spawned {
                Ok(c) => c,
                Err(msg) => {
                    give_up(&w, &msg);
                    return;
                }
            };
            let pid = child.id();
            *w.child_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
            if let Some(url) = &w.open_url {
                open_browser(url, &w.env);
            }
            // 3. Wait for the client to exit, then drain the daemon.
            loop {
                if w.done.load(Ordering::Relaxed) {
                    return; // the daemon beat us to it; main reaps the child
                }
                let exited = w
                    .child_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                    .map(|c| matches!(c.try_wait(), Ok(Some(_))))
                    .unwrap_or(true);
                if exited {
                    let _ = tty_println(
                        &w.tty,
                        &format!("agentd {sub}: client (pid {pid}) exited; draining the daemon"),
                    );
                    unsafe { libc::raise(libc::SIGTERM) };
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
        .expect("spawn launcher watcher")
}

/// Open `url` in the desktop's browser. Best effort: a headless host has no
/// opener, and the URL is already on the terminal.
fn open_browser(url: &str, env: &[(String, String)]) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(opener)
        .arg(url)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut c| {
            // Reaped on its own thread so an opener that lingers never
            // becomes a zombie of the launcher.
            std::thread::spawn(move || c.wait());
        });
}

/// Print a line to the SAVED terminal (the daemon's own stderr is redirected).
fn tty_println(tty: &Tty, msg: &str) -> std::io::Result<()> {
    use std::io::Write;
    // Through a close-on-exec duplicate, so the saved fd stays open.
    let mut f = std::fs::File::from(tty.stderr.try_clone()?);
    writeln!(f, "{msg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The daemon loads exactly what it was given: the launcher takes its own
    /// flags out and adds nothing — the `--interface.enabled` it used to push
    /// is the kind of line this pins.
    #[test]
    fn the_daemon_args_are_the_ones_given_minus_the_launchers() {
        let given = args(&[
            "--config",
            "a.yaml",
            "--daemon-log",
            "/tmp/x.log",
            "--port",
            "0",
            "--no-open",
            "--a2a.events.enabled",
            "true",
        ]);
        let (opts, daemon) = split_args("ui", &given).unwrap();
        assert_eq!(
            daemon,
            args(&["--config", "a.yaml", "--a2a.events.enabled", "true"])
        );
        assert_eq!(opts.daemon_log.as_deref(), Some("/tmp/x.log"));
        assert_eq!((opts.port, opts.open), (0, false));
        // `--port` and `--no-open` are `ui`'s; under `tui` they are the
        // daemon's to accept or refuse.
        let (_, daemon) = split_args("tui", &args(&["--no-open", "--config", "a.yaml"])).unwrap();
        assert_eq!(daemon, args(&["--no-open", "--config", "a.yaml"]));
    }

    #[test]
    fn a_removed_launcher_flag_is_refused_by_name() {
        for (flag, hint) in REMOVED_LAUNCHER_FLAGS {
            for sub in ["tui", "ui"] {
                let e = split_args(sub, &args(&["--config", "a.yaml", flag])).unwrap_err();
                assert!(
                    e.contains(&format!("agentd {sub} {flag} was removed in agentd 1.17.0"))
                        && e.contains(hint),
                    "{e}"
                );
            }
        }
    }

    #[test]
    fn every_contract_client_is_launchable() {
        for c in agentd::runtime::surface::launch::LAUNCH_CONTRACT {
            assert_eq!(launch_client(c.sub), Some(c));
        }
        assert_eq!(dial_authority("http://127.0.0.1:8420"), "127.0.0.1:8420");
        assert_eq!(dial_authority("https://localhost"), "localhost:443");
        assert_eq!(dial_authority("http://[::1]:9/"), "[::1]:9");
        assert_eq!(dial_authority("https://[::1]"), "[::1]:443");
    }
}
