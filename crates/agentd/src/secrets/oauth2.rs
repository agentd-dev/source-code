//! OAuth2 client-credentials source (RFC 6749 §4.4) for `[[secrets]]`.
//!
//! One job: turn `(token_url, client_id, client_secret, scopes)` into a
//! cached access token that refreshes itself before expiry. Built on
//! `ureq` — the same blocking rustls client `intel-remote` and
//! `tools-http-tls` already ship, so no new dependency and no async
//! runtime.
//!
//! Generic by construction: `extra_params` carries provider quirks
//! (Auth0's `audience`, Azure's `resource`), and `auth_style` covers
//! both credential placements providers use (`body` form fields, or
//! RFC 6749 §2.3.1 HTTP `basic`). The token endpoint may be plain
//! `http://` only in tests — production endpoints are HTTPS and go
//! through rustls.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::SecretError;

/// A fetched access token plus its refresh deadline. The deadline is
/// computed from `expires_in` at fetch time (monotonic clock — wall
/// clock changes can't expire a token early or late).
#[derive(Clone)]
pub(crate) struct CachedToken {
    pub access_token: String,
    fetched_at: Instant,
    ttl: Duration,
}

impl CachedToken {
    /// Expired (for refresh purposes) once within `skew` seconds of
    /// the real expiry — refresh early, never serve a dying token.
    pub fn expired(&self, skew_secs: u64) -> bool {
        let ttl = self.ttl.saturating_sub(Duration::from_secs(skew_secs));
        self.fetched_at.elapsed() >= ttl
    }
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CachedToken(***)")
    }
}

fn err(name: &str, reason: impl Into<String>) -> SecretError {
    SecretError {
        name: name.to_string(),
        reason: reason.into(),
    }
}

