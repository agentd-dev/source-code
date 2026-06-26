//! Observability. The default build ships exactly two things: a hand-rolled
//! JSON-lines logger to stderr + a tiny health surface. Everything heavier
//! (metrics, OTLP) is feature-gated. RFC 0010.
//!
//! Invariant: **stdout is the agent's result; stderr is all telemetry.**

pub mod log;
pub mod health;
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
