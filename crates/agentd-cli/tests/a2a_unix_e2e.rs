// SPDX-License-Identifier: AGPL-3.0-only
//! Cross-instance connection over a **unix domain socket**: two real agentd
//! daemons, one listening on `a2a.listen: unix://…`, the other declaring it as
//! a peer and delegating work to it — the co-located fast lane. Same A2A
//! protocol, no TCP, no TLS; the kernel (SO_PEERCRED, same uid) and the socket
//! file's 0600 mode are the authenticators.
#![cfg(all(unix, feature = "a2a"))]

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
            "\
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

    // The socket is published by renaming it out of a 0700 staging directory,
    // so it is never briefly world-accessible. Nothing of that staging may
    // survive: a leaked 0700 directory would be a private hole in a shared
    // runtime dir, and a leaked socket would be a second bindable endpoint.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().starts_with(".agentd-sock-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "socket staging left something behind: {leftovers:?}"
    );

    // A: declares B as a peer BY SOCKET PATH and delegates to it from a
    // workflow — no model of its own needed.
    let cfg_a = format!("{dir}/a.yaml");
    std::fs::write(
        &cfg_a,
        format!(
            "\
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

/// One HTTP POST over the unix socket at `sock`, read to the end.
fn unix_post(sock: &str, body: &str, extra: &[(&str, &str)]) -> (u16, Value) {
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(sock)
        .unwrap_or_else(|e| panic!("connect {sock}: {e}"));
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nA2A-Version: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        common::A2A_VERSION,
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok();
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let v = serde_json::from_str(body).unwrap_or_else(|e| panic!("non-JSON ({e}): {raw}"));
    (status, v)
}

/// A unix-socket peer of the daemon's own uid is the operator, even where
/// principal rules exist.
///
/// The kernel already named the caller: `SO_PEERCRED` let only the daemon's
/// own uid (or root) through the accept, and the socket file is 0600. Rules
/// written for the network — here a bearer the TCP world would need — do not
/// demote the person who owns the process.
#[test]
fn a_same_uid_unix_caller_is_operator_even_with_principals() {
    let dir = common::unique_path("agentd-uds-op", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = format!("{dir}/op.sock");
    let cfg = format!("{dir}/op.yaml");
    std::fs::write(
        &cfg,
        format!(
            "\
             agent:\n  name: uds-op\n  instruction: Test.\n  preflight: never\n\
             intelligence:\n  endpoints: https://127.0.0.1:9\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: \"unix://{sock}\"\n\
             \x20 principals:\n\
             \x20   - id: ci\n\
             \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_UDS_CI}}}}\" }}\n\
             \x20     role: user\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n"
        ),
    )
    .unwrap();
    let stderr_path = common::unique_path("uds-op-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_UDS_CI", "ci-secret")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn daemon");
    let d = Daemon { child, stderr_path };
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(&sock).is_err() {
        assert!(
            Instant::now() < deadline,
            "never bound its socket:\n{}",
            d.stderr()
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // `config` is an operator-floor op: no grant reaches it, only the role.
    let command = common::SendMessage::command("config", json!({}));
    let headers = command.headers();
    let extra: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (status, v) = unix_post(&sock, &command.body(1), &extra);
    assert_eq!(status, 200, "{v}\n{}", d.stderr());
    assert!(
        v.get("error").is_none(),
        "the socket's owner is the operator: {v}"
    );
    // Even a junk bearer does not demote it: the uid decided before any
    // header was read.
    let mut junk = extra.clone();
    junk.push(("Authorization", "Bearer not-the-ci-secret"));
    let (status, v) = unix_post(&sock, &command.body(2), &junk);
    assert_eq!((status, v.get("error")), (200, None), "{v}");

    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}