/// Percent-encode one form value (application/x-www-form-urlencoded).
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// POST the client-credentials grant and parse the token response.
pub(crate) fn fetch_token(
    name: &str,
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    scopes: &[String],
    extra_params: &HashMap<String, String>,
    auth_style: Option<&str>,
) -> Result<CachedToken, SecretError> {
    let style = auth_style.unwrap_or("body");
    let mut pairs: Vec<(String, String)> = vec![("grant_type".into(), "client_credentials".into())];
    match style {
        "body" => {
            pairs.push(("client_id".into(), client_id.into()));
            pairs.push(("client_secret".into(), client_secret.into()));
        }
        "basic" => {}
        other => {
            return Err(err(
                name,
                format!("auth_style `{other}` is not one of: body, basic"),
            ));
        }
    }
    if !scopes.is_empty() {
        pairs.push(("scope".into(), scopes.join(" ")));
    }
    // Deterministic param order keeps request logs / test assertions
    // stable across runs.
    let mut extras: Vec<_> = extra_params.iter().collect();
    extras.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in extras {
        pairs.push((k.clone(), v.clone()));
    }
    let body = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirects(0)
        .build();
    let mut request = agent
        .post(token_url)
        .set("content-type", "application/x-www-form-urlencoded")
        .set("accept", "application/json");
    if style == "basic" {
        request = request.set(
            "authorization",
            &format!(
                "Basic {}",
                base64_encode(format!("{client_id}:{client_secret}").as_bytes())
            ),
        );
    }

    let fetched_at = Instant::now();
    let response = match request.send_string(&body) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            // Token-endpoint errors carry useful JSON (`error`,
            // `error_description`) — surface it, truncated, never the
            // credentials.
            let detail = r.into_string().unwrap_or_default();
            let detail: String = detail.chars().take(300).collect();
            return Err(err(
                name,
                format!("token endpoint returned {code}: {detail}"),
            ));
        }
        Err(e) => return Err(err(name, format!("token endpoint: {e}"))),
    };
    let raw = response
        .into_string()
        .map_err(|e| err(name, format!("read token response: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| err(name, format!("token response is not JSON: {e}")))?;
    let access_token = v["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| err(name, "token response has no access_token"))?
        .to_string();
    // Missing/odd expires_in → conservative 300 s cache, so a
    // provider that omits it still refreshes promptly.
    let ttl = v["expires_in"].as_u64().filter(|s| *s > 0).unwrap_or(300);

    tracing::info!(
        target: "agentd::audit",
        event = "secrets.oauth2_token_fetched",
        secret = %name,
        expires_in_secs = ttl,
    );

    Ok(CachedToken {
        access_token,
        fetched_at,
        ttl: Duration::from_secs(ttl),
    })
}

/// RFC 4648 base64 encode (standard alphabet, padded) — counterpart to
/// the strict decoder in `auth::basic`; kept dependency-free for the
/// same reason.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let triple = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — against an in-process fake token endpoint
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// One-shot-per-connection fake token endpoint. Returns the URL,
    /// a hit counter, and captured request bodies.
    fn spawn_token_server(
        response_json: &'static str,
        accept: u32,
    ) -> (String, Arc<AtomicU32>, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicU32::new(0));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (h, b) = (hits.clone(), bodies.clone());
        std::thread::spawn(move || {
            for _ in 0..accept {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = vec![0u8; 8192];
                let mut seen = Vec::new();
                loop {
                    let n = s.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    if let Some(hdr_end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&seen[..hdr_end]).to_string();
                        let want: usize = headers
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if seen.len() - hdr_end - 4 >= want {
                            let full = String::from_utf8_lossy(&seen).to_string();
                            b.lock().unwrap().push(full);
                            break;
                        }
                    }
                }
                h.fetch_add(1, Ordering::SeqCst);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_json.len(),
                    response_json
                );
                s.write_all(resp.as_bytes()).unwrap();
                s.flush().unwrap();
            }
        });
        (url, hits, bodies)
    }

    #[test]
    fn fetches_and_parses_a_token() {
        let (url, _hits, bodies) =
            spawn_token_server(r#"{"access_token":"tok-1","expires_in":3600}"#, 1);
        let tok = fetch_token(
            "T",
            &url,
            "my-client",
            "my-secret",
            &["api".into(), "read".into()],
            &HashMap::from([("audience".to_string(), "https://api.example".to_string())]),
            None,
        )
        .unwrap();
        assert_eq!(tok.access_token, "tok-1");
        assert!(!tok.expired(60));
        let body = bodies.lock().unwrap().join("");
        assert!(body.contains("grant_type=client_credentials"), "{body}");
        assert!(body.contains("client_id=my-client"), "{body}");
        assert!(body.contains("scope=api+read"), "{body}");
        assert!(
            body.contains("audience=https%3A%2F%2Fapi.example"),
            "{body}"
        );
    }

    #[test]
    fn basic_auth_style_uses_the_header_not_the_body() {
        let (url, _hits, bodies) =
            spawn_token_server(r#"{"access_token":"tok-2","expires_in":60}"#, 1);
        fetch_token(
            "T",
            &url,
            "id",
            "s3cret",
            &[],
            &HashMap::new(),
            Some("basic"),
        )
        .unwrap();
        let body = bodies.lock().unwrap().join("");
        // base64("id:s3cret")
        assert!(body.contains("authorization: Basic aWQ6czNjcmV0"), "{body}");
        assert!(!body.contains("client_secret="), "{body}");
    }

    #[test]
    fn registry_caches_until_skewed_expiry() {
        let (url, hits, _bodies) =
            spawn_token_server(r#"{"access_token":"tok-3","expires_in":3600}"#, 2);
        // Client credentials come from FILES — proving source
        // composition: the oauth2 source's own inputs resolve through
        // the registry.
        let dir = tempfile::TempDir::new().unwrap();
        let id_path = dir.path().join("id");
        let secret_path = dir.path().join("secret");
        std::fs::write(&id_path, "id").unwrap();
        std::fs::write(&secret_path, "s").unwrap();
        let reg = crate::secrets::SecretsRegistry::build(&[
            crate::secrets::SecretDef {
                name: "OAUTH_CLIENT_ID".into(),
                source: crate::secrets::SourceDef::File {
                    path: id_path.display().to_string(),
                    trim: true,
                },
            },
            crate::secrets::SecretDef {
                name: "OAUTH_CLIENT_SECRET".into(),
                source: crate::secrets::SourceDef::File {
                    path: secret_path.display().to_string(),
                    trim: true,
                },
            },
            crate::secrets::SecretDef {
                name: "API_TOKEN".into(),
                source: crate::secrets::SourceDef::Oauth2 {
                    token_url: url.clone(),
                    client_id_env: "OAUTH_CLIENT_ID".into(),
                    client_secret_env: "OAUTH_CLIENT_SECRET".into(),
                    scopes: vec![],
                    extra_params: HashMap::new(),
                    auth_style: None,
                    skew_secs: Some(60),
                },
            },
        ])
        .unwrap();
        assert_eq!(reg.resolve("API_TOKEN").unwrap(), "tok-3");
        assert_eq!(reg.resolve("API_TOKEN").unwrap(), "tok-3");
        // The build probe fetched once; both resolves hit the cache.
        assert_eq!(hits.load(Ordering::SeqCst), 1, "token must be cached");
    }

    #[test]
    fn expired_cache_refetches() {
        let cached = CachedToken {
            access_token: "x".into(),
            fetched_at: Instant::now() - Duration::from_secs(100),
            ttl: Duration::from_secs(120),
        };
        assert!(!cached.expired(10)); // 100 < 120-10
        assert!(cached.expired(30)); // 100 >= 120-30
    }

    #[test]
    fn error_responses_surface_without_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = r#"{"error":"invalid_client"}"#;
            let resp = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes());
        });
        let e =
            fetch_token("T", &url, "id", "sup3r-secret", &[], &HashMap::new(), None).unwrap_err();
        assert!(e.reason.contains("401"), "{e}");
        assert!(e.reason.contains("invalid_client"), "{e}");
        assert!(!e.reason.contains("sup3r-secret"), "{e}");
    }

    #[test]
    fn base64_encode_round_trips_with_basic_decoder() {
        assert_eq!(base64_encode(b"id:s3cret"), "aWQ6czNjcmV0");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }
}
