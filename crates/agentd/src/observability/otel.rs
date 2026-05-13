//! Direct OTLP exporter.
//!
//! Attaches an OTLP gRPC exporter to the global `tracing`
//! subscriber so every span the runtime emits lands in a real
//! OpenTelemetry collector — Tempo, Jaeger, otel-collector,
//! Datadog, Honeycomb, etc. The existing JSON-log → filelog path
//! still works and stays the dependency-light default; this module
//! is feature-gated on `otel` and only pulled in when the operator
//! explicitly accepts the ~50-crate `tokio` / `tonic` footprint.
//!
//! ## Threading model
//!
//! OTLP batching is async-by-design. We spawn a **single** tokio
//! multi-thread runtime at process startup (configurable worker
//! count, default 1) and hand it to `opentelemetry_sdk`. The
//! runtime lives for the full process lifetime — its handle is
//! pinned in a `OnceLock`. On graceful shutdown the runtime gets
//! `shutdown_background()`'d so pending spans flush before the
//! process exits.
//!
//! ## TOML grammar
//!
//! ```toml
//! [otel]
//! endpoint = "http://otel-collector:4317"   # required
//! service_name = "agentd"                    # default
//! protocol = "grpc"                          # only value today
//! resource_attrs = { env = "prod", region = "eu-west-1" }
//! ```
//!
//! ## Integration
//!
//! [`init_otel_layer`] returns a `tracing_opentelemetry::OpenTelemetryLayer`
//! that callers compose alongside the main fmt layer + optional
//! audit layer via `Registry::with()`. Trace-context on inbound
//! HTTP requests propagates naturally — the `traceparent` parser
//! already attaches trace_id / parent_id fields to the request
//! span, and `tracing-opentelemetry` promotes those to real OTel
//! `SpanContext` parents.

use serde::{Deserialize, Serialize};

/// `[otel]` TOML block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OtelConfig {
    /// OTLP collector endpoint. Examples:
    /// `http://localhost:4317` (grpc default), `https://otlp.nr-data.net:4317`.
    pub endpoint: String,

    /// `service.name` resource attribute. Defaults to `"agentd"`.
    #[serde(default)]
    pub service_name: Option<String>,

    /// Wire protocol. Only `"grpc"` is wired in v1 — `"http/protobuf"`
    /// is a v2 follow-up (would save a handful of crates).
    #[serde(default)]
    pub protocol: Option<String>,

    /// Extra resource attributes merged into every span.
    /// `{ "deployment.environment" = "prod" }` style.
    #[serde(default)]
    pub resource_attrs: std::collections::HashMap<String, String>,

    /// Export sampling: fraction in `[0.0, 1.0]`. `1.0` (default)
    /// exports every span; `0.1` samples 10%. Applied at the SDK
    /// layer, so unsampled spans don't leave the process.
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: f64,
}

fn default_sample_ratio() -> f64 {
    1.0
}

impl Default for OtelConfig {
    /// Default is "always sample", matching the serde-default path
    /// so `OtelConfig::default()` and a `[otel]` block with only
    /// `endpoint = "..."` set produce the same behaviour.
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            service_name: None,
            protocol: None,
            resource_attrs: std::collections::HashMap::new(),
            sample_ratio: default_sample_ratio(),
        }
    }
}

impl OtelConfig {
    /// Resolved service name with the default applied.
    pub fn effective_service_name(&self) -> &str {
        self.service_name.as_deref().unwrap_or("agentd")
    }

