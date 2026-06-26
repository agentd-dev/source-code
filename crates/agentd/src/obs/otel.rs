//! OTLP span export with the GenAI semantic conventions. RFC 0010. [feature: otel]
//!
//! Hand-rolled OTLP-over-HTTP/**JSON** — no `opentelemetry` crate, no protobuf.
//! It reuses what agentd already has: the W3C trace/span ids on every run
//! ([`crate::obs::trace`]), `serde_json`, and the hand-rolled HTTP client
//! ([`crate::net::http`]). So `--features otel` stays dependency-free.
//!
//! Off the default path: [`export_run_span`] is a **no-op unless built
//! `--features otel`** (so loop call sites stay clean and the default build pays
//! nothing). With the feature, a run that finishes exports an `invoke_agent`
//! span (GenAI semconv) to `OTEL_EXPORTER_OTLP_ENDPOINT`, best-effort — an export
//! failure is logged-and-dropped; telemetry never fails a run. (`chat` /
//! `execute_tool` child spans build on this.)

use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as unix nanoseconds — a span start/end stamp.
pub fn now_unix_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// Export the run as an `invoke_agent` span (GenAI semconv). No-op without the
/// `otel` feature or without `OTEL_EXPORTER_OTLP_ENDPOINT` set.
pub fn export_run_span(
    trace_id: Option<&str>,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    ok: bool,
    start_unix_nanos: u128,
) {
    #[cfg(feature = "otel")]
    imp::export_run_span(trace_id, model, input_tokens, output_tokens, ok, start_unix_nanos);
    #[cfg(not(feature = "otel"))]
    let _ = (trace_id, model, input_tokens, output_tokens, ok, start_unix_nanos);
}

#[cfg(feature = "otel")]
mod imp {
    use crate::net::http::{self, Url};
    use serde_json::{json, Value};
    use std::time::Duration;

    /// A finished span, ready to encode as OTLP. Times are unix nanoseconds.
    pub(super) struct Span {
        pub trace_id: String,
        pub span_id: String,
        pub parent_span_id: Option<String>,
        pub name: String,
        pub start_unix_nanos: u128,
        pub end_unix_nanos: u128,
        pub ok: bool,
        pub attrs: Vec<(&'static str, Value)>,
    }

    /// OTLP `AnyValue` for a string.
    pub(super) fn str_val(s: impl Into<String>) -> Value {
        json!({ "stringValue": s.into() })
    }

    /// OTLP `AnyValue` for an integer (OTLP ints are stringified on the wire).
    pub(super) fn int_val(n: u64) -> Value {
        json!({ "intValue": n.to_string() })
    }

