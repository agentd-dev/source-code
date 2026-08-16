// SPDX-License-Identifier: AGPL-3.0-only
//! The **context plan** (RFC 0026 §5.3, RFC 0028 §3 `plan.*`): a small ordered
//! checklist the model owns for one context — created by the preflight or by
//! `plan.create`, advanced by `plan.update`, cleared by `plan.clear`, rendered
//! into every prompt, auto-advanced when a bound run/subagent/task reaches a
//! terminal state. Temporary by intent (it belongs to the conversation, never
//! to memory), durable by construction (part of the context record).

use crate::state::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    #[default]
    Pending,
    InProgress,
    Done,
    Blocked,
    Skipped,
}

impl ItemStatus {
    pub fn parse(s: &str) -> Option<ItemStatus> {
        Some(match s {
            "pending" => ItemStatus::Pending,
            "in_progress" => ItemStatus::InProgress,
            "done" => ItemStatus::Done,
            "blocked" => ItemStatus::Blocked,
            "skipped" => ItemStatus::Skipped,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ItemStatus::Pending => "pending",
            ItemStatus::InProgress => "in_progress",
            ItemStatus::Done => "done",
            ItemStatus::Blocked => "blocked",
            ItemStatus::Skipped => "skipped",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ItemStatus::Done | ItemStatus::Blocked | ItemStatus::Skipped
        )
    }
}

/// What an item is bound to: its status follows the bound thing's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Binding {
    Run { id: String },
    Subagent { handle: String },
    Task { id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanItem {
    pub id: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub status: ItemStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<Binding>,
    #[serde(default)]
    pub updated: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub items: Vec<PlanItem>,
    #[serde(default)]
    pub next_id: u32,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
}

/// The default cap (`context.plan.max_items`).
pub const DEFAULT_MAX_ITEMS: usize = 32;

impl Plan {
    /// `plan.create {goal, items: [{title, detail?}]}`.
    pub fn create(goal: &str, items: &[Value], max_items: usize) -> Result<Plan, String> {
        if goal.trim().is_empty() {
            return Err("plan.create: goal must be non-empty".into());
        }
        if items.len() > max_items {
            return Err(format!(
                "plan.create: {} items exceed context.plan.max_items ({max_items})",
                items.len()
            ));
        }
        let mut p = Plan {
            goal: goal.trim().to_string(),
            items: Vec::new(),
            next_id: 1,
            created: now_ms(),
            updated: now_ms(),
        };
        for it in items {
            let (title, detail) = item_fields(it)?;
            p.push(title, detail);
        }
        Ok(p)
    }

    fn push(&mut self, title: String, detail: Option<String>) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(PlanItem {
            id,
            title,
            detail,
            status: ItemStatus::Pending,
            note: None,
            bound: None,
            updated: now_ms(),
        });
        self.updated = now_ms();
        id
    }

    /// `plan.update {item, status?, note?, bind?, insert?, reorder?, title?, detail?}`:
    /// `item` addresses by id (number) or by exact title; `insert` = `{title,
    /// detail?, after?: id}` adds a new item (no `item` needed); `reorder` =
    /// `[ids…]` sets the order.
    pub fn update(&mut self, args: &Value, max_items: usize) -> Result<(), String> {
        let mut did = false;
        if let Some(ins) = args.get("insert") {
            if self.items.len() >= max_items {
                return Err(format!(
                    "plan.update: context.plan.max_items ({max_items}) reached"
                ));
            }
            let (title, detail) = item_fields(ins)?;
            let id = self.push(title, detail);
            if let Some(after) = ins.get("after").and_then(Value::as_u64) {
                let item = self.items.pop().expect("just pushed");
                let pos = self
                    .items
                    .iter()
                    .position(|i| i.id as u64 == after)
                    .map(|p| p + 1)
                    .unwrap_or(self.items.len());
                self.items.insert(pos, item);
            }
            let _ = id;
            did = true;
        }
        if let Some(order) = args.get("reorder").and_then(Value::as_array) {
            let ids: Vec<u32> = order
                .iter()
                .filter_map(Value::as_u64)
                .map(|x| x as u32)
                .collect();
            let mut new_items = Vec::with_capacity(self.items.len());
            for id in &ids {
                if let Some(pos) = self.items.iter().position(|i| i.id == *id) {
                    new_items.push(self.items.remove(pos));
                }
            }
            new_items.append(&mut self.items);
            self.items = new_items;
            did = true;
        }
        if let Some(item) = args.get("item") {
            let idx = self
                .find(item)
                .ok_or_else(|| format!("plan.update: no such item {item}"))?;
            let it = &mut self.items[idx];
            if let Some(s) = args.get("status") {
                let s = s.as_str().and_then(ItemStatus::parse).ok_or_else(|| {
                    "plan.update: status must be pending|in_progress|done|blocked|skipped"
                        .to_string()
                })?;
                it.status = s;
            }
            if let Some(n) = args.get("note").and_then(Value::as_str) {
                it.note = if n.is_empty() {
                    None
                } else {
                    Some(n.to_string())
                };
            }
            if let Some(t) = args.get("title").and_then(Value::as_str)
                && !t.trim().is_empty()
            {
                it.title = t.trim().to_string();
            }
            if let Some(d) = args.get("detail").and_then(Value::as_str) {
                it.detail = if d.is_empty() {
                    None
                } else {
                    Some(d.to_string())
                };
            }
            if let Some(b) = args.get("bind") {
                it.bound = Some(parse_binding(b)?);
                if it.status == ItemStatus::Pending {
                    it.status = ItemStatus::InProgress;
                }
            }
            it.updated = now_ms();
            did = true;
        }
        if !did {
            return Err(
                "plan.update: nothing to do (give item+status/note/bind, insert, or reorder)"
                    .into(),
            );
        }
        self.updated = now_ms();
        Ok(())
    }

    fn find(&self, key: &Value) -> Option<usize> {
        match key {
            Value::Number(n) => n
                .as_u64()
                .and_then(|id| self.items.iter().position(|i| i.id as u64 == id)),
            Value::String(s) => s
                .parse::<u32>()
                .ok()
                .and_then(|id| self.items.iter().position(|i| i.id == id))
                .or_else(|| self.items.iter().position(|i| i.title == *s)),
            _ => None,
        }
    }

    /// Auto-advance items bound to `binding` (RFC 0026 §5.3): a terminal
    /// outcome marks the item done (success) or blocked (failure) with the
    /// outcome as the note. Returns the ids advanced.
    pub fn settle_binding(&mut self, binding: &Binding, ok: bool, note: Option<&str>) -> Vec<u32> {
        let mut out = Vec::new();
        for it in self
            .items
            .iter_mut()
            .filter(|i| i.bound.as_ref() == Some(binding) && !i.status.is_terminal())
        {
            it.status = if ok {
                ItemStatus::Done
            } else {
                ItemStatus::Blocked
            };
            if let Some(n) = note {
                it.note = Some(n.to_string());
            }
            it.updated = now_ms();
            out.push(it.id);
        }
        if !out.is_empty() {
            self.updated = now_ms();
        }
        out
    }

    /// `"2/5 done"`-style progress.
    pub fn progress(&self) -> String {
        let done = self
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::Done)
            .count();
        format!("{done}/{} done", self.items.len())
    }

