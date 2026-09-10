// SPDX-License-Identifier: AGPL-3.0-only
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
//! can be **dual-era** without branching everywhere. [`wire`] holds the message
//! types and [`version`] the era model; the [`client`], [`http`] transport, and
//! [`server`] base build on them.
//!
//! Dependencies: the official `rmcp` SDK and the async seam it needs, over
//! agentd's own `net` transport — plus serde/serde_json, and `vsock` only when
//! the `vsock` feature asks the server to listen on one. (The header in
//! `Cargo.toml` says the same thing; this copy said "serde + serde_json only",
//! which stopped being true when the SDK was adopted.)

pub mod client;
pub mod http;
pub mod http_server;
pub mod inbound;
pub mod modern;
pub mod rmcp_client;
/// agentd's credentialed socket under the SDK's transport trait.
pub mod rmcp_transport;
pub mod rpc;
pub mod server;
pub mod version;
pub mod wire;
