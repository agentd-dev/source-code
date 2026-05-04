//! `agentd`: a bounded, workflow-driven runtime.
//!
//! The runtime executes predeclared DAG workflows triggered by events,
//! HTTP requests, or explicit start-node invocation. Capabilities are
//! compile-time selected; intelligence is a bounded reasoning step,
//! not the owner of control flow.
//!
//! Full design: [`rfcs/0001-bounded-workflow-runtime.md`] at the
//! workspace root.

#[cfg(feature = "auth")]
pub mod auth;
pub mod embedded;
pub mod engine;
pub mod error;
pub mod intelligence;
pub mod mcp;
pub mod observability;
pub mod policy;
pub mod ratelimit;
pub mod runtime;
pub mod server_config;
pub mod signals;

// Re-export for integration tests + external consumers that want to
// build a client against our TLS server without pulling rustls
// directly.
#[cfg(feature = "server-tls")]
pub use rustls;
pub mod testing;
pub mod tools;
pub mod triggers;
pub mod workflow;

pub use error::{Error, Result};
