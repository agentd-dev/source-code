// SPDX-License-Identifier: AGPL-3.0-only
//! AWS **Signature Version 4** request signing (RFC 0031 §8) — the `kind: aws`
//! credential provider. Signs each outbound request with AWS credentials so an
//! endpoint behind AWS IAM (an API-Gateway MCP server, Bedrock behind a gateway)
//! authenticates the agent by SigV4 rather than a bearer.
//!
//! Dependency-free (the minimalism moat): the whole algorithm is HMAC-SHA256 +
//! SHA-256, both already in `crate::sha`; UTC date decomposition reuses the
//! logger's `civil_from_days`. Credential **sources**: `env`/`static` (the
//! standard `AWS_*` variables), `sso` (IAM Identity Center login → temporary
//! creds, see [`super::aws_sso`]), `imds` (EC2 instance role, IMDSv2), and `irsa`
//! (EKS web identity via STS). Temporary-credential sources are refetched as they
//! near expiry; a session token is signed via `x-amz-security-token`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::net::http::{self, Url};
use crate::sha::{hmac_sha256, sha256_hex, to_hex};

/// Resolved AWS credentials (long-term or temporary).
#[derive(Debug, Clone)]
pub struct AwsCreds {
    pub access_key: String,
    pub secret_key: String,
    /// Present for temporary credentials (STS/SSO/IMDS) — signed via
    /// `x-amz-security-token`.
    pub session_token: Option<String>,
}

/// Read AWS credentials from the standard environment (the `env`/`static`
/// source): `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`.
pub fn creds_from_env() -> Result<AwsCreds, String> {
    let access_key = std::env::var("AWS_ACCESS_KEY_ID")
        .map_err(|_| "aws: AWS_ACCESS_KEY_ID is not set".to_string())?;
    let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
        .map_err(|_| "aws: AWS_SECRET_ACCESS_KEY is not set".to_string())?;
    Ok(AwsCreds {
        access_key,
        secret_key,
        session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
    })
}

/// The two SigV4 timestamps from a `SystemTime`: the amz-date
/// (`YYYYMMDDTHHMMSSZ`) and the date-stamp (`YYYYMMDD`).
fn timestamps(now: SystemTime) -> (String, String) {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = crate::obs::log::civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    (
        format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z"),
        format!("{y:04}{m:02}{d:02}"),
    )
}

/// Compute the SigV4 headers to add to a request: `X-Amz-Date`, `Authorization`
/// (and `X-Amz-Security-Token` for temporary credentials). `host` is the `Host`
/// value; `target` is the request-target (`path[?query]`); `body` is the exact
/// payload bytes. `amz_date`/`date_stamp` are passed in (the signer derives them
/// from the clock) so the algorithm stays deterministic + testable.
#[allow(clippy::too_many_arguments)]
pub fn sigv4_headers(
    creds: &AwsCreds,
    region: &str,
    service: &str,
    method: &str,
    host: &str,
    target: &str,
    body: &[u8],
    amz_date: &str,
    date_stamp: &str,
) -> Vec<(String, String)> {
    // Split the request-target into canonical URI + canonical query.
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let canonical_uri = if path.is_empty() { "/" } else { path };
    let canonical_query = canonical_query(query);
    let payload_hash = sha256_hex(body);

    // Canonical + signed headers. host;x-amz-date always; x-amz-security-token
    // is signed when a session token is present (a MUST for temporary creds).
    let mut canonical_headers = format!("host:{host}\nx-amz-date:{amz_date}\n");
    let mut signed_headers = String::from("host;x-amz-date");
    if creds.session_token.is_some() {
        // Header names sort lexically: security-token < x-amz-date? No — all are
        // `x-amz-*`; the canonical order is host, x-amz-date, x-amz-security-token
        // (d < s). Append after x-amz-date.
        let tok = creds.session_token.as_deref().unwrap_or("");
        canonical_headers.push_str(&format!("x-amz-security-token:{tok}\n"));
        signed_headers.push_str(";x-amz-security-token");
    }

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    // Signing key: HMAC chain seeded with "AWS4"+secret.
    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_key).as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = to_hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    );
    let mut out = vec![
        ("X-Amz-Date".to_string(), amz_date.to_string()),
        ("Authorization".to_string(), authorization),
    ];
    if let Some(tok) = &creds.session_token {
        out.push(("X-Amz-Security-Token".to_string(), tok.clone()));
    }
    out
}

