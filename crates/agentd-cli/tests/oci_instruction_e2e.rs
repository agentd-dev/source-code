// SPDX-License-Identifier: AGPL-3.0-only
//! **Instruction from an OCI registry, end to end** (RFC 0040): the real
//! daemon boots against an in-process mock registry, runs the token dance,
//! pulls the manifest and blob, verifies both digests, loads the document —
//! `instruction.loaded` carries the manifest digest that pins the version —
//! and reaches `proc.ready` with the document's workflow armed. With the
//! `decrypt` feature too, the registry serves an age-ENCRYPTED blob and the
//! oci→decrypt→idoc chain is proven in one pass.
#![cfg(all(unix, feature = "oci", feature = "workflow"))]

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Minimal Distribution-API mock: token dance, manifest, blob.
fn mock_registry(blob: Vec<u8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let sha = |b: &[u8]| -> String {
            use std::fmt::Write as _;
            let d = ring::digest::digest(&ring::digest::SHA256, b);
            let mut s = String::new();
            for x in d.as_ref() {
                let _ = write!(s, "{x:02x}");
            }
            s
        };
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let authed = req
                .to_ascii_lowercase()
                .contains("authorization: bearer tok-e2e");
            let manifest = json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "artifactType": "application/vnd.instruction.document.v1",
                "layers": [{
                    "mediaType": "text/markdown; variant=instruction",
                    "digest": format!("sha256:{}", sha(&blob)),
                    "size": blob.len(),
                }],
            })
            .to_string();
            let (status, extra, body): (&str, String, Vec<u8>) = if path.starts_with("/token") {
                ("200 OK", String::new(), br#"{"token":"tok-e2e"}"#.to_vec())
            } else if !authed {
                (
                    "401 Unauthorized",
                    format!(
                        "WWW-Authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"mock\"\r\n"
                    ),
                    b"{}".to_vec(),
                )
            } else if path.contains("/manifests/") {
                ("200 OK", String::new(), manifest.into_bytes())
            } else if path.contains("/blobs/") {
                ("200 OK", String::new(), blob.clone())
            } else {
                ("404 Not Found", String::new(), Vec::new())
            };
            let head = format!(
                "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&body);
        }
    });
    port
}

const DOC: &str = "---\nspec: \"1\"\n---\n# Pulled agent\n\nServe pulls.\n\n:::!workflow{name=pulled-drain}\nsteps:\n  start: { kind: manual }\n  done:  { kind: finish, depends_on: [start] }\n:::\n";

/// Boot the daemon with the given instruction URI (+ extra config), wait for
/// `proc.ready`, return the captured stderr log.
fn boot_and_capture(uri: &str, extra: Value) -> String {
    let mut cfg = json!({
        "config_version": "1",
        "agent": {"name": "oci-e2e", "preflight": "never", "instruction": uri},
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
    });
    if let (Some(dst), Some(src)) = (cfg.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    let cfg_path = common::unique_path("oci-e2e", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err_path = common::unique_path("oci-e2e", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", &cfg_path])
        .stdout(Stdio::null())
        .stderr(errf)
        .spawn()
        .unwrap();
    // Wait for proc.ready, a logged exit, or the process dying (a config-load
    // refusal exits before the JSON logger exists) — the pull precedes all.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if log.contains("proc.ready")
            || log.contains("proc.exit")
            || child.try_wait().is_ok_and(|s| s.is_some())
        {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("daemon neither became ready nor exited:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    for p in [cfg_path, err_path] {
        let _ = std::fs::remove_file(&p);
    }
    log
}

fn loaded_event(log: &str) -> Option<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "instruction.loaded")
}

#[test]
fn the_daemon_pulls_loads_and_arms_from_a_registry() {
    let port = mock_registry(DOC.as_bytes().to_vec());
    let log = boot_and_capture(&format!("oci://127.0.0.1:{port}/acme/agent:v1"), json!({}));
    let ev = loaded_event(&log).unwrap_or_else(|| panic!("no instruction.loaded in:\n{log}"));
    assert!(
        ev["manifest_digest"]
            .as_str()
            .is_some_and(|d| d.starts_with("sha256:")),
        "the load records its version pin: {ev}"
    );
    assert!(log.contains("proc.ready"), "startup completed:\n{log}");
    assert!(
        log.contains("pulled-drain"),
        "the pulled document's workflow registered:\n{log}"
    );
}

#[test]
fn a_registry_that_cannot_serve_the_document_fails_startup_loudly() {
    // Nothing listens on the port: the pull fails at CONFIG LOAD, and the
    // refusal names the reference — never a daemon running with no instruction.
    let port = common::free_port();
    let log = boot_and_capture(&format!("oci://127.0.0.1:{port}/acme/agent:v1"), json!({}));
    assert!(!log.contains("proc.ready"), "startup must fail:\n{log}");
    assert!(log.contains("oci://") && log.contains("connect"), "{log}");
}

/// The full chain — registry serves CIPHERTEXT, agentd decrypts on the fly.
#[cfg(feature = "decrypt")]
#[test]
fn an_encrypted_artifact_pulls_and_decrypts_in_one_pass() {
    use agentd::config::decrypt::{agefile, x25519};
    let sk = [31u8; 32];
    let pk = x25519::public_key(&sk);
    let enc = agefile::encrypt(DOC.as_bytes(), &[pk], None).unwrap();
    let port = mock_registry(enc);

    let key_path = common::unique_path("oci-age-key", "txt");
    std::fs::write(&key_path, agefile::encode_identity(&sk)).unwrap();
    let log = boot_and_capture(
        &format!("oci://127.0.0.1:{port}/acme/agent:v1"),
        json!({"instruction": {"decrypt": {"keys": [key_path.clone()]}}}),
    );
    assert!(
        loaded_event(&log).is_some() && log.contains("pulled-drain"),
        "encrypted artifact decrypted and loaded:\n{log}"
    );
    let _ = std::fs::remove_file(&key_path);
}
