// SPDX-License-Identifier: Apache-2.0
//! CODE-REGISTERED tools (RFC 0022 §4) — the embedder seam.
//!
//! An embedder building its own binary on the `agentd-core` library can
//! register **native Rust tools** the agent calls alongside MCP tools:
//!
//! ```no_run
//! agentd::tools::register(agentd::tools::CodeTool::new(
//!     "shout",
//!     "Uppercase the input text.",
//!     serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}},
//!                        "required": ["text"]}),
//!     |args| {
//!         let text = args.get("text").and_then(serde_json::Value::as_str).unwrap_or("");
//!         Ok(serde_json::json!({ "text": text.to_uppercase() }))
//!     },
//! ))
//! .expect("unique tool name");
//! ```
//!
//! Design constraints (binding):
//!
//! - **The registry is process-global and must be populated in `main`, BEFORE
//!   the subagent dispatch.** Subagents re-exec `current_exe()`; the child runs
//!   the embedder's `main` again, which re-registers the same tools — that is
//!   how a tool registered "by code" is visible in every process of the tree
//!   (the exact pattern the stock CLI uses for nothing, preserving its
//!   no-local-code posture: agentd-cli registers zero tools, so this registry
//!   is empty in every stock binary).
//! - **Dispatch priority is self-tools → code tools → MCP.** A registered tool
//!   cannot shadow agentd's own orchestration primitives
//!   ([`SELF_CONTROL_TOOLS`](crate::agentloop::action::SELF_CONTROL_TOOLS) —
//!   registration refuses those names), and a remote MCP server cannot steal a
//!   code tool's calls by publishing a colliding name (the code tool wins the
//!   catalogue slot and the dispatch).
//! - **Workflows address code tools as the reserved server name `code`**
//!   (`{"kind": "tool", "server": "code", "tool": "shout", …}`); config
//!   validation refuses an `--mcp` server named `code`.
//! - Handlers are `Fn(&Value) -> Result<Value, String> + Send + Sync`: they may
//!   be called from the agent loop, from workflow `tool` nodes, and from
//!   parallel foreach/parallel lanes (threads of the same process)
//!   concurrently. Keep them reentrant; hold no lock across a call into agentd.
//! - **Trust:** a code tool is the embedder's own compiled code — it is
//!   first-party by definition, like the binary itself. It sits OUTSIDE the
//!   `--mcp-tags` trifecta accounting (RFC 0012 §3); an embedder whose tool
//!   does egress or touches secrets owns that risk the way it owns the rest of
//!   its binary. (Tagging code tools into the trifecta gate is deferred —
//!   RFC 0022 §7.)

use crate::wire::intel::ToolDef;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// The handler signature: JSON arguments in, JSON result (or a refusal string)
/// out. `Err` takes the tool-error path (the model sees it as a failed call;
/// a workflow `tool` node takes its `error` edge).
pub type CodeToolFn = dyn Fn(&Value) -> Result<Value, String> + Send + Sync;

/// One registered native tool: an MCP-shaped definition (name + description +
/// input JSON Schema) plus the Rust handler.
#[derive(Clone)]
pub struct CodeTool {
    name: String,
    description: String,
    input_schema: Value,
    handler: Arc<CodeToolFn>,
}

impl CodeTool {
    /// Build a tool. `input_schema` is the MCP `inputSchema` the model sees —
    /// give it real properties; the model routes on schemas.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> CodeTool {
        CodeTool {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler: Arc::new(handler),
        }
    }

    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

fn registry() -> &'static RwLock<BTreeMap<String, CodeTool>> {
    static REG: std::sync::OnceLock<RwLock<BTreeMap<String, CodeTool>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Register a tool. Refuses (Err) an empty name, a duplicate, or a name that
/// collides with agentd's own self/control primitives — a code tool may shadow
/// a remote MCP tool (first-party wins) but never the orchestration surface.
pub fn register(tool: CodeTool) -> Result<(), String> {
    if tool.name.trim().is_empty() {
        return Err("code tool name must be non-empty".into());
    }
    if crate::agentloop::action::SELF_CONTROL_TOOLS.contains(&tool.name.as_str()) {
        return Err(format!(
            "code tool {:?} collides with an agentd self/control primitive",
            tool.name
        ));
    }
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    if reg.contains_key(&tool.name) {
        return Err(format!("code tool {:?} is already registered", tool.name));
    }
    reg.insert(tool.name.clone(), tool);
    Ok(())
}

/// Remove a registered tool (dynamic embedders). Returns whether it existed.
/// Prefer registering once in `main` — see the module doc's re-exec rule.
pub fn unregister(name: &str) -> bool {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(name)
        .is_some()
}

