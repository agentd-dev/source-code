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
//!    be valid JSON (full JSON-Schema enforcement arrives in Phase 7
//!    alongside the policy pass).
//!
//! The handler emits `{"content": "...", "parsed": <value|null>,
//! "usage": {...}}` so downstream nodes can branch on either the raw
//! text (via `node.content`) or the parsed payload (`node.parsed.*`).

use serde_json::{Value, json};

use crate::engine::{ExecutionContext, HandlerRegistry, NodeHandler, NodeOutcome};
use crate::error::{Error, Result};
use crate::intelligence::backends::BackendMap;
use crate::intelligence::client::IntelligenceClient;
use crate::intelligence::protocol::{Message, Request};
use crate::workflow::{Node, NodeKind};

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the `llm_infer` handler on a registry. `backends` maps
/// names to transports; `"default"` is the CLI-configured socket
/// transport when present (RFC 0006 §3).
pub fn register(registry: &mut HandlerRegistry, backends: BackendMap) {
    registry.register("llm_infer", Box::new(LlmInferHandler::new(backends)));
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub struct LlmInferHandler {
    backends: BackendMap,
    /// Max tokens applied when a node doesn't override it.
    default_max_tokens: u32,
}

impl LlmInferHandler {
    pub fn new(backends: BackendMap) -> Self {
        Self {
            backends,
            default_max_tokens: 1024,
        }
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

        // Submit. Single user message — system messages are a Phase 6
        // concern (they land when mission config defines a persona).
        let request = Request {
            model: "fast".into(),
            messages: vec![Message {
                role: "user".into(),
                content: rendered,
            }],
            max_tokens: Some(self.default_max_tokens),
            temperature: None,
        };
        let response = client.complete(&request)?;

        // Optional parse. If a schema is declared, the model must
        // return JSON; otherwise the raw content becomes the output.
        let parsed = if output_schema.is_some() {
            let p: Value = serde_json::from_str(&response.content).map_err(|e| {
                Error::Schema(format!(
                    "llm_infer node `{}`: model returned invalid JSON: {e}",
                    node.id
                ))
            })?;
            Some(p)
        } else {
            None
        };

        Ok(NodeOutcome::Continue {
            value: json!({
                "content": response.content,
                "parsed": parsed,
                "usage": {
                    "prompt_tokens": response.usage.prompt_tokens,
                    "completion_tokens": response.usage.completion_tokens,
                },
            }),
            branch: None,
        })
    }
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
        Node {
            id: "infer".into(),
            retry: None,
            kind: NodeKind::LlmInfer {
                backend: "default".into(),
                prompt: prompt.into(),
                input_from: input_from.map(Into::into),
                output_schema: output_schema.map(Into::into),
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
