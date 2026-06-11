//! Node handlers — pluggable executors per [`NodeKind`].
//!
//! Each tool family (fs / env / data / intelligence / MCP) will land
//! its own handler and register it with the [`HandlerRegistry`]. The
//! Phase 2 handlers cover only the control-node family (Condition,
//! Switch, Merge, Fail, Terminate) plus a `StubHandler` that lets
//! engine tests walk through non-control nodes without requiring the
//! real tool implementations.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::engine::context::ExecutionContext;
use crate::engine::outcome::NodeOutcome;
use crate::error::{Error, Result};
use crate::workflow::{Node, NodeKind};

/// One node handler. Implementations map a single [`NodeKind`]
/// variant to a [`NodeOutcome`].
pub trait NodeHandler: Send + Sync {
    /// Execute the node. Returning `Err` aborts the run (unless a
    /// future tier wires up declared error edges to catch it).
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome>;
}

/// Registry that dispatches a [`Node`] to the right [`NodeHandler`]
/// based on `NodeKind::name()`.
///
/// The registry owns each handler as a trait object. Adding a new
/// tool family is a one-line registration:
///
/// ```ignore
/// registry.register("read_file", Box::new(ReadFileHandler::new(policy)));
/// ```
pub struct HandlerRegistry {
    handlers: HashMap<&'static str, Box<dyn NodeHandler>>,
    fallback: Option<Box<dyn NodeHandler>>,
}

impl HandlerRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            fallback: None,
        }
    }

    /// Registry pre-populated with the five control-node handlers.
    /// Tool-family registration happens on top of this base.
    pub fn with_builtin_controls() -> Self {
        let mut r = Self::new();
        r.register("condition", Box::new(ConditionHandler));
        r.register("switch", Box::new(SwitchHandler));
        r.register("merge", Box::new(MergeHandler));
        r.register("fail", Box::new(FailHandler));
        r.register("terminate", Box::new(TerminateHandler));
        r.register("respond", Box::new(RespondHandler));
        r
    }

    pub fn register(&mut self, kind: &'static str, handler: Box<dyn NodeHandler>) {
        self.handlers.insert(kind, handler);
    }

    /// Install a handler that catches every node kind without a
    /// dedicated registration. Used in tests so engine traversal
    /// works even when tool families are not wired.
    pub fn set_fallback(&mut self, handler: Box<dyn NodeHandler>) {
        self.fallback = Some(handler);
    }

    pub fn dispatch(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let kind = node.kind.name();
        if let Some(h) = self.handlers.get(kind) {
            return h.handle(node, ctx);
        }
        if let Some(h) = &self.fallback {
            return h.handle(node, ctx);
        }
        Err(Error::CapabilityUnavailable(format!(
            "no handler registered for node kind `{kind}` (node `{}`); \
             link the corresponding tool family at build time or register \
             a handler at startup",
            node.id
        )))
    }
}

impl Default for HandlerRegistry {
    fn default() -> Self {
        Self::with_builtin_controls()
    }
}

// ---------------------------------------------------------------------------
// Control-node handlers
// ---------------------------------------------------------------------------

/// `condition { expr }` → evaluates `expr` as a dotted path against
/// the execution context and picks the `"true"` or `"false"` branch
/// based on JSON truthiness.
pub struct ConditionHandler;

impl NodeHandler for ConditionHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::Condition { expr } = &node.kind else {
            return Err(wrong_kind(node, "condition"));
        };
        let resolved = ctx.resolve_path(expr).cloned().unwrap_or(Value::Null);
        let truthy = is_truthy(&resolved);
        let label = if truthy { "true" } else { "false" };
        Ok(NodeOutcome::branch(label, json!({ "value": resolved })))
    }
}

/// `switch { expr }` → evaluates `expr` and branches on its string
/// form. The switch node's own output records `{"value": <resolved>}`
/// so downstream nodes can still read it.
pub struct SwitchHandler;

impl NodeHandler for SwitchHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::Switch { expr } = &node.kind else {
            return Err(wrong_kind(node, "switch"));
        };
        let resolved = ctx.resolve_path(expr).cloned().unwrap_or(Value::Null);
        let label = switch_label(&resolved);
        Ok(NodeOutcome::branch(label, json!({ "value": resolved })))
    }
}

/// Merge is a pass-through. Fan-in is handled by the engine (a merge
/// simply has multiple incoming edges and one outgoing edge).
pub struct MergeHandler;

impl NodeHandler for MergeHandler {
    fn handle(&self, _node: &Node, _ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        Ok(NodeOutcome::ok_null())
    }
}

/// `fail { reason? }` → ends the run with a declared failure.
pub struct FailHandler;

impl NodeHandler for FailHandler {
    fn handle(&self, node: &Node, _ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::Fail { reason } = &node.kind else {
            return Err(wrong_kind(node, "fail"));
        };
        Ok(NodeOutcome::Fail {
            reason: reason.clone().unwrap_or_else(|| "workflow failed".into()),
        })
    }
}

/// `terminate` → ends the run successfully.
pub struct TerminateHandler;

