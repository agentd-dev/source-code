// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 9728 **OAuth 2.0 Protected Resource Metadata** discovery. When an MCP
//! server's `auth: { kind: oauth2 }` block names no `issuer` and no
//! `token_url`, agentd asks the server — at `--login`, and at the first
//! request the daemon needs a token for: an unauthenticated request draws a
//! `401 WWW-Authenticate: Bearer resource_metadata="…"`, and that metadata
//! document lists the `authorization_servers`, the issuer to run RFC 8414 /
//! OIDC discovery against.
//!
//! The document is used only when its `resource` is the server's URL byte
//! for byte (RFC 9728 §3.3, §7.3): metadata describing another resource is
//! refused, not used. The authorization server it names is a URL the SERVER
//! chose, so it is held to [`reach`]: on the server's own origin it is as
//! trusted as the server the operator configured; anywhere else it must be
//! HTTPS, is dialled through the SSRF guard, and under `security.egress:
//! closed` must be in the catalog. The endpoints that server's metadata
//! names must be on its own origin ([`same_origin`]).

use std::time::Duration;

use serde::Deserialize;

use crate::auth::http::{self, Dial};
use crate::net::http::Url;

/// The RFC 9728 protected-resource metadata document (the fields consumed).
#[derive(Debug, Clone, Deserialize)]
pub struct ResourceMetadata {
    /// The resource the document describes.
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    /// The DPoP proof algorithms the resource takes (RFC 9728 §2).
    #[serde(default)]
    pub dpop_signing_alg_values_supported: Vec<String>,
    /// The resource takes DPoP-bound access tokens alone (RFC 9728 §2).
    #[serde(default)]
    pub dpop_bound_access_tokens_required: bool,
}

