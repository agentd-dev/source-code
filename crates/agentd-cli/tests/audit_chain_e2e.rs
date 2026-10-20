// SPDX-License-Identifier: AGPL-3.0-only
//! The hash-chained audit log (RFC 0044 P2a, T32): with
//! `observability.audit.chain` set, every A2A call's audit record is a line
//! of the chain, carrying the SHA-256 of the line before it; a restart
//! verifies the chain and goes on with it; and a chain with a record
//! changed, or its tail cut behind the head the store keeps, refuses the
//! start, naming the line.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::SendMessage;

struct Daemon {
    child: Child,
    stderr_path: String,
}

impl Daemon {
    /// Drain it and wait for it to go.
    fn stop(mut self) {
        // SAFETY: a signal to the child this test spawned.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

fn config(dir: &str, port: u16) -> String {
    format!(
        "agent: {{ name: chain, preflight: never }}\n\
         intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
         store: {{ kind: file, file: {{ path: \"{dir}/state\" }} }}\n\
         a2a: {{ listen: \"http://127.0.0.1:{port}\" }}\n\
         lifecycle: {{ run_until: drained }}\n\
         observability: {{ log_level: info, audit: {{ chain: {{ path: \"{dir}/audit/audit.log\" }} }} }}\n"
    )
}

/// The daemon, ready, and the A2A authority it bound.
fn start(dir: &str) -> (Daemon, String) {
    let cfg = format!("{dir}/c.yaml");
    common::spawn_bound(|port| {
        std::fs::write(&cfg, config(dir, port)).unwrap();
        let stderr_path = common::unique_path("chain-daemon", "log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
            .spawn()
            .expect("spawn agentd");
        (
            Daemon {
                child,
                stderr_path: stderr_path.clone(),
            },
            stderr_path,
        )
    })
}

/// A start that must be refused: its exit code and log. A daemon that
/// starts instead is stopped after 20 s, and its code is `None`.
fn refused(dir: &str) -> (Option<i32>, String) {
    let log = format!("{dir}/refused.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &format!("{dir}/c.yaml")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&log).unwrap()))
        .spawn()
        .expect("run");
    let deadline = Instant::now() + Duration::from_secs(20);
    let code = loop {
        if let Ok(Some(s)) = child.try_wait() {
            break s.code();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (code, std::fs::read_to_string(&log).unwrap_or_default())
}

#[test]
fn a2a_calls_are_chained_and_a_tampered_chain_refuses_the_start() {
    let dir = common::unique_path("audit-chain", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let chain = format!("{dir}/audit/audit.log");
    let (d, addr) = start(&dir);
    for _ in 0..2 {
        let _ = SendMessage::command("status", json!({})).result(&addr);
    }
    d.stop();

    // Every line carries the hash of the one before, from the genesis.
    let text = std::fs::read_to_string(&chain).expect("the chain was written");
    let lines: Vec<&str> = text.lines().collect();
    let records: Vec<Value> = lines
        .iter()
        .map(|l| serde_json::from_str(l).expect("a JSON record"))
        .collect();
    assert_eq!(records[0]["action"], "audit.chain.segment", "{text}");
    assert_eq!(records[0]["prev"], "0".repeat(64));
    for (i, w) in lines.windows(2).enumerate() {
        assert_eq!(
            records[i + 1]["prev"],
            agentd::sha::sha256_hex(w[0].as_bytes()),
            "record {} does not chain to the one before:\n{text}",
            i + 2
        );
        assert_eq!(records[i + 1]["seq"], json!(i + 2));
    }
    let calls = records
        .iter()
        .filter(|r| {
            r["action"]
                .as_str()
                .is_some_and(|a| a.starts_with("a2a.SendMessage") && a.contains("status"))
        })
        .count();
    assert!(calls >= 2, "each status call is a record:\n{text}");

    // A restart verifies it and goes on with it.
    let (d, addr) = start(&dir);
    let _ = SendMessage::command("status", json!({})).result(&addr);
    let log = std::fs::read_to_string(&d.stderr_path).unwrap_or_default();
    assert!(log.contains("\"event\":\"audit.chain.open\""), "{log}");
    d.stop();
    let good = std::fs::read_to_string(&chain).unwrap();
    assert!(good.lines().count() > lines.len(), "the restart appended");

    // One record changed: the start is refused at the record after it.
    let edited = good.replacen("\"outcome\":\"ok\"", "\"outcome\":\"no\"", 2);
    std::fs::write(&chain, &edited).unwrap();
    let (code, log) = refused(&dir);
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("audit chain broken") && log.contains("a record was changed"),
        "{log}"
    );

    // The last record cut off while agentd was down: the head the store
    // kept at the clean stop names it, so the start says so.
    let kept: Vec<&str> = good.lines().collect();
    let cut = kept[..kept.len() - 1].join("\n") + "\n";
    std::fs::write(&chain, &cut).unwrap();
    let (code, log) = refused(&dir);
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("its tail was cut"), "{log}");

    // Put back, it opens again.
    std::fs::write(&chain, &good).unwrap();
    let (d, _) = start(&dir);
    d.stop();

    // The recovery docs/security.md gives a broken chain: moved aside alone
    // it is a chain cut to nothing; with the store's head of it removed
    // too, the next start begins a new chain from the genesis.
    std::fs::write(&chain, &edited).unwrap();
    std::fs::rename(&chain, format!("{dir}/audit/evidence.log")).unwrap();
    let (code, log) = refused(&dir);
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("its tail was cut"), "{log}");
    std::fs::remove_file(format!("{dir}/state/agentd/chain/audit/chain-head.json"))
        .expect("the store's head of the chain is where the docs say");
    let (d, _) = start(&dir);
    d.stop();
    let fresh = std::fs::read_to_string(&chain).unwrap();
    let first: Value = serde_json::from_str(fresh.lines().next().unwrap()).unwrap();
    assert_eq!(
        (first["seq"].clone(), first["prev"].clone()),
        (json!(1), json!("0".repeat(64)))
    );
    let _ = std::fs::remove_dir_all(&dir);
}
