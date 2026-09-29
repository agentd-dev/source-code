// SPDX-License-Identifier: AGPL-3.0-only
//! The **`agentd tui` / `agentd ui` launcher** end to end, with a stub client
//! that records what it was handed: exactly the contract's argv, an
//! environment with nothing the config loader reads and no config secret, a
//! single-use launch code on a pipe for the TUI, a listening loopback socket
//! for the web UI — each on fd 3 — and no other descriptor. Also: the code
//! signs a client in once and only where it was meant to, a browser tab gets
//! its code in a URL fragment or from the launcher's terminal, the postures
//! the clients cannot use are refused before anything is spawned, the daemon
//! log will not write through a planted path, and the lifetimes are tied.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use common::{HttpReply, SendMessage};
use serde_json::{Value, json};

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
        "\
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
    /// pid, its open descriptors (a snapshot taken before the shell opens
    /// anything of its own), the contents of any regular file it holds open,
    /// the launch code on fd 3 when it was given `--launch-fd` (read to
    /// end-of-file, bounded, so a write end left open shows as a timeout) and
    /// — on Linux — the kernel's TCP table so fd 3 can be identified. Then it
    /// marks `recorded`, holds while a `hold` file exists until `release`
    /// appears, and exits 0, which must drain the daemon.
    fn stub(&self) -> String {
        let out = self.dir.path().display();
        let script = format!(
            "#!/bin/sh\n\
             out={out}\n\
             printf '%s\\n' \"$@\" > $out/argv\n\
             env > $out/env\n\
             echo $$ > $out/pid\n\
             : > $out/fdfiles\n\
             : > $out/fds\n\
             if [ -d /proc/$$/fd ]; then\n\
             \x20 ls /proc/$$/fd > $out/fdnums\n\
             \x20 for fd in $(cat $out/fdnums); do\n\
             \x20   echo \"$fd $(readlink /proc/$$/fd/$fd)\" >> $out/fds\n\
             \x20   if [ -f /proc/$$/fd/$fd ]; then head -c 65536 /proc/$$/fd/$fd >> $out/fdfiles 2>/dev/null; fi\n\
             \x20 done\n\
             \x20 cat /proc/net/tcp /proc/net/tcp6 > $out/tcp 2>/dev/null\n\
             fi\n\
             if [ \"$3\" = --launch-fd ]; then\n\
             \x20 timeout 2 cat <&3 > $out/code.part\n\
             \x20 echo $? > $out/code.rc\n\
             \x20 mv $out/code.part $out/code\n\
             \x20 exec 3<&-\n\
             fi\n\
             touch $out/recorded\n\
             if [ -e $out/hold ]; then\n\
             \x20 i=0\n\
             \x20 while [ ! -e $out/release ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done\n\
             fi\n\
             touch $out/ran\n\
             exit 0\n"
        );
        let p = self.write("stub.sh", &script);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// [`Self::stub`], told to stay up until [`Running::finish`].
    fn holding_stub(&self) -> String {
        self.write("hold", "");
        self.stub()
    }

    /// The descriptors the stub held, number → what it pointed at — without
    /// the shell's own: the one it reads its script on, and any it had open
    /// for an instant and closed before the stub could name it. An inherited
    /// descriptor is never closed by the shell, so it is always named.
    fn client_fds(&self) -> BTreeMap<u32, String> {
        let script = self.path("stub.sh").display().to_string();
        self.read("fds")
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter(|(_, target)| *target != script && !target.is_empty())
            .filter_map(|(n, target)| Some((n.parse().ok()?, target.to_string())))
            .collect()
    }

    /// A desktop opener on a PATH of its own: it records its argv and a copy
    /// of the page it was asked to open, and exits `rc`.
    fn opener(&self, rc: i32) -> String {
        let bin = self.path("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let out = self.dir.path().display();
        let name = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let p = bin.join(name);
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$@\" > {out}/opener.argv\n\
                 cp \"$1\" {out}/opener.page\n\
                 exit {rc}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// Wait for `name` to exist, panicking with the launcher's output if it
    /// never does.
    fn wait_file(&self, name: &str, within: Duration) {
        let deadline = Instant::now() + within;
        while !self.path(name).exists() {
            assert!(
                Instant::now() < deadline,
                "{name} never appeared; launcher stderr:\n{}",
                self.read("launcher.err")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until the launcher's terminal output satisfies `ok`.
    fn wait_terminal(&self, what: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let err = self.read("launcher.err");
            if ok(&err) {
                return err;
            }
            assert!(Instant::now() < deadline, "{what}; terminal:\n{err}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A launcher running in the background, for a test that talks to the
/// daemon while the client is up.
struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Running {
    /// Type a line on the launcher's terminal.
    fn type_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("the launcher's stdin is ours");
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    /// Let the stub exit, and wait for the launcher to follow it.
    fn finish(mut self, s: &Scratch) -> (ExitStatus, String) {
        s.write("release", "");
        drop(self.stdin.take());
        wait_exit(&mut self.child, s)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start the launcher and return at once; `stdin` is a pipe the test holds
/// when `typed`, else /dev/null.
fn start(s: &Scratch, args: &[&str], env: &[(&str, &str)], typed: bool) -> Running {
    let err = std::fs::File::create(s.path("launcher.err")).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(args)
        .stdin(if typed { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::from(err));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn the launcher");
    let stdin = child.stdin.take();
    Running { child, stdin }
}

fn wait_exit(child: &mut Child, s: &Scratch) -> (ExitStatus, String) {
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

/// Run the launcher to completion (it must exit by itself once the stub has)
/// and return its status and stderr.
fn launch(s: &Scratch, args: &[&str], env: &[(&str, &str)]) -> (ExitStatus, String) {
    let mut r = start(s, args, env, false);
    wait_exit(&mut r.child, s)
}

/// The variables the daemon's configuration reads, each holding a value the
/// client must never see — and `AGENTD_BEARER`, which the TUI would read as a
/// credential and refuse beside its launch code.
const SEKRIT_ENV: &[(&str, &str)] = &[
    ("LLM_KEY", "sekrit-llm-key"),
    ("AGENTD_BEARER", "sekrit-tui-credential"),
    ("AGENTD_AGENT_DESCRIPTION", "sekrit-description"),
    ("AGENTD_A2A_BEARER", "sekrit-a2a-bearer"),
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

/// The launch code the stub read from fd 3: the whole of what the pipe held,
/// which must be exactly one code and a newline, delivered to end-of-file.
fn tui_code(s: &Scratch) -> String {
    s.wait_file("recorded", Duration::from_secs(20));
    assert_eq!(
        s.read("code.rc").trim(),
        "0",
        "the client read fd 3 to end-of-file within 2 s (a write end left open never ends)"
    );
    let raw = s.read("code");
    let code = raw
        .strip_suffix('\n')
        .unwrap_or_else(|| panic!("one line: {raw:?}"));
    assert!(
        code.len() == 10 + 64
            && code.starts_with("agentd_lc_")
            && code[10..].bytes().all(|b| b.is_ascii_hexdigit()),
        "one launch code: {raw:?}"
    );
    code.to_string()
}

#[test]
fn the_launcher_passes_only_the_contract() {
    // tui: `--endpoint <url> --launch-fd 3`, the scrubbed environment, the
    // terminal on 0/1/2 and the code's pipe on 3 — and no other descriptor.
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
        vec![
            "--endpoint",
            &format!("http://127.0.0.1:{port}"),
            "--launch-fd",
            "3"
        ],
        "the tui gets its endpoint and its code's descriptor, and nothing else"
    );
    tui_code(&s);
    assert_nothing_leaked(&s);
    assert!(
        s.read("env").contains("LAUNCHER_E2E_PASSES=through"),
        "a variable the loader does not read is the client's to keep"
    );
    if cfg!(target_os = "linux") {
        let fds = s.client_fds();
        assert_eq!(
            fds.keys().copied().collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "the tui holds its stdio and the code's pipe, nothing the launcher kept: {fds:?}"
        );
        assert!(fds[&3].starts_with("pipe:["), "fd 3 is a pipe: {fds:?}");
    }

    // ui: `--endpoint <url> --listen-fd 3`, fd 3 a listening loopback socket,
    // stdin /dev/null — and no other descriptor.
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
        let fds = s.client_fds();
        assert_eq!(
            fds.keys().copied().collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "the web UI holds its stdio and its socket, nothing the launcher kept: {fds:?}"
        );
        assert_eq!(fds[&0], "/dev/null", "the web UI's stdin is /dev/null");
        assert_listening_on_loopback(&s, 3);
    }
}

/// On Linux: the stub's fd `fd` was a socket listening on 127.0.0.1; its
/// port.
fn assert_listening_on_loopback(s: &Scratch, fd: u32) -> u16 {
    let fds = s.read("fds");
    let inode = socket_inode(&fds, fd).unwrap_or_else(|| panic!("fd {fd} is a socket: {fds}"));
    // /proc/net/tcp: `sl local rem st … uid timeout inode`; 0A = LISTEN,
    // 0100007F = 127.0.0.1 in the kernel's byte order.
    let tcp = s.read("tcp");
    tcp.lines()
        .find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() > 9 && f[9] == inode && f[3] == "0A")
                .then(|| f[1].strip_prefix("0100007F:"))
                .flatten()
                .and_then(|p| u16::from_str_radix(p, 16).ok())
        })
        .unwrap_or_else(|| panic!("fd {fd} is a socket listening on 127.0.0.1:\n{tcp}"))
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
}

// ---- the launch code, from the client's side ---------------------------------

/// The launch grant, form-encoded.
fn grant() -> String {
    agentd::runtime::surface::launch::LAUNCH_GRANT_TYPE
        .replace(':', "%3A")
        .replace('/', "%2F")
}

/// A form POST to `path`, as an OAuth client sends one — from a browser page
/// when `origin` is given.
fn form_post(addr: &str, path: &str, body: &str, origin: Option<&str>) -> HttpReply {
    use std::io::Read;
    let mut s =
        std::net::TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let origin = origin
        .map(|o| format!("Origin: {o}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/x-www-form-urlencoded\r\n\
         {origin}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    HttpReply {
        status,
        headers: lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect(),
        body: body.to_string(),
    }
}

/// Present a launch code as `client`, with `origin`.
fn exchange(addr: &str, code: &str, client: &str, origin: Option<&str>) -> HttpReply {
    form_post(
        addr,
        "/oauth2/token",
        &format!("grant_type={}&code={code}&client_id={client}", grant()),
        origin,
    )
}

/// The OAuth `error` of a refusal.
fn oauth_error(r: &HttpReply) -> String {
    serde_json::from_str::<Value>(&r.body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The access token of a successful exchange, which must be an operator's.
fn operator_token(r: &HttpReply) -> String {
    assert_eq!(r.status, 200, "{r:?}");
    let v = r.json();
    assert_eq!(v["scope"], "operator", "{v}");
    v["access_token"].as_str().expect("a token").to_string()
}

/// The document a command answered with.
fn answer(v: &Value) -> Value {
    if let Some(doc) = v["message"]["parts"][0].get("data") {
        return doc.clone();
    }
    v["task"]["artifacts"][0]["parts"][0]["data"].clone()
}

/// `token` is an operator's session: it reads status, and the operator-only
/// session list names it as a launch.
fn assert_operator_session(addr: &str, token: &str) {
    SendMessage::command("status", json!({}))
        .bearer(token)
        .result(addr);
    let listed = answer(
        &SendMessage::command("auth.sessions", json!({}))
            .bearer(token)
            .result(addr),
    );
    let rows = listed["sessions"].as_array().cloned().unwrap_or_default();
    assert!(
        rows.iter()
            .any(|r| r["kind"] == "launch" && r["role"] == "operator"),
        "the session is the operator's, and a launch: {listed}"
    );
}

/// `a2a.bearer` stays in the daemon: the client gets a single-use code
/// instead, which signs it in once, as the operator, from no browser.
#[test]
fn a_protected_daemon_launches_the_tui_with_a_single_use_code() {
    let s = Scratch::new();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = s.write(
        "agentd.yaml",
        &config(port, "  bearer: \"{{secret:A2A_TOKEN}}\"\n", ""),
    );
    let stub = s.holding_stub();
    let run = start(
        &s,
        &["tui", "--config", &cfg],
        &[
            ("AGENTD_TUI_BIN", &stub),
            ("A2A_TOKEN", "sekrit-root-bearer"),
        ],
        false,
    );
    let code = tui_code(&s);
    assert_eq!(
        s.read("argv").lines().collect::<Vec<_>>(),
        vec!["--endpoint", &format!("http://{addr}"), "--launch-fd", "3"],
    );
    assert!(
        !s.read("env").contains("A2A_TOKEN="),
        "the bearer's variable is scrubbed"
    );
    assert_nothing_leaked(&s);
    assert!(
        !s.read("code").contains("sekrit"),
        "the pipe held a code, not the bearer"
    );

    let token = operator_token(&exchange(&addr, &code, "agentd-tui", None));
    assert_operator_session(&addr, &token);
    let again = exchange(&addr, &code, "agentd-tui", None);
    assert_eq!(
        (again.status, oauth_error(&again)),
        (400, "invalid_grant".into()),
        "a code signs in once: {again:?}"
    );
    let (status, err) = run.finish(&s);
    assert!(status.success(), "{status:?}: {err}");
    assert!(!err.contains(&code), "the code reached the terminal: {err}");

    // A fresh launch whose listener admits one browser origin: a code minted
    // for no browser is burned by a page of that origin, and a page of any
    // other origin is turned away before it can burn anything.
    let s = Scratch::new();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = s.write(
        "agentd.yaml",
        &config(
            port,
            "  bearer: \"{{secret:A2A_TOKEN}}\"\n  cors:\n    origins: [\"http://localhost:9\"]\n",
            "",
        ),
    );
    let stub = s.holding_stub();
    let run = start(
        &s,
        &["tui", "--config", &cfg],
        &[
            ("AGENTD_TUI_BIN", &stub),
            ("A2A_TOKEN", "sekrit-root-bearer"),
        ],
        false,
    );
    let code = tui_code(&s);
    let foreign = exchange(&addr, &code, "agentd-tui", Some("http://evil.example"));
    assert_eq!(foreign.status, 403, "not an admitted origin: {foreign:?}");
    let page = exchange(&addr, &code, "agentd-tui", Some("http://localhost:9"));
    assert_eq!(
        (page.status, oauth_error(&page)),
        (400, "invalid_grant".into()),
        "a terminal's code is no page's: {page:?}"
    );
    let after = exchange(&addr, &code, "agentd-tui", None);
    assert_eq!(
        oauth_error(&after),
        "invalid_grant",
        "and it was burned: {after:?}"
    );
    let (status, err) = run.finish(&s);
    assert!(status.success(), "{status:?}: {err}");
}

/// The TUI gets a code even from a daemon that asks no credential, so the
/// session it buys outlives a reload that starts asking for one.
#[cfg(feature = "hot-reload")]
#[test]
fn the_tui_always_gets_a_code_and_survives_a_posture_reload() {
    let s = Scratch::new();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = s.write("agentd.yaml", &config(port, "", ""));
    let stub = s.holding_stub();
    let log = s.path("daemon.log");
    let run = start(
        &s,
        &[
            "tui",
            "--daemon-log",
            log.to_str().unwrap(),
            "--config",
            &cfg,
        ],
        &[("AGENTD_TUI_BIN", &stub), ("E2E_CI_TOKEN", "ci-token")],
        false,
    );
    let code = tui_code(&s);
    assert_eq!(
        s.read("argv").lines().collect::<Vec<_>>(),
        vec!["--endpoint", &format!("http://{addr}"), "--launch-fd", "3"],
        "the argv does not depend on the posture"
    );
    let token = operator_token(&exchange(&addr, &code, "agentd-tui", None));

    // A principal rule ends the implicit operator.
    s.write(
        "agentd.yaml",
        &config(
            port,
            "  principals:\n    - id: ci\n      match: { bearer_ref: \"{{secret:E2E_CI_TOKEN}}\" }\n      role: user\n",
            "",
        ),
    );
    unsafe { libc::kill(run.child.id() as i32, libc::SIGHUP) };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let r = common::a2a_post(&addr, &common::rpc_body(1, "ListTasks", json!({})), &[]);
        if r.status == 401 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the reload never took the implicit operator away: {r:?}\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_operator_session(&addr, &token);
    let (status, err) = run.finish(&s);
    assert!(status.success(), "{status:?}: {err}");
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
                "\
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

// ---- the web UI's sign-in ----------------------------------------------------

/// The code in a `#launch=` URL, and the URL's port.
fn fragment_code(text: &str) -> (String, u16) {
    let at = text
        .find("/#launch=")
        .unwrap_or_else(|| panic!("no launch URL: {text}"));
    let code: String = text[at + 9..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    assert!(
        code.starts_with("agentd_lc_") && code.len() == 10 + 64,
        "{text}"
    );
    let port = text[..at]
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no port before the fragment: {text}"));
    assert!(
        text[..at].ends_with(&format!("http://127.0.0.1:{port}")),
        "the URL is the UI's loopback origin: {text}"
    );
    (code, port)
}

/// `ListTasks` from a page of `origin`, as `bearer` when given.
fn list_tasks_from(addr: &str, origin: &str, bearer: Option<&str>) -> HttpReply {
    let auth = bearer.map(|b| format!("Bearer {b}"));
    let mut headers = vec![("Origin", origin)];
    if let Some(a) = &auth {
        headers.push(("Authorization", a.as_str()));
    }
    common::a2a_post(addr, &common::rpc_body(1, "ListTasks", json!({})), &headers)
}

/// `agentd ui` opens a tab that signs itself in: the opener gets the path of
/// a private page that redirects to the UI with the code in its fragment —
/// never the code itself — and the UI's server gets nothing but its socket.
#[test]
fn agentd_ui_opens_a_signed_in_tab_without_serving_a_credential() {
    let s = Scratch::new();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = s.write("agentd.yaml", &config(port, "", ""));
    let stub = s.holding_stub();
    let path = s.opener(0);
    let home = s.path("home");
    std::fs::create_dir_all(&home).unwrap();
    let log = s.path("daemon.log");
    let run = start(
        &s,
        &[
            "ui",
            "--port",
            "0",
            "--daemon-log",
            log.to_str().unwrap(),
            "--config",
            &cfg,
        ],
        &[
            ("AGENTD_UI_BIN", &stub),
            ("PATH", &path),
            ("HOME", home.to_str().unwrap()),
        ],
        false,
    );
    s.wait_file("recorded", Duration::from_secs(20));
    s.wait_file("opener.page", Duration::from_secs(20));
    assert_eq!(
        s.read("argv").lines().collect::<Vec<_>>(),
        vec!["--endpoint", &format!("http://{addr}"), "--listen-fd", "3"],
    );
    let ui_port = if cfg!(target_os = "linux") {
        let fds = s.client_fds();
        assert_eq!(
            fds.get(&0).map(String::as_str),
            Some("/dev/null"),
            "{fds:?}"
        );
        Some(assert_listening_on_loopback(&s, 3))
    } else {
        None
    };

    // The opener was handed one path: a private file in a private, visible
    // directory of the user's own.
    let opened = s.read("opener.argv");
    let args: Vec<&str> = opened.lines().collect();
    assert_eq!(
        args.len(),
        1,
        "the opener gets only the launch file: {args:?}"
    );
    let file = PathBuf::from(args[0]);
    assert!(
        !opened.contains("agentd_lc_"),
        "the code is not on the opener's argv"
    );
    let dir = file.parent().unwrap().to_path_buf();
    assert_eq!(
        dir.parent(),
        Some(home.as_path()),
        "under $HOME: {}",
        file.display()
    );
    let dir_name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        dir_name.starts_with("agentd-launch-") && !dir_name.starts_with('.'),
        "a visible directory a sandboxed browser may read: {dir_name}"
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600, "the launch file is the user's alone");
    assert_eq!(mode(&dir), 0o700, "and so is its directory");
    let page = s.read("opener.page");
    let (code, p) = fragment_code(&page);
    if let Some(ui_port) = ui_port {
        assert_eq!(p, ui_port, "the page redirects to the UI's own socket");
    }
    assert!(
        page.contains("location.replace(") && page.contains("http-equiv=\"refresh\""),
        "a same-document redirect: {page}"
    );
    let origin = format!("http://127.0.0.1:{p}");

    // The page's exchange signs it in as the operator, and the file is gone
    // by the time the answer arrives.
    let token = operator_token(&exchange(&addr, &code, "agentd-ui", Some(&origin)));
    assert!(
        !file.exists() && !dir.exists(),
        "the launch file outlived its code"
    );

    let ok = list_tasks_from(&addr, &origin, Some(&token));
    assert_eq!(ok.status, 200, "{ok:?}");
    assert_eq!(
        ok.header("access-control-allow-origin"),
        Some(origin.as_str()),
        "the launched page passes CORS: {ok:?}"
    );
    assert!(ok.json().get("result").is_some(), "{ok:?}");
    let anonymous = list_tasks_from(&addr, &origin, None);
    assert_eq!(
        anonymous.status, 401,
        "a page is never the implicit operator: {anonymous:?}"
    );
    let neighbour = list_tasks_from(&addr, &format!("http://127.0.0.1:{}", p + 1), Some(&token));
    assert_eq!(
        neighbour.status, 403,
        "another port is another origin: {neighbour:?}"
    );

    // A reload re-reads the files, and the launched origin is in none of them.
    #[cfg(feature = "hot-reload")]
    {
        unsafe { libc::kill(run.child.id() as i32, libc::SIGHUP) };
        let deadline = Instant::now() + Duration::from_secs(15);
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("\"config.reload")
        {
            assert!(Instant::now() < deadline, "no reload was logged");
            std::thread::sleep(Duration::from_millis(50));
        }
        let still = list_tasks_from(&addr, &origin, Some(&token));
        assert_eq!(
            still.status, 200,
            "the reload kept the launched origin: {still:?}"
        );
    }
    let (status, err) = run.finish(&s);
    assert!(status.success(), "{status:?}: {err}");
}

/// With `--no-open` the URL goes to the launcher's terminal — the person who
/// ran it — and nowhere else: no file, and no code, request code or token in
/// the daemon's log or its audit trail.
#[test]
fn no_open_prints_the_url_to_the_terminal_only() {
    let s = Scratch::new();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = s.write(
        "agentd.yaml",
        &config(
            port,
            "",
            "observability:\n  log_level: debug\n  audit:\n    sink: [log]\n",
        ),
    );
    let stub = s.holding_stub();
    let path = s.opener(0);
    let home = s.path("home");
    std::fs::create_dir_all(&home).unwrap();
    let log = s.path("daemon.log");
    let run = start(
        &s,
        &[
            "ui",
            "--port",
            "0",
            "--no-open",
            "--daemon-log",
            log.to_str().unwrap(),
            "--config",
            &cfg,
        ],
        &[
            ("AGENTD_UI_BIN", &stub),
            ("PATH", &path),
            ("HOME", home.to_str().unwrap()),
        ],
        false,
    );
    let terminal = s.wait_terminal("the URL on the terminal", |t| t.contains("/#launch="));
    let (code, p) = fragment_code(&terminal);
    assert!(
        terminal.contains(&format!("ssh -L {p}:127.0.0.1:{p}")),
        "a forward must keep the port: {terminal}"
    );
    assert!(
        !s.path("opener.argv").exists(),
        "--no-open opened something"
    );
    assert_eq!(
        std::fs::read_dir(&home).unwrap().count(),
        0,
        "--no-open wrote a launch file"
    );
    let origin = format!("http://127.0.0.1:{p}");
    let token = operator_token(&exchange(&addr, &code, "agentd-ui", Some(&origin)));
    // A terminal-approved request too, so its code has a chance to leak.
    let asked = form_post(
        &addr,
        "/oauth2/launch_authorization",
        "client_id=agentd-ui",
        Some(&origin),
    );
    assert_eq!(asked.status, 200, "{asked:?}");
    let request = asked.json()["request_code"].as_str().unwrap().to_string();
    let polled = form_post(
        &addr,
        "/oauth2/token",
        &format!(
            "grant_type={}&request_code={request}&client_id=agentd-ui",
            grant()
        ),
        Some(&origin),
    );
    assert_eq!(oauth_error(&polled), "authorization_pending", "{polled:?}");
    let (status, err) = run.finish(&s);
    assert!(status.success(), "{status:?}: {err}");

    let dlog = std::fs::read_to_string(&log).unwrap();
    assert!(
        dlog.contains("auth.launch.exchanged"),
        "the exchange was audited:\n{dlog}"
    );
    for secret in [
        "agentd_lc_",
        "agentd_lr_",
        "agentd_at_",
        code.as_str(),
        &request,
        &token,
    ] {
        assert!(!dlog.contains(secret), "{secret} reached the daemon log");
    }
}

/// A page the launcher never handed a code to — a second tab, a sandboxed
/// browser, an SSH forward — asks, shows a short code, and the person at the
/// launcher's terminal types it. One prompt at a time, however many ask.
#[test]
fn the_terminal_approves_a_tab_that_never_saw_the_code() {
    let s = Scratch::new();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = s.write("agentd.yaml", &config(port, "", ""));
    let stub = s.holding_stub();
    let mut run = start(
        &s,
        &["ui", "--port", "0", "--no-open", "--config", &cfg],
        &[("AGENTD_UI_BIN", &stub)],
        true,
    );
    let terminal = s.wait_terminal("the URL on the terminal", |t| t.contains("/#launch="));
    let (_, p) = fragment_code(&terminal);
    let origin = format!("http://127.0.0.1:{p}");
    const PROMPT: &str =
        "A browser tab asks to sign in to agentd: type the code it shows (Enter to skip)";
    let prompts = |t: &str| t.lines().filter(|l| l.starts_with(PROMPT)).count();
    let ask = || {
        let r = form_post(
            &addr,
            "/oauth2/launch_authorization",
            "client_id=agentd-ui",
            Some(&origin),
        );
        assert_eq!(r.status, 200, "{r:?}");
        let v = r.json();
        (
            v["request_code"].as_str().unwrap().to_string(),
            v["user_code"].as_str().unwrap().to_string(),
        )
    };
    let poll = |request: &str| {
        form_post(
            &addr,
            "/oauth2/token",
            &format!(
                "grant_type={}&request_code={request}&client_id=agentd-ui",
                grant()
            ),
            Some(&origin),
        )
    };

    // A tab asks: the terminal is prompted once.
    let (request, user_code) = ask();
    s.wait_terminal("a prompt", |t| prompts(t) == 1);
    run.type_line("WRONG-CODE");
    s.wait_terminal("the wrong code refused", |t| {
        t.contains("no tab is showing that code")
    });
    run.type_line(&user_code.to_lowercase());
    s.wait_terminal("the right code approved", |t| t.contains("signed in"));
    let token = operator_token(&poll(&request));
    assert_operator_session(&addr, &token);

    // A second tab whose code is never typed waits…
    let (waiting, _) = ask();
    s.wait_terminal("a prompt for the second tab", |t| prompts(t) == 2);
    let asked_at = Instant::now();
    let pending = poll(&waiting);
    assert_eq!(
        oauth_error(&pending),
        "authorization_pending",
        "{pending:?}"
    );
    run.type_line("");

    // …and a flood is ONE prompt that counts it, never one per request; the
    // flood displaces the oldest waiting tab, which is told to start over.
    // Sent at once, as a page reloading in a loop (or a local process) would.
    std::thread::scope(|scope| {
        for _ in 0..20 {
            scope.spawn(ask);
        }
    });
    let t = s.wait_terminal("one prompt for the flood", |t| prompts(t) == 3);
    let last = t.lines().rfind(|l| l.starts_with(PROMPT)).unwrap();
    assert!(
        last.ends_with("[15 more waiting]"),
        "the prompt counts the others: {last}"
    );
    // More tabs while that prompt is up, each on its own rather than in a
    // burst, still print nothing: one prompt at a time.
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(500));
        ask();
    }
    std::thread::sleep(Duration::from_millis(1500));
    let t2 = s.read("launcher.err");
    assert_eq!(prompts(&t2), 3, "a prompt per request:\n{t2}");
    std::thread::sleep(Duration::from_millis(2200).saturating_sub(asked_at.elapsed()));
    let displaced = poll(&waiting);
    assert_eq!(oauth_error(&displaced), "expired_token", "{displaced:?}");

    let (status, err) = run.finish(&s);
    assert!(status.success(), "{status:?}: {err}");
}

/// The web UI's code is bound to the page the launcher started: any other
/// admitted origin, or no origin at all, burns it; an origin the listener
/// does not admit is turned away before it can.
#[test]
fn the_ui_code_is_bound_to_the_launched_origin() {
    for (first, first_status) in [(Some("http://other.example"), 400), (None, 400)] {
        let s = Scratch::new();
        let port = free_port();
        let addr = format!("127.0.0.1:{port}");
        let cfg = s.write(
            "agentd.yaml",
            &config(
                port,
                "  cors:\n    origins: [\"http://other.example\"]\n",
                "",
            ),
        );
        let stub = s.holding_stub();
        let run = start(
            &s,
            &["ui", "--port", "0", "--no-open", "--config", &cfg],
            &[("AGENTD_UI_BIN", &stub)],
            false,
        );
        let terminal = s.wait_terminal("the URL on the terminal", |t| t.contains("/#launch="));
        let (code, p) = fragment_code(&terminal);
        let origin = format!("http://127.0.0.1:{p}");
        let evil = exchange(&addr, &code, "agentd-ui", Some("http://evil.example"));
        assert_eq!(evil.status, 403, "not admitted, nothing burned: {evil:?}");
        let wrong = exchange(&addr, &code, "agentd-ui", first);
        assert_eq!(
            (wrong.status, oauth_error(&wrong)),
            (first_status, "invalid_grant".into()),
            "{first:?}: {wrong:?}"
        );
        let right = exchange(&addr, &code, "agentd-ui", Some(&origin));
        assert_eq!(
            oauth_error(&right),
            "invalid_grant",
            "{first:?} burned the code: {right:?}"
        );
        let (status, err) = run.finish(&s);
        assert!(status.success(), "{status:?}: {err}");
    }
}

// ---- descriptor hygiene ------------------------------------------------------

/// The pids whose parent is `ppid` and whose command is `comm`.
#[cfg(all(
    target_os = "linux",
    feature = "exec",
    any(feature = "internal-mocks", debug_assertions)
))]
fn children_named(ppid: i32, comm: &str) -> Vec<i32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            // `pid (comm) state ppid …` — comm may hold spaces, so split at
            // its closing parenthesis.
            let Some((head, rest)) = stat.rsplit_once(')') else {
                return false;
            };
            head.ends_with(&format!("({comm}"))
                && rest.split_whitespace().nth(1) == Some(&ppid.to_string())
        })
        .collect()
}

/// A process's descriptors: number → what it points at.
#[cfg(all(
    target_os = "linux",
    feature = "exec",
    any(feature = "internal-mocks", debug_assertions)
))]
fn proc_fds(pid: i32) -> BTreeMap<u32, String> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|d| {
            d.filter_map(|e| {
                let e = e.ok()?;
                let n = e.file_name().to_str()?.parse().ok()?;
                let target = std::fs::read_link(e.path()).ok()?;
                Some((n, target.display().to_string()))
            })
            .collect()
        })
        .unwrap_or_default()
}