/// The authorization server `resource`'s RFC 9728 metadata names, and how
/// it may be dialled. `Ok(None)` when the server offers no metadata at all
/// (the caller then needs an explicit `issuer`); `Err` when the metadata it
/// offers is refused — it describes another resource, or names an
/// authorization server agentd may not reach.
pub fn discover_issuer(
    resource: &str,
    catalog: Option<&[String]>,
    timeout: Duration,
) -> Result<Option<(String, Dial)>, String> {
    // Prefer the metadata URL the server names in its 401 challenge; else the
    // well-known locations derived from the resource (RFC 9728 §3.1): the
    // suffix inserted before the resource's path, then at the origin's root.
    // A `resource_metadata` URL the SERVER named is only honoured when it lives
    // on the resource's own origin. RFC 9728 metadata describes that resource,
    // so same-origin is where it belongs — and the alternative is a blind GET at
    // an address a hostile MCP server picks. Cross-origin, we ignore the
    // challenge and use the well-known location instead of refusing outright,
    // so a misconfigured server degrades rather than breaks.
    let named = challenge_metadata_url(resource, timeout).filter(|u| same_origin(resource, u));
    let candidates = match named {
        Some(u) => vec![u],
        None => well_known_urls(resource),
    };
    let mut refused = None;
    for url in &candidates {
        let Some(meta) = get_json::<ResourceMetadata>(url, timeout) else {
            continue;
        };
        // RFC 9728 §3.3: a document whose `resource` is not the URL it was
        // derived from MUST NOT be used.
        if meta.resource.as_deref() != Some(resource) {
            refused = Some(format!(
                "oauth: {url} describes the resource {:?}, not {resource:?} (RFC 9728 §3.3); its authorization servers are not used",
                meta.resource.as_deref().unwrap_or("")
            ));
            continue;
        }
        let Some(issuer) = meta
            .authorization_servers
            .into_iter()
            .find(|s| !s.trim().is_empty())
        else {
            return Ok(None);
        };
        let dial = reach(&issuer, resource, catalog)?;
        return Ok(Some((issuer, dial)));
    }
    match refused {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

/// The RFC 9728 metadata of the server at `endpoint`, read from the
/// well-known locations on its own origin — the suffix inserted before its
/// path, then the root — with no request to the server itself: what a
/// delegated server's pins are confirmed against (RFC 0044 §5.6), before
/// agentd has sent it anything a challenge could answer. The first document
/// found, or why none was.
pub fn resource_metadata(endpoint: &str, timeout: Duration) -> Result<ResourceMetadata, String> {
    let urls = well_known_urls(endpoint);
    urls.iter()
        .find_map(|u| get_json::<ResourceMetadata>(u, timeout))
        .ok_or_else(|| {
            format!(
                "no RFC 9728 protected-resource metadata at {}",
                urls.join(" or ")
            )
        })
}

/// How a URL a server chose — the authorization server RFC 9728 metadata
/// names, or an endpoint that server's metadata names — may be dialled for
/// a client of `resource`, the server the operator configured. On
/// `resource`'s own origin it is dialled as the server is; anywhere else
/// only over HTTPS, through the SSRF guard (no loopback, private or
/// link-local address), and — when `catalog` is given, under
/// `security.egress: closed` — only when a catalog entry covers it.
pub fn reach(url: &str, resource: &str, catalog: Option<&[String]>) -> Result<Dial, String> {
    if same_origin(resource, url) {
        return Ok(Dial::Named);
    }
    let parsed = Url::parse(url).map_err(|e| format!("oauth: {url:?}: {e}"))?;
    if !parsed.is_tls() {
        return Err(format!(
            "oauth: the authorization server {url} that {resource} named is not an https URL; a server-chosen URL off the server's own origin must be"
        ));
    }
    if let Some(entries) = catalog
        && !entries.iter().any(|e| covers(e, url))
    {
        return Err(format!(
            "oauth: security.egress is closed and the authorization server {url} that {resource} named matches no services: catalog entry — catalog it to allow it"
        ));
    }
    Ok(Dial::Vetted {
        allow_private: false,
    })
}

/// Whether the catalog entry `entry` covers `url`: the same scheme and
/// authority, and a path that extends the entry's on a segment boundary.
fn covers(entry: &str, url: &str) -> bool {
    let (Some((eo, ep)), Some((uo, up))) = (http::split_origin(entry), http::split_origin(url))
    else {
        return false;
    };
    let ep = ep.split(['?', '#']).next().unwrap_or_default();
    let up = up.split(['?', '#']).next().unwrap_or_default();
    eo.eq_ignore_ascii_case(uo)
        && (ep.is_empty()
            || ep == "/"
            || up == ep
            || (up.starts_with(ep)
                && (ep.ends_with('/') || up.as_bytes().get(ep.len()) == Some(&b'/'))))
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

/// Whether `candidate` has the same scheme, host and port as `base`.
pub fn same_origin(base: &str, candidate: &str) -> bool {
    let (Ok(b), Ok(c)) = (Url::parse(base), Url::parse(candidate)) else {
        return false;
    };
    b.scheme.eq_ignore_ascii_case(&c.scheme)
        && b.host.eq_ignore_ascii_case(&c.host)
        && b.port == c.port
}

/// GET + parse JSON, tolerant of a non-2xx / bad body (→ `None`). Each URL
/// is on the configured server's origin.
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

    /// A loopback MCP server: `401` on every POST, and at the path-inserted
    /// RFC 9728 URL the document `doc` (`{base}` is its own URL).
    fn spawn_prm(doc: &'static str) -> String {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let me = base.clone();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).ok();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                let _ = std::io::Read::read_exact(&mut r, &mut body);
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, body) = if path == "/.well-known/oauth-protected-resource/mcp" {
                    ("200 OK", doc.replace("{base}", &me))
                } else if line.starts_with("POST") {
                    ("401 Unauthorized", String::new())
                } else {
                    ("404 Not Found", String::new())
                };
                let mut w = c;
                let _ = write!(
                    w,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        base
    }

    /// RFC 9728 §3.3: metadata whose `resource` is not the server's URL byte
    /// for byte is not used — refused, its authorization servers ignored —
    /// and metadata that names it gives its first authorization server.
    #[test]
    fn metadata_for_another_resource_is_refused() {
        let t = Duration::from_secs(5);
        let base = spawn_prm(r#"{"resource":"{base}/mcp","authorization_servers":["{base}/as"]}"#);
        let (issuer, dial) = discover_issuer(&format!("{base}/mcp"), None, t)
            .unwrap()
            .unwrap();
        assert_eq!(issuer, format!("{base}/as"));
        assert_eq!(dial, Dial::Named, "on the server's own origin");
        for doc in [
            r#"{"resource":"{base}/other","authorization_servers":["{base}/as"]}"#,
            r#"{"resource":"{base}/mcp/","authorization_servers":["{base}/as"]}"#,
            r#"{"authorization_servers":["{base}/as"]}"#,
        ] {
            let base = spawn_prm(doc);
            let e = discover_issuer(&format!("{base}/mcp"), None, t).unwrap_err();
            assert!(e.contains("RFC 9728 §3.3"), "{doc}: {e}");
        }
        // No metadata at all is not a refusal: the caller needs an issuer.
        let base = spawn_prm("not json");
        assert_eq!(discover_issuer(&format!("{base}/mcp"), None, t), Ok(None));
    }

    /// A server-chosen authorization server off the server's origin is
    /// reached over HTTPS only, through the SSRF guard, and — under closed
    /// egress — only inside the catalog; on the server's origin it is
    /// dialled as the server is.
    #[test]
    fn a_server_named_authorization_server_is_held_to_https_the_guard_and_the_catalog() {
        let mcp = "https://mcp.example/mcp";
        assert_eq!(reach("https://mcp.example/as", mcp, None), Ok(Dial::Named));
        assert_eq!(
            reach("https://auth.example/tenant", mcp, None),
            Ok(Dial::Vetted {
                allow_private: false
            })
        );
        let e = reach("http://auth.example/tenant", mcp, None).unwrap_err();
        assert!(e.contains("not an https URL"), "{e}");
        let e = reach("http://169.254.169.254/latest", mcp, None).unwrap_err();
        assert!(e.contains("not an https URL"), "{e}");
        let catalog = vec!["https://auth.example/tenant".to_string()];
        assert!(reach("https://auth.example/tenant", mcp, Some(&catalog)).is_ok());
        assert!(reach("https://auth.example/tenant/v2", mcp, Some(&catalog)).is_ok());
        let e = reach("https://auth.example/tenantX", mcp, Some(&catalog)).unwrap_err();
        assert!(e.contains("egress is closed"), "{e}");
        let e = reach("https://evil.example/", mcp, Some(&catalog)).unwrap_err();
        assert!(e.contains("egress is closed"), "{e}");
        // A loopback development server's own origin.
        assert_eq!(
            reach(
                "http://127.0.0.1:8080/as",
                "http://127.0.0.1:8080/mcp",
                None
            ),
            Ok(Dial::Named)
        );
        assert!(
            reach(
                "http://127.0.0.1:9090/as",
                "http://127.0.0.1:8080/mcp",
                None
            )
            .is_err()
        );
    }
}