/// Canonicalize a query string (sort by key, URI-encode components). Empty → "".
fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (uri_encode(k, false), uri_encode(v, false)),
            None => (uri_encode(p, false), String::new()),
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// SigV4 URI-encoding: unreserved `A-Za-z0-9-._~` pass through; `/` is kept only
/// for a path (`keep_slash`). Everything else → `%XX` (uppercase hex).
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Where a signer's credentials come from (RFC 0031 §8).
enum CredProvider {
    /// Long-term keys from the environment (`env`/`static`).
    Fixed(AwsCreds),
    /// SSO temporary creds, reloaded from the login cache under this target.
    Sso(String),
    /// EC2 instance metadata (IMDSv2) — fetched + cached until near expiry.
    Imds(Mutex<Option<(AwsCreds, u64)>>),
    /// EKS web-identity (IRSA) via STS AssumeRoleWithWebIdentity — fetched + cached.
    Irsa(Mutex<Option<(AwsCreds, u64)>>),
}

/// A transport signer (RFC 0031) that SigV4-signs each request. Temporary
/// credentials (SSO / IMDS / IRSA) are reloaded/refetched as they near expiry, so
/// a long-lived daemon keeps signing with live keys.
pub struct SigV4Signer {
    provider: CredProvider,
    region: String,
    service: String,
    timeout: Duration,
}

impl SigV4Signer {
    pub fn new(creds: AwsCreds, region: String, service: String) -> SigV4Signer {
        SigV4Signer {
            provider: CredProvider::Fixed(creds),
            region,
            service,
            timeout: Duration::from_secs(10),
        }
    }

    /// Build from an `AuthSpec` (`kind: aws`). `target` is the credential-cache
    /// key for `source: sso`. Sources: `env`/`static` (the `AWS_*` env), `sso`
    /// (temporary creds from `agentd login`), `imds` (EC2 instance role), `irsa`
    /// (EKS web identity).
    pub fn from_spec(
        auth: &crate::config::AuthSpec,
        target: &str,
    ) -> Result<Arc<SigV4Signer>, String> {
        let region = auth.region.clone().ok_or("aws: `region` is required")?;
        let service = auth
            .service
            .clone()
            .ok_or("aws: `service` is required (e.g. bedrock, execute-api)")?;
        let provider = match auth.source.as_deref().unwrap_or("env") {
            "env" | "static" => CredProvider::Fixed(creds_from_env()?),
            "sso" => CredProvider::Sso(target.to_string()),
            "imds" => CredProvider::Imds(Mutex::new(None)),
            "irsa" => CredProvider::Irsa(Mutex::new(None)),
            other => {
                return Err(format!(
                    "aws: unknown source '{other}' (env|static|sso|imds|irsa)"
                ));
            }
        };
        Ok(Arc::new(SigV4Signer {
            provider,
            region,
            service,
            timeout: Duration::from_secs(10),
        }))
    }

    /// The credentials to sign with now — fixed, or reloaded/refetched for the
    /// temporary-credential sources (cached until ~1 min before expiry).
    fn current_creds(&self) -> Option<AwsCreds> {
        match &self.provider {
            CredProvider::Fixed(c) => Some(c.clone()),
            CredProvider::Sso(t) => crate::auth::aws_sso::cached_creds(t),
            CredProvider::Imds(cache) => self.cached_or_fetch(cache, || fetch_imds(self.timeout)),
            CredProvider::Irsa(cache) => {
                self.cached_or_fetch(cache, || fetch_irsa(&self.region, self.timeout))
            }
        }
    }

    /// Serve a cached temporary credential, refetching when it's within a minute
    /// of expiry. Fail-closed: a fetch error yields `None` (no signature).
    fn cached_or_fetch(
        &self,
        cache: &Mutex<Option<(AwsCreds, u64)>>,
        fetch: impl FnOnce() -> Result<(AwsCreds, u64), String>,
    ) -> Option<AwsCreds> {
        let now = crate::auth::cache::now_ms();
        {
            let g = cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((c, exp)) = g.as_ref()
                && (*exp == 0 || now + 60_000 < *exp)
            {
                return Some(c.clone());
            }
        }
        match fetch() {
            Ok((c, exp)) => {
                *cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((c.clone(), exp));
                Some(c)
            }
            Err(_) => None,
        }
    }
}

