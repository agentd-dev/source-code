// SPDX-License-Identifier: Apache-2.0
//! AAuth resource discovery (RFC 0023 §Step 3): fetch a server's
//! `/.well-known/aauth-resource.json` to learn its `access_mode` and whether it
//! requires a `content-digest` cover. Best-effort — a server without the
//! document is called proactively (agentd signs anyway and reacts to the
//! runtime `AAuth-Requirement`).

use crate::net::http::{self, Url};
use serde::Deserialize;
use std::time::Duration;

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
