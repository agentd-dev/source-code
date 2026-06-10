//! The dynamic-agent layer (RFC 0006).
//!
//! Everything here *produces or drives* structure that executes on
//! the bounded substrate — it never bypasses it:
//!
//! - [`loop_node`] — the `agent_loop` node kind: a bounded ReAct
//!   loop embedded in a declared graph node (Mode 2).
//! - [`instructions`] — `--instructions agent.toml`: the agent's
//!   standing identity, default backend, and loop-tool defaults.
//! - [`planner`] — goal mode (Mode 3): an LLM drafts a workflow
//!   TOML, the standard validator judges it, an approval gate
//!   decides whether it runs, and failures get bounded re-planning.

pub mod catalog;
pub mod instructions;
pub mod loop_node;
pub mod planner;

pub use instructions::AgentInstructions;
