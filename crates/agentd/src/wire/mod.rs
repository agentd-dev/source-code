// SPDX-License-Identifier: AGPL-3.0-only
pub mod intel;
// The MCP wire types + version/era model now live in the reusable `mcp` crate;
// re-export so `crate::wire::mcp::*` keeps resolving across the runtime.
pub use ::mcp::wire as mcp;
