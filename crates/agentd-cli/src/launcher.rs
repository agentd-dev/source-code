// SPDX-License-Identifier: AGPL-3.0-only
//! The **`agentd tui` / `agentd ui` launcher**: run the daemon and one display
//! client as one command, and sign the client in without handing it a
//! credential the daemon was configured with.
//!
//! The daemon runs exactly as `agentd <args>` would — the launcher adds no
//! config key, flag or variable, so what an operator sees under the launcher is
//! what they get without it. The client (`agentd-tui` / `agentd-ui`, or the
//! binary `AGENTD_TUI_BIN` / `AGENTD_UI_BIN` names) gets the argv
//! [`LAUNCH_CONTRACT`](agentd::runtime::surface::launch::LAUNCH_CONTRACT) lists and the launcher's environment minus every
//! variable the config loader reads and every `{{secret:NAME}}` the loaded
//! settings reference. It never gets `a2a.bearer`: a display client that holds
//! the daemon's root credential is one more place to steal it from.
//!
//! What it gets instead is a **single-use launch code** from the
//! [`LaunchSlot`] the launcher installs in the daemon's own process — the one
//! place a code can be minted. The TUI reads it from an inherited pipe. The
//! web UI's travels only in a URL fragment, which a browser never sends over
//! HTTP: in a 0600 launch file handed to the desktop's opener, or printed on
//! the launcher's terminal. A browser that can be given neither (a sandboxed
//! one, an SSH forward, a second tab) asks the daemon for a sign-in and shows
//! a short code, which the person types here: the launcher's terminal is the
//! trust anchor for every sign-in after the first.
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
//! terminal, the pipe holding a launch code or the UI's socket. The one fd a
//! client is meant to have reaches descriptor 3 only in that client, between
//! fork and exec. What the launcher inherited itself is marked close-on-exec
//! by `main` before any role runs ([`agentd::signals::cloexec_inherited_fds`]),
//! so a stray descriptor from whatever started it reaches no child either.
//!
//! Child processes: the daemon's reaper collects every exited child of this
//! process (`waitpid(-1)`), the launcher's own included, so the client and the
//! opener are registered with it and their exit is read from whichever side
//! reaped them first.

