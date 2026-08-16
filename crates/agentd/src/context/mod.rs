// SPDX-License-Identifier: Apache-2.0
//! **Contexts** (RFC 0026 §5): the durable, self-compacting working memory of
//! the root agent (`context/root`) and of every A2A conversation
//! (`context/<contextId>`). A context is a versioned record of messages, a
//! structured summary block, the loaded skill set, the working **plan**
//! (RFC 0026 §5.3), the last preflight verdict and a token estimate; the
//! runtime is its single writer and checkpoints it after every turn.
//!
//! The transcript representation here ([`Msg`]) is **serializable** (unlike
//! the provider wire type) and converts to [`crate::wire::intel::Message`]
//! at request time. Sub-modules: [`plan`] (the plan object), [`memory`] (the
//! durable KV), [`compact`] (compaction planning + application), [`skills`]
//! (skill catalogue/loaded set), [`tokens`] (estimates).

pub mod compact;
pub mod memory;
pub mod plan;
pub mod skills;
pub mod tokens;

use crate::state::{Durable, Kind, now_ms};
use crate::store::StoreError;
use crate::wire::intel::{Message, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The root context id (RFC 0025 §3.3).
pub const ROOT: &str = "root";

/// One transcript entry. `ts` is wall-clock ms; tool results keep the parsed
/// JSON when the tool returned structured content (text otherwise).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Msg {
    System {
        text: String,
        #[serde(default)]
        ts: u64,
    },
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<String>,
        #[serde(default)]
        ts: u64,
    },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default)]
        ts: u64,
    },
    Tool {
        id: String,
        name: String,
        content: Value,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        ts: u64,
    },
    /// A runtime note (a run finished, a subagent reported, an instruction
    /// changed…) — rendered to the model as a system message.
    Note {
        text: String,
        #[serde(default)]
        ts: u64,
    },
}

impl Msg {
    pub fn system(text: impl Into<String>) -> Msg {
        Msg::System {
            text: text.into(),
            ts: now_ms(),
        }
    }
    pub fn user(text: impl Into<String>, principal: Option<String>) -> Msg {
        Msg::User {
            text: text.into(),
            principal,
            ts: now_ms(),
        }
    }
    pub fn assistant(text: Option<String>, tool_calls: Vec<ToolCall>) -> Msg {
        Msg::Assistant {
            text,
            tool_calls,
            ts: now_ms(),
        }
    }
    pub fn tool(
        id: impl Into<String>,
        name: impl Into<String>,
        content: Value,
        is_error: bool,
    ) -> Msg {
        Msg::Tool {
            id: id.into(),
            name: name.into(),
            content,
            is_error,
            ts: now_ms(),
        }
    }
    pub fn note(text: impl Into<String>) -> Msg {
        Msg::Note {
            text: text.into(),
            ts: now_ms(),
        }
    }
    pub fn ts(&self) -> u64 {
        match self {
            Msg::System { ts, .. }
            | Msg::User { ts, .. }
            | Msg::Assistant { ts, .. }
            | Msg::Tool { ts, .. }
            | Msg::Note { ts, .. } => *ts,
        }
    }
    /// The provider wire message.
    pub fn to_wire(&self) -> Message {
        match self {
            Msg::System { text, .. } => Message::System(text.clone()),
            Msg::Note { text, .. } => Message::System(format!("[note] {text}")),
            Msg::User { text, .. } => Message::User(text.clone()),
            Msg::Assistant {
                text, tool_calls, ..
            } => Message::Assistant {
                text: text.clone(),
                tool_calls: tool_calls.clone(),
            },
            Msg::Tool {
                id,
                content,
                is_error,
                ..
            } => Message::ToolResult {
                id: id.clone(),
                content: match content {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
                is_error: *is_error,
            },
        }
    }
    /// A rough token estimate for this message.
    pub fn est_tokens(&self) -> u64 {
        let body = match self {
            Msg::System { text, .. } | Msg::User { text, .. } | Msg::Note { text, .. } => {
                tokens::estimate(text)
            }
            Msg::Assistant {
                text, tool_calls, ..
            } => {
                tokens::estimate(text.as_deref().unwrap_or(""))
                    + tool_calls
                        .iter()
                        .map(|c| tokens::estimate(&c.name) + tokens::estimate_value(&c.arguments))
                        .sum::<u64>()
            }
            Msg::Tool { content, .. } => tokens::estimate_value(content),
        };
        body + tokens::MESSAGE_OVERHEAD
    }
    pub fn is_user(&self) -> bool {
        matches!(self, Msg::User { .. })
    }
}

/// The structured summary block a compaction produces (RFC 0026 §5.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Summary {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goals: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<String>,
    /// Free-form narrative when the summarizer could not fill the fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrative: Option<String>,
    /// How many messages the summary stands for (cumulative).
    #[serde(default)]
    pub covers_messages: u64,
    #[serde(default)]
    pub updated: u64,
}

