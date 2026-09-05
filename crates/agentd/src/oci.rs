// SPDX-License-Identifier: AGPL-3.0-only
//! **Instruction documents from an OCI artifact registry** (RFC 0040).
//!
//! An instruction document is pushed to any OCI-compliant registry as a
//! first-class artifact (`artifactType: application/vnd.instruction.document.v1`,
//! one `text/markdown; variant=instruction` layer) and pulled here by
//! `oci://registry/repo:tag` or `…@sha256:…`. The Distribution Spec v2 is plain
//! HTTPS + JSON, so this rides the existing hand-rolled HTTP client, `serde_json`
//! and `ring`'s SHA-256 — no OCI client crate (the minimalism moat).
//!
//! What a pull does: resolve the manifest (token dance on a 401), verify the
//! manifest digest (a `@sha256:` reference MUST match; a `:tag` records the
//! digest it resolved to as the content pin), select the single instruction
//! layer, fetch the blob (following a CDN redirect without leaking the
//! registry token), and verify the blob's SHA-256 against its descriptor
//! before a byte of it is used. Content integrity is by construction; a wrong
//! digest is a refusal, never a warning.
//!
//! Credentials come from the ambient docker config (`$DOCKER_CONFIG/config.json`
//! or `~/.docker/config.json`) — static `auths` entries only. Credential-helper
//! binaries are NOT executed (agentd runs no local processes); cloud-IAM token
//! exchange (ECR SigV4) is deferred to a later phase and documented as such.

use std::io::{Read, Write};
use std::time::Duration;

use serde_json::Value;

use crate::net::http::{self, Url};

/// The dial + read deadline for each registry request.
const TIMEOUT: Duration = Duration::from_secs(30);
/// CDN blob redirects followed at most this many times.
const MAX_REDIRECTS: usize = 3;
/// A manifest or layer larger than this is refused (an instruction document is
/// text; 8 MiB is generous and bounds a hostile registry's memory cost).
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// A parsed `oci://` reference.
#[derive(Debug, Clone, PartialEq)]
pub struct OciRef {
    /// Registry host (the API host — `docker.io` is rewritten to
    /// `registry-1.docker.io`, whose credentials live under the legacy
    /// `https://index.docker.io/v1/` key).
    pub host: String,
    pub port: u16,
    /// Repository path (`acme/support-agent`).
    pub repo: String,
    /// A tag (`v3`) — mutually exclusive with `digest`.
    pub tag: Option<String>,
    /// A pinned `sha256:…` digest. When present it wins over any tag.
    pub digest: Option<String>,
}

impl OciRef {
    /// Parse `oci://registry[:port]/repo[:tag][@sha256:…]`. A digest pin wins
    /// over a tag; with neither, the tag defaults to `latest`.
    pub fn parse(uri: &str) -> Result<OciRef, String> {
        let rest = uri
            .strip_prefix("oci://")
            .ok_or_else(|| format!("not an oci:// reference: {uri}"))?;
        let (rest, digest) = match rest.split_once('@') {
            Some((r, d)) => {
                if !d.starts_with("sha256:") || d.len() != 71 || !is_hex(&d[7..]) {
                    return Err(format!(
                        "oci: malformed digest pin {d:?} (want sha256:<64 hex>)"
                    ));
                }
                (r, Some(d.to_string()))
            }
            None => (rest, None),
        };
        let (hostport, repo_ref) = rest
            .split_once('/')
            .ok_or_else(|| format!("oci: reference {uri:?} has no repository path"))?;
        // A tag is the last `:` in the repo part — but only after the final
        // path segment (a port lives in hostport, already split off).
        let (repo, tag) = match repo_ref.rsplit_once(':') {
            Some((r, t)) if !t.contains('/') && !t.is_empty() => {
                (r.to_string(), Some(t.to_string()))
            }
            _ => (repo_ref.to_string(), None),
        };
        if repo.is_empty() {
            return Err(format!("oci: reference {uri:?} has an empty repository"));
        }
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| format!("oci: bad port {p:?}"))?,
            ),
            _ => (hostport.to_string(), 443),
        };
        // Docker Hub convenience: the API host differs from the name.
        let host = if host == "docker.io" || host == "index.docker.io" {
            "registry-1.docker.io".to_string()
        } else {
            host
        };
        let tag = if digest.is_some() {
            None
        } else {
            Some(tag.unwrap_or_else(|| "latest".into()))
        };
        Ok(OciRef {
            host,
            port,
            repo,
            tag,
            digest,
        })
    }

    fn reference(&self) -> &str {
        self.digest
            .as_deref()
            .or(self.tag.as_deref())
            .unwrap_or("latest")
    }
}

