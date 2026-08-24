// SPDX-License-Identifier: AGPL-3.0-only
pub mod intel;
// The MCP wire types and version/era model live in the reusable `mcp` crate;
// re-exported here so `crate::wire::mcp::*` resolves throughout the runtime.
pub use ::mcp::wire as mcp;
