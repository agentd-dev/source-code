// SPDX-License-Identifier: AGPL-3.0-only
//! The **`http` workflow node** (RFC 0027): make an outbound REST call from a
//! workflow — `GET`/`POST`/`PUT`/`PATCH`/`DELETE` with `headers`, `query`, and a
//! `json`/`body` payload — and observe `{status, ok, headers, body, json}`. This
//! is also how a workflow **emits a webhook** (a `POST` to a URL). It runs on an
//! executor thread over the one SSRF-guarded HTTP client (RFC 0012); the URL and
//! body are already template-rendered (`render_spec`) against the run's data.
//!
//! Security: the SSRF classifier guards the resolved host — private/loopback/
//! link-local targets are refused unless the node sets `allow_private: true`
//! (for a declared internal API). The guard resolves once and the dial takes the
//! addresses it vetted, so a name that answers the check public and the connect
//! private cannot rebind its way in. `https://` verifies the server certificate.

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::engine::run::StepStatus;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

impl crate::runtime::reactor::Runtime {
    /// Execute an `http` step (fields already rendered by `render_spec`).
    pub(crate) fn step_http(&mut self, run_id: &str, step_id: &str, spec: &Map<String, Value>) {
        let url = spec
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if url.is_empty() {
            self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some("http: url is required".into()),
                0,
            );
            return;
        }
        let method = spec
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        // RFC 0037 Phase B: the `http` step is a covered egress surface —
        // `closed` mode requires a `kind: http` catalog entry (templated URLs
        // are judged HERE, literals already at load), and a matching entry's
        // `methods:` is a ceiling either mode.
        {
            use crate::config::v2 as cfgv2;
            if let Err(e) = cfgv2::egress_allows(
                &self.settings.services,
                self.settings.security.egress,
                cfgv2::ServiceKind::Http,
                &url,
            ) {
                self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                return;
            }
            if let Some((name, entry)) =
                cfgv2::service_match(&self.settings.services, cfgv2::ServiceKind::Http, &url)
                && let Some(methods) = &entry.methods
                && !methods.iter().any(|m| m == &method)
            {
                self.finish_step_pub(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    None,
                    Some(format!(
                        "http: {method} is outside services.{name}.methods ({methods:?}) — the catalog's method ceiling"
                    )),
                    0,
                );
                return;
            }
        }
        let mut headers: Vec<(String, String)> = spec
            .get("headers")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), header_value(v)))
                    .collect()
            })
            .unwrap_or_default();
        // Resolve `{{secret:NAME}}` / `{{secret-file:PATH}}` references through the
        // redacting resolver — so an `Authorization: Bearer {{secret:API_TOKEN}}`
        // header (or the `sign` secret below) carries a real credential without it
        // ever passing through the workflow templater, a log line, or step output.
        let envs = self.env.clone();
        let resolve_secret = move |s: &str| -> Result<String, String> {
            if s.contains("{{secret") {
                crate::sec::secret::resolve(s, &|k| {
                    envs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
                })
            } else {
                Ok(s.to_string())
            }
        };
        for (_, v) in headers.iter_mut() {
            match resolve_secret(v) {
                Ok(r) => *v = r,
                Err(e) => {
                    self.finish_step_pub(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("http: header secret: {e}")),
                        0,
                    );
                    return;
                }
            }
        }
        let query = spec
            .get("query")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| format!("{}={}", pct(k), pct(&header_value(v))))
                    .collect::<Vec<_>>()
                    .join("&")
            })
            .unwrap_or_default();
        // `idempotency: {header|query, value?}` — the retry-safety declaration.
        // The default value is derived from run+step identity, so every attempt
        // of THIS step presents the same key and a deduping API treats a retry
        // as the retry it is; `value:` substitutes an application key (already
        // rendered, like every field) for APIs where the operation's real
        // identity is a business fact such as an order id.
        let mut query = query;
        if let Some(idem) = spec.get("idempotency") {
            let value = idem
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| crate::engine::run::idempotency_key(run_id, step_id));
            if let Some(h) = idem.get("header").and_then(Value::as_str) {
                headers.push((h.to_string(), value));
            } else if let Some(q) = idem.get("query").and_then(Value::as_str) {
                let pair = format!("{}={}", pct(q), pct(&value));
                if query.is_empty() {
                    query = pair;
                } else {
                    query.push('&');
                    query.push_str(&pair);
                }
            }
        }
        // Body: `json` (serialized + Content-Type) takes precedence over `body`.
        let body: Vec<u8> = if let Some(j) = spec.get("json").filter(|v| !v.is_null()) {
            if !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
            serde_json::to_vec(j).unwrap_or_default()
        } else {
            spec.get("body")
                .and_then(Value::as_str)
                .map(|s| s.as_bytes().to_vec())
                .unwrap_or_default()
        };
        // `sign`: HMAC-sign the body so a receiver can verify the payload — this
        // makes the node a best-practice **webhook emitter**, symmetric with the
        // inbound `webhook` node's `hmac` verify. `{secret, header?, prefix?}`.
        if let Some(sig) = spec.get("sign").and_then(Value::as_object)
            && let Some(secret_ref) = sig.get("secret").and_then(Value::as_str)
        {
            let secret = match resolve_secret(secret_ref) {
                Ok(s) => s,
                Err(e) => {
                    self.finish_step_pub(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("http: sign secret: {e}")),
                        0,
                    );
                    return;
                }
            };
            let header = sig
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or("X-Signature")
                .to_string();
            let prefix = sig
                .get("prefix")
                .and_then(Value::as_str)
                .unwrap_or("sha256=");
            let mac = crate::sha::hmac_sha256(secret.as_bytes(), &body);
            let value = format!("{prefix}{}", crate::sha::to_hex(&mac));
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&header));
            headers.push((header, value));
        }
        let timeout = spec
            .get("timeout")
            .and_then(crate::engine::model::duration_ms_opt)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_TIMEOUT);
        let allow_private = spec
            .get("allow_private")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // `expect`: acceptable status codes; default = 2xx is `ok`, else error.
        let expect: Vec<u64> = spec
            .get("expect")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();

        self.log.info(
            "http.request",
            json!({"run": run_id, "step": step_id, "method": method, "url": url}),
        );
        let tx = self.events_tx.clone();
        let (r, s) = (run_id.to_string(), step_id.to_string());
        self.executing
            .insert(format!("{run_id}/{step_id}"), Instant::now());
        std::thread::Builder::new()
            .name("step:http".into())
            .spawn(move || {
                let (output, is_error, error) = match do_http(
                    &url,
                    &method,
                    &query,
                    &headers,
                    &body,
                    timeout,
                    allow_private,
                ) {
                    Ok(v) => {
                        let status = v["status"].as_u64().unwrap_or(0);
                        let ok = if expect.is_empty() {
                            (200..400).contains(&status)
                        } else {
                            expect.contains(&status)
                        };
                        if ok {
                            (v, false, None)
                        } else {
                            (v.clone(), true, Some(format!("http status {status}")))
                        }
                    }
                    Err(e) => (Value::Null, true, Some(format!("http: {e}"))),
                };
                let _ = tx.send(super::events::Event::StepDone {
                    run: r,
                    step: s,
                    output,
                    is_error,
                    error,
                    tokens: 0,
                });
            })
            .ok();
    }
}