/// Nothing the launcher holds for its client reaches a process the daemon
/// spawns: a model-driven `exec` child has its stdio and no other descriptor
/// — not the operator's terminal, not the code's pipe, not the UI's socket —
/// and every descriptor the launcher itself holds beyond its stdio is
/// close-on-exec, so no later child can inherit one either.
///
/// The code's pipe is closed before this child is spawned, so its own
/// close-on-exec is not what this run sees: the launcher's unit test
/// `both_ends_of_the_launch_pipe_are_close_on_exec` reads it off the
/// descriptors, and the TUI tests' exact descriptor set and end-of-file read
/// fail if either end leaks into the client.
#[cfg(all(
    target_os = "linux",
    feature = "exec",
    any(feature = "internal-mocks", debug_assertions)
))]
#[test]
fn launcher_fds_never_reach_daemon_children() {
    for sub in ["tui", "ui"] {
        let s = Scratch::new();
        let port = free_port();
        let addr = format!("127.0.0.1:{port}");
        let playbook = s.write(
            "playbook.json",
            &json!({"turns": [
                {"tool_calls": [{"name": "exec", "arguments": {"cmd": "sleep", "args": ["30"]}}]},
                {"content": "slept"}
            ]})
            .to_string(),
        );
        let workdir = s.path("work");
        std::fs::create_dir_all(&workdir).unwrap();
        let cfg = s.write(
            "agentd.yaml",
            &format!(
                "\
                 agent:\n  name: launcher-fds\n  instruction: Test.\n  preflight: never\n\
                 intelligence:\n  endpoints: \"mock:file:{playbook}\"\n  model: mock\n\
                 store:\n  kind: memory\n\
                 security:\n  exec:\n    enabled: true\n    allow: [sleep]\n    workdir: {}\n    timeout: 60s\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n\
                 lifecycle:\n  run_until: drained\n  drain_timeout: 2s\n",
                workdir.display()
            ),
        );
        let stub = s.holding_stub();
        let env = [(
            if sub == "tui" {
                "AGENTD_TUI_BIN"
            } else {
                "AGENTD_UI_BIN"
            },
            stub.as_str(),
        )];
        let mut args = vec![sub];
        if sub == "ui" {
            args.extend(["--port", "0", "--no-open"]);
        }
        args.extend(["--config", &cfg]);
        let mut run = start(&s, &args, &env, true);
        s.wait_file("recorded", Duration::from_secs(20));
        let launcher = run.child.id() as i32;
        // What the client was handed on fd 3: the code's pipe, or the socket.
        let handed = s
            .client_fds()
            .get(&3)
            .cloned()
            .expect("the client had fd 3");
        let terminal_in = std::fs::read_link(format!("/proc/{launcher}/fd/0"))
            .unwrap()
            .display()
            .to_string();
        let terminal_err = s.path("launcher.err").display().to_string();

        // Every descriptor the launcher holds beyond its stdio is
        // close-on-exec: the saved terminal, and anything the daemon opened.
        for (n, target) in proc_fds(launcher).range(3..) {
            let info =
                std::fs::read_to_string(format!("/proc/{launcher}/fdinfo/{n}")).unwrap_or_default();
            let flags = info
                .lines()
                .find_map(|l| l.strip_prefix("flags:"))
                .and_then(|f| u32::from_str_radix(f.trim(), 8).ok());
            if let Some(flags) = flags {
                assert!(
                    flags & 0o2000000 != 0,
                    "{sub}: the launcher's fd {n} ({target}) is inherited by every child: {info}"
                );
            }
        }

        // A model turn that runs a long command through the exec tool.
        SendMessage::text("sleep, please")
            .return_immediately()
            .result(&addr);
        let deadline = Instant::now() + Duration::from_secs(30);
        let sleeper = loop {
            if let Some(pid) = children_named(launcher, "sleep").first() {
                break *pid;
            }
            assert!(
                Instant::now() < deadline,
                "{sub}: the exec tool never ran sleep; terminal:\n{}",
                s.read("launcher.err")
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let fds = proc_fds(sleeper);
        unsafe { libc::kill(sleeper, libc::SIGKILL) };
        assert_eq!(
            fds.keys().copied().collect::<Vec<_>>(),
            vec![0, 1, 2],
            "{sub}: the exec child holds its stdio only: {fds:?}"
        );
        for (n, target) in &fds {
            assert!(
                *target != handed && *target != terminal_in && *target != terminal_err,
                "{sub}: the exec child's fd {n} is the launcher's ({target}): {fds:?}"
            );
        }
        run.type_line("");
        let (status, err) = run.finish(&s);
        assert!(status.success(), "{sub}: {status:?}: {err}");
    }
}
