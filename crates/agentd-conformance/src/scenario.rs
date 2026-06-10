//! The conformance scenario model.
//!
//! A scenario is a self-contained, declarative test of the agentd
//! runtime: a workflow (inline or by path), a trigger, the canned
//! intelligence responses its `llm_infer` nodes should see, an
//! optional policy to enforce, and the expected outcome / trace /
//! cost. Scenarios are tagged against a capability matrix so a corpus
//! doubles as coverage tracking (goal tracking).
//!
//! Intelligence responses are modelled as ordered **turns** — one per
//! successive `llm_infer` call — each offering one or more **variants**.
//! A reliability run ([`crate::reliability`]) seeds a different variant
//! selection per trial to simulate model nondeterminism; a bounded
//! workflow that validates its inputs holds pass^k = 1.0 where a
//! fragile one decays.

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use agentd::policy::PolicyManifest;
use agentd::workflow::WorkflowDoc;

/// One conformance scenario, parsed from a `*.toml` file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Capability tags this scenario exercises. Drives the coverage
    /// matrix; see [`crate::capability`].
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Reliability trial count (pass^k). Defaults to 1.
    #[serde(default = "default_trials")]
    pub trials: u32,
    /// Per-run wall-clock deadline. Defaults to 30s.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// The workflow under test.
    pub workflow: WorkflowSrc,
    #[serde(default)]
    pub trigger: TriggerSpec,
    /// Canned intelligence responses, one entry per `llm_infer` call.
    #[serde(default)]
    pub intel: IntelSpec,
    /// Optional policy to compile and enforce. When present, tool
    /// calls outside it are denied — the basis of security scenarios.
    /// When absent, the workflow's own `[policy]` applies, else
    /// AllowAll.
    #[serde(default)]
    pub policy: Option<PolicyManifest>,
    #[serde(default)]
    pub expected: Expected,
    /// Not part of the file format — populated by [`Scenario::load`] so
    /// a `workflow.path` reference resolves relative to the scenario.
    #[serde(skip)]
    pub base_dir: Option<std::path::PathBuf>,
}

fn default_trials() -> u32 {
    1
}
fn default_timeout() -> u64 {
    30
}

impl Scenario {
    /// Parse a scenario from TOML source.
    pub fn from_toml(src: &str) -> Result<Self, String> {
        toml::from_str(src).map_err(|e| format!("parse scenario: {e}"))
    }

    /// Load a scenario file. `path` is also used to resolve a
    /// `workflow.path` reference relative to the scenario's directory.
    pub fn load(path: &Path) -> Result<Self, String> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("read scenario {}: {e}", path.display()))?;
        let mut scenario = Self::from_toml(&src)?;
        scenario.base_dir = path.parent().map(Path::to_path_buf);
        Ok(scenario)
    }

    /// Resolve the workflow source into a validated-shape document.
    /// (Structural validation happens in the harness so a malformed
    /// workflow is a scenario failure, not a panic.)
    pub fn workflow_doc(&self) -> Result<WorkflowDoc, String> {
        let toml_src = match (&self.workflow.inline, &self.workflow.path) {
            (Some(inline), _) => inline.clone(),
            (None, Some(rel)) => {
                let p = match &self.base_dir {
                    Some(dir) => dir.join(rel),
                    None => Path::new(rel).to_path_buf(),
                };
                std::fs::read_to_string(&p)
                    .map_err(|e| format!("read workflow {}: {e}", p.display()))?
            }
            (None, None) => {
                return Err("scenario.workflow needs `inline` or `path`".to_string());
            }
        };
        WorkflowDoc::from_toml(&toml_src).map_err(|e| format!("parse workflow: {e}"))
    }

    /// The start-node name the engine should enter at: the trigger's
    /// explicit `start`, else the workflow's first declared start node.
    pub fn start_name(&self, doc: &WorkflowDoc) -> Result<String, String> {
        if let Some(s) = &self.trigger.start {
            return Ok(s.clone());
        }
        doc.start_nodes
            .first()
            .map(|s| s.name.clone())
            .ok_or_else(|| "workflow declares no start nodes".to_string())
    }
}

