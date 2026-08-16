// SPDX-License-Identifier: Apache-2.0
//! AWS workload-identity credential sources (RFC 0031 §8): `source: irsa` (EKS
//! web identity → STS AssumeRoleWithWebIdentity) and `source: imds` (EC2 instance
//! metadata, IMDSv2). Each mock server yields temporary credentials; the SigV4
//! signer fetches, caches, and signs with them. Both run in one test (sequential)
//! to avoid process-env races.
#![cfg(all(unix, feature = "oauth"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use agentd::mcp::http::RequestSigner;

/// A one-shot-per-connection mock that dispatches on the request path.
fn spawn<F: Fn(&str) -> (&'static str, Vec<u8>) + Send + 'static>(f: F) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().take(16).flatten() {
            serve(c, &f);
        }
    });
    port
}

fn serve<F: Fn(&str) -> (&'static str, Vec<u8>)>(stream: TcpStream, f: &F) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok();
    let (ctype, payload) = f(&path);
    let mut s = r.into_inner();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(&payload);
}

fn signer(source: &str) -> std::sync::Arc<agentd::auth::aws::SigV4Signer> {
    let spec = agentd::config::AuthSpec {
        kind: "aws".into(),
        source: Some(source.into()),
        region: Some("us-east-1".into()),
        service: Some("execute-api".into()),
        ..Default::default()
    };
    agentd::auth::aws::SigV4Signer::from_spec(&spec, "mcp:x").unwrap()
}

#[test]
fn irsa_and_imds_credentials_sign_requests() {
    // ---- IRSA: web identity → STS AssumeRoleWithWebIdentity ----
    let sts_port = spawn(|_path| {
        (
            "text/xml",
            br#"<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult><Credentials><AccessKeyId>ASIAIRSA</AccessKeyId><SecretAccessKey>irsa-secret</SecretAccessKey><SessionToken>irsa-sess</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>"#.to_vec(),
        )
    });
    let tokf = std::env::temp_dir().join(format!("agentd-wi-{}.tok", std::process::id()));
    std::fs::write(&tokf, "web-identity-jwt").unwrap();
    // SAFETY: single test in this binary; env is set then read on this thread.
    unsafe {
        std::env::set_var("AWS_WEB_IDENTITY_TOKEN_FILE", &tokf);
        std::env::set_var("AWS_ROLE_ARN", "arn:aws:iam::123:role/agentd");
        std::env::set_var(
            "AGENTD_STS_ENDPOINT",
            format!("http://127.0.0.1:{sts_port}"),
        );
    }
    let h = signer("irsa").sign("GET", "api.example", "/x", b"");
    let auth = &h.iter().find(|(k, _)| k == "Authorization").unwrap().1;
    assert!(auth.contains("Credential=ASIAIRSA/"), "IRSA SigV4: {auth}");
    assert!(
        h.iter()
            .any(|(k, v)| k == "X-Amz-Security-Token" && v == "irsa-sess"),
        "IRSA session token rides the request"
    );
    std::fs::remove_file(&tokf).ok();

    // ---- IMDS: IMDSv2 token → role → credentials ----
    let imds_port = spawn(|path| {
        if path.ends_with("/latest/api/token") {
            ("text/plain", b"imds-session-token".to_vec())
        } else if path.ends_with("/security-credentials/") {
            ("text/plain", b"agentd-role".to_vec())
        } else {
            (
                "application/json",
                br#"{"AccessKeyId":"ASIAIMDS","SecretAccessKey":"imds-secret","Token":"imds-sess","Expiration":"2099-01-01T00:00:00Z"}"#.to_vec(),
            )
        }
    });
    unsafe {
        std::env::set_var(
            "AGENTD_IMDS_ENDPOINT",
            format!("http://127.0.0.1:{imds_port}"),
        );
    }
    let h = signer("imds").sign("GET", "api.example", "/x", b"");
    let auth = &h.iter().find(|(k, _)| k == "Authorization").unwrap().1;
    assert!(auth.contains("Credential=ASIAIMDS/"), "IMDS SigV4: {auth}");
    assert!(
        h.iter()
            .any(|(k, v)| k == "X-Amz-Security-Token" && v == "imds-sess"),
        "IMDS session token rides the request"
    );
}
