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

    /// Inline policy manifest (RFC §16). Omitting the block keeps
    /// the permissive `AllowAll` default; adding it switches to
    /// fail-closed allowlist enforcement.
    #[serde(default)]
    pub policy: Option<crate::policy::PolicyManifest>,

    /// Optional `[auth]` block defining bearer / HMAC bindings.
    /// Route-level `auth = "bearer:..."` / `"hmac:..."` refs
    /// resolve against this at startup.
    #[cfg(feature = "auth")]
    #[serde(default)]
    pub auth: Option<crate::auth::AuthConfig>,

    /// Optional `[logging]` block. Provides base logging config;
    /// CLI flags + `AGENTD_LOG_*` env vars override these fields.
    #[serde(default)]
    pub logging: Option<crate::observability::LoggingConfig>,

    /// Optional `[[mcp_servers]]` entries. Each spawns an MCP stdio
    /// child; `call_mcp_tool` / `read_mcp_resource` nodes route to
    /// them by name. Empty or absent leaves the process with no MCP
    /// plane — workflows that use MCP nodes will fail validation.
    /// Entries here compose with `--mcp-stdio` (the CLI path is kept
    /// for back-compat as the default-named server).
    #[serde(default, rename = "mcp_servers")]
    pub mcp_servers: Vec<crate::mcp::config::McpServerDef>,

    /// Optional `[server]` block. TLS + mTLS termination. The full
    /// rustls wiring is behind the `server-tls` Cargo feature;
    /// absence of the feature turns a present `[server.tls]` block
    /// into a clean startup error pointing at the rebuild path.
    #[serde(default)]
    pub server: Option<crate::server_config::ServerConfig>,

    /// Optional workflow-signing configuration (RFC 0002). Parsed
    /// unconditionally; verification is a no-op unless the `signing`
    /// Cargo feature is compiled in.
    #[serde(default)]
    pub signing: Option<crate::signing::SigningConfig>,

    /// Optional `[budget]` block. Process-wide resource
    /// caps — memory (RLIMIT_AS), CPU time (RLIMIT_CPU), wall-clock
    /// per run, and cumulative fs-write bytes. Scoped per-process
    /// because agent is a micro-agent (1 workflow / process).
    #[serde(default)]
    pub budget: Option<crate::budget::BudgetConfig>,
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
    /// Fire on a cron schedule. `schedule` is a 5-field cron
    /// expression (`m h dom mon dow`) in the runtime's local TZ —
    /// operators who need a specific TZ set `TZ=...` on the
    /// process. Feature-gated on `trigger-cron`.
    #[serde(rename = "cron")]
    Cron {
        schedule: String,
        start_node: String,
    },
    /// Fire on a fixed interval. `every` is a human duration
    /// ("30s", "5m", "1h"). Equivalent to a cron expression but
    /// cheaper to parse and more intuitive for "poll this every N"
    /// workflows. Feature-gated on `trigger-cron`.
    #[serde(rename = "interval")]
    Interval { every: String, start_node: String },
    /// Fire on a filesystem change under `path`. `events` is the
    /// filter list (`create`, `modify`, `remove`, `rename`);
    /// defaults to all four. `recursive` defaults to false.
    /// `debounce_ms` coalesces rapid events into one trigger fire
    /// (default 250ms). Feature-gated on `trigger-fs-watch`.
    #[serde(rename = "fs_watch")]
    FsWatch {
        path: std::path::PathBuf,
        start_node: String,
        #[serde(default)]
        recursive: bool,
        #[serde(default)]
        events: Vec<String>,
        #[serde(default = "default_debounce")]
        debounce_ms: u64,
    },
}

fn default_debounce() -> u64 {
    250
}

