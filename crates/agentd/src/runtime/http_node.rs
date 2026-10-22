// SPDX-License-Identifier: AGPL-3.0-only
//! The **`http` workflow node**: make an outbound REST call from a workflow —
//! `GET`/`POST`/`PUT`/`PATCH`/`DELETE` with `headers`, `query`, and a
//! `json`/`body` payload — and observe `{status, ok, headers, body, json}`. This
//! is also how a workflow **emits a webhook** (a `POST` to a URL). It runs on an
//! executor thread over the one SSRF-guarded HTTP client, so every outbound dial
//! in the daemon passes the same guard; the URL and body are already
//! template-rendered (`render_spec`) against the run's data.
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
        // The `http` step is a covered egress surface. In `closed` mode the URL
        // must match a `kind: http` service-catalog entry; a literal URL is
        // judged at config load, but a templated one only resolves here, so the
        // check has to run again at dial time. A matching entry's `methods:`
        // list is a ceiling in either mode — declaring an endpoint for reads
        // must not silently authorize writes to it.
        {
            use crate::config::settings;
            if let Err(e) = settings::egress_allows(
                &self.settings.services,
                self.settings.security.egress,
                settings::ServiceKind::Http,
                &url,
            ) {
                self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                return;
            }
            if let Some((name, entry)) =
                settings::service_match(&self.settings.services, settings::ServiceKind::Http, &url)
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
        // The call is a span of the step's, named to the API it reaches in
        // `traceparent` and `tracestate` — ahead of the signature — on the
        // step's `http.request` line, and as the CLIENT span it exports. A
        // step whose `headers` set either owns the trace context: agentd
        // adds neither, and the line is the step's, since no span of
        // agentd's went out.
        let authored = headers.iter().any(|(k, _)| {
            k.eq_ignore_ascii_case("traceparent") || k.eq_ignore_ascii_case("tracestate")
        });
        let hop = if authored {
            None
        } else {
            self.step_trace(run_id, step_id).map(|t| t.child())
        };
        if let Some(t) = &hop {
            headers.extend(t.headers());
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

        let line = match &hop {
            Some(h) => self.log.in_trace(Some(h)),
            None => self.step_log(run_id, step_id),
        };
        line.info(
            "http.request",
            json!({"run": run_id, "step": step_id, "method": method, "url": url}),
        );
        // A step that may pay (RFC 0049): what it already paid, or may
        // have, is read before anything is sent.
        let paying = spec.get("payment").is_some();
        if paying {
            #[cfg(feature = "x402")]
            if let Err(e) = self.payment_precheck(run_id, step_id) {
                self.payment_refused_before(run_id, step_id, e);
                return;
            }
            #[cfg(not(feature = "x402"))]
            {
                self.finish_step_pub(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    None,
                    Some("http: payment needs the `x402` feature".into()),
                    0,
                );
                return;
            }
        }
        let request = Request {
            url,
            method,
            query,
            headers,
            body,
            timeout,
            allow_private,
            expect,
            hop,
        };
        let tx = self.events_tx.clone();
        let (r, s) = (run_id.to_string(), step_id.to_string());
        self.executing
            .insert(format!("{run_id}/{step_id}"), Instant::now());
        std::thread::Builder::new()
            .name("step:http".into())
            .spawn(move || {
                let start = crate::obs::otel::now_unix_nanos();
                let answered = request.send(&[]);
                // A 402 that asks for a payment: the step's to make when it
                // may pay, else its error (RFC 0049 D2).
                let asked = answered
                    .as_ref()
                    .ok()
                    .filter(|v| v["status"] == 402)
                    .and_then(|v| v["headers"]["payment-required"].as_str())
                    .map(str::to_string);
                // Only a build that reads x402 adds the payment request it
                // was answered with to the output.
                #[cfg_attr(not(feature = "x402"), allow(unused_mut))]
                let (mut output, is_error, mut error) = match answered {
                    Ok(v) => request.judge(v),
                    Err(e) => (Value::Null, true, Some(format!("http: {}", e.why))),
                };
                request.span(start, &r, &s, &output, is_error);
                if let Some(header) = asked {
                    #[cfg(feature = "x402")]
                    if paying {
                        let _ = tx.send(super::events::Event::PaymentRequired(Box::new(
                            super::payments::Asked {
                                run: r,
                                step: s,
                                header,
                                first: output,
                                request,
                            },
                        )));
                        return;
                    }
                    // A step whose `expect` takes a 402 has it as its
                    // answer; any other fails, saying why.
                    if is_error {
                        #[cfg(feature = "x402")]
                        if let Ok(v) = crate::payments::x402::decode_header(&header) {
                            output["payment_required"] = v;
                        }
                        error = Some(
                            "http status 402: the server asks to be paid (x402), and this step has no payment field — it pays only with one, within security.payments (RFC 0049)".into(),
                        );
                    }
                    #[cfg(not(feature = "x402"))]
                    let _ = header;
                }
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

/// An `http` step's request, as rendered: what it sends, and what a paid
/// retry (RFC 0049 D3) sends again — the same method, URL, headers and
/// body.
pub(crate) struct Request {
    pub url: String,
    pub method: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
    pub allow_private: bool,
    /// The statuses that are `ok`; empty, 2xx and 3xx.
    pub expect: Vec<u64>,
    /// The step's call span, when it carries one.
    pub hop: Option<crate::obs::trace::TraceContext>,
}

impl Request {
    /// The URL as sent, its `query` appended: what a server's
    /// `resource.url` is held to.
    #[cfg(feature = "x402")]
    pub fn full_url(&self) -> String {
        if self.query.is_empty() {
            self.url.clone()
        } else if self.url.contains('?') {
            format!("{}&{}", self.url, self.query)
        } else {
            format!("{}?{}", self.url, self.query)
        }
    }

    /// Send it, with `extra` headers after its own.
    pub fn send(&self, extra: &[(String, String)]) -> Result<Value, Unanswered> {
        if extra.is_empty() {
            return exchange(
                &self.url,
                &self.method,
                &self.query,
                &self.headers,
                &self.body,
                self.timeout,
                self.allow_private,
            );
        }
        let mut headers = self.headers.clone();
        headers.extend(extra.iter().cloned());
        exchange(
            &self.url,
            &self.method,
            &self.query,
            &headers,
            &self.body,
            self.timeout,
            self.allow_private,
        )
    }

    /// An answer as the step's outcome: `(output, is_error, error)`.
    pub fn judge(&self, v: Value) -> (Value, bool, Option<String>) {
        let status = v["status"].as_u64().unwrap_or(0);
        let ok = if self.expect.is_empty() {
            (200..400).contains(&status)
        } else {
            self.expect.contains(&status)
        };
        if ok {
            (v, false, None)
        } else {
            (v, true, Some(format!("http status {status}")))
        }
    }

    /// The CLIENT span of one exchange.
    pub fn span(&self, start: u128, run: &str, step: &str, output: &Value, is_error: bool) {
        crate::obs::otel::span(
            self.hop.as_ref(),
            &self.method,
            crate::obs::otel::Kind::Client,
            start,
        )
        .attr("http.request.method", self.method.as_str())
        .attr("url.full", self.url.as_str())
        .attr_opt("http.response.status_code", output["status"].as_u64())
        .attr("agentd.run", run)
        .attr("agentd.step", step)
        .end(!is_error);
    }
}

/// The `http` step's request as a paid call (RFC 0049 §2, [X-HTTP]): its
/// resource is its URL, query included; the payment goes once more to the
/// same URL, with the same method, headers and body, in a
/// `PAYMENT-SIGNATURE` header; the settlement comes back in
/// `PAYMENT-RESPONSE`. Like every request of the step, it follows no
/// redirect.
#[cfg(feature = "x402")]
impl super::payments::PaidCall for Request {
    fn is_resource(&self, url: &str) -> bool {
        crate::payments::x402::same_resource(&self.full_url(), url)
    }

    fn shown(&self) -> String {
        crate::config::settings::url_locator(&self.full_url())
    }

    /// The method and the URL, quoted: a run may template either.
    fn describe(&self) -> String {
        crate::sec::render::quoted(
            &format!("{} {}", self.method, self.shown()),
            super::payments::SAID_SHOWN,
        )
    }

    fn send_paid(&self, payload: &Value, run: &str, step: &str) -> super::payments::Answer {
        use crate::payments::x402;
        let start = crate::obs::otel::now_unix_nanos();
        let header = (
            x402::PAYMENT_SIGNATURE.to_string(),
            x402::encode_header(payload),
        );
        match self.send(std::slice::from_ref(&header)) {
            Err(e) => super::payments::Answer::Unanswered {
                why: e.why,
                may_have_sent: e.may_have_sent,
            },
            Ok(v) => {
                let status = v["status"].as_u64().unwrap_or(0);
                let settlement = v["headers"]["payment-response"]
                    .as_str()
                    .map(x402::Settlement::from_header);
                let declined = settlement.is_none() && matches!(status, 400 | 402);
                let (output, is_error, error) = self.judge(v);
                self.span(start, run, step, &output, is_error);
                super::payments::Answer::Answered {
                    output,
                    settlement,
                    declined,
                    error,
                }
            }
        }
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

/// One outbound request, SSRF-guarded. `pub(crate)` because the `http` node is
/// not the only outbound dial: a stream `forward:` pushes through the same
/// guard rather than opening a second, differently-checked path out.
pub(crate) fn do_http(
    url: &str,
    method: &str,
    query: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    allow_private: bool,
) -> Result<Value, String> {
    exchange(url, method, query, headers, body, timeout, allow_private).map_err(|e| e.why)
}

/// Why a request got no answer, and whether any of it may have reached the
/// server: a URL that does not parse, a dial the SSRF guard refused, a
/// connection or a TLS handshake that failed sent nothing; a failure once
/// the request was being written may have. A paid request needs to know
/// which (RFC 0049 D4).
pub(crate) struct Unanswered {
    pub why: String,
    #[cfg_attr(not(feature = "x402"), allow(dead_code))]
    pub may_have_sent: bool,
}

/// [`do_http`], saying of a failure whether the request may have left.
pub(crate) fn exchange(
    url: &str,
    method: &str,
    query: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    allow_private: bool,
) -> Result<Value, Unanswered> {
    let unsent = |why: String| Unanswered {
        why,
        may_have_sent: false,
    };
    let sent = |why: String| Unanswered {
        why,
        may_have_sent: true,
    };
    let u = crate::net::http::Url::parse(url).map_err(unsent)?;
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
        .map_err(|e| unsent(e.to_string()))?;
    let resp = if u.is_tls() {
        #[cfg(feature = "tls")]
        {
            let mut s =
                crate::net::tls::connect(tcp, &u.host, None).map_err(|e| unsent(e.to_string()))?;
            crate::net::http::send(&mut s, &u.host_header(), method, &path, &hdr_refs, body)
                .map_err(|e| sent(e.to_string()))?
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = (tcp, sent);
            return Err(unsent("https requires the 'tls' build feature".into()));
        }
    } else {
        let mut s = tcp;
        crate::net::http::send(&mut s, &u.host_header(), method, &path, &hdr_refs, body)
            .map_err(|e| sent(e.to_string()))?
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

/// Percent-encode one query component. Only the RFC 3986 unreserved characters —
/// ASCII letters, digits and `-` `_` `.` `~` — pass through untouched;
/// every other byte becomes `%XX`, so `&`, `=` and `?` in a value cannot
/// break out and forge extra query parameters.
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
