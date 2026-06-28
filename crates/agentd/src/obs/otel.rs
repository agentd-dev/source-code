// SPDX-License-Identifier: Apache-2.0
//! OTLP span export with the GenAI semantic conventions. RFC 0010. [feature: otel]
//!
//! Hand-rolled OTLP-over-HTTP/**JSON** — no `opentelemetry` crate, no protobuf.
//! It reuses what agentd already has: the W3C trace/span ids on every run
//! ([`crate::obs::trace`]), `serde_json`, and the hand-rolled HTTP client
//! ([`crate::net::http`]). So `--features otel` stays dependency-free.
//!
//! Off the default path: [`RunSpan`] is a **no-op handle unless built
//! `--features otel`** (so loop call sites stay clean and the default build pays
//! nothing). With the feature, a run records a `chat` span per model call and an
//! `execute_tool` span per tool call, then flushes the whole trace — the
//! `invoke_agent` run span (GenAI semconv) plus those children, one OTLP batch —
//! to `OTEL_EXPORTER_OTLP_ENDPOINT` when it finishes. Best-effort: an export
//! failure is logged-and-dropped; telemetry never fails a run.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as unix nanoseconds — a span start/end stamp.
pub fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// A run's span recorder. Begin it once (mints the `invoke_agent` span id under
/// the run trace), record a `chat`/`execute_tool` child as each completes, then
/// `finish` to flush the whole trace. Every method is a **no-op without the
/// `otel` feature** (or without `OTEL_EXPORTER_OTLP_ENDPOINT`), so the loop wires
/// it unconditionally with no `cfg` at the call sites.
pub struct RunSpan {
    #[cfg(feature = "otel")]
    inner: Option<imp::RunSpan>,
}

/// Begin the run span. `trace_id` is the run's W3C trace id; `start_unix_nanos`
/// stamps the `invoke_agent` span start.
pub fn run_begin(trace_id: Option<&str>, start_unix_nanos: u128) -> RunSpan {
    #[cfg(feature = "otel")]
    {
        RunSpan {
            inner: imp::RunSpan::begin(trace_id, start_unix_nanos),
        }
    }
    #[cfg(not(feature = "otel"))]
    {
        let _ = (trace_id, start_unix_nanos);
        RunSpan {}
    }
}

impl RunSpan {
    /// Record a `chat` child span for one model call (parent = the run span).
    pub fn record_chat(
        &mut self,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        ok: bool,
        start_unix_nanos: u128,
    ) {
        #[cfg(feature = "otel")]
        if let Some(i) = self.inner.as_mut() {
            i.record_chat(model, input_tokens, output_tokens, ok, start_unix_nanos);
        }
        #[cfg(not(feature = "otel"))]
        let _ = (model, input_tokens, output_tokens, ok, start_unix_nanos);
    }

    /// Record an `execute_tool` child span for one tool call (parent = the run span).
    pub fn record_tool(&mut self, tool_name: &str, ok: bool, start_unix_nanos: u128) {
        #[cfg(feature = "otel")]
        if let Some(i) = self.inner.as_mut() {
            i.record_tool(tool_name, ok, start_unix_nanos);
        }
        #[cfg(not(feature = "otel"))]
        let _ = (tool_name, ok, start_unix_nanos);
    }

    /// Close the `invoke_agent` span and export the run trace (run span + every
    /// recorded child) as one OTLP batch. No-op without the feature/endpoint.
    pub fn finish(self, model: &str, input_tokens: u64, output_tokens: u64, ok: bool) {
        #[cfg(feature = "otel")]
        if let Some(i) = self.inner {
            i.finish(model, input_tokens, output_tokens, ok);
        }
        #[cfg(not(feature = "otel"))]
        let _ = (model, input_tokens, output_tokens, ok);
    }
}

#[cfg(feature = "otel")]
mod imp {
    use crate::net::http::{self, Url};
    use serde_json::{Value, json};
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

    /// The live recorder for one run: the `invoke_agent` span identity + the
    /// child spans collected so far + where to ship them.
    pub(super) struct RunSpan {
        trace_id: String,
        span_id: String,
        start_unix_nanos: u128,
        endpoint: String,
        children: Vec<Span>,
    }

    /// OTLP `AnyValue` for a string.
    pub(super) fn str_val(s: impl Into<String>) -> Value {
        json!({ "stringValue": s.into() })
    }

    /// OTLP `AnyValue` for an integer (OTLP ints are stringified on the wire).
    pub(super) fn int_val(n: u64) -> Value {
        json!({ "intValue": n.to_string() })
    }

    impl RunSpan {
        /// Begin a run span, or `None` if there's no endpoint / trace to anchor it.
        pub(super) fn begin(trace_id: Option<&str>, start_unix_nanos: u128) -> Option<RunSpan> {
            let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                .ok()
                .filter(|s| !s.is_empty())?;
            let trace_id = trace_id?.to_string();
            Some(RunSpan {
                span_id: crate::obs::trace::new_span_id(),
                trace_id,
                start_unix_nanos,
                endpoint,
                children: Vec::new(),
            })
        }

