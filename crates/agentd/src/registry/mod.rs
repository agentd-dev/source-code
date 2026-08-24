// SPDX-License-Identifier: AGPL-3.0-only
//! The **tool registry**: one registry serving the root agent,
//! workflow steps and subagents, with three tiers and dispatch precedence
//! **internal > code > MCP**. Every tool carries JSON Schemas for input and
//! output and a **grant** (who may call it). Internal tools are contracts with
//! a built-in implementation by default, **overridable** by a mapped MCP tool
//! (`tools.overrides`) and **disable-able** (`tools.disabled`); mapping-only
//! contracts (`code.run`, `knowledge.*`, `search.*`) are unavailable until
//! mapped (or until a server advertises the profile's tool names).
//!
//! The registry knows *what* a tool is and *where* it goes ([`Route`]); the
//! runtime executes built-ins (they mutate runtime state), the turn worker
//! calls MCP tools itself, and mapped tools are executed by whoever holds the
//! server connection ([`Registry::map_args`] / [`Registry::map_result`]).

pub mod internal;

use crate::config::v2::{Role, Settings, ToolOverride};
use crate::jsonschema;
use crate::sec::scope::TrifectaTag;
use crate::store::mapping::{self, Vars};
use crate::wire::intel::ToolDef;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The tier a tool belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolClass {
    Internal,
    Code,
    Mcp,
}

/// An override mapping: which server's tool stands in for an internal
/// contract, plus the optional argument and result transforms that reconcile
/// the two shapes. Both transforms are compiled at startup, so a mapping that
/// cannot be applied is a config error rather than a call-time surprise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    pub server: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// How a tool is implemented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "impl", rename_all = "snake_case")]
pub enum Impl {
    /// A built-in executed by the runtime.
    BuiltIn,
    /// A contract with no implementation until an override maps it.
    MappingOnly,
    /// An internal contract implemented by a mapped MCP tool.
    Mapped(Mapping),
    /// A tool registered from Rust by an embedding application.
    Code,
    /// An MCP server tool.
    Mcp { server: String, tool: String },
}

/// Who may call a tool. These flags gate the INTERNAL tools — the ones that
/// reach agentd's own state — so a caller they do not name is refused. Code
/// and MCP tools are not gated by them: an operator who wired a server in has
/// already made that decision, and a subagent narrowed by an explicit `allow`
/// list is held to that list instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub root: bool,
    pub workflows: bool,
    pub subagents: bool,
    /// A2A roles granted by default (`user`, `agent`; operator always).
    pub roles: Vec<Role>,
}

impl Grant {
    fn all() -> Grant {
        Grant {
            root: true,
            workflows: true,
            subagents: true,
            roles: Vec::new(),
        }
    }
}

/// One registered tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub class: ToolClass,
    #[serde(rename = "implementation")]
    pub imp: Impl,
    pub grant: Grant,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub family: String,
    /// Trifecta tags inherited from the serving MCP server (mapped/MCP tools).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<TrifectaTag>,
    /// The server that serves this tool (MCP / mapped), for `_meta` + status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

impl ToolSpec {
    /// Callable at all (not disabled, has an implementation).
    pub fn is_available(&self) -> bool {
        !self.disabled && !matches!(self.imp, Impl::MappingOnly)
    }
    pub fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

/// A connected MCP server's advertised tools (input to [`Registry::build`]).
#[derive(Debug, Clone, Default)]
pub struct ServerTools {
    pub name: String,
    pub ns: Option<String>,
    pub tags: Vec<TrifectaTag>,
    pub tools: Vec<::mcp::wire::Tool>,
}

/// Where a call goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Route<'a> {
    /// A built-in: the runtime executes it.
    Internal,
    /// A mapped internal contract: call `mapping.tool` on `mapping.server`.
    Mapped(&'a Mapping),
    /// A code-registered tool.
    Code,
    /// An MCP tool.
    Mcp { server: &'a str, tool: &'a str },
}

/// The caller asking for definitions / permission.
#[derive(Debug, Clone, PartialEq)]
pub enum Caller<'a> {
    Root,
    Workflow,
    /// A subagent, optionally restricted to an explicit allow-list (`subagent.run.tools`).
    Subagent {
        allow: Option<&'a [String]>,
    },
    /// An A2A principal with a role and its explicit grants (patterns).
    Principal {
        role: Role,
        grants: &'a [String],
    },
}

