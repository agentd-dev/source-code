//! Execution context — per-run state threaded through every node.
//!
//! The context carries trigger metadata, the growing map of node
//! outputs, a small execution-local state bag, the deadline, and a
//! dry-run flag. Node handlers read and write this struct; the
//! engine owns it for the duration of one workflow run.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Per-run execution context. Handlers mutate `node_outputs` and
/// `state`; the engine mutates `current_node_id`.
#[derive(Debug)]
pub struct ExecutionContext {
    pub execution_id: String,
    pub workflow_id: String,
    pub start_node: String,
    pub trigger: TriggerMeta,
    /// Produced values keyed by node id. Also pre-populated with a
    /// reserved `"trigger"` entry carrying `trigger.to_value()` so
    /// input-node expressions like `trigger.resource_uri` resolve.
    pub node_outputs: HashMap<String, Value>,
    /// Small, opt-in execution-local key/value state (RFC §18.1).
    pub state: HashMap<String, Value>,
    /// Absolute deadline; `Engine::run` checks it before each node.
    pub deadline: Instant,
    /// If `true`, side-effect node handlers must skip their side
    /// effect and return a placeholder value (RFC §22.2).
    pub dry_run: bool,
    /// Most recently entered node id, for diagnostics.
    pub current_node_id: Option<String>,
    /// W3C trace-context the run was kicked off with (inbound
    /// `traceparent`), if any. Populated by the HTTP trigger when
    /// the request carried a valid header; absent for cron /
    /// fs_watch / manual runs. Propagated verbatim to outbound
    /// tool calls (`http_request`) as the parent trace.
    pub trace_context: Option<crate::observability::TraceParent>,
    /// Fresh 8-byte span id that represents THIS workflow run in
    /// W3C traceparent propagation. Set at construction so every
    /// outbound call uses the same id — downstream services see a
    /// single "agent span" per run, not a sequence of
    /// disconnected IDs. Only meaningful when `trace_context` is
    /// `Some` (no inbound trace → nothing to propagate).
    pub outbound_span_id: String,
}

impl ExecutionContext {
    pub fn new(
        execution_id: impl Into<String>,
        workflow_id: impl Into<String>,
        start_node: impl Into<String>,
        trigger: TriggerMeta,
        options: &RunOptions,
    ) -> Self {
        let trace_context = trigger.trace_context.clone();
        let mut node_outputs = HashMap::new();
        node_outputs.insert("trigger".to_string(), trigger.to_value());
        Self {
            execution_id: execution_id.into(),
            workflow_id: workflow_id.into(),
            start_node: start_node.into(),
            trigger,
            node_outputs,
            state: HashMap::new(),
            deadline: Instant::now() + options.timeout,
            dry_run: options.dry_run,
            current_node_id: None,
            trace_context,
            outbound_span_id: crate::observability::fresh_span_id(),
        }
    }

    /// Render the outbound `traceparent` header value, if an
    /// inbound context was captured. Returns `None` for runs that
    /// weren't started by an HTTP request (cron, fs_watch, manual)
    /// — those don't originate traces.
    pub fn outbound_traceparent(&self) -> Option<String> {
        self.trace_context
            .as_ref()
            .map(|tp| tp.with_parent_id(&self.outbound_span_id).format())
    }

    /// Resolve a dotted path against `node_outputs`, e.g.
    /// `"trigger.resource_uri"` → the nested JSON value.
    ///
    /// The first segment is the node id (or the reserved `"trigger"`
    /// pseudo-node). Subsequent segments walk through JSON objects.
    /// Returns `None` if any segment is missing or a non-object is
    /// indexed.
    pub fn resolve_path<'a>(&'a self, path: &str) -> Option<&'a Value> {
        let mut parts = path.split('.');
        let head = parts.next()?;
        let mut cursor = self.node_outputs.get(head)?;
        for segment in parts {
            cursor = cursor.as_object()?.get(segment)?;
        }
        Some(cursor)
    }
}

