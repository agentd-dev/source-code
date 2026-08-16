// SPDX-License-Identifier: AGPL-3.0-only
//! **Compaction** (RFC 0026 §5.2): when a context's token estimate crosses
//! `context.compact_at × model_window` (or `context.compact` is called), the
//! older messages are summarized by a structured `think` into the summary
//! block, the last `keep_last` messages stay verbatim, the plan stays
//! verbatim, skill bodies not referenced in the kept window are evicted (the
//! names stay), the version bumps and the record is checkpointed.
//!
//! The runtime never calls the model itself, so compaction is two halves: a
//! pure **plan** ([`plan_compaction`] — which messages to fold + the prompt +
//! the output schema for the summarizer) and a pure **apply**
//! ([`apply_compaction`] — fold the summarizer's verdict into the context).
//! The turn worker runs the `think` in between.

use super::{ContextState, Msg, Summary};
use crate::state::now_ms;
use serde_json::{Value, json};

/// A prepared compaction: what to summarize and how.
#[derive(Debug, Clone)]
pub struct CompactionRequest {
    /// The number of leading messages that will be folded.
    pub fold: usize,
    /// The summarizer prompt (system).
    pub system: String,
    /// The summarizer input (user).
    pub input: String,
    /// The summarizer's output schema.
    pub output_schema: Value,
    /// The context version this plan was made against (apply refuses drift).
    pub version: u64,
}

/// The summarizer's output schema (RFC 0026 §5.1 summary block).
pub fn summary_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goals": {"type": "array", "items": {"type": "string"}},
            "decisions": {"type": "array", "items": {"type": "string"}},
            "open": {"type": "array", "items": {"type": "string"}},
            "facts": {"type": "array", "items": {"type": "string"}},
            "narrative": {"type": "string"}
        },
        "required": ["goals", "decisions", "open", "facts"],
        "additionalProperties": false
    })
}

const SUMMARIZER_SYSTEM: &str = "You compact an agent's conversation memory. Read the transcript excerpt and \
produce a faithful structured summary: goals (what is being pursued), decisions (what was decided and why), \
open (unresolved questions, pending work, promises made), facts (concrete facts, values, identifiers, results \
worth remembering). Keep entries short and specific; never invent; keep identifiers, numbers and names verbatim. \
Reply with ONLY one JSON object matching the schema.";

/// Decide what to fold. `keep_last` messages stay verbatim; a fold always
/// ends before the kept window and never splits an assistant tool-call from
/// its tool results. Returns `None` when there is nothing worth folding
/// (fewer than `keep_last + 2` messages).
pub fn plan_compaction(
    ctx: &ContextState,
    keep_last: usize,
    target_tokens: Option<u64>,
) -> Option<CompactionRequest> {
    let n = ctx.messages.len();
    if n < keep_last + 2 {
        return None;
    }
    let mut fold = n - keep_last;
    // If a target is given, fold more aggressively until the estimate of the
    // kept tail is under it (but always keep at least 2 messages).
    if let Some(target) = target_tokens {
        let mut kept: u64 = ctx.messages[fold..].iter().map(Msg::est_tokens).sum();
        while kept > target && n - fold > 2 {
            kept -= ctx.messages[fold].est_tokens();
            fold += 1;
        }
    }
    // Do not split a tool round: if the first kept message is a tool result,
    // move the boundary back to its assistant call.
    while fold > 0 && matches!(ctx.messages.get(fold), Some(Msg::Tool { .. })) {
        fold -= 1;
    }
    if fold == 0 {
        return None;
    }
    let mut input = String::new();
    if !ctx.summary.is_empty() {
        input.push_str("Previous summary (already compacted; extend it, do not lose it):\n");
        input.push_str(&ctx.summary.render());
        input.push('\n');
    }
    input.push_str("Transcript excerpt to compact:\n");
    for m in &ctx.messages[..fold] {
        input.push_str(&render_for_summary(m));
        input.push('\n');
    }
    Some(CompactionRequest {
        fold,
        system: SUMMARIZER_SYSTEM.to_string(),
        input,
        output_schema: summary_schema(),
        version: ctx.version,
    })
}

fn render_for_summary(m: &Msg) -> String {
    const CAP: usize = 2000;
    let clip = |s: &str| {
        if s.chars().count() > CAP {
            format!("{}…", s.chars().take(CAP).collect::<String>())
        } else {
            s.to_string()
        }
    };
    match m {
        Msg::System { text, .. } => format!("[system] {}", clip(text)),
        Msg::Note { text, .. } => format!("[note] {}", clip(text)),
        Msg::User {
            text, principal, ..
        } => format!(
            "[user{}] {}",
            principal
                .as_deref()
                .map(|p| format!(" {p}"))
                .unwrap_or_default(),
            clip(text)
        ),
        Msg::Assistant {
            text, tool_calls, ..
        } => {
            let calls: Vec<String> = tool_calls
                .iter()
                .map(|c| format!("{}({})", c.name, clip(&c.arguments.to_string())))
                .collect();
            format!(
                "[assistant] {}{}",
                clip(text.as_deref().unwrap_or("")),
                if calls.is_empty() {
                    String::new()
                } else {
                    format!(" calls: {}", calls.join(", "))
                }
            )
        }
        Msg::Tool {
            name,
            content,
            is_error,
            ..
        } => {
            format!(
                "[tool {name}{}] {}",
                if *is_error { " error" } else { "" },
                clip(&content.to_string())
            )
        }
    }
}

