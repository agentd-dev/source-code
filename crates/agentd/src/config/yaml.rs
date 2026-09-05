// SPDX-License-Identifier: AGPL-3.0-only
//! Hand-rolled YAML subset parser — moved to [`instruction_core::yaml`] with
//! the C-01 extraction (the instruction parser owns its body format);
//! re-exported here so every `config::yaml::parse` call site keeps working.

pub use instruction_core::yaml::*;