impl Trigger {
    /// The start-node name this trigger fires.
    pub fn start_node(&self) -> &str {
        match self {
            Trigger::McpResourceUpdated { start_node, .. }
            | Trigger::McpResourceCreated { start_node, .. }
            | Trigger::InternalEvent { start_node, .. }
            | Trigger::Cron { start_node, .. }
            | Trigger::Interval { start_node, .. }
            | Trigger::FsWatch { start_node, .. } => start_node,
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
    #[serde(default)]
    pub auth: Option<String>,
    /// Optional token-bucket limit. Denied requests return 429 with
    /// a `Retry-After` header.
    #[serde(default)]
    pub rate_limit: Option<crate::ratelimit::RateLimitConfig>,
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
    /// Optional retry policy applied when the node handler returns
    /// an error. Terminal outcomes (Fail / Terminate) are not
    /// retried; only dispatch-level errors are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// Node-level retry policy. Applied to handler-returned `Err` only;
/// `NodeOutcome::Fail` is a declared terminal state and never retried.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Total attempts (including the first). Must be ≥ 1.
    pub max_attempts: u32,
    /// Base backoff between attempts, in milliseconds. Attempt N
    /// waits `backoff_ms * N` (linear ramp).
    #[serde(default = "default_backoff_ms")]
    pub backoff_ms: u64,
    /// Which error categories are retryable. Default: any error.
    #[serde(default)]
    pub on: RetryOn,
    /// Randomised jitter — each retry's sleep is multiplied by a
    /// random factor in `[1 - jitter, 1 + jitter]`, capped to
    /// `[0.0, 0.5]`. Default 0.0 (deterministic). Recommended
    /// 0.2–0.3 for thundering-herd mitigation.
    #[serde(default)]
    pub jitter: f32,
}

fn default_backoff_ms() -> u64 {
    100
}

impl RetryPolicy {
    /// Effective jitter factor, clamped to `[0.0, 0.5]`. A value
    /// above 0.5 would let a retry sleep more than 1.5× the base
    /// backoff, which is past the point of diminishing returns
    /// for thundering-herd smoothing.
    pub fn clamped_jitter(&self) -> f32 {
        self.jitter.clamp(0.0, 0.5)
    }

    /// Compute the sleep before attempt `n` (1-indexed; `n=1` is
    /// the first retry, so the ramp starts at `backoff_ms * 1`).
    /// When `jitter > 0`, the result is multiplied by a
    /// `[1 - j, 1 + j]` random factor sourced from `rng_bits` —
    /// the engine provides a u64 (e.g. `rand::random::<u64>()`)
    /// so this function stays pure and deterministic under test.
    pub fn backoff_for(&self, n: u32, rng_bits: u64) -> std::time::Duration {
        let base = self.backoff_ms.saturating_mul(n.max(1) as u64);
        let j = self.clamped_jitter();
        if j == 0.0 {
            return std::time::Duration::from_millis(base);
        }
        // Derive a [-1.0, 1.0] value from the lower 32 bits of
        // rng_bits — cheap, avoids a float dep.
        let normalised = (rng_bits as u32) as f64 / u32::MAX as f64; // [0, 1]
        let signed = 2.0 * normalised - 1.0; // [-1, 1]
        let factor = 1.0 + (j as f64) * signed;
        let scaled = (base as f64 * factor).max(0.0);
        std::time::Duration::from_millis(scaled as u64)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetryOn {
    /// Any `Error` variant. Default — matches operator expectation
    /// of "retry on transient failure".
    #[default]
    Any,
    /// Only tool-dispatch errors (`Error::Tool { .. }`) and
    /// intelligence / MCP transport errors. Policy violations,
    /// schema-validation failures, and timeouts stay non-retryable.
    Transient,
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
    ReadFile {
        path_from: String,
    },
    ReadEnv {
        key: String,
    },
    ReadMcpResource {
        resource_from: String,
        /// Which configured MCP server to read from. Optional when
        /// exactly one server is registered; required when more than
        /// one exists (validator enforces). Matches the `name`
        /// in `[[mcp_servers]]`.
        #[serde(default)]
        server: Option<String>,
    },
    ParseJson {
        input_from: String,
    },

    // --- Transformation ---
    TemplateRender {
        template: String,
        #[serde(default)]
        input_from: Option<String>,
    },
    DiffCompute {
        left_from: String,
        right_from: String,
    },
    JsonSelect {
        input_from: String,
        path: String,
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
    WriteFile {
        path_from: String,
        content_from: String,
    },
    CreateDir {
        path_from: String,
    },
    HttpRequest {
        method: String,
        url_from: String,
        #[serde(default)]
        body_from: Option<String>,
    },
    CallMcpTool {
        tool: String,
        #[serde(default)]
        args_from: Option<String>,
        /// Which configured MCP server to route this call to.
        /// Optional when exactly one server is registered; required
        /// when more than one exists. Matches the `name` in
        /// `[[mcp_servers]]`.
        #[serde(default)]
        server: Option<String>,
    },
    /// Invoke a local binary with argv-style arguments. No shell
    /// interpolation, no PATH lookup — `command` is a literal path.
    /// The optional `args_from` resolves to a JSON array of strings.
    ShellRun {
        command: String,
        #[serde(default)]
        args_from: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
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
            NodeKind::ReadFile { .. } => "read_file",
            NodeKind::ReadEnv { .. } => "read_env",
            NodeKind::ReadMcpResource { .. } => "read_mcp_resource",
            NodeKind::ParseJson { .. } => "parse_json",
            NodeKind::TemplateRender { .. } => "template_render",
            NodeKind::DiffCompute { .. } => "diff_compute",
            NodeKind::JsonSelect { .. } => "json_select",
            NodeKind::LlmInfer { .. } => "llm_infer",
            NodeKind::WriteFile { .. } => "write_file",
            NodeKind::CreateDir { .. } => "create_dir",
            NodeKind::HttpRequest { .. } => "http_request",
            NodeKind::CallMcpTool { .. } => "call_mcp_tool",
            NodeKind::ShellRun { .. } => "shell_run",
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
        matches!(
            self,
            NodeKind::WriteFile { .. }
                | NodeKind::CreateDir { .. }
                | NodeKind::HttpRequest { .. }
                | NodeKind::CallMcpTool { .. }
                | NodeKind::ShellRun { .. }
        )
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

    /// The RFC §17.2 worked example, trimmed to the workflow body.
    const RFC_EXAMPLE: &str = r#"
        name = "document_review"

        [[start_nodes]]
        name = "on_resource_update"
        source = "event"

        [[start_nodes]]
        name = "on_http_request"
        source = "http"

        [[start_nodes]]
        name = "manual_review"
        source = "manual"

        [[triggers]]
        type = "mcp.resource.updated"
        server = "docs"
        resource = "docs://pages/*"
        start_node = "on_resource_update"

        [[http_routes]]
        method = "POST"
        path = "/workflows/document-review"
        start_node = "on_http_request"
        input_schema = "schemas/review_request.json"

        [[nodes]]
        id = "load_resource"
        type = "read_mcp_resource"
        resource_from = "trigger.resource_uri"

        [[nodes]]
        id = "analyze"
        type = "llm_infer"
        backend = "default"
        input_from = "load_resource"
        prompt = "Analyze the updated document."
        output_schema = "schemas/review_decision.json"

        [[nodes]]
        id = "decision"
        type = "switch"
        expr = "analyze.decision"

        [[nodes]]
        id = "post_comment"
        type = "call_mcp_tool"
        tool = "comment_on_page"
        args_from = "analyze.comment_payload"

        [[nodes]]
        id = "done"
        type = "terminate"

        [[edges]]
        from = "load_resource"
        to = "analyze"

        [[edges]]
        from = "analyze"
        to = "decision"

        [[edges]]
        from = "decision"
        when = "comment"
        to = "post_comment"

        [[edges]]
        from = "decision"
        when = "ignore"
        to = "done"

        [[edges]]
        from = "post_comment"
        to = "done"
    "#;

    #[test]
    fn parses_rfc_example() {
        let doc = WorkflowDoc::from_toml(RFC_EXAMPLE).unwrap();
        assert_eq!(doc.name, "document_review");
        assert_eq!(doc.start_nodes.len(), 3);
        assert_eq!(doc.triggers.len(), 1);
        assert_eq!(doc.http_routes.len(), 1);
        assert_eq!(doc.nodes.len(), 5);
        assert_eq!(doc.edges.len(), 5);
    }

    #[test]
    fn start_node_sources() {
        let doc = WorkflowDoc::from_toml(RFC_EXAMPLE).unwrap();
        let sources: Vec<_> = doc.start_nodes.iter().map(|s| s.source).collect();
        assert_eq!(
            sources,
            vec![StartSource::Event, StartSource::Http, StartSource::Manual]
        );
    }

    #[test]
    fn trigger_start_node_accessor() {
        let doc = WorkflowDoc::from_toml(RFC_EXAMPLE).unwrap();
        assert_eq!(doc.triggers[0].start_node(), "on_resource_update");
    }

    #[test]
    fn node_kinds_round_trip() {
        let doc = WorkflowDoc::from_toml(RFC_EXAMPLE).unwrap();
        let kinds: Vec<_> = doc.nodes.iter().map(|n| n.kind.name()).collect();
        assert_eq!(
            kinds,
            vec![
                "read_mcp_resource",
                "llm_infer",
                "switch",
                "call_mcp_tool",
                "terminate",
            ]
        );
    }

    #[test]
    fn side_effect_flag() {
        let doc = WorkflowDoc::from_toml(RFC_EXAMPLE).unwrap();
        let side_effects: Vec<_> = doc
            .nodes
            .iter()
            .filter(|n| n.kind.is_side_effect())
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(side_effects, vec!["post_comment"]);
    }

    #[test]
    fn edge_when_selectors() {
        let doc = WorkflowDoc::from_toml(RFC_EXAMPLE).unwrap();
        let whens: Vec<_> = doc.edges.iter().filter_map(|e| e.when.as_deref()).collect();
        assert_eq!(whens, vec!["comment", "ignore"]);
    }

    #[test]
    fn wrapped_form_parses() {
        let wrapped = format!("[[workflows]]\n{}", RFC_EXAMPLE);
        let doc = WorkflowDoc::from_toml(&wrapped).unwrap();
        assert_eq!(doc.name, "document_review");
    }

    #[test]
    fn node_lookup_helpers() {
        let doc = WorkflowDoc::from_toml(RFC_EXAMPLE).unwrap();
        assert!(doc.node("analyze").is_some());
        assert!(doc.node("no-such-id").is_none());
        assert!(doc.start_node("manual_review").is_some());
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

    // -----------------------------------------------------------------
    // RetryPolicy jitter
    // -----------------------------------------------------------------
    #[test]
    fn retry_backoff_linear_without_jitter() {
        let p = RetryPolicy {
            max_attempts: 3,
            backoff_ms: 100,
            on: RetryOn::Any,
            jitter: 0.0,
        };
        assert_eq!(p.backoff_for(1, 0).as_millis(), 100);
        assert_eq!(p.backoff_for(2, 0).as_millis(), 200);
        assert_eq!(p.backoff_for(3, 0).as_millis(), 300);
    }

    #[test]
    fn retry_jitter_clamps_to_half() {
        let p = RetryPolicy {
            max_attempts: 1,
            backoff_ms: 100,
            on: RetryOn::Any,
            jitter: 2.0, // would add ±200ms uncapped
        };
        assert_eq!(p.clamped_jitter(), 0.5);
        // With rng_bits = u64::MAX the normalised factor is +1.0;
        // factor = 1 + 0.5 * 1 = 1.5; 100 * 1.5 = 150ms.
        assert_eq!(p.backoff_for(1, u64::MAX).as_millis(), 150);
    }

    #[test]
    fn retry_jitter_symmetric_bounds() {
        let p = RetryPolicy {
            max_attempts: 1,
            backoff_ms: 1000,
            on: RetryOn::Any,
            jitter: 0.2,
        };
        // rng_bits = 0 → factor ≈ 0.8 → ~800ms. Allow ±1ms for f64
        // rounding (800 * 0.8 intermediate).
        let lo = p.backoff_for(1, 0).as_millis();
        assert!((799..=801).contains(&lo), "lo = {lo}");
        // rng_bits with lower-32 = u32::MAX/2 ≈ 0.5 → signed ~ 0 → 1000ms.
        let mid = (u32::MAX / 2) as u64;
        let ms = p.backoff_for(1, mid).as_millis();
        assert!((998..=1002).contains(&ms), "mid = {ms}");
        // rng_bits with lower-32 = u32::MAX → factor = 1.2 → 1200ms.
        let hi = p.backoff_for(1, u32::MAX as u64).as_millis();
        assert!((1198..=1200).contains(&hi), "hi = {hi}");
    }

    #[test]
    fn retry_jitter_zero_is_deterministic() {
        let p = RetryPolicy {
            max_attempts: 1,
            backoff_ms: 500,
            on: RetryOn::Any,
            jitter: 0.0,
        };
        // Every rng value produces the same result.
        assert_eq!(p.backoff_for(1, 0).as_millis(), 500);
        assert_eq!(p.backoff_for(1, u64::MAX).as_millis(), 500);
        assert_eq!(p.backoff_for(1, 12345).as_millis(), 500);
    }

    #[test]
    fn retry_attempt_floor_is_one() {
        let p = RetryPolicy {
            max_attempts: 1,
            backoff_ms: 100,
            on: RetryOn::Any,
            jitter: 0.0,
        };
        // n=0 is meaningless but shouldn't panic / underflow;
        // backoff_for clamps the attempt to ≥ 1.
        assert_eq!(p.backoff_for(0, 0).as_millis(), 100);
    }
}
