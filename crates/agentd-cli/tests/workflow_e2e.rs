// SPDX-License-Identifier: Apache-2.0
//! End-to-end RFC 0021 §8: the MCP checkpointer — a REAL `--mode workflow` run,
//! checkpointing to a real (mock) MCP checkpointer server over HTTP, killed
//! dead (SIGKILL) mid-node, then resumed with `--workflow-resume` and driven to
//! completion. Proves: per-superstep envelopes land via `state.put`; the
//! monotonic-seq store holds them; a resumed run re-enters the in-flight node
//! (at-least-once) with the blackboard + budget carried over; and a
//! hash-mismatched resume is REFUSED (exit 5).
#![cfg(all(unix, feature = "workflow"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Start the built-in mock LLM (addr-file handshake). The workflow under test is
/// tool/assign-only, so the model is never consulted — but `--intelligence` is
/// mandatory config.
fn start_mock_llm(exe: &str, addr_file: &Path) -> (Child, String) {
    let child = Command::new(exe)
        .args(["--internal-mock-llm", addr_file.to_str().unwrap(), "final"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let addr = await_addr(addr_file);
    (child, format!("http://{addr}"))
}

/// Start the built-in mock MCP HTTP server (the RFC 0021 §8.3 checkpointer
/// profile + the crash-shaped `flaky` tool).
fn start_mock_mcp(exe: &str, addr_file: &Path) -> (Child, String) {
    let child = Command::new(exe)
        .args([
            "--internal-mock-mcp-http",
            addr_file.to_str().unwrap(),
            "mock://resource",
            "--no-emit",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-mcp");
    let addr = await_addr(addr_file);
    (child, addr)
}

fn await_addr(addr_file: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !addr_file.exists() {
        assert!(
            Instant::now() < deadline,
            "mock never announced its address"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::read_to_string(addr_file)
        .expect("read addr-file")
        .trim()
        .to_string()
}

/// One JSON-RPC POST straight at the mock MCP server (the test talks to the
/// checkpointer store directly to observe / seed envelopes).
fn mcp_rpc(addr: &str, body: &str) -> serde_json::Value {
    let mut s = TcpStream::connect(addr).expect("connect mock mcp");
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).expect("write rpc");
    let mut reader = BufReader::new(s);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("read header") == 0 {
            panic!("mock mcp closed mid-headers");
        }
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some(v) = t
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .map(str::trim)
            .map(String::from)
        {
            content_length = v.parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).expect("read body");
    serde_json::from_slice(&body).expect("parse rpc response")
}

/// `state.list` on the store → the seqs recorded under `key`.
fn stored_seqs(addr: &str, key: &str) -> Vec<u64> {
    let resp = mcp_rpc(
        addr,
        &format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"state.list","arguments":{{"key":"{key}"}}}}}}"#
        ),
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("{}");
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v["seqs"]
                .as_array()
                .map(|a| a.iter().filter_map(|s| s.as_u64()).collect())
        })
        .unwrap_or_default()
}

/// The crash-shaped workflow: seed the board, then call the mock's `flaky` tool
/// (first call hangs → the harness SIGKILLs the agent mid-node; later calls
/// succeed instantly). Checkpoints every superstep under a STABLE key.
fn crash_workflow(dir: &Path) -> String {
    let wf = serde_json::json!({
        "start": "seed",
        "checkpoint": {"server": "state", "key": "run/e2e-crash", "every": 1},
        "nodes": {
            "seed": {"kind": "assign", "value": {"n": 41}, "writes": "data",
                     "edges": {"ok": "call", "error": "fail"}},
            "call": {"kind": "tool", "server": "state", "tool": "flaky",
                     "writes": "out", "edges": {"ok": "done", "error": "fail"}},
            "done": {"kind": "halt", "status": "completed", "result_from": "data"},
            "fail": {"kind": "halt", "status": "crashed"}
        }
    });
    let path = dir.join("crash.json");
    std::fs::write(&path, wf.to_string()).expect("write workflow");
    path.to_str().unwrap().to_string()
}