        pub(super) fn record_chat(
            &mut self,
            model: &str,
            input_tokens: u64,
            output_tokens: u64,
            ok: bool,
            start_unix_nanos: u128,
        ) {
            self.children.push(Span {
                trace_id: self.trace_id.clone(),
                span_id: crate::obs::trace::new_span_id(),
                parent_span_id: Some(self.span_id.clone()),
                name: "chat".into(),
                start_unix_nanos,
                end_unix_nanos: super::now_unix_nanos(),
                ok,
                attrs: vec![
                    ("gen_ai.operation.name", str_val("chat")),
                    ("gen_ai.request.model", str_val(model)),
                    ("gen_ai.usage.input_tokens", int_val(input_tokens)),
                    ("gen_ai.usage.output_tokens", int_val(output_tokens)),
                ],
            });
        }

        pub(super) fn record_tool(&mut self, tool_name: &str, ok: bool, start_unix_nanos: u128) {
            self.children.push(Span {
                trace_id: self.trace_id.clone(),
                span_id: crate::obs::trace::new_span_id(),
                parent_span_id: Some(self.span_id.clone()),
                name: "execute_tool".into(),
                start_unix_nanos,
                end_unix_nanos: super::now_unix_nanos(),
                ok,
                attrs: vec![
                    ("gen_ai.operation.name", str_val("execute_tool")),
                    ("gen_ai.tool.name", str_val(tool_name)),
                ],
            });
        }

        /// Close the run span and export the whole trace as one OTLP batch.
        pub(super) fn finish(
            mut self,
            model: &str,
            input_tokens: u64,
            output_tokens: u64,
            ok: bool,
        ) {
            let run = Span {
                trace_id: self.trace_id.clone(),
                span_id: self.span_id.clone(),
                parent_span_id: None,
                name: "invoke_agent".into(),
                start_unix_nanos: self.start_unix_nanos,
                end_unix_nanos: super::now_unix_nanos(),
                ok,
                attrs: vec![
                    ("gen_ai.operation.name", str_val("invoke_agent")),
                    ("gen_ai.request.model", str_val(model)),
                    ("gen_ai.usage.input_tokens", int_val(input_tokens)),
                    ("gen_ai.usage.output_tokens", int_val(output_tokens)),
                ],
            };
            self.children.push(run);
            // Best-effort: telemetry export must never fail the run.
            let _ = export(
                &self.endpoint,
                &to_otlp_json(&self.children, "agentd", crate::VERSION),
            );
        }
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
        let attrs: Vec<Value> = s
            .attrs
            .iter()
            .map(|(k, v)| json!({ "key": k, "value": v }))
            .collect();
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
        let target = if base.ends_with("/v1/traces") {
            base.to_string()
        } else {
            format!("{base}/v1/traces")
        };
        let url = Url::parse(&target).map_err(|e| format!("otel: bad endpoint '{target}': {e}"))?;
        if url.is_tls() {
            return Err("otel: https OTLP endpoints need --features tls".into());
        }
        let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let mut stream = http::connect_tcp(&url.host, url.port, Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        let headers = [("content-type", "application/json")];
        let resp = http::send(
            &mut stream,
            &url.host_header(),
            "POST",
            &url.path,
            &headers,
            &bytes,
        )
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
            assert_eq!(
                rs["resource"]["attributes"][0]["value"]["stringValue"],
                "agentd"
            );
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

        #[test]
        fn a_run_records_chat_and_tool_children_under_the_run_span() {
            let mut run = RunSpan {
                trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
                span_id: "00f067aa0ba902b7".into(),
                start_unix_nanos: 1_700_000_000_000_000_000,
                endpoint: "http://127.0.0.1:4318".into(),
                children: Vec::new(),
            };
            run.record_chat("m", 10, 20, true, 1_700_000_000_000_000_000);
            run.record_tool("resource.read", true, 1_700_000_000_500_000_000);
            assert_eq!(run.children.len(), 2);

            // The batch the exporter would ship: 2 children + the run span, all
            // sharing the trace, children parented to the run span.
            let mut batch = std::mem::take(&mut run.children);
            batch.push(Span {
                trace_id: run.trace_id.clone(),
                span_id: run.span_id.clone(),
                parent_span_id: None,
                name: "invoke_agent".into(),
                start_unix_nanos: run.start_unix_nanos,
                end_unix_nanos: 1_700_000_001_000_000_000,
                ok: true,
                attrs: vec![],
            });
            let v = to_otlp_json(&batch, "agentd", "0.1.0");
            let spans = &v["resourceSpans"][0]["scopeSpans"][0]["spans"];
            assert_eq!(spans[0]["name"], "chat");
            assert_eq!(spans[0]["parentSpanId"], "00f067aa0ba902b7");
            assert_eq!(spans[0]["attributes"][1]["value"]["stringValue"], "m");
            assert_eq!(spans[1]["name"], "execute_tool");
            assert_eq!(
                spans[1]["attributes"][1]["value"]["stringValue"],
                "resource.read"
            );
            assert_eq!(spans[1]["parentSpanId"], "00f067aa0ba902b7");
            assert_eq!(spans[2]["name"], "invoke_agent");
            assert!(spans[2].get("parentSpanId").is_none()); // root
            // every child shares the run trace id
            assert_eq!(spans[0]["traceId"], spans[2]["traceId"]);
        }
    }
}
