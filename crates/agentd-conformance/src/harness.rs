// SPDX-License-Identifier: Apache-2.0
//! The black-box harness: locate + build the real `agentd` binary, then drive it
//! as a peer would — a served-MCP JSON-RPC client, a once-mode runner, the mock
//! LLM / mock MCP helpers — with no link against the agentd library.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
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
    /// The default agentd (serve-mcp + internal-mocks) most checks run.
    agentd: PathBuf,
    /// A `cluster`-featured agentd (adds sharding + the `--claim`/`--standby`
    /// path), built into a SEPARATE target dir so it never clobbers `agentd`.
    /// Used by the work-claim family, whose `--claim` route exits 2 on a
    /// non-cluster build (RFC 0019 §3.6 / RFC 0015 §5.6).
    agentd_cluster: PathBuf,
    /// The recording reference MCP server (MCP-client conformance).
    confmcp: PathBuf,
    /// The mock work-claim coordination server (work-claim conformance).
    workmcp: PathBuf,
}

/// Build the binaries the suite needs once, then resolve their paths.
fn binaries() -> &'static Bins {
    static BINS: OnceLock<Bins> = OnceLock::new();
    BINS.get_or_init(|| {
        // Ensure the agentd binary (with serve-mcp + the mock LLM / mock MCP the
        // suite drives) and our reference MCP servers all exist, regardless of
        // whether we were invoked via `cargo test` (which builds them) or `cargo
        // run` (which may not). `internal-mocks` is implicit in a debug build but
        // we ask for it explicitly so a `--release` conformance run still ships
        // the mock re-exec modes.
        build(&[
            "build",
            "-p",
            "agentd",
            "--features",
            "serve-mcp,internal-mocks",
        ]);
        build(&["build", "-p", "agentd-conformance", "--bin", "confmcp"]);
        build(&["build", "-p", "agentd-conformance", "--bin", "workmcp"]);
        let dir = target_dir();
        let agentd = dir.join("agentd");
        let confmcp = dir.join("confmcp");
        let workmcp = dir.join("workmcp");
        // The `cluster`-featured agentd. The `--claim` path (RFC 0019 §3) is
        // `cluster`-gated, so the work-claim e2e check NEEDS a cluster build — a
        // default build exits 2 on `--claim`. We build it with the SAME feature
        // set as the default (serve-mcp + internal-mocks) PLUS `cluster`, into a
        // dedicated `--target-dir` so the resulting `agentd` does not overwrite
        // the default one (both are named `agentd`). The dir is a sibling of the
        // shared target so it inherits the same toolchain/deps cache.
        let cluster_target = cluster_target_dir(&dir);
        build_with_target_dir(
            &[
                "build",
                "-p",
                "agentd",
                "--features",
                "serve-mcp,internal-mocks,cluster",
            ],
            &cluster_target,
        );
        let agentd_cluster = cluster_target.join("debug").join("agentd");
        for (p, what) in [
            (&agentd, "agentd"),
            (&agentd_cluster, "agentd (cluster)"),
            (&confmcp, "confmcp"),
            (&workmcp, "workmcp"),
        ] {
            assert!(p.exists(), "{what} binary not found at {}", p.display());
        }
        Bins {
            agentd,
            agentd_cluster,
            confmcp,
            workmcp,
        }
    })
}

/// Where the `cluster`-featured agentd is built: `<target>/conf-cluster/` (its
/// own `--target-dir`, so the cluster `agentd` never clobbers the default one).
fn cluster_target_dir(shared_target: &Path) -> PathBuf {
    shared_target.join("conf-cluster")
}

fn build(args: &[&str]) {
    let status = Command::new(env!("CARGO"))
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo {args:?}: {e}"));
    assert!(status.success(), "cargo {args:?} failed");
}