/// The registry.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    tools: BTreeMap<String, ToolSpec>,
    servers: Vec<String>,
    /// Non-fatal findings from the build (collisions, missing profile tools).
    pub warnings: Vec<String>,
}

impl Registry {
    /// Build from settings and the connected servers' tool lists.
    ///
    /// Every override is resolved here, at startup, so a misconfiguration
    /// fails to boot rather than failing on the call that needed the tool.
    /// The errors returned are: an override naming an unknown internal tool,
    /// a server that is not declared, a tool the server does not advertise, a
    /// mapping expression that does not compile, a tool that is both disabled
    /// and overridden, and disabling a tool that does not exist.
    pub fn build(settings: &Settings, servers: &[ServerTools]) -> Result<Registry, Vec<String>> {
        let mut reg = Registry::default();
        let mut errors = Vec::new();
        // 1. Internal contracts.
        for c in internal::contracts() {
            // `exec` runs LOCAL commands, so it is off at two independent
            // layers: the binary must be built `--features exec` AND
            // `security.exec.enabled` must be set. Failing either, it is
            // mapping-only, so execution can still be delegated off-box
            // through `tools.overrides` without agentd ever running code
            // itself. It is always
            // tagged `sensitive` + `egress` so the Rule-of-Two gate refuses to
            // combine it with untrusted input.
            let (imp, tags) = if c.name == "exec" {
                let local = cfg!(feature = "exec") && settings.security.exec.enabled;
                (
                    if local {
                        Impl::BuiltIn
                    } else {
                        Impl::MappingOnly
                    },
                    vec![TrifectaTag::Sensitive, TrifectaTag::Egress],
                )
            } else if c.builtin {
                (Impl::BuiltIn, Vec::new())
            } else {
                (Impl::MappingOnly, Vec::new())
            };
            reg.tools.insert(
                c.name.to_string(),
                ToolSpec {
                    name: c.name.to_string(),
                    description: c.description.to_string(),
                    input_schema: c.input,
                    output_schema: Some(c.output),
                    class: ToolClass::Internal,
                    imp,
                    grant: Grant {
                        root: c.grant.root,
                        workflows: c.grant.workflows,
                        subagents: c.grant.subagents,
                        roles: [(c.grant.user, Role::User), (c.grant.agent, Role::Agent)]
                            .into_iter()
                            .filter(|(on, _)| *on)
                            .map(|(_, r)| r)
                            .collect(),
                    },
                    disabled: false,
                    family: c.family.to_string(),
                    tags,
                    server: None,
                },
            );
        }
        // 2. Code-registered tools (never shadow internal names — registration
        //    already refuses them, defensively skip anyway).
        for d in crate::tools::defs() {
            if reg.tools.contains_key(&d.name) {
                reg.warnings.push(format!(
                    "code tool {:?} collides with an internal tool and is ignored",
                    d.name
                ));
                continue;
            }
            reg.tools.insert(
                d.name.clone(),
                ToolSpec {
                    name: d.name.clone(),
                    description: d.description.clone(),
                    input_schema: d.input_schema.clone(),
                    output_schema: None,
                    class: ToolClass::Code,
                    imp: Impl::Code,
                    grant: Grant::all(),
                    disabled: false,
                    family: "code".into(),
                    tags: Vec::new(),
                    server: Some("code".into()),
                },
            );
        }
        // 3. MCP tools: `<ns>.<tool>` when the server declares `ns`; else the
        //    bare name unless it collides, then `<server>.<tool>`. A profile
        //    tool (`knowledge.*`, `search.*`, `code.run`) advertised by the
        //    configured profile server BECOMES that contract's implementation.
        let profile_servers: BTreeMap<&str, &str> = [
            ("knowledge", settings.knowledge.server.as_deref()),
            ("search", settings.search.server.as_deref()),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect();
        for srv in servers {
            reg.servers.push(srv.name.clone());
            // Per-server admission control (`mcp.servers[].allow`/`exclude`),
            // on the ADVERTISED name: a tool the operator excluded never
            // exists here — not disabled, absent — so nothing downstream
            // (defs, grants, overrides) can resurrect it.
            let gate = settings.mcp.servers.iter().find(|s| s.name == srv.name);
            for t in &srv.tools {
                if let Some(g) = gate {
                    let allowed = g
                        .allow
                        .as_ref()
                        .is_none_or(|a| a.iter().any(|p| pattern_matches(p, &t.name)));
                    let excluded = g.exclude.iter().any(|p| pattern_matches(p, &t.name));
                    if !allowed || excluded {
                        continue;
                    }
                }
                let profile_family = t.name.split('.').next().unwrap_or("");
                let is_profile = reg
                    .tools
                    .get(&t.name)
                    .is_some_and(|s| matches!(s.imp, Impl::MappingOnly))
                    && profile_servers
                        .get(profile_family)
                        .is_some_and(|ps| *ps == srv.name);
                if is_profile {
                    let spec = reg.tools.get_mut(&t.name).expect("checked");
                    spec.imp = Impl::Mapped(Mapping {
                        server: srv.name.clone(),
                        tool: t.name.clone(),
                        args: None,
                        result: None,
                    });
                    spec.tags = srv.tags.clone();
                    spec.server = Some(srv.name.clone());
                    continue;
                }
                let name = match &srv.ns {
                    Some(ns) if !ns.is_empty() => format!("{ns}.{}", t.name),
                    _ => {
                        if reg.tools.contains_key(&t.name) {
                            let q = format!("{}.{}", srv.name, t.name);
                            reg.warnings.push(format!(
                                "mcp tool {:?} of server {:?} collides; registered as {q:?}",
                                t.name, srv.name
                            ));
                            q
                        } else {
                            t.name.clone()
                        }
                    }
                };
                if reg.tools.contains_key(&name) {
                    reg.warnings.push(format!("mcp tool {name:?} of server {:?} collides with an existing tool and is ignored", srv.name));
                    continue;
                }
                reg.tools.insert(
                    name.clone(),
                    ToolSpec {
                        name,
                        description: t.description.clone().unwrap_or_default(),
                        input_schema: t.input_schema.clone(),
                        output_schema: t.output_schema.clone(),
                        class: ToolClass::Mcp,
                        imp: Impl::Mcp {
                            server: srv.name.clone(),
                            tool: t.name.clone(),
                        },
                        grant: Grant::all(),
                        disabled: false,
                        family: "mcp".into(),
                        tags: srv.tags.clone(),
                        server: Some(srv.name.clone()),
                    },
                );
            }
        }
        // 4. Overrides.
        for (name, ov) in &settings.tools.overrides {
            match reg.apply_override(name, ov, servers) {
                Ok(()) => {}
                Err(e) => errors.push(e),
            }
        }
        // 5. Disabled.
        for name in &settings.tools.disabled {
            if settings.tools.overrides.contains_key(name) {
                errors.push(format!(
                    "tools.disabled and tools.overrides both name {name:?}"
                ));
                continue;
            }
            match reg.tools.get_mut(name) {
                Some(t) => t.disabled = true,
                None => errors.push(format!("tools.disabled names an unknown tool {name:?}")),
            }
        }
        if errors.is_empty() {
            Ok(reg)
        } else {
            Err(errors)
        }
    }

    fn apply_override(
        &mut self,
        name: &str,
        ov: &ToolOverride,
        servers: &[ServerTools],
    ) -> Result<(), String> {
        let spec = self
            .tools
            .get(name)
            .ok_or_else(|| format!("tools.overrides.{name}: unknown internal tool"))?;
        if spec.class != ToolClass::Internal {
            return Err(format!(
                "tools.overrides.{name}: only internal tools can be overridden ({name} is {:?})",
                spec.class
            ));
        }
        let srv = servers
            .iter()
            .find(|s| s.name == ov.server)
            .ok_or_else(|| {
                format!(
                    "tools.overrides.{name}: server {:?} is not a connected MCP server",
                    ov.server
                )
            })?;
        if !srv.tools.iter().any(|t| t.name == ov.tool) {
            return Err(format!(
                "tools.overrides.{name}: server {:?} does not advertise tool {:?}",
                ov.server, ov.tool
            ));
        }
        // The mapping must compile: render against sample vars.
        let mut vars = Vars::new();
        vars.insert("args".into(), json!({}));
        vars.insert("ctx".into(), json!({"instance": "x"}));
        if let Some(a) = &ov.args
            && !a.trim_start().starts_with("CEL:")
        {
            // A JSON template with unknown placeholders would fail on `args.<field>`
            // lookups against an empty args object; only check syntax here by
            // rendering with a permissive args object built from the schema.
            let sample = sample_args(&spec.input_schema);
            vars.insert("args".into(), sample);
            mapping::render_json(a, &vars)
                .map_err(|e| format!("tools.overrides.{name}.args: {e}"))?;
        }
        if let Some(a) = &ov.args
            && a.trim_start().starts_with("CEL:")
        {
            crate::cel::compile_check(a.trim_start().trim_start_matches("CEL:").trim())
                .map_err(|e| format!("tools.overrides.{name}.args: {e}"))?;
        }
        if let Some(r) = &ov.result
            && r.trim_start().starts_with("CEL:")
        {
            crate::cel::compile_check(r.trim_start().trim_start_matches("CEL:").trim())
                .map_err(|e| format!("tools.overrides.{name}.result: {e}"))?;
        }
        let tags = srv.tags.clone();
        let server = ov.server.clone();
        let spec = self.tools.get_mut(name).expect("checked");
        spec.imp = Impl::Mapped(Mapping {
            server: ov.server.clone(),
            tool: ov.tool.clone(),
            args: ov.args.clone(),
            result: ov.result.clone(),
        });
        spec.tags = tags;
        spec.server = Some(server);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
    pub fn servers(&self) -> &[String] {
        &self.servers
    }
    pub fn len(&self) -> usize {
        self.tools.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.values()
    }

    /// Whether `caller` may call `name`.
    ///
    /// Fails closed at every step: an unknown tool, or one that is disabled or
    /// still unmapped, is refused before any grant is consulted; an anonymous
    /// A2A principal is refused outright; and a subagent carrying an explicit
    /// `allow` list is held to it alone, so narrowing a child can only ever
    /// remove reach, never restore it through a default.
    pub fn allowed(&self, caller: &Caller, name: &str) -> bool {
        let Some(t) = self.tools.get(name) else {
            return false;
        };
        if !t.is_available() {
            return false;
        }
        match caller {
            Caller::Root => t.grant.root || t.class != ToolClass::Internal,
            Caller::Workflow => t.grant.workflows || t.class != ToolClass::Internal,
            Caller::Subagent { allow } => match allow {
                Some(list) => list.iter().any(|p| pattern_matches(p, name)),
                None => t.grant.subagents || t.class != ToolClass::Internal,
            },
            Caller::Principal { role, grants } => match role {
                Role::Operator => true,
                Role::Anonymous => false,
                r => {
                    name == "status"
                        || t.grant.roles.contains(r)
                        || grants.iter().any(|p| pattern_matches(p, name))
                }
            },
        }
    }

    /// The LLM-facing definitions for a caller, filtered by the agent's tool
    /// selection (`agent.tools.internal|mcp|code`) when given.
    pub fn defs_for(
        &self,
        caller: &Caller,
        select: Option<&crate::config::v2::AgentTools>,
    ) -> Vec<ToolDef> {
        self.tools
            .values()
            .filter(|t| self.allowed(caller, &t.name))
            .filter(|t| match select {
                None => true,
                Some(sel) => match t.class {
                    ToolClass::Internal => {
                        sel.internal.allows(&t.name) || sel.internal.allows(&t.family)
                    }
                    ToolClass::Code => sel.code.allows(&t.name),
                    ToolClass::Mcp => {
                        sel.mcp.allows(&t.name)
                            || t.server.as_deref().is_some_and(|s| sel.mcp.allows(s))
                    }
                },
            })
            .map(ToolSpec::def)
            .collect()
    }

    /// Validate call arguments against the tool's input schema.
    pub fn validate_args(&self, name: &str, args: &Value) -> Result<(), String> {
        let t = self
            .tools
            .get(name)
            .ok_or_else(|| format!("no such tool {name:?}"))?;
        jsonschema::validate(&t.input_schema, args)
            .map_err(|e| format!("invalid arguments for {name}: {}", jsonschema::explain(&e)))
    }

    /// Validate a result against the tool's output schema (when it has one).
    pub fn validate_result(&self, name: &str, result: &Value) -> Result<(), String> {
        let t = self
            .tools
            .get(name)
            .ok_or_else(|| format!("no such tool {name:?}"))?;
        match &t.output_schema {
            None => Ok(()),
            Some(schema) => jsonschema::validate(schema, result).map_err(|e| {
                format!(
                    "result of {name} does not match its output schema: {}",
                    jsonschema::explain(&e)
                )
            }),
        }
    }

    /// Where a call goes (`None` = unknown or unavailable).
    pub fn route(&self, name: &str) -> Option<Route<'_>> {
        let t = self.tools.get(name)?;
        if !t.is_available() {
            return None;
        }
        Some(match &t.imp {
            Impl::BuiltIn => Route::Internal,
            Impl::MappingOnly => return None,
            Impl::Mapped(m) => Route::Mapped(m),
            Impl::Code => Route::Code,
            Impl::Mcp { server, tool } => Route::Mcp { server, tool },
        })
    }

    /// Render a mapped tool's MCP arguments from the internal call's `args`
    /// and the call context (`{instance, run?, ctx?, principal?}`). Without an
    /// `args` template the internal args pass through unchanged.
    pub fn map_args(m: &Mapping, args: &Value, ctx: &Value) -> Result<Value, String> {
        match &m.args {
            None => Ok(args.clone()),
            Some(t) => {
                let mut vars = Vars::new();
                vars.insert("args".into(), args.clone());
                vars.insert("ctx".into(), ctx.clone());
                mapping::render_json(t, &vars).map_err(|e| format!("override args mapping: {e}"))
            }
        }
    }

    /// Map an MCP `CallToolResult` (as the `{"result": …}` context the store
    /// adapter also uses) back to the internal output. Without a `result`
    /// template: `structuredContent`, else the text parsed as JSON, else the text.
    pub fn map_result(m: &Mapping, result_ctx: &Value) -> Result<Value, String> {
        match &m.result {
            None => {
                let sc = &result_ctx["result"]["structuredContent"];
                if !sc.is_null() {
                    return Ok(sc.clone());
                }
                let text = result_ctx["result"]["text"].as_str().unwrap_or("");
                Ok(serde_json::from_str::<Value>(text)
                    .unwrap_or_else(|_| Value::String(text.to_string())))
            }
            Some(t) => {
                let t = t.trim();
                if t.starts_with("CEL:") {
                    return mapping::extract(t, result_ctx)
                        .map_err(|e| format!("override result mapping: {e}"))?
                        .ok_or_else(|| "override result mapping produced nothing".into());
                }
                // A JSON template with `{{result.…}}`/`{result.…}` placeholders,
                // or a bare path.
                if t.starts_with('{') && !t.starts_with("{{") && !t.starts_with("{result") {
                    let vars: Vars = match result_ctx {
                        Value::Object(o) => mapping::vars_from(o),
                        _ => Vars::new(),
                    };
                    return mapping::render_json(t, &vars)
                        .map_err(|e| format!("override result mapping: {e}"));
                }
                if t.starts_with("{{") {
                    let vars: Vars = match result_ctx {
                        Value::Object(o) => mapping::vars_from(o),
                        _ => Vars::new(),
                    };
                    return mapping::render_json(t, &vars)
                        .map_err(|e| format!("override result mapping: {e}"));
                }
                mapping::extract(t, result_ctx)
                    .map_err(|e| format!("override result mapping: {e}"))?
                    .ok_or_else(|| {
                        format!("override result mapping: path {t:?} not found in the result")
                    })
            }
        }
    }

    /// The trifecta tags a set of tool names carries (for the gate).
    pub fn tags_of(&self, names: &[String]) -> Vec<TrifectaTag> {
        let mut out: Vec<TrifectaTag> = Vec::new();
        for t in names.iter().filter_map(|n| self.tools.get(n)) {
            for tag in &t.tags {
                if !out.contains(tag) {
                    out.push(*tag);
                }
            }
        }
        out
    }

    /// A status view (`agent://tools`).
    pub fn status(&self) -> Value {
        json!({
            "count": self.tools.len(),
            "servers": self.servers,
            "warnings": self.warnings,
            "tools": self.tools.values().map(|t| json!({
                "name": t.name, "class": t.class, "impl": t.imp, "disabled": t.disabled,
                "available": t.is_available(), "server": t.server, "family": t.family,
            })).collect::<Vec<_>>(),
        })
    }
}

/// `memory.*` / `workflow.run` / `*` style pattern match.
pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    let p = pattern.trim();
    if p == "*" || p == name {
        return true;
    }
    if let Some(prefix) = p.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    false
}

