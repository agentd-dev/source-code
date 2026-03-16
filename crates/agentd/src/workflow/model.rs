//! Workflow document model and TOML parser (RFC §9, §17.2).
//!
//! A [`WorkflowDoc`] is the parsed form of a workflow config. Each
//! `Node` carries an `id` plus a typed [`NodeKind`] that says what
//! the node does. Triggers, start nodes, and edges are modelled as
//! separate small records.
//!
//! TOML encoding follows the RFC example verbatim:
//!
//! ```toml
//! [[workflows.nodes]]
//! id = "load_resource"
//! type = "read_mcp_resource"
//! resource_from = "trigger.resource_uri"
//!
//! [[workflows.edges]]
//! from = "decision"
//! when = "comment"
//! to = "post_comment"
//! ```
//!
//! Only the variants that appear in the RFC's worked example (§17.2)
//! plus the five control-node kinds are implemented in Phase 1; more
//! variants land as their tool families are wired.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Top-level workflow document
// ---------------------------------------------------------------------------

/// A single workflow. Usually lives inside an agent config under
/// `[[workflows]]`; can also be parsed standalone with
/// [`WorkflowDoc::from_toml`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDoc {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,

    #[serde(default)]
    pub start_nodes: Vec<StartNode>,

    #[serde(default)]
    pub triggers: Vec<Trigger>,

    #[serde(default)]
    pub http_routes: Vec<HttpRoute>,

    #[serde(default)]
    pub nodes: Vec<Node>,

    #[serde(default)]
    pub edges: Vec<Edge>,
}

impl WorkflowDoc {
    /// Parse a workflow document from a TOML string.
    ///
    /// Accepts both the bare `WorkflowDoc` shape (fields at the top
    /// level) and the `[[workflows]]`-wrapped shape used by the agent
    /// config.
    pub fn from_toml(s: &str) -> Result<Self> {
        /// Helper wrapper to accept `[[workflows]]` at the root.
        #[derive(Deserialize)]
        struct Wrapped {
            workflows: Vec<WorkflowDoc>,
        }

        // Try the wrapped form first.
        if let Ok(Wrapped { mut workflows }) = toml::from_str::<Wrapped>(s) {
            if workflows.len() == 1 {
                return Ok(workflows.remove(0));
            }
            return Err(Error::Workflow {
                workflow: "<root>".into(),
                reason: format!(
                    "expected exactly one [[workflows]] entry; found {}",
                    workflows.len()
                ),
            });
        }

        // Fall back to the bare form.
        toml::from_str::<WorkflowDoc>(s).map_err(|e| Error::Config(e.to_string()))
    }

    /// Look up a node by id.
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Look up a start node by name.
    pub fn start_node(&self, name: &str) -> Option<&StartNode> {
        self.start_nodes.iter().find(|s| s.name == name)
    }
}

// ---------------------------------------------------------------------------
// Start nodes
// ---------------------------------------------------------------------------

/// A named DAG entry point. A workflow may declare several and the
/// same graph body can be reached from any of them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StartNode {
    pub name: String,
    pub source: StartSource,
    /// Optional node id the start-node lands on. Omitting it means
    /// "the start node *is* a node whose id matches `name`".
    #[serde(default)]
    pub entry_node: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartSource {
    Event,
    Http,
    Manual,
}

// ---------------------------------------------------------------------------
// Triggers
// ---------------------------------------------------------------------------

/// A trigger binds an external signal to a start node.
///
/// Internally tagged by `type` — the RFC's TOML examples use a dotted
/// form (`mcp.resource.updated`) which serde accepts verbatim as a
/// rename.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Trigger {
    #[serde(rename = "mcp.resource.updated")]
    McpResourceUpdated {
        server: String,
        resource: String,
        start_node: String,
    },
    #[serde(rename = "mcp.resource.created")]
    McpResourceCreated {
        server: String,
        resource: String,
        start_node: String,
    },
    #[serde(rename = "internal.event")]
    InternalEvent { name: String, start_node: String },
}

