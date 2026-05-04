//! Fixture document — parsed shape of `fixture.toml`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// The whole fixture declaration. Empty fields default cleanly so
/// authors only write the parts they care about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    /// Start-node name to invoke.
    pub start: String,
    /// Trigger payload + kind.
    #[serde(default)]
    pub trigger: FixtureTrigger,
    /// Canned mocks for external calls.
    #[serde(default)]
    pub mocks: FixtureMocks,
    /// Assertions to run after execution.
    #[serde(default)]
    pub expected: Expected,
    /// Optional deadline override. Defaults to 30s.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Run in dry-run mode?
    #[serde(default)]
    pub dry_run: bool,
}

impl Fixture {
    pub fn from_toml(src: &str) -> Result<Self> {
        toml::from_str(src).map_err(|e| Error::Config(format!("invalid fixture.toml: {e}")))
    }
}

/// Trigger payload shape. Mirrors the CLI's `--input-file` usage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FixtureTrigger {
    #[serde(default = "default_trigger_kind")]
    pub kind: TriggerKindSpec,
    #[serde(default = "default_payload")]
    pub payload: Value,
}

impl Default for FixtureTrigger {
    fn default() -> Self {
        Self {
            kind: default_trigger_kind(),
            payload: default_payload(),
        }
    }
}

fn default_trigger_kind() -> TriggerKindSpec {
    TriggerKindSpec::Manual
}

fn default_payload() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKindSpec {
    Manual,
    Http,
    Event,
}

/// Canned responses for side-effectful subsystems.
///
/// Each list is FIFO: the first call gets the first entry. An empty
/// list means "no calls expected" and the [`MockClient`]s error if
/// one arrives.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FixtureMocks {
    /// Response text each successive `llm_infer` call returns.
    #[serde(default)]
    pub intel: Vec<String>,
    /// MCP `tools/call` canned responses, keyed by tool name.
    #[serde(default)]
    pub mcp_tools: std::collections::HashMap<String, Vec<Value>>,
    /// MCP `resources/read` canned responses, keyed by URI.
    #[serde(default)]
    pub mcp_resources: std::collections::HashMap<String, Vec<Value>>,
}

/// Post-run assertions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    /// Required outcome flavour: `"completed"` | `"failed"` | `"timed_out"`.
    #[serde(default)]
    pub status: Option<String>,
    /// The node the workflow must end on.
    #[serde(default)]
    pub last_node: Option<String>,
    /// Reason string (Failed) — substring match.
    #[serde(default)]
    pub reason_contains: Option<String>,
    /// Ordered list of node ids the trace must visit. Subsequence
    /// or exact depending on [`path_exact`].
    #[serde(default)]
    pub path: Vec<String>,
    /// When `true` the recorded trace must equal `path` exactly;
    /// when `false` (default) it only needs to contain `path` as a
    /// contiguous sub-sequence at the start. Makes round-tripping
    /// fragile branches easier while still catching reordering.
    #[serde(default)]
    pub path_exact: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_viable_fixture_parses() {
        let src = r#"
            start = "main"
        "#;
        let f = Fixture::from_toml(src).unwrap();
        assert_eq!(f.start, "main");
        assert_eq!(f.trigger.kind, TriggerKindSpec::Manual);
        assert_eq!(f.trigger.payload, Value::Object(Default::default()));
        assert!(f.mocks.intel.is_empty());
        assert!(f.expected.path.is_empty());
    }

    #[test]
    fn full_fixture_parses() {
        let src = r#"
            start = "on_http"
            dry_run = false
            timeout_secs = 10

            [trigger]
            kind = "http"
            payload = { user = "Ada" }

            [mocks]
            intel = ["first response", "second"]

            [mocks.mcp_tools]
            say_hi = [{ content = [{ type = "text", text = "hi" }] }]

            [expected]
            status = "completed"
            last_node = "done"
            path = ["a", "b", "done"]
            path_exact = true
        "#;
        let f = Fixture::from_toml(src).unwrap();
        assert_eq!(f.start, "on_http");
        assert_eq!(f.trigger.kind, TriggerKindSpec::Http);
        assert_eq!(f.mocks.intel.len(), 2);
        assert!(f.mocks.mcp_tools.contains_key("say_hi"));
        assert_eq!(f.expected.status.as_deref(), Some("completed"));
        assert_eq!(f.expected.last_node.as_deref(), Some("done"));
        assert!(f.expected.path_exact);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let src = r#"
            start = "main"
            nope = 1
        "#;
        assert!(Fixture::from_toml(src).is_err());
    }
}
