// SPDX-License-Identifier: AGPL-3.0-only
//! The **environment data** a system-prompt template renders, and the built-in
//! default template that renders it.
//!
//! The runtime exposes what it knows as *data* and the shape lives in a
//! template, so the same knobs an operator has (loops, conditions, limits,
//! field access) are the ones the built-in default uses. Nothing the default
//! renders is reachable only from Rust.
//!
//! ## The ordering is a cache decision, not a taste one
//!
//! Providers cache on the literal prefix of a request, so a section that
//! changes between turns invalidates the cache for everything after it. The
//! default template is therefore ordered by VOLATILITY:
//!
//! 1. persona + instruction — change only on reload
//! 2. workflows, services, streams, subagent templates — configuration
//! 3. skills catalogue — configuration, plus per-context bodies
//! 4. peers, signals, memory — live state, changing turn to turn
//!
//! A custom template that puts `{{#each signals.waiting}}` near the top will
//! still work; it will just miss the cache on most turns, which on a busy
//! instance is real money.

use super::reactor::Runtime;
use crate::engine::template::Data;
use serde_json::{Value, json};

/// How many entries of a list the DATA carries. The default template renders
/// what it is given rather than slicing (slicing is CEL, and the default must
/// work on a build without the `cel` feature) — so the cap lives here, where
/// it also bounds the work of building the data at all.
const CAP_LIST: usize = 16;
const CAP_PEERS: usize = 24;

