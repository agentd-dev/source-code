// SPDX-License-Identifier: Apache-2.0
//! AAuth discovery (RFC 0023 §Step 3 + §7.1 G1): fetch a party's well-known
//! metadata document (AAuth protocol §12.10). Resource discovery
//! (`/.well-known/aauth-resource.json`) learns a server's `access_mode` /
//! `content-digest` requirement; Agent-Provider discovery
//! (`/.well-known/aauth-agent.json`) confirms the provider's `issuer`. Both are
//! best-effort — a party without a document is used as configured — EXCEPT the
//! §12.10 anti-host-poisoning rule: a document that IS served MUST declare an
//! `issuer` equal to the URL it was fetched from, or it is rejected.

use crate::net::http::{self, Url};
use serde::Deserialize;
use std::time::Duration;

/// The §12.10 issuer check: a fetched metadata document MUST declare an `issuer`
/// equal to the URL it was retrieved from (origin), else it is host-poisoned and
/// rejected. Compared after trimming a trailing slash, ASCII-case-insensitively
/// (scheme+host are case-insensitive; agentd uses the configured provider URL
/// verbatim to reach the document, so an exact origin match is the intent).
pub(super) fn issuer_matches(doc_issuer: &str, base_url: &str) -> bool {
    doc_issuer
        .trim_end_matches('/')
        .eq_ignore_ascii_case(base_url.trim_end_matches('/'))
}

/// The subset of `aauth-resource.json` agentd acts on.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ResourceMeta {
    /// `agent-token` | `aauth-access-token` | `auth-token` — the case the
    /// server declares up front. Parsed for completeness; agentd reacts to the
    /// runtime `AAuth-Requirement` (always authoritative) rather than pre-picking.
    #[serde(default)]
    #[allow(dead_code)]
    pub access_mode: Option<String>,
    /// Whether requests must cover a body `content-digest`.
    #[serde(default)]
    pub content_digest: bool,
}

/// Fetch the discovery document for `endpoint` (any MCP endpoint URL). Returns
/// `None` on any failure (missing document, non-JSON, unreachable) — discovery
/// is advisory, never fatal.
pub fn fetch(endpoint: &str, timeout: Duration) -> Option<ResourceMeta> {
    let base = Url::parse(endpoint).ok()?;
    let well_known = Url {
        scheme: base.scheme.clone(),
        host: base.host.clone(),
        port: base.port,
        path: "/.well-known/aauth-resource.json".to_string(),
    };
    let mut stream = connect(&well_known, timeout).ok()?;
    let resp = http::send(
        stream.as_mut(),
        &well_known.host_header(),
        "GET",
        &well_known.path,
        &[("Accept", "application/json")],
        &[],
    )
    .ok()?;
    if !resp.is_success() {
        return None;
    }
    serde_json::from_slice(&resp.body).ok()
}

/// The subset of `aauth-agent.json` (Agent Provider metadata, protocol §12.10.1)
/// agentd acts on. The enroll/token endpoints are NOT here — those are
/// informational bootstrap conventions, not advertised in metadata — so all
/// agentd needs is the `issuer` to validate against the configured provider.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderMeta {
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub name: Option<String>,
}

