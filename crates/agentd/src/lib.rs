//! `agentd`: a bounded, workflow-driven runtime.
//!
//! The runtime executes predeclared DAG workflows triggered by events,
//! HTTP requests, or explicit start-node invocation. Capabilities are
//! compile-time selected; intelligence is a bounded reasoning step,
//! not the owner of control flow.
//!
//! Full design: [`rfcs/0001-bounded-workflow-runtime.md`] at the
//! workspace root.

pub mod error;

pub use error::{Error, Result};
