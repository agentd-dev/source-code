// SPDX-License-Identifier: AGPL-3.0-only
//! The **workflow engine**: the workflow model and its validation ([`model`]),
//! templates ([`template`]), the data steps ([`data`]) and the durable run
//! record with its pure scheduler ([`run`]).
//!
//! The split that matters: everything here is pure. Step *execution* — turn
//! workers, MCP calls, internal tools, timers — belongs to `crate::runtime`.
//! The engine only says what is ready to run and records what happened, which
//! is what lets a run be scheduled deterministically from its checkpoint after
//! a restart.

pub mod data;
pub mod model;
pub mod run;
pub mod template;

pub use model::{Workflow, parse_workflow, workflow_schema};
pub use run::{Next, RunState, RunStatus, StepStatus};
