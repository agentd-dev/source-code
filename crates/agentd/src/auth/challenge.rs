// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 9728 **OAuth 2.0 Protected Resource Metadata** discovery (RFC 0031 §7 /
//! rollout P2). When an MCP `auth: { kind: oauth2 }` block names no `issuer`,
//! agentd probes the server before login: an unauthenticated request draws a
//! `401 WWW-Authenticate: Bearer resource_metadata="…"`, and that metadata
//! document lists the `authorization_servers` — the issuer to run OIDC / RFC 8414
//! discovery against. Best-effort: any failure returns `None`, so an explicit
//! `issuer` still wins and the caller reports the usual "issuer required" error.

use std::time::Duration;

use serde::Deserialize;

use crate::auth::oauth2;
use crate::net::http::{self, Url};

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
    // well-known location derived from the resource origin (RFC 9728 §3.1).
    // A `resource_metadata` URL the SERVER named is only honoured when it lives
    // on the resource's own origin. RFC 9728 metadata describes that resource,
    // so same-origin is where it belongs — and the alternative is a blind GET at
    // an address a hostile MCP server picks, whose `authorization_servers` answer
    // then chooses the issuer for the OIDC discovery and token requests that
    // follow. Cross-origin, we ignore the challenge and use the well-known
    // location instead of refusing outright, so a misconfigured server degrades
    // rather than breaks. Same-origin (rather than the SSRF classifier) keeps a
    // loopback development server working, since its resource is loopback too.
    let meta_url = challenge_metadata_url(resource, timeout)
        .filter(|u| same_origin(resource, u))
        .or_else(|| well_known_url(resource))?;
    let meta: ResourceMetadata = get_json(&meta_url, timeout)?;
    meta.authorization_servers
        .into_iter()
        .find(|s| !s.trim().is_empty())
}

/// Probe the resource with one unauthenticated MCP request; on a `401`, return
/// the `resource_metadata` URL named in the `WWW-Authenticate` challenge.
fn challenge_metadata_url(resource: &str, timeout: Duration) -> Option<String> {
    let url = Url::parse(resource).ok()?;
    let mut stream = oauth2::connect(&url, timeout).ok()?;
    // A minimal JSON-RPC `initialize` is the canonical MCP request; an
    // unauthenticated server answers 401 with the challenge before processing it.
    let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let resp = http::send(
        stream.as_mut(),
        &url.host_header(),
        "POST",
        &url.path,
        &[
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
        ],
        body,
    )
    .ok()?;
    parse_resource_metadata(resp.header("www-authenticate")?)
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

/// The RFC 9728 §3.1 well-known metadata URL for a resource origin:
/// `{scheme}://{host[:port]}/.well-known/oauth-protected-resource`.
fn well_known_url(resource: &str) -> Option<String> {
    let url = Url::parse(resource).ok()?;
    Some(format!(
        "{}://{}/.well-known/oauth-protected-resource",
        url.scheme,
        url.host_header()
    ))
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
    let url = Url::parse(url).ok()?;
    let mut stream = oauth2::connect(&url, timeout).ok()?;
    let resp = http::send(
        stream.as_mut(),
        &url.host_header(),
        "GET",
        &url.path,
        &[("Accept", "application/json")],
        &[],
    )
    .ok()?;
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

    #[test]
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

    #[test]
    fn well_known_url_is_origin_scoped() {
        assert_eq!(
            well_known_url("https://mcp.example/mcp").as_deref(),
            Some("https://mcp.example/.well-known/oauth-protected-resource")
        );
        // The port is preserved; the resource path is dropped (origin-scoped).
        assert_eq!(
            well_known_url("https://mcp.example:8443/a/b").as_deref(),
            Some("https://mcp.example:8443/.well-known/oauth-protected-resource")
        );
    }
}
