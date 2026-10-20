// SPDX-License-Identifier: AGPL-3.0-only
//! Observability. The default build ships three dependency-free things: a
//! hand-rolled JSON-lines logger to stderr, a tiny health surface, and W3C
//! trace-context propagation. Only the heavier surfaces (metrics, OTLP export)
//! are feature-gated.
//!
//! Invariant: **stdout is the agent's result; stderr is all telemetry.**

// The hash-chained audit log `observability.audit.chain` names (RFC 0044
// P2a): dependency-free, on the hand-rolled SHA-256.
pub mod chain;
pub mod health;
pub mod log;
// W3C trace-context *propagation* is default-on and dependency-free (a few
// formatted fields). Only span *export* (OTLP) is gated behind `otel` (added
// inside `trace.rs`).
pub mod trace;

// The metrics *module* is always compiled, but its `record_*` fns are no-ops
// unless built `--features metrics` (the atomic registry + Prometheus render +
// `/metrics` surface are gated). This keeps call sites clean and the default
// build cost-free.
pub mod metrics;

// The opt-in HTTP probe/scrape surface (/metrics + /healthz + /readyz) is the
// one piece that needs a listener thread, so it is gated with the registry.
#[cfg(feature = "metrics")]
pub mod serve;

// OTLP span export (GenAI semconv). Always compiled, but every `Span` and the
// turn's `RunSpan` recorder are no-ops unless built `--features otel` (the
// OTLP encoder + HTTP export are gated) — clean call sites, zero default
// cost. Hand-rolled, dep-free.
pub mod otel;

// One line a minute about something that keeps failing: the OTLP exporter's
// per-signal record, and the credentials' per-target one.
pub(crate) mod quiet;