use agentd::a2a::oauth::{LaunchBind, LaunchSlot};
use agentd::config::settings::{self, Ask};
use agentd::exit;
use agentd::runtime::surface::launch::{
    DEFAULT_UI_PORT, LAUNCH_CODE_TTL, LAUNCH_FD, LAUNCH_FD_FLAG, LAUNCHER_DOCS, LaunchClient,
    launch_client, launch_endpoint,
};
use agentd::supervisor::{reap, reaper};
use std::io::{BufRead, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the watcher waits for the A2A listener to become connectable
/// before giving up and draining the daemon.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a client gets to exit on SIGTERM before it is killed.
const CLIENT_GRACE: Duration = Duration::from_secs(3);
/// The terminal prompt waits this long for a burst of sign-in requests to
/// end, so a burst is one prompt that counts them…
const PROMPT_COALESCE: Duration = Duration::from_millis(250);
/// …but never longer than this, so a steady stream cannot hold the prompt
/// back.
const PROMPT_COALESCE_MAX: Duration = Duration::from_secs(1);
/// What the launcher's terminal says when a browser tab asks to be signed in.
const PROMPT: &str =
    "A browser tab asks to sign in to agentd: type the code it shows (Enter to skip)";

/// The launcher's own flags, split from the daemon's.
#[derive(Debug, Clone, PartialEq)]
struct Options {
    daemon_log: Option<String>,
    /// `agentd ui` only: the loopback port the web UI is served on (0 = any).
    port: u16,
    /// `agentd ui` only: open the page in a browser.
    open: bool,
}

/// Split the launcher's flags from the daemon's. Everything that is not the
/// launcher's is passed on untouched, in order — the daemon loads exactly the
/// arguments it would have been given, and refuses one it does not know the
/// way it would without the launcher.
fn split_args(sub: &str, args: &[String]) -> Result<(Options, Vec<String>), String> {
    let mut opts = Options {
        daemon_log: None,
        port: DEFAULT_UI_PORT,
        open: true,
    };
    let mut daemon = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
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
    // Before any configuration is read: a launcher flag missing its value is
    // the operator's to fix, and loading a config first would bury that under
    // whatever it says.
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
    // The page's origin is exactly the socket bound above: the one origin the
    // slot binds a code to and the listener's CORS list gains.
    let ui_origin = ui_listener
        .as_ref()
        .and_then(|l| l.local_addr().ok())
        .map(|a| format!("http://127.0.0.1:{}", a.port()));
    let slot = match LaunchSlot::new(ui_origin.as_deref()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("agentd {sub}: {e}");
            return exit::USAGE;
        }
    };

    let log_path = opts
        .daemon_log
        .clone()
        .unwrap_or_else(|| default_log_path(sub, env));
    match &ui_origin {
        Some(o) => {
            eprintln!("agentd {sub}: endpoint {endpoint} · web UI {o}/ · daemon logs → {log_path}")
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

    let launch_file: Arc<Mutex<Option<LaunchFile>>> = Arc::new(Mutex::new(None));
    if ui_origin.is_some() {
        // The file is worth nothing once its code is spent, so it goes the
        // moment the daemon consumes the code — whoever presented it.
        let lf = Arc::clone(&launch_file);
        slot.on_consume(move || {
            remove_launch_file(&lf);
        });
        if let Err(e) = terminal_sign_in(&slot, &tty) {
            let _ = tty_println(
                &tty,
                &format!("agentd {sub}: the terminal cannot sign browser tabs in: {e}"),
            );
        }
    }

    let child_slot: Arc<Mutex<Option<Tracked>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));
    let watcher = spawn_watcher(Watch {
        client: *client,
        endpoint,
        env: child_env,
        ui_listener,
        ui_origin,
        open: opts.open,
        launch_dir: launch_dir_base(env),
        launch_file: Arc::clone(&launch_file),
        slot: Arc::clone(&slot),
        tty,
        child_slot: Arc::clone(&child_slot),
        done: Arc::clone(&done),
        failed: Arc::clone(&failed),
    });

    agentd::state::record_config_digest(&inv.loaded.settings);
    let code = agentd::runtime::run_with(
        &inv.loaded,
        &inv.args,
        &inv.env,
        agentd::runtime::RunOpts { launch: Some(slot) },
    );

    // The daemon is down: reap the client (graceful first) and let the
    // watcher wind down.
    done.store(true, Ordering::Relaxed);
    if let Some(mut child) = child_slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
        child.stop();
    }
    let _ = watcher.join();
    remove_launch_file(&launch_file);
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
fn client_env(loaded: &settings::Loaded, env: &[(String, String)]) -> Vec<(String, String)> {
    let mut scrub: std::collections::HashSet<String> =
        settings::consumed_env_names(env).into_iter().collect();
    scrub.extend(agentd::sec::secret::secret_env_names(&loaded.doc));
    // The TUI reads AGENTD_BEARER as a credential and refuses one beside
    // --launch-fd, so a value inherited from the operator's shell would stop
    // `agentd tui` before it signed in with its launch code.
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
    /// `agentd ui`: the origin the web UI is served at.
    ui_origin: Option<String>,
    /// `agentd ui`: hand the code to the desktop's opener (not `--no-open`).
    open: bool,
    /// Where the launch file's directory is made.
    launch_dir: Option<PathBuf>,
    launch_file: Arc<Mutex<Option<LaunchFile>>>,
    slot: Arc<LaunchSlot>,
    tty: Tty,
    child_slot: Arc<Mutex<Option<Tracked>>>,
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
/// scrubbed environment, and its stdio. `code_pipe` is the read end of the
/// pipe holding a TUI's launch code.
fn client_command(w: &Watch, code_pipe: Option<&OwnedFd>) -> Result<(Command, String), String> {
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
            LAUNCH_FD_FLAG => {
                cmd.arg(LAUNCH_FD.to_string());
                let pipe = code_pipe.ok_or("no launch code was minted for the client")?;
                inherit_as_launch_fd(&mut cmd, pipe.as_raw_fd());
            }
            other => {
                return Err(format!(
                    "the launch contract names {other}, which this launcher cannot supply"
                ));
            }
        }
    }
    // The web UI reads no terminal input; the launcher keeps the terminal
    // for the sign-in prompt.
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

