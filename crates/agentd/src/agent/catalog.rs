//! Capability catalogue (RFC 0006 §3) — what the planner is told it
//! may use.
//!
//! When the agent compiles its own workflow from an instruction, the
//! planner prompt must describe the agent's *actual* capabilities, not
//! a hardcoded menu: only the node kinds this binary can execute (a
//! build without `tools-http` must not be offered `http_request`), the
//! intelligence backends that are configured, the policy allowlists the
//! plan has to stay inside, and the MCP servers + tools that exist.
//!
//! The catalogue is rendered into the planner system prompt alongside
//! the instruction. It is descriptive only — it grants nothing; the
//! same validator and policy gates still bind whatever the planner
//! produces.

use crate::workflow::WorkflowDoc;

/// Everything the planner is allowed to reference, assembled from the
/// compiled feature set plus the operator-provided base config.
pub struct CapabilityCatalog {
    /// Configured intelligence backend names (`llm_infer` /
    /// `agent_loop` may reference these).
    pub backends: Vec<String>,
    /// MCP servers and the tools each exposes to the workflow.
    pub mcp: Vec<McpServerCaps>,
    /// Rendered policy allowlist summary (or the AllowAll note).
    pub policy_summary: String,
}

pub struct McpServerCaps {
    pub name: String,
    pub tools: Vec<String>,
    pub resources: Vec<String>,
}