/// A pulled instruction artifact: the document bytes plus the digests that pin
/// them (the manifest digest is the version pin the §7.7 freshness re-check
/// compares; the layer digest is what the bytes hashed to).
#[derive(Debug)]
pub struct Pulled {
    pub bytes: Vec<u8>,
    pub manifest_digest: String,
    pub layer_digest: String,
    pub media_type: String,
}

/// Pull an instruction artifact by `oci://` reference — resolve, verify,
/// select, fetch, verify. Every failure is a refusal naming the step.
pub fn pull(uri: &str) -> Result<Pulled, String> {
    let r = OciRef::parse(uri)?;
    let creds = docker_basic_auth(&r.host);
    let mut token: Option<String> = None;

    // 1. Manifest (with one token-dance retry on 401).
    let path = format!("/v2/{}/manifests/{}", r.repo, r.reference());
    let accept = "application/vnd.oci.image.manifest.v1+json, \
                  application/vnd.docker.distribution.manifest.v2+json";
    let resp = get_with_auth(&r, &path, accept, &mut token, creds.as_deref())?;
    if !resp.is_success() {
        return Err(format!(
            "oci: {} manifest {}: HTTP {}",
            r.host,
            r.reference(),
            resp.status
        ));
    }
    let manifest_digest = format!("sha256:{}", sha256_hex(&resp.body));
    if let Some(pin) = &r.digest
        && *pin != manifest_digest
    {
        return Err(format!(
            "oci: manifest hashes to {manifest_digest} but the reference pins {pin} — refused"
        ));
    }
    let m: Value = serde_json::from_slice(&resp.body)
        .map_err(|e| format!("oci: manifest is not JSON: {e}"))?;
    if m.get("manifests").is_some() {
        return Err(
            "oci: the reference is an image INDEX (multi-arch); an instruction \
                    artifact is a single manifest — push with a plain artifact manifest"
                .into(),
        );
    }

    // 2. Select the instruction layer.
    let layers = m["layers"].as_array().cloned().unwrap_or_default();
    let layer = select_layer(&layers)?;
    let media_type = layer["mediaType"].as_str().unwrap_or("").to_string();
    let layer_digest = layer["digest"]
        .as_str()
        .ok_or("oci: layer has no digest")?
        .to_string();
    let size = layer["size"].as_u64().unwrap_or(0) as usize;
    if size > MAX_BYTES {
        return Err(format!("oci: layer is {size} bytes (cap {MAX_BYTES})"));
    }

    // 3. Blob, following CDN redirects; verify its digest before use.
    let blob_path = format!("/v2/{}/blobs/{}", r.repo, layer_digest);
    let blob = get_blob(&r, &blob_path, &mut token, creds.as_deref())?;
    let got = format!("sha256:{}", sha256_hex(&blob));
    if got != layer_digest {
        return Err(format!(
            "oci: blob hashes to {got} but the manifest says {layer_digest} — refused"
        ));
    }
    Ok(Pulled {
        bytes: blob,
        manifest_digest,
        layer_digest,
        media_type,
    })
}

/// The single instruction layer: prefer a `text/markdown` / `instruction`
/// media type; accept a lone layer of another UNCOMPRESSED type; refuse
/// ambiguity and compression (there is deliberately no decompressor in the
/// tree — push the document uncompressed, which `oras push` does by default).
fn select_layer(layers: &[Value]) -> Result<Value, String> {
    let compressed =
        |mt: &str| mt.ends_with("+gzip") || mt.ends_with("+zstd") || mt.contains(".tar");
    let marked: Vec<&Value> = layers
        .iter()
        .filter(|l| {
            let mt = l["mediaType"].as_str().unwrap_or("");
            mt.starts_with("text/markdown") || mt.contains("instruction")
        })
        .collect();
    let chosen = match (marked.len(), layers.len()) {
        (1, _) => marked[0],
        (0, 1) => &layers[0],
        (0, 0) => return Err("oci: the manifest has no layers".into()),
        (0, n) => {
            return Err(format!(
                "oci: {n} layers and none is text/markdown — mark the instruction layer \
                 (media type `text/markdown; variant=instruction`)"
            ));
        }
        (n, _) => {
            return Err(format!(
                "oci: {n} layers claim to be the instruction — ambiguous"
            ));
        }
    };
    let mt = chosen["mediaType"].as_str().unwrap_or("");
    if compressed(mt) {
        return Err(format!(
            "oci: the instruction layer is compressed ({mt}); push it uncompressed \
             (`oras push … file.md:'text/markdown; variant=instruction'`)"
        ));
    }
    Ok(chosen.clone())
}

