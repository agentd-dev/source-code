//! `[[mcp_servers]]` TOML block — operator-declarative list of MCP
//! stdio children to spawn alongside the runtime.
//!
//! Each entry names an MCP server (`name = "github"`), its spawn
//! command, and optional per-server allowlist. `call_mcp_tool` /
//! `read_mcp_resource` nodes route to a server by this name via
//! their `server` field.
//!
//! Why declarative over CLI-only: the single `--mcp-stdio "CMD ARGS"`
//! path scales to one server. Real workflows compose several
//! (language-server + issue-tracker + knowledge-base), each with
//! different tool allowlists. Stuffing that into CLI flags is
//! awkward and doesn't survive workflow-reload semantics.
//!
//! **Back-compat.** The `--mcp-stdio` CLI flag still works —
//! `runtime::build_engine` treats it as an implicit
//! `{ name = "default", command = [...] }` server, appended to the
//! TOML list. Workflows that pre-date multi-server can keep using
//! `type = "call_mcp_tool"` without a `server` field as long as
//! exactly one server is configured.

use serde::{Deserialize, Serialize};

/// One entry under `[[mcp_servers]]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpServerDef {
    /// Operator-chosen name. `call_mcp_tool.server = "<name>"`
    /// references this entry. Names are case-sensitive and must be
    /// unique within the workflow.
    pub name: String,

    /// Spawn command — first element is the program, rest are
    /// argv. Same shape the single `--mcp-stdio` flag accepts
    /// (space-split). Stored as a vector so JSON args with
    /// embedded spaces don't require shell quoting.
    pub command: Vec<String>,

    /// Per-server allowlist. Defaults to deny-everything (empty
    /// allowlist) so adding a server without a policy block fails
    /// closed. Use `allow_all = true` in dev if you really mean it.
    #[serde(default)]
    pub allow_tools: Vec<String>,

    /// Resource URI pattern allowlist (same grammar as tools:
    /// literal, `prefix/*`, `prefix/**`, or `*`).
    #[serde(default)]
    pub allow_resources: Vec<String>,

    /// Environment variables for the child process, as
    /// `VAR = "<secret name>"` pairs. Each value is a NAME resolved
    /// through the secrets registry at spawn ([[secrets]] source
    /// first, process env second) — so an MCP server gets its
    /// `DATABASE_URL` from a file, a command, or an OAuth2 grant
    /// without the secret ever entering this process's own
    /// environment (children inherit that too; this is additive).
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

impl McpServerDef {
    /// Validate a list of server defs against a workflow's expected
    /// shape. Checks: every entry has a non-empty name, spawn command
    /// has at least one element, names are unique.
    pub fn validate_list(entries: &[McpServerDef]) -> Result<(), String> {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for (i, def) in entries.iter().enumerate() {
            if def.name.trim().is_empty() {
                return Err(format!("mcp_servers[{i}]: name must be non-empty"));
            }
            if def.command.is_empty() {
                return Err(format!(
                    "mcp_servers[{i}] (`{name}`): command must have at least one element",
                    name = def.name
                ));
            }
            if def.command[0].trim().is_empty() {
                return Err(format!(
                    "mcp_servers[{i}] (`{name}`): command[0] is empty",
                    name = def.name
                ));
            }
            if !seen.insert(def.name.clone()) {
                return Err(format!(
                    "mcp_servers: duplicate name `{name}`",
                    name = def.name
                ));
            }
        }
        Ok(())
    }
}

/// Back-compat stand-in for the single `--mcp-stdio CMD ARG...`
/// CLI flag. Turns a flat argv into a default-named server def
/// with an allow-all tool/resource allowlist — the legacy semantic
/// was "whatever the CLI lets you reach, let you call."
pub fn from_cli_stdio(argv: Vec<String>) -> McpServerDef {
    McpServerDef {
        name: "default".into(),
        command: argv,
        allow_tools: vec!["*".into()],
        allow_resources: vec!["*".into()],
        env: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_unique_names() {
        let entries = vec![
            McpServerDef {
                name: "a".into(),
                command: vec!["/bin/a".into()],
                allow_tools: vec![],
                allow_resources: vec![],
                env: Default::default(),
            },
            McpServerDef {
                name: "a".into(),
                command: vec!["/bin/b".into()],
                allow_tools: vec![],
                allow_resources: vec![],
                env: Default::default(),
            },
        ];
        let err = McpServerDef::validate_list(&entries).unwrap_err();
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn validates_non_empty_command() {
        let entries = vec![McpServerDef {
            name: "a".into(),
            command: vec![],
            allow_tools: vec![],
            allow_resources: vec![],
            env: Default::default(),
        }];
        let err = McpServerDef::validate_list(&entries).unwrap_err();
        assert!(err.contains("at least one element"));
    }

    #[test]
    fn validates_non_empty_name() {
        let entries = vec![McpServerDef {
            name: "   ".into(),
            command: vec!["/bin/x".into()],
            allow_tools: vec![],
            allow_resources: vec![],
            env: Default::default(),
        }];
        let err = McpServerDef::validate_list(&entries).unwrap_err();
        assert!(err.contains("non-empty"));
    }

    #[test]
    fn cli_stdio_maps_to_default_server() {
        let def = from_cli_stdio(vec!["/bin/mcp".into(), "--port".into(), "3000".into()]);
        assert_eq!(def.name, "default");
        assert_eq!(def.command, vec!["/bin/mcp", "--port", "3000"]);
        assert_eq!(def.allow_tools, vec!["*"]);
    }

    #[test]
    fn parses_from_toml() {
        let src = r#"
            name = "github"
            command = ["/usr/local/bin/mcp-github", "--repo", "agentd-dev/source-code"]
            allow_tools = ["create_issue", "comment_on_*"]
            allow_resources = ["issue://**"]
        "#;
        let def: McpServerDef = toml::from_str(src).unwrap();
        assert_eq!(def.name, "github");
        assert_eq!(def.command.len(), 3);
        assert_eq!(def.allow_tools.len(), 2);
    }
}
