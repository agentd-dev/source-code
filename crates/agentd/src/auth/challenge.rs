// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 9728 **OAuth 2.0 Protected Resource Metadata** discovery. When an MCP
//! `auth: { kind: oauth2 }` block names no `issuer`, agentd probes the server
//! before login: an unauthenticated request draws a
//! `401 WWW-Authenticate: Bearer resource_metadata="…"`, and that metadata
//! document lists the `authorization_servers` — the issuer to run RFC 8414 /
//! OIDC discovery against. Best-effort throughout: any failure returns
//! `None`, so an explicit `issuer` still wins and the caller reports its usual
//! "issuer required" error rather than a confusing discovery failure.

use std::time::Duration;

use serde::Deserialize;

use crate::auth::http::{self, Dial};
use crate::net::http::Url;

/// The RFC 9728 protected-resource metadata document (only the field we consume).
#[derive(Debug, Deserialize)]
struct ResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

/// Discover the authorization-server **issuer** for an MCP `resource` endpoint
/// via RFC 9728. Returns the first advertised `authorization_servers` entry, or
/// `None` when the server offers no challenge / metadata (the caller then
/// requires an explicit `issuer`). `resource` is the MCP server's endpoint URL.
pub fn discover_issuer(resource: &str, timeout: Duration) -> Option<String> {
    // Prefer the metadata URL the server names in its 401 challenge; else the
    // well-known locations derived from the resource (RFC 9728 §3.1): the
    // suffix inserted before the resource's path, then at the origin's root.
    // A `resource_metadata` URL the SERVER named is only honoured when it lives
    // on the resource's own origin. RFC 9728 metadata describes that resource,
    // so same-origin is where it belongs — and the alternative is a blind GET at
    // an address a hostile MCP server picks, whose `authorization_servers` answer
    // then chooses the issuer for the OIDC discovery and token requests that
    // follow. Cross-origin, we ignore the challenge and use the well-known
    // location instead of refusing outright, so a misconfigured server degrades
    // rather than breaks. Same-origin (rather than the SSRF classifier) keeps a
    // loopback development server working, since its resource is loopback too.
    let named = challenge_metadata_url(resource, timeout).filter(|u| same_origin(resource, u));
    let candidates = match named {
        Some(u) => vec![u],
        None => well_known_urls(resource),
    };
    let meta = candidates
        .iter()
        .find_map(|u| get_json::<ResourceMetadata>(u, timeout))?;
    meta.authorization_servers
        .into_iter()
        .find(|s| !s.trim().is_empty())
}

/// Probe the resource with an unauthenticated MCP request; on a `401`, return
/// the `resource_metadata` URL named in the `WWW-Authenticate` challenge.
///
/// Two probes, because a server may check the request before it checks the
/// caller. The first is a bare `initialize` with no revision header, which a
/// 2025-11-25 server that checks `MCP-Protocol-Version` first still answers
/// with its 401. Only when that draws no challenge, a `server/discover` with
/// the MCP 2026-07-28 standard headers and `_meta`, which a 2026-07-28 server
/// that validates those before auth answers with its 401 instead of a -32020.
fn challenge_metadata_url(resource: &str, timeout: Duration) -> Option<String> {
    let initialize = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    match probe(
        resource,
        timeout,
        &[("Accept", "application/json")],
        initialize,
    ) {
        Answer::Challenge(url) => return Some(url),
        // Nothing answered: a second probe would only wait as long again.
        Answer::Unreached => return None,
        Answer::NoChallenge => {}
    }
    let version = ::mcp::wire::DISCOVER_PROTOCOL_VERSION;
    let discover = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": {"_meta": {
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientInfo": {"name": "agentd", "version": crate::VERSION},
            "io.modelcontextprotocol/clientCapabilities": {},
        }}
    })
    .to_string();
    match probe(
        resource,
        timeout,
        &[
            ("Accept", "application/json, text/event-stream"),
            ("MCP-Protocol-Version", version),
            ("Mcp-Method", "server/discover"),
        ],
        discover.as_bytes(),
    ) {
        Answer::Challenge(url) => Some(url),
        Answer::Unreached | Answer::NoChallenge => None,
    }
}

/// What one probe drew.
enum Answer {
    /// A challenge naming its `resource_metadata` URL.
    Challenge(String),
    /// An HTTP answer without one.
    NoChallenge,
    /// No HTTP answer at all.
    Unreached,
}

/// One unauthenticated POST of `body` with `headers`.
fn probe(resource: &str, timeout: Duration, headers: &[(&str, &str)], body: &[u8]) -> Answer {
    let mut all: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    all.extend_from_slice(headers);
    let Ok(resp) = http::send("POST", resource, &all, body, timeout, Dial::Named) else {
        return Answer::Unreached;
    };
    match resp
        .header("www-authenticate")
        .and_then(parse_resource_metadata)
    {
        Some(url) => Answer::Challenge(url),
        None => Answer::NoChallenge,
    }
}