/// A permissive sample of `schema`'s properties (strings/empty values) so an
/// args template's `{{args.x}}` placeholders resolve at compile-check time.
fn sample_args(schema: &Value) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (k, p) in props {
            let v = match p.get("type").and_then(Value::as_str) {
                Some("integer") | Some("number") => json!(0),
                Some("boolean") => json!(false),
                Some("array") => json!([]),
                Some("object") => json!({}),
                _ => json!(""),
            };
            m.insert(k.clone(), v);
        }
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::mcp::wire::Tool;

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            title: None,
            description: Some(format!("{name} tool")),
            input_schema: json!({"type": "object"}),
            output_schema: None,
        }
    }

    fn settings(doc: Value) -> Settings {
        Settings::from_document(doc, "test").unwrap()
    }

    #[test]
    fn exec_is_default_off_mapping_only_and_tagged() {
        // Default (no `security.exec`): `exec` is a mapping-only contract — not
        // available unless mapped off-box — and always carries the sensitive +
        // egress trifecta tags (so the Rule-of-Two gate constrains it).
        let reg = Registry::build(&settings(json!({})), &[]).unwrap();
        let exec = reg.get("exec").expect("exec contract exists");
        assert!(
            !exec.is_available(),
            "exec is off by default (mapping-only)"
        );
        assert!(
            exec.tags.contains(&TrifectaTag::Sensitive) && exec.tags.contains(&TrifectaTag::Egress),
            "exec is tagged sensitive+egress: {:?}",
            exec.tags
        );

        // `security.exec.enabled` flips it to a LOCAL built-in — but only in a
        // binary built with the `exec` feature (off-by-default at BOTH layers).
        let on = settings(json!({"security": {"exec": {"enabled": true, "allow": ["echo"]}}}));
        let reg = Registry::build(&on, &[]).unwrap();
        assert_eq!(
            reg.get("exec").unwrap().is_available(),
            cfg!(feature = "exec"),
            "enabled → available iff built with --features exec",
        );
    }

    #[test]
    fn per_server_allow_and_exclude_gate_advertised_tools() {
        let s = settings(json!({"mcp": {"servers": [{
            "name": "fs", "endpoint": "https://fs.example",
            "allow": ["read_*", "list"], "exclude": ["read_secrets"]
        }]}}));
        let servers = vec![ServerTools {
            name: "fs".into(),
            ns: None,
            tags: vec![],
            tools: vec![
                tool("read_file"),
                tool("read_secrets"),
                tool("list"),
                tool("delete_everything"),
            ],
        }];
        let reg = Registry::build(&s, &servers).unwrap();
        assert!(reg.get("read_file").is_some(), "matches allow");
        assert!(reg.get("list").is_some(), "exact allow");
        assert!(
            reg.get("read_secrets").is_none(),
            "exclude beats allow — the tool does not exist, not merely disabled"
        );
        assert!(reg.get("delete_everything").is_none(), "not in allow");
    }

    #[test]
    fn precedence_namespaces_and_collisions() {
        let servers = vec![
            ServerTools {
                name: "fs".into(),
                ns: Some("fs".into()),
                tags: vec![TrifectaTag::Sensitive],
                tools: vec![tool("read"), tool("write")],
            },
            ServerTools {
                name: "misc".into(),
                ns: None,
                tags: vec![],
                tools: vec![tool("echo"), tool("memory.get"), tool("read")],
            },
            ServerTools {
                name: "misc2".into(),
                ns: None,
                tags: vec![],
                tools: vec![tool("echo")],
            },
        ];
        let reg =
            Registry::build(&settings(json!({"agent": {"instruction": "x"}})), &servers).unwrap();
        assert!(reg.get("fs.read").is_some(), "namespaced");
        assert!(
            reg.get("read").is_some(),
            "bare name of a server without ns"
        );
        assert_eq!(reg.get("echo").unwrap().server.as_deref(), Some("misc"));
        assert!(
            reg.get("misc2.echo").is_some(),
            "second server's colliding tool is server-qualified"
        );
        assert_eq!(
            reg.get("memory.get").unwrap().class,
            ToolClass::Internal,
            "internal wins over MCP"
        );
        assert!(
            reg.get("misc.memory.get").is_some(),
            "the MCP one is reachable qualified"
        );
        assert_eq!(
            reg.get("fs.read").unwrap().tags,
            vec![TrifectaTag::Sensitive]
        );
        assert!(matches!(
            reg.route("fs.read"),
            Some(Route::Mcp {
                server: "fs",
                tool: "read"
            })
        ));
        assert!(matches!(reg.route("memory.get"), Some(Route::Internal)));
        assert!(
            reg.route("code.run").is_none(),
            "mapping-only without a mapping is unavailable"
        );
        assert!(reg.warnings.iter().any(|w| w.contains("collides")));
    }

    #[test]
    fn overrides_disabled_and_profiles() {
        let servers = vec![
            ServerTools {
                name: "mem".into(),
                ns: None,
                tags: vec![],
                tools: vec![tool("search")],
            },
            ServerTools {
                name: "kb".into(),
                ns: None,
                tags: vec![TrifectaTag::UntrustedInput],
                tools: vec![tool("knowledge.search"), tool("knowledge.get")],
            },
            ServerTools {
                name: "sandbox".into(),
                ns: None,
                tags: vec![],
                tools: vec![tool("execute")],
            },
        ];
        let s = settings(json!({
            "agent": {"instruction": "x"},
            "knowledge": {"server": "kb"},
            "tools": {
                "disabled": ["workflow.delete"],
                "overrides": {
                    "memory.get": {"server": "mem", "tool": "search", "args": "{\"query\": \"{{args.key}}\", \"limit\": 1}", "result": "{\"found\": true, \"value\": {{result.structuredContent.results.0.text}}}"},
                    "code.run": {"server": "sandbox", "tool": "execute", "args": "{\"lang\": \"{{args.language}}\", \"code\": \"{{args.code}}\"}"}
                }
            }
        }));
        let reg = Registry::build(&s, &servers).unwrap();
        // The override kept the contract, swapped the implementation.
        let mg = reg.get("memory.get").unwrap();
        assert_eq!(mg.class, ToolClass::Internal);
        let Some(Route::Mapped(m)) = reg.route("memory.get") else {
            panic!("mapped")
        };
        assert_eq!(m.tool, "search");
        let args =
            Registry::map_args(m, &json!({"key": "user/name"}), &json!({"instance": "i"})).unwrap();
        assert_eq!(args, json!({"query": "user/name", "limit": 1}));
        let out = Registry::map_result(m, &json!({"result": {"structuredContent": {"results": [{"text": "andrii"}]}, "isError": false, "text": ""}})).unwrap();
        assert_eq!(out, json!({"found": true, "value": "andrii"}));
        assert!(reg.validate_result("memory.get", &out).is_ok());
        // Mapping-only code.run is now available; knowledge.* got the profile server.
        assert!(matches!(reg.route("code.run"), Some(Route::Mapped(_))));
        let Some(Route::Mapped(k)) = reg.route("knowledge.search") else {
            panic!("profile mapped")
        };
        assert_eq!(k.server, "kb");
        assert_eq!(
            reg.get("knowledge.search").unwrap().tags,
            vec![TrifectaTag::UntrustedInput]
        );
        assert!(
            reg.route("knowledge.list").is_none(),
            "not advertised ⇒ still unavailable"
        );
        // Default result mapping prefers structuredContent, then text-JSON, then text.
        let plain = Mapping {
            server: "s".into(),
            tool: "t".into(),
            args: None,
            result: None,
        };
        assert_eq!(
            Registry::map_result(
                &plain,
                &json!({"result": {"structuredContent": {"a": 1}, "text": "x"}})
            )
            .unwrap(),
            json!({"a": 1})
        );
        assert_eq!(
            Registry::map_result(
                &plain,
                &json!({"result": {"structuredContent": null, "text": "{\"b\": 2}"}})
            )
            .unwrap(),
            json!({"b": 2})
        );
        assert_eq!(
            Registry::map_result(
                &plain,
                &json!({"result": {"structuredContent": null, "text": "hello"}})
            )
            .unwrap(),
            json!("hello")
        );
        // Disabled.
        assert!(reg.get("workflow.delete").unwrap().disabled);
        assert!(reg.route("workflow.delete").is_none());
        assert!(!reg.allowed(&Caller::Root, "workflow.delete"));
        // Errors.
        let bad = settings(json!({"agent": {"instruction": "x"}, "tools": {
            "disabled": ["nope", "memory.get"],
            "overrides": {"memory.get": {"server": "mem", "tool": "search"}, "fs.read": {"server": "mem", "tool": "search"}, "memory.set": {"server": "ghost", "tool": "x"}, "memory.list": {"server": "mem", "tool": "missing"}}
        }}));
        let errs = Registry::build(&bad, &servers).unwrap_err();
        let joined = errs.join("\n");
        assert!(joined.contains("unknown tool \"nope\""), "{joined}");
        assert!(joined.contains("both name \"memory.get\""), "{joined}");
        assert!(
            joined.contains("fs.read: unknown internal tool"),
            "{joined}"
        );
        assert!(joined.contains("\"ghost\" is not a connected"), "{joined}");
        assert!(
            joined.contains("does not advertise tool \"missing\""),
            "{joined}"
        );
    }

    #[test]
    fn grants_and_definitions_per_caller() {
        let servers = vec![ServerTools {
            name: "fs".into(),
            ns: None,
            tags: vec![],
            tools: vec![tool("read")],
        }];
        let s = settings(
            json!({"agent": {"instruction": "x", "tools": {"internal": ["memory", "plan.get", "finish"], "mcp": "all"}}}),
        );
        let reg = Registry::build(&s, &servers).unwrap();
        assert!(reg.allowed(&Caller::Root, "subagent.run"));
        assert!(!reg.allowed(&Caller::Workflow, "finish"));
        assert!(reg.allowed(&Caller::Workflow, "memory.set"));
        assert!(reg.allowed(&Caller::Subagent { allow: None }, "memory.get"));
        assert!(!reg.allowed(&Caller::Subagent { allow: None }, "subagent.run"));
        assert!(reg.allowed(
            &Caller::Subagent {
                allow: Some(&["memory.*".to_string()])
            },
            "memory.list"
        ));
        assert!(!reg.allowed(
            &Caller::Subagent {
                allow: Some(&["memory.*".to_string()])
            },
            "read"
        ));
        assert!(reg.allowed(
            &Caller::Principal {
                role: Role::User,
                grants: &[]
            },
            "status"
        ));
        assert!(!reg.allowed(
            &Caller::Principal {
                role: Role::User,
                grants: &[]
            },
            "workflow.run"
        ));
        assert!(reg.allowed(
            &Caller::Principal {
                role: Role::User,
                grants: &["workflow.*".to_string()]
            },
            "workflow.run"
        ));
        assert!(reg.allowed(
            &Caller::Principal {
                role: Role::Operator,
                grants: &[]
            },
            "workflow.delete"
        ));
        assert!(!reg.allowed(
            &Caller::Principal {
                role: Role::Anonymous,
                grants: &["*".to_string()]
            },
            "status"
        ));
        // Root definitions honour agent.tools selection: family + explicit names + all MCP.
        let defs = reg.defs_for(&Caller::Root, Some(&s.agent.tools));
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"memory.get") && names.contains(&"memory.set"));
        assert!(names.contains(&"plan.get") && !names.contains(&"plan.update"));
        assert!(names.contains(&"finish"));
        assert!(names.contains(&"read"));
        assert!(!names.contains(&"subagent.run"));
        // Argument validation.
        assert!(
            reg.validate_args("memory.get", &json!({"key": "k"}))
                .is_ok()
        );
        let e = reg
            .validate_args("memory.get", &json!({"ke": "k"}))
            .unwrap_err();
        assert!(
            e.contains("missing required property \"key\"")
                && e.contains("unknown property \"ke\""),
            "{e}"
        );
        assert!(reg.validate_args("nope", &json!({})).is_err());
    }
}
