// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP registry client (RFC 0055 §4.1): plain HTTPS + JSON against the
//! API the MCP project's registry serves (`v0.1`) and any registry that
//! serves the same.
//!
//! - `GET {base}/v0.1/servers/{name}/versions/{version}` — one record,
//!   `{"server": <server.json>, "_meta": {...}}`.
//! - `GET {base}/v0.1/servers?search=…&version=latest&limit=…` — matching
//!   records, `{"servers": [...], "metadata": {...}}`.
//!
//! A record is untrusted input about where to connect. [`validate_record`]
//! holds it to what agentd will dial: the name asked for, a status that is
//! not `deleted`, a `streamable-http` remote (agentd's client speaks no other
//! transport, and starts no local process, so a server with only `packages`
//! is refused), an `https://` URL to a public address unless the registry
//! entry says `allow_private`, and remote headers whose `{variable}` values
//! the operator fills — a registry never supplies a credential.

use crate::config::RegistryDial;
use serde_json::{Value, json};
use std::time::Duration;

/// The API revision agentd speaks.
pub const API: &str = "v0.1";
/// The `_meta` key the official registry puts its own record of a server
/// under (status, publication times).
pub const OFFICIAL_META: &str = "io.modelcontextprotocol.registry/official";
/// The MCP project's registry: what `agentd mcp registry search` asks when
/// no registry is named.
pub const OFFICIAL: &str = "https://registry.modelcontextprotocol.io";

/// One registry's API, as a request is made to it.
#[derive(Debug, Clone)]
pub struct Api {
    /// The API base, without a trailing `/`.
    pub base: String,
    /// Secret-free header templates, resolved per request.
    pub headers: Vec<(String, String)>,
    pub auth: Option<crate::config::AuthSpec>,
    /// The credential cache key (`registry:<name>`).
    pub target: String,
    pub timeout: Duration,
}

impl Api {
    /// The API a registry server is resolved through.
    pub fn of(dial: &RegistryDial) -> Api {
        Api {
            base: dial.base.clone(),
            headers: dial.headers.clone(),
            auth: dial.auth.clone(),
            target: format!("registry:{}", dial.registry),
            timeout: dial.timeout,
        }
    }

    /// The API at `base`, with no credential.
    pub fn bare(base: &str, timeout: Duration) -> Api {
        Api {
            base: base.trim_end_matches('/').to_string(),
            headers: Vec::new(),
            auth: None,
            target: format!("registry:{base}"),
            timeout,
        }
    }

    /// GET `path` (after `/v0.1`) and read the JSON answer.
    fn get(&self, path: &str) -> Result<Value, String> {
        let url = format!("{}/{API}{path}", self.base);
        let mut headers = crate::mcp::auth::resolve_headers(&self.headers)?;
        headers.push(("Accept".into(), "application/json".into()));
        #[cfg(feature = "oauth")]
        if let Some(a) = &self.auth {
            let signer = if a.kind == "aws" {
                Some(crate::auth::aws::SigV4Signer::from_spec(a, &self.target)?
                    as std::sync::Arc<dyn ::mcp::http::RequestSigner>)
            } else {
                crate::auth::device::signer_for(a, &self.target, self.timeout)?
            };
            if let Some(signer) = signer {
                let parsed = crate::net::http::Url::parse(&url).map_err(|e| e.to_string())?;
                headers.extend(signer.sign(
                    "GET",
                    &::mcp::http::authority_of(&url),
                    &parsed.path,
                    &[],
                )?);
            }
        }
        #[cfg(not(feature = "oauth"))]
        if self.auth.is_some() {
            return Err(format!(
                "registry {}: its `auth` needs the oauth feature (build with --features oauth)",
                self.base
            ));
        }
        let refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let reply = get(&url, &refs, self.timeout).map_err(|e| format!("registry {url}: {e}"))?;
        if reply.status == 404 {
            return Err(format!("registry {url}: not found (HTTP 404)"));
        }
        if !reply.is_success() {
            return Err(format!("registry {url}: HTTP {}", reply.status));
        }
        serde_json::from_slice(&reply.body)
            .map_err(|e| format!("registry {url}: the answer is not JSON: {e}"))
    }

