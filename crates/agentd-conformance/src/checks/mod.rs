// SPDX-License-Identifier: AGPL-3.0-only
//! The conformance check families. Each module exposes `checks() -> Vec<Check>`.
//!
//! The families are: [`supervisor`] (the exit-code table, drain, fail-fast),
//! [`security`] (trifecta refusal, secret redaction, tool scoping), [`store`]
//! (the durable-store contract), [`durability`] (the crash/restore contract),
//! [`tools`] (the internal tool registry), [`a2a_conversation`] (conversations,
//! principals & commands), and [`interface`] (the display-client surface).

pub mod a2a_conversation;
pub mod durability;
pub mod interface;
pub mod security;
pub mod store;
pub mod supervisor;
pub mod tools;
pub mod util;
