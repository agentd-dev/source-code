//! Goal-mode planner (RFC 0006 §2, Mode 3).
//!
//! The agent defines its own workflow. A planner prompt — the
//! instructions, the build's *actual* node-kind vocabulary, and a
//! summary of the active policy — asks a backend to emit a workflow
//! TOML. That TOML is then treated exactly like a human-authored
//! one: parsed, validated, and (only on operator approval) executed.
//!
//! Validation failures are fed back for a bounded number of repair
//! rounds. The materialized plan is a file: it can be saved, diffed,
//! signed, and promoted into a Mode-1 workflow.
//!
//! This module *produces* a [`WorkflowDoc`] + its source text; it
//! does not run it. The runtime owns approval and execution so the
//! governance gate lives on one path.

use serde_json::json;

use crate::error::{Error, Result};
use crate::intelligence::client::IntelligenceClient;
use crate::intelligence::protocol::{Message, Request};
use crate::workflow::{self, WorkflowDoc};

/// A plan the runtime may execute after approval.
#[derive(Debug)]
pub struct Plan {
    pub doc: WorkflowDoc,
    /// The TOML the model produced (post-fence-stripping). Saved /
    /// hashed for audit; promotable into a Mode-1 workflow.
    pub source: String,
    /// How many generation attempts it took (1 = first try).
    pub attempts: u32,
}

/// Inputs the planner needs beyond the goal text.
pub struct PlanContext<'a> {
    /// System prompt from `--instructions`, if any.
    pub system: Option<&'a str>,
    /// Human-readable policy summary so the plan stays inside the
    /// allowlists (e.g. "fs.write: /tmp/out/**; http: <none>").
    pub policy_summary: String,
    /// Names of configured intelligence backends, so a planned
    /// `llm_infer` references one that exists.
    pub backends: Vec<String>,
}

