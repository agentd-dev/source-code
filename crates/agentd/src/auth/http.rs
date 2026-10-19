// SPDX-License-Identifier: AGPL-3.0-only
//! The one HTTP helper every OAuth request goes through: metadata discovery,
//! the device authorization, token, refresh and revocation requests, and the
//! RFC 9728 probe. It returns the status, the response headers and the body —
//! a caller needs the headers for `Retry-After`, `WWW-Authenticate` and
//! `DPoP-Nonce` — and never follows a redirect: a `3xx` is an answer like
//! any other, and a credentialed request is never re-sent to wherever one
//! points.
//!
//! How a URL is dialled is the caller's to say. [`Dial::Named`] is for an
//! endpoint the operator configured, or one an operator-configured issuer's
//! metadata names: it dials by name, as every OAuth request always has, so a
//! loopback development authorization server keeps working. [`Dial::Vetted`]
//! is for a URL a server chose: HTTPS only, resolved once, every address
//! classified, and only a vetted address dialled (`net::ssrf`).

use std::time::Duration;

use crate::net::http::{self, Url};

/// How a request's URL is dialled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dial {
    /// By name, `http://` or `https://` — an operator-configured endpoint.
    #[default]
    Named,
    /// HTTPS only, through the SSRF guard; `allow_private` admits loopback,
    /// private and link-local addresses.
    Vetted { allow_private: bool },
}

/// One answer: status, headers (names lowercased) and body.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// How long the server asked the client to wait (`Retry-After`, seconds
    /// or an HTTP-date), from now.
    pub fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after")
            .and_then(|v| http::parse_retry_after(v, std::time::SystemTime::now()))
    }
}

/// POST an `application/x-www-form-urlencoded` body. A non-2xx is an answer,
/// not an error: the device poll reads the OAuth error in a `400`'s body.
pub fn post_form(
    url: &str,
    form: &[u8],
    headers: &[(&str, &str)],
    timeout: Duration,
    dial: Dial,
) -> Result<Reply, String> {
    let mut all = vec![("Content-Type", "application/x-www-form-urlencoded")];
    all.extend_from_slice(headers);
    send("POST", url, &all, form, timeout, dial)
}

/// GET `url` with `headers`.
pub fn get(
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
    dial: Dial,
) -> Result<Reply, String> {
    send("GET", url, headers, &[], timeout, dial)
}

/// One request: dial, send, read the whole answer.
pub fn send(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
    dial: Dial,
) -> Result<Reply, String> {
    send_reaching(method, url, headers, body, timeout, dial).map_err(|f| f.to_string())
}

/// Why a request got no answer, and whether any of it left: a request that
/// failed before a byte of it was sent is safe to send again, whatever it
/// was (RFC 0047 §5.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Nothing was sent: no connection, or no TLS session with the server.
    Unsent(String),
    /// Some or all of it was sent, and no whole answer came.
    Unanswered(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Unsent(e) | Failure::Unanswered(e) => f.write_str(e),
        }
    }
}

/// [`send`], saying how far a failed request got.
pub fn send_reaching(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
    dial: Dial,
) -> Result<Reply, Failure> {
    let u = Url::parse(url).map_err(|e| Failure::Unsent(format!("oauth: url {url}: {e}")))?;
    let mut stream = connect(&u, timeout, dial).map_err(Failure::Unsent)?;
    let resp = http::send(
        stream.as_mut(),
        &u.host_header(),
        method,
        &u.path,
        headers,
        body,
    )
    .map_err(|e| Failure::Unanswered(format!("oauth: request failed: {e}")))?;
    Ok(Reply {
        status: resp.status,
        headers: resp.headers,
        body: resp.body,
    })
}

/// One request whose answer is read as it arrives: the status and headers,
/// and the body left on the connection for the caller to read — a
/// `text/event-stream`, say, event by event.
pub fn send_streaming(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
    dial: Dial,
) -> Result<http::StreamingResponse<Box<dyn http::Stream>>, Failure> {
    let u = Url::parse(url).map_err(|e| Failure::Unsent(format!("oauth: url {url}: {e}")))?;
    let stream = connect(&u, timeout, dial).map_err(Failure::Unsent)?;
    http::send_streaming(stream, &u.host_header(), method, &u.path, headers, body)
        .map_err(|e| Failure::Unanswered(format!("oauth: request failed: {e}")))
}

