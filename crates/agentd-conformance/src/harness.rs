// SPDX-License-Identifier: AGPL-3.0-only
//! The black-box harness: locate + build the real `agentd` binary, then drive it
//! as a peer would — a one-shot runner, a daemon, the mock LLM / mock MCP
//! helpers — with no link against the agentd library.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A self-cleaning scratch directory (no `tempfile` dependency — the suite
/// keeps to crates the workspace already builds).
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "agentd-conf-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Build the agentd binary the suite drives once, then resolve its path.
fn binary() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        // Ensure the agentd binary (with the a2a listener + the mock LLM / mock
        // MCP the suite drives) exists, whether we were invoked via `cargo test`
        // (which builds it) or `cargo run` (which may not). `internal-mocks` is
        // implicit in a debug build but we ask for it explicitly so a
        // `--release` conformance run still ships the mocks. `oauth` is the
        // interactive login the `mcp-auth-core` checks drive, and what the
        // fake Company needs to verify ES256; `companies` is agentd as the
        // Personal Agent the `companies` family holds to the draft; `xaa`
        // is agentd acting for an enrolled person, which the `ema` family
        // drives against its mock IdP, authorization server and MCP server;
        // `hot-reload` is the SIGHUP reload its template checks send; `exec`
        // the local command its check runs beside a delegation, as root;
        // `x402` agentd paying, which the `payments` family holds to x402
        // and ADR 0003 against its mock resource server.
        build(&[
            "build",
            "-p",
            "agentd-cli",
            "--features",
            "a2a,cron,internal-mocks,oauth,companies,xaa,hot-reload,exec,x402",
        ]);
        let agentd = target_dir().join("agentd");
        assert!(
            agentd.exists(),
            "agentd binary not found at {}",
            agentd.display()
        );
        agentd
    })
}

fn build(args: &[&str]) {
    let status = Command::new(env!("CARGO"))
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo {args:?}: {e}"));
    assert!(status.success(), "cargo {args:?} failed");
}

/// The `target/<profile>/` dir, derived from our own executable's location
/// (`.../target/<profile>/[deps/]<exe>`).
fn target_dir() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // drop the exe file
    if p.ends_with("deps") {
        p.pop();
    }
    p
}

/// The harness: holds the resolved binary paths. Cheap to clone-by-reference;
/// every spawn gets its own temp dir + sockets so checks never collide.
pub struct Harness {
    agentd: PathBuf,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Harness {
        Harness {
            agentd: binary().clone(),
        }
    }

    pub fn agentd(&self) -> &Path {
        &self.agentd
    }

    /// The same harness driving the agentd binary at `agentd` — a copy of
    /// the built one where another uid must run it.
    pub fn at(agentd: PathBuf) -> Harness {
        Harness { agentd }
    }

    /// Launch the built-in agentd Streamable HTTP mock MCP server, serving one
    /// resource at `uri` (`emit` pushes a resources/updated after subscribe). The
    /// mock binds loopback TCP and announces its address through `addr_file`;
    /// blocks until announced. The guard kills it on drop.
    pub fn spawn_mock_mcp(&self, addr_file: &Path, uri: &str, emit: bool) -> ConfServer {
        let mut args: Vec<&Path> = vec![
            Path::new("--internal-mock-mcp-http"),
            addr_file,
            Path::new(uri),
        ];
        if !emit {
            args.push(Path::new("--no-emit"));
        }
        ConfServer::spawn_http(&self.agentd, &args, addr_file, "http")
    }

    /// [`Harness::spawn_mock_mcp`] at the MCP revision `revision`
    /// (`2026-07-28` reaches it with `server/discover`, where the mock serves
    /// the MCP Tasks extension), its stderr — the server's own record of each
    /// request that reached it — written to `log`.
    pub fn spawn_mock_mcp_at(
        &self,
        addr_file: &Path,
        uri: &str,
        revision: &str,
        log: &Path,
    ) -> ConfServer {
        self.spawn_mock_mcp_with(addr_file, uri, revision, log, &[])
    }

