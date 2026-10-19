// SPDX-License-Identifier: AGPL-3.0-only
//! OAuth 2.1 **authorization-code + PKCE** loopback flow (RFC 7636 / RFC 8252)
//! — the browser alternative to the device grant, selected by
//! `auth: { grant: authorization_code }`.
//!
//! agentd **prints** the authorization URL instead of shelling out to a
//! browser: launching one would mean executing a local program chosen by the
//! environment, which this binary never does. It runs a one-shot loopback
//! callback server to capture the redirect, verifies the `state` (a mismatch
//! means the callback did not come from the request we started, so the code is
//! discarded) and the RFC 9207 `iss` (a response from another authorization
//! server is a mix-up, and its code is discarded too), and exchanges the code
//! plus the PKCE `code_verifier` for tokens. The `state`, the verifier and —
//! when the login asks for `openid` — the OpenID Connect `nonce` come from
//! the kernel CSPRNG (`sec::random`), with no fallback: a host that cannot
//! read it fails the login rather than send a `state` an attacker could
//! guess. An ID token in the answer must carry that `nonce`, name the issuer
//! and be for this client. An authorization server whose metadata does not
//! list the `S256` PKCE method is refused before the browser is sent
//! anywhere (MCP 2026-07-28).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

use crate::auth::cache::CachedCred;
use crate::auth::oauth2::{self, OAuth2Params};
use crate::sha::sha256;