/// Extract the `resource_metadata` URL from a `WWW-Authenticate` header value
/// (RFC 9728 §5.1). Accepts a quoted or bare param value.
fn parse_resource_metadata(header: &str) -> Option<String> {
    let idx = header.find("resource_metadata")?;
    let rest = header[idx + "resource_metadata".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let val = if let Some(q) = rest.strip_prefix('"') {
        q.split('"').next()?
    } else {
        rest.split([',', ' ', ';']).next()?
    };
    (!val.is_empty()).then(|| val.to_string())
}

/// The RFC 9728 §3.1 well-known metadata URLs for a resource, in the order
/// MCP's authorization spec tries them: the suffix inserted between the
/// host and the resource's path
/// (`https://rs.example/.well-known/oauth-protected-resource/mcp`), then the
/// origin's root (`…/.well-known/oauth-protected-resource`). A resource with
/// no path has only the second.
fn well_known_urls(resource: &str) -> Vec<String> {
    let suffix = "oauth-protected-resource";
    let mut out: Vec<String> = Vec::new();
    if let Some(inserted) = http::insert_well_known(resource, suffix) {
        out.push(inserted);
    }
    if let Some((origin, _)) = http::split_origin(resource) {
        let root = format!("{origin}/.well-known/{suffix}");
        if !out.contains(&root) {
            out.push(root);
        }
    }
    out
}

/// Whether `candidate` has the same scheme, host and port as `base`. Used to
/// bound a server-named metadata URL to the resource it describes.
fn same_origin(base: &str, candidate: &str) -> bool {
    let (Ok(b), Ok(c)) = (Url::parse(base), Url::parse(candidate)) else {
        return false;
    };
    b.scheme.eq_ignore_ascii_case(&c.scheme)
        && b.host.eq_ignore_ascii_case(&c.host)
        && b.port == c.port
}

/// GET + parse JSON, tolerant of a non-2xx / bad body (→ `None`).
fn get_json<T: serde::de::DeserializeOwned>(url: &str, timeout: Duration) -> Option<T> {
    let resp = http::get(url, &[("Accept", "application/json")], timeout, Dial::Named).ok()?;
    if !resp.is_success() {
        return None;
    }
    serde_json::from_slice(&resp.body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resource_metadata_reads_quoted_and_bare() {
        // The canonical RFC 9728 §5.1 challenge (quoted, with trailing params).
        let h = r#"Bearer resource_metadata="https://rs.example/.well-known/oauth-protected-resource", error="invalid_token""#;
        assert_eq!(
            parse_resource_metadata(h).as_deref(),
            Some("https://rs.example/.well-known/oauth-protected-resource")
        );
        // A bare (unquoted) value terminated by a comma.
        let h = "Bearer resource_metadata=https://rs.example/meta, error=invalid_token";
        assert_eq!(
            parse_resource_metadata(h).as_deref(),
            Some("https://rs.example/meta")
        );
        // No resource_metadata param → None (fall back to well-known).
        assert_eq!(parse_resource_metadata("Bearer realm=\"x\""), None);
    }

    /// A `resource_metadata` URL the server names off the resource's own origin
    /// is not honoured. Without this, a hostile MCP server answers its 401 with
    /// `resource_metadata="http://169.254.169.254/latest/meta-data/"`, agentd
    /// GETs it blind, and whatever comes back names the issuer for every
    /// authenticated request that follows.
    #[test]
    fn a_cross_origin_metadata_url_is_not_honoured() {
        let res = "https://mcp.example/mcp";
        assert!(same_origin(res, "https://mcp.example/.well-known/x"));
        assert!(same_origin(res, "https://MCP.EXAMPLE/other"));
        // Different host, different scheme, and a different port are all other
        // origins — the last is the one an attacker reaches for on a shared host.
        assert!(!same_origin(
            res,
            "http://169.254.169.254/latest/meta-data/"
        ));
        assert!(!same_origin(res, "https://evil.example/meta"));
        assert!(!same_origin(res, "http://mcp.example/meta"));
        assert!(!same_origin(res, "https://mcp.example:8443/meta"));
        // A loopback development server keeps working: its resource is loopback
        // too, so its own metadata URL is same-origin.
        assert!(same_origin(
            "http://127.0.0.1:8080/mcp",
            "http://127.0.0.1:8080/.well-known/oauth-protected-resource"
        ));
        assert!(!same_origin(
            "http://127.0.0.1:8080/mcp",
            "http://127.0.0.1:9090/x"
        ));
    }

    /// RFC 9728 §3.1: the path-inserted URL first, then the root; the port
    /// is kept, a terminating slash dropped, and a resource without a path
    /// has the root only.
    #[test]
    fn well_known_urls_insert_the_suffix_before_the_path_then_try_the_root() {
        assert_eq!(
            well_known_urls("https://mcp.example/mcp"),
            [
                "https://mcp.example/.well-known/oauth-protected-resource/mcp",
                "https://mcp.example/.well-known/oauth-protected-resource",
            ]
        );
        assert_eq!(
            well_known_urls("https://mcp.example:8443/a/b/"),
            [
                "https://mcp.example:8443/.well-known/oauth-protected-resource/a/b",
                "https://mcp.example:8443/.well-known/oauth-protected-resource",
            ]
        );
        for bare in ["https://mcp.example", "https://mcp.example/"] {
            assert_eq!(
                well_known_urls(bare),
                ["https://mcp.example/.well-known/oauth-protected-resource"],
                "{bare}"
            );
        }
    }
}