/// A pipe for a launch code, both ends close-on-exec from the moment they
/// exist: the daemon may be spawning a child on another thread right now, and
/// a copy of either end in it would leak the code (the read end) or keep the
/// TUI from ever seeing end-of-file (the write end).
fn launch_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    // Darwin has no pipe2: the flag is set straight after, which leaves an
    // instant in which a concurrent fork could copy the ends.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let rc = unsafe {
        let rc = libc::pipe(fds.as_mut_ptr());
        if rc == 0 {
            libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        rc
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
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

/// Wait for the listener, spawn the display client on the saved terminal,
/// sign it in, and SIGTERM the daemon (graceful drain) when the client exits.
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
            // 2. A terminal client's code, on a pipe only it will hold. Minted
            // whatever the posture: a reload that adds principals later must
            // not strand a console that signed in as the implicit operator.
            let tui_code = if w.client.argv.contains(&LAUNCH_FD_FLAG) {
                let minted = w
                    .slot
                    .issue(LaunchBind::NoOrigin, w.client.client_id)
                    .and_then(|code| launch_pipe().map(|pipe| (code, pipe)));
                match minted {
                    Ok(m) => Some(m),
                    Err(e) => {
                        give_up(&w, &format!("agentd {sub}: cannot mint the launch code: {e}"));
                        return;
                    }
                }
            } else {
                None
            };
            // 3. Spawn the client.
            let spawned = client_command(&w, tui_code.as_ref().map(|(_, (read, _))| read))
                .and_then(|(mut cmd, bin)| {
                    Tracked::spawn(&mut cmd).map_err(|e| {
                        format!(
                            "agentd {sub}: cannot start {bin:?}: {e}\n  set {} to the display client's path — see {LAUNCHER_DOCS}",
                            w.client.bin_env
                        )
                    })
                });
            // The client holds its copies now; this process never needs the
            // UI's socket or the pipe's read end again, and a later child must
            // not inherit them.
            w.ui_listener = None;
            let tui_code = tui_code.map(|(code, (read, write))| {
                drop(read);
                (code, write)
            });
            let child = match spawned {
                Ok(c) => c,
                Err(msg) => {
                    give_up(&w, &msg);
                    return;
                }
            };
            let pid = child.id();
            *w.child_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
            // 4. Hand over the code. The TUI's: one line, then end-of-file,
            // which is what its read waits for. The web UI's: once the UI is
            // serving (its socket was bound before the spawn, so a browser's
            // connection waits in the backlog), in a URL fragment.
            if let Some((code, write)) = tui_code {
                let mut pipe = std::fs::File::from(write);
                if let Err(e) = pipe.write_all(format!("{code}\n").as_bytes()) {
                    let _ = tty_println(
                        &w.tty,
                        &format!("agentd {sub}: cannot hand the client its launch code: {e}"),
                    );
                }
            } else if let Some(origin) = w.ui_origin.clone() {
                match w.slot.issue(LaunchBind::Origin(origin.clone()), w.client.client_id) {
                    Ok(code) => deliver_ui_code(&w, &origin, &code),
                    Err(e) => {
                        let _ = tty_println(
                            &w.tty,
                            &format!("agentd {sub}: cannot mint the launch code ({e}); a tab can still ask this terminal to sign it in"),
                        );
                    }
                }
            }
            // 5. Wait for the client to exit, then drain the daemon.
            loop {
                if w.done.load(Ordering::Relaxed) {
                    return; // the daemon beat us to it; main reaps the child
                }
                let exited = w
                    .child_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                    .map(|c| c.exited().is_some())
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

/// A child of this process whose exit the daemon's reaper may collect first.
///
/// The daemon reaps with `waitpid(-1)`, which takes any exited child of the
/// process, the launcher's own included. A plain `try_wait` then answers
/// `ECHILD` forever and the launcher would wait on a client long gone, so the
/// child is the reaper's [`OwnedChild`](reaper::OwnedChild): routed from the
/// fork, its exit read from whichever side collected it, and never signalled
/// once its pid may name another process.
struct Tracked(reaper::OwnedChild);

impl Tracked {
    fn spawn(cmd: &mut Command) -> std::io::Result<Tracked> {
        reaper::spawn_owned(|| cmd.spawn()).map(Tracked)
    }

    fn id(&self) -> i32 {
        self.0.id()
    }

    /// `Some(clean)` once the child has exited, `None` while it runs.
    fn exited(&mut self) -> Option<bool> {
        Self::clean(self.0.try_wait())
    }

    /// Wait up to `limit` for the exit (`None`: for as long as it takes).
    fn wait_for(&mut self, limit: Option<Duration>) -> Option<bool> {
        Self::clean(self.0.wait_until(limit.map(|l| Instant::now() + l)))
    }

    /// A status something outside the registry took is gone for good: the
    /// child has exited all the same, and nothing says it went cleanly.
    fn clean(waited: std::io::Result<Option<reap::WaitOutcome>>) -> Option<bool> {
        match waited {
            Ok(outcome) => outcome.map(|o| o.is_clean()),
            Err(_) => Some(false),
        }
    }

    /// SIGTERM, then SIGKILL after [`CLIENT_GRACE`].
    fn stop(&mut self) {
        if self.exited().is_some() {
            return;
        }
        self.0.signal(libc::SIGTERM);
        if self.wait_for(Some(CLIENT_GRACE)).is_none() {
            self.0.kill();
            self.wait_for(Some(CLIENT_GRACE));
        }
    }
}

// ---- the web UI's launch code -----------------------------------------------

/// The launch file and the directory made for it alone.
struct LaunchFile {
    dir: PathBuf,
    file: PathBuf,
}

/// Where the launch file's directory goes: `$HOME` — which a snap-confined
/// browser may read and other users may not — else `$XDG_RUNTIME_DIR`.
fn launch_dir_base(env: &[(String, String)]) -> Option<PathBuf> {
    let var = |name: &str| {
        env.iter()
            .find(|(k, v)| k == name && !v.is_empty())
            .map(|(_, v)| PathBuf::from(v))
    };
    var("HOME").or_else(|| var("XDG_RUNTIME_DIR"))
}

/// Delete the launch file and its directory, if they are still there. `true`
/// when this call removed them — the code in them was never consumed.
fn remove_launch_file(lf: &Mutex<Option<LaunchFile>>) -> bool {
    let Some(f) = lf.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return false;
    };
    let _ = std::fs::remove_file(&f.file);
    let _ = std::fs::remove_dir(&f.dir);
    true
}

/// The page a launch file holds: a same-document redirect to `url`, by
/// script and by meta refresh, so a browser with scripts off follows it too.
/// `url` is `http://127.0.0.1:<port>/#launch=<code>` — nothing in it needs
/// escaping in HTML or in a string literal.
fn launch_page(url: &str) -> String {
    format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<meta name=\"referrer\" content=\"no-referrer\">\n\
         <meta http-equiv=\"refresh\" content=\"0;url={url}\">\n<title>agentd</title>\n\
         <script>location.replace(\"{url}\")</script>\n<a href=\"{url}\">Open agentd</a>\n"
    )
}

