//! Instruction files (RFC 0006 §4) — the agent's standing identity.
//!
//! ```toml
//! [agent]
//! name = "log-auditor"
//! system = """You are a careful operations assistant."""
//! default_backend = "claude"
//! loop_tools = ["read_file", "json_select"]
//! ```
//!
//! Instructions are config, not code: diffable, signable alongside
//! workflows, and deliberately small. They feed the goal-mode
//! planner and any `agent_loop` node that doesn't override them.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstructionsDoc {
    #[serde(default)]
    pub agent: AgentInstructions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentInstructions {
    /// Operator-facing identity; lands in audit events.
    #[serde(default)]
    pub name: Option<String>,
    /// System prompt prepended to planner and loop conversations.
    #[serde(default)]
    pub system: Option<String>,
    /// Backend used when a node / goal doesn't name one.
    #[serde(default)]
    pub default_backend: Option<String>,
    /// Default tool subset for `agent_loop` nodes that omit `tools`.
    #[serde(default)]
    pub loop_tools: Vec<String>,
    /// Standing instruction — what the agent should accomplish. When
    /// present, starting with just `--instructions FILE` compiles a
    /// workflow for this task and runs it (RFC 0006 §3).
    #[serde(default)]
    pub task: Option<String>,
    /// Operator opt-in: a self-contained agent spec may run its
    /// compiled plan unattended without the interactive `--auto-approve`
    /// gate. Defaults false (fail-closed).
    #[serde(default)]
    pub auto_approve: bool,
}

impl AgentInstructions {
    pub fn load(path: &Path) -> Result<Self> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("read instructions {}: {e}", path.display())))?;
        let doc: InstructionsDoc = toml::from_str(&src)
            .map_err(|e| Error::Config(format!("parse instructions {}: {e}", path.display())))?;
        Ok(doc.agent)
    }

    pub fn effective_backend(&self) -> &str {
        self.default_backend.as_deref().unwrap_or("default")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_block() {
        let src = r#"
            [agent]
            name = "auditor"
            system = "be careful"
            default_backend = "claude"
            loop_tools = ["read_file"]
            task = "summarise the newest log file"
            auto_approve = true
        "#;
        let doc: InstructionsDoc = toml::from_str(src).unwrap();
        assert_eq!(doc.agent.name.as_deref(), Some("auditor"));
        assert_eq!(doc.agent.effective_backend(), "claude");
        assert_eq!(doc.agent.loop_tools, vec!["read_file"]);
        assert_eq!(
            doc.agent.task.as_deref(),
            Some("summarise the newest log file")
        );
        assert!(doc.agent.auto_approve);
    }

    #[test]
    fn empty_is_fine_defaults_apply() {
        let doc: InstructionsDoc = toml::from_str("").unwrap();
        assert_eq!(doc.agent.effective_backend(), "default");
    }

    #[test]
    fn unknown_fields_rejected() {
        assert!(toml::from_str::<InstructionsDoc>("[agent]\nrole = \"x\"").is_err());
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("agent.toml");
        std::fs::write(&p, "[agent]\nname = \"a\"\n").unwrap();
        assert_eq!(
            AgentInstructions::load(&p).unwrap().name.as_deref(),
            Some("a")
        );
    }
}
