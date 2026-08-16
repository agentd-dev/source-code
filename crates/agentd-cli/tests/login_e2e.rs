// SPDX-License-Identifier: Apache-2.0
//! `agentd login` (RFC 0031 §12) end to end: the binary runs the OAuth 2.1
//! **device-authorization** flow against a mock authorization server — device
//! request → poll (pending, then authorized) → cache the token in a per-user
//! file the daemon would read. Proves the interactive login UX works headlessly.
#![cfg(all(unix, feature = "oauth"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A mock OAuth authorization server: `POST /device` returns the device auth,
/// `POST /token` returns `authorization_pending` on the first poll and the tokens
/// thereafter (simulating the human authorizing between polls).
fn spawn_mock_as() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let polls = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for conn in listener.incoming().take(16) {
            let Ok(stream) = conn else { continue };
            handle(stream, &polls);
        }
    });
    port
}

fn handle(stream: TcpStream, polls: &Arc<AtomicUsize>) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok();

    let (status, payload): (&str, Vec<u8>) = if path.ends_with("/device") {
        (
            "200 OK",
            br#"{"device_code":"dev-123","user_code":"WDJB-MJHT","verification_uri":"https://idp.example/device","interval":1,"expires_in":300}"#
                .to_vec(),
        )
    } else if path.ends_with("/token") {
        let n = polls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            (
                "400 Bad Request",
                br#"{"error":"authorization_pending"}"#.to_vec(),
            )
        } else {
            (
                "200 OK",
                br#"{"access_token":"at-xyz","refresh_token":"rt-xyz","token_type":"Bearer","expires_in":3600}"#
                    .to_vec(),
            )
        }
    } else {
        ("404 Not Found", b"{}".to_vec())
    };

    let mut s = reader.into_inner();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(&payload);
    let _ = s.flush();
}

fn config(port: u16) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: login\n  instruction: x\n  preflight: never\n\
         intelligence:\n  endpoints: [https://api.openai.com/v1]\n  model: m\n\
         store:\n  kind: memory\n\
         mcp:\n  servers:\n    - name: test\n      endpoint: https://mcp.example\n\
         \x20     auth:\n        kind: oauth2\n        grant: device\n\
         \x20       token_url: http://127.0.0.1:{port}/token\n\
         \x20       device_authorization_url: http://127.0.0.1:{port}/device\n\
         \x20       client_id: agentd-cli\n        scopes: [mcp:read]\n\
         lifecycle:\n  run_until: idle\n"
    )
}

#[test]
fn agentd_login_completes_the_device_flow_and_caches_the_token() {
    let port = spawn_mock_as();
    let cfg = common::unique_path("login-cfg", "yaml");
    std::fs::write(&cfg, config(port)).unwrap();
    let cred_dir = common::unique_path("login-creds", "d");
    std::fs::create_dir_all(&cred_dir).ok();

    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--login", "mcp:test", "--config", &cfg])
        .env("AGENTD_CRED_DIR", &cred_dir)
        .output()
        .expect("run agentd --login");

    assert!(
        out.status.success(),
        "login exits 0: status={:?}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    // The device prompt (URL + code) is printed, but never the token.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("WDJB-MJHT"),
        "the user code is shown: {stderr}"
    );
    assert!(
        !stderr.contains("at-xyz"),
        "the token is never printed: {stderr}"
    );

    // The token is cached in a 0600 file the daemon would read.
    let id = agentd::auth::cache::cred_id("mcp:test");
    let path = format!("{cred_dir}/{id}.json");
    let body = std::fs::read_to_string(&path).expect("a cred file was written");
    let cred: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(cred["access_token"], "at-xyz");
    assert_eq!(cred["refresh_token"], "rt-xyz");
    assert!(cred["expires_at_ms"].as_u64().unwrap() > 0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the cred file is owner-only");
    }

    // Close the loop: the daemon's own path (`signer_for`) reads the same cache
    // and injects the cached bearer on the wire — no re-login, no network (the
    // token is still valid). This is exactly what `mcp::from_spec` attaches.
    // SAFETY: this binary has a single test — no intra-process env race.
    unsafe { std::env::set_var("AGENTD_CRED_DIR", &cred_dir) };
    let authspec = agentd::config::AuthSpec {
        kind: "oauth2".into(),
        grant: Some("device".into()),
        token_url: Some(format!("http://127.0.0.1:{port}/token")),
        client_id: Some("agentd-cli".into()),
        ..Default::default()
    };
    let signer = agentd::auth::device::signer_for(&authspec, "mcp:test", Duration::from_secs(5))
        .expect("signer_for ok")
        .expect("a signer is built for an oauth2 auth block");
    let headers = signer.sign("POST", "mcp.example", "/mcp", b"{}");
    assert_eq!(
        headers,
        vec![("Authorization".to_string(), "Bearer at-xyz".to_string())],
        "the daemon injects the cached bearer"
    );

    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&cred_dir).ok();
}
