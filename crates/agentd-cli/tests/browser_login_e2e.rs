// SPDX-License-Identifier: AGPL-3.0-only
//! OAuth 2.1 **authorization-code + PKCE** browser flow, end to end:
//! `browser_login` prints the authorization URL, runs a loopback callback server,
//! verifies `state`, and exchanges the code (+ PKCE verifier) at the token
//! endpoint. The test plays the browser — it reads the printed URL, extracts the
//! redirect + state, and hits the callback. The callback's RFC 9207 `iss` is
//! checked before the code is exchanged. [feature: oauth]
#![cfg(all(unix, feature = "oauth"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use agentd::auth::cache::CachedCred;
use agentd::auth::oauth2::OAuth2Params;

/// A mock token endpoint that returns a bearer for the authorization-code
/// grant, counting the exchanges that reach it.
fn spawn_token_endpoint() -> (u16, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let exchanges = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&exchanges);
    std::thread::spawn(move || {
        for c in l.incoming().take(4).flatten() {
            let mut s = c;
            drain_request(&mut s);
            counted.fetch_add(1, Ordering::SeqCst);
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
    (port, exchanges)
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

/// The params of a login against the mock token endpoint on `port`.
fn params(port: u16) -> OAuth2Params {
    OAuth2Params {
        token_url: format!("http://127.0.0.1:{port}/token"),
        authorization_url: Some("https://as.example/authorize".into()),
        client_id: "agentd".into(),
        scopes: vec!["profile".into()],
        ..Default::default()
    }
}

/// Run the login and play the browser: follow the printed URL back to the
/// callback with a code, the echoed state, and `extra` query parameters.
fn play(params: OAuth2Params, extra: &str) -> Result<CachedCred, String> {
    let (tx, rx) = mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        agentd::auth::browser::browser_login(
            &params,
            |url| tx.send(url.to_string()).unwrap(),
            Duration::from_secs(10),
        )
    });
    let url = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("auth URL printed");
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
        "GET /callback?code=test-code&state={state}{extra} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    );
    cb.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    cb.read_to_string(&mut resp).ok();
    handle.join().unwrap()
}

/// RFC 9207 on the callback: an `iss` equal to the issuer is exchanged; one
/// naming another server is a mix-up; none, where the metadata declares the
/// server always sends one, is refused too — and a refused callback's code
/// never reaches the token endpoint.
#[test]
fn the_callbacks_iss_is_checked_before_the_code_is_exchanged() {
    let issuer = "https://as.example";
    let cases = [
        // (iss on the callback, metadata declares it, accepted)
        (Some(issuer), false, true),
        (Some(issuer), true, true),
        (Some("https://evil.example"), false, false),
        (Some("https://evil.example"), true, false),
        (None, false, true),
        (None, true, false),
    ];
    for (iss, declared, accepted) in cases {
        let (port, exchanges) = spawn_token_endpoint();
        let mut p = params(port);
        p.issuer = Some(issuer.into());
        p.iss_supported = declared;
        let extra = iss
            .map(|i| format!("&iss={}", i.replace(':', "%3A").replace('/', "%2F")))
            .unwrap_or_default();
        let got = play(p, &extra);
        let row = format!("iss {iss:?}, declared {declared}");
        if accepted {
            assert_eq!(got.expect(&row).access_token, "at-browser", "{row}");
            assert_eq!(exchanges.load(Ordering::SeqCst), 1, "{row}");
        } else {
            let e = got.expect_err(&row);
            assert!(e.contains("RFC 9207"), "{row}: {e}");
            assert_eq!(exchanges.load(Ordering::SeqCst), 0, "{row}: no exchange");
        }
    }
}

#[test]
fn browser_authorization_code_pkce_flow() {
    let (token_port, _) = spawn_token_endpoint();
    let params = params(token_port);

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
    // 128 bits of state, base64url.
    assert_eq!(state.len(), 22, "{state}");
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

/// A Client ID Metadata Document client returns to the loopback redirect URI
/// its document registers (RFC 0052): with a port, the login sends that URI
/// exactly and listens on its port; without one, it listens on a free port
/// and sends the URI with that port, keeping the registered path.
#[test]
fn a_document_client_returns_to_the_redirect_uri_it_registers() {
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    for (registered, fixed) in [
        (format!("http://127.0.0.1:{free}/oauth/callback"), true),
        ("http://127.0.0.1/oauth/callback".to_string(), false),
    ] {
        let (token_port, exchanges) = spawn_token_endpoint();
        let mut p = params(token_port);
        p.client_id_metadata_document = true;
        p.redirect_uri = Some(registered.clone());
        let (tx, rx) = mpsc::channel::<String>();
        let handle = std::thread::spawn(move || {
            agentd::auth::browser::browser_login(
                &p,
                |url| tx.send(url.to_string()).unwrap(),
                Duration::from_secs(10),
            )
        });
        let url = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("auth URL printed");
        let redirect = query_param(&url, "redirect_uri").expect("redirect_uri");
        let state = query_param(&url, "state").expect("state");
        let port: u16 = redirect
            .strip_prefix("http://127.0.0.1:")
            .and_then(|r| r.strip_suffix("/oauth/callback"))
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("{redirect}"));
        if fixed {
            assert_eq!(redirect, registered, "the registered URI, exactly");
        }
        let mut cb = TcpStream::connect(("127.0.0.1", port)).expect("connect callback");
        cb.write_all(
            format!(
                "GET /oauth/callback?code=c&state={state} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
        let mut resp = String::new();
        cb.read_to_string(&mut resp).ok();
        let cred = handle.join().unwrap().expect("browser_login succeeds");
        assert_eq!(cred.access_token, "at-browser");
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
    }
}
