//! MCP allowlist policy (RFC §12.2).
//!
//! Separate from [`crate::tools::policy::Policy`] because MCP
//! concerns — tool name, resource URI pattern — are different
//! enough from fs/env checks that a dedicated type keeps the
//! interfaces legible. The engine holds one
//! [`McpAllowlist`] per run; handlers query it before every call.
//!
//! Hot-reload: handlers actually hold an `Arc<ReloadableMcpAllowlist>`
//! wrapper that forwards to the current allowlist via `ArcSwap`, so
//! `[policy.mcp]` edits take effect on SIGHUP without touching the
//! handler registry.

/// Allowlist keyed on the server + item names. An empty allowlist
/// denies everything — fail-closed by default.
#[derive(Debug, Default, Clone)]
pub struct McpAllowlist {
    pub allowed_tools: Vec<String>,
    pub allowed_resource_patterns: Vec<String>,
}

impl McpAllowlist {
    /// Construct a wide-open allowlist — useful for tests and for
    /// dev runs where policy narrowing is not yet configured. Real
    /// deployments build the allowlist from config.
    pub fn allow_all() -> Self {
        Self {
            allowed_tools: vec!["*".into()],
            allowed_resource_patterns: vec!["*".into()],
        }
    }

    pub fn tool_allowed(&self, tool: &str) -> bool {
        list_matches(&self.allowed_tools, tool)
    }

    pub fn resource_allowed(&self, uri: &str) -> bool {
        list_matches(&self.allowed_resource_patterns, uri)
    }
}

// ---------------------------------------------------------------------------
// Hot-reloadable allowlist
// ---------------------------------------------------------------------------

/// Process-wide `McpAllowlist` held behind an `ArcSwap` so SIGHUP
/// can replace the whole allowlist atomically without touching the
/// MCP handler registry. Public API matches [`McpAllowlist`] so
/// call sites don't care whether they're holding the raw type or
/// the reloadable one.
pub struct ReloadableMcpAllowlist {
    inner: arc_swap::ArcSwap<McpAllowlist>,
}

impl ReloadableMcpAllowlist {
    pub fn new(initial: McpAllowlist) -> Self {
        Self {
            inner: arc_swap::ArcSwap::from_pointee(initial),
        }
    }

    /// Atomically replace the inner allowlist. In-flight checks
    /// that already dereferenced the old one finish against it;
    /// subsequent checks see the new list.
    pub fn swap(&self, next: McpAllowlist) {
        self.inner.store(std::sync::Arc::new(next));
    }

    pub fn tool_allowed(&self, tool: &str) -> bool {
        self.inner.load().tool_allowed(tool)
    }

    pub fn resource_allowed(&self, uri: &str) -> bool {
        self.inner.load().resource_allowed(uri)
    }
}

/// Minimal matcher: `"*"` matches anything; otherwise an entry
/// ending in `*` is a prefix match, and exact strings match on
/// equality. Good enough for Phase 5; a proper glob/URI matcher
/// can slot in under the same API later.
fn list_matches(list: &[String], needle: &str) -> bool {
    list.iter().any(|entry| matches_entry(entry, needle))
}

fn matches_entry(entry: &str, needle: &str) -> bool {
    if entry == "*" {
        return true;
    }
    if let Some(prefix) = entry.strip_suffix('*') {
        return needle.starts_with(prefix);
    }
    entry == needle
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloadable_swap_changes_visible_allowlist() {
        let r = ReloadableMcpAllowlist::new(McpAllowlist {
            allowed_tools: vec!["only_old".into()],
            ..Default::default()
        });
        assert!(r.tool_allowed("only_old"));
        assert!(!r.tool_allowed("new_one"));

        r.swap(McpAllowlist {
            allowed_tools: vec!["new_one".into()],
            ..Default::default()
        });
        assert!(!r.tool_allowed("only_old"));
        assert!(r.tool_allowed("new_one"));
    }

    #[test]
    fn default_allowlist_denies_everything() {
        let a = McpAllowlist::default();
        assert!(!a.tool_allowed("any"));
        assert!(!a.resource_allowed("docs://x"));
    }

    #[test]
    fn wildcard_allowlist_allows_everything() {
        let a = McpAllowlist::allow_all();
        assert!(a.tool_allowed("any_tool"));
        assert!(a.resource_allowed("any://uri"));
    }

    #[test]
    fn exact_match_tools() {
        let a = McpAllowlist {
            allowed_tools: vec!["comment_on_page".into()],
            ..Default::default()
        };
        assert!(a.tool_allowed("comment_on_page"));
        assert!(!a.tool_allowed("delete_page"));
    }

    #[test]
    fn prefix_wildcard_resources() {
        let a = McpAllowlist {
            allowed_resource_patterns: vec!["docs://pages/*".into()],
            ..Default::default()
        };
        assert!(a.resource_allowed("docs://pages/42"));
        assert!(a.resource_allowed("docs://pages/anything/nested"));
        assert!(!a.resource_allowed("docs://other/42"));
        assert!(!a.resource_allowed("secret://thing"));
    }
}
