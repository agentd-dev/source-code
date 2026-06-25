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

#[cfg(feature = "metrics")]
pub mod metrics;
