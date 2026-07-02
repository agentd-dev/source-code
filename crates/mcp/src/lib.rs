// SPDX-License-Identifier: Apache-2.0
//! **mcp** — the Model Context Protocol base library.
//!
//! A reusable, agentd-independent core for speaking MCP across every protocol
//! revision. Two protocol **eras** coexist (see [`version`]):
//!
//! * **Legacy** (`2025-11-25` and earlier): an `initialize` handshake establishes
//!   a session; the negotiated version + capabilities are learned once, and
//!   server→client messages ride a session-scoped SSE stream.
//! * **Modern** (`2026-07-28`+, "stateless"): no handshake and no session — every
//!   request carries its protocol version, client identity, and capabilities in
//!   `_meta` (mirrored to `MCP-Protocol-Version` / `Mcp-Method` / `Mcp-Name`
//!   headers on Streamable HTTP), any request can hit any server instance, and
//!   long-lived notifications ride a `subscriptions/listen` response stream.
//!
//! This crate keeps that era logic in one place so a client or server built on it
//! can be **dual-era** without branching everywhere. Phase 1 provides the
//! [`wire`] types and the [`version`] model; the [`client`], [`http`] transport,
//! and [`server`] base build on them.
//!
//! Dependency budget: `serde` + `serde_json` only (the agentd minimalism moat).

pub mod client;
pub mod http;
pub mod modern;
pub mod rpc;
pub mod server;
pub mod version;
pub mod wire;
