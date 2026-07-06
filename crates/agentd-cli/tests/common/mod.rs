// SPDX-License-Identifier: Apache-2.0
//! Shared E2E harness: launch the built-in **HTTP** mock MCP server as a
//! subprocess and hand agentd its loopback-TCP endpoint. The mock binds
//! `127.0.0.1:0` and announces the bound address through an **addr-file**
//! (`agentd::announce_addr`); the harness waits for the file, reads the
//! address, and dials `http://<addr>`.
#![allow(dead_code)] // each test file uses a different subset of these helpers.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A running mock HTTP MCP server. Killed (and its addr-file removed) on drop.
pub struct MockMcp {
    child: Child,
    addr_file: String,
    addr: String,
}

impl MockMcp {
    /// The bare `http://<addr>` endpoint agentd dials.
    pub fn uri(&self) -> String {
        format!("http://{}", self.addr)
    }
    /// The `--mcp` argument value: `name=http://<addr>`.
    pub fn mcp_arg(&self, name: &str) -> String {
        format!("{name}=http://{}", self.addr)
    }
}

impl Drop for MockMcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}

/// Launch the mock HTTP MCP server serving one resource at `uri`. `emit` controls
/// the post-subscribe `resources/updated` push on the GET SSE stream. Blocks
/// until the mock has bound and announced its address (so agentd can connect
/// immediately).
pub fn spawn_mock_mcp(uri: &str, emit: bool) -> MockMcp {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let addr_file = unique_path("mock-mcp", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let mut args = vec![
        "--internal-mock-mcp-http".to_string(),
        addr_file.clone(),
        uri.to_string(),
    ];
    if !emit {
        args.push("--no-emit".to_string());
    }
    let child = Command::new(exe)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock http mcp");
    let addr = read_addr_file(&addr_file);
    MockMcp {
        child,
        addr_file,
        addr,
    }
}

/// A unique path under the temp dir (per-process + per-call), for addr-files
/// and other per-test artifacts.
pub fn unique_path(tag: &str, ext: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir();
    dir.join(format!("agentd-{tag}-{}-{n}.{ext}", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// Block until `path` exists (the mock has bound + announced), then return the
/// `host:port` address it carries.
pub fn read_addr_file(path: &str) -> String {
    wait_for_file(path);
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read mock addr-file {path}: {e}"))
        .trim()
        .to_string()
}

/// Block until `path` exists (bounded).
pub fn wait_for_file(path: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new(path).exists() {
        assert!(
            Instant::now() < deadline,
            "mock addr-file never appeared: {path}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
