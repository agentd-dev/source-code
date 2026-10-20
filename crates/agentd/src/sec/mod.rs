// SPDX-License-Identifier: AGPL-3.0-only
pub mod harden;
pub mod landlock; // workers denied the vault and the key files (RFC 0044 P2a)
pub mod policy;
pub mod random;
pub mod render; // outside text as agentd's security text shows it (RFC 0044 P2b)
pub mod scope;
pub mod secret;
#[cfg(feature = "oauth")]
pub mod signer; // keys held by an external signer on a unix socket (RFC 0044 P2a)
#[cfg(feature = "mcp-events")]
pub mod stdwebhooks; // Standard Webhooks signatures (MCP Events webhook delivery)
pub mod worker; // workers under a uid of their own (RFC 0044 P2b)