impl Trigger {
    /// The start-node name this trigger fires.
    pub fn start_node(&self) -> &str {
        match self {
            Trigger::McpResourceUpdated { start_node, .. }
            | Trigger::McpResourceCreated { start_node, .. }
            | Trigger::InternalEvent { start_node, .. } => start_node,
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP routes
// ---------------------------------------------------------------------------

/// An HTTP route — a structured description of the listener side.
/// The runtime does not mount a server unless the `trigger-http`
/// feature is enabled and an HTTP transport is configured.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HttpRoute {
    pub method: String,
    pub path: String,
    pub start_node: String,
    #[serde(default)]
    pub input_schema: Option<String>,
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

/// A typed DAG node. `id` is unique within the workflow.
///
/// `deny_unknown_fields` intentionally omitted here because
/// `#[serde(flatten)]` + an internally tagged enum would otherwise
/// make the `type` discriminator look unknown to the outer struct.
/// Strictness is still enforced at the variant level: each
/// [`NodeKind`] variant carries `deny_unknown_fields`, so unknown
/// keys inside a variant fail loudly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Node {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// Node-kind discriminator (RFC §9.4).
///
/// Only the variants that appear in the RFC example plus the five
/// control-node kinds are modelled in Phase 1; the set grows as each
/// tool family is wired. Adding a variant is additive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeKind {
    // --- Input / context ---
    ReadMcpResource {
        resource_from: String,
    },

    // --- Intelligence ---
    LlmInfer {
        backend: String,
        prompt: String,
        #[serde(default)]
        input_from: Option<String>,
        #[serde(default)]
        output_schema: Option<String>,
    },

    // --- Action ---
    CallMcpTool {
        tool: String,
        #[serde(default)]
        args_from: Option<String>,
    },

    // --- Control ---
    Condition {
        expr: String,
    },
    Switch {
        expr: String,
    },
    Merge,
    Fail {
        #[serde(default)]
        reason: Option<String>,
    },
    Terminate,
}

impl NodeKind {
    /// Human-readable name of the node kind (matches the `type`
    /// discriminator used in config files).
    pub fn name(&self) -> &'static str {
        match self {
            NodeKind::ReadMcpResource { .. } => "read_mcp_resource",
            NodeKind::LlmInfer { .. } => "llm_infer",
            NodeKind::CallMcpTool { .. } => "call_mcp_tool",
            NodeKind::Condition { .. } => "condition",
            NodeKind::Switch { .. } => "switch",
            NodeKind::Merge => "merge",
            NodeKind::Fail { .. } => "fail",
            NodeKind::Terminate => "terminate",
        }
    }

    /// Whether this node category is pure (no side effects) — useful
    /// for dry-run mode, which never calls impure node handlers.
    pub fn is_side_effect(&self) -> bool {
        matches!(self, NodeKind::CallMcpTool { .. })
    }
}

// ---------------------------------------------------------------------------
// Edges
// ---------------------------------------------------------------------------

/// Directed edge. `when` selects a branch on the source node's output
/// (e.g. a switch-node case label); `None` means unconditional.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub when: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_form_parses() {
        let doc = WorkflowDoc::from_toml("[[workflows]]\nname = \"x\"").unwrap();
        assert_eq!(doc.name, "x");
    }

    #[test]
    fn multiple_workflows_rejected_in_bare_parse() {
        // Two workflows under [[workflows]] — from_toml expects one.
        let toml = r#"
            [[workflows]]
            name = "a"

            [[workflows]]
            name = "b"
        "#;
        let err = WorkflowDoc::from_toml(toml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("exactly one"), "got: {msg}");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let toml = r#"
            name = "x"
            totally_unexpected = 42
        "#;
        assert!(WorkflowDoc::from_toml(toml).is_err());
    }

    #[test]
    fn bare_missing_name_rejected() {
        let err = WorkflowDoc::from_toml("").unwrap_err();
        assert!(format!("{err}").contains("missing field `name`"));
    }
}
