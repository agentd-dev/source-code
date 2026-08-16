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
    let meta_url =
        challenge_metadata_url(resource, timeout).or_else(|| well_known_url(resource))?;
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
