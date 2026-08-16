// SPDX-License-Identifier: AGPL-3.0-only
pub mod anthropic;
// Amazon Bedrock Converse dialect (RFC 0031 §8 — native Bedrock). Pure
// translation; the SigV4 signing that authenticates the dial is a separate axis
// (`crate::auth::aws`). Compiled always (dependency-free); reached when the
// `intelligence.dialect: bedrock` selector is set.
pub mod bedrock;
pub mod client;
// RFC 0018 intelligence transport resilience — the endpoint list, per-endpoint
// health + circuit breaker, and the sticky-primary failover policy. Core
// (always compiled, dependency-free): a single-endpoint list is byte-for-byte
// RFC 0006, with the resilience machinery inert.
pub mod endpoints;
pub mod failover;
pub mod health;
pub mod openai;
// RFC 0018 §5.4 — optional, capability-negotiated model discovery. A best-effort
// `GET /v1/models` over the existing intel transport, consumed by the
// supervisor-side `agentd://intelligence` + capabilities `intelligence.models`
// surfaces. Off the hot path, silent on failure, never fatal.
pub mod discovery;
// Built-in mock LLM (the hidden `--internal-mock-llm` mode) for the M7
// observe-to-validate + conformance suites. Debug builds always carry it (so
// `cargo test` works with no flag); release ships it only under `internal-mocks`,
// keeping the production binary free of test scaffolding.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock;
