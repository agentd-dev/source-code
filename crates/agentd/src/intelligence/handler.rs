//! `llm_infer` node handler (RFC §15.3).
//!
//! Orchestrates one bounded reasoning step:
//!
//! 1. Pull input context from `input_from` if set.
//! 2. Render the `prompt` template using the same `{{key}}`
//!    substitution as [`tools::data::TemplateRenderHandler`].
//! 3. Build an [`intelligence::Request`] — single user message plus
//!    an optional system prefix.
//! 4. Dispatch through the registered [`IntelligenceClient`].
//! 5. If `output_schema` is declared, require the response content to
//!    be valid JSON — and, with the `schema` feature, conform to the
//!    named JSON Schema file. `output_repairs` re-prompts with the
//!    validation error a bounded number of times before failing.
//!
//! The handler emits `{"content": "...", "parsed": <value|null>,
//! "usage": {...}}` so downstream nodes can branch on either the raw
//! text (via `node.content`) or the parsed payload (`node.parsed.*`).

use serde_json::{Value, json};

use crate::budget::BudgetRef;
use crate::engine::{ExecutionContext, HandlerRegistry, NodeHandler, NodeOutcome};
use crate::error::{Error, Result};
use crate::intelligence::backends::BackendMap;
use crate::intelligence::client::IntelligenceClient;
use crate::intelligence::protocol::{Message, Request};
use crate::observability::Metrics;
use crate::workflow::{Node, NodeKind};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the `llm_infer` handler on a registry. `backends` maps
/// names to transports; `"default"` is the CLI-configured socket
/// transport when present (RFC 0006 §3). `budget` enforces the
/// cumulative token cap; `metrics` records calls + tokens.
pub fn register(
    registry: &mut HandlerRegistry,
    backends: BackendMap,
    budget: BudgetRef,
    metrics: Arc<Metrics>,
) {
    registry.register(
        "llm_infer",
        Box::new(LlmInferHandler::new(backends).with_budget(budget, metrics)),
    );
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub struct LlmInferHandler {
    backends: BackendMap,
    /// Max tokens applied when a node doesn't override it.
    default_max_tokens: u32,
    budget: Option<BudgetRef>,
    metrics: Option<Arc<Metrics>>,
}

impl LlmInferHandler {
    pub fn new(backends: BackendMap) -> Self {
        Self {
            backends,
            default_max_tokens: 1024,
            budget: None,
            metrics: None,
        }
    }

    pub fn with_budget(mut self, budget: BudgetRef, metrics: Arc<Metrics>) -> Self {
        self.budget = Some(budget);
        self.metrics = Some(metrics);
        self
    }

    pub fn with_default_max_tokens(mut self, max_tokens: u32) -> Self {
        self.default_max_tokens = max_tokens;
        self
    }
}

impl NodeHandler for LlmInferHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::LlmInfer {
            backend,
            prompt,
            input_from,
            output_schema,
            output_repairs,
        } = &node.kind
        else {
            return Err(Error::Tool {
                tool: "llm_infer".into(),
                reason: format!(
                    "handler for `llm_infer` received node `{}` of kind `{}`",
                    node.id,
                    node.kind.name()
                ),
            });
        };

        // Resolve the named backend (RFC 0006 §3). `"default"` is
        // the CLI socket transport; everything else comes from
        // `[[intelligence.backends]]`.
        let client = self.backends.get(backend.as_str()).ok_or_else(|| {
            let mut known: Vec<&str> = self.backends.keys().map(String::as_str).collect();
            known.sort_unstable();
            Error::Intelligence(format!(
                "backend `{backend}` is not configured on this runtime; known backends: {}",
                if known.is_empty() {
                    "<none>".to_string()
                } else {
                    known.join(", ")
                }
            ))
        })?;

        // Dry-run: skip the actual call so CI / preview runs don't
        // burn tokens (RFC §22.2). Emit a visible placeholder so
        // downstream nodes can still see the node executed.
        if ctx.dry_run {
            return Ok(NodeOutcome::Continue {
                value: json!({
                    "content": "<dry-run>",
                    "parsed": null,
                    "dry_run": true,
                }),
                branch: None,
            });
        }

        // Pull the input payload (if any) — used to render the
        // prompt template.
        let input = match input_from.as_deref() {
            Some(path) => ctx.resolve_path(path).cloned().unwrap_or(Value::Null),
            None => Value::Null,
        };
        let rendered = render_template(prompt, &input);

        // One user message to start; repair rounds append the rejected
        // output + the error and re-prompt (system messages are a
        // later concern).
        let mut messages = vec![Message {
            role: "user".into(),
            content: rendered,
        }];
        let max_repairs = output_repairs.unwrap_or(0);
        let mut last_err = String::new();

        for attempt in 0..=max_repairs {
            // Token budget gate (RFC 0006 §5) — checked before each call;
            // repair rounds cost tokens too.
            if let Some(budget) = &self.budget
                && let Err(reason) = budget.check_llm_budget()
            {
                return Err(Error::Tool {
                    tool: "llm_infer".into(),
                    reason,
                });
            }
            let request = Request {
                model: "fast".into(),
                messages: messages.clone(),
                max_tokens: Some(self.default_max_tokens),
                temperature: None,
            };
            let response = client.complete(&request)?;
            let tokens = u64::from(response.usage.prompt_tokens + response.usage.completion_tokens);
            if let Some(budget) = &self.budget {
                budget.add_llm_tokens(tokens);
            }
            if let Some(metrics) = &self.metrics {
                metrics.add_llm(tokens);
            }

            // No output contract → raw content out.
            let Some(schema_spec) = output_schema.as_deref() else {
                return Ok(NodeOutcome::Continue {
                    value: json!({
                        "content": response.content,
                        "parsed": null,
                        "usage": usage_value(&response),
                    }),
                    branch: None,
                });
            };

            match parse_and_validate(&response.content, schema_spec) {
                Ok(parsed) => {
                    return Ok(NodeOutcome::Continue {
                        value: json!({
                            "content": response.content,
                            "parsed": parsed,
                            "usage": usage_value(&response),
                        }),
                        branch: None,
                    });
                }
                Err(e) => {
                    last_err = e;
                    if attempt < max_repairs {
                        tracing::warn!(
                            target: "agentd::audit",
                            event = "llm_infer.repair",
                            node = %node.id,
                            attempt = attempt + 1,
                            reason = %last_err,
                        );
                        messages.push(Message {
                            role: "assistant".into(),
                            content: response.content,
                        });
                        messages.push(Message {
                            role: "user".into(),
                            content: format!(
                                "Your previous output was rejected: {last_err}. \
                                 Reply with corrected output only."
                            ),
                        });
                    }
                }
            }
        }

        Err(Error::Schema(format!(
            "llm_infer node `{}`: output failed validation after {} attempt(s): {last_err}",
            node.id,
            max_repairs + 1
        )))
    }
}