impl NodeHandler for TerminateHandler {
    fn handle(&self, _node: &Node, _ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        Ok(NodeOutcome::Terminate { value: Value::Null })
    }
}

/// `respond { status?, content_type?, body_template, input_from? }` →
/// renders the declared HTTP reply and records it on the context. The
/// HTTP trigger writes it verbatim when the run completes; non-HTTP
/// runs carry it in the outcome/record (visible, inert). Last
/// `respond` wins.
pub struct RespondHandler;

impl NodeHandler for RespondHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::Respond {
            status,
            content_type,
            body_template,
            input_from,
        } = &node.kind
        else {
            return Err(wrong_kind(node, "respond"));
        };
        let data = match input_from {
            Some(path) => {
                ctx.resolve_path(path)
                    .cloned()
                    .ok_or_else(|| crate::error::Error::Tool {
                        tool: "respond".into(),
                        reason: format!(
                            "input_from path `{path}` is not set in the execution context"
                        ),
                    })?
            }
            None => ctx.trigger.input.clone(),
        };
        let status = status.unwrap_or(200);
        // The validator enforces this statically; keep the runtime
        // check so programmatically-built workflows fail loudly too.
        if !(100..=599).contains(&status) {
            return Err(crate::error::Error::Tool {
                tool: "respond".into(),
                reason: format!("status {status} is outside 100..=599"),
            });
        }
        let body = crate::engine::template::render_template(body_template, &data)?;
        let spec = crate::engine::outcome::HttpResponseSpec {
            status,
            content_type: content_type
                .clone()
                .unwrap_or_else(|| "application/json".into()),
            body,
        };
        let summary = json!({
            "status": spec.status,
            "content_type": spec.content_type,
            "bytes": spec.body.len(),
        });
        ctx.http_response = Some(spec);
        Ok(NodeOutcome::Continue {
            value: summary,
            branch: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Test-only stub handler
// ---------------------------------------------------------------------------

/// Pass-through handler used only in engine tests. Real builds
/// register concrete handlers from the tools module instead.
#[doc(hidden)]
pub struct StubHandler;

impl NodeHandler for StubHandler {
    fn handle(&self, node: &Node, _ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        Ok(NodeOutcome::Continue {
            value: json!({ "stub": node.kind.name() }),
            branch: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wrong_kind(node: &Node, expected: &str) -> Error {
    Error::Workflow {
        workflow: node.id.clone(),
        reason: format!(
            "handler for `{expected}` received node of kind `{}`",
            node.kind.name()
        ),
    }
}

/// JSON truthiness rule used by `Condition`:
/// - `null` / `false` / `""` / `0` / `[]` / `{}` → false
/// - everything else → true
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Derive a switch branch label from a resolved JSON value.
/// Strings use their content verbatim; bools / numbers use their
/// JSON text; everything else degrades to the JSON type name so
/// authors see a loud mismatch against their declared `when` labels.
fn switch_label(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".into(),
        Value::Array(_) => "array".into(),
        Value::Object(_) => "object".into(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{RunOptions, TriggerMeta};
    use crate::workflow::model::Node;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(json!({"x": 1})),
            &RunOptions::default(),
        )
    }

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.into(),
            retry: None,
            kind,
        }
    }

    #[test]
    fn condition_true_branch() {
        let mut c = ctx();
        c.node_outputs.insert("a".into(), json!({ "flag": true }));
        let h = ConditionHandler;
        let out = h
            .handle(
                &node(
                    "c",
                    NodeKind::Condition {
                        expr: "a.flag".into(),
                    },
                ),
                &mut c,
            )
            .unwrap();
        assert_eq!(out, NodeOutcome::branch("true", json!({ "value": true })));
    }

    #[test]
    fn condition_false_branch_on_missing() {
        let mut c = ctx();
        let h = ConditionHandler;
        let out = h
            .handle(
                &node(
                    "c",
                    NodeKind::Condition {
                        expr: "no.such.path".into(),
                    },
                ),
                &mut c,
            )
            .unwrap();
        assert!(matches!(
            out,
            NodeOutcome::Continue { branch: Some(ref l), .. } if l == "false"
        ));
    }

    #[test]
    fn switch_uses_string_value_as_label() {
        let mut c = ctx();
        c.node_outputs
            .insert("analyze".into(), json!({ "decision": "comment" }));
        let h = SwitchHandler;
        let out = h
            .handle(
                &node(
                    "s",
                    NodeKind::Switch {
                        expr: "analyze.decision".into(),
                    },
                ),
                &mut c,
            )
            .unwrap();
        assert_eq!(
            out,
            NodeOutcome::branch("comment", json!({ "value": "comment" }))
        );
    }

    #[test]
    fn merge_passes_through_null() {
        let mut c = ctx();
        let h = MergeHandler;
        let out = h.handle(&node("m", NodeKind::Merge), &mut c).unwrap();
        assert_eq!(out, NodeOutcome::ok_null());
    }

    #[test]
    fn fail_carries_reason() {
        let mut c = ctx();
        let h = FailHandler;
        let out = h
            .handle(
                &node(
                    "f",
                    NodeKind::Fail {
                        reason: Some("nope".into()),
                    },
                ),
                &mut c,
            )
            .unwrap();
        assert_eq!(
            out,
            NodeOutcome::Fail {
                reason: "nope".into()
            }
        );
    }

    #[test]
    fn fail_default_reason() {
        let mut c = ctx();
        let h = FailHandler;
        let out = h
            .handle(&node("f", NodeKind::Fail { reason: None }), &mut c)
            .unwrap();
        assert!(matches!(
            out,
            NodeOutcome::Fail { ref reason } if reason == "workflow failed"
        ));
    }

    #[test]
    fn terminate_ends_run() {
        let mut c = ctx();
        let h = TerminateHandler;
        let out = h.handle(&node("t", NodeKind::Terminate), &mut c).unwrap();
        assert!(matches!(out, NodeOutcome::Terminate { .. }));
    }

    #[test]
    fn respond_records_templated_reply_on_context() {
        let mut c = ctx();
        c.node_outputs
            .insert("classify".into(), json!({ "reply": "Connecting you now." }));
        let h = RespondHandler;
        let out = h
            .handle(
                &node(
                    "answer",
                    NodeKind::Respond {
                        status: Some(200),
                        content_type: Some("text/xml".into()),
                        body_template: "<Response><Say>{{reply}}</Say></Response>".into(),
                        input_from: Some("classify".into()),
                    },
                ),
                &mut c,
            )
            .unwrap();
        let spec = c.http_response.as_ref().expect("spec recorded");
        assert_eq!(spec.status, 200);
        assert_eq!(spec.content_type, "text/xml");
        assert_eq!(
            spec.body,
            "<Response><Say>Connecting you now.</Say></Response>"
        );
        match out {
            NodeOutcome::Continue { value, branch } => {
                assert_eq!(value["status"], 200);
                assert_eq!(value["bytes"], spec.body.len());
                assert!(branch.is_none());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn respond_defaults_and_trigger_input() {
        // No input_from → templates against the trigger input; default
        // status/content-type fill in.
        let mut c = ctx();
        let h = RespondHandler;
        h.handle(
            &node(
                "answer",
                NodeKind::Respond {
                    status: None,
                    content_type: None,
                    body_template: r#"{"echo": {{x}}}"#.into(),
                    input_from: None,
                },
            ),
            &mut c,
        )
        .unwrap();
        let spec = c.http_response.as_ref().unwrap();
        assert_eq!(spec.status, 200);
        assert_eq!(spec.content_type, "application/json");
        assert_eq!(spec.body, r#"{"echo": 1}"#);
    }

    #[test]
    fn respond_missing_input_path_errors() {
        let mut c = ctx();
        let h = RespondHandler;
        let err = h
            .handle(
                &node(
                    "answer",
                    NodeKind::Respond {
                        status: None,
                        content_type: None,
                        body_template: "x".into(),
                        input_from: Some("nope.value".into()),
                    },
                ),
                &mut c,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("nope.value"));
    }

    #[test]
    fn registry_dispatches_by_kind_name() {
        let r = HandlerRegistry::with_builtin_controls();
        let mut c = ctx();
        let out = r.dispatch(&node("t", NodeKind::Terminate), &mut c).unwrap();
        assert!(matches!(out, NodeOutcome::Terminate { .. }));
    }

    #[test]
    fn registry_without_handler_errors() {
        let r = HandlerRegistry::with_builtin_controls();
        let mut c = ctx();
        let err = r
            .dispatch(
                &node(
                    "rf",
                    NodeKind::ReadFile {
                        path_from: "x".into(),
                    },
                ),
                &mut c,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("no handler"));
    }

    #[test]
    fn registry_fallback_catches_unregistered_kinds() {
        let mut r = HandlerRegistry::with_builtin_controls();
        r.set_fallback(Box::new(StubHandler));
        let mut c = ctx();
        let out = r
            .dispatch(
                &node(
                    "rf",
                    NodeKind::ReadFile {
                        path_from: "x".into(),
                    },
                ),
                &mut c,
            )
            .unwrap();
        assert!(matches!(out, NodeOutcome::Continue { .. }));
    }

    #[test]
    fn is_truthy_rules() {
        assert!(!is_truthy(&Value::Null));
        assert!(!is_truthy(&json!(false)));
        assert!(!is_truthy(&json!(0)));
        assert!(!is_truthy(&json!("")));
        assert!(!is_truthy(&json!([])));
        assert!(!is_truthy(&json!({})));
        assert!(is_truthy(&json!(true)));
        assert!(is_truthy(&json!(1)));
        assert!(is_truthy(&json!("x")));
        assert!(is_truthy(&json!([1])));
        assert!(is_truthy(&json!({ "a": 1 })));
    }

    #[test]
    fn switch_label_forms() {
        assert_eq!(switch_label(&json!("a")), "a");
        assert_eq!(switch_label(&json!(true)), "true");
        assert_eq!(switch_label(&json!(7)), "7");
        assert_eq!(switch_label(&Value::Null), "null");
        assert_eq!(switch_label(&json!([1, 2])), "array");
        assert_eq!(switch_label(&json!({ "a": 1 })), "object");
    }
}
