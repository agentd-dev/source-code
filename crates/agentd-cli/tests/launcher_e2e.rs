// SPDX-License-Identifier: AGPL-3.0-only
//! The **`agentd tui` / `agentd ui` launcher** end to end, with a stub client
//! that records what it was handed: exactly the contract's argv, an
//! environment with nothing the config loader reads and no config secret, a
//! listening loopback socket on fd 3 for the web UI — and nothing else. Also:
//! the removed launcher flags refused by name before any config is read, the
//! postures the clients cannot use refused before anything is spawned, the
//! daemon log that will not write through a planted path, and the tied
//! lifetimes.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A loopback daemon that never dials its model: `preflight: never` and no
/// turn is ever run.
fn config(port: u16, extra_a2a: &str, extra: &str) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: launcher-e2e\n  instruction: Test.\n  preflight: never\n\
         intelligence:\n  endpoints: https://127.0.0.1:9\n  model: mock\n{extra}\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{extra_a2a}\
         lifecycle:\n  run_until: drained\n  drain_timeout: 2s\n"
    )
}

/// A scratch directory for one launch: the config, the stub client, what the
/// stub records, and the launcher's own output.
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn write(&self, name: &str, body: &str) -> String {
        let p = self.path(name);
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().into_owned()
    }
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    /// The stub client: records its argv (one per line), its environment, its
    /// open descriptors, the contents of any regular file it holds open, and
    /// — on Linux — the kernel's TCP table so fd 3 can be identified. Then it
    /// exits 0, which must drain the daemon.
    fn stub(&self) -> String {
        let out = self.dir.path().display();
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {out}/argv\n\
             env > {out}/env\n\
             : > {out}/fdfiles\n\
             if [ -d /proc/$$/fd ]; then\n\
             \x20 for fd in /proc/$$/fd/*; do\n\
             \x20   echo \"$(basename $fd) $(readlink $fd)\" >> {out}/fds\n\
             \x20   if [ -f \"$fd\" ]; then head -c 65536 \"$fd\" >> {out}/fdfiles 2>/dev/null; fi\n\
             \x20 done\n\
             \x20 cat /proc/net/tcp /proc/net/tcp6 > {out}/tcp 2>/dev/null\n\
             fi\n\
             touch {out}/ran\n\
             exit 0\n"
        );
        let p = self.write("stub.sh", &script);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

/// Run the launcher to completion (it must exit by itself once the stub has)
/// and return its status and stderr.
fn launch(s: &Scratch, args: &[&str], env: &[(&str, &str)]) -> (ExitStatus, String) {
    let err = std::fs::File::create(s.path("launcher.err")).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn the launcher");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Ok(Some(st)) = child.try_wait() {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "the launcher did not exit by itself; stderr:\n{}",
                s.read("launcher.err")
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    (status, s.read("launcher.err"))
}

/// A removed launcher flag is refused by name before any configuration is
/// read — the `--config` here does not exist, so reading it would fail
/// differently.
#[test]
fn removed_launcher_flags_are_refused_by_name() {
    let s = Scratch::new();
    for (sub, flag, hint) in [
        ("tui", "--debug", "set a2a.introspection.enabled"),
        ("ui", "--inline", "a display-client option"),
        ("ui", "--debug", "set a2a.introspection.enabled"),
        ("tui", "--inline", "a display-client option"),
    ] {
        let (status, err) = launch(
            &s,
            &[sub, flag, "--config", "/nonexistent/agentd.yaml"],
            &[],
        );
        assert_eq!(status.code(), Some(2), "{sub} {flag}: {err}");
        assert!(
            err.contains(&format!("agentd {sub} {flag} was removed in agentd 1.17.0"))
                && err.contains(hint),
            "{sub} {flag}: {err}"
        );
        assert!(
            !err.contains("npm") && !err.contains("@agentd-dev"),
            "{sub} {flag}: {err}"
        );
        assert!(!err.contains("/nonexistent"), "no config was read: {err}");
    }
}

/// The variables the daemon's configuration reads, each holding a value the
/// client must never see.
const SEKRIT_ENV: &[(&str, &str)] = &[
    ("LLM_KEY", "sekrit-llm-key"),
    ("AGENTD_BEARER", "sekrit-old-handoff"),
    ("AGENTD_AGENT_DESCRIPTION", "sekrit-description"),
    ("SERVE_BEARER", "sekrit-serve-bare"),
    ("AGENTD_SERVE_BEARER", "sekrit-serve-branded"),
    ("AGENT_SERVE_BEARER", "sekrit-serve-neutral"),
];

fn assert_nothing_leaked(s: &Scratch) {
    let env = s.read("env");
    assert!(!env.is_empty(), "the stub recorded its environment");
    for (name, _) in SEKRIT_ENV {
        assert!(
            !env.lines().any(|l| l.starts_with(&format!("{name}="))),
            "{name} reached the client:\n{env}"
        );
    }
    for (what, text) in [
        ("environment", env),
        ("argv", s.read("argv")),
        ("open files", s.read("fdfiles")),
        ("descriptors", s.read("fds")),
    ] {
        assert!(
            !text.contains("sekrit"),
            "a secret reached the client's {what}:\n{text}"
        );
    }
}

/// The inode of the socket the stub held on `fd`, from its descriptor list.
fn socket_inode(fds: &str, fd: u32) -> Option<String> {
    fds.lines()
        .find_map(|l| l.strip_prefix(&format!("{fd} ")))
        .and_then(|t| t.strip_prefix("socket:["))
        .map(|t| t.trim_end_matches(']').to_string())
}

#[test]
fn the_launcher_passes_only_the_contract() {
    // tui: `--endpoint <url>`, the scrubbed environment, the terminal on 0/1/2.
    let s = Scratch::new();
    let port = free_port();
    let cfg = s.write(
        "agentd.yaml",
        &config(
            port,
            "",
            "  headers:\n    x-api-key: \"{{secret:LLM_KEY}}\"\n",
        ),
    );
    let stub = s.stub();
    let mut env = SEKRIT_ENV.to_vec();
    env.push(("AGENTD_TUI_BIN", &stub));
    env.push(("LAUNCHER_E2E_PASSES", "through"));
    let (status, err) = launch(&s, &["tui", "--config", &cfg], &env);
    assert!(status.success(), "{status:?}: {err}");
    assert_eq!(
        s.read("argv").lines().collect::<Vec<_>>(),
        vec!["--endpoint", &format!("http://127.0.0.1:{port}")],
        "the tui gets its endpoint and nothing else"
    );
    assert_nothing_leaked(&s);
    assert!(
        s.read("env").contains("LAUNCHER_E2E_PASSES=through"),
        "a variable the loader does not read is the client's to keep"
    );

    // ui: `--endpoint <url> --listen-fd 3`, fd 3 a listening loopback socket,
    // stdin /dev/null.
    let s = Scratch::new();
    let port = free_port();
    let cfg = s.write(
        "agentd.yaml",
        &config(
            port,
            "",
            "  headers:\n    x-api-key: \"{{secret:LLM_KEY}}\"\n",
        ),
    );
    let stub = s.stub();
    let mut env = SEKRIT_ENV.to_vec();
    env.push(("AGENTD_UI_BIN", &stub));
    let (status, err) = launch(
        &s,
        &["ui", "--port", "0", "--no-open", "--config", &cfg],
        &env,
    );
    assert!(status.success(), "{status:?}: {err}");
    let endpoint = format!("http://127.0.0.1:{port}");
    assert_eq!(
        s.read("argv").lines().collect::<Vec<_>>(),
        vec!["--endpoint", endpoint.as_str(), "--listen-fd", "3"],
    );
    assert_nothing_leaked(&s);
    if cfg!(target_os = "linux") {
        let fds = s.read("fds");
        assert!(
            fds.lines().any(|l| l == "0 /dev/null"),
            "the web UI's stdin is /dev/null: {fds}"
        );
        let inode = socket_inode(&fds, 3).unwrap_or_else(|| panic!("fd 3 is a socket: {fds}"));
        // /proc/net/tcp: `sl local rem st … uid timeout inode`; 0A = LISTEN,
        // 0100007F = 127.0.0.1 in the kernel's byte order.
        let tcp = s.read("tcp");
        let listening = tcp.lines().any(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            f.len() > 9 && f[9] == inode && f[3] == "0A" && f[1].starts_with("0100007F:")
        });
        assert!(listening, "fd 3 is a socket listening on 127.0.0.1:\n{tcp}");
    }
}

/// The launcher adds nothing to the daemon: with neither switch in the config,
/// the daemon's own startup line reports both off. (That the argv it loads is
/// the argv it was given, minus the launcher's flags, is pinned beside the
/// split itself.)
#[test]
fn the_launcher_forces_no_configuration() {
    let s = Scratch::new();
    let cfg = s.write("agentd.yaml", &config(free_port(), "", ""));
    let stub = s.stub();
    let log = s.path("daemon.log");
    let (status, err) = launch(
        &s,
        &[
            "tui",
            "--daemon-log",
            log.to_str().unwrap(),
            "--config",
            &cfg,
        ],
        &[("AGENTD_TUI_BIN", &stub)],
    );
    assert!(status.success(), "{status:?}: {err}");
    let dlog = std::fs::read_to_string(&log).unwrap();
    let listen = dlog
        .lines()
        .find(|l| l.contains("\"event\":\"a2a.listen\""))
        .unwrap_or_else(|| panic!("no a2a.listen line:\n{dlog}"));
    assert!(
        listen.contains("\"events\":false") && listen.contains("\"introspection\":false"),
        "the launcher switched something on: {listen}"
    );
    assert!(!dlog.contains("\"interface\":true"), "{dlog}");
}

/// `a2a.bearer` stays in the daemon: not in the client's argv, environment or
/// open files, under any name.
#[test]
fn a_protected_daemon_never_hands_its_bearer_to_the_client() {
    let s = Scratch::new();
    let cfg = s.write(
        "agentd.yaml",
        &config(free_port(), "  bearer: \"{{secret:A2A_TOKEN}}\"\n", ""),
    );
    let stub = s.stub();
    let (status, err) = launch(
        &s,
        &["tui", "--config", &cfg],
        &[
            ("AGENTD_TUI_BIN", &stub),
            ("A2A_TOKEN", "sekrit-root-bearer"),
        ],
    );
    assert!(status.success(), "{status:?}: {err}");
    assert!(
        !s.read("env").contains("A2A_TOKEN="),
        "the bearer's variable is scrubbed"
    );
    assert_nothing_leaked(&s);
}

/// The client exiting drains the daemon, and the daemon's output went to
/// `--daemon-log` while the client had the terminal.
#[test]
fn client_exit_drains_the_daemon() {
    let s = Scratch::new();
    let port = free_port();
    let cfg = s.write("agentd.yaml", &config(port, "", ""));
    let stub = s.stub();
    let log = s.path("daemon.log");
    let (status, err) = launch(
        &s,
        &[
            "tui",
            "--daemon-log",
            log.to_str().unwrap(),
            "--config",
            &cfg,
        ],
        &[("AGENTD_TUI_BIN", &stub)],
    );
    assert!(status.success(), "clean drain exit: {status:?}: {err}");
    assert!(Path::new(&s.path("ran")).exists(), "the client ran");
    let dlog = std::fs::read_to_string(&log).unwrap();
    assert!(dlog.contains("a2a.listen"), "the daemon logged to the file");
    assert!(
        err.contains(&format!("endpoint http://127.0.0.1:{port}")) && err.contains("exited"),
        "{err}"
    );
}

#[test]
fn a_missing_client_names_the_override_not_a_package() {
    let s = Scratch::new();
    let cfg = s.write("agentd.yaml", &config(free_port(), "", ""));
    let (status, err) = launch(
        &s,
        &["tui", "--config", &cfg],
        &[("AGENTD_TUI_BIN", "/nonexistent/agentd-tui")],
    );
    assert!(
        !status.success(),
        "a launch whose client never ran failed: {err}"
    );
    assert!(
        err.contains("AGENTD_TUI_BIN")
            && err.contains("https://agentd.dev/docs/interface#launcher"),
        "{err}"
    );
    assert!(
        !err.contains("npm") && !err.contains("@agentd-dev"),
        "{err}"
    );
}

/// What a display client could never use is refused before anything is
/// spawned — no client ran and no daemon log was opened — naming the cause.
#[test]
fn the_launcher_refuses_what_its_clients_cannot_use() {
    let tls = "  tls: {cert: /nonexistent/c.pem, key: /nonexistent/k.pem";
    for (what, listen_and_more, cause) in [
        (
            "client_ca",
            format!(
                "a2a:\n  listen: https://127.0.0.1:{}\n{tls}, client_ca: /nonexistent/ca.pem}}\n",
                free_port()
            ),
            "client certificates",
        ),
        (
            "unix",
            "a2a:\n  listen: \"unix:/tmp/agentd-launcher-e2e.sock\"\n".to_string(),
            "unix",
        ),
        (
            "port 0",
            "a2a:\n  listen: http://127.0.0.1:0\n".to_string(),
            "port",
        ),
        (
            "public endpoint",
            format!(
                "a2a:\n  listen: https://0.0.0.0:{p}\n  url: https://agent.example:{p}\n  bearer: \"{{{{secret:A2A_TOKEN}}}}\"\n{tls}}}\n",
                p = free_port()
            ),
            "loopback",
        ),
    ] {
        let s = Scratch::new();
        let cfg = s.write(
            "agentd.yaml",
            &format!(
                "config_version: \"1\"\n\
                 agent:\n  name: launcher-refuse\n  instruction: Test.\n  preflight: never\n\
                 intelligence:\n  endpoints: https://127.0.0.1:9\n  model: mock\n\
                 store:\n  kind: memory\n{listen_and_more}"
            ),
        );
        let stub = s.stub();
        let log = s.path("daemon.log");
        let (status, err) = launch(
            &s,
            &[
                "tui",
                "--daemon-log",
                log.to_str().unwrap(),
                "--config",
                &cfg,
            ],
            &[("AGENTD_TUI_BIN", &stub), ("A2A_TOKEN", "t")],
        );
        assert_eq!(status.code(), Some(2), "{what}: {err}");
        assert!(
            err.contains(cause),
            "{what}: the refusal names the cause: {err}"
        );
        assert!(!s.path("ran").exists(), "{what}: a client was spawned");
        assert!(!log.exists(), "{what}: the daemon got as far as its log");
    }
}

/// `--daemon-log` never writes through what is already at the path: in a
/// shared temp dir, a planted file or symlink at the predictable name would
/// otherwise receive the daemon's log, or aim it at a file of its choosing.
#[test]
fn the_daemon_log_refuses_a_planted_path() {
    let s = Scratch::new();
    let cfg = s.write("agentd.yaml", &config(free_port(), "", ""));
    let stub = s.stub();

    let planted = s.write("planted.log", "untouched\n");
    let target = s.write("target", "untouched\n");
    let link = s.path("link.log");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    for path in [planted.clone(), link.to_string_lossy().into_owned()] {
        let (status, err) = launch(
            &s,
            &["tui", "--daemon-log", &path, "--config", &cfg],
            &[("AGENTD_TUI_BIN", &stub)],
        );
        assert!(!status.success(), "{path}: {err}");
        assert!(err.contains("the log must be a new file"), "{path}: {err}");
    }
    assert_eq!(std::fs::read_to_string(&planted).unwrap(), "untouched\n");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched\n");
    assert!(
        !s.path("ran").exists(),
        "no client ran against a refused log"
    );

    let fresh = s.path("fresh.log");
    let (status, err) = launch(
        &s,
        &[
            "tui",
            "--daemon-log",
            fresh.to_str().unwrap(),
            "--config",
            &cfg,
        ],
        &[("AGENTD_TUI_BIN", &stub)],
    );
    assert!(status.success(), "{err}");
    let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the daemon log is the operator's alone");
}
