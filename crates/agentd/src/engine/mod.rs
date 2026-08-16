// SPDX-License-Identifier: Apache-2.0
//! The **workflow engine v3** (RFC 0027): the dialect-3 model + validation
//! ([`model`]), templates ([`template`]), and the durable run record + pure
//! scheduler ([`run`]). Step *execution* (turn workers, MCP calls, internal
//! tools, timers) is the runtime's job (`crate::runtime`): the engine says
//! what is ready and records what happened. P3 ships the model, validation
//! and the scheduler with the core step kinds; P4 completes the catalogue
//! (iteration, waits, child runs, start nodes beyond `once`/`manual`).

pub mod data;
pub mod model;
pub mod run;
pub mod template;

pub use model::{Workflow, parse_workflow, workflow_schema};
pub use run::{Next, RunState, RunStatus, StepStatus};
