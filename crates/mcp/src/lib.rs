// SPDX-License-Identifier: AGPL-3.0-only
//! **mcp** — agentd's Model Context Protocol client layer.
//!
//! A blocking facade over the official `rmcp` SDK ([`client`], [`rmcp_client`]),
//! run over agentd's own credentialed Streamable-HTTP socket ([`http`],
//! [`rmcp_transport`]): the SDK owns the protocol — the handshake, the version
//! negotiation, the typed requests and notifications — and agentd keeps the
//! connection, so a request signer and an mTLS client identity apply to every
//! dial. [`wire`] holds agentd's own view of the message types,
//! [`inbound`] the host seam for server→client elicitation, and [`rpc`] the
//! JSON-RPC codec agentd's other channels share.
//!
//! [`http_server`] is a raw HTTP/1.1 listener (TLS-terminated, `Origin`-guarded)
//! for embedders that serve plain HTTP, such as agentd's webhook listener. It
//! serves no MCP.
//!
//! Dependencies: the official `rmcp` SDK and the async seam it needs, over
//! agentd's own `net` transport — plus serde/serde_json.

pub mod client;
pub mod http;
pub mod http_server;
pub mod inbound;
pub mod rmcp_client;
/// agentd's credentialed socket under the SDK's transport trait.
pub mod rmcp_transport;
pub mod rpc;
pub mod wire;