/// Fold the summarizer's verdict into the context: absorb the summary, drop
/// the folded messages, bump the version, evict unreferenced skill bodies
/// (names stay — the caller drops the bodies from its cache), recount.
/// Refuses when the context changed since the plan (`version` drift).
pub fn apply_compaction(
    ctx: &mut ContextState,
    req: &CompactionRequest,
    verdict: &Value,
) -> Result<CompactionOutcome, String> {
    if ctx.version != req.version {
        return Err(format!(
            "context version moved from {} to {} during compaction",
            req.version, ctx.version
        ));
    }
    if req.fold > ctx.messages.len() {
        return Err("compaction fold exceeds the message count".into());
    }
    let mut newer: Summary = match verdict {
        Value::Object(_) => serde_json::from_value(verdict.clone())
            .map_err(|e| format!("summary does not match the schema: {e}"))?,
        Value::String(s) => Summary {
            narrative: Some(s.clone()),
            ..Default::default()
        },
        _ => return Err("summary verdict must be an object".into()),
    };
    newer.covers_messages = req.fold as u64;
    newer.updated = now_ms();
    let before_tokens = ctx.est_tokens;
    ctx.summary.absorb(newer);
    ctx.messages.drain(..req.fold);
    ctx.version += 1;
    ctx.recount();
    ctx.touch();
    Ok(CompactionOutcome {
        folded: req.fold,
        version: ctx.version,
        before_tokens,
        after_tokens: ctx.est_tokens,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionOutcome {
    pub folded: usize,
    pub version: u64,
    pub before_tokens: u64,
    pub after_tokens: u64,
}

/// A degraded compaction for when the summarizer is unavailable: fold the
/// older messages into a plain narrative built from their rendered lines
/// (truncated). Never loses the plan or the skill names.
pub fn apply_fallback(
    ctx: &mut ContextState,
    req: &CompactionRequest,
) -> Result<CompactionOutcome, String> {
    let mut lines: Vec<String> = ctx.messages[..req.fold.min(ctx.messages.len())]
        .iter()
        .map(render_for_summary)
        .collect();
    let mut narrative = lines.join("\n");
    while narrative.len() > 8_000 && lines.len() > 1 {
        lines.remove(0);
        narrative = format!("(earlier messages elided)\n{}", lines.join("\n"));
    }
    apply_compaction(ctx, req, &Value::String(narrative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextKind;
    use crate::wire::intel::ToolCall;

    fn ctx_with(n: usize) -> ContextState {
        let mut c = ContextState::new(ContextKind::Conversation, 1000);
        for i in 0..n {
            c.append(Msg::user(
                format!("message number {i} with some words in it"),
                None,
            ));
        }
        c
    }

    #[test]
    fn plan_keeps_the_tail_and_does_not_split_tool_rounds() {
        let mut c = ctx_with(6);
        c.append(Msg::assistant(
            None,
            vec![ToolCall {
                id: "c1".into(),
                name: "memory.get".into(),
                arguments: json!({"key": "k"}),
            }],
        ));
        c.append(Msg::tool(
            "c1",
            "memory.get",
            json!({"found": false}),
            false,
        ));
        c.append(Msg::assistant(Some("done".into()), vec![]));
        // 9 messages; keep_last 2 → fold 7 → messages[7] is the tool result → move back to 6.
        let req = plan_compaction(&c, 2, None).unwrap();
        assert_eq!(req.fold, 6);
        assert!(req.input.contains("[user] message number 0"));
        assert!(
            !req.input.contains("memory.get"),
            "the tool round stays verbatim"
        );
        assert!(
            plan_compaction(&ctx_with(3), 2, None).is_none(),
            "too short to fold"
        );
        // A target folds more aggressively (never below 2 kept).
        let big = ctx_with(20);
        let req = plan_compaction(&big, 10, Some(1)).unwrap();
        assert_eq!(req.fold, 18);
    }

    #[test]
    fn apply_absorbs_the_summary_bumps_version_and_recounts() {
        let mut c = ctx_with(10);
        c.plan = Some(super::super::plan::Plan::create("goal", &[json!("a")], 32).unwrap());
        c.load_skill("review", "h", 8).unwrap();
        let before = c.est_tokens;
        let req = plan_compaction(&c, 3, None).unwrap();
        let out = apply_compaction(
            &mut c,
            &req,
            &json!({"goals": ["finish"], "decisions": [], "open": ["q1"], "facts": ["n=7"]}),
        )
        .unwrap();
        assert_eq!(out.folded, 7);
        assert_eq!(out.version, 2);
        assert_eq!(c.messages.len(), 3);
        assert_eq!(c.summary.goals, vec!["finish".to_string()]);
        assert_eq!(c.summary.covers_messages, 7);
        assert!(c.est_tokens < before);
        assert!(c.plan.is_some(), "plan kept verbatim");
        assert_eq!(c.skills.len(), 1, "skill names kept");
        assert!(c.dirty);
        // Version drift is refused.
        let req2 = plan_compaction(&ctx_with(10), 3, None).unwrap();
        assert!(apply_compaction(&mut c, &req2, &json!({})).is_err());
        // Fallback path.
        let mut c2 = ctx_with(10);
        let req = plan_compaction(&c2, 3, None).unwrap();
        let out = apply_fallback(&mut c2, &req).unwrap();
        assert_eq!(out.folded, 7);
        assert!(
            c2.summary
                .narrative
                .as_deref()
                .unwrap()
                .contains("message number 0")
        );
        // A wire slice carries the summary + plan first.
        let wire = c.to_wire();
        assert!(
            matches!(&wire[0], crate::wire::intel::Message::System(s) if s.starts_with("Summary of earlier"))
        );
        assert!(
            matches!(&wire[1], crate::wire::intel::Message::System(s) if s.starts_with("Plan ("))
        );
    }
}