/// The built-in template. It is written in the same language an operator
/// gets, and `agentd --context-template` prints it — so overriding starts
/// from a copy rather than from a guess.
pub const DEFAULT_TEMPLATE: &str = r#"You are {{instance}}, an autonomous, durable agent (agentd). You act by calling tools and reply when done.
{{#if tools.internal_text}}Internal tools ({{tools.internal_text}}) are executed by your runtime and are durable; other tools come from connected MCP servers.
{{/if}}Be concise and factual; never invent tool results.
{{#if instruction}}
## Instruction
{{instruction}}
{{/if}}{{#if extra}}
{{extra}}
{{/if}}{{#if workflows}}
## Workflows
{{#each workflows}}- {{this.name}}{{#if this.description}}: {{this.description}}{{/if}}
{{/each}}{{/if}}{{#if services}}
## Services (the external services this deployment may use)
{{#each services}}- {{this.name}}{{#if this.tags_text}} [{{this.tags_text}}]{{/if}}{{#if this.tools_text}} — tools: {{this.tools_text}}{{/if}}{{#if this.rate}} (rate {{this.rate}}){{/if}}
{{/each}}{{#if egress_closed}}Egress is CLOSED: only these services are reachable.
{{/if}}{{/if}}{{#if streams}}
## Streams (durable events; publish with an emit step, consume with a stream start)
{{#each streams}}- {{this}}
{{/each}}{{/if}}{{#if templates}}
## Subagent templates (spawn with subagent.run {template, params})
{{#each templates}}- {{this.name}} ({{this.tier}}){{#if this.params_text}} — params: {{this.params_text}}{{/if}}
{{/each}}{{/if}}{{#if skills}}
{{skills}}
{{/if}}{{#if peers}}
## Peers (agents reachable with a2a.send / a2a.delegate)
{{#each peers}}- {{this.name}}{{#if this.note}} ({{this.note}}){{/if}}
{{/each}}{{/if}}{{#if signals.any}}
## Signals (durable coordination; deliver with workflow.signal)
{{#each signals.waiting}}- waiting: {{this.name}} (run {{this.run}}, step {{this.step}})
{{/each}}{{#each signals.recent}}- fired recently: {{this}}
{{/each}}{{/if}}{{#if memory.keys_text}}
## Memory
Keys you can read with memory.get: {{memory.keys_text}}
{{/if}}"#;

impl Runtime {
    /// Everything a prompt template may read. Cheap to build (the memory-key
    /// hint is the only store read, and it is bounded).
    pub(crate) fn prompt_data(
        &self,
        ctx: Option<&crate::context::ContextState>,
        extra: Option<&str>,
    ) -> Data {
        let mut d = Data::new();
        d.insert("instance".into(), json!(self.instance));
        d.insert(
            "instruction".into(),
            json!(self.instruction.text.trim().to_string()),
        );
        d.insert("extra".into(), json!(extra.unwrap_or("")));
        let internal = self.granted_internal_tools();
        d.insert(
            "tools".into(),
            json!({"internal": internal, "internal_text": internal.join(", ")}),
        );

        // --- configuration-derived (stable across turns) --------------------
        d.insert(
            "workflows".into(),
            Value::Array(
                self.workflows
                    .values()
                    .map(|w| json!({"name": w.name, "description": w.description}))
                    .collect(),
            ),
        );
        d.insert(
            "services".into(),
            Value::Array(
                self.settings
                    .services
                    .iter()
                    .take(CAP_LIST)
                    .map(|(name, e)| {
                        let tags: Vec<&str> = e
                            .tags
                            .values()
                            .flatten()
                            .map(String::as_str)
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        let tools: Vec<String> = e.allow.clone().unwrap_or_default();
                        json!({"name": name, "kind": e.kind.as_str(),
                               "tags": tags, "tags_text": tags.join(", "),
                               "tools": tools, "tools_text": tools.join(", "),
                               "rate": e.rate})
                    })
                    .collect(),
            ),
        );
        d.insert(
            "egress_closed".into(),
            json!(self.settings.security.egress == crate::config::v2::Egress::Closed),
        );
        d.insert(
            "streams".into(),
            Value::Array(
                self.settings
                    .streams
                    .keys()
                    .take(CAP_LIST)
                    .map(|k| json!(k))
                    .collect(),
            ),
        );
        d.insert(
            "templates".into(),
            Value::Array(
                self.settings
                    .subagents
                    .templates
                    .iter()
                    .take(CAP_LIST)
                    .map(|(name, t)| {
                        let tier = if t
                            .instruction
                            .lines()
                            .any(|l| l.trim_start().starts_with(":::"))
                        {
                            "instance"
                        } else {
                            "flat"
                        };
                        let params: Vec<String> = t
                            .params
                            .iter()
                            .map(|(p, spec)| {
                                if spec.required {
                                    format!("{p} (required)")
                                } else {
                                    p.clone()
                                }
                            })
                            .collect();
                        json!({"name": name, "tier": tier,
                               "params": params, "params_text": params.join(", ")})
                    })
                    .collect(),
            ),
        );
        // The skills catalogue + this context's loaded bodies stay pre-rendered
        // prose: they are authored text, not a list to reshape.
        let mut skills = String::new();
        if let Some(cat) = self.skills.render_catalogue() {
            skills.push_str(&cat);
        }
        if let Some(c) = ctx {
            let bodies: Vec<&crate::context::skills::SkillBody> = c
                .skills
                .iter()
                .filter_map(|r| self.skills.body(&r.hash))
                .collect();
            if let Some(b) = crate::context::skills::render_bodies(&bodies) {
                if !skills.is_empty() {
                    skills.push('\n');
                }
                skills.push_str(&b);
            }
        }
        d.insert("skills".into(), json!(skills.trim_end()));

        // --- live state (volatile; last, so the prefix above stays cached) ---
        let mut peers: Vec<Value> = self
            .settings
            .a2a
            .peers
            .iter()
            .map(|p| json!({"name": p.name, "note": Value::Null}))
            .collect();
        for rec in self.subagents.values() {
            if rec.tier.as_deref() == Some("instance")
                && !super::reactor::is_terminal_status(&rec.status)
            {
                peers.push(json!({"name": rec.handle, "note": format!(
                    "instance child of template '{}', {}",
                    rec.template.as_deref().unwrap_or("?"), rec.status)}));
            }
        }
        peers.truncate(CAP_PEERS);
        d.insert("peers".into(), Value::Array(peers));

        let mut waiting: Vec<Value> = Vec::new();
        for (rid, run) in &self.runs {
            for (sid, st) in &run.steps {
                if st.status == crate::engine::run::StepStatus::Suspended
                    && let Some(w) = &st.wait
                    && w["kind"] == "signal"
                    && let Some(name) = w["signal"].as_str()
                {
                    waiting.push(json!({"name": name, "run": rid, "step": sid}));
                }
            }
        }
        waiting.truncate(CAP_LIST);
        let recent: Vec<Value> = self
            .recent_signals
            .keys()
            .rev()
            .take(8)
            .map(|k| json!(k))
            .collect();
        let any = !waiting.is_empty() || !recent.is_empty();
        d.insert(
            "signals".into(),
            json!({"waiting": waiting, "recent": recent, "any": any}),
        );

        let keys = self.memory_keys_hint().unwrap_or_default();
        d.insert(
            "memory".into(),
            json!({"keys": keys, "keys_text": keys.join(", ")}),
        );
        d
    }

    /// The internal tools this instance ACTUALLY grants, as tool-name families.
    ///
    /// Derived from the live registry rather than a fixed list, so an instance
    /// that narrows `agent.tools.internal` is never briefed on a family it
    /// would then be refused — the persona and the gate agree by construction.
    fn granted_internal_tools(&self) -> Vec<String> {
        let mut families: Vec<String> = Vec::new();
        for t in self.registry.iter() {
            if t.class != crate::registry::ToolClass::Internal
                || t.disabled
                || !self
                    .registry
                    .allowed(&crate::registry::Caller::Root, &t.name)
            {
                continue;
            }
            let fam = match t.name.split_once('.') {
                Some((head, _)) => format!("{head}.*"),
                None => t.name.clone(),
            };
            if !families.contains(&fam) {
                families.push(fam);
            }
        }
        families.sort();
        families
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_template_needs_no_cel() {
        // The default must render on a build WITHOUT `--features cel`, or
        // every such build silently ships an empty system prompt. Caps and
        // joins therefore live in the data, not in template expressions.
        let t = crate::context::prompt::Template::parse(DEFAULT_TEMPLATE)
            .expect("the built-in template parses");
        assert!(
            !t.needs_cel,
            "the default template must use bare paths only — it renders on every build"
        );
        assert!(
            t.reads("instruction"),
            "the default carries standing policy"
        );
    }

    #[test]
    fn the_default_renders_stable_before_volatile() {
        // Providers cache on the literal prefix: a block that changes every
        // turn invalidates everything after it.
        let pos = |needle: &str| {
            DEFAULT_TEMPLATE
                .find(needle)
                .unwrap_or_else(|| panic!("default template lost {needle}"))
        };
        assert!(pos("## Instruction") < pos("## Services"));
        assert!(pos("## Services") < pos("## Peers"));
        assert!(pos("## Peers") < pos("## Signals"));
        assert!(pos("## Signals") < pos("## Memory"));
    }
}
