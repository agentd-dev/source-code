//! Intelligence adapter (RFC §15).
//!
//! Splits reasoning out of the runtime: the engine asks an
//! [`IntelligenceClient`] for a response and never opens a model
//! socket itself. Workflows declare `llm_infer` nodes that the
//! [`handler::LlmInferHandler`] dispatches.
//!
//! Transports:
//!
//! - **Unix domain socket** (always on) — length-framed JSON-RPC 2.0;
//!   any host-side server speaking the shape plugs in.
//! - **HTTP** (feature `intel-http`) — same JSON-RPC shape over HTTP.
//! - **Remote providers** (feature `intel-remote`) — Anthropic,
//!   OpenAI, Gemini, and any openai-compatible endpoint, addressed
//!   as named backends (RFC 0006 §3).
//! - **Mock** (test-only) — canned responses keyed by a selector.
//!
//! Schema validation of the model's structured output lands in
//! Phase 7 alongside the policy pass; for Phase 4 the handler
//! only enforces "must be JSON" when `output_schema` is declared.

pub mod backends;
pub mod client;
pub mod handler;
pub mod protocol;
#[cfg(feature = "intel-remote")]
pub mod providers;

pub use backends::{BackendDef, BackendMap, IntelligenceConfig, ProviderKind};
#[cfg(feature = "intel-http")]
pub use client::HttpClient;
pub use client::{IntelligenceClient, MockClient};
pub use handler::LlmInferHandler;
pub use protocol::{Message, Request, Response, Usage};
