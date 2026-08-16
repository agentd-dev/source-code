// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd login` for **AWS IAM Identity Center (SSO)** (RFC 0031 §8) end to end:
//! the binary runs the SSO-OIDC device flow against a mock (register → device →
//! poll pending/authorized → exchange for temporary AWS credentials) and caches
//! the temp keys the SigV4 signer will use. Proves the "enterprise login for
//! Bedrock" interactive flow.
#![cfg(all(unix, feature = "oauth"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A mock AWS SSO-OIDC + portal server serving the four steps.
fn spawn_mock_sso() -> u16 {
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

    let (status, payload): (&str, &[u8]) = if path.ends_with("/client/register") {
        ("200 OK", br#"{"clientId":"cid","clientSecret":"csec"}"#)
    } else if path.ends_with("/device_authorization") {
        (
            "200 OK",
            br#"{"deviceCode":"dc","userCode":"WXYZ-1234","verificationUri":"https://device.sso.example","interval":1,"expiresIn":300}"#,
        )
    } else if path.ends_with("/token") {
        if polls.fetch_add(1, Ordering::SeqCst) == 0 {
            ("400 Bad Request", br#"{"error":"authorization_pending"}"#)
        } else {
            (
                "200 OK",
                br#"{"accessToken":"sso-access-tok","expiresIn":3600}"#,
            )
        }
    } else if path.contains("/federation/credentials") {
        (
            "200 OK",
            br#"{"roleCredentials":{"accessKeyId":"ASIAEXAMPLE","secretAccessKey":"topsecret","sessionToken":"sess-tok","expiration":9999999999999}}"#,
        )
    } else {
        ("404 Not Found", b"{}")
    };

    let mut s = reader.into_inner();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(payload);
    let _ = s.flush();
}

fn config() -> String {
    "config_version: \"2\"\n\
     agent:\n  name: sso\n  instruction: x\n  preflight: never\n\
     intelligence:\n  endpoints: [https://api.openai.com/v1]\n  model: m\n\
     store:\n  kind: memory\n\
     mcp:\n  servers:\n    - name: bedrock\n      endpoint: https://gateway.example/mcp\n\
     \x20     auth:\n        kind: aws\n        region: us-east-1\n        service: execute-api\n\
     \x20       source: sso\n        sso_start_url: https://my-org.awsapps.com/start\n\
     \x20       account_id: \"123456789012\"\n        role_name: AgentdBedrock\n\
     lifecycle:\n  run_until: idle\n"
        .to_string()
}

#[test]
fn agentd_login_aws_sso_caches_temporary_credentials() {
    let port = spawn_mock_sso();
    let cfg = common::unique_path("sso-cfg", "yaml");
    std::fs::write(&cfg, config()).unwrap();
    let cred_dir = common::unique_path("sso-creds", "d");
    std::fs::create_dir_all(&cred_dir).ok();

    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--login", "mcp:bedrock", "--config", &cfg])
        .env("AGENTD_CRED_DIR", &cred_dir)
        .env("AGENTD_SSO_OIDC", format!("http://127.0.0.1:{port}"))
        .env("AGENTD_SSO_PORTAL", format!("http://127.0.0.1:{port}"))
        .output()
        .expect("run agentd --login (aws sso)");

    assert!(
        out.status.success(),
        "sso login exits 0: {:?}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("WXYZ-1234"),
        "the user code is shown: {stderr}"
    );
    assert!(
        !stderr.contains("topsecret"),
        "the secret key is never printed: {stderr}"
    );

    // The temporary AWS credentials are cached in `extra`, ready for SigV4.
    let id = agentd::auth::cache::cred_id("mcp:bedrock");
    let body = std::fs::read_to_string(format!("{cred_dir}/{id}.json")).expect("a cred file");
    let cred: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(cred["extra"]["aws_access_key_id"], "ASIAEXAMPLE");
    assert_eq!(cred["extra"]["aws_secret_access_key"], "topsecret");
    assert_eq!(cred["extra"]["aws_session_token"], "sess-tok");

    // Close the loop: the daemon's SigV4 signer (source: sso) reads the cached
    // temp creds and signs a request with them. SAFETY: single test in this binary.
    unsafe { std::env::set_var("AGENTD_CRED_DIR", &cred_dir) };
    use agentd::mcp::http::RequestSigner;
    let authspec = agentd::config::AuthSpec {
        kind: "aws".into(),
        source: Some("sso".into()),
        region: Some("us-east-1".into()),
        service: Some("execute-api".into()),
        ..Default::default()
    };
    let signer =
        agentd::auth::aws::SigV4Signer::from_spec(&authspec, "mcp:bedrock").expect("signer");
    let h = signer.sign("GET", "gateway.example", "/mcp", b"");
    let auth_h = &h.iter().find(|(k, _)| k == "Authorization").unwrap().1;
    assert!(
        auth_h.contains("Credential=ASIAEXAMPLE/"),
        "SigV4 uses the SSO temp key: {auth_h}"
    );
    assert!(
        h.iter().any(|(k, _)| k == "X-Amz-Security-Token"),
        "the session token rides the request"
    );

    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&cred_dir).ok();
}
