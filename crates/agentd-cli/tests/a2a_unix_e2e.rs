// SPDX-License-Identifier: AGPL-3.0-only
//! Cross-instance connection over a **unix domain socket**: two real agentd
//! daemons, one listening on `a2a.listen: unix://…`, the other declaring it as
//! a peer and delegating work to it — the co-located fast lane. Same A2A
//! protocol, no TCP, no TLS; the kernel (SO_PEERCRED, same uid) and the socket
//! file's 0600 mode are the authenticators.
#![cfg(all(unix, feature = "a2a", feature = "workflow"))]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct MockLlm {
    child: Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}
fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("uds-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("uds-mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn spawn_daemon(config: &str) -> Daemon {
    let stderr_path = common::unique_path("uds-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn daemon");
    Daemon { child, stderr_path }
}

#[test]
fn two_instances_connect_and_delegate_over_a_unix_socket() {
    let dir = common::unique_path("agentd-uds", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = format!("{dir}/b.sock");

    // B: listens on the socket; an inbound delegation becomes a turn its
    // (mock) model answers.
    let llm_b = spawn_mock_llm(&json!({"turns": [{"content": "PONG_FROM_B"}]}));
    let cfg_b = format!("{dir}/b.yaml");
    std::fs::write(
        &cfg_b,
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: bee\n  instruction: You are B; answer briefly.\n  preflight: never\n\
             intelligence:\n  endpoints: {}\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: \"unix://{sock}\"\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n  log_content: true\n",
            llm_b.uri
        ),
    )
    .unwrap();
    let b = spawn_daemon(&cfg_b);

    // The socket appears, mode 0600 — the filesystem gate.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(meta) = std::fs::metadata(&sock) {
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "the socket is owner-only"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B never bound its socket:\n{}",
            b.stderr()
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // A: declares B as a peer BY SOCKET PATH and delegates to it from a
    // workflow — no model of its own needed.
    let cfg_a = format!("{dir}/a.yaml");
    std::fs::write(
        &cfg_a,
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: aye\n\
             a2a:\n  peers:\n    - name: bee\n      endpoint: \"unix://{sock}\"\n\
             workflows:\n  - name: ask\n    steps:\n\
             \x20     start: {{kind: once}}\n\
             \x20     del: {{kind: a2a.delegate, depends_on: [start], peer: bee, objective: \"say pong\", timeout: 30s}}\n\
             \x20     done: {{kind: finish, depends_on: [del], status: completed, output: \"{{{{steps.del.output}}}}\"}}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 1s\n\
             observability:\n  log_level: info\n  log_content: true\n"
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_a])
        .stdin(Stdio::null())
        .output()
        .expect("run A");
    let stderr_a = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "A stderr:\n{stderr_a}\n\nB stderr:\n{}",
        b.stderr()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("PONG_FROM_B"),
        "B's answer crossed the socket into A's run output: {stdout}\nB:\n{}",
        b.stderr()
    );
    // And B really served it over the unix listener (bound = unix:<path>).
    assert!(
        b.stderr().contains(&format!("unix:{sock}")),
        "B logged its unix bind:\n{}",
        b.stderr()
    );

    drop(b);
    let _ = std::fs::remove_dir_all(&dir);
}
