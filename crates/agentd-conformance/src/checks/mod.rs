// SPDX-License-Identifier: AGPL-3.0-only
//! The conformance check families. Each module exposes `checks() -> Vec<Check>`.
//!
//! The families are: [`supervisor`] (the exit-code table, drain, fail-fast),
//! [`security`] (trifecta refusal, secret redaction, tool scoping), [`store`]
//! (the durable-store contract), [`durability`] (the crash/restore contract),
//! [`tools`] (the internal tool registry), [`a2a_conversation`] (the core A2A
//! surface), [`events`] (the observation feed), [`extensions`] (activation and
//! what an extension may add to a reply) and [`auth`] (the card's security
//! promises held against the listener).

pub mod a2a_conversation;
pub mod auth;
pub mod durability;
pub mod events;
pub mod extensions;
pub mod security;
pub mod store;
pub mod supervisor;
pub mod tools;
pub mod util;
