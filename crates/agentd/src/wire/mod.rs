// SPDX-License-Identifier: Apache-2.0
pub mod intel;
// The MCP wire types + version/era model now live in the reusable `mcp` crate;
// re-export so `crate::wire::mcp::*` keeps resolving across the runtime.
pub use ::mcp::wire as mcp;