/// Run the authorization-code + PKCE loopback flow. `on_url` receives the URL for
/// the human to open; the call blocks until the redirect arrives (or `timeout`).
pub fn browser_login(
    params: &OAuth2Params,
    on_url: impl FnOnce(&str),
    timeout: Duration,
) -> Result<CachedCred, String> {
    let auth_url = params
        .authorization_url
        .as_deref()
        .ok_or("oauth: authorization_url is required for the browser flow (or set issuer)")?;
    if params.pkce_s256 == Some(false) {
        return Err(format!(
            "oauth: the metadata of {} does not list S256 in code_challenge_methods_supported, so it cannot be held to PKCE — refused",
            params.authorization_server()
        ));
    }

    // PKCE (RFC 7636): a high-entropy verifier (32 bytes, 43 characters) and
    // its S256 challenge; and the CSRF `state`.
    let random = |n| {
        crate::sec::random::b64url_token(n)
            .map_err(|e| format!("oauth: no OS randomness for the login: {e}"))
    };
    let verifier = random(32)?;
    let challenge = crate::b64::url_nopad(&sha256(verifier.as_bytes()));
    let state = random(16)?;
    let oidc = params.scopes.iter().any(|s| s == "openid");
    let nonce = if oidc { Some(random(16)?) } else { None };

    // The redirect URI a Client ID Metadata Document registers, on its port
    // when it names one; otherwise any free port of 127.0.0.1.
    let at = match params.redirect_uri.as_deref() {
        Some(uri) => crate::config::settings::client::loopback(uri)
            .ok_or_else(|| format!("oauth: {uri:?} is not a loopback redirect URI"))?,
        None => crate::config::settings::client::Loopback {
            host: "127.0.0.1",
            port: None,
            path: "/callback".into(),
        },
    };
    let listener = TcpListener::bind(format!("{}:{}", at.host, at.port.unwrap_or(0)))
        .map_err(|e| format!("oauth: loopback: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = at.uri(port);

    let scope = params.scopes.join(" ");
    let mut url = format!(
        "{auth_url}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        pct(&params.client_id),
        pct(&redirect),
        pct(&scope),
        pct(&state),
        challenge,
    );
    if let Some(n) = &nonce {
        url.push_str(&format!("&nonce={}", pct(n)));
    }
    if let Some(r) = &params.resource {
        url.push_str(&format!("&resource={}", pct(r)));
    }
    on_url(&url);

    let back = wait_for_callback(&listener, timeout)?;
    if back.state != state {
        return Err("oauth: state mismatch on the callback (possible CSRF) — aborted".into());
    }
    check_iss(
        back.iss.as_deref(),
        params.issuer.as_deref(),
        params.iss_supported,
    )?;
    let tokens = oauth2::exchange_code(params, &back.code, &verifier, &redirect, timeout)?;
    if let Some(id_token) = &tokens.id_token {
        check_id_token(
            id_token,
            nonce.as_deref(),
            params.issuer.as_deref(),
            &params.client_id,
        )?;
    }
    Ok(crate::auth::login::tokens_to_cred(&tokens))
}

/// An ID token the token endpoint returned (OpenID Connect Core §3.1.3.7):
/// its `nonce` the one the request carried, its `iss` the issuer when one is
/// known, and its `aud` this client. It came straight from the token
/// endpoint, so TLS stands in for its signature (§3.1.3.7, item 6); what is
/// checked is that it answers THIS request.
fn check_id_token(
    id_token: &str,
    nonce: Option<&str>,
    issuer: Option<&str>,
    client_id: &str,
) -> Result<(), String> {
    let claims = id_token
        .split('.')
        .nth(1)
        .and_then(|p| crate::b64::url_decode(p).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .ok_or("oauth: the ID token in the answer is not a JWT — aborted")?;
    if let Some(want) = nonce
        && claims["nonce"].as_str() != Some(want)
    {
        return Err(
            "oauth: the ID token does not carry the nonce this login sent (a replayed or injected token) — aborted"
                .into(),
        );
    }
    if let Some(want) = issuer
        && claims["iss"].as_str() != Some(want)
    {
        return Err(format!(
            "oauth: the ID token names the issuer {:?}, not {want:?} — aborted",
            claims["iss"].as_str().unwrap_or("")
        ));
    }
    let for_us = match &claims["aud"] {
        serde_json::Value::String(a) => a == client_id,
        serde_json::Value::Array(a) => a.iter().any(|v| v.as_str() == Some(client_id)),
        _ => false,
    };
    if !for_us {
        return Err(format!(
            "oauth: the ID token is not for this client ({client_id}) — aborted"
        ));
    }
    Ok(())
}

/// RFC 9207 §2.4: an authorization response's `iss` must be the issuer the
/// request went to, byte for byte, and one without it is refused when the
/// server's metadata declares it always sends one. With endpoints pinned and
/// no `issuer` configured there is nothing to compare an `iss` with.
fn check_iss(got: Option<&str>, issuer: Option<&str>, declared: bool) -> Result<(), String> {
    match (got, issuer) {
        (Some(got), Some(want)) if got != want => Err(format!(
            "oauth: the authorization response names the issuer {got:?}, not {want:?} (a mix-up, RFC 9207) — aborted"
        )),
        (None, _) if declared => Err(
            "oauth: the authorization response carries no `iss`, which this server's metadata says it always sends (RFC 9207) — aborted"
                .into(),
        ),
        _ => Ok(()),
    }
}

/// What the authorization server's redirect carried.
struct Callback {
    code: String,
    state: String,
    iss: Option<String>,
}

/// Accept one loopback callback, parse `?code=…&state=…&iss=…`, and reply
/// with a page.
fn wait_for_callback(listener: &TcpListener, timeout: Duration) -> Result<Callback, String> {
    let (stream, _) = listener
        .accept()
        .map_err(|e| format!("oauth: callback accept: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    // Drain the rest of the request head (so the client gets a clean response).
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
    }
    // `GET /callback?code=…&state=… HTTP/1.1`
    let target = line.split_whitespace().nth(1).unwrap_or("");
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let (mut code, mut state, mut iss, mut err) = (None, None, None, None);
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            let val = pct_decode(v);
            match k {
                "code" => code = Some(val),
                "state" => state = Some(val),
                "iss" => iss = Some(val),
                "error" => err = Some(val),
                _ => {}
            }
        }
    }
    let page = "<html><body>agentd: authorized. You may close this window and return to the terminal.</body></html>";
    let mut s = stream;
    let _ = s.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        )
        .as_bytes(),
    );
    if let Some(e) = err {
        return Err(format!("oauth: authorization denied: {e}"));
    }
    Ok(Callback {
        code: code.ok_or("oauth: no `code` in the callback")?,
        state: state.unwrap_or_default(),
        iss,
    })
}

// --- encoders (dependency-free) --------------------------------------------

