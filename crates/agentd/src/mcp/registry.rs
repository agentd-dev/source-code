//! Multi-server MCP registry.
//!
//! Holds one [`McpServerHandle`] per configured `[[mcp_servers]]`
//! entry plus (optionally) a default entry fed by `--mcp-stdio`. The
//! MCP node handlers dispatch through this registry using the
//! `server` field on `call_mcp_tool` / `read_mcp_resource` — if
//! `server` is absent and exactly one entry exists, that entry is
//! used (back-compat with single-server workflows).
//!
//! Hot reload: each entry's client is wrapped in a
//! `ReloadableMcpClient` and its allowlist in a
//! `ReloadableMcpAllowlist`, so the SIGHUP reload path can
//! respawn an individual server or rotate its allowlist without
//! touching the registry shape. Adding or removing entries
//! requires a restart — that's out of scope for hot-reload because the
//! handler registry would need to pick up new bindings
//! mid-process.
//!
//! Thread-safety: the whole registry is immutable after
//! construction; every field inside the handles uses its own
//! ArcSwap for mutation.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Error;
use crate::mcp::allowlist::ReloadableMcpAllowlist;
use crate::mcp::client::ReloadableMcpClient;

/// One configured MCP server's runtime state.
pub struct McpServerHandle {
    /// Name the workflow references this server as. Stored for
    /// diagnostics + audit events; resolution already knows the
    /// key.
    pub name: String,
    /// Hot-reloadable client wrapper. Handlers read via the trait
    /// object; SIGHUP respawns replace the inner.
    pub client: Arc<ReloadableMcpClient>,
    /// Per-server allowlist, hot-reloadable.
    pub allowlist: Arc<ReloadableMcpAllowlist>,
}

impl std::fmt::Debug for McpServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerHandle")
            .field("name", &self.name)
            .finish()
    }
}

impl std::fmt::Debug for McpRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.servers.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("McpRegistry")
            .field("servers", &names)
            .finish()
    }
}

/// Map of configured servers, keyed by the operator-chosen name.
pub struct McpRegistry {
    servers: HashMap<String, Arc<McpServerHandle>>,
}

impl McpRegistry {
    pub fn empty() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    pub fn new(entries: impl IntoIterator<Item = Arc<McpServerHandle>>) -> Self {
        let servers = entries.into_iter().map(|h| (h.name.clone(), h)).collect();
        Self { servers }
    }

    pub fn len(&self) -> usize {
        self.servers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<McpServerHandle>> {
        self.servers.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<McpServerHandle>> {
        self.servers.values()
    }

    /// Resolve a workflow-node `server: Option<String>` reference
    /// to a handle. Policy:
    ///   * `Some(name)` → exact lookup, `Err` when unknown.
    ///   * `None` + exactly one server → that server (back-compat
    ///     with single-server workflows that pre-date `[[mcp_servers]]`).
    ///   * `None` + zero or >1 servers → `Err`.
    pub fn resolve(&self, requested: Option<&str>) -> Result<&Arc<McpServerHandle>, Error> {
        match requested {
            Some(name) => self.servers.get(name).ok_or_else(|| Error::Workflow {
                workflow: String::new(),
                reason: format!(
                    "unknown mcp server `{name}`; configured: [{}]",
                    self.name_list()
                ),
            }),
            None => {
                if self.servers.len() == 1 {
                    Ok(self.servers.values().next().expect("len == 1"))
                } else if self.servers.is_empty() {
                    Err(Error::Workflow {
                        workflow: String::new(),
                        reason: "call_mcp_tool / read_mcp_resource: no mcp_servers configured"
                            .into(),
                    })
                } else {
                    Err(Error::Workflow {
                        workflow: String::new(),
                        reason: format!(
                            "call_mcp_tool / read_mcp_resource: `server` field required \
                             when multiple mcp_servers are configured; got [{}]",
                            self.name_list()
                        ),
                    })
                }
            }
        }
    }

    fn name_list(&self) -> String {
        let mut names: Vec<&str> = self.servers.keys().map(String::as_str).collect();
        names.sort_unstable();
        names.join(", ")
    }
}

/// Shared handle the handlers capture at registration time.
pub type McpRegistryRef = Arc<McpRegistry>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::allowlist::McpAllowlist;
    use crate::mcp::client::MockMcpClient;

    fn handle(name: &str) -> Arc<McpServerHandle> {
        Arc::new(McpServerHandle {
            name: name.into(),
            client: Arc::new(ReloadableMcpClient::new(
                Box::new(MockMcpClient::new()) as Box<dyn crate::mcp::client::McpClient>
            )),
            allowlist: Arc::new(ReloadableMcpAllowlist::new(McpAllowlist::allow_all())),
        })
    }

    #[test]
    fn resolve_none_with_one_server_returns_it() {
        let r = McpRegistry::new(vec![handle("only")]);
        let h = r.resolve(None).unwrap();
        assert_eq!(h.name, "only");
    }

    #[test]
    fn resolve_none_with_multiple_errors() {
        let r = McpRegistry::new(vec![handle("a"), handle("b")]);
        let err = r.resolve(None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("server` field required"), "msg: {msg}");
        assert!(msg.contains("a, b"), "msg: {msg}");
    }

    #[test]
    fn resolve_unknown_name_errors_with_list() {
        let r = McpRegistry::new(vec![handle("github"), handle("linear")]);
        let err = r.resolve(Some("jira")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown mcp server `jira`"), "msg: {msg}");
        assert!(msg.contains("github"));
        assert!(msg.contains("linear"));
    }

    #[test]
    fn resolve_known_name_hits() {
        let r = McpRegistry::new(vec![handle("github"), handle("linear")]);
        let h = r.resolve(Some("linear")).unwrap();
        assert_eq!(h.name, "linear");
    }

    #[test]
    fn resolve_none_with_empty_registry_errors() {
        let r = McpRegistry::empty();
        let err = r.resolve(None).unwrap_err();
        assert!(format!("{err}").contains("no mcp_servers configured"));
    }
}
