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

#[cfg(feature = "tools-fs")]
pub mod fs;

#[cfg(feature = "tools-env")]
pub mod env;

use crate::engine::HandlerRegistry;
use crate::tools::policy::PolicyRef;

// Helper functions used by fs / data handlers. Gated to avoid
// unused-warnings when no tool families are compiled in.
#[cfg(feature = "tools-fs")]
mod resolve {
    use serde_json::Value;

    use crate::engine::ExecutionContext;
    use crate::error::{Error, Result};

    /// Resolve a dotted path in the context down to an owned JSON
    /// value. Returns an `Error::Tool` if the path is missing.
    pub(crate) fn resolve_value(
        tool: &'static str,
        ctx: &ExecutionContext,
        path: &str,
    ) -> Result<Value> {
        ctx.resolve_path(path).cloned().ok_or_else(|| Error::Tool {
            tool: tool.into(),
            reason: format!("path `{path}` is not set in the execution context"),
        })
    }

    /// Resolve a path and require the value to be a JSON string.
    pub(crate) fn resolve_string(
        tool: &'static str,
        ctx: &ExecutionContext,
        path: &str,
    ) -> Result<String> {
        match resolve_value(tool, ctx, path)? {
            Value::String(s) => Ok(s),
            other => Err(Error::Tool {
                tool: tool.into(),
                reason: format!(
                    "path `{path}` must resolve to a string; got {}",
                    value_type_name(&other)
                ),
            }),
        }
    }

    pub(crate) fn value_type_name(v: &Value) -> &'static str {
        match v {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

#[cfg(feature = "tools-fs")]
pub(crate) use resolve::{resolve_string, resolve_value, value_type_name};

/// Register every compiled-in foundational tool handler onto
/// `registry`, using `policy` for side-effect checks.
///
/// When no tool features are enabled the arguments go unused — the
/// underscore prefixes silence the lint.
pub fn register_default_tools(_registry: &mut HandlerRegistry, _policy: PolicyRef) {
    #[cfg(feature = "tools-fs")]
    fs::register(_registry, _policy.clone());

    #[cfg(feature = "tools-env")]
    env::register(_registry, _policy.clone());
}