impl Summary {
    pub fn is_empty(&self) -> bool {
        self.goals.is_empty()
            && self.decisions.is_empty()
            && self.open.is_empty()
            && self.facts.is_empty()
            && self.narrative.as_deref().is_none_or(str::is_empty)
    }
    /// The block as the model sees it.
    pub fn render(&self) -> String {
        let mut out = String::from("Summary of earlier conversation:\n");
        let sect = |out: &mut String, title: &str, items: &[String]| {
            if !items.is_empty() {
                out.push_str(title);
                out.push('\n');
                for i in items {
                    out.push_str("- ");
                    out.push_str(i);
                    out.push('\n');
                }
            }
        };
        sect(&mut out, "Goals:", &self.goals);
        sect(&mut out, "Decisions:", &self.decisions);
        sect(&mut out, "Open items:", &self.open);
        sect(&mut out, "Facts:", &self.facts);
        if let Some(n) = &self.narrative
            && !n.is_empty()
        {
            out.push_str(n);
            out.push('\n');
        }
        out
    }
    /// Merge a newer summary over this one (lists appended + deduped, capped).
    pub fn absorb(&mut self, newer: Summary) {
        fn merge(into: &mut Vec<String>, more: Vec<String>) {
            for m in more {
                if !into.contains(&m) {
                    into.push(m);
                }
            }
            if into.len() > 32 {
                let drop = into.len() - 32;
                into.drain(0..drop);
            }
        }
        merge(&mut self.goals, newer.goals);
        merge(&mut self.decisions, newer.decisions);
        merge(&mut self.open, newer.open);
        merge(&mut self.facts, newer.facts);
        if newer.narrative.is_some() {
            self.narrative = newer.narrative;
        }
        self.covers_messages += newer.covers_messages;
        self.updated = now_ms();
    }
}

/// A loaded skill reference (name + version hash), RFC 0028 §7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRef {
    pub name: String,
    pub hash: String,
}

/// The kind of a context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    #[default]
    Root,
    Conversation,
}

/// The durable context record (RFC 0025 §3.3 `context`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextState {
    #[serde(default)]
    pub kind: ContextKind,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub summary: Summary,
    #[serde(default)]
    pub messages: Vec<Msg>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<plan::Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<Value>,
    #[serde(default)]
    pub est_tokens: u64,
    #[serde(default)]
    pub model_window: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The A2A task the conversation's current work is attached to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default)]
    pub turns: u64,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    /// Whether the record has changed since its last checkpoint (never stored).
    #[serde(skip)]
    pub dirty: bool,
}

impl ContextState {
    pub fn new(kind: ContextKind, model_window: u64) -> ContextState {
        ContextState {
            kind,
            version: 1,
            summary: Summary::default(),
            messages: Vec::new(),
            skills: Vec::new(),
            plan: None,
            preflight: None,
            est_tokens: 0,
            model_window,
            principal: None,
            task: None,
            turns: 0,
            created: now_ms(),
            updated: now_ms(),
            dirty: true,
        }
    }

    pub fn append(&mut self, msg: Msg) {
        self.est_tokens += msg.est_tokens();
        self.messages.push(msg);
        self.touch();
    }

    pub fn append_all(&mut self, msgs: impl IntoIterator<Item = Msg>) {
        for m in msgs {
            self.append(m);
        }
    }

    pub fn touch(&mut self) {
        self.updated = now_ms();
        self.dirty = true;
    }

    /// Recompute the token estimate from scratch (after compaction / restore).
    pub fn recount(&mut self) {
        self.est_tokens = self.messages.iter().map(Msg::est_tokens).sum::<u64>()
            + tokens::estimate(&self.summary.render())
            + self
                .plan
                .as_ref()
                .map(|p| tokens::estimate(&p.render()))
                .unwrap_or(0);
    }

