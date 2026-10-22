// SPDX-License-Identifier: AGPL-3.0-only
//! The conformance check families. Each module exposes `checks() -> Vec<Check>`.
//!
//! The families are: [`supervisor`] (the exit-code table, drain, fail-fast),
//! [`security`] (trifecta refusal, secret redaction, tool scoping), [`store`]
//! (the durable-store contract), [`durability`] (the crash/restore contract),
//! [`tools`] (the internal tool registry), [`a2a_conversation`] (the core A2A
//! surface), [`events`] (the observation feed), [`extensions`] (activation and
//! what an extension may add to a reply), [`auth`] (the card's security
//! promises held against the listener), [`mcp_auth_core`] (agentd as an
//! OAuth client: metadata discovery, the issuer, revocation), [`mcp_tasks`]
//! (agentd as a client of the MCP Tasks extension), [`mcp_skills`] (agentd
//! as a client of the MCP Skills extension) and
//! [`companies`] (the fake Company, held to the Personal Agent Protocol
//! draft by the [`probe`] Personal Agent, and agentd held to it as the
//! Personal Agent by [`companies_agent`]), [`ema`] (agentd acting for an
//! enrolled person through their IdP: ID-JAG and Enterprise-Managed
//! Authorization), and [`payments`] (agentd paying with x402 within ADR
//! 0003's spend policy).

pub mod a2a_conversation;
pub mod auth;
pub mod companies;
pub mod companies_agent;
pub mod durability;
pub mod ema;
pub mod events;
pub mod extensions;
pub mod mcp_auth_core;
pub mod mcp_skills;
pub mod mcp_tasks;
pub mod payments;
pub mod probe;
pub mod security;
pub mod store;
pub mod supervisor;
pub mod tools;
pub mod util;
