//! Observability. The default build ships three dependency-free things: a
//! hand-rolled JSON-lines logger to stderr, a tiny health surface, and W3C
//! trace-context propagation. Only the heavier surfaces (metrics, OTLP export)
//! are feature-gated. RFC 0010.
//!
//! Invariant: **stdout is the agent's result; stderr is all telemetry.**

pub mod health;
pub mod log;
// W3C trace-context *propagation* is default-on and dependency-free (a few
// formatted fields). Only span *export* (OTLP) is gated behind `otel` (added
// inside `trace.rs`). RFC 0010 §context-propagation.
pub mod trace;

// The metrics *module* is always compiled, but its `record_*` fns are no-ops
// unless built `--features metrics` (the atomic registry + Prometheus render +
// `/metrics` surface are gated). This keeps call sites clean and the default
// build cost-free. RFC 0010 §metrics.
pub mod metrics;

// The opt-in HTTP probe/scrape surface (/metrics + /healthz + /readyz) is the
// one piece that needs a listener thread, so it is gated with the registry.
#[cfg(feature = "metrics")]
pub mod serve;

// OTLP span export (GenAI semconv). Always compiled, but the `RunSpan` recorder
// (run span + `chat`/`execute_tool` children) is a no-op unless built
// `--features otel` (the OTLP encoder + HTTP export are gated) — clean loop call
// sites, zero default cost. Hand-rolled, dep-free.
pub mod otel;
