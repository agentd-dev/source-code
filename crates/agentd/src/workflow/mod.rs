//! Workflow document model, TOML parse, and DAG validator.
//!
//! A workflow is a directed acyclic graph of typed nodes with one or
//! more declared start nodes and explicit edges. The full shape is
//! specified in RFC §9; the TOML encoding is sketched in §17.2.
//!
//! The Phase 1 goal is to land the data types so the rest of the
//! runtime can compile against the surface. Parsing and validation
//! implementations are stubbed until Phase 1a / 1b.

pub mod model;

pub use model::{Edge, Node, NodeKind, StartNode, StartSource, WorkflowDoc};