    /// Whether the compaction threshold is crossed (`compact_at` × window).
    pub fn needs_compaction(&self, compact_at: f64) -> bool {
        self.model_window > 0 && (self.est_tokens as f64) > compact_at * (self.model_window as f64)
    }

    /// The transcript slice a turn worker receives: summary block (if any) +
    /// plan block (if any) as system messages, then the messages verbatim.
    pub fn slice(&self) -> Vec<Msg> {
        let mut out = Vec::with_capacity(self.messages.len() + 2);
        if !self.summary.is_empty() {
            out.push(Msg::system(self.summary.render()));
        }
        if let Some(p) = &self.plan {
            out.push(Msg::system(p.render()));
        }
        out.extend(self.messages.iter().cloned());
        out
    }

    /// The prompt slice for a turn: summary block (if any) + plan block (if
    /// any) as system messages, then the messages verbatim.
    pub fn to_wire(&self) -> Vec<Message> {
        let mut out = Vec::with_capacity(self.messages.len() + 2);
        if !self.summary.is_empty() {
            out.push(Message::System(self.summary.render()));
        }
        if let Some(p) = &self.plan {
            out.push(Message::System(p.render()));
        }
        out.extend(self.messages.iter().map(Msg::to_wire));
        out
    }

    /// The loaded skill names.
    pub fn skill_names(&self) -> BTreeSet<String> {
        self.skills.iter().map(|s| s.name.clone()).collect()
    }

    pub fn load_skill(&mut self, name: &str, hash: &str, max_loaded: usize) -> Result<(), String> {
        if let Some(s) = self.skills.iter_mut().find(|s| s.name == name) {
            s.hash = hash.to_string();
            self.touch();
            return Ok(());
        }
        if self.skills.len() >= max_loaded {
            return Err(format!(
                "skills.max_loaded ({max_loaded}) reached; unload one first"
            ));
        }
        self.skills.push(SkillRef {
            name: name.to_string(),
            hash: hash.to_string(),
        });
        self.touch();
        Ok(())
    }

    pub fn unload_skill(&mut self, name: &str) -> bool {
        let before = self.skills.len();
        self.skills.retain(|s| s.name != name);
        if self.skills.len() != before {
            self.touch();
            true
        } else {
            false
        }
    }
}

/// The in-memory registry of contexts + their durable mirror.
pub struct Contexts {
    map: BTreeMap<String, ContextState>,
    model_window: u64,
}

impl Contexts {
    pub fn new(model_window: u64) -> Contexts {
        Contexts {
            map: BTreeMap::new(),
            model_window,
        }
    }

    /// Adopt restored context envelopes.
    pub fn restore(&mut self, envelopes: &[crate::store::Envelope]) -> Vec<String> {
        let mut lost = Vec::new();
        for env in envelopes {
            match serde_json::from_value::<ContextState>(env.state.clone()) {
                Ok(mut c) => {
                    c.dirty = false;
                    if c.model_window == 0 {
                        c.model_window = self.model_window;
                    }
                    c.recount();
                    self.map.insert(env.id.clone(), c);
                }
                Err(_) => lost.push(env.id.clone()),
            }
        }
        lost
    }

