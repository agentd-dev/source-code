//! Policy stub.
//!
//! Defines the [`Policy`] trait that tool handlers consult before
//! every side effect (RFC §10.5, §16). Phase 3 ships [`AllowAll`]
//! as the only implementation; Phase 7 adds a path- / domain- /
//! MCP-allowlist-driven policy that the operator wires at startup.

use std::path::Path;
use std::sync::Arc;

/// Outcome of a single policy check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// Policy hook invoked from tool handlers before every side effect.
///
/// All methods default to [`Decision::Allow`] so a handler can call
/// through unconditionally; real policies override only the checks
/// they narrow.
pub trait Policy: Send + Sync {
    fn check_fs_read(&self, _path: &Path) -> Decision {
        Decision::Allow
    }
    fn check_fs_write(&self, _path: &Path) -> Decision {
        Decision::Allow
    }
    fn check_fs_delete(&self, _path: &Path) -> Decision {
        Decision::Allow
    }
    fn check_fs_list(&self, _path: &Path) -> Decision {
        Decision::Allow
    }
    fn check_env_read(&self, _key: &str) -> Decision {
        Decision::Allow
    }
    /// Outbound HTTP request. `url` is the fully-formed request
    /// target; the policy gets to inspect method + scheme + host.
    fn check_http_request(&self, _method: &str, _url: &str) -> Decision {
        Decision::Allow
    }
}

/// Policy that grants every request. Phase 3 default; replaced by the
/// manifest-driven policy in Phase 7.
#[derive(Debug, Default)]
pub struct AllowAll;

impl Policy for AllowAll {}

/// Shared policy handle for tool handlers.
pub type PolicyRef = Arc<dyn Policy>;

pub fn allow_all() -> PolicyRef {
    Arc::new(AllowAll)
}
