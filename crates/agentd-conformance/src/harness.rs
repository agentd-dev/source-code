// SPDX-License-Identifier: Apache-2.0
//! The black-box harness: locate + build the real `agentd` binary, then drive it
//! as a peer would — a served-MCP JSON-RPC client, a once-mode runner, the mock
//! LLM / mock MCP helpers — with no link against the agentd library.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A self-cleaning scratch directory (no `tempfile` dependency — the suite keeps
/// to just `serde_json` + `libc`).
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

/// The resolved binary paths the suite drives.
struct Bins {
    /// The default agentd (the a2a listener + the mock LLM/MCP re-exec) the
    /// checks run.
    agentd: PathBuf,
    /// The recording reference MCP server (kept for the P7 v2 client family).
    confmcp: PathBuf,
}

/// Build the binaries the suite needs once, then resolve their paths.
fn binaries() -> &'static Bins {
    static BINS: OnceLock<Bins> = OnceLock::new();
    BINS.get_or_init(|| {
        // Ensure the agentd binary (with the a2a listener + the mock LLM / mock
        // MCP the suite drives) and the reference MCP server exist, whether we
        // were invoked via `cargo test` (which builds them) or `cargo run` (which
        // may not). `internal-mocks` is implicit in a debug build but we ask for
        // it explicitly so a `--release` conformance run still ships the mocks.
        build(&[
            "build",
            "-p",
            "agentd-cli",
            "--features",
            "a2a,cron,internal-mocks",
        ]);
        build(&["build", "-p", "agentd-conformance", "--bin", "confmcp"]);
        let dir = target_dir();
        let agentd = dir.join("agentd");
        let confmcp = dir.join("confmcp");
        for (p, what) in [(&agentd, "agentd"), (&confmcp, "confmcp")] {
            assert!(p.exists(), "{what} binary not found at {}", p.display());
        }
        Bins { agentd, confmcp }
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
    confmcp: PathBuf,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Harness {
        let b = binaries();
        Harness {
            agentd: b.agentd.clone(),
            confmcp: b.confmcp.clone(),
        }
    }

    pub fn agentd(&self) -> &Path {
        &self.agentd
    }

    /// Path to the recording reference MCP server (kept for the P7 v2 client
    /// conformance family).
    pub fn confmcp(&self) -> &Path {
        &self.confmcp
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
        ConfServer::spawn_http(&self.agentd, &args, addr_file)
    }

    /// Launch `confmcp` as a Streamable HTTP MCP server (loopback TCP, announcing
    /// through the `addr_file`), recording requests to `rec` and serving resource
    /// `uri`. Blocks until announced; the guard kills it on drop. agentd dials
    /// `.endpoint()` (an `http://<addr>`).
    pub fn spawn_confmcp(&self, addr_file: &Path, rec: &Path, uri: &str) -> ConfServer {
        ConfServer::spawn_http(&self.confmcp, &[addr_file, rec, Path::new(uri)], addr_file)
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
    /// SIGTERMs it on drop (or via [`Daemon::sigterm`] / [`Daemon::wait`]).
    pub fn spawn(&self, args: &[&str]) -> Daemon {
        self.spawn_exe(&self.agentd, args)
    }

    /// Spawn an arbitrary agentd binary as a daemon, capturing nothing.
    fn spawn_exe(&self, exe: &Path, args: &[&str]) -> Daemon {
        let child = Command::new(exe)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn agentd daemon");
        Daemon { child: Some(child) }
    }
}

/// A spawned conformance MCP server serving Streamable HTTP. agentd (or a
/// probe) dials [`ConfServer::endpoint`]. Killed (and its socket/addr-file
/// removed) on drop. Two spawn shapes: `confmcp`/`workmcp` still bind a unix
/// socket ([`spawn`](ConfServer::spawn)); the built-in agentd mock binds
/// loopback TCP and announces through an addr-file
/// ([`spawn_http`](ConfServer::spawn_http)).
pub struct ConfServer {
    child: Child,
    /// The socket path (unix) or addr-file (http) — removed on drop.
    path: PathBuf,
    endpoint: String,
}

impl ConfServer {
    fn spawn_http(bin: &Path, args: &[&Path], addr_file: &Path) -> ConfServer {
        let _ = std::fs::remove_file(addr_file);
        let child = Self::launch(bin, args, addr_file);
        let addr = std::fs::read_to_string(addr_file)
            .unwrap_or_else(|e| panic!("read mock addr-file {}: {e}", addr_file.display()));
        ConfServer {
            child,
            path: addr_file.to_path_buf(),
            endpoint: format!("http://{}", addr.trim()),
        }
    }

    /// Spawn `bin` and block until `ready_path` exists (the unix socket bound /
    /// the loopback address announced).
    fn launch(bin: &Path, args: &[&Path], ready_path: &Path) -> Child {
        let child = Command::new(bin)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() {
            assert!(
                Instant::now() < deadline,
                "conformance mcp server never became ready: {}",
                ready_path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child
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

/// A spawned agentd daemon. SIGTERM on drop; [`Daemon::wait`] consumes it to
/// observe the graceful exit code.
pub struct Daemon {
    child: Option<Child>,
}

impl Daemon {
    /// Send SIGTERM (the graceful-drain signal).
    pub fn sigterm(&self) {
        if let Some(c) = &self.child {
            unsafe {
                libc::kill(c.id() as i32, libc::SIGTERM);
            }
        }
    }

    /// Wait (bounded) for exit, returning the code. SIGKILLs past `timeout`.
    pub fn wait(mut self, timeout: Duration) -> Option<i32> {
        let mut child = self.child.take().expect("alive");
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    return child.wait().ok().and_then(|s| s.code());
                }
            }
        }
    }
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