    /// [`Harness::spawn_mock_mcp_at`], with the mock's further `options`
    /// (`--prm <issuer>`, …).
    pub fn spawn_mock_mcp_with(
        &self,
        addr_file: &Path,
        uri: &str,
        revision: &str,
        log: &Path,
        options: &[&str],
    ) -> ConfServer {
        let mut args: Vec<&Path> = vec![
            Path::new("--internal-mock-mcp-http"),
            addr_file,
            Path::new(uri),
            Path::new("--no-emit"),
            Path::new("--protocol-version"),
            Path::new(revision),
        ];
        args.extend(options.iter().map(Path::new));
        let _ = std::fs::remove_file(addr_file);
        let child = Command::new(&self.agentd)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log).expect("a log file for the mock"))
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", self.agentd.display()));
        ConfServer::announced(child, addr_file, "http")
    }

    /// Launch the built-in fake Company (`--internal-mock-company`): HTTPS on
    /// loopback with the test PKI's server identity ([`tls_fixture`]),
    /// trusting the test CA for the documents it fetches, and departing from
    /// the draft as each of `variants` names. Blocks until announced; the
    /// guard kills it on drop. Its endpoint is `https://127.0.0.1:<port>`.
    pub fn spawn_company(&self, addr_file: &Path, variants: &[&str]) -> ConfServer {
        let (cert, key, ca) = (
            tls_fixture("server.pem"),
            tls_fixture("server.key"),
            tls_fixture("ca.pem"),
        );
        let mut args: Vec<&Path> = vec![
            Path::new("--internal-mock-company"),
            addr_file,
            Path::new("--cert"),
            &cert,
            Path::new("--key"),
            &key,
            Path::new("--ca"),
            &ca,
        ];
        for v in variants {
            args.push(Path::new("--variant"));
            args.push(Path::new(v));
        }
        ConfServer::spawn_http(&self.agentd, &args, addr_file, "https")
    }

    pub fn tempdir(&self) -> TempDir {
        TempDir::new()
    }

    /// Run agentd to completion with `args`; capture the exit code + streams.
    pub fn run(&self, args: &[&str]) -> RunResult {
        self.run_env(args, &[])
    }

    /// Like [`Harness::run`], but with extra environment variables — the durable
    /// test hooks (`AGENTD_TEST_KILL_AT` for a SIGKILL at a kill point,
    /// `AGENTD_TEST_INBOX_FILE` to seed the durable inbox) the chaos/restore
    /// conformance drives.
    pub fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> RunResult {
        let mut cmd = Command::new(&self.agentd);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("spawn agentd");
        RunResult {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Start the built-in mock LLM on loopback TCP (intelligence endpoint),
    /// discovering the bound address through a fresh addr-file.
    pub fn mock_llm(&self, script: &str) -> MockLlm {
        let tmp = TempDir::new();
        let addr_file = tmp.path().join("llm.addr");
        let child = Command::new(&self.agentd)
            .args(["--internal-mock-llm", addr_file.to_str().unwrap(), script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mock-llm");
        wait_for(&addr_file, Duration::from_secs(5));
        let addr = std::fs::read_to_string(&addr_file).expect("read mock-llm addr-file");
        MockLlm {
            child,
            uri: format!("http://{}", addr.trim()),
            _tmp: tmp,
        }
    }

    /// Spawn agentd as a long-lived daemon with `args`; returns a guard that
    /// SIGTERMs it on drop.
    pub fn spawn(&self, args: &[&str]) -> Daemon {
        self.spawn_env(args, &[])
    }

    /// Like [`Harness::spawn`], its stderr — agentd's log — written to `log`.
    pub fn spawn_logged(&self, args: &[&str], log: &Path) -> Daemon {
        self.spawn_logged_env(args, &[], log)
    }

    /// Like [`Harness::spawn_logged`], with extra environment variables — a
    /// debug build's test hooks among them.
    pub fn spawn_logged_env(&self, args: &[&str], env: &[(&str, &str)], log: &Path) -> Daemon {
        let mut cmd = Command::new(&self.agentd);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log).expect("a log file for the daemon"))
            .spawn()
            .expect("spawn agentd daemon");
        Daemon { child: Some(child) }
    }

    /// Like [`Harness::spawn`], with extra environment variables — how a check
    /// hands the daemon a credential its config names as `{{secret:NAME}}`,
    /// since agentd refuses one written inline.
    pub fn spawn_env(&self, args: &[&str], env: &[(&str, &str)]) -> Daemon {
        let mut cmd = Command::new(&self.agentd);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn agentd daemon");
        Daemon { child: Some(child) }
    }
}

/// A spawned conformance MCP server serving Streamable HTTP. agentd (or a
/// probe) dials [`ConfServer::endpoint`]. Killed (and its addr-file removed) on
/// drop, so a failed check leaks neither a process nor a stale address file.
/// The server binds loopback TCP on an ephemeral port and announces the address
/// it actually got through an addr-file, which the spawner blocks on: a fixed
/// port would make two concurrent checks collide.
pub struct ConfServer {
    child: Child,
    /// The socket path (unix) or addr-file (http) — removed on drop.
    path: PathBuf,
    endpoint: String,
}

impl ConfServer {
    fn spawn_http(bin: &Path, args: &[&Path], addr_file: &Path, scheme: &str) -> ConfServer {
        let _ = std::fs::remove_file(addr_file);
        let child = Command::new(bin)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
        Self::announced(child, addr_file, scheme)
    }

    /// Block until `child` announces its address in `addr_file`, and hold it.
    fn announced(child: Child, addr_file: &Path, scheme: &str) -> ConfServer {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !addr_file.exists() {
            assert!(
                Instant::now() < deadline,
                "conformance mcp server never became ready: {}",
                addr_file.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let addr = std::fs::read_to_string(addr_file)
            .unwrap_or_else(|e| panic!("read mock addr-file {}: {e}", addr_file.display()));
        ConfServer {
            child,
            path: addr_file.to_path_buf(),
            endpoint: format!("{scheme}://{}", addr.trim()),
        }
    }

    /// The endpoint agentd connects to (`unix:<socket>` or `http://<addr>`).
    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }
}

impl Drop for ConfServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A spawned agentd daemon, SIGTERMed (then killed) on drop.
pub struct Daemon {
    child: Option<Child>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A mock LLM (intelligence) endpoint; killed on drop.
pub struct MockLlm {
    child: Child,
    pub uri: String,
    _tmp: TempDir,
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A captured once-mode run.
pub struct RunResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl RunResult {
    /// Parse the stderr JSON-lines telemetry into events (best-effort: skips
    /// non-JSON lines).
    pub fn events(&self) -> Vec<Value> {
        self.stderr
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    /// Whether any telemetry event has `event == name`.
    pub fn saw_event(&self, name: &str) -> bool {
        self.events().iter().any(|e| e["event"] == name)
    }
}

/// A file of the workspace's test PKI (`crates/net/tests/fixtures`): a CA and
/// a server identity for `localhost`, `127.0.0.1` and `::1`, committed test
/// data that protects nothing.
pub fn tls_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../net/tests/fixtures")
        .join(name)
}

/// Block until `path` exists (a socket has bound), or panic past `timeout`.
fn wait_for(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
