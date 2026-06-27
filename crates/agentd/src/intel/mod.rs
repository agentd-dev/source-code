pub mod anthropic;
pub mod client;
// RFC 0018 intelligence transport resilience — the endpoint list, per-endpoint
// health + circuit breaker, and the sticky-primary failover policy. Core
// (always compiled, dependency-free): a single-endpoint list is byte-for-byte
// RFC 0006, with the resilience machinery inert.
pub mod endpoints;
pub mod failover;
pub mod health;
pub mod openai;
// Built-in mock LLM (the hidden `--internal-mock-llm` mode) for the M7
// observe-to-validate + conformance suites. Debug builds always carry it (so
// `cargo test` works with no flag); release ships it only under `internal-mocks`,
// keeping the production binary free of test scaffolding.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock;
