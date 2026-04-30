//! Intelligence adapter (RFC §15).
//!
//! Splits reasoning out of the runtime: the engine asks an
//! [`IntelligenceClient`] for a response and never opens a model
//! socket itself. Workflows declare `llm_infer` nodes that the
//! [`handler::LlmInferHandler`] dispatches.
//!
//! Phase 4 transports:
//!
//! - **Unix domain socket** (always on) — length-framed JSON-RPC 2.0
//!   wire-compatible with `sandbox::intelligence_server` so operators
//!   can point the harness at the existing host-side server.
//! - **HTTP** (feature `intel-http`) — OpenAI-shaped Messages API;
//!   same request / response shape.
//! - **Mock** (test-only) — canned responses keyed by a selector.
//!
//! Schema validation of the model's structured output lands in
//! Phase 7 alongside the policy pass; for Phase 4 the handler
//! only enforces "must be JSON" when `output_schema` is declared.

pub mod client;
pub mod handler;
pub mod protocol;

#[cfg(feature = "intel-http")]
pub use client::HttpClient;
pub use client::{IntelligenceClient, MockClient};
pub use handler::LlmInferHandler;
pub use protocol::{Message, Request, Response, Usage};