/// Write the launch file: a fresh 0700 directory under `base` that is not
/// hidden (a sandboxed browser's portal may refuse dot-directories), holding
/// one 0600 file created exclusively, so nothing planted can be written
/// through and no other user can read the code.
fn write_launch_file(base: &Path, url: &str) -> std::io::Result<LaunchFile> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let mut attempts = 0;
    let dir = loop {
        let dir = base.join(format!(
            "agentd-launch-{}",
            agentd::sec::random::hex_token(6)?
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1
            }
            Err(e) => return Err(e),
        }
    };
    // The mode above is masked by the umask; this one is not.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let file = dir.join("agentd.html");
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&file)
        .and_then(|mut f| f.write_all(launch_page(url).as_bytes()));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
        return Err(e);
    }
    Ok(LaunchFile { dir, file })
}

/// Give the web UI its code: through the desktop's opener when there is one,
/// else — `--no-open`, no opener, or one that failed — on this terminal.
fn deliver_ui_code(w: &Watch, origin: &str, code: &str) {
    let url = format!("{origin}/#launch={code}");
    let port = origin.rsplit(':').next().unwrap_or_default().to_string();
    let print_url = {
        let out = w.tty.stderr.try_clone().ok();
        let url = url.clone();
        move || {
            if let Some(fd) = out.as_ref().and_then(|o| o.try_clone().ok()) {
                let _ = put_line(
                    &mut std::fs::File::from(fd),
                    &format!(
                        "agentd ui: sign in by opening {url}\n  (it works once, within {}s; over SSH forward the same port: ssh -L {port}:127.0.0.1:{port} …)\n  after that, a tab that asks is signed in here: type the code it shows",
                        LAUNCH_CODE_TTL.as_secs()
                    ),
                );
            }
        }
    };
    let base = match (&w.launch_dir, w.open) {
        (Some(base), true) => base.clone(),
        _ => return print_url(),
    };
    let lf = match write_launch_file(&base, &url) {
        Ok(lf) => lf,
        Err(e) => {
            let _ = tty_println(
                &w.tty,
                &format!(
                    "agentd ui: cannot write the launch file under {}: {e}",
                    base.display()
                ),
            );
            return print_url();
        }
    };
    let path = lf.file.clone();
    *w.launch_file.lock().unwrap_or_else(|e| e.into_inner()) = Some(lf);
    // Unconsumed after its code's lifetime, the file is only a stale copy.
    let expire = Arc::clone(&w.launch_file);
    std::thread::spawn(move || {
        std::thread::sleep(LAUNCH_CODE_TTL);
        remove_launch_file(&expire);
    });
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // The opener is given the file's path, never the URL: a process's argv
    // is readable by every user on the host.
    let mut cmd = Command::new(opener);
    cmd.arg(&path)
        .env_clear()
        .envs(w.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let lf = Arc::clone(&w.launch_file);
    // Waited on its own thread: an opener may linger as long as the browser
    // it started, and the launcher must not.
    std::thread::spawn(move || {
        let opened = Tracked::spawn(&mut cmd)
            .ok()
            .and_then(|mut c| c.wait_for(None))
            .unwrap_or(false);
        if !opened && remove_launch_file(&lf) {
            print_url();
        }
    });
}

// ---- the terminal sign-in ---------------------------------------------------

/// What the terminal prompt reacts to.
enum PromptEvent {
    /// A browser tab asked to be signed in.
    Request,
    /// The person typed a line.
    Line(String),
    /// The terminal's input ended: nothing more can be approved.
    Eof,
}

/// Let the person at this terminal sign browser tabs in: a tab that asks
/// shows a code, and typing it here approves exactly that tab. One prompt is
/// shown however many tabs ask at once, with a count of the others.
fn terminal_sign_in(slot: &Arc<LaunchSlot>, tty: &Tty) -> std::io::Result<()> {
    let input = std::fs::File::from(tty.stdin.try_clone()?);
    let out = tty.stderr.try_clone()?;
    let (tx, rx) = mpsc::channel();
    let requests = Mutex::new(tx.clone());
    slot.on_request(move || {
        let _ = requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(PromptEvent::Request);
    });
    std::thread::Builder::new()
        .name("launcher-stdin".into())
        .spawn(move || {
            let mut lines = std::io::BufReader::new(input);
            loop {
                let mut line = String::new();
                match lines.read_line(&mut line) {
                    Ok(n) if n > 0 => {
                        if tx.send(PromptEvent::Line(line)).is_err() {
                            return;
                        }
                    }
                    _ => {
                        let _ = tx.send(PromptEvent::Eof);
                        return;
                    }
                }
            }
        })?;
    let slot = Arc::clone(slot);
    std::thread::Builder::new()
        .name("launcher-prompt".into())
        .spawn(move || prompt_loop(&slot, &rx, &mut std::fs::File::from(out)))?;
    Ok(())
}

/// The prompt's state machine, apart from the threads that feed it.
///
/// A prompt is `showing` from when it is printed until the next line is typed;
/// requests that arrive meanwhile print nothing. A burst is gathered first, so
/// the one prompt counts it. After a line, a prompt is shown again only if a
/// request arrived since the last one — Enter really skips.
fn prompt_loop(slot: &LaunchSlot, rx: &Receiver<PromptEvent>, out: &mut impl Write) {
    let mut showing = false;
    let mut unseen = false;
    while let Ok(first) = rx.recv() {
        let mut events = vec![first];
        if matches!(events[0], PromptEvent::Request) {
            let gather_until = Instant::now() + PROMPT_COALESCE_MAX;
            while Instant::now() < gather_until {
                match rx.recv_timeout(PROMPT_COALESCE) {
                    Ok(PromptEvent::Request) => events.push(PromptEvent::Request),
                    Ok(other) => {
                        events.push(other);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        for ev in events {
            match ev {
                PromptEvent::Request => unseen = true,
                PromptEvent::Line(line) => {
                    showing = false;
                    let typed = line.trim();
                    if typed.is_empty() {
                        continue;
                    }
                    let said = if slot.approve_user_code(typed) {
                        "agentd ui: signed in"
                    } else {
                        "agentd ui: no tab is showing that code"
                    };
                    let _ = put_line(out, said);
                }
                PromptEvent::Eof => return,
            }
        }
        if unseen && !showing {
            unseen = false;
            let waiting = slot.waiting();
            if waiting > 0 {
                let _ = match waiting - 1 {
                    0 => put_line(out, PROMPT),
                    more => put_line(out, &format!("{PROMPT} [{more} more waiting]")),
                };
                showing = true;
            }
        }
    }
}

/// Print a line to the SAVED terminal (the daemon's own stderr is redirected).
fn tty_println(tty: &Tty, msg: &str) -> std::io::Result<()> {
    // Through a close-on-exec duplicate, so the saved fd stays open.
    put_line(&mut std::fs::File::from(tty.stderr.try_clone()?), msg)
}

/// Write `line` and its newline in ONE write. The terminal is unbuffered and
/// shared — the prompt, the watcher and the opener's thread all print to it —
/// and `writeln!` issues a write per formatting piece, so another line could
/// land inside this one, and a reader could see a prompt without its count.
fn put_line(out: &mut impl Write, line: &str) -> std::io::Result<()> {
    out.write_all(format!("{line}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Both ends of the code's pipe are close-on-exec from the moment they
    /// exist. Read straight off the descriptors: the risk is a child the
    /// daemon spawns while the pipe is open, and no end-to-end run can time a
    /// spawn into that window.
    #[test]
    fn both_ends_of_the_launch_pipe_are_close_on_exec() {
        let (r, w) = launch_pipe().expect("pipe");
        for fd in [r.as_raw_fd(), w.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0, "fd {fd}: {}", std::io::Error::last_os_error());
            assert_ne!(flags & libc::FD_CLOEXEC, 0, "fd {fd} is inheritable");
        }
    }

    /// Every line the terminal gets is ONE write — the prompt with its count
    /// too. A line written in pieces can be read half done, or have another
    /// thread's line land inside it: the e2e flood test once read a prompt
    /// whose count had not been written yet.
    #[test]
    fn every_terminal_line_is_one_write() {
        struct Writes(Vec<String>);
        impl Write for Writes {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.push(String::from_utf8_lossy(b).into_owned());
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        use agentd::a2a::oauth::{Authority, Sessions, system_clock};
        let origin = "http://127.0.0.1:4555";
        let slot = Arc::new(LaunchSlot::new(Some(origin)).unwrap());
        let auth = Authority::new(
            None,
            Some(Arc::clone(&slot)),
            Arc::new(Sessions::new(system_clock())),
        );
        // Three tabs ask, so the prompt carries a count.
        for _ in 0..3 {
            let r = auth.launch_authorization(
                Some("application/x-www-form-urlencoded"),
                b"client_id=agentd-ui",
                Some([127, 0, 0, 1].into()),
                Some(origin),
            );
            assert_eq!(r.status, 200, "{r:?}");
        }
        let (tx, rx) = mpsc::channel();
        for ev in [
            PromptEvent::Request,
            PromptEvent::Request,
            PromptEvent::Request,
            PromptEvent::Line("WRONG-CODE\n".into()),
            PromptEvent::Eof,
        ] {
            tx.send(ev).unwrap();
        }
        let mut out = Writes(Vec::new());
        prompt_loop(&slot, &rx, &mut out);
        assert_eq!(
            out.0,
            [
                "agentd ui: no tab is showing that code\n".to_string(),
                format!("{PROMPT} [2 more waiting]\n"),
            ]
        );
    }

    /// The daemon loads exactly what it was given: the launcher takes its own
    /// flags out and adds nothing, so a flag it slipped in would be a setting
    /// the operator never wrote.
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

    /// A client whose status a `waitpid` outside the reaper's registry took
    /// still counts as exited — else the launcher would wait on a client long
    /// gone and never drain the daemon. (The daemon's own reaper hands the
    /// status over on the route: `supervisor::reaper`'s tests.)
    #[test]
    fn a_client_whose_status_is_gone_still_counts_as_exited() {
        let mut t = Tracked::spawn(&mut Command::new("true")).expect("spawn true");
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(t.id(), &mut status, 0) }, t.id());
        assert_eq!(t.exited(), Some(false), "gone, with no status to say how");
        assert_eq!(t.wait_for(None), Some(false), "and it stays gone");
    }

    #[test]
    fn a_client_exit_is_its_own_status() {
        let mut ok = Tracked::spawn(&mut Command::new("true")).expect("spawn true");
        assert_eq!(ok.wait_for(Some(Duration::from_secs(10))), Some(true));
        let mut bad = Tracked::spawn(&mut Command::new("false")).expect("spawn false");
        assert_eq!(bad.wait_for(Some(Duration::from_secs(10))), Some(false));
        let mut slow = Tracked::spawn(Command::new("sleep").arg("30")).expect("spawn sleep");
        assert_eq!(slow.exited(), None, "still running");
        slow.stop();
        assert_eq!(slow.exited(), Some(false), "stopped by a signal");
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