    /// One record: `{"server": …, "_meta": …}` for `name` at `version`.
    pub fn version(&self, name: &str, version: &str) -> Result<Value, String> {
        self.get(&format!(
            "/servers/{}/versions/{}",
            encode(name),
            encode(version)
        ))
    }

    /// The latest records whose names match `query` (at least 3 characters,
    /// the registry's rule), at most `limit`.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Value>, String> {
        if query.trim().chars().count() < 3 {
            return Err("a registry search term is at least 3 characters".into());
        }
        let v = self.get(&format!(
            "/servers?search={}&version=latest&limit={}",
            encode(query.trim()),
            limit.clamp(1, 100)
        ))?;
        Ok(v.get("servers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

/// One GET to an operator-configured registry, dialled by name: `https://`,
/// or plaintext `http://` to a loopback host (a registry under test). No
/// redirect is followed.
fn get(
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<crate::net::http::Response, String> {
    use crate::net::http::{self, Url};
    let u = Url::parse(url).map_err(|e| e.to_string())?;
    if !u.is_tls() && !http::is_loopback_host(&u.host) {
        return Err("plaintext http:// is allowed for a loopback host only; use https://".into());
    }
    let tcp = http::connect_tcp(&u.host, u.port, timeout).map_err(|e| format!("connect: {e}"))?;
    let _ = tcp.set_read_timeout(Some(timeout));
    let _ = tcp.set_write_timeout(Some(timeout));
    let mut stream: Box<dyn http::Stream> = if u.is_tls() {
        #[cfg(feature = "tls")]
        {
            Box::new(crate::net::tls::connect(tcp, &u.host, None).map_err(|e| format!("tls: {e}"))?)
        }
        #[cfg(not(feature = "tls"))]
        {
            return Err("https requires building with --features tls".into());
        }
    } else {
        Box::new(tcp)
    };
    http::send(
        stream.as_mut(),
        &u.host_header(),
        "GET",
        &u.path,
        headers,
        &[],
    )
    .map_err(|e| e.to_string())
}

/// Percent-encode everything but the unreserved characters (RFC 3986).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A record's status (`active` when it says none).
pub fn status(record: &Value) -> &str {
    record
        .pointer(&format!(
            "/_meta/{}/status",
            OFFICIAL_META.replace('/', "~1")
        ))
        .and_then(Value::as_str)
        .unwrap_or("active")
}

/// What a resolution found: the endpoint to dial and the headers to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub name: String,
    /// The version the record is, exactly.
    pub version: String,
    pub url: String,
    /// Secret-free templates: the operator's values, `{{secret:…}}` as
    /// anywhere.
    pub headers: Vec<(String, String)>,
    pub status: String,
    /// Remotes passed over, each said (`sse https://…: …`).
    pub skipped: Vec<String>,
    /// The document's `src`, beside the operator's own entry, that the
    /// record's remotes do not hold: the document's declaration is refused,
    /// with this; the operator's server is not.
    pub document_refused: Option<String>,
}

impl Resolution {
    pub fn deprecated(&self) -> bool {
        self.status == "deprecated"
    }
}

/// Hold the record `record` (an envelope: `{"server", "_meta"}`) for the
/// name `asked` to what agentd dials, and fill its remote's headers from the
/// operator's `operator` headers. Pure: the SSRF guard on the URL's address
/// is [`guard_url`]'s.
pub fn validate_record(
    asked: &str,
    record: &Value,
    dial: &RegistryDial,
    operator: &[(String, String)],
) -> Result<Resolution, String> {
    let at = format!("mcp registry {:?}: server {asked:?}", dial.registry);
    let server = record
        .get("server")
        .filter(|s| s.is_object())
        .ok_or_else(|| format!("{at}: the registry's answer holds no server record"))?;
    let name = server.get("name").and_then(Value::as_str).unwrap_or("");
    if name != asked {
        return Err(format!(
            "{at}: the record is named {name:?}, not the name asked for — refused"
        ));
    }
    let status = status(record).to_string();
    if status == "deleted" {
        return Err(format!("{at}: the registry marks it deleted — refused"));
    }
    let version = server
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if version.is_empty() {
        return Err(format!("{at}: the record names no version"));
    }
    if dial.version != "latest" && version != dial.version {
        return Err(format!(
            "{at}: version {:?} was asked for and the record is version {version:?} — refused",
            dial.version
        ));
    }
    let mut skipped = Vec::new();
    let mut candidates = Vec::new();
    for r in server
        .get("remotes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = r.get("type").and_then(Value::as_str).unwrap_or("");
        let url = r.get("url").and_then(Value::as_str).unwrap_or("");
        match kind {
            "streamable-http" => candidates.push(r),
            // The 2024-11-05 HTTP+SSE transport: agentd's client speaks
            // Streamable HTTP alone.
            "sse" => skipped.push(format!(
                "sse {url}: agentd's MCP client speaks Streamable HTTP, not the 2024-11-05 HTTP+SSE transport"
            )),
            other => skipped.push(format!("{other} {url}: not a transport agentd speaks")),
        }
    }
    let mut document_refused = None;
    if let Some(doc) = &dial.document_src {
        let src = &doc.src;
        let norm = |u: &str| u.trim().trim_end_matches('/').to_string();
        let all: Vec<&str> = server
            .get("remotes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| r.get("url").and_then(Value::as_str))
            .collect();
        let held = all.iter().any(|u| norm(u) == norm(src));
        if !held {
            let why = format!(
                "{at}: the document's src {src:?} is none of the record's remotes {all:?} — the document and the registry disagree, refused"
            );
            // The operator's own entry is not the document's to refuse:
            // only the document's block is (RFC 0055 §4.2).
            if !doc.operator {
                return Err(why);
            }
            document_refused = Some(why);
        } else if !doc.operator {
            // The document's own server dials the remote it named.
            candidates.retain(|r| {
                r.get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|u| norm(u) == norm(src))
            });
        }
    }
    let Some(remote) = candidates.first() else {
        let packages = server
            .get("packages")
            .and_then(Value::as_array)
            .is_some_and(|p| !p.is_empty());
        return Err(if packages && skipped.is_empty() {
            format!(
                "{at}: the record lists only packages (stdio, OCI, npm …) — agentd speaks MCP over HTTP and starts no local process, so it is refused"
            )
        } else if skipped.is_empty() {
            format!("{at}: the record lists no remote to dial")
        } else {
            format!("{at}: no remote agentd can dial ({})", skipped.join("; "))
        });
    };
    let url = remote
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if url.contains('{') {
        return Err(format!(
            "{at}: its remote URL {url:?} is a template, which agentd does not fill"
        ));
    }
    check_url(&url, dial.allow_private).map_err(|e| format!("{at}: {e}"))?;
    let headers = fill_headers(remote, operator).map_err(|e| format!("{at}: {e}"))?;
    Ok(Resolution {
        name: name.to_string(),
        version,
        url,
        headers,
        status,
        skipped,
        document_refused,
    })
}

/// A remote URL's scheme: `https://`, or with `allow_private` also a
/// loopback `http://`.
fn check_url(url: &str, allow_private: bool) -> Result<(), String> {
    let u = crate::net::http::Url::parse(url).map_err(|e| format!("remote url {url:?}: {e}"))?;
    if u.is_tls() {
        return Ok(());
    }
    if allow_private && crate::net::http::is_loopback_host(&u.host) {
        return Ok(());
    }
    Err(format!(
        "remote url {url:?} is not https:// — a registry's server is dialed over TLS (a loopback http:// only where the registry entry sets allow_private)"
    ))
}

/// The SSRF guard on a resolved remote: its host resolves to public
/// addresses only, unless the registry entry sets `allow_private`.
pub fn guard_url(url: &str, allow_private: bool) -> Result<(), String> {
    let u = crate::net::http::Url::parse(url).map_err(|e| format!("remote url {url:?}: {e}"))?;
    crate::net::ssrf::guard_host(&u.host, allow_private)
        .map_err(|e| format!("remote url {url}: {e} (set allow_private on the registry entry for an internal registry)"))
}

/// The headers a remote declares, as sent: an operator header of the same
/// name supplies one whole; a `{variable}` in a declared value is filled
/// from the operator header named after the variable (which is then not sent
/// itself); a declared value with no variable is sent unless it is marked
/// secret — a registry never supplies a credential. A required header left
/// unfilled refuses the server. Every other operator header is sent as is.
pub fn fill_headers(
    remote: &Value,
    operator: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    let find = |n: &str| {
        operator
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.clone())
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let mut consumed: Vec<String> = Vec::new();
    for h in remote
        .get("headers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = h.get("name").and_then(Value::as_str) else {
            continue;
        };
        let required = h
            .get("isRequired")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let secret = h.get("isSecret").and_then(Value::as_bool).unwrap_or(false);
        let declared = h.get("value").and_then(Value::as_str);
        let value = if let Some(v) = find(name) {
            Some(v)
        } else if let Some(template) = declared {
            let vars = variables(template);
            if vars.is_empty() {
                (!secret).then(|| template.to_string())
            } else {
                let mut filled = template.to_string();
                let mut all = true;
                for var in &vars {
                    match find(var) {
                        Some(v) => {
                            filled = filled.replace(&format!("{{{var}}}"), &v);
                            consumed.push(var.to_ascii_lowercase());
                        }
                        None => all = false,
                    }
                }
                all.then_some(filled)
            }
        } else {
            None
        };
        match value {
            Some(v) => out.push((name.to_string(), v)),
            None if required => {
                let what = match declared.map(variables) {
                    Some(v) if !v.is_empty() => format!(
                        "set {} in the mcp.servers entry's headers",
                        v.iter()
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    _ => format!("set {name:?} in the mcp.servers entry's headers"),
                };
                return Err(format!(
                    "its remote requires the header {name:?}, which the operator has not filled — {what}"
                ));
            }
            None => {}
        }
    }
    for (k, v) in operator {
        if consumed.contains(&k.to_ascii_lowercase())
            || out.iter().any(|(o, _)| o.eq_ignore_ascii_case(k))
        {
            continue;
        }
        out.push((k.clone(), v.clone()));
    }
    Ok(out)
}

/// The `{variable}` names in a header value template.
fn variables(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        let after = &rest[i + 1..];
        // `{{secret:…}}` is agentd's, never a registry variable.
        if after.starts_with('{') {
            rest = after.trim_start_matches('{');
            continue;
        }
        match after.find('}') {
            Some(j) if j > 0 && !after[..j].contains(['{', ' ']) => {
                out.push(after[..j].to_string());
                rest = &after[j + 1..];
            }
            _ => break,
        }
    }
    out
}

/// What `mcp.registry.search` and the CLI show of a record: name, title,
/// description, version and the kinds of its remotes (and their URLs).
pub fn summary(record: &Value) -> Value {
    let server = record.get("server").unwrap_or(record);
    let s = |k: &str| server.get(k).and_then(Value::as_str);
    let remotes: Vec<Value> = server
        .get("remotes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| json!({"type": r.get("type"), "url": r.get("url")}))
        .collect();
    let packages = server
        .get("packages")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    json!({
        "name": s("name"),
        "title": s("title"),
        "description": s("description"),
        "version": s("version"),
        "remotes": remotes,
        "packages": packages,
        "status": status(record),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dial() -> RegistryDial {
        RegistryDial {
            registry: "official".into(),
            base: "https://registry.example".into(),
            server: "io.acme/kb".into(),
            version: "latest".into(),
            ..Default::default()
        }
    }

    fn record(server: Value, status: &str) -> Value {
        json!({"server": server, "_meta": {OFFICIAL_META: {"status": status}}})
    }

    fn remote(url: &str) -> Value {
        json!({"name": "io.acme/kb", "version": "1.2.0",
               "remotes": [{"type": "sse", "url": "https://kb.acme.example/sse"},
                           {"type": "streamable-http", "url": url}]})
    }

    #[test]
    fn a_streamable_http_remote_is_chosen_and_sse_is_skipped_said() {
        let r = validate_record(
            "io.acme/kb",
            &record(remote("https://kb.acme.example/mcp"), "active"),
            &dial(),
            &[],
        )
        .unwrap();
        assert_eq!(r.url, "https://kb.acme.example/mcp");
        assert_eq!(r.version, "1.2.0");
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].starts_with("sse "), "{:?}", r.skipped);
        assert!(!r.deprecated());
    }

    #[test]
    fn records_agentd_cannot_or_must_not_dial_are_refused() {
        let d = dial();
        // A name other than the one asked for.
        let e = validate_record(
            "io.acme/other",
            &record(remote("https://a.example/mcp"), "active"),
            &d,
            &[],
        )
        .unwrap_err();
        assert!(e.contains("not the name asked for"), "{e}");
        // Deleted refuses; deprecated connects.
        let e = validate_record(
            "io.acme/kb",
            &record(remote("https://a.example/mcp"), "deleted"),
            &d,
            &[],
        )
        .unwrap_err();
        assert!(e.contains("deleted"), "{e}");
        assert!(
            validate_record(
                "io.acme/kb",
                &record(remote("https://a.example/mcp"), "deprecated"),
                &d,
                &[]
            )
            .unwrap()
            .deprecated()
        );
        // Packages only.
        let pkgs = json!({"name": "io.acme/kb", "version": "1",
                          "packages": [{"registryType": "npm", "identifier": "@acme/kb"}]});
        let e = validate_record("io.acme/kb", &record(pkgs, "active"), &d, &[]).unwrap_err();
        assert!(
            e.contains("only packages") && e.contains("starts no local process"),
            "{e}"
        );
        // Only an sse remote.
        let sse = json!({"name": "io.acme/kb", "version": "1",
                         "remotes": [{"type": "sse", "url": "https://a.example/sse"}]});
        let e = validate_record("io.acme/kb", &record(sse, "active"), &d, &[]).unwrap_err();
        assert!(e.contains("no remote agentd can dial"), "{e}");
        // Plain http, unless the registry allows private addresses and the
        // host is loopback.
        let e = validate_record(
            "io.acme/kb",
            &record(remote("http://127.0.0.1:9/mcp"), "active"),
            &d,
            &[],
        )
        .unwrap_err();
        assert!(e.contains("not https://"), "{e}");
        let private = RegistryDial {
            allow_private: true,
            ..dial()
        };
        assert!(
            validate_record(
                "io.acme/kb",
                &record(remote("http://127.0.0.1:9/mcp"), "active"),
                &private,
                &[]
            )
            .is_ok()
        );
        assert!(
            validate_record(
                "io.acme/kb",
                &record(remote("http://10.0.0.1/mcp"), "active"),
                &private,
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn an_exact_version_asked_for_refuses_a_record_of_another_version() {
        let mut d = dial();
        d.version = "1.2.0".into();
        let r = validate_record(
            "io.acme/kb",
            &record(remote("https://kb.acme.example/mcp"), "active"),
            &d,
            &[],
        )
        .unwrap();
        assert_eq!(r.version, "1.2.0");
        d.version = "1.1.0".into();
        let e = validate_record(
            "io.acme/kb",
            &record(remote("https://kb.acme.example/mcp"), "active"),
            &d,
            &[],
        )
        .unwrap_err();
        assert!(
            e.contains("\"1.1.0\" was asked for") && e.contains("\"1.2.0\""),
            "{e}"
        );
    }

    fn src(src: &str, operator: bool) -> Option<crate::config::DocumentSrc> {
        Some(crate::config::DocumentSrc {
            src: src.into(),
            operator,
        })
    }

    #[test]
    fn a_document_src_must_be_one_of_the_records_remotes() {
        let mut d = dial();
        d.document_src = src("https://kb.acme.example/mcp/", false);
        let r = validate_record(
            "io.acme/kb",
            &record(remote("https://kb.acme.example/mcp"), "active"),
            &d,
            &[],
        )
        .unwrap();
        assert_eq!(r.url, "https://kb.acme.example/mcp");
        assert!(r.document_refused.is_none());
        d.document_src = src("https://evil.example/mcp", false);
        let e = validate_record(
            "io.acme/kb",
            &record(remote("https://kb.acme.example/mcp"), "active"),
            &d,
            &[],
        )
        .unwrap_err();
        assert!(e.contains("disagree"), "{e}");
    }

    /// A document's `src` beside the operator's own entry that the record
    /// does not hold refuses the document's block, not the operator's
    /// server: it resolves as it would without the document.
    #[test]
    fn a_wrong_document_src_on_the_operators_entry_refuses_only_the_document() {
        let mut d = dial();
        d.document_src = src("https://evil.example/mcp", true);
        let r = validate_record(
            "io.acme/kb",
            &record(remote("https://kb.acme.example/mcp"), "active"),
            &d,
            &[],
        )
        .expect("the operator's server resolves");
        assert_eq!(r.url, "https://kb.acme.example/mcp");
        assert!(
            r.document_refused
                .as_deref()
                .is_some_and(|e| e.contains("disagree")),
            "{r:?}"
        );
        // One the record holds is no refusal, and does not choose the
        // operator's remote.
        d.document_src = src("https://kb.acme.example/sse", true);
        let r = validate_record(
            "io.acme/kb",
            &record(remote("https://kb.acme.example/mcp"), "active"),
            &d,
            &[],
        )
        .unwrap();
        assert!(r.document_refused.is_none());
        assert_eq!(r.url, "https://kb.acme.example/mcp");
    }

    #[test]
    fn header_variables_are_the_operators_to_fill() {
        let remote = json!({"type": "streamable-http", "url": "https://x.example/mcp", "headers": [
            {"name": "Authorization", "value": "Bearer {token}", "isRequired": true, "isSecret": true},
            {"name": "X-Org", "value": "acme"},
            {"name": "X-Key", "value": "baked-in", "isSecret": true},
            {"name": "X-Optional", "value": "{opt}"}
        ]});
        let op = vec![
            ("token".to_string(), "{{secret:GH_TOKEN}}".to_string()),
            ("X-Extra".to_string(), "1".to_string()),
        ];
        let h = fill_headers(&remote, &op).unwrap();
        assert_eq!(
            h,
            vec![
                ("Authorization".into(), "Bearer {{secret:GH_TOKEN}}".into()),
                ("X-Org".into(), "acme".into()),
                ("X-Extra".into(), "1".into()),
            ],
            "the variable is consumed, a registry secret never sent, an unfilled optional dropped"
        );
        // A required header left unfilled refuses, naming the variable.
        let e = fill_headers(&remote, &[]).unwrap_err();
        assert!(
            e.contains("Authorization") && e.contains("\"token\""),
            "{e}"
        );
        // An operator header of the same name supplies it whole.
        let h = fill_headers(&remote, &[("authorization".into(), "Basic x".into())]).unwrap();
        assert_eq!(h[0], ("Authorization".into(), "Basic x".into()));
        assert_eq!(variables("Bearer {{secret:X}} {a} {b}"), vec!["a", "b"]);
    }

    #[test]
    fn names_are_encoded_into_one_path_segment() {
        assert_eq!(
            encode("io.github.github/github-mcp-server"),
            "io.github.github%2Fgithub-mcp-server"
        );
        assert_eq!(encode("1.2.0"), "1.2.0");
    }
}