fn agentd_workflow_cmd(exe: &str, wf: &str, intel: &str, mcp_addr: &str) -> Command {
    let mut c = Command::new(exe);
    c.args([
        "--mode",
        "workflow",
        "--workflow",
        wf,
        "--intelligence",
        intel,
        "--mcp",
        &format!("state=http://{mcp_addr}"),
        "--run-id",
        "e2e-crash-run",
        "--log-level",
        "warn",
        "--deadline",
        "120s",
    ]);
    c
}

#[test]
fn a_killed_workflow_resumes_from_its_checkpoint_and_completes() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().unwrap();
    let (mut llm, intel) = start_mock_llm(exe, &dir.path().join("llm.addr"));
    let (mut mcp, mcp_addr) = start_mock_mcp(exe, &dir.path().join("mcp.addr"));
    let wf = crash_workflow(dir.path());

    // RUN 1: enters `call` (the first `flaky` hangs). Wait until the envelope
    // whose cursor is the in-flight node has landed, then SIGKILL the whole
    // agent (supervisor + child — the pod-gone shape).
    let mut run1 = agentd_workflow_cmd(exe, &wf, &intel, &mcp_addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run 1");
    let deadline = Instant::now() + Duration::from_secs(45);
    while stored_seqs(&mcp_addr, "run/e2e-crash").is_empty() {
        assert!(
            Instant::now() < deadline,
            "no checkpoint landed before the deadline"
        );
        assert!(
            run1.try_wait().expect("poll run 1").is_none(),
            "run 1 exited before it could be killed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    run1.kill().expect("SIGKILL run 1"); // std kill = SIGKILL on unix
    let _ = run1.wait();

    // RUN 2: resume from the latest envelope. It re-enters `call`
    // (at-least-once for the in-flight node), `flaky` now succeeds, and the
    // halt projects the SEEDED board value — proving the blackboard carried
    // across the crash. Exit 0.
    let out = agentd_workflow_cmd(exe, &wf, &intel, &mcp_addr)
        .args(["--workflow-resume", "state:run/e2e-crash"])
        .output()
        .expect("run 2");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.code() == Some(0),
        "resume run must complete (got {:?}); stdout: {stdout}",
        out.status.code()
    );
    assert!(
        stdout.contains("\"n\": 41") || stdout.contains("\"n\":41"),
        "the resumed run's result must carry the pre-crash blackboard: {stdout}"
    );

    // The store now holds a longer, still strictly-monotonic history.
    let seqs = stored_seqs(&mcp_addr, "run/e2e-crash");
    assert!(seqs.len() >= 2, "resume appended envelopes: {seqs:?}");
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "monotonic: {seqs:?}");

    let _ = llm.kill();
    let _ = llm.wait();
    let _ = mcp.kill();
    let _ = mcp.wait();
}

#[test]
fn a_hash_mismatched_resume_is_refused() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().unwrap();
    let (mut llm, intel) = start_mock_llm(exe, &dir.path().join("llm.addr"));
    let (mut mcp, mcp_addr) = start_mock_mcp(exe, &dir.path().join("mcp.addr"));

    // Seed the store with an envelope bound to a DIFFERENT graph's hash.
    let foreign = serde_json::json!({
        "v": 1, "seq": 3, "workflow_hash": "0".repeat(64),
        "state": {"at": "call", "blackboard": {}, "visits": {}, "entry_hash": {},
                  "budget": {"max_steps": 10, "steps": 3, "max_tokens": 100, "tokens": 0}},
        "ts_ms": 0
    });
    let put = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "state.put",
                   "arguments": {"key": "run/foreign", "seq": 3, "state": foreign}}
    });
    let resp = mcp_rpc(&mcp_addr, &put.to_string());
    assert_eq!(resp["result"]["isError"], serde_json::json!(false));

    // A trivially-different workflow tries to resume that state → REFUSED
    // (exit 5), with both hashes named in the run result.
    let wf = crash_workflow(dir.path());
    let out = agentd_workflow_cmd(exe, &wf, &intel, &mcp_addr)
        .args(["--workflow-resume", "state:run/foreign"])
        .output()
        .expect("mismatch run");
    assert_eq!(
        out.status.code(),
        Some(5),
        "hash mismatch is a REFUSAL; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hash mismatch"),
        "the refusal names the mismatch: {stdout}"
    );

    let _ = llm.kill();
    let _ = llm.wait();
    let _ = mcp.kill();
    let _ = mcp.wait();
}