/// Trigger metadata — what caused this workflow to start.
#[derive(Debug, Clone)]
pub struct TriggerMeta {
    pub kind: TriggerKind,
    /// The operator-facing input JSON (HTTP body, manual invoke
    /// payload, or MCP resource URI bundle).
    pub input: Value,
    /// Inbound W3C `traceparent`. Only set by
    /// the HTTP trigger when the request carried a valid header;
    /// propagated verbatim to outbound `http_request` calls so the
    /// trace chain continues across agent → downstream hops.
    pub trace_context: Option<crate::observability::TraceParent>,
}

impl TriggerMeta {
    pub fn manual(input: Value) -> Self {
        Self {
            kind: TriggerKind::Manual,
            input,
            trace_context: None,
        }
    }

    pub fn http(input: Value) -> Self {
        Self {
            kind: TriggerKind::Http,
            input,
            trace_context: None,
        }
    }

    /// HTTP trigger with an inbound W3C `traceparent`. Used by
    /// `triggers::http` when the request had a valid header — the
    /// agent continues that trace across any outbound tool calls.
    pub fn http_with_trace(input: Value, trace_context: crate::observability::TraceParent) -> Self {
        Self {
            kind: TriggerKind::Http,
            input,
            trace_context: Some(trace_context),
        }
    }

    pub fn event(input: Value) -> Self {
        Self {
            kind: TriggerKind::Event,
            input,
            trace_context: None,
        }
    }

    /// Flatten into a JSON value usable from dotted paths. Top-level
    /// `kind` is always present; `input` fields are merged at the
    /// root so `trigger.resource_uri` resolves when `input` is
    /// `{"resource_uri": "…"}`.
    fn to_value(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("kind".into(), json!(self.kind.as_str()));
        if let Some(fields) = self.input.as_object() {
            for (k, v) in fields {
                obj.insert(k.clone(), v.clone());
            }
        } else {
            obj.insert("input".into(), self.input.clone());
        }
        Value::Object(obj)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Manual,
    Http,
    Event,
}

impl TriggerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerKind::Manual => "manual",
            TriggerKind::Http => "http",
            TriggerKind::Event => "event",
        }
    }
}

impl TriggerMeta {
    /// Rebuild a trigger from a checkpoint's persisted kind + input
    /// (the ephemeral trace context is not carried across a resume).
    pub fn from_kind(kind: TriggerKind, input: Value) -> Self {
        Self {
            kind,
            input,
            trace_context: None,
        }
    }
}

/// Per-run tunables. Deadline, dry-run flag, and (eventually) retry
/// defaults live here so the engine surface stays small.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    pub dry_run: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            dry_run: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_ctx() -> ExecutionContext {
        let input = json!({ "resource_uri": "docs://pages/42", "author": "me" });
        let trigger = TriggerMeta::event(input);
        ExecutionContext::new("exec-1", "wf", "on_event", trigger, &RunOptions::default())
    }

    #[test]
    fn trigger_fields_are_reachable_via_path() {
        let ctx = mk_ctx();
        assert_eq!(
            ctx.resolve_path("trigger.resource_uri"),
            Some(&json!("docs://pages/42"))
        );
        assert_eq!(ctx.resolve_path("trigger.author"), Some(&json!("me")));
        assert_eq!(ctx.resolve_path("trigger.kind"), Some(&json!("event")));
    }

    #[test]
    fn missing_path_is_none() {
        let ctx = mk_ctx();
        assert!(ctx.resolve_path("trigger.nope").is_none());
        assert!(ctx.resolve_path("nope.whatever").is_none());
    }

    #[test]
    fn node_outputs_are_reachable() {
        let mut ctx = mk_ctx();
        ctx.node_outputs
            .insert("analyze".into(), json!({ "decision": "comment" }));
        assert_eq!(
            ctx.resolve_path("analyze.decision"),
            Some(&json!("comment"))
        );
    }

    #[test]
    fn non_object_input_wraps_as_input_field() {
        let ctx = ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(json!(42)),
            &RunOptions::default(),
        );
        assert_eq!(ctx.resolve_path("trigger.input"), Some(&json!(42)));
    }
}
