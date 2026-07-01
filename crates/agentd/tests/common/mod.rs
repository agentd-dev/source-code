// SPDX-License-Identifier: Apache-2.0
//! Shared E2E harness: launch the built-in **HTTP** mock MCP server as a
//! subprocess and hand agentd its unix-socket endpoint (v2.0.0). Replaces the
//! old stdio mock, which agentd spawned as a child; the HTTP transport connects
//! to a separately-running server, so the harness owns the mock's lifecycle.
#![allow(dead_code)] // each test file uses a different subset of these helpers.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A running mock HTTP MCP server. Killed and its socket removed on drop.
pub struct MockMcp {
    child: Child,
    socket: String,
}

impl MockMcp {
    /// The bare `unix:<socket>` endpoint agentd dials.
    pub fn uri(&self) -> String {
        format!("unix:{}", self.socket)
    }
    /// The `--mcp` argument value: `name=unix:<socket>`.
    pub fn mcp_arg(&self, name: &str) -> String {
        format!("{name}=unix:{}", self.socket)
    }
}

impl Drop for MockMcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Launch the mock HTTP MCP server serving one resource at `uri`. `emit` controls
/// the post-subscribe `resources/updated` push on the GET SSE stream. Blocks
/// until the socket is bound (so agentd can connect immediately).
pub fn spawn_mock_mcp(uri: &str, emit: bool) -> MockMcp {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let socket = unique_socket("mock-mcp");
    let _ = std::fs::remove_file(&socket);
    let mut args = vec![
        "--internal-mock-mcp-http".to_string(),
        socket.clone(),
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
    wait_for_socket(&socket);
    MockMcp { child, socket }
}

/// A unique unix-socket path under the temp dir (per-process + per-call).
pub fn unique_socket(tag: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir();
    dir.join(format!("agentd-{tag}-{}-{n}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// Block until `socket` exists (the mock has bound its listener).
pub fn wait_for_socket(socket: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new(socket).exists() {
        assert!(
            Instant::now() < deadline,
            "mock mcp socket never bound: {socket}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
