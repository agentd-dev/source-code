//! Runtime error type.
//!
//! One `Error` enum covers every subsystem so call sites can use the
//! `?` operator freely. Variants mirror the error categories in
//! RFC §19.1.

use std::io;

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("workflow `{workflow}`: {reason}")]
    Workflow { workflow: String, reason: String },

    #[error("policy violation: {0}")]
    Policy(String),

    #[error("capability unavailable: {0}")]
    CapabilityUnavailable(String),

    #[error("tool `{tool}` failed: {reason}")]
    Tool { tool: String, reason: String },

    #[error("schema validation failed: {0}")]
    Schema(String),

    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),

    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
