//! Capability-altitude plan review.
//!
//! A human approving a compiled plan should not have to read TOML. This
//! renders a plan as what it *does to the world* — what it reads, what
//! it writes, where it reaches on the network, which models it calls —
//! plus the policy it runs under. The raw TOML stays one flag away
//! (`--plan-only` / `--plan-out`) for anyone who wants it.

use std::collections::BTreeSet;

use crate::workflow::{NodeKind, WorkflowDoc};

/// Render a plan as a capability summary for the approval gate.
pub fn summarize_plan(doc: &WorkflowDoc, policy_summary: &str) -> String {
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    let mut network = Vec::new();
    let mut exec = Vec::new();
    let mut mcp = Vec::new();
    let mut compose = Vec::new();
    let mut backends: BTreeSet<&str> = BTreeSet::new();
    let mut kinds: BTreeSet<&'static str> = BTreeSet::new();

    for node in &doc.nodes {
        kinds.insert(node.kind.name());
        match &node.kind {
            NodeKind::ReadFile { path_from } => reads.push(format!("file (path ← {path_from})")),
            NodeKind::ReadEnv { key } => reads.push(format!("env[{key}]")),
            NodeKind::ReadMcpResource { resource_from, .. } => {
                reads.push(format!("mcp resource (← {resource_from})"))
            }
            NodeKind::WriteFile { path_from, .. } => {
                writes.push(format!("file (path ← {path_from})"))
            }
            NodeKind::CreateDir { path_from } => writes.push(format!("dir (path ← {path_from})")),
            NodeKind::HttpRequest {
                method, url_from, ..
            } => network.push(format!("{method} (url ← {url_from})")),
            NodeKind::ShellRun { command, .. } => exec.push(command.clone()),
            NodeKind::CallMcpTool { tool, .. } => mcp.push(tool.clone()),
            NodeKind::Call { workflow, .. } => compose.push(workflow.clone()),
            NodeKind::LlmInfer { backend, .. } => {
                backends.insert(backend.as_str());
            }
            NodeKind::AgentLoop { backend, .. } => {
                backends.insert(backend.as_str());
            }
            _ => {}
        }
    }

    let mut s = String::new();
    s.push_str(&format!(
        "Plan `{}`: {} node(s), {} edge(s).\n",
        doc.name,
        doc.nodes.len(),
        doc.edges.len()
    ));
    let line = |s: &mut String, label: &str, items: &[String]| {
        if !items.is_empty() {
            s.push_str(&format!("  {label}: {}\n", items.join(", ")));
        }
    };
    line(&mut s, "Reads ", &reads);
    line(&mut s, "Writes", &writes);
    line(&mut s, "Network", &network);
    line(&mut s, "Shell ", &exec);
    line(&mut s, "MCP   ", &mcp);
    line(&mut s, "Calls ", &compose);
    if !backends.is_empty() {
        s.push_str(&format!(
            "  Models: backends {}\n",
            backends.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    // The side-effecting kinds are what an approver most cares about.
    let world: Vec<&str> = kinds.iter().copied().filter(|k| touches_world(k)).collect();
    if world.is_empty() {
        s.push_str("  Side effects: none — read-only / pure plan.\n");
    } else {
        s.push_str(&format!("  Touches the world via: {}\n", world.join(", ")));
    }

    s.push_str("  Runs under policy:\n");
    for l in policy_summary.lines() {
        s.push_str(&format!("    {}\n", l.trim_start()));
    }
    s
}

/// Node kinds that cause an external side effect (the ones an approver
/// weighs). Read-only and pure-data kinds are excluded.
fn touches_world(kind: &str) -> bool {
    matches!(
        kind,
        "write_file" | "create_dir" | "http_request" | "shell_run" | "call_mcp_tool" | "call"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{Node, NodeKind};

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.into(),
            retry: None,
            kind,
        }
    }

    #[test]
    fn summary_lists_effects_and_backends() {
        let doc = WorkflowDoc {
            name: "p".into(),
            nodes: vec![
                node(
                    "ask",
                    NodeKind::LlmInfer {
                        backend: "claude".into(),
                        prompt: "x".into(),
                        input_from: None,
                        output_schema: None,
                    },
                ),
                node(
                    "save",
                    NodeKind::WriteFile {
                        path_from: "ask.parsed.path".into(),
                        content_from: "ask.content".into(),
                    },
                ),
            ],
            ..Default::default()
        };
        let out = summarize_plan(&doc, "fs.write [/tmp/**]");
        assert!(out.contains("Plan `p`"));
        assert!(out.contains("backends claude"));
        assert!(out.contains("ask.parsed.path"));
        assert!(out.contains("Touches the world via: write_file"));
        assert!(out.contains("/tmp/**"));
    }

    #[test]
    fn read_only_plan_says_no_side_effects() {
        let doc = WorkflowDoc {
            name: "ro".into(),
            nodes: vec![
                node("r", NodeKind::ReadEnv { key: "HOME".into() }),
                node("done", NodeKind::Terminate),
            ],
            ..Default::default()
        };
        let out = summarize_plan(&doc, "AllowAll");
        assert!(out.contains("Side effects: none"));
        assert!(out.contains("env[HOME]"));
    }
}