/// EC2 instance metadata credentials (IMDSv2). `AGENTD_IMDS_ENDPOINT` overrides
/// the link-local base for testing. Connects directly (a fixed metadata address,
/// intentionally outside the SSRF guard).
fn fetch_imds(timeout: Duration) -> Result<(AwsCreds, u64), String> {
    let base = std::env::var("AGENTD_IMDS_ENDPOINT")
        .unwrap_or_else(|_| "http://169.254.169.254".to_string());
    // 1. A session token (IMDSv2).
    let token = imds_req(
        &format!("{base}/latest/api/token"),
        "PUT",
        &[("X-aws-ec2-metadata-token-ttl-seconds", "21600")],
        timeout,
    )?;
    let hdr = [("X-aws-ec2-metadata-token", token.trim())];
    // 2. The instance role name, then its credentials.
    let role = imds_req(
        &format!("{base}/latest/meta-data/iam/security-credentials/"),
        "GET",
        &hdr,
        timeout,
    )?;
    let role = role.trim();
    let body = imds_req(
        &format!("{base}/latest/meta-data/iam/security-credentials/{role}"),
        "GET",
        &hdr,
        timeout,
    )?;
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ImdsCreds {
        access_key_id: String,
        secret_access_key: String,
        token: String,
    }
    let c: ImdsCreds =
        serde_json::from_str(&body).map_err(|e| format!("imds: bad credentials json: {e}"))?;
    Ok((
        AwsCreds {
            access_key: c.access_key_id,
            secret_key: c.secret_access_key,
            session_token: Some(c.token),
        },
        // Refetch well before the ~6h IMDS lifetime; a fixed 50-min horizon.
        crate::auth::cache::now_ms() + 50 * 60_000,
    ))
}

/// EKS IRSA credentials: exchange the projected web-identity token at STS
/// (`AssumeRoleWithWebIdentity`). `AGENTD_STS_ENDPOINT` overrides STS for testing.
fn fetch_irsa(region: &str, timeout: Duration) -> Result<(AwsCreds, u64), String> {
    let token_file = std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE")
        .map_err(|_| "irsa: AWS_WEB_IDENTITY_TOKEN_FILE is not set".to_string())?;
    let role_arn =
        std::env::var("AWS_ROLE_ARN").map_err(|_| "irsa: AWS_ROLE_ARN is not set".to_string())?;
    let token = std::fs::read_to_string(&token_file)
        .map_err(|e| format!("irsa: token file: {e}"))?
        .trim()
        .to_string();
    let sts = std::env::var("AGENTD_STS_ENDPOINT")
        .unwrap_or_else(|_| format!("https://sts.{region}.amazonaws.com"));
    let form = format!(
        "Action=AssumeRoleWithWebIdentity&RoleArn={}&RoleSessionName=agentd&WebIdentityToken={}&Version=2011-06-15",
        uri_encode(&role_arn, false),
        uri_encode(&token, false)
    );
    let body = sts_post(&format!("{sts}/"), form.as_bytes(), timeout)?;
    let ak = xml_field(&body, "AccessKeyId").ok_or("irsa: STS returned no AccessKeyId")?;
    let sk = xml_field(&body, "SecretAccessKey").ok_or("irsa: STS returned no SecretAccessKey")?;
    let st = xml_field(&body, "SessionToken");
    Ok((
        AwsCreds {
            access_key: ak,
            secret_key: sk,
            session_token: st,
        },
        crate::auth::cache::now_ms() + 50 * 60_000,
    ))
}