fn usage_value(response: &crate::intelligence::protocol::Response) -> Value {
    json!({
        "prompt_tokens": response.usage.prompt_tokens,
        "completion_tokens": response.usage.completion_tokens,
    })
}

/// Parse the model's output as JSON and, when `spec` names a readable
/// schema file (and the `schema` feature is compiled), validate it
/// against that JSON Schema. A non-file `spec` (e.g. `inline`) is a
/// JSON-only contract. Returns the parsed value or a human-readable
/// rejection reason for the repair loop.
fn parse_and_validate(content: &str, spec: &str) -> std::result::Result<Value, String> {
    let parsed: Value =
        serde_json::from_str(content).map_err(|e| format!("model returned invalid JSON: {e}"))?;

    #[cfg(feature = "schema")]
    if let Ok(schema_text) = std::fs::read_to_string(spec) {
        let schema: Value = serde_json::from_str(&schema_text)
            .map_err(|e| format!("output_schema `{spec}` is not valid JSON: {e}"))?;
        let compiled = jsonschema::JSONSchema::compile(&schema)
            .map_err(|e| format!("output_schema `{spec}` is not a valid schema: {e}"))?;
        if let Err(errors) = compiled.validate(&parsed) {
            let msgs: Vec<String> = errors.map(|e| e.to_string()).collect();
            return Err(format!("schema violation: {}", msgs.join("; ")));
        }
    }
    #[cfg(not(feature = "schema"))]
    let _ = spec;

    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Shared `{{key}}` template renderer
// ---------------------------------------------------------------------------

/// Minimal `{{key}}` substitution — kept in-module instead of
/// leaning on `tools::data::render_template` so the intelligence
/// layer compiles without the `tools-data` feature. `key` is a
/// dotted path into `data`; unknown keys render the `{{key}}` marker
/// back so authors see missing inputs instead of silent empties.
fn render_template(template: &str, data: &Value) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let rest = &template[i + 2..];
            if let Some(end_rel) = rest.find("}}") {
                let key = rest[..end_rel].trim();
                match walk(data, key) {
                    Some(Value::String(s)) => out.push_str(s),
                    Some(v) => out.push_str(&v.to_string()),
                    None => {
                        out.push_str("{{");
                        out.push_str(key);
                        out.push_str("}}");
                    }
                }
                i += 2 + end_rel + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn walk<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(root);
    }
    let mut cursor = root;
    for seg in path.split('.') {
        cursor = cursor.as_object()?.get(seg)?;
    }
    Some(cursor)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{RunOptions, TriggerMeta};
    use crate::intelligence::backends::single_backend;
    use crate::intelligence::client::MockClient;
    use crate::intelligence::protocol::Response;
    use std::sync::Arc;

    fn ctx(input: Value) -> ExecutionContext {
        ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(input),
            &RunOptions::default(),
        )
    }

    fn node_with(prompt: &str, input_from: Option<&str>, output_schema: Option<&str>) -> Node {
        node_full(prompt, input_from, output_schema, None)
    }

    fn node_full(
        prompt: &str,
        input_from: Option<&str>,
        output_schema: Option<&str>,
        output_repairs: Option<u32>,
    ) -> Node {
        Node {
            id: "infer".into(),
            retry: None,
            kind: NodeKind::LlmInfer {
                backend: "default".into(),
                prompt: prompt.into(),
                input_from: input_from.map(Into::into),
                output_schema: output_schema.map(Into::into),
                output_repairs,
            },
        }
    }

    #[test]
    fn handler_dispatches_rendered_prompt() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text("summary line");
        let h = LlmInferHandler::new(single_backend(mock.clone()));

        let mut c = ctx(json!({ "author": "Ada", "doc": "v1" }));
        let out = h
            .handle(
                &node_with("Review doc `{{doc}}` by {{author}}.", Some("trigger"), None),
                &mut c,
            )
            .unwrap();

        // The mock was called exactly once with the rendered prompt.
        let received = mock.received();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].messages[0].content, "Review doc `v1` by Ada.");

        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["content"], "summary line");
                assert_eq!(value["parsed"], Value::Null);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn dry_run_short_circuits() {
        let mock = Arc::new(MockClient::new());
        // No response enqueued — proving the client is never called.
        let h = LlmInferHandler::new(single_backend(mock.clone()));
        let mut c = ctx(json!({}));
        c.dry_run = true;
        let out = h
            .handle(&node_with("anything", None, None), &mut c)
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["dry_run"], true);
                assert_eq!(value["content"], "<dry-run>");
            }
            _ => panic!(),
        }
        assert!(mock.received().is_empty());
    }

    #[test]
    fn output_schema_requires_valid_json() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text("not json");
        let h = LlmInferHandler::new(single_backend(mock));
        let mut c = ctx(json!({}));
        let err = h
            .handle(&node_with("classify", None, Some("decision.json")), &mut c)
            .unwrap_err();
        assert!(format!("{err}").contains("invalid JSON"));
    }

    #[test]
    fn output_schema_accepts_well_formed_json() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue(Response {
            content: r#"{"decision":"comment"}"#.into(),
            usage: Default::default(),
        });
        let h = LlmInferHandler::new(single_backend(mock));
        let mut c = ctx(json!({}));
        let out = h
            .handle(&node_with("classify", None, Some("decision.json")), &mut c)
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["parsed"]["decision"], "comment");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn repair_round_recovers_from_bad_then_good_json() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text("not json at all"); // attempt 1: parse fails
        mock.enqueue_text(r#"{"decision":"alpha"}"#); // attempt 2: ok
        let h = LlmInferHandler::new(single_backend(mock.clone()));
        let mut c = ctx(json!({}));
        let out = h
            .handle(
                &node_full("classify", None, Some("inline"), Some(1)),
                &mut c,
            )
            .unwrap();
        assert_eq!(mock.received().len(), 2, "one repair round");
        match out {
            NodeOutcome::Continue { value, .. } => assert_eq!(value["parsed"]["decision"], "alpha"),
            other => panic!("{other:?}"),
        }
        // The repair turn fed the rejection back to the model.
        let second = &mock.received()[1];
        assert!(
            second
                .messages
                .iter()
                .any(|m| m.content.contains("rejected"))
        );
    }

    #[test]
    fn no_repair_budget_fails_bounded_on_bad_output() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text("not json");
        let h = LlmInferHandler::new(single_backend(mock));
        let mut c = ctx(json!({}));
        let err = h
            .handle(
                &node_full("classify", None, Some("inline"), Some(0)),
                &mut c,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("after 1 attempt"));
    }

    #[cfg(feature = "schema")]
    #[test]
    fn schema_violation_is_caught_then_repaired() {
        let dir = tempfile::TempDir::new().unwrap();
        let schema = dir.path().join("decision.json");
        std::fs::write(
            &schema,
            r#"{"type":"object","required":["decision"],
                "properties":{"decision":{"enum":["alpha","beta"]}}}"#,
        )
        .unwrap();
        let spec = schema.to_string_lossy().into_owned();

        let mock = Arc::new(MockClient::new());
        mock.enqueue_text(r#"{"decision":"zeta"}"#); // valid JSON, wrong enum
        mock.enqueue_text(r#"{"decision":"alpha"}"#); // conforms
        let h = LlmInferHandler::new(single_backend(mock.clone()));
        let mut c = ctx(json!({}));
        let out = h
            .handle(&node_full("classify", None, Some(&spec), Some(1)), &mut c)
            .unwrap();
        assert_eq!(mock.received().len(), 2, "schema rejection forced a repair");
        match out {
            NodeOutcome::Continue { value, .. } => assert_eq!(value["parsed"]["decision"], "alpha"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn token_budget_blocks_when_exhausted() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue(Response {
            content: "ok".into(),
            usage: crate::intelligence::protocol::Usage {
                prompt_tokens: 80,
                completion_tokens: 30,
            },
        });
        mock.enqueue_text("should never be reached");
        let cfg = crate::budget::BudgetConfig {
            max_llm_tokens: Some(100),
            ..Default::default()
        };
        let budget = Arc::new(crate::budget::BudgetTracker::new(&cfg));
        let metrics = crate::observability::Metrics::new();
        let h =
            LlmInferHandler::new(single_backend(mock.clone())).with_budget(budget, metrics.clone());

        let mut c = ctx(json!({}));
        // First call succeeds and records 110 tokens.
        h.handle(&node_with("a", None, None), &mut c).unwrap();
        assert_eq!(metrics.snapshot().llm_tokens, 110);
        assert_eq!(metrics.snapshot().llm_calls, 1);
        // Second call is refused — budget already over cap.
        let mut c2 = ctx(json!({}));
        let err = h.handle(&node_with("b", None, None), &mut c2).unwrap_err();
        assert!(format!("{err}").contains("max_llm_tokens"));
        // The blocked call never reached the client.
        assert_eq!(mock.received().len(), 1);
    }

    #[test]
    fn unknown_backend_errors_cleanly() {
        let mock = Arc::new(MockClient::new());
        let h = LlmInferHandler::new(single_backend(mock));
        let mut c = ctx(json!({}));

        let err = h
            .handle(
                &Node {
                    id: "i".into(),
                    retry: None,
                    kind: NodeKind::LlmInfer {
                        backend: "enterprise-gateway".into(),
                        prompt: "hi".into(),
                        input_from: None,
                        output_schema: None,
                        output_repairs: None,
                    },
                },
                &mut c,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("not configured"));
    }

    #[test]
    fn template_render_with_missing_key_shows_marker() {
        assert_eq!(
            render_template("x={{nope}}", &json!({ "other": 1 })),
            "x={{nope}}"
        );
    }

    #[test]
    fn template_render_with_nested_path() {
        let data = json!({ "user": { "name": "Ada" } });
        assert_eq!(render_template("hi {{user.name}}", &data), "hi Ada");
    }

    #[test]
    fn template_render_with_non_string_value() {
        let data = json!({ "n": 42, "flag": true });
        assert_eq!(
            render_template("n={{n}} flag={{flag}}", &data),
            "n=42 flag=true"
        );
    }
}
