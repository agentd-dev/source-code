// SPDX-License-Identifier: AGPL-3.0-only
//! OAuth 2.1 **authorization-code + PKCE** browser flow (RFC 0031 §7) end to end:
//! `browser_login` prints the authorization URL, runs a loopback callback server,
//! verifies `state`, and exchanges the code (+ PKCE verifier) at the token
//! endpoint. The test plays the browser — it reads the printed URL, extracts the
//! redirect + state, and hits the callback. [feature: oauth]
#![cfg(all(unix, feature = "oauth"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

/// A mock token endpoint that returns a bearer for the authorization-code grant.
fn spawn_token_endpoint() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().take(4).flatten() {
            let mut s = c;
            drain_request(&mut s);
            let body = r#"{"access_token":"at-browser","token_type":"Bearer","expires_in":3600,"refresh_token":"rt-browser"}"#;
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    port
}

fn drain_request(s: &mut TcpStream) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).ok();
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
    let mut b = vec![0u8; len];
    r.read_exact(&mut b).ok();
}

/// Extract a query param and percent-decode the value (enough for this test).
fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    for kv in q.split('&') {
        if let Some((k, v)) = kv.split_once('=')
            && k == key
        {
            return Some(pct_decode(v));
        }
    }
    None
}
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = |c: u8| (c as char).to_digit(16);
            if let (Some(x), Some(y)) = (h(b[i + 1]), h(b[i + 2])) {
                out.push((x * 16 + y) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn browser_authorization_code_pkce_flow() {
    let token_port = spawn_token_endpoint();
    let params = agentd::auth::oauth2::OAuth2Params {
        token_url: format!("http://127.0.0.1:{token_port}/token"),
        device_authorization_url: None,
        authorization_url: Some("https://as.example/authorize".into()),
        client_id: "agentd".into(),
        client_secret: None,
        scopes: vec!["profile".into()],
        audience: None,
    };

    // Run the flow on a thread; capture the printed URL via a channel.
    let (tx, rx) = mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        agentd::auth::browser::browser_login(
            &params,
            |url| tx.send(url.to_string()).unwrap(),
            Duration::from_secs(10),
        )
    });

    // Play the browser: read the URL, verify PKCE + state are present, then hit
    // the loopback callback with a code + the echoed state.
    let url = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("auth URL printed");
    assert!(
        url.contains("code_challenge="),
        "PKCE challenge present: {url}"
    );
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    let redirect = query_param(&url, "redirect_uri").expect("redirect_uri");
    let state = query_param(&url, "state").expect("state");
    let port: u16 = redirect
        .rsplit(':')
        .next()
        .and_then(|hp| hp.split('/').next())
        .and_then(|p| p.parse().ok())
        .expect("callback port");

    let mut cb = TcpStream::connect(("127.0.0.1", port)).expect("connect callback");
    let req = format!(
        "GET /callback?code=test-code&state={state} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    );
    cb.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    cb.read_to_string(&mut resp).ok();
    assert!(
        resp.contains("authorized"),
        "the callback page is served: {resp}"
    );

    let cred = handle.join().unwrap().expect("browser_login succeeds");
    assert_eq!(cred.access_token, "at-browser");
    assert_eq!(cred.refresh_token.as_deref(), Some("rt-browser"));
}