    /// Build + export the run's `invoke_agent` span, if an endpoint is set.
    pub(super) fn export_run_span(
        trace_id: Option<&str>,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        ok: bool,
        start_unix_nanos: u128,
    ) {
        let Some(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok().filter(|s| !s.is_empty()) else {
            return;
        };
        let Some(trace_id) = trace_id else { return };
        let span = Span {
            trace_id: trace_id.to_string(),
            span_id: crate::obs::trace::new_span_id(),
            parent_span_id: None,
            name: "invoke_agent".into(),
            start_unix_nanos,
            end_unix_nanos: super::now_unix_nanos(),
            ok,
            attrs: vec![
                ("gen_ai.operation.name", str_val("invoke_agent")),
                ("gen_ai.request.model", str_val(model)),
                ("gen_ai.usage.input_tokens", int_val(input_tokens)),
                ("gen_ai.usage.output_tokens", int_val(output_tokens)),
            ],
        };
        // Best-effort: telemetry export must never fail the run.
        let _ = export(&endpoint, &to_otlp_json(&[span], "agentd", crate::VERSION));
    }

    /// Encode spans as an OTLP `ExportTraceServiceRequest` body (`resourceSpans`).
    pub(super) fn to_otlp_json(spans: &[Span], service: &str, version: &str) -> Value {
        let encoded: Vec<Value> = spans.iter().map(encode_span).collect();
        json!({
            "resourceSpans": [{
                "resource": { "attributes": [
                    { "key": "service.name", "value": str_val(service) },
                    { "key": "service.version", "value": str_val(version) },
                ]},
                "scopeSpans": [{ "scope": { "name": "agentd" }, "spans": encoded }]
            }]
        })
    }

    fn encode_span(s: &Span) -> Value {
        let attrs: Vec<Value> = s.attrs.iter().map(|(k, v)| json!({ "key": k, "value": v })).collect();
        let mut span = json!({
            "traceId": s.trace_id,
            "spanId": s.span_id,
            "name": s.name,
            "kind": 1, // SPAN_KIND_INTERNAL
            "startTimeUnixNano": s.start_unix_nanos.to_string(),
            "endTimeUnixNano": s.end_unix_nanos.to_string(),
            "status": { "code": if s.ok { 1 } else { 2 } }, // OK / ERROR
            "attributes": attrs,
        });
        if let Some(p) = &s.parent_span_id {
            span["parentSpanId"] = json!(p);
        }
        span
    }

    /// POST the OTLP body to `<endpoint>/v1/traces` (OTLP/HTTP, JSON). `http://`
    /// only in the default build; an `https://` collector needs `--features tls`.
    fn export(endpoint: &str, body: &Value) -> Result<(), String> {
        let base = endpoint.trim_end_matches('/');
        let target = if base.ends_with("/v1/traces") { base.to_string() } else { format!("{base}/v1/traces") };
        let url = Url::parse(&target).map_err(|e| format!("otel: bad endpoint '{target}': {e}"))?;
        if url.is_tls() {
            return Err("otel: https OTLP endpoints need --features tls".into());
        }
        let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let mut stream =
            http::connect_tcp(&url.host, url.port, Duration::from_secs(5)).map_err(|e| e.to_string())?;
        let headers = [("content-type", "application/json")];
        let resp = http::send(&mut stream, &url.host_header(), "POST", &url.path, &headers, &bytes)
            .map_err(|e| e.to_string())?;
        if resp.is_success() {
            Ok(())
        } else {
            Err(format!("otel: collector returned HTTP {}", resp.status))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn span() -> Span {
            Span {
                trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
                span_id: "00f067aa0ba902b7".into(),
                parent_span_id: None,
                name: "invoke_agent".into(),
                start_unix_nanos: 1_700_000_000_000_000_000,
                end_unix_nanos: 1_700_000_001_000_000_000,
                ok: true,
                attrs: vec![
                    ("gen_ai.operation.name", str_val("invoke_agent")),
                    ("gen_ai.usage.input_tokens", int_val(1234)),
                ],
            }
        }

        #[test]
        fn otlp_json_has_the_expected_shape() {
            let v = to_otlp_json(&[span()], "agentd", "0.1.0");
            let rs = &v["resourceSpans"][0];
            assert_eq!(rs["resource"]["attributes"][0]["value"]["stringValue"], "agentd");
            let sp = &rs["scopeSpans"][0]["spans"][0];
            assert_eq!(sp["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
            assert_eq!(sp["name"], "invoke_agent");
            assert_eq!(sp["status"]["code"], 1); // OK
            assert_eq!(sp["startTimeUnixNano"], "1700000000000000000"); // stringified
            assert_eq!(sp["attributes"][1]["value"]["intValue"], "1234"); // stringified int
            assert!(sp.get("parentSpanId").is_none());
        }

        #[test]
        fn error_span_sets_status_error_and_parent() {
            let mut s = span();
            s.ok = false;
            s.parent_span_id = Some("aaaaaaaaaaaaaaaa".into());
            let sp = to_otlp_json(&[s], "agentd", "0.1.0");
            let sp = &sp["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
            assert_eq!(sp["status"]["code"], 2); // ERROR
            assert_eq!(sp["parentSpanId"], "aaaaaaaaaaaaaaaa");
        }
    }
}