/// Connect to an OAuth endpoint as `dial` says: `https://` over TLS, and a
/// [`Dial::Named`] `http://` in the clear. Plain HTTP is only ever sane for a
/// loopback development authorization server — one reached in the clear sees
/// the client's credential and returns the token in the clear — so a real
/// deployment configures `https` endpoints. The TLS handshake is made here,
/// before the request is written, so a server agentd could not reach over
/// TLS has been sent nothing.
fn connect(url: &Url, timeout: Duration, dial: Dial) -> Result<Box<dyn http::Stream>, String> {
    let tcp = match dial {
        Dial::Named => http::connect_tcp(&url.host, url.port, timeout),
        Dial::Vetted { allow_private } => {
            if !url.is_tls() {
                return Err(format!(
                    "oauth: {} is not an https URL, and a server-chosen URL must be",
                    url.host
                ));
            }
            crate::net::ssrf::connect_vetted(&url.host, url.port, timeout, allow_private)
        }
    }
    .map_err(|e| format!("oauth: connect {}: {e}", url.host))?;
    if url.is_tls() {
        #[cfg(feature = "tls")]
        {
            let tls = |e: std::io::Error| format!("oauth: tls {}: {e}", url.host);
            let mut s = crate::net::tls::connect(tcp, &url.host, None).map_err(tls)?;
            while s.conn.is_handshaking() {
                s.conn.complete_io(&mut s.sock).map_err(tls)?;
            }
            Ok(Box::new(s))
        }
        #[cfg(not(feature = "tls"))]
        {
            Err("oauth: https requires building with --features tls".to_string())
        }
    } else {
        Ok(Box::new(tcp))
    }
}

/// `url` with a well-known suffix inserted between its authority and its
/// path (RFC 8414 §3.1, RFC 9728 §3.1): `https://as.example/tenant` and
/// `oauth-authorization-server` give
/// `https://as.example/.well-known/oauth-authorization-server/tenant`. A
/// terminating `/` on the path is removed first, a query is kept after it, a
/// fragment dropped. `None` for a string that is not an absolute URL.
pub fn insert_well_known(url: &str, suffix: &str) -> Option<String> {
    let (origin, path) = split_origin(url)?;
    let path = path.split('#').next().unwrap_or_default();
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let path = path.trim_end_matches('/');
    let mut out = format!("{origin}/.well-known/{suffix}{path}");
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    Some(out)
}

/// `url`'s origin (`scheme://authority`) and what follows it (path, query
/// and fragment, possibly empty), as written.
pub fn split_origin(url: &str) -> Option<(&str, &str)> {
    let scheme_end = url.find("://")?;
    let rest = &url[scheme_end + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let split = scheme_end + 3 + end;
    Some((&url[..split], &url[split..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8414 §3.1 and RFC 9728 §3.1 insertion, for URLs with and
    /// without a path.
    #[test]
    fn a_well_known_suffix_goes_between_the_authority_and_the_path() {
        let asm = "oauth-authorization-server";
        for (issuer, want) in [
            (
                "https://as.example",
                "https://as.example/.well-known/oauth-authorization-server",
            ),
            (
                "https://as.example/",
                "https://as.example/.well-known/oauth-authorization-server",
            ),
            (
                "https://as.example/tenant1",
                "https://as.example/.well-known/oauth-authorization-server/tenant1",
            ),
            (
                "https://as.example:8443/a/b/",
                "https://as.example:8443/.well-known/oauth-authorization-server/a/b",
            ),
        ] {
            assert_eq!(
                insert_well_known(issuer, asm).as_deref(),
                Some(want),
                "{issuer}"
            );
        }
        let prm = "oauth-protected-resource";
        assert_eq!(
            insert_well_known("https://rs.example/resource1?q=1#f", prm).as_deref(),
            Some("https://rs.example/.well-known/oauth-protected-resource/resource1?q=1")
        );
        assert_eq!(
            insert_well_known("https://rs.example/mcp", prm).as_deref(),
            Some("https://rs.example/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(insert_well_known("as.example/x", asm), None);
        assert_eq!(insert_well_known("https:///x", asm), None);
    }

    #[test]
    fn a_server_chosen_url_must_be_https_and_public() {
        let t = Duration::from_secs(2);
        let e = get(
            "http://example.com/x",
            &[],
            t,
            Dial::Vetted {
                allow_private: true,
            },
        )
        .unwrap_err();
        assert!(e.contains("not an https URL"), "{e}");
        let e = get(
            "https://127.0.0.1:9/x",
            &[],
            t,
            Dial::Vetted {
                allow_private: false,
            },
        )
        .unwrap_err();
        assert!(e.contains("loopback"), "{e}");
    }
}
