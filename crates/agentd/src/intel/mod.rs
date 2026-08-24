// SPDX-License-Identifier: AGPL-3.0-only
pub mod anthropic;
// Amazon Bedrock Converse dialect: pure translation, with no signing of its
// own. Authenticating the dial is a separate axis (`crate::auth::aws` supplies
// SigV4), so the two can be configured independently. Always compiled because
// it pulls in no dependencies; reached only when the `intelligence.dialect:
// bedrock` selector is set.
pub mod bedrock;
pub mod client;
// Intelligence transport resilience: the endpoint list, per-endpoint health +
// circuit breaker, and the sticky-primary failover policy. Always compiled and
// dependency-free. A single-endpoint list drives exactly the same code path as
// a plain one-shot dial, with the resilience machinery inert — so configuring
// one endpoint costs nothing at runtime.
pub mod endpoints;
pub mod failover;
pub mod health;
pub mod openai;
// Optional, capability-negotiated model discovery: a best-effort
// `GET /v1/models` over the existing intel transport, consumed by the
// supervisor-side `agentd://intelligence` and capabilities `intelligence.models`
// surfaces. Off the hot path, silent on failure, never fatal — a provider that
// does not expose the endpoint simply reports no models.
pub mod discovery;
// Built-in mock LLM (the hidden `--internal-mock-llm` mode) that backs the
// observe-to-validate and conformance suites. Debug builds always carry it so
// `cargo test` works with no flag; release ships it only under `internal-mocks`,
// keeping the production binary free of test scaffolding.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock;
