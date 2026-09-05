// SPDX-License-Identifier: AGPL-3.0-only
//! **The Instruction Document** — a re-export of the extracted
//! [`instruction_core::doc`] reference implementation (C-01), so every
//! existing `crate::config::idoc::…` call site keeps working and agentd and
//! the platform run byte-identical parser/validator/delivery code. The
//! agentd-specific configuration FOLDING (workflows splice, mcp servers,
//! tools.narrow, secret-ref rewriting) lives in the crate too — it is plain
//! serde_json shapes, not agentd types — which is what made the extraction a
//! move rather than a rewrite.

pub use instruction_core::doc::*;