/// Build into a dedicated `--target-dir` (so a differently-featured variant of a
/// same-named binary doesn't overwrite the shared one).
fn build_with_target_dir(args: &[&str], target_dir: &Path) {
    let status = Command::new(env!("CARGO"))
        .args(args)
        .arg("--target-dir")
        .arg(target_dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo {args:?}: {e}"));
    assert!(status.success(), "cargo {args:?} (cluster) failed");
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
    agentd_cluster: PathBuf,
    confmcp: PathBuf,
    workmcp: PathBuf,
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
            agentd_cluster: b.agentd_cluster.clone(),
            confmcp: b.confmcp.clone(),
            workmcp: b.workmcp.clone(),
        }
    }

    pub fn agentd(&self) -> &Path {
        &self.agentd
    }

    /// Path to a `cluster`-featured agentd (sharding + the `--claim`/`--standby`
    /// path). The default `agentd()` exits 2 on `--claim`; the work-claim e2e
    /// check spawns THIS one instead.
    pub fn agentd_cluster(&self) -> &Path {
        &self.agentd_cluster
    }

    /// Path to the recording reference MCP server (for client conformance).
    pub fn confmcp(&self) -> &Path {
        &self.confmcp
    }

    /// Path to the mock work-claim coordination server (for work-claim
    /// conformance — the frozen `work.*` contract with atomic single-grant).
    pub fn workmcp(&self) -> &Path {
        &self.workmcp
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

    /// Launch `confmcp` as a Streamable HTTP MCP server on `socket`, recording
    /// requests to `rec` and serving resource `uri`. Blocks until the socket
    /// binds; the returned guard kills it on drop. agentd dials `.endpoint()`.
    pub fn spawn_confmcp(&self, socket: &Path, rec: &Path, uri: &str) -> ConfServer {
        ConfServer::spawn(&self.confmcp, &[socket, rec, Path::new(uri)], socket)
    }

    /// Launch `workmcp` as a Streamable HTTP MCP server on `socket`, backed by the
    /// shared lease `state` file and serving item `uri`. Blocks until the socket
    /// binds; the returned guard kills it on drop.
    pub fn spawn_workmcp(&self, socket: &Path, state: &Path, uri: &str) -> ConfServer {
        ConfServer::spawn(&self.workmcp, &[socket, state, Path::new(uri)], socket)
    }

    pub fn tempdir(&self) -> TempDir {
        TempDir::new()
    }

    /// Run agentd to completion with `args`; capture the exit code + streams.
    pub fn run(&self, args: &[&str]) -> RunResult {
        let out = Command::new(&self.agentd)
            .args(args)
            .output()
            .expect("spawn agentd");
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

    /// Spawn the `cluster`-featured agentd as a daemon (for the `--claim` /
    /// `--standby` paths the default build rejects with exit 2).
    pub fn spawn_cluster(&self, args: &[&str]) -> Daemon {
        self.spawn_exe(&self.agentd_cluster, args)
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

    /// Start an idle daemon that serves MCP on a fresh unix socket and does
    /// nothing else (reactive, subscribed to a URI no server owns). Returns a
    /// connected, initialized JSON-RPC client.
    pub fn serve(&self) -> Served {
        let tmp = TempDir::new();
        let sock = tmp.path().join("agentd.sock");
        let child = Command::new(&self.agentd)
            .args([
                "--mode",
                "reactive",
                "--subscribe",
                "file:///noop",
                "--instruction",
                "stand by",
                "--intelligence",
                "unix:/nonexistent/agentd-conf.sock",
                "--serve-mcp",
            ])
            .arg(format!("unix:{}", sock.display()))
            .args(["--log-level", "warn"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn served daemon");
        let client = Client::connect(&sock, Duration::from_secs(5));
        Served {
            child,
            client,
            _tmp: tmp,
        }
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
    fn spawn(bin: &Path, args: &[&Path], socket: &Path) -> ConfServer {
        let _ = std::fs::remove_file(socket);
        let child = Self::launch(bin, args, socket);
        ConfServer {
            child,
            path: socket.to_path_buf(),
            endpoint: format!("unix:{}", socket.display()),
        }
    }

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

/// A running served daemon + its client; SIGTERM'd on drop.
pub struct Served {
    child: Child,
    pub client: Client,
    _tmp: TempDir,
}

impl Served {
    pub fn client(&mut self) -> &mut Client {
        &mut self.client
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        // Bounded wait so a wedged daemon can't hang the suite.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A line-delimited JSON-RPC client over a unix socket. Built around raw JSON so
/// it never agrees with agentd's own codec — a conformance checker, not a peer.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    id: i64,
}

impl Client {
    fn connect(sock: &Path, timeout: Duration) -> Client {
        let deadline = Instant::now() + timeout;
        let stream = loop {
            if let Ok(s) = UnixStream::connect(sock) {
                break s;
            }
            assert!(
                Instant::now() < deadline,
                "served socket never connectable: {}",
                sock.display()
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        // A read timeout so notification / no-response checks can't block forever.
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set read timeout");
        let reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut c = Client {
            reader,
            writer: stream,
            id: 0,
        };
        // Every served session opens with the MCP handshake.
        let _ = c.call("initialize", json!({}));
        c
    }

    /// The id that the next [`Client::call`] will use.
    pub fn next_id(&self) -> i64 {
        self.id + 1
    }

    /// Send a JSON-RPC request and return the parsed response object.
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let line = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
            .to_string();
        self.send(&line);
        self.read_value()
            .unwrap_or_else(|| panic!("no response to {method}"))
    }

    /// Send a raw line verbatim (for malformed-input / framing checks) and return
    /// the next response line if one arrives within the read timeout.
    pub fn raw(&mut self, line: &str) -> Option<Value> {
        self.send(line);
        self.read_value()
    }

    fn send(&mut self, line: &str) {
        writeln!(self.writer, "{line}").expect("write line");
        self.writer.flush().ok();
    }

    fn read_value(&mut self) -> Option<Value> {
        let mut buf = String::new();
        match self.reader.read_line(&mut buf) {
            Ok(0) => None, // EOF
            Ok(_) => serde_json::from_str(&buf).ok(),
            Err(_) => None, // timeout / would-block
        }
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
