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
    /// Shell / sub-process invocation. `command` has been
    /// canonicalised to an absolute path before the handler reaches
    /// the policy.
    fn check_shell_run(&self, _command: &Path) -> Decision {
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

// ---------------------------------------------------------------------------
// Hot-reloadable policy wrapper
// ---------------------------------------------------------------------------

/// [`Policy`] implementation that holds its inner `Arc<dyn Policy>`
/// behind an [`arc_swap::ArcSwap`] so the runtime can replace the
/// whole policy atomically on SIGHUP.
///
/// Handlers keep holding the exact same `Arc<dyn Policy>` they
/// captured at registration — there's no signature change. Every
/// check-method defers to the currently-stored inner policy via
/// `load()`; in-flight checks that have already dereferenced the
/// snapshot complete against the old policy, matching the rest of
/// the hot-reload surface (TLS, auth, routes) which also works on
/// per-snapshot semantics.
///
/// The `regorus::Engine` thread-locals inside `ManifestPolicy`
/// self-invalidate through `RegoSpec.id`: a new `ManifestPolicy`
/// built from a reloaded `[policy.rego]` block has a fresh id,
/// which triggers per-thread recompilation on first use.
pub struct ReloadablePolicy {
    inner: arc_swap::ArcSwap<PolicyArc>,
}

/// Inner type the `ArcSwap` stores. `Box<dyn Policy>` rather than
/// `Arc<dyn Policy>` avoids double-Arc indirection — ArcSwap adds
/// its own `Arc` layer around whatever it wraps.
type PolicyArc = Box<dyn Policy>;

impl ReloadablePolicy {
    /// Wrap an initial policy. The returned value implements
    /// [`Policy`]; callers put it in an `Arc` and pass it to
    /// handler registration exactly like any other `PolicyRef`.
    pub fn new(initial: Box<dyn Policy>) -> Self {
        Self {
            inner: arc_swap::ArcSwap::from_pointee(initial),
        }
    }

    /// Atomically replace the inner policy. Next check on any
    /// thread sees the new rules; in-flight checks that already
    /// hold a `Guard` complete against the old policy.
    pub fn swap(&self, next: Box<dyn Policy>) {
        self.inner.store(Arc::new(next));
    }
}

impl Policy for ReloadablePolicy {
    fn check_fs_read(&self, path: &Path) -> Decision {
        self.inner.load().check_fs_read(path)
    }
    fn check_fs_write(&self, path: &Path) -> Decision {
        self.inner.load().check_fs_write(path)
    }
    fn check_fs_delete(&self, path: &Path) -> Decision {
        self.inner.load().check_fs_delete(path)
    }
    fn check_fs_list(&self, path: &Path) -> Decision {
        self.inner.load().check_fs_list(path)
    }
    fn check_env_read(&self, key: &str) -> Decision {
        self.inner.load().check_env_read(key)
    }
    fn check_http_request(&self, method: &str, url: &str) -> Decision {
        self.inner.load().check_http_request(method, url)
    }
    fn check_shell_run(&self, command: &Path) -> Decision {
        self.inner.load().check_shell_run(command)
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;
    use std::path::PathBuf;

    struct DenyAll;
    impl Policy for DenyAll {
        fn check_fs_read(&self, _: &Path) -> Decision {
            Decision::Deny("nope".into())
        }
    }

    #[test]
    fn swap_changes_check_result() {
        let reloadable = ReloadablePolicy::new(Box::new(AllowAll));
        let path = PathBuf::from("/etc/hosts");
        assert!(matches!(reloadable.check_fs_read(&path), Decision::Allow));

        reloadable.swap(Box::new(DenyAll));
        assert!(matches!(reloadable.check_fs_read(&path), Decision::Deny(_)));
    }

    #[test]
    fn swap_is_visible_through_arc_dyn_policy_boundary() {
        // Mirror the engine-side plumbing: handlers hold
        // `Arc<dyn Policy>`, reload swaps the inner via a separate
        // handle. Verify the handlers' captured `Arc` sees the new
        // inner after a swap.
        let reloadable = Arc::new(ReloadablePolicy::new(Box::new(AllowAll)));
        let as_policy: PolicyRef = reloadable.clone();
        let p = PathBuf::from("/x");
        assert!(matches!(as_policy.check_fs_read(&p), Decision::Allow));

        reloadable.swap(Box::new(DenyAll));
        assert!(matches!(as_policy.check_fs_read(&p), Decision::Deny(_)));
    }
}
