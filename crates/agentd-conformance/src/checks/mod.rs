// SPDX-License-Identifier: AGPL-3.0-only
//! The conformance check families. Each module exposes `checks() -> Vec<Check>`.
//!
//! agentd note: the v1-only families (`mcp-server` — the v1 self-MCP surface,
//! `mcp-client` — the v1 reactive discovery path, `work-claim` — the `cluster`
//! lease) were retired with the mode cut-over and rebuilt as the v2 families
//! (P7): [`store`] (RFC 0025 durable-store contract), [`durability`] (the
//! crash/restore contract), [`tools`] (RFC 0028 registry), and
//! [`a2a_conversation`] (RFC 0029 conversations, principals & commands).

pub mod a2a_conversation;
pub mod durability;
pub mod interface;
pub mod security;
pub mod store;
pub mod supervisor;
pub mod tools;
pub mod util;