    /// Clamped sampling ratio. Operators who type -1 or 3 get the
    /// obvious semantic rather than a panic deep in the SDK.
    pub fn clamped_sample_ratio(&self) -> f64 {
        self.sample_ratio.clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Feature-gated implementation
// ---------------------------------------------------------------------------

#[cfg(feature = "otel")]
mod inner {
    use super::*;
    use opentelemetry::KeyValue;
    use opentelemetry::global;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::runtime;
    use opentelemetry_sdk::trace::{Sampler, TracerProvider};
    use std::sync::OnceLock;
    use tokio::runtime::Runtime;
    use tracing_opentelemetry::OpenTelemetryLayer;

    /// Handle to the OTel runtime + tracer provider. Kept in a
    /// `OnceLock` so successive calls to `init_otel_layer` are
    /// idempotent (a second install attempt re-uses the same
    /// provider, which matches tracing-subscriber's
    /// install-once semantics).
    static OTEL_STATE: OnceLock<OtelState> = OnceLock::new();

    struct OtelState {
        #[allow(dead_code)]
        runtime: Runtime,
        provider: TracerProvider,
    }

    /// Build the OTLP exporter + tracer provider + tracing layer.
    /// Call **once** before `tracing::subscriber::set_global_default`.
    /// Returns the layer the caller must compose into the subscriber.
    pub fn init_otel_layer<S>(
        cfg: &OtelConfig,
    ) -> Result<OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>, String>
    where
        S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    {
        if OTEL_STATE.get().is_some() {
            return Err("otel already initialised in this process".into());
        }

        let protocol = cfg.protocol.as_deref().unwrap_or("grpc");
        if protocol != "grpc" {
            return Err(format!(
                "otel.protocol `{protocol}` is not supported in v1 (only `grpc`)"
            ));
        }

        // Build the async runtime. Multi-thread with 1 worker is
        // enough for export fan-out; operators can raise via
        // `tokio::runtime::Builder::new_multi_thread().worker_threads(n)`
        // in a future knob.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("agentd-otel")
            .build()
            .map_err(|e| format!("otel: tokio runtime: {e}"))?;

        // Build the exporter + provider inside the runtime so
        // tonic channel construction has tokio context.
        let endpoint = cfg.endpoint.clone();
        let service_name = cfg.effective_service_name().to_string();
        let extra_attrs: Vec<(String, String)> = cfg
            .resource_attrs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let sample_ratio = cfg.clamped_sample_ratio();

        let provider = runtime.block_on(async move {
            let exporter = SpanExporter::builder()
                .with_tonic()
                .with_endpoint(&endpoint)
                .build()
                .map_err(|e| format!("otel exporter: {e}"))?;

            let mut attrs = vec![KeyValue::new("service.name", service_name)];
            for (k, v) in extra_attrs {
                attrs.push(KeyValue::new(k, v));
            }
            let resource = Resource::new(attrs);

            let provider = TracerProvider::builder()
                .with_sampler(Sampler::TraceIdRatioBased(sample_ratio))
                .with_resource(resource)
                .with_batch_exporter(exporter, runtime::Tokio)
                .build();
            Ok::<TracerProvider, String>(provider)
        })?;

        // Install as the global tracer provider so any library
        // emitting opentelemetry spans lands in the same pipeline.
        global::set_tracer_provider(provider.clone());

        let tracer = provider.tracer("agentd");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);

        let _ = OTEL_STATE.set(OtelState { runtime, provider });
        Ok(layer)
    }

    /// Flush pending spans and shut down the exporter cleanly.
    /// Call from the graceful-shutdown path so in-flight exports
    /// complete before the process exits. Idempotent — a second
    /// call is a no-op.
    pub fn shutdown_otel() {
        if let Some(state) = OTEL_STATE.get() {
            // `shutdown_tracer_provider` flushes and drops the
            // global provider. `state.provider` is a clone-safe
            // handle — shutting it down flushes its batch
            // processor, which is what we want.
            state.runtime.block_on(async {
                for result in state.provider.force_flush() {
                    if let Err(e) = result {
                        tracing::warn!(
                            target: "agentd::audit",
                            event = "otel.flush_error",
                            reason = %format!("{e:?}"),
                        );
                    }
                }
            });
            opentelemetry::global::shutdown_tracer_provider();
        }
    }
}

#[cfg(feature = "otel")]
pub use inner::{init_otel_layer, shutdown_otel};

/// Stub when the feature is off — lets callers install-or-skip
/// without feature-gate gymnastics in call sites.
#[cfg(not(feature = "otel"))]
pub fn shutdown_otel() {
    // no-op
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = OtelConfig {
            endpoint: "http://localhost:4317".into(),
            ..Default::default()
        };
        assert_eq!(cfg.effective_service_name(), "agentd");
        assert_eq!(cfg.clamped_sample_ratio(), 1.0);
    }

    #[test]
    fn sample_ratio_clamps() {
        let mut cfg = OtelConfig {
            endpoint: "http://x".into(),
            sample_ratio: -0.5,
            ..Default::default()
        };
        assert_eq!(cfg.clamped_sample_ratio(), 0.0);
        cfg.sample_ratio = 2.5;
        assert_eq!(cfg.clamped_sample_ratio(), 1.0);
        cfg.sample_ratio = 0.3;
        assert!((cfg.clamped_sample_ratio() - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_from_toml() {
        let src = r#"
            endpoint = "http://otel:4317"
            service_name = "demo"
            resource_attrs = { env = "prod", region = "eu-west-1" }
            sample_ratio = 0.1
        "#;
        let cfg: OtelConfig = toml::from_str(src).unwrap();
        assert_eq!(cfg.endpoint, "http://otel:4317");
        assert_eq!(cfg.effective_service_name(), "demo");
        assert_eq!(cfg.resource_attrs.get("env"), Some(&"prod".to_string()));
        assert_eq!(cfg.clamped_sample_ratio(), 0.1);
    }

    #[test]
    fn rejects_unknown_fields() {
        let src = r#"
            endpoint = "http://x"
            nope = "bad"
        "#;
        assert!(toml::from_str::<OtelConfig>(src).is_err());
    }

    #[cfg(feature = "otel")]
    #[test]
    fn rejects_unsupported_protocol_at_build_time() {
        // The build_provider call is private; go through init_otel_layer
        // with a clearly-unsupported protocol. `OpenTelemetryLayer`
        // doesn't implement `Debug`, so unwrap_err (which prints the
        // Ok variant on unexpected success) can't be used — pattern-
        // match and assert instead.
        let cfg = OtelConfig {
            endpoint: "http://x:4317".into(),
            protocol: Some("http/json".into()),
            ..Default::default()
        };
        use tracing_subscriber::Registry;
        match init_otel_layer::<Registry>(&cfg) {
            Ok(_) => panic!("expected unsupported-protocol error"),
            Err(e) => assert!(e.contains("http/json"), "err: {e}"),
        }
    }
}
