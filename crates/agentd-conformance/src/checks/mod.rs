// SPDX-License-Identifier: Apache-2.0
//! The conformance check families. Each module exposes `checks() -> Vec<Check>`.

pub mod agent_loop;
pub mod mcp_client;
pub mod mcp_server;
pub mod security;
pub mod supervisor;
pub mod work_claim;