impl CapabilityCatalog {
    /// Build from an optional base config (its `[policy]`,
    /// `[[mcp_servers]]`) and the resolved backend names.
    pub fn from_base(base: Option<&WorkflowDoc>, backends: Vec<String>) -> Self {
        let mcp = base
            .map(|d| {
                d.mcp_servers
                    .iter()
                    .map(|s| McpServerCaps {
                        name: s.name.clone(),
                        tools: s.allow_tools.clone(),
                        resources: s.allow_resources.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let policy_summary = base
            .and_then(|d| d.policy.as_ref())
            .map(render_policy)
            .unwrap_or_else(|| {
                "no [policy] block — the workflow runs under AllowAll. \
                 Still prefer the narrowest tools that accomplish the task."
                    .to_string()
            });
        Self {
            backends,
            mcp,
            policy_summary,
        }
    }

    /// Render the catalogue as a prompt section.
    pub fn render(&self) -> String {
        let mut s = String::new();

        s.push_str("Node types this build can execute (use ONLY these):\n");
        for line in available_node_kinds(!self.backends.is_empty()) {
            s.push_str("- ");
            s.push_str(line);
            s.push('\n');
        }

        s.push('\n');
        if self.backends.is_empty() {
            s.push_str(
                "Intelligence backends: none configured — do NOT emit \
                 llm_infer or agent_loop nodes.\n",
            );
        } else {
            s.push_str(&format!(
                "Intelligence backends (reference by name in `backend`): {}.\n",
                self.backends.join(", ")
            ));
        }

        if !self.mcp.is_empty() {
            s.push_str("\nMCP servers and their allowed tools (call_mcp_tool):\n");
            for m in &self.mcp {
                s.push_str(&format!(
                    "- server `{}`: tools [{}]",
                    m.name,
                    if m.tools.is_empty() {
                        "none".into()
                    } else {
                        m.tools.join(", ")
                    }
                ));
                if !m.resources.is_empty() {
                    s.push_str(&format!("; resources [{}]", m.resources.join(", ")));
                }
                s.push('\n');
            }
        }

        s.push_str("\nActive policy (any tool call outside this is denied at runtime):\n");
        s.push_str(&self.policy_summary);
        s.push('\n');
        s
    }
}

/// Node kinds the compiled binary can actually run. Feature-gated
/// families are omitted so the planner never proposes a node this
/// build would reject with CapabilityUnavailable.
fn available_node_kinds(have_backend: bool) -> Vec<&'static str> {
    let mut v = vec![
        // control — always present
        "condition{expr}  switch{expr}  merge  fail{reason?}  terminate",
        "respond{status?,content_type?,body_template,input_from?} — shape the HTTP reply of an http-triggered run",
    ];
    #[cfg(feature = "tools-fs")]
    v.push("read_file{path_from}  write_file{path_from,content_from}  create_dir{path_from}");
    #[cfg(feature = "tools-env")]
    v.push("read_env{key}");
    #[cfg(feature = "tools-data")]
    v.push(
        "parse_json{input_from}  json_select{input_from,path}  \
         template_render{template,input_from?}  diff_compute{left_from,right_from}",
    );
    #[cfg(feature = "tools-http")]
    v.push("http_request{method,url_from,body_from?}");
    #[cfg(feature = "tools-shell")]
    v.push("shell_run{command,args_from?}  (allowlisted absolute path)");
    #[cfg(feature = "tools-mcp")]
    v.push("call_mcp_tool{tool,args_from?,server?}  read_mcp_resource{resource_from,server?}");
    if have_backend {
        v.push("llm_infer{backend,prompt,input_from?,output_schema?}");
        v.push(
            "agent_loop{backend,instructions|instructions_from,tools[],max_steps,max_tokens?}  \
             (bounded ReAct inside one node; max_steps<=64)",
        );
    }
    v
}

/// One-line-per-family summary of a policy manifest.
fn render_policy(p: &crate::policy::PolicyManifest) -> String {
    let mut lines = Vec::new();
    let join = |v: &[String]| {
        if v.is_empty() {
            "<none>".to_string()
        } else {
            v.join(", ")
        }
    };
    if !p.fs.read.is_empty() || !p.fs.write.is_empty() || !p.fs.delete.is_empty() {
        lines.push(format!(
            "  fs.read [{}]  fs.write [{}]  fs.delete [{}]",
            join(&p.fs.read),
            join(&p.fs.write),
            join(&p.fs.delete)
        ));
    }
    if !p.env.read_keys.is_empty() {
        lines.push(format!("  env.read [{}]", join(&p.env.read_keys)));
    }
    if !p.http.urls.is_empty() {
        lines.push(format!(
            "  http.urls [{}]  http.methods [{}]",
            join(&p.http.urls),
            join(&p.http.methods)
        ));
    }
    if !p.shell.commands.is_empty() {
        lines.push(format!("  shell.commands [{}]", join(&p.shell.commands)));
    }
    if !p.mcp.tools.is_empty() || !p.mcp.resources.is_empty() {
        lines.push(format!(
            "  mcp.tools [{}]  mcp.resources [{}]",
            join(&p.mcp.tools),
            join(&p.mcp.resources)
        ));
    }
    if lines.is_empty() {
        "  [policy] present but empty — fail-closed: every side effect denied.".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{EnvPolicy, FsPolicy, PolicyManifest};

    #[test]
    fn render_lists_node_kinds_and_backends() {
        let cat = CapabilityCatalog::from_base(None, vec!["claude".into()]);
        let out = cat.render();
        assert!(out.contains("terminate"));
        assert!(out.contains("llm_infer"));
        assert!(out.contains("claude"));
        assert!(out.contains("AllowAll"));
    }

    #[test]
    fn no_backend_suppresses_llm_nodes() {
        let cat = CapabilityCatalog::from_base(None, vec![]);
        let out = cat.render();
        assert!(out.contains("do NOT emit"));
        assert!(!out.contains("llm_infer{"));
    }

    #[test]
    fn policy_summary_reflects_allowlists() {
        let mut doc = WorkflowDoc {
            name: "x".into(),
            ..Default::default()
        };
        doc.policy = Some(PolicyManifest {
            fs: FsPolicy {
                write: vec!["/tmp/out/**".into()],
                ..Default::default()
            },
            env: EnvPolicy {
                read_keys: vec!["TOKEN".into()],
            },
            ..Default::default()
        });
        let cat = CapabilityCatalog::from_base(Some(&doc), vec!["m".into()]);
        let out = cat.render();
        assert!(out.contains("/tmp/out/**"));
        assert!(out.contains("TOKEN"));
    }

    #[test]
    fn mcp_servers_listed_with_tools() {
        let mut doc = WorkflowDoc {
            name: "x".into(),
            ..Default::default()
        };
        doc.mcp_servers = vec![crate::mcp::config::McpServerDef {
            name: "github".into(),
            command: vec!["/bin/mcp".into()],
            allow_tools: vec!["comment".into()],
            allow_resources: vec![],
        }];
        let cat = CapabilityCatalog::from_base(Some(&doc), vec!["m".into()]);
        let out = cat.render();
        assert!(out.contains("server `github`"));
        assert!(out.contains("comment"));
    }
}