    pub fn is_complete(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(|i| i.status.is_terminal())
    }

    /// The compact prompt block.
    pub fn render(&self) -> String {
        let mut out = format!("Plan ({}): {}\n", self.progress(), self.goal);
        for it in &self.items {
            let mark = match it.status {
                ItemStatus::Pending => "[ ]",
                ItemStatus::InProgress => "[~]",
                ItemStatus::Done => "[x]",
                ItemStatus::Blocked => "[!]",
                ItemStatus::Skipped => "[-]",
            };
            out.push_str(&format!("{mark} {}. {}", it.id, it.title));
            if let Some(d) = &it.detail {
                out.push_str(&format!(" — {d}"));
            }
            if let Some(n) = &it.note {
                out.push_str(&format!(" ({n})"));
            }
            if let Some(b) = &it.bound {
                let b = match b {
                    Binding::Run { id } => format!("run {id}"),
                    Binding::Subagent { handle } => format!("subagent {handle}"),
                    Binding::Task { id } => format!("task {id}"),
                };
                out.push_str(&format!(" [{b}]"));
            }
            out.push('\n');
        }
        out
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(json!({}))
    }
}

fn item_fields(v: &Value) -> Result<(String, Option<String>), String> {
    let title = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Object(_) => v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        _ => String::new(),
    };
    if title.is_empty() {
        return Err("plan item needs a non-empty title".into());
    }
    let detail = v
        .get("detail")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    Ok((title, detail))
}

fn parse_binding(v: &Value) -> Result<Binding, String> {
    if let Some(id) = v.get("run").and_then(Value::as_str) {
        return Ok(Binding::Run { id: id.to_string() });
    }
    if let Some(h) = v.get("subagent").and_then(Value::as_str) {
        return Ok(Binding::Subagent {
            handle: h.to_string(),
        });
    }
    if let Some(id) = v.get("task").and_then(Value::as_str) {
        return Ok(Binding::Task { id: id.to_string() });
    }
    Err("bind must be {run: id} | {subagent: handle} | {task: id}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_update_bind_settle_and_render() {
        let mut p = Plan::create(
            "ship v2",
            &[
                json!("write code"),
                json!({"title": "test", "detail": "all suites"}),
            ],
            32,
        )
        .unwrap();
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.progress(), "0/2 done");
        p.update(
            &json!({"item": 1, "status": "in_progress", "note": "started"}),
            32,
        )
        .unwrap();
        p.update(&json!({"item": "test", "bind": {"run": "r-1"}}), 32)
            .unwrap();
        assert_eq!(
            p.items[1].status,
            ItemStatus::InProgress,
            "binding moves pending → in_progress"
        );
        p.update(&json!({"insert": {"title": "docs", "after": 1}}), 32)
            .unwrap();
        assert_eq!(
            p.items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            vec!["write code", "docs", "test"]
        );
        p.update(&json!({"reorder": [3, 1]}), 32).unwrap();
        assert_eq!(
            p.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
        assert_eq!(
            p.settle_binding(&Binding::Run { id: "r-1".into() }, true, Some("completed")),
            vec![2]
        );
        assert_eq!(
            p.items.iter().find(|i| i.id == 2).unwrap().status,
            ItemStatus::Done
        );
        let r = p.render();
        assert!(r.starts_with("Plan (1/3 done): ship v2"), "{r}");
        assert!(r.contains("[~] 1. write code (started)"), "{r}");
        assert!(
            r.contains("[x] 2. test — all suites (completed) [run r-1]"),
            "{r}"
        );
        assert!(!p.is_complete());
        // Errors.
        assert!(
            p.update(&json!({"item": 99, "status": "done"}), 32)
                .is_err()
        );
        assert!(
            p.update(&json!({"item": 1, "status": "bogus"}), 32)
                .is_err()
        );
        assert!(p.update(&json!({}), 32).is_err());
        assert!(Plan::create("", &[], 32).is_err());
        assert!(Plan::create("g", &[json!("a"), json!("b")], 1).is_err());
        assert!(
            p.update(&json!({"insert": {"title": "x"}}), 3).is_err(),
            "cap on insert"
        );
        // Round trip.
        let v = p.to_value();
        let back: Plan = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }
}
