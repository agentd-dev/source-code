pub mod anthropic;
pub mod client;
pub mod openai;
// Built-in mock LLM (the hidden `--internal-mock-llm` mode) for the M7
// observe-to-validate + conformance suites. Debug builds always carry it (so
// `cargo test` works with no flag); release ships it only under `internal-mocks`,
// keeping the production binary free of test scaffolding.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock;
