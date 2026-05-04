//! Fixture runner — the thing that actually drives a fixture dir
//! against the engine and produces a structured pass/fail report.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::engine::{
    Engine, ExecutionOutcome, ExecutionTrace, HandlerRegistry, RunOptions, StubHandler, TriggerMeta,
};
use crate::error::{Error, Result};
use crate::intelligence::{MockClient, client::IntelligenceRef, handler as intel_handler};
use crate::mcp::allowlist::McpAllowlist;
use crate::mcp::client::McpClient;
use crate::mcp::handler as mcp_handler;
use crate::mcp::protocol::{ResourcesReadResult, ToolsCallResult};
use crate::testing::fixture::{Expected, Fixture, TriggerKindSpec};
use crate::tools::policy::allow_all;
use crate::workflow::{self, WorkflowDoc};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run a fixture directory. Panics if the directory is malformed.
/// Returns the structured [`FixtureResult`] so callers can assert.
pub fn run_fixture(dir: impl Into<PathBuf>) -> FixtureResult {
    FixtureRunner::from_dir(dir)
        .and_then(|r| r.run())
        .unwrap_or_else(|e| FixtureResult::load_error(e.to_string()))
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

pub struct FixtureRunner {
    pub dir: PathBuf,
    pub workflow: WorkflowDoc,
    pub fixture: Fixture,
}

impl FixtureRunner {
    pub fn from_dir(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        let wf_path = dir.join("workflow.toml");
        let fx_path = dir.join("fixture.toml");
        let wf_src = std::fs::read_to_string(&wf_path).map_err(|e| {
            Error::Config(format!(
                "fixture `{}`: read workflow.toml: {e}",
                dir.display()
            ))
        })?;
        let fx_src = std::fs::read_to_string(&fx_path).map_err(|e| {
            Error::Config(format!(
                "fixture `{}`: read fixture.toml: {e}",
                dir.display()
            ))
        })?;
        let workflow = WorkflowDoc::from_toml(&wf_src)?;
        let fixture = Fixture::from_toml(&fx_src)?;

        Ok(Self {
            dir,
            workflow,
            fixture,
        })
    }

    pub fn run(self) -> Result<FixtureResult> {
        let FixtureRunner {
            dir,
            workflow,
            fixture,
        } = self;

        // Validate the workflow — fixture authors benefit from
        // catching structural issues before the mock plumbing
        // muddies the failure.
        let report = workflow::validate(&workflow);
        if !report.ok() {
            return Ok(FixtureResult {
                dir,
                status: FixtureStatus::LoadError,
                failures: report
                    .issues
                    .iter()
                    .map(|i| format!("[{}] {}", i.code, i.message))
                    .collect(),
                outcome: None,
                trace: None,
            });
        }

        // Build the engine. Control handlers + every default tool
        // family, AllowAll for the Phase-3 policy. Intelligence +
        // MCP are wired only if the fixture provided mocks for them.
        let mut registry = HandlerRegistry::with_builtin_controls();
        crate::tools::register_default_tools(&mut registry, allow_all());

        if !fixture.mocks.intel.is_empty() {
            let client = Arc::new(MockClient::new());
            for text in &fixture.mocks.intel {
                client.enqueue_text(text.clone());
            }
            let arc: IntelligenceRef = client;
            intel_handler::register(&mut registry, arc);
        }

        if !fixture.mocks.mcp_tools.is_empty() || !fixture.mocks.mcp_resources.is_empty() {
            let mock = FixtureMcpClient::from_mocks(&fixture);
            let client: Box<dyn crate::mcp::client::McpClient> = Box::new(mock);
            let handle = Arc::new(crate::mcp::McpServerHandle {
                name: "default".into(),
                client: Arc::new(crate::mcp::client::ReloadableMcpClient::new(client)),
                allowlist: Arc::new(crate::mcp::allowlist::ReloadableMcpAllowlist::new(
                    McpAllowlist::allow_all(),
                )),
            });
            let mcp = Arc::new(crate::mcp::McpRegistry::new(vec![handle]));
            mcp_handler::register(&mut registry, mcp);
        }

        registry.set_fallback(Box::new(StubHandler));

        let engine = Engine::new(registry);
        let options = RunOptions {
            timeout: Duration::from_secs(fixture.timeout_secs.unwrap_or(30).max(1)),
            dry_run: fixture.dry_run,
        };
        let trigger = build_trigger(&fixture);

        let (outcome, trace) =
            engine.run_with_trace(&workflow, &fixture.start, trigger, options)?;

        let failures = diff_expected(&fixture.expected, &outcome, &trace);
        let status = if failures.is_empty() {
            FixtureStatus::Pass
        } else {
            FixtureStatus::Fail
        };
        Ok(FixtureResult {
            dir,
            status,
            failures,
            outcome: Some(outcome),
            trace: Some(trace),
        })
    }
}

fn build_trigger(fixture: &Fixture) -> TriggerMeta {
    match fixture.trigger.kind {
        TriggerKindSpec::Manual => TriggerMeta::manual(fixture.trigger.payload.clone()),
        TriggerKindSpec::Http => TriggerMeta::http(fixture.trigger.payload.clone()),
        TriggerKindSpec::Event => TriggerMeta::event(fixture.trigger.payload.clone()),
    }
}

// ---------------------------------------------------------------------------
// Result reporting
// ---------------------------------------------------------------------------

/// Outcome of a single fixture run.
#[derive(Debug)]
pub struct FixtureResult {
    pub dir: PathBuf,
    pub status: FixtureStatus,
    pub failures: Vec<String>,
    pub outcome: Option<ExecutionOutcome>,
    pub trace: Option<ExecutionTrace>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureStatus {
    Pass,
    Fail,
    LoadError,
}

impl FixtureResult {
    /// Convenience for `#[test]` use: assert a Pass and render the
    /// failure list if not.
    pub fn assert_pass(&self) {
        if self.status != FixtureStatus::Pass {
            panic!(
                "fixture `{}` failed ({:?}):\n  {}",
                self.dir.display(),
                self.status,
                self.failures.join("\n  ")
            );
        }
    }

    fn load_error(msg: String) -> Self {
        Self {
            dir: PathBuf::new(),
            status: FixtureStatus::LoadError,
            failures: vec![msg],
            outcome: None,
            trace: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Diffing
// ---------------------------------------------------------------------------

fn diff_expected(
    expected: &Expected,
    outcome: &ExecutionOutcome,
    trace: &ExecutionTrace,
) -> Vec<String> {
    let mut out = Vec::new();

    // `status` check
    if let Some(want) = &expected.status {
        let got = match outcome {
            ExecutionOutcome::Completed { .. } => "completed",
            ExecutionOutcome::Failed { .. } => "failed",
            ExecutionOutcome::TimedOut { .. } => "timed_out",
        };
        if want != got {
            out.push(format!("status mismatch: expected `{want}`, got `{got}`"));
        }
    }

    // `last_node` check
    if let Some(want) = &expected.last_node {
        let got = match outcome {
            ExecutionOutcome::Completed { last_node, .. } => last_node.clone(),
            ExecutionOutcome::Failed { last_node, .. } => last_node.clone(),
            ExecutionOutcome::TimedOut { last_node, .. } => last_node.clone(),
        };
        if got.as_deref() != Some(want.as_str()) {
            out.push(format!(
                "last_node mismatch: expected `{want}`, got `{:?}`",
                got
            ));
        }
    }

    // `reason_contains` check (Failed-only)
    if let Some(needle) = &expected.reason_contains {
        let reason = match outcome {
            ExecutionOutcome::Failed { reason, .. } => Some(reason.as_str()),
            _ => None,
        };
        match reason {
            Some(r) if r.contains(needle) => {}
            Some(r) => out.push(format!("reason_contains `{needle}` not found in `{r}`")),
            None => out.push("reason_contains set but outcome is not Failed".to_string()),
        }
    }

    // `path` check
    if !expected.path.is_empty() {
        let got_ids = trace.node_ids();
        if expected.path_exact {
            if got_ids != expected.path {
                out.push(format!(
                    "path exact mismatch:\n    expected: {:?}\n    got:      {:?}",
                    expected.path, got_ids
                ));
            }
        } else {
            // Treat `path` as a prefix the trace must match.
            let prefix_len = expected.path.len();
            if got_ids.len() < prefix_len || got_ids[..prefix_len] != expected.path[..] {
                out.push(format!(
                    "path prefix mismatch:\n    expected prefix: {:?}\n    got:             {:?}",
                    expected.path, got_ids
                ));
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Fixture-backed MCP client
// ---------------------------------------------------------------------------

/// MCP mock that pulls responses from the fixture's FIFO queues.
struct FixtureMcpClient {
    tools: std::sync::Mutex<std::collections::HashMap<String, std::collections::VecDeque<Value>>>,
    resources:
        std::sync::Mutex<std::collections::HashMap<String, std::collections::VecDeque<Value>>>,
}

impl FixtureMcpClient {
    fn from_mocks(fixture: &Fixture) -> Self {
        let mut tools = std::collections::HashMap::new();
        for (name, vals) in &fixture.mocks.mcp_tools {
            tools.insert(name.clone(), vals.iter().cloned().collect());
        }
        let mut resources = std::collections::HashMap::new();
        for (uri, vals) in &fixture.mocks.mcp_resources {
            resources.insert(uri.clone(), vals.iter().cloned().collect());
        }
        Self {
            tools: std::sync::Mutex::new(tools),
            resources: std::sync::Mutex::new(resources),
        }
    }
}

impl McpClient for FixtureMcpClient {
    fn call_tool(&self, name: &str, _arguments: Value) -> Result<ToolsCallResult> {
        let mut guard = self.tools.lock().unwrap();
        let queue = guard
            .get_mut(name)
            .ok_or_else(|| Error::Mcp(format!("fixture has no mcp_tools.{name} responses")))?;
        let raw = queue
            .pop_front()
            .ok_or_else(|| Error::Mcp(format!("fixture exhausted mcp_tools.{name} queue")))?;
        let parsed: ToolsCallResult = serde_json::from_value(raw)?;
        Ok(parsed)
    }

    fn read_resource(&self, uri: &str) -> Result<ResourcesReadResult> {
        let mut guard = self.resources.lock().unwrap();
        let queue = guard.get_mut(uri).ok_or_else(|| {
            Error::Mcp(format!("fixture has no mcp_resources[`{uri}`] responses"))
        })?;
        let raw = queue
            .pop_front()
            .ok_or_else(|| Error::Mcp(format!("fixture exhausted mcp_resources[`{uri}`] queue")))?;
        let parsed: ResourcesReadResult = serde_json::from_value(raw)?;
        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// Auto-discovery helper
// ---------------------------------------------------------------------------

/// Walk `root` for subdirectories that contain both `workflow.toml`
/// and `fixture.toml`. Consumers call this from a single `#[test]`
/// that forwards to `run_fixture` for every discovered directory.
pub fn discover_fixtures(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("workflow.toml").is_file() && path.join("fixture.toml").is_file() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fixture(workflow: &str, fixture: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("workflow.toml"), workflow).unwrap();
        std::fs::write(tmp.path().join("fixture.toml"), fixture).unwrap();
        let path = tmp.path().to_path_buf();
        (tmp, path)
    }

    const LINEAR_WF: &str = r#"
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
    "#;

    #[test]
    fn linear_workflow_passes() {
        let (_g, dir) = write_fixture(
            LINEAR_WF,
            r#"
                start = "main"

                [expected]
                status = "completed"
                last_node = "b"
                path = ["a", "b"]
                path_exact = true
            "#,
        );
        let r = run_fixture(dir);
        r.assert_pass();
        assert_eq!(r.trace.as_ref().unwrap().node_ids(), vec!["a", "b"]);
    }

    #[test]
    fn status_mismatch_is_flagged() {
        let (_g, dir) = write_fixture(
            LINEAR_WF,
            r#"
                start = "main"

                [expected]
                status = "failed"
            "#,
        );
        let r = run_fixture(dir);
        assert_eq!(r.status, FixtureStatus::Fail);
        assert!(
            r.failures.iter().any(|f| f.contains("status mismatch")),
            "failures: {:?}",
            r.failures
        );
    }

    #[test]
    fn last_node_mismatch_is_flagged() {
        let (_g, dir) = write_fixture(
            LINEAR_WF,
            r#"
                start = "main"

                [expected]
                last_node = "nope"
            "#,
        );
        let r = run_fixture(dir);
        assert_eq!(r.status, FixtureStatus::Fail);
        assert!(r.failures.iter().any(|f| f.contains("last_node mismatch")));
    }

    #[test]
    fn path_prefix_allows_additional_trailing_nodes() {
        let (_g, dir) = write_fixture(
            LINEAR_WF,
            r#"
                start = "main"

                [expected]
                path = ["a"]
            "#,
        );
        let r = run_fixture(dir);
        r.assert_pass();
    }

    #[test]
    fn failed_workflow_with_reason_contains() {
        let wf = r#"
            name = "fail"

            [[start_nodes]]
            name = "main"
            source = "manual"
            entry_node = "f"

            [[nodes]]
            id = "f"
            type = "fail"
            reason = "boom-boom-boom"
        "#;
        let (_g, dir) = write_fixture(
            wf,
            r#"
                start = "main"

                [expected]
                status = "failed"
                reason_contains = "boom"
                last_node = "f"
            "#,
        );
        let r = run_fixture(dir);
        r.assert_pass();
    }

    #[test]
    fn invalid_workflow_produces_load_error() {
        let wf = r#"
            name = "bad"

            [[nodes]]
            id = "x"
            type = "merge"

            [[nodes]]
            id = "x"
            type = "merge"
        "#;
        let (_g, dir) = write_fixture(wf, r#"start = "main""#);
        let r = run_fixture(dir);
        assert_eq!(r.status, FixtureStatus::LoadError);
        assert!(r.failures.iter().any(|f| f.contains("dup_node_id")));
    }

    #[test]
    fn intel_mocks_feed_llm_infer_nodes() {
        let wf = r#"
            name = "with_intel"

            [[start_nodes]]
            name = "main"
            source = "manual"
            entry_node = "ask"

            [[nodes]]
            id = "ask"
            type = "llm_infer"
            backend = "default"
            prompt = "say something"

            [[nodes]]
            id = "done"
            type = "terminate"

            [[edges]]
            from = "ask"
            to = "done"
        "#;
        let (_g, dir) = write_fixture(
            wf,
            r#"
                start = "main"

                [mocks]
                intel = ["mocked reply"]

                [expected]
                status = "completed"
                last_node = "done"
                path = ["ask", "done"]
                path_exact = true
            "#,
        );
        let r = run_fixture(dir);
        r.assert_pass();
    }

    #[test]
    fn discover_fixtures_picks_up_valid_dirs_only() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Good.
        let good = root.join("good");
        std::fs::create_dir(&good).unwrap();
        std::fs::write(good.join("workflow.toml"), LINEAR_WF).unwrap();
        std::fs::write(good.join("fixture.toml"), "start = \"main\"").unwrap();
        // Missing fixture.toml.
        let bad = root.join("no_fixture");
        std::fs::create_dir(&bad).unwrap();
        std::fs::write(bad.join("workflow.toml"), LINEAR_WF).unwrap();
        // A plain file, not a dir.
        std::fs::write(root.join("README"), "hi").unwrap();

        let found = discover_fixtures(root).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("good"));
    }
}
