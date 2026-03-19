//! Execution engine (RFC §8).
//!
//! Sequential DAG traversal: trigger → context → node handler →
//! outcome → next edge. The engine owns a [`HandlerRegistry`] and a
//! small amount of per-run state; everything else — tool dispatch,
//! policy, observability — is a handler concern so each tool family
//! can land independently.
//!
//! Phase 2 deliberately ships only the control-node handlers
//! (Condition / Switch / Merge / Fail / Terminate) plus a
//! `StubHandler` used in tests. Phase 3+ register real handlers for
//! fs / env / data / intelligence / MCP.

pub mod context;
pub mod outcome;

pub use context::{ExecutionContext, RunOptions, TriggerKind, TriggerMeta};
pub use outcome::{ExecutionOutcome, NodeOutcome};