/// One IMDS request; returns the body as a trimmed string.
fn imds_req(
    url: &str,
    method: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<String, String> {
    let u = Url::parse(url).map_err(|e| format!("imds: url {url}: {e}"))?;
    // IMDS is a fixed link-local metadata address — connect directly.
    let mut s =
        http::connect_tcp(&u.host, u.port, timeout).map_err(|e| format!("imds: connect: {e}"))?;
    let resp = http::send(&mut s, &u.host_header(), method, &u.path, headers, &[])
        .map_err(|e| format!("imds: request failed: {e}"))?;
    if !resp.is_success() {
        return Err(format!("imds: {url} → HTTP {}", resp.status));
    }
    Ok(resp.body_str().to_string())
}

/// POST an `x-www-form-urlencoded` body to STS; returns the XML response body.
fn sts_post(url: &str, form: &[u8], timeout: Duration) -> Result<String, String> {
    let u = Url::parse(url).map_err(|e| format!("sts: url {url}: {e}"))?;
    let tcp =
        http::connect_tcp(&u.host, u.port, timeout).map_err(|e| format!("sts: connect: {e}"))?;
    let mut stream: Box<dyn http::Stream> = if u.is_tls() {
        #[cfg(feature = "tls")]
        {
            Box::new(
                crate::net::tls::connect(tcp, &u.host, None)
                    .map_err(|e| format!("sts: tls: {e}"))?,
            )
        }
        #[cfg(not(feature = "tls"))]
        {
            return Err("sts: https requires --features tls".to_string());
        }
    } else {
        Box::new(tcp)
    };
    let resp = http::send(
        stream.as_mut(),
        &u.host_header(),
        "POST",
        &u.path,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        form,
    )
    .map_err(|e| format!("sts: request failed: {e}"))?;
    if !resp.is_success() {
        return Err(format!("sts: HTTP {}", resp.status));
    }
    Ok(resp.body_str().to_string())
}

/// Extract the text of the first `<tag>…</tag>` in an XML body (STS is simple,
/// well-formed XML — a hand match avoids an XML-parser dependency).
fn xml_field(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].trim().to_string())
}

impl ::mcp::http::RequestSigner for SigV4Signer {
    fn sign(
        &self,
        method: &str,
        authority: &str,
        path: &str,
        body: &[u8],
    ) -> Vec<(String, String)> {
        // Fail-closed: no creds (SSO not logged in / expired) → no header, the
        // server answers 403 surfacing "run agentd login".
        let Some(creds) = self.current_creds() else {
            return Vec::new();
        };
        let (amz_date, date_stamp) = timestamps(SystemTime::now());
        sigv4_headers(
            &creds,
            &self.region,
            &self.service,
            method,
            authority,
            path,
            body,
            &amz_date,
            &date_stamp,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigv4_matches_the_aws_get_vanilla_vector() {
        // AWS SigV4 test suite `get-vanilla`: service=service, region=us-east-1,
        // AKIDEXAMPLE / wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY at 20150830T123600Z.
        let creds = AwsCreds {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let h = sigv4_headers(
            &creds,
            "us-east-1",
            "service",
            "GET",
            "example.amazonaws.com",
            "/",
            b"",
            "20150830T123600Z",
            "20150830",
        );
        let auth = &h.iter().find(|(k, _)| k == "Authorization").unwrap().1;
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert_eq!(
            h.iter().find(|(k, _)| k == "X-Amz-Date").unwrap().1,
            "20150830T123600Z"
        );
    }

    #[test]
    fn session_token_is_signed_and_present() {
        let creds = AwsCreds {
            access_key: "AKID".into(),
            secret_key: "secret".into(),
            session_token: Some("tok-123".into()),
        };
        let h = sigv4_headers(
            &creds,
            "us-east-1",
            "bedrock",
            "POST",
            "h",
            "/x",
            b"{}",
            "20200101T000000Z",
            "20200101",
        );
        let auth = &h.iter().find(|(k, _)| k == "Authorization").unwrap().1;
        assert!(
            auth.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"),
            "the session token is a signed header: {auth}"
        );
        assert_eq!(
            h.iter()
                .find(|(k, _)| k == "X-Amz-Security-Token")
                .unwrap()
                .1,
            "tok-123"
        );
    }

    #[test]
    fn canonical_query_sorts_and_encodes() {
        assert_eq!(canonical_query(""), "");
        assert_eq!(canonical_query("b=2&a=1"), "a=1&b=2");
        assert_eq!(canonical_query("x=a b"), "x=a%20b");
    }

    #[test]
    fn xml_field_extracts_sts_credentials() {
        let body = "<AssumeRoleWithWebIdentityResult><Credentials>\
            <AccessKeyId>ASIAIRSA</AccessKeyId>\
            <SecretAccessKey>irsa-secret</SecretAccessKey>\
            <SessionToken>irsa-sess</SessionToken>\
            <Expiration>2026-01-01T00:00:00Z</Expiration>\
            </Credentials></AssumeRoleWithWebIdentityResult>";
        assert_eq!(xml_field(body, "AccessKeyId").as_deref(), Some("ASIAIRSA"));
        assert_eq!(
            xml_field(body, "SecretAccessKey").as_deref(),
            Some("irsa-secret")
        );
        assert_eq!(
            xml_field(body, "SessionToken").as_deref(),
            Some("irsa-sess")
        );
        assert_eq!(xml_field(body, "Nope"), None);
    }
}
