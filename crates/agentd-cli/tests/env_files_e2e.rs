// SPDX-License-Identifier: AGPL-3.0-only
//! `--env <FILE>` end to end: values from dotenv files reach every consumer —
//! `${VAR}` expansion in the config, `{{secret:NAME}}` resolution — with the
//! documented precedence (real environment beats any file, later file beats
//! earlier), and a malformed file is a startup refusal naming file and line.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A loopback endpoint recording the `x-key` header of the one request it serves.
fn spawn_key_recorder() -> (u16, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let seen = slot.clone();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut reader = BufReader::new(stream);
            let mut start = String::new();
            if reader.read_line(&mut start).is_err() || start.is_empty() {
                continue;
            }
            let mut len = 0usize;
            let mut key = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    break;
                }
                if line.trim().is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                } else if lower.starts_with("x-key:")
                    && let Some((_, v)) = line.split_once(':')
                {
                    key = Some(v.trim().to_string());
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).ok();
            *seen.lock().unwrap() = key;
            let mut s = reader.into_inner();
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            );
            let _ = s.flush();
        }
    });
    (port, slot)
}

fn run(cfg: &str, envs: &[&str], set: &[(&str, &str)], unset: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(["--config", cfg]);
    for e in envs {
        cmd.args(["--env", e]);
    }
    for (k, v) in set {
        cmd.env(k, v);
    }
    for k in unset {
        cmd.env_remove(k);
    }
    cmd.stdin(Stdio::null()).output().expect("run agentd")
}

#[test]
fn env_files_feed_expansion_and_secrets_with_documented_precedence() {
    let dir = common::unique_path("agentd-envfiles", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let base = format!("{dir}/base.env");
    let over = format!("{dir}/override.env");
    std::fs::write(
        &base,
        "EF_NAME=from-base\nEF_SECRET=base-secret\nEF_PINNED=file-value\n",
    )
    .unwrap();
    std::fs::write(&over, "EF_NAME=from-override\n").unwrap();

    let (port, seen) = spawn_key_recorder();
    let cfg = format!("{dir}/config.yaml");
    std::fs::write(
        &cfg,
        // ${EF_NAME}/${EF_PINNED} exercise config expansion; the http header
        // exercises {{secret:…}} resolution at the point of use, at runtime.
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: ${{EF_NAME}}\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     start: {{kind: once}}\n\
             \x20     t: {{kind: template, depends_on: [start], text: \"pinned=${{EF_PINNED}}\"}}\n\
             \x20     call: {{kind: http, depends_on: [t], method: POST, url: \"http://127.0.0.1:{port}/k\", allow_private: true, headers: {{x-key: \"{{{{secret:EF_SECRET}}}}\"}}}}\n\
             \x20     done: {{kind: finish, depends_on: [call], status: completed, output: \"{{{{steps.t.output}}}}\"}}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 1s\n\
             observability:\n  log_level: info\n  log_content: true\n"
        ),
    )
    .unwrap();

    // EF_PINNED is set in the REAL environment — it must beat the file.
    let out = run(
        &cfg,
        &[&base, &over],
        &[("EF_PINNED", "real-env-wins")],
        &["EF_NAME", "EF_SECRET"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        seen.lock().unwrap().as_deref(),
        Some("base-secret"),
        "a file value resolved a {{{{secret:…}}}} at the point of use:\n{stderr}"
    );
    assert!(
        stdout.contains("pinned=real-env-wins"),
        "the real environment beat the file: {stdout}"
    );
    // Later file beat the earlier one for the agent name.
    assert!(
        stderr.contains("\"instance\":\"from-override\""),
        "later file wins: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_env_file_refuses_startup_naming_the_line() {
    let dir = common::unique_path("agentd-envfiles-bad", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let bad = format!("{dir}/bad.env");
    std::fs::write(&bad, "GOOD=1\nnot a pair\n").unwrap();
    let cfg = format!("{dir}/config.yaml");
    std::fs::write(
        &cfg,
        "config_version: \"1\"\nagent:\n  name: x\n  instruction: hi\n  preflight: never\n\
         intelligence:\n  endpoints: [https://x/v1]\n  model: m\nstore:\n  kind: none\n",
    )
    .unwrap();
    let out = run(&cfg, &[&bad], &[], &[]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bad.env:2"),
        "the refusal names file and line:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