/// Percent-encode a query component (unreserved set passes through).
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-decode a query component (`+` → space; `%XX` → byte).
pub(crate) fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_matches_rfc7636_example() {
        // The S256 challenge is this encoding applied to the SHA-256 of the
        // verifier, so the encoder is checked against known vectors here.
        assert_eq!(crate::b64::url_nopad(b""), "");
        assert_eq!(crate::b64::url_nopad(b"f"), "Zg");
        assert_eq!(crate::b64::url_nopad(b"foobar"), "Zm9vYmFy");
        // url-safe alphabet: bytes that would be + / in standard base64.
        assert_eq!(crate::b64::url_nopad(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn pct_roundtrips_and_decodes_plus() {
        assert_eq!(pct("a b/c"), "a%20b%2Fc");
        assert_eq!(pct_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(pct_decode("x+y"), "x y");
    }

    fn id_token(claims: serde_json::Value) -> String {
        format!(
            "{}.{}.sig",
            crate::b64::url_nopad(br#"{"alg":"RS256"}"#),
            crate::b64::url_nopad(claims.to_string().as_bytes())
        )
    }

    /// An ID token answers this login only with its nonce, the issuer and
    /// this client in its audience.
    #[test]
    fn an_id_token_must_carry_the_nonce_the_issuer_and_the_client() {
        let iss = Some("https://as.example");
        let ok =
            id_token(serde_json::json!({"iss": "https://as.example", "aud": "c", "nonce": "n1"}));
        assert!(check_id_token(&ok, Some("n1"), iss, "c").is_ok());
        let e = check_id_token(&ok, Some("n2"), iss, "c").unwrap_err();
        assert!(e.contains("nonce"), "{e}");
        let e = check_id_token(&ok, Some("n1"), Some("https://other.example"), "c").unwrap_err();
        assert!(e.contains("issuer"), "{e}");
        let e = check_id_token(&ok, Some("n1"), iss, "d").unwrap_err();
        assert!(e.contains("not for this client"), "{e}");
        let many = id_token(
            serde_json::json!({"iss": "https://as.example", "aud": ["x", "c"], "nonce": "n1"}),
        );
        assert!(check_id_token(&many, Some("n1"), iss, "c").is_ok());
        let none = id_token(serde_json::json!({"iss": "https://as.example", "aud": "c"}));
        assert!(check_id_token(&none, Some("n1"), iss, "c").is_err());
        assert!(check_id_token("not-a-jwt", None, iss, "c").is_err());
    }

    /// MCP's PKCE gate: metadata that lists no `S256` is refused before any
    /// URL is printed.
    #[test]
    fn an_authorization_server_without_s256_is_refused_before_the_browser() {
        let p = OAuth2Params {
            authorization_url: Some("https://as.example/authorize".into()),
            token_url: "https://as.example/token".into(),
            issuer: Some("https://as.example".into()),
            client_id: "c".into(),
            pkce_s256: Some(false),
            ..Default::default()
        };
        // On a thread of its own: a login that went on would wait for a
        // browser that never comes.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let printed = std::sync::atomic::AtomicBool::new(false);
            let r = browser_login(
                &p,
                |_| printed.store(true, std::sync::atomic::Ordering::SeqCst),
                Duration::from_millis(1),
            );
            let _ = tx.send((r, printed.into_inner()));
        });
        let (r, printed) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("refused at once, not waiting for a browser");
        let e = r.unwrap_err();
        assert!(
            e.contains("S256") && e.contains("https://as.example"),
            "{e}"
        );
        assert!(!printed, "no URL was handed out");
    }

    /// Every RFC 9207 row: `iss` present or absent, the metadata declaring
    /// support or not, and a present `iss` matching the issuer or not.
    #[test]
    fn every_iss_row_is_decided_as_rfc_9207_says() {
        let want = Some("https://as.example");
        let other = "https://evil.example";
        for declared in [false, true] {
            // Present and equal: accepted, declared or not.
            assert!(check_iss(want, want, declared).is_ok(), "{declared}");
            // Present and different: a mix-up, declared or not.
            let e = check_iss(Some(other), want, declared).unwrap_err();
            assert!(e.contains("mix-up") && e.contains(other), "{e}");
            // Equal only byte for byte: a trailing slash is another issuer.
            assert!(check_iss(Some("https://as.example/"), want, declared).is_err());
        }
        // Absent: refused only when the server says it always sends one —
        // whatever it would have named.
        for issuer in [want, Some(other)] {
            assert!(check_iss(None, issuer, false).is_ok());
            let e = check_iss(None, issuer, true).unwrap_err();
            assert!(e.contains("carries no `iss`"), "{e}");
        }
        // No issuer known (endpoints pinned): nothing to compare with.
        assert!(check_iss(Some(other), None, false).is_ok());
    }
}
