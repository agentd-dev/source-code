// SPDX-License-Identifier: AGPL-3.0-only
//! AAuth discovery: fetch a party's well-known
//! metadata document (AAuth protocol §12.10). Resource discovery
//! (`/.well-known/aauth-resource.json`) learns a server's `content-digest`
//! requirement — the access mode is the runtime `AAuth-Requirement`'s to say,
//! which is always authoritative; Agent-Provider discovery
//! (`/.well-known/aauth-agent.json`) confirms the provider's `issuer`. A party
//! without a document is used as configured, and the §12.10 anti-host-poisoning
//! rule holds for both: a document that IS served MUST declare an `issuer`
//! equal to the URL it was fetched from, or it is rejected. Resource discovery
//! is advisory. The provider's is not: a provider whose document could not be
//! read is not used until it can be.

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
    /// Whether requests must cover a body `content-digest`.
    #[serde(default)]
    pub content_digest: bool,
}

/// Whether the server at `endpoint` (any MCP endpoint URL) requires a
/// `content-digest` cover, as its discovery document says. `Some(false)` for
/// a server that answered with no document, or one that does not parse;
/// `None` for one that did not answer, or answered with a server error —
/// discovery is advisory, never fatal.
pub fn fetch(endpoint: &str, timeout: Duration) -> Option<bool> {
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
    if resp.status >= 500 || resp.status == 429 {
        return None;
    }
    Some(
        resp.is_success()
            && serde_json::from_slice::<ResourceMeta>(&resp.body).is_ok_and(|m| m.content_digest),
    )
}

/// The subset of `aauth-agent.json` (Agent Provider metadata, protocol §12.10.1)
/// agentd acts on. The enroll/token endpoints are NOT here — those are
/// informational bootstrap conventions, not advertised in metadata — so all
/// agentd needs is the `issuer` to validate against the configured provider.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderMeta {
    #[serde(default)]
    pub issuer: Option<String>,
}

/// Why the Agent-Provider metadata was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    /// The provider did not answer: a connection, TLS or read failure, or a
    /// server error. Asked again by the next request that needs the provider.
    Unreachable(String),
    /// The provider answered with metadata that cannot be used: a document
    /// that does not parse, or one that names another `issuer`.
    Invalid(String),
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetadataError::Unreachable(e) | MetadataError::Invalid(e) => f.write_str(e),
        }
    }
}

/// Fetch + validate the Agent-Provider metadata document for `base_url`
/// (protocol §12.10.1). Returns:
///   * `Ok(Some(meta))` — a document was served and its `issuer` matches (or it
///     carried no `issuer` to check);
///   * `Ok(None)` — the provider serves none (a status that is neither a
///     success nor a server error): the AP need not publish one, and the
///     enroll+token flow proceeds as configured;
///   * `Err(Unreachable)` — no answer, or a `5xx` or `429`: a document may
///     exist, so the provider is not verified;
///   * `Err(Invalid)` — a served document that does not parse, or whose
///     `issuer` CONTRADICTS the configured provider (host-poisoning) → refuse
///     to enroll against it.
pub fn fetch_agent_provider(
    base_url: &str,
    timeout: Duration,
) -> Result<Option<ProviderMeta>, MetadataError> {
    let base = Url::parse(base_url)
        .map_err(|e| MetadataError::Invalid(format!("aauth: provider url {base_url:?}: {e}")))?;
    let well_known = Url {
        scheme: base.scheme.clone(),
        host: base.host.clone(),
        port: base.port,
        path: "/.well-known/aauth-agent.json".to_string(),
    };
    let unreachable =
        |e: String| MetadataError::Unreachable(format!("aauth: provider metadata {base_url}: {e}"));
    let mut stream = connect(&well_known, timeout).map_err(unreachable)?;
    let resp = http::send(
        stream.as_mut(),
        &well_known.host_header(),
        "GET",
        &well_known.path,
        &[("Accept", "application/json")],
        &[],
    )
    .map_err(|e| unreachable(e.to_string()))?;
    if resp.status >= 500 || resp.status == 429 {
        return Err(unreachable(format!("HTTP {}", resp.status)));
    }
    if !resp.is_success() {
        return Ok(None);
    }
    let invalid = |why: String| {
        MetadataError::Invalid(format!(
            "aauth: provider metadata {base_url} is not an aauth-agent.json document: {why}"
        ))
    };
    let doc: serde_json::Value =
        serde_json::from_slice(&resp.body).map_err(|e| invalid(e.to_string()))?;
    if !doc.is_object() {
        return Err(invalid("not a JSON object".into()));
    }
    let meta: ProviderMeta = serde_json::from_value(doc).map_err(|e| invalid(e.to_string()))?;
    // §12.10 anti-host-poisoning: reject a served document that names a different
    // issuer than the provider we were told to use.
    if let Some(iss) = meta.issuer.as_deref()
        && !issuer_matches(iss, base_url)
    {
        return Err(MetadataError::Invalid(format!(
            "aauth: provider metadata issuer {iss:?} does not match configured provider {base_url:?} (possible host-poisoning)"
        )));
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
        assert!(
            matches!(&err, MetadataError::Invalid(e) if e.contains("host-poisoning")),
            "{err}"
        );
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

    /// A served document that is not one is refused, as a contradicting one
    /// is: the provider answered, and not with its metadata.
    #[test]
    fn a_document_that_does_not_parse_is_invalid() {
        for body in ["<html>gateway</html>", r#"{"issuer": 7}"#, "[]"] {
            let origin = serve_once(200, |_| body.to_string());
            let err = fetch_agent_provider(&origin, Duration::from_secs(2)).unwrap_err();
            assert!(matches!(err, MetadataError::Invalid(_)), "{body}: {err}");
        }
    }

    /// No answer, or a server error, leaves the provider unverified: it is
    /// not taken for one that publishes nothing.
    #[test]
    fn no_answer_or_a_server_error_is_unreachable() {
        let origin = serve_once(503, |_| "{}".to_string());
        let err = fetch_agent_provider(&origin, Duration::from_secs(2)).unwrap_err();
        assert!(
            matches!(&err, MetadataError::Unreachable(e) if e.contains("HTTP 503")),
            "{err}"
        );
        let closed = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let err = fetch_agent_provider(&closed, Duration::from_secs(2)).unwrap_err();
        assert!(matches!(err, MetadataError::Unreachable(_)), "{err}");
    }
}