/// Generate and validate a plan for `goal`, repairing validation
/// failures up to `max_repairs` times. Returns the first plan that
/// validates clean, or the last error.
pub fn generate(
    client: &dyn IntelligenceClient,
    goal: &str,
    ctx: &PlanContext<'_>,
    max_repairs: u32,
) -> Result<Plan> {
    let mut messages = vec![
        Message {
            role: "system".into(),
            content: planner_system_prompt(ctx),
        },
        Message {
            role: "user".into(),
            content: format!("Goal:\n{goal}\n\nEmit the workflow TOML now."),
        },
    ];

    let mut last_err = String::new();
    for attempt in 1..=(max_repairs + 1) {
        let response = client.complete(&Request {
            model: "reasoning".into(),
            messages: messages.clone(),
            max_tokens: None,
            temperature: None,
        })?;
        let source = strip_fences(&response.content);

        match WorkflowDoc::from_toml(&source) {
            Ok(doc) => {
                let report = workflow::validate(&doc);
                if report.ok() {
                    tracing::info!(
                        target: "agentd::audit",
                        event = "plan.generated",
                        attempt,
                        workflow = %doc.name,
                        plan_hash = %short_hash(&source),
                    );
                    return Ok(Plan {
                        doc,
                        source,
                        attempts: attempt,
                    });
                }
                last_err = format!(
                    "validation failed: {}",
                    report
                        .issues
                        .iter()
                        .map(|i| format!("[{}] {}", i.code, i.message))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            Err(e) => {
                last_err = format!("TOML did not parse: {e}");
            }
        }

        tracing::warn!(
            target: "agentd::audit",
            event = "plan.repair",
            attempt,
            reason = %last_err,
        );
        // Feed the failure back for a repair round.
        messages.push(Message {
            role: "assistant".into(),
            content: response.content,
        });
        messages.push(Message {
            role: "user".into(),
            content: json!({
                "error": last_err,
                "instruction": "Fix the workflow and re-emit the full TOML. \
                                Output only TOML.",
            })
            .to_string(),
        });
    }

    Err(Error::Workflow {
        workflow: "<plan>".into(),
        reason: format!(
            "planner could not produce a valid workflow after {} attempts: {last_err}",
            max_repairs + 1
        ),
    })
}

fn planner_system_prompt(ctx: &PlanContext<'_>) -> String {
    let mut s = String::new();
    if let Some(sys) = ctx.system {
        s.push_str(sys);
        s.push_str("\n\n");
    }
    s.push_str(
        "You are a workflow planner for the agentd runtime. Given a goal, \
         emit a single workflow as TOML — and nothing else. The runtime \
         validates and (after operator approval) executes it.\n\n\
         A workflow is a directed ACYCLIC graph. Required shape:\n\
         - top-level `name`\n\
         - one or more `[[start_nodes]]` with name + source \
           (\"manual\"|\"event\"|\"http\") + entry_node\n\
         - `[[nodes]]` each with a unique `id` and a `type`\n\
         - `[[edges]]` from→to, optionally `when = \"<label>\"` for \
           switch/condition branches\n\n\
         Node types you may use:\n\
         - read_file{path_from} read_env{key} parse_json{input_from}\n\
         - template_render{template,input_from?} json_select{input_from,path} \
           diff_compute{left_from,right_from}\n\
         - llm_infer{backend,prompt,input_from?,output_schema?}\n\
         - write_file{path_from,content_from} create_dir{path_from}\n\
         - http_request{method,url_from,body_from?} shell_run{command,args_from?}\n\
         - condition{expr} switch{expr} merge fail{reason?} terminate\n\n\
         A node's output is read by later nodes as `<node_id>.<field>` \
         (e.g. `analyze.parsed.decision`). The reserved `trigger` holds \
         the input payload.\n\n",
    );
    s.push_str("Active policy (stay inside it — anything else will be denied at runtime):\n");
    s.push_str(&ctx.policy_summary);
    s.push('\n');
    if !ctx.backends.is_empty() {
        s.push_str(&format!(
            "Available llm_infer backends: {}.\n",
            ctx.backends.join(", ")
        ));
    }
    s.push_str(
        "\nKeep it minimal and correct: every node reachable from a start \
         node, no cycles, edges reference declared nodes, exactly one \
         terminal path. Output ONLY the TOML.",
    );
    s
}

/// Strip a leading ```toml / ``` fence if the model wrapped its
/// output. Tolerant: returns the inner content or the trimmed input.
fn strip_fences(raw: &str) -> String {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // drop an optional language tag on the first line
        let body = rest.split_once('\n').map(|(_, b)| b).unwrap_or("");
        let body = body.strip_suffix("```").unwrap_or(body);
        return body.trim().to_string();
    }
    t.to_string()
}

/// Short, dependency-free content hash for audit correlation. Not
/// cryptographic — just a stable fingerprint of a plan's text.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::client::MockClient;
    use std::sync::Arc;

    const GOOD: &str = r#"
        name = "auto_plan"
        [[start_nodes]]
        name = "main"
        source = "manual"
        entry_node = "a"
        [[nodes]]
        id = "a"
        type = "terminate"
    "#;

    fn ctx() -> PlanContext<'static> {
        PlanContext {
            system: Some("be terse"),
            policy_summary: "fs.write: /tmp/**".into(),
            backends: vec!["claude".into()],
        }
    }

    #[test]
    fn first_try_success() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text(GOOD);
        let plan = generate(mock.as_ref(), "do a thing", &ctx(), 2).unwrap();
        assert_eq!(plan.doc.name, "auto_plan");
        assert_eq!(plan.attempts, 1);

        // The system prompt carried the policy summary + vocabulary +
        // the standing instructions.
        let sent = &mock.received()[0].messages[0].content;
        assert!(sent.contains("be terse"));
        assert!(sent.contains("fs.write: /tmp/**"));
        assert!(sent.contains("llm_infer"));
        assert!(sent.contains("claude"));
    }

    #[test]
    fn strips_code_fences() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text(format!("```toml\n{GOOD}\n```"));
        let plan = generate(mock.as_ref(), "g", &ctx(), 1).unwrap();
        assert_eq!(plan.doc.name, "auto_plan");
        assert!(!plan.source.contains("```"));
    }

    #[test]
    fn repairs_invalid_then_succeeds() {
        let mock = Arc::new(MockClient::new());
        // First: a cycle (invalid). Second: the good one.
        mock.enqueue_text(
            r#"
            name = "bad"
            [[start_nodes]]
            name = "main"
            source = "manual"
            entry_node = "a"
            [[nodes]]
            id = "a"
            type = "merge"
            [[nodes]]
            id = "b"
            type = "merge"
            [[edges]]
            from = "a"
            to = "b"
            [[edges]]
            from = "b"
            to = "a"
            "#,
        );
        mock.enqueue_text(GOOD);
        let plan = generate(mock.as_ref(), "g", &ctx(), 2).unwrap();
        assert_eq!(plan.attempts, 2);
        // The repair turn told the model what was wrong.
        let second = &mock.received()[1];
        let feedback = &second.messages[second.messages.len() - 1].content;
        assert!(feedback.contains("cycle"), "{feedback}");
    }

    #[test]
    fn gives_up_after_max_repairs() {
        let mock = Arc::new(MockClient::new());
        for _ in 0..3 {
            mock.enqueue_text("name = \"x\"\nnot valid toml [[[");
        }
        let err = generate(mock.as_ref(), "g", &ctx(), 2).unwrap_err();
        assert!(format!("{err}").contains("after 3 attempts"));
        assert_eq!(mock.received().len(), 3); // 1 + 2 repairs
    }
}
