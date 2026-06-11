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

pub mod checkpoint;
pub mod context;
pub mod handler;
pub mod outcome;
pub mod record;
pub mod runner;
pub(crate) mod template;

pub use checkpoint::Checkpoint;
pub use context::{ExecutionContext, RunOptions, TriggerKind, TriggerMeta};
pub use handler::{
    ConditionHandler, FailHandler, HandlerRegistry, MergeHandler, NodeHandler, RespondHandler,
    StubHandler, SwitchHandler, TerminateHandler,
};
pub use outcome::{ExecutionOutcome, ExecutionTrace, HttpResponseSpec, NodeOutcome, TraceEntry};
pub use record::RunRecord;
pub use runner::{Engine, ReloadHandles};
