// SPDX-License-Identifier: AGPL-3.0-only
//! OAuth 2.1 **authorization-code + PKCE** loopback flow (RFC 0031 §7; RFC 7636 /
//! RFC 8252) — the browser alternative to the device grant, selected by
//! `auth: { grant: authorization_code }`.
//!
//! agentd **prints** the authorization URL rather than shelling out to a browser
//! (no local execution, RFC 0012); it runs a one-shot loopback callback server to
//! capture the redirect, verifies the `state` (CSRF), and exchanges the code plus
//! the PKCE `code_verifier` for tokens. Randomness is from `/dev/urandom`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use crate::auth::cache::{self, CachedCred};
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

    // PKCE (RFC 7636): a high-entropy verifier and its S256 challenge.
    let verifier = rand_b64url(32);
    let challenge = b64url_nopad(&sha256(verifier.as_bytes()));
    let state = rand_b64url(16);

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("oauth: loopback: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");

    let scope = params.scopes.join(" ");
    let url = format!(
        "{auth_url}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        pct(&params.client_id),
        pct(&redirect),
        pct(&scope),
        pct(&state),
        challenge,
    );
    on_url(&url);

    let (code, got_state) = wait_for_callback(&listener, timeout)?;
    if got_state != state {
        return Err("oauth: state mismatch on the callback (possible CSRF) — aborted".into());
    }
    let tokens = oauth2::exchange_code(params, &code, &verifier, &redirect, timeout)?;
    Ok(crate::auth::login::tokens_to_cred(&tokens))
}

/// Accept one loopback callback, parse `?code=…&state=…`, and reply with a page.
fn wait_for_callback(
    listener: &TcpListener,
    timeout: Duration,
) -> Result<(String, String), String> {
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
    let (mut code, mut state, mut err) = (None, None, None);
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            let val = pct_decode(v);
            match k {
                "code" => code = Some(val),
                "state" => state = Some(val),
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
    Ok((
        code.ok_or("oauth: no `code` in the callback")?,
        state.unwrap_or_default(),
    ))
}

// --- encoders (dependency-free) --------------------------------------------

/// `n` random bytes from `/dev/urandom`, base64url-encoded (no padding). Falls
/// back to a splitmix over time+pid if `/dev/urandom` is unavailable.
fn rand_b64url(n: usize) -> String {
    let mut buf = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let mut z = cache::now_ms() ^ (std::process::id() as u64).rotate_left(32);
        for b in buf.iter_mut() {
            z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            *b = (x >> 24) as u8;
        }
    }
    b64url_nopad(&buf)
}

/// base64url without padding (RFC 4648 §5).
fn b64url_nopad(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(A[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(A[(n & 63) as usize] as char);
        }
    }
    out
}

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
fn pct_decode(s: &str) -> String {
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
        // RFC 7636 Appendix B: the ASCII verifier bytes hash to this challenge.
        // Here we just check the encoder against a known vector.
        assert_eq!(b64url_nopad(b""), "");
        assert_eq!(b64url_nopad(b"f"), "Zg");
        assert_eq!(b64url_nopad(b"foobar"), "Zm9vYmFy");
        // url-safe alphabet: bytes that would be + / in standard base64.
        assert_eq!(b64url_nopad(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn pct_roundtrips_and_decodes_plus() {
        assert_eq!(pct("a b/c"), "a%20b%2Fc");
        assert_eq!(pct_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(pct_decode("x+y"), "x y");
    }

    #[test]
    fn rand_b64url_is_high_entropy_and_distinct() {
        let a = rand_b64url(32);
        let b = rand_b64url(32);
        assert!(a.len() >= 43, "43+ chars for 32 bytes: {}", a.len());
        assert_ne!(a, b);
    }
}
