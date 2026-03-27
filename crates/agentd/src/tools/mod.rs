//! Foundational tool families (RFC §10).
//!
//! Each family is behind a Cargo feature so the built binary only
//! includes the tools its configured workflows need. Handlers are
//! concrete implementations of [`crate::engine::NodeHandler`] and
//! are registered into a [`HandlerRegistry`] by
//! [`register_default_tools`] (or by callers that want finer-grained
//! control).
//!
//! Every handler with a side effect consults a [`policy::Policy`]
//! before touching the outside world. In Phase 3 the policy defaults
//! to [`policy::AllowAll`]; Phase 7 wires the manifest-driven policy.

pub mod policy;

use crate::engine::HandlerRegistry;
use crate::tools::policy::PolicyRef;

/// Register every compiled-in foundational tool handler onto
/// `registry`, using `policy` for side-effect checks.
///
/// When no tool features are enabled the arguments go unused — the
/// underscore prefixes silence the lint.
pub fn register_default_tools(_registry: &mut HandlerRegistry, _policy: PolicyRef) {}