/// A header value as a string (JSON scalars stringify; non-scalars serialize).
fn header_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The blocking request (executor thread): parse + SSRF-guard + connect (+TLS) +
/// round-trip, returning `{status, ok, headers, body, json}`.
/// GET a URL and return its body as text.
///
/// A thin wrapper over the same guarded path the `http` node uses, for callers
/// that want a document rather than a step result — loading a workflow
/// definition from a definitions service, say. Sharing the path matters: the
/// SSRF guard, the single resolve and the vetted dial are the parts that must
/// not be reimplemented slightly differently somewhere else.
pub(crate) fn fetch_text(
    url: &str,
    headers: &[(String, String)],
    timeout: Duration,
    allow_private: bool,
) -> Result<String, String> {
    let v = do_http(url, "GET", "", headers, &[], timeout, allow_private)?;
    let status = v.get("status").and_then(Value::as_u64).unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    match v.get("body") {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Ok(other.to_string()),
        None => Err("empty body".into()),
    }
}

fn do_http(
    url: &str,
    method: &str,
    query: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    allow_private: bool,
) -> Result<Value, String> {
    let u = crate::net::http::Url::parse(url)?;
    let path = if query.is_empty() {
        u.path.clone()
    } else if u.path.contains('?') {
        format!("{}&{}", u.path, query)
    } else {
        format!("{}?{}", u.path, query)
    };
    let hdr_refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    // The guard and the dial are one step: a standalone `guard_host` followed by
    // `connect_tcp` resolves the name twice, and a URL a model or a workflow
    // author supplied is exactly where a hostile nameserver would answer the
    // check public and the connect `169.254.169.254`. `connect_vetted` resolves
    // once, classifies, and dials an address it vetted; TLS/SNI and the `Host`
    // header stay on the hostname below. The refusal is a PermissionDenied
    // carrying the same `SsrfError` text, so the `http: {e}` step error a
    // workflow surfaces reads as it always did.
    let tcp = crate::net::ssrf::connect_vetted(&u.host, u.port, timeout, allow_private)
        .map_err(|e| e.to_string())?;
    let resp = if u.is_tls() {
        #[cfg(feature = "tls")]
        {
            let mut s = crate::net::tls::connect(tcp, &u.host, None).map_err(|e| e.to_string())?;
            crate::net::http::send(&mut s, &u.host_header(), method, &path, &hdr_refs, body)
                .map_err(|e| e.to_string())?
        }
        #[cfg(not(feature = "tls"))]
        {
            return Err("https requires the 'tls' build feature".into());
        }
    } else {
        let mut s = tcp;
        crate::net::http::send(&mut s, &u.host_header(), method, &path, &hdr_refs, body)
            .map_err(|e| e.to_string())?
    };
    let body_str = resp.body_str().to_string();
    let headers_obj: Map<String, Value> = resp
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    Ok(json!({
        "status": resp.status,
        "ok": resp.is_success(),
        "headers": headers_obj,
        "body": body_str,
        "json": serde_json::from_str::<Value>(&body_str).ok(),
    }))
}

/// Minimal percent-encoding for query components (RFC 3986 unreserved kept).
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