/// How many tools are registered (the capabilities manifest surfaces this).
pub fn count() -> usize {
    registry().read().unwrap_or_else(|e| e.into_inner()).len()
}

/// Whether `name` is a registered code tool (the `ToolClass::Code` predicate).
pub(crate) fn is_registered(name: &str) -> bool {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(name)
}

/// The catalogue entries for every registered tool (deterministic order).
pub(crate) fn defs() -> Vec<ToolDef> {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .map(CodeTool::def)
        .collect()
}

/// Dispatch a loop tool call: `None` = not a code tool (fall through to MCP);
/// `Some((content, is_error))` matches the self-handler convention. The handler
/// runs OUTSIDE the registry lock, so a handler may itself consult the registry
/// (and a slow tool never blocks registration reads elsewhere).
pub(crate) fn dispatch(name: &str, args: &Value) -> Option<(String, bool)> {
    let handler = {
        let reg = registry().read().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&reg.get(name)?.handler)
    };
    Some(match handler(args) {
        Ok(v) => (
            match v {
                Value::String(s) => s,
                other => other.to_string(),
            },
            false,
        ),
        Err(e) => (e, true),
    })
}

/// Call a registered tool directly — the PUBLIC entry an embedder's own
/// [`GraphExec`](crate::graph::GraphExec) or dispatcher uses. `None` =
/// unregistered; `Some(Ok(v))` / `Some(Err(reason))` mirror the handler. The
/// handler runs outside the registry lock.
pub fn call(name: &str, args: &Value) -> Option<Result<Value, String>> {
    let handler = {
        let reg = registry().read().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&reg.get(name)?.handler)
    };
    Some(handler(args))
}

/// Dispatch a workflow `tool` node addressed to the reserved server `code`:
/// `(result_value, is_error)` — an unregistered name is an error result (the
/// node's `error` edge), mirroring an unknown MCP server/tool. (The production
/// call site is `graph::exec`, so a non-`workflow` LIB build carries it only
/// for its unit tests — hence the allow.)
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) fn call_for_workflow(name: &str, args: &Value) -> (Value, bool) {
    match call(name, args) {
        None => (
            Value::String(format!("no such code tool {name:?} (register it in main)")),
            true,
        ),
        Some(Ok(v)) => (v, false),
        Some(Err(e)) => (Value::String(e), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // NOTE: the registry is process-global and unit tests share a process —
    // every test uses UNIQUE tool names and cleans up after itself.

    #[test]
    fn register_dispatch_and_unregister_round_trip() {
        register(CodeTool::new(
            "t.echo",
            "echo",
            json!({"type": "object"}),
            |args| Ok(json!({ "got": args.clone() })),
        ))
        .expect("fresh name registers");
        assert!(is_registered("t.echo"));
        assert!(count() >= 1);
        assert_eq!(defs().iter().filter(|d| d.name == "t.echo").count(), 1);

        let (content, is_err) = dispatch("t.echo", &json!({"x": 1})).expect("registered");
        assert!(!is_err);
        assert!(content.contains("\"x\":1"), "{content}");

        let (v, e) = call_for_workflow("t.echo", &json!({"y": 2}));
        assert!(!e);
        assert_eq!(v["got"]["y"], json!(2));

        assert!(unregister("t.echo"));
        assert!(
            dispatch("t.echo", &json!({})).is_none(),
            "gone after unregister"
        );
        let (_, e) = call_for_workflow("t.echo", &json!({}));
        assert!(
            e,
            "workflow call of an unregistered tool is an error result"
        );
    }

    #[test]
    fn registration_refuses_duplicates_empties_and_self_tool_names() {
        register(CodeTool::new("t.dup", "", json!({}), |_| Ok(json!(1)))).unwrap();
        assert!(register(CodeTool::new("t.dup", "", json!({}), |_| Ok(json!(2)))).is_err());
        assert!(register(CodeTool::new("  ", "", json!({}), |_| Ok(json!(1)))).is_err());
        assert!(
            register(CodeTool::new("subagent.spawn", "", json!({}), |_| Ok(
                json!(1)
            )))
            .is_err(),
            "self/control primitives are unshadowable"
        );
        assert!(unregister("t.dup"));
    }

    #[test]
    fn a_handler_error_is_a_tool_error_not_a_panic() {
        register(CodeTool::new("t.fail", "", json!({}), |_| {
            Err("deliberate".into())
        }))
        .unwrap();
        let (content, is_err) = dispatch("t.fail", &json!({})).unwrap();
        assert!(is_err);
        assert_eq!(content, "deliberate");
        let (v, e) = call_for_workflow("t.fail", &json!({}));
        assert!(e);
        assert_eq!(v, json!("deliberate"));
        assert!(unregister("t.fail"));
    }
}