/// Fetch + validate the Agent-Provider metadata document for `base_url` (RFC 0023
/// §7.1 G1, protocol §12.10.1). Returns:
///   * `Ok(Some(meta))` — a document was served and its `issuer` matches (or it
///     carried no `issuer` to check);
///   * `Ok(None)` — no document / unreachable / unparseable (best-effort: the AP
///     need not publish one, and the enroll+token flow proceeds regardless);
///   * `Err(..)` — a document was served whose `issuer` CONTRADICTS the
///     configured provider (host-poisoning) → refuse to enroll against it.
pub fn fetch_agent_provider(
    base_url: &str,
    timeout: Duration,
) -> Result<Option<ProviderMeta>, String> {
    let Ok(base) = Url::parse(base_url) else {
        return Ok(None);
    };
    let well_known = Url {
        scheme: base.scheme.clone(),
        host: base.host.clone(),
        port: base.port,
        path: "/.well-known/aauth-agent.json".to_string(),
    };
    let Ok(mut stream) = connect(&well_known, timeout) else {
        return Ok(None);
    };
    let Ok(resp) = http::send(
        stream.as_mut(),
        &well_known.host_header(),
        "GET",
        &well_known.path,
        &[("Accept", "application/json")],
        &[],
    ) else {
        return Ok(None);
    };
    if !resp.is_success() {
        return Ok(None);
    }
    let Ok(meta) = serde_json::from_slice::<ProviderMeta>(&resp.body) else {
        return Ok(None);
    };
    // §12.10 anti-host-poisoning: reject a served document that names a different
    // issuer than the provider we were told to use.
    if let Some(iss) = meta.issuer.as_deref()
        && !issuer_matches(iss, base_url)
    {
        return Err(format!(
            "aauth: provider metadata issuer {iss:?} does not match configured provider {base_url:?} (possible host-poisoning)"
        ));
    }
    Ok(Some(meta))
}

fn connect(url: &Url, timeout: Duration) -> Result<Box<dyn http::Stream>, String> {
    let tcp = http::connect_tcp(&url.host, url.port, timeout).map_err(|e| e.to_string())?;
    if url.is_tls() {
        #[cfg(feature = "tls")]
        {
            let s = crate::net::tls::connect(tcp, &url.host, None).map_err(|e| e.to_string())?;
            return Ok(Box::new(s));
        }
        #[cfg(not(feature = "tls"))]
        {
            return Err("tls off".into());
        }
    }
    Ok(Box::new(tcp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn issuer_match_is_origin_exact_modulo_trailing_slash_and_case() {
        assert!(issuer_matches("https://ap.example", "https://ap.example"));
        assert!(issuer_matches("https://ap.example/", "https://ap.example")); // trailing /
        assert!(issuer_matches("https://AP.example", "https://ap.example")); // host case
        // A different host/scheme/port is the poisoning case → no match.
        assert!(!issuer_matches(
            "https://evil.example",
            "https://ap.example"
        ));
        assert!(!issuer_matches("http://ap.example", "https://ap.example"));
        assert!(!issuer_matches(
            "https://ap.example:8443",
            "https://ap.example"
        ));
    }

    /// Bind a fresh loopback port and serve one HTTP response. `body` is built
    /// from the port (so a doc can claim its own origin as `issuer`). Returns the
    /// `http://127.0.0.1:<port>` origin.
    fn serve_once(status: u16, body: impl Fn(&str) -> String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let doc = body(&origin);
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{doc}",
                    doc.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        origin
    }

    #[test]
    fn matching_issuer_is_accepted() {
        // The doc claims issuer == its own origin → accepted.
        let origin = serve_once(200, |o| format!(r#"{{"issuer":"{o}","name":"AP"}}"#));
        let meta = fetch_agent_provider(&origin, Duration::from_secs(2)).unwrap();
        assert_eq!(meta.unwrap().issuer.as_deref(), Some(origin.as_str()));
    }

    #[test]
    fn mismatched_issuer_is_rejected() {
        // Doc served at our origin but claiming a DIFFERENT issuer → host-poisoning.
        let origin = serve_once(200, |_| r#"{"issuer":"https://evil.example"}"#.to_string());
        let err = fetch_agent_provider(&origin, Duration::from_secs(2)).unwrap_err();
        assert!(err.contains("host-poisoning"), "{err}");
    }

    #[test]
    fn absent_document_is_best_effort_none() {
        let origin = serve_once(404, |_| r#"{"error":"not found"}"#.to_string());
        assert!(
            fetch_agent_provider(&origin, Duration::from_secs(2))
                .unwrap()
                .is_none()
        );
    }
}