/// Where the workflow comes from — inline TOML or a sibling file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSrc {
    #[serde(default)]
    pub inline: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

/// How the workflow is triggered.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TriggerSpec {
    #[serde(default)]
    pub kind: TriggerKind,
    /// Start-node name; defaults to the workflow's first start node.
    #[serde(default)]
    pub start: Option<String>,
    /// The payload bound to the reserved `trigger` context entry.
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    #[default]
    Manual,
    Http,
    Event,
}

/// Canned `llm_infer` responses, in call order.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct IntelSpec {
    #[serde(default)]
    pub turns: Vec<Turn>,
}

/// One `llm_infer` call's response set. A single variant is
/// deterministic; multiple variants drive pass^k seeded selection.
///
/// Shorthand: a turn may set `content` directly instead of a
/// one-element `variants` list.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub variants: Vec<ResponseSpec>,
}

impl Turn {
    /// The variants for this turn, normalising the `content` shorthand
    /// into a single-element list.
    pub fn variants(&self) -> Vec<ResponseSpec> {
        if !self.variants.is_empty() {
            self.variants.clone()
        } else {
            vec![ResponseSpec {
                content: self.content.clone().unwrap_or_default(),
                prompt_tokens: self.prompt_tokens,
                completion_tokens: self.completion_tokens,
            }]
        }
    }
}

/// One candidate response, with declared token usage for cost
/// reporting (the mock backend reports exactly these to the engine).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseSpec {
    pub content: String,
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

/// What the run is expected to produce. Every field is optional; only
/// the ones set are asserted.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    /// `completed` | `failed` | `timed_out`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub last_node: Option<String>,
    /// Substring the `failed` reason must contain.
    #[serde(default)]
    pub reason_contains: Option<String>,
    /// Node id path. By default a prefix; set `path_exact` for equality.
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub path_exact: bool,
    /// Cost ceilings (cost-per-success scenarios).
    #[serde(default)]
    pub max_llm_calls: Option<u64>,
    #[serde(default)]
    pub max_total_tokens: Option<u64>,
    /// Minimum number of policy denials the run must record (security
    /// scenarios that assert a tool call was blocked).
    #[serde(default)]
    pub min_policy_denials: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR: &str = r#"
        name = "linear"
        capabilities = ["merge", "terminate"]

        [workflow]
        inline = """
        name = "linear"
        [[start_nodes]]
        name = "main"
        source = "manual"
        entry_node = "a"
        [[nodes]]
        id = "a"
        type = "merge"
        [[nodes]]
        id = "b"
        type = "terminate"
        [[edges]]
        from = "a"
        to = "b"
        """

        [expected]
        status = "completed"
        last_node = "b"
        path = ["a", "b"]
        path_exact = true
    "#;

    #[test]
    fn parses_and_resolves_workflow() {
        let s = Scenario::from_toml(LINEAR).unwrap();
        assert_eq!(s.name, "linear");
        assert_eq!(s.trials, 1);
        assert_eq!(s.timeout_secs, 30);
        let doc = s.workflow_doc().unwrap();
        assert_eq!(doc.name, "linear");
        assert_eq!(s.start_name(&doc).unwrap(), "main");
    }

    #[test]
    fn turn_content_shorthand_becomes_one_variant() {
        let t = Turn {
            content: Some("hi".into()),
            prompt_tokens: 3,
            completion_tokens: 1,
            variants: vec![],
        };
        let v = t.variants();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].content, "hi");
        assert_eq!(v[0].prompt_tokens, 3);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err =
            Scenario::from_toml("name = \"x\"\nbogus = 1\n[workflow]\ninline = \"\"").unwrap_err();
        assert!(err.contains("parse scenario"), "{err}");
    }
}