    pub fn get(&self, id: &str) -> Option<&ContextState> {
        self.map.get(id)
    }
    pub fn get_mut(&mut self, id: &str) -> Option<&mut ContextState> {
        self.map.get_mut(id)
    }
    /// Get or create the root context.
    pub fn root(&mut self) -> &mut ContextState {
        let w = self.model_window;
        self.map
            .entry(ROOT.to_string())
            .or_insert_with(|| ContextState::new(ContextKind::Root, w))
    }
    /// Get or create a conversation context.
    pub fn conversation(&mut self, id: &str, principal: Option<&str>) -> &mut ContextState {
        let w = self.model_window;
        let c = self.map.entry(id.to_string()).or_insert_with(|| {
            let mut c = ContextState::new(ContextKind::Conversation, w);
            c.principal = principal.map(str::to_string);
            c
        });
        if c.principal.is_none() && principal.is_some() {
            c.principal = principal.map(str::to_string);
        }
        c
    }
    pub fn ids(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    /// The token estimate of the largest live context (for `agent_context_tokens`).
    pub fn max_est_tokens(&self) -> u64 {
        self.map.values().map(|c| c.est_tokens).max().unwrap_or(0)
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn remove(&mut self, id: &str) -> Option<ContextState> {
        self.map.remove(id)
    }

    /// Checkpoint every dirty context (RFC 0025 §5: after each turn /
    /// compaction). Returns the ids written.
    pub fn checkpoint(&mut self, durable: &Durable) -> Result<Vec<String>, StoreError> {
        let mut written = Vec::new();
        for (id, c) in self.map.iter_mut() {
            if !c.dirty {
                continue;
            }
            crate::state::kill_point("context.before_put");
            durable.put(
                Kind::Context,
                id,
                serde_json::to_value(&*c).unwrap_or(Value::Null),
                None,
            )?;
            c.dirty = false;
            written.push(id.clone());
        }
        Ok(written)
    }

    /// A status view (`agent://conversations`).
    pub fn status(&self) -> Value {
        json!(
            self.map
                .iter()
                .map(|(id, c)| {
                    json!({
                        "id": id, "kind": c.kind, "version": c.version, "messages": c.messages.len(),
                        "est_tokens": c.est_tokens, "turns": c.turns, "principal": c.principal,
                        "skills": c.skills.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
                        "plan": c.plan.as_ref().map(|p| p.progress()),
                        "updated": c.updated,
                    })
                })
                .collect::<Vec<_>>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use std::sync::Arc;

    #[test]
    fn messages_round_trip_and_convert_to_wire() {
        let m = Msg::tool("c1", "memory.get", json!({"value": 1}), false);
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], json!("tool"));
        let back: Msg = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
        match back.to_wire() {
            Message::ToolResult {
                id,
                content,
                is_error,
            } => {
                assert_eq!(id, "c1");
                assert_eq!(content, r#"{"value":1}"#);
                assert!(!is_error);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(Msg::note("run finished").to_wire(), Message::System(s) if s.starts_with("[note]"))
        );
        assert!(Msg::user("hello world", None).est_tokens() > tokens::MESSAGE_OVERHEAD);
    }

    #[test]
    fn contexts_checkpoint_dirty_only_and_restore() {
        let mem = Arc::new(MemoryStore::new());
        let d = Durable::new(
            mem.clone(),
            "agentd",
            "i",
            crate::state::Policy::default(),
            None,
        );
        let mut cs = Contexts::new(100_000);
        cs.root().append(Msg::user("hi", None));
        cs.conversation("ctx-1", Some("user:a"))
            .append(Msg::user("q", Some("user:a".into())));
        let written = cs.checkpoint(&d).unwrap();
        assert_eq!(written, vec!["ctx-1".to_string(), "root".to_string()]);
        assert!(
            cs.checkpoint(&d).unwrap().is_empty(),
            "clean after checkpoint"
        );
        cs.get_mut("ctx-1")
            .unwrap()
            .append(Msg::assistant(Some("a".into()), vec![]));
        assert_eq!(cs.checkpoint(&d).unwrap(), vec!["ctx-1".to_string()]);
        // Restore into a fresh registry.
        let restored = d.restore().unwrap();
        let mut cs2 = Contexts::new(100_000);
        assert!(cs2.restore(restored.of(Kind::Context)).is_empty());
        assert_eq!(cs2.len(), 2);
        let c = cs2.get("ctx-1").unwrap();
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.principal.as_deref(), Some("user:a"));
        assert!(!c.dirty);
        assert!(c.est_tokens > 0);
    }

    #[test]
    fn summary_renders_and_absorbs() {
        let mut s = Summary {
            goals: vec!["ship".into()],
            ..Default::default()
        };
        assert!(s.render().contains("Goals:\n- ship"));
        s.absorb(Summary {
            goals: vec!["ship".into(), "test".into()],
            facts: vec!["x=1".into()],
            covers_messages: 5,
            ..Default::default()
        });
        assert_eq!(s.goals, vec!["ship".to_string(), "test".to_string()]);
        assert_eq!(s.facts, vec!["x=1".to_string()]);
        assert_eq!(s.covers_messages, 5);
        assert!(!s.is_empty());
    }

    #[test]
    fn skills_load_unload_and_caps() {
        let mut c = ContextState::new(ContextKind::Conversation, 1000);
        c.load_skill("a", "h1", 2).unwrap();
        c.load_skill("b", "h2", 2).unwrap();
        assert!(c.load_skill("c", "h3", 2).is_err());
        c.load_skill("a", "h9", 2).unwrap();
        assert_eq!(c.skills[0].hash, "h9");
        assert!(c.unload_skill("a"));
        assert!(!c.unload_skill("a"));
        assert_eq!(c.skill_names().len(), 1);
    }
}