// ── transport ────────────────────────────────────────────────────────────────

fn connect(host: &str, port: u16, tls: bool) -> Result<Box<dyn http::Stream>, String> {
    let tcp = http::connect_tcp(host, port, TIMEOUT)
        .map_err(|e| format!("oci: connect {host}:{port}: {e}"))?;
    if tls {
        let s = crate::net::tls::connect(tcp, host, None)
            .map_err(|e| format!("oci: tls {host}: {e}"))?;
        Ok(Box::new(s))
    } else {
        Ok(Box::new(tcp))
    }
}

fn use_tls(host: &str) -> bool {
    // Loopback may speak plain http (dev/test, the repo-wide convention);
    // everything else is https.
    !http::is_loopback_host(host)
}

fn get(
    host: &str,
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<http::Response, String> {
    let mut stream = connect(host, port, use_tls(host))?;
    let host_header = if (use_tls(host) && port == 443) || (!use_tls(host) && port == 80) {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    let resp = http::send(stream.as_mut(), &host_header, "GET", path, headers, &[])
        .map_err(|e| format!("oci: GET {host}{path}: {e}"))?;
    if resp.body.len() > MAX_BYTES {
        return Err(format!("oci: response exceeds {MAX_BYTES} bytes"));
    }
    Ok(resp)
}

/// GET with bearer auth, running the Distribution token dance once on a 401:
/// parse `WWW-Authenticate: Bearer realm=…,service=…,scope=…`, fetch a token
/// (with docker-config Basic credentials when present), retry.
fn get_with_auth(
    r: &OciRef,
    path: &str,
    accept: &str,
    token: &mut Option<String>,
    basic: Option<&str>,
) -> Result<http::Response, String> {
    let auth_header = |t: &Option<String>| t.as_ref().map(|t| format!("Bearer {t}"));
    let mut hdrs: Vec<(String, String)> = vec![("Accept".into(), accept.into())];
    if let Some(a) = auth_header(token) {
        hdrs.push(("Authorization".into(), a));
    }
    let hdr_refs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let resp = get(&r.host, r.port, path, &hdr_refs)?;
    if resp.status != 401 {
        return Ok(resp);
    }
    // Token dance.
    let challenge = resp
        .header("www-authenticate")
        .ok_or("oci: 401 with no WWW-Authenticate challenge")?;
    let ch = parse_bearer_challenge(challenge)
        .ok_or_else(|| format!("oci: unsupported auth challenge {challenge:?}"))?;
    *token = Some(fetch_token(
        &ch,
        &format!("repository:{}:pull", r.repo),
        basic,
    )?);
    let mut hdrs: Vec<(String, String)> = vec![("Accept".into(), accept.into())];
    if let Some(a) = auth_header(token) {
        hdrs.push(("Authorization".into(), a));
    }
    let hdr_refs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    get(&r.host, r.port, path, &hdr_refs)
}

/// Fetch a blob, following up to [`MAX_REDIRECTS`] redirects. The registry
/// token is presented to the REGISTRY only — a redirect to another host (a
/// presigned CDN URL) is followed with no Authorization header, so the token
/// never leaks to a third party.
fn get_blob(
    r: &OciRef,
    path: &str,
    token: &mut Option<String>,
    basic: Option<&str>,
) -> Result<Vec<u8>, String> {
    let resp = get_with_auth(r, path, "application/octet-stream", token, basic)?;
    let mut resp = resp;
    let mut host = r.host.clone();
    let mut redirects = 0;
    loop {
        if resp.is_success() {
            return Ok(resp.body);
        }
        if !(301..=308).contains(&resp.status) {
            return Err(format!("oci: {host} blob: HTTP {}", resp.status));
        }
        redirects += 1;
        if redirects > MAX_REDIRECTS {
            return Err("oci: too many blob redirects".into());
        }
        let loc = resp
            .header("location")
            .ok_or("oci: redirect with no Location")?
            .to_string();
        let url = Url::parse(&loc).map_err(|e| format!("oci: redirect url {loc}: {e}"))?;
        let same_host = url.host == r.host;
        let mut hdrs: Vec<(String, String)> = Vec::new();
        if same_host && let Some(t) = token.as_ref() {
            hdrs.push(("Authorization".into(), format!("Bearer {t}")));
        }
        let hdr_refs: Vec<(&str, &str)> =
            hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut stream = connect(&url.host, url.port, url.is_tls())?;
        host = url.host.clone();
        resp = http::send(
            stream.as_mut(),
            &url.host_header(),
            "GET",
            &url.path,
            &hdr_refs,
            &[],
        )
        .map_err(|e| format!("oci: GET {loc}: {e}"))?;
        if resp.body.len() > MAX_BYTES {
            return Err(format!("oci: blob exceeds {MAX_BYTES} bytes"));
        }
    }
}

#[derive(Debug, PartialEq)]
struct Challenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

/// Parse `Bearer realm="…",service="…",scope="…"` (parameter order free,
/// values quoted or bare).
fn parse_bearer_challenge(h: &str) -> Option<Challenge> {
    let rest = h.trim().strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for part in split_challenge_params(rest) {
        let (k, v) = part.split_once('=')?;
        let v = v.trim().trim_matches('"').to_string();
        match k.trim().to_ascii_lowercase().as_str() {
            "realm" => realm = Some(v),
            "service" => service = Some(v),
            "scope" => scope = Some(v),
            _ => {}
        }
    }
    Some(Challenge {
        realm: realm?,
        service,
        scope,
    })
}

/// Split challenge parameters on commas OUTSIDE quotes.
fn split_challenge_params(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_q = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_q = !in_q,
            ',' if !in_q => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

/// GET the token endpoint the challenge names; return the bearer token.
fn fetch_token(ch: &Challenge, scope: &str, basic: Option<&str>) -> Result<String, String> {
    let mut url = format!(
        "{}?scope={}",
        ch.realm,
        url_encode(ch.scope.as_deref().unwrap_or(scope))
    );
    if let Some(svc) = &ch.service {
        url.push_str(&format!("&service={}", url_encode(svc)));
    }
    let u = Url::parse(&url).map_err(|e| format!("oci: token realm {url}: {e}"))?;
    let mut hdrs: Vec<(String, String)> = vec![("Accept".into(), "application/json".into())];
    if let Some(b) = basic {
        hdrs.push(("Authorization".into(), format!("Basic {b}")));
    }
    let hdr_refs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut stream = connect(&u.host, u.port, u.is_tls())?;
    let resp = http::send(
        stream.as_mut(),
        &u.host_header(),
        "GET",
        &u.path,
        &hdr_refs,
        &[],
    )
    .map_err(|e| format!("oci: token fetch: {e}"))?;
    if !resp.is_success() {
        return Err(format!(
            "oci: token endpoint {}: HTTP {}",
            ch.realm, resp.status
        ));
    }
    let v: Value =
        serde_json::from_slice(&resp.body).map_err(|e| format!("oci: token body: {e}"))?;
    v["token"]
        .as_str()
        .or_else(|| v["access_token"].as_str())
        .map(str::to_string)
        .ok_or_else(|| "oci: token response has neither `token` nor `access_token`".into())
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── docker config credentials ────────────────────────────────────────────────

/// The base64 `user:pass` for `host` from the ambient docker config, if any.
/// Static `auths` entries only — credential-helper binaries are never executed.
fn docker_basic_auth(host: &str) -> Option<String> {
    let dir = match std::env::var("DOCKER_CONFIG") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => std::path::Path::new(&std::env::var("HOME").ok()?).join(".docker"),
    };
    docker_basic_auth_from(&dir.join("config.json"), host)
}

fn docker_basic_auth_from(path: &std::path::Path, host: &str) -> Option<String> {
    let cfg: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let auths = cfg.get("auths")?.as_object()?;
    // Try the host bare, with an https:// prefix, and Docker Hub's legacy key.
    let candidates: Vec<String> = if host == "registry-1.docker.io" {
        vec![
            host.to_string(),
            format!("https://{host}"),
            "https://index.docker.io/v1/".to_string(),
            "index.docker.io".to_string(),
        ]
    } else {
        vec![host.to_string(), format!("https://{host}")]
    };
    for key in candidates {
        if let Some(entry) = auths.get(&key)
            && let Some(auth) = entry.get("auth").and_then(Value::as_str)
            && !auth.is_empty()
        {
            return Some(auth.to_string());
        }
    }
    None
}

// ── digest ───────────────────────────────────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut s = String::with_capacity(64);
    for b in d.as_ref() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

// Keep `Read`/`Write` in scope for the boxed stream trait object.
#[allow(unused)]
fn _assert_stream_traits(s: &mut dyn http::Stream) -> &mut (dyn http::Stream) {
    let _ = s as &mut dyn Read;
    let _ = s as &mut dyn Write;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse_with_tags_digests_and_ports() {
        let r = OciRef::parse("oci://ghcr.io/acme/support-agent:v3").unwrap();
        assert_eq!((r.host.as_str(), r.port), ("ghcr.io", 443));
        assert_eq!(r.repo, "acme/support-agent");
        assert_eq!(r.tag.as_deref(), Some("v3"));
        assert!(r.digest.is_none());

        // No tag → latest.
        let r = OciRef::parse("oci://ghcr.io/acme/agent").unwrap();
        assert_eq!(r.tag.as_deref(), Some("latest"));

        // A digest pin wins over a tag and drops it.
        let d = format!("sha256:{}", "ab".repeat(32));
        let r = OciRef::parse(&format!("oci://ghcr.io/acme/agent:v3@{d}")).unwrap();
        assert_eq!(r.digest.as_deref(), Some(d.as_str()));
        assert!(r.tag.is_none());
        assert_eq!(r.reference(), d);

        // Port + loopback (plain http for dev).
        let r = OciRef::parse("oci://127.0.0.1:5000/local/agent:dev").unwrap();
        assert_eq!((r.host.as_str(), r.port), ("127.0.0.1", 5000));
        assert!(!use_tls(&r.host));
        assert!(use_tls("ghcr.io"));

        // Docker Hub rewrites to the API host.
        let r = OciRef::parse("oci://docker.io/acme/agent:v1").unwrap();
        assert_eq!(r.host, "registry-1.docker.io");

        // Malformed digests are refused.
        assert!(OciRef::parse("oci://ghcr.io/a/b@sha256:short").is_err());
        assert!(OciRef::parse("oci://ghcr.io").is_err());
        assert!(OciRef::parse("https://ghcr.io/a/b").is_err());
    }

    #[test]
    fn bearer_challenges_parse_in_any_order() {
        let ch = parse_bearer_challenge(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:a/b:pull""#,
        )
        .unwrap();
        assert_eq!(ch.realm, "https://ghcr.io/token");
        assert_eq!(ch.service.as_deref(), Some("ghcr.io"));
        assert_eq!(ch.scope.as_deref(), Some("repository:a/b:pull"));
        // A comma INSIDE a quoted value does not split.
        let ch = parse_bearer_challenge(r#"Bearer service="s",realm="https://t/x,y""#).unwrap();
        assert_eq!(ch.realm, "https://t/x,y");
        // Basic-only challenges are unsupported (None), not a panic.
        assert!(parse_bearer_challenge(r#"Basic realm="reg""#).is_none());
    }

    #[test]
    fn layer_selection_is_strict_but_practical() {
        let md = |mt: &str| serde_json::json!({"mediaType": mt, "digest": "sha256:aa", "size": 10});
        // The marked layer wins even beside others.
        let l = select_layer(&[
            md("application/foo"),
            md("text/markdown; variant=instruction"),
        ])
        .unwrap();
        assert!(
            l["mediaType"]
                .as_str()
                .unwrap()
                .starts_with("text/markdown")
        );
        // A lone unmarked layer is accepted.
        assert!(select_layer(&[md("application/foo")]).is_ok());
        // Ambiguity and absence refuse.
        assert!(select_layer(&[]).is_err());
        assert!(select_layer(&[md("application/a"), md("application/b")]).is_err());
        assert!(
            select_layer(&[
                md("text/markdown"),
                md("text/markdown; variant=instruction")
            ])
            .is_err()
        );
        // Compression refuses with the fix named.
        let e = select_layer(&[md("text/markdown+gzip")]).unwrap_err();
        assert!(e.contains("uncompressed"), "{e}");
    }

    #[test]
    fn docker_config_lookup_covers_the_key_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            serde_json::json!({"auths": {
                "ghcr.io": {"auth": "Z2g6dG9r"},
                "https://index.docker.io/v1/": {"auth": "aHViOnRvaw=="},
            }})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            docker_basic_auth_from(&p, "ghcr.io").as_deref(),
            Some("Z2g6dG9r")
        );
        assert_eq!(
            docker_basic_auth_from(&p, "registry-1.docker.io").as_deref(),
            Some("aHViOnRvaw==")
        );
        assert!(docker_basic_auth_from(&p, "quay.io").is_none());
        assert!(docker_basic_auth_from(&dir.path().join("absent.json"), "ghcr.io").is_none());
    }

    /// A minimal in-process registry: token dance + manifest + blob, speaking
    /// enough HTTP/1.1 for the hand-rolled client. Returns the bound port.
    fn mock_registry(doc: &'static str, tamper_blob: bool) -> u16 {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = req.lines().next().unwrap_or("").to_string();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let authed = req
                    .to_ascii_lowercase()
                    .contains("authorization: bearer tok123");

                let blob = if tamper_blob {
                    format!("{doc}<tampered>")
                } else {
                    doc.to_string()
                };
                let layer_digest = format!("sha256:{}", sha256_hex(doc.as_bytes()));
                let manifest = serde_json::json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "artifactType": "application/vnd.instruction.document.v1",
                    "layers": [{
                        "mediaType": "text/markdown; variant=instruction",
                        "digest": layer_digest,
                        "size": doc.len(),
                    }],
                })
                .to_string();

                let (status, headers, body): (&str, String, Vec<u8>) = if path.starts_with("/token")
                {
                    (
                        "200 OK",
                        "Content-Type: application/json".into(),
                        br#"{"token":"tok123"}"#.to_vec(),
                    )
                } else if !authed {
                    (
                        "401 Unauthorized",
                        format!(
                            "WWW-Authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"mock\""
                        ),
                        b"{}".to_vec(),
                    )
                } else if path.contains("/manifests/") {
                    (
                        "200 OK",
                        "Content-Type: application/vnd.oci.image.manifest.v1+json".into(),
                        manifest.into_bytes(),
                    )
                } else if path.contains("/blobs/") {
                    (
                        "200 OK",
                        "Content-Type: application/octet-stream".into(),
                        blob.into_bytes(),
                    )
                } else {
                    ("404 Not Found", String::new(), Vec::new())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\n{headers}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.write_all(&body);
            }
        });
        port
    }

    const DOC: &str = "---\nspec: \"1\"\n---\n# Pulled agent\n\nBe useful.\n";

    #[test]
    fn a_pull_runs_the_token_dance_and_verifies_digests() {
        let port = mock_registry(DOC, false);
        let p = pull(&format!("oci://127.0.0.1:{port}/acme/agent:v1")).unwrap();
        assert_eq!(p.bytes, DOC.as_bytes());
        assert!(p.manifest_digest.starts_with("sha256:"));
        assert_eq!(
            p.layer_digest,
            format!("sha256:{}", sha256_hex(DOC.as_bytes()))
        );
        assert!(p.media_type.starts_with("text/markdown"));

        // A digest-pinned reference to the same content verifies…
        let pinned = pull(&format!(
            "oci://127.0.0.1:{port}/acme/agent@{}",
            p.manifest_digest
        ))
        .unwrap();
        assert_eq!(pinned.bytes, DOC.as_bytes());
        // …and a WRONG pin is refused naming both digests.
        let bad = format!("sha256:{}", "0".repeat(64));
        let e = pull(&format!("oci://127.0.0.1:{port}/acme/agent@{bad}")).unwrap_err();
        assert!(e.contains("pins") && e.contains(&bad), "{e}");
    }

    #[test]
    fn a_tampered_blob_is_refused_by_its_digest() {
        let port = mock_registry(DOC, true);
        let e = pull(&format!("oci://127.0.0.1:{port}/acme/agent:v1")).unwrap_err();
        assert!(e.contains("blob hashes to"), "{e}");
    }
}
