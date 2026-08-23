// SPDX-License-Identifier: AGPL-3.0-only
//! **Skills** (RFC 0028 §7): named instruction bundles — the SKILL.md idiom —
//! discovered from MCP servers as **prompts** (`prompts/list` = catalogue,
//! `prompts/get` = body) or **resources** (`skill://<name>` URIs or
//! `mimeType: text/x-skill+markdown`), referenced as `@skill:<name>` in the
//! instruction, a step, or a chat message, and **preloaded** into the calling
//! context (progressive disclosure: the catalogue is always visible, a body
//! only when referenced or `skills.load`ed). Bodies are cached by hash, never
//! stored; the loaded set `[{name, hash}]` is part of the context record.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The default reference prefix (`skills.reference_prefix`).
pub const DEFAULT_PREFIX: &str = "@skill:";
/// The URI scheme skills-as-resources use.
pub const SKILL_SCHEME: &str = "skill://";
/// The mime type that marks a resource as a skill.
pub const SKILL_MIME: &str = "text/x-skill+markdown";

/// A catalogue entry (no body).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<Value>,
    pub source: SkillSourceRef,
}

/// Where a skill comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSourceRef {
    pub server: String,
    #[serde(rename = "kind")]
    pub kind: SkillSourceKind,
    /// The prompt name or the resource URI.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The body, for [`SkillSourceKind::Inline`] only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSourceKind {
    Prompt,
    Resource,
    /// Defined by a `:::skill` directive in the instruction; the body lives on
    /// the meta — no server round trip, no server at all.
    Inline,
}

/// A loaded skill body (cached by hash).
#[derive(Debug, Clone, PartialEq)]
pub struct SkillBody {
    pub name: String,
    pub hash: String,
    pub body: String,
}

/// The MCP surface skill discovery needs — implemented for the MCP client and
/// by test fakes.
pub trait SkillServer {
    fn server_name(&self) -> String;
    fn supports_prompts(&self) -> bool;
    fn supports_resources(&self) -> bool;
    fn list_prompts(&self) -> Result<Vec<::mcp::wire::Prompt>, String>;
    fn get_prompt(&self, name: &str, arguments: Option<Value>) -> Result<Vec<Value>, String>;
    fn list_resources(&self) -> Result<Vec<::mcp::wire::Resource>, String>;
    fn read_resource(&self, uri: &str) -> Result<String, String>;
}

impl SkillServer for crate::mcp::client::McpClient {
    fn server_name(&self) -> String {
        self.name().to_string()
    }
    fn supports_prompts(&self) -> bool {
        self.capabilities().supports_prompts()
    }
    fn supports_resources(&self) -> bool {
        self.capabilities().supports_resources()
    }
    fn list_prompts(&self) -> Result<Vec<::mcp::wire::Prompt>, String> {
        crate::mcp::client::McpClient::list_prompts(self).map_err(|e| e.to_string())
    }
    fn get_prompt(&self, name: &str, arguments: Option<Value>) -> Result<Vec<Value>, String> {
        crate::mcp::client::McpClient::get_prompt(self, name, arguments)
            .map(|r| r.messages)
            .map_err(|e| e.to_string())
    }
    fn list_resources(&self) -> Result<Vec<::mcp::wire::Resource>, String> {
        crate::mcp::client::McpClient::list_resources(self).map_err(|e| e.to_string())
    }
    fn read_resource(&self, uri: &str) -> Result<String, String> {
        crate::mcp::client::McpClient::read_resource(self, uri)
            .map(|r| r.text())
            .map_err(|e| e.to_string())
    }
}

/// How to discover on one source (`skills.sources[].discover`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discover {
    Prompts,
    Resources,
    Auto,
}

/// The catalogue + body cache.
#[derive(Debug, Default)]
pub struct Catalogue {
    skills: BTreeMap<String, SkillMeta>,
    bodies: BTreeMap<String, SkillBody>, // by hash
    max_bytes: usize,
    pub prefix: String,
    /// Discovery errors per server (surfaced in status, never fatal).
    pub errors: BTreeMap<String, String>,
}

impl Catalogue {
    pub fn new(prefix: &str, max_bytes: usize) -> Catalogue {
        Catalogue {
            prefix: prefix.to_string(),
            max_bytes,
            ..Default::default()
        }
    }

    /// (Re)discover the skills of one server. Later sources do not override
    /// an existing name (first source wins; a collision is logged by the caller).
    /// Returns the names discovered on this server.
    pub fn discover(
        &mut self,
        server: &dyn SkillServer,
        mode: Discover,
        filter: Option<&str>,
    ) -> Vec<String> {
        let name = server.server_name();
        let mut found = Vec::new();
        let want_prompts =
            matches!(mode, Discover::Prompts | Discover::Auto) && server.supports_prompts();
        let want_resources =
            matches!(mode, Discover::Resources | Discover::Auto) && server.supports_resources();
        if want_prompts {
            match server.list_prompts() {
                Ok(prompts) => {
                    for p in prompts {
                        if !passes(filter, &p.name) {
                            continue;
                        }
                        let (description, when) =
                            split_when(p.description.as_deref().unwrap_or(""));
                        let meta = SkillMeta {
                            name: p.name.clone(),
                            description,
                            when_to_use: when,
                            arguments: p
                                .arguments
                                .iter()
                                .map(|a| serde_json::to_value(a).unwrap_or(Value::Null))
                                .collect(),
                            source: SkillSourceRef {
                                server: name.clone(),
                                kind: SkillSourceKind::Prompt,
                                reference: p.name.clone(),
                                body: None,
                            },
                        };
                        if self.insert(meta) {
                            found.push(p.name);
                        }
                    }
                }
                Err(e) => {
                    self.errors
                        .insert(name.clone(), format!("prompts/list: {e}"));
                }
            }
        }
        if want_resources {
            match server.list_resources() {
                Ok(resources) => {
                    for r in resources {
                        let is_skill = r.uri.starts_with(SKILL_SCHEME)
                            || r.mime_type.as_deref() == Some(SKILL_MIME);
                        if !is_skill {
                            continue;
                        }
                        let skill_name = r
                            .uri
                            .strip_prefix(SKILL_SCHEME)
                            .map(|s| s.trim_matches('/').to_string())
                            .filter(|s| !s.is_empty())
                            .or_else(|| r.name.clone())
                            .unwrap_or_else(|| r.uri.clone());
                        // The optional index resource `skill://` itself is not a skill.
                        if skill_name.is_empty() {
                            continue;
                        }
                        if !passes(filter, &skill_name) {
                            continue;
                        }
                        let (description, when) =
                            split_when(r.description.as_deref().unwrap_or(""));
                        let meta = SkillMeta {
                            name: skill_name.clone(),
                            description,
                            when_to_use: when,
                            arguments: Vec::new(),
                            source: SkillSourceRef {
                                server: name.clone(),
                                kind: SkillSourceKind::Resource,
                                reference: r.uri.clone(),
                                body: None,
                            },
                        };
                        if self.insert(meta) {
                            found.push(skill_name);
                        }
                    }
                }
                Err(e) => {
                    self.errors
                        .entry(name.clone())
                        .and_modify(|m| m.push_str(&format!("; resources/list: {e}")))
                        .or_insert(format!("resources/list: {e}"));
                }
            }
        }
        found
    }

    fn insert(&mut self, meta: SkillMeta) -> bool {
        if let Some(existing) = self.skills.get(&meta.name) {
            // Same source refreshed: replace; a different source: first wins.
            if existing.source == meta.source {
                self.skills.insert(meta.name.clone(), meta);
                return true;
            }
            return false;
        }
        self.skills.insert(meta.name.clone(), meta);
        true
    }

    /// Forget every skill of `server` (before a re-discovery).
    pub fn forget_server(&mut self, server: &str) {
        self.skills.retain(|_, m| m.source.server != server);
        self.errors.remove(server);
    }

    pub fn get(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.get(name)
    }
    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }
    pub fn len(&self) -> usize {
        self.skills.len()
    }
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// `skills.list` output.
    pub fn list_value(&self) -> Value {
        json!({
            "skills": self.skills.values().map(|m| json!({
                "name": m.name, "description": m.description, "when_to_use": m.when_to_use,
                "arguments": m.arguments, "source": {"server": m.source.server, "kind": m.source.kind}
            })).collect::<Vec<_>>(),
            "errors": self.errors,
        })
    }

    /// The catalogue block for a prompt (names + descriptions), or `None`
    /// when empty.
    pub fn render_catalogue(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let mut out = format!(
            "Available skills (reference one as {}<name> or call skills.load to read its full instructions):\n",
            self.prefix
        );
        for m in self.skills.values() {
            out.push_str(&format!("- {}: {}", m.name, m.description));
            if let Some(w) = &m.when_to_use {
                out.push_str(&format!(" (use when: {w})"));
            }
            out.push('\n');
        }
        Some(out)
    }

    /// Fetch (or serve from cache) a skill body. `servers` resolves the source
    /// server by name.
    pub fn load(
        &mut self,
        name: &str,
        arguments: Option<Value>,
        servers: &dyn Fn(&str) -> Option<std::sync::Arc<dyn SkillServer>>,
    ) -> Result<SkillBody, String> {
        let meta = self
            .skills
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown skill {name:?}"))?;
        let text = if meta.source.kind == SkillSourceKind::Inline {
            meta.source
                .body
                .clone()
                .ok_or_else(|| format!("inline skill {name:?} lost its body"))?
        } else {
            let server = servers(&meta.source.server).ok_or_else(|| {
                format!(
                    "skill {name:?}: server {:?} is not connected",
                    meta.source.server
                )
            })?;
            match meta.source.kind {
                SkillSourceKind::Prompt => {
                    let messages = server.get_prompt(&meta.source.reference, arguments)?;
                    prompt_messages_text(&messages)
                }
                SkillSourceKind::Resource => server.read_resource(&meta.source.reference)?,
                SkillSourceKind::Inline => unreachable!("handled above"),
            }
        };
        if text.trim().is_empty() {
            return Err(format!("skill {name:?} has an empty body"));
        }
        let text = if text.len() > self.max_bytes {
            let mut cut = self.max_bytes;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            format!(
                "{}\n\n[skill body truncated to skills.max_bytes = {} bytes]",
                &text[..cut],
                self.max_bytes
            )
        } else {
            text
        };
        let hash = crate::sha::sha256_hex(text.as_bytes());
        let body = SkillBody {
            name: name.to_string(),
            hash: hash.clone(),
            body: text,
        };
        self.bodies.insert(hash, body.clone());
        Ok(body)
    }

    /// A cached body by hash.
    /// Register the instruction's `:::skill` definitions. Inline skills win a
    /// name collision with discovered ones — the operator wrote them CLOSER to
    /// this agent than any server did.
    pub fn add_inline(&mut self, skills: &[crate::config::directives::InlineSkill]) -> Vec<String> {
        let mut names = Vec::new();
        for sk in skills {
            self.skills.insert(
                sk.name.clone(),
                SkillMeta {
                    name: sk.name.clone(),
                    description: sk.description.clone(),
                    when_to_use: sk.when_to_use.clone(),
                    arguments: Vec::new(),
                    source: SkillSourceRef {
                        server: "instruction".into(),
                        kind: SkillSourceKind::Inline,
                        reference: sk.name.clone(),
                        body: Some(sk.body.clone()),
                    },
                },
            );
            names.push(sk.name.clone());
        }
        names
    }

    pub fn body(&self, hash: &str) -> Option<&SkillBody> {
        self.bodies.get(hash)
    }

    /// Drop cached bodies whose hash is not in `keep`.
    pub fn evict_except(&mut self, keep: &[String]) {
        self.bodies.retain(|h, _| keep.iter().any(|k| k == h));
    }

    /// The `@skill:<name>` references in a text (deduplicated, in order).
    pub fn references(&self, text: &str) -> Vec<String> {
        find_references(text, &self.prefix)
    }
}

/// `@skill:<name>` references in `text` — a name is `[A-Za-z0-9_.:-]+`
/// (trailing punctuation stripped), deduplicated in order of appearance.
pub fn find_references(text: &str, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if prefix.is_empty() {
        return out;
    }
    let mut rest = text;
    while let Some(pos) = rest.find(prefix) {
        let after = &rest[pos + prefix.len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
            .collect();
        let name = name.trim_end_matches(['.', '/']).to_string();
        let consumed = name.len().min(after.len());
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
        rest = &after[consumed..];
    }
    out
}

/// A `prompts/get` result's text: the concatenated text parts of its messages.
pub fn prompt_messages_text(messages: &[Value]) -> String {
    let mut out = String::new();
    for m in messages {
        let content = m.get("content").unwrap_or(&Value::Null);
        let text = match content {
            Value::String(s) => s.clone(),
            Value::Object(o) => o
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&text);
        }
    }
    out
}

/// Render the loaded skill bodies as one system block.
pub fn render_bodies(bodies: &[&SkillBody]) -> Option<String> {
    if bodies.is_empty() {
        return None;
    }
    let mut out = String::from("Loaded skills — follow these instructions when relevant:\n");
    for b in bodies {
        out.push_str(&format!("\n### Skill: {}\n{}\n", b.name, b.body.trim()));
    }
    Some(out)
}

fn passes(filter: Option<&str>, name: &str) -> bool {
    match filter {
        None => true,
        Some(f) => {
            let f = f.trim();
            if let Some(prefix) = f.strip_suffix('*') {
                name.starts_with(prefix)
            } else {
                f == name || f.is_empty()
            }
        }
    }
}

/// Split a description of the form `"… When to use: …"` (or `"… Use when …"`).
fn split_when(desc: &str) -> (String, Option<String>) {
    for marker in ["When to use:", "when to use:", "Use when:", "use when:"] {
        if let Some((a, b)) = desc.split_once(marker) {
            let w = b.trim();
            return (
                a.trim().trim_end_matches('.').to_string(),
                (!w.is_empty()).then(|| w.to_string()),
            );
        }
    }
    (desc.trim().to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::mcp::wire::{Prompt, PromptArgument, Resource};

    struct Fake {
        name: String,
        prompts: Vec<Prompt>,
        resources: Vec<Resource>,
        bodies: BTreeMap<String, String>,
    }
    impl SkillServer for Fake {
        fn server_name(&self) -> String {
            self.name.clone()
        }
        fn supports_prompts(&self) -> bool {
            !self.prompts.is_empty()
        }
        fn supports_resources(&self) -> bool {
            !self.resources.is_empty()
        }
        fn list_prompts(&self) -> Result<Vec<Prompt>, String> {
            Ok(self.prompts.clone())
        }
        fn get_prompt(&self, name: &str, arguments: Option<Value>) -> Result<Vec<Value>, String> {
            let body = self.bodies.get(name).cloned().ok_or("no such prompt")?;
            let body = match arguments
                .and_then(|a| a.get("target").and_then(Value::as_str).map(str::to_string))
            {
                Some(t) => body.replace("{target}", &t),
                None => body,
            };
            Ok(vec![
                json!({"role": "user", "content": {"type": "text", "text": body}}),
            ])
        }
        fn list_resources(&self) -> Result<Vec<Resource>, String> {
            Ok(self.resources.clone())
        }
        fn read_resource(&self, uri: &str) -> Result<String, String> {
            self.bodies
                .get(uri)
                .cloned()
                .ok_or("no such resource".into())
        }
    }

    fn fake() -> Fake {
        Fake {
            name: "skills".into(),
            prompts: vec![
                Prompt {
                    name: "review-pr".into(),
                    title: None,
                    description: Some(
                        "Review a pull request. When to use: any code review request".into(),
                    ),
                    arguments: vec![PromptArgument {
                        name: "target".into(),
                        title: None,
                        description: None,
                        required: Some(false),
                    }],
                },
                Prompt {
                    name: "internal-tool".into(),
                    title: None,
                    description: None,
                    arguments: vec![],
                },
            ],
            resources: vec![
                Resource {
                    uri: "skill://deploy".into(),
                    name: Some("deploy".into()),
                    title: None,
                    description: Some("Deploy safely".into()),
                    mime_type: Some(SKILL_MIME.into()),
                },
                Resource {
                    uri: "file:///readme.md".into(),
                    name: None,
                    title: None,
                    description: None,
                    mime_type: Some("text/markdown".into()),
                },
                Resource {
                    uri: "notes://x".into(),
                    name: Some("x".into()),
                    title: None,
                    description: None,
                    mime_type: Some(SKILL_MIME.into()),
                },
            ],
            bodies: [
                (
                    "review-pr".to_string(),
                    "# Review PR\nLook at {target} carefully.".to_string(),
                ),
                ("internal-tool".to_string(), "internal".to_string()),
                (
                    "skill://deploy".to_string(),
                    "# Deploy\n1. plan 2. apply".to_string(),
                ),
                ("notes://x".to_string(), "x body".to_string()),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn discovery_over_prompts_and_resources_with_filters() {
        let f = fake();
        let mut c = Catalogue::new(DEFAULT_PREFIX, 1024);
        let found = c.discover(&f, Discover::Auto, None);
        assert_eq!(found, vec!["review-pr", "internal-tool", "deploy", "x"]);
        let m = c.get("review-pr").unwrap();
        assert_eq!(m.description, "Review a pull request");
        assert_eq!(m.when_to_use.as_deref(), Some("any code review request"));
        assert_eq!(m.arguments.len(), 1);
        assert_eq!(
            c.get("deploy").unwrap().source.kind,
            SkillSourceKind::Resource
        );
        assert!(
            c.get("readme.md").is_none(),
            "a plain markdown resource is not a skill"
        );
        let cat = c.render_catalogue().unwrap();
        assert!(
            cat.contains("- review-pr: Review a pull request (use when: any code review request)"),
            "{cat}"
        );
        // Filter + prompts-only.
        let mut c2 = Catalogue::new(DEFAULT_PREFIX, 1024);
        assert_eq!(
            c2.discover(&f, Discover::Prompts, Some("review-*")),
            vec!["review-pr"]
        );
        // A second source does not steal an existing name.
        let mut other = fake();
        other.name = "other".into();
        assert!(c.discover(&other, Discover::Auto, None).is_empty());
        assert_eq!(c.get("deploy").unwrap().source.server, "skills");
        c.forget_server("skills");
        assert!(c.is_empty());
    }

    #[test]
    fn load_caches_by_hash_truncates_and_renders() {
        let f = std::sync::Arc::new(fake());
        let mut c = Catalogue::new(DEFAULT_PREFIX, 30);
        c.discover(&*f, Discover::Auto, None);
        let f2 = f.clone();
        let servers = move |n: &str| -> Option<std::sync::Arc<dyn SkillServer>> {
            (n == "skills").then(|| f2.clone() as std::sync::Arc<dyn SkillServer>)
        };
        let b = c
            .load("review-pr", Some(json!({"target": "PR #7"})), &servers)
            .unwrap();
        assert!(b.body.contains("PR #7"));
        assert!(b.body.contains("truncated to skills.max_bytes"));
        assert!(c.body(&b.hash).is_some());
        let d = c.load("deploy", None, &servers).unwrap();
        assert!(d.body.starts_with("# Deploy"));
        assert!(c.load("nope", None, &servers).is_err());
        assert!(
            c.load("deploy", None, &|_| None).is_err(),
            "server not connected"
        );
        let block = render_bodies(&[&b, &d]).unwrap();
        assert!(block.contains("### Skill: review-pr") && block.contains("### Skill: deploy"));
        c.evict_except(std::slice::from_ref(&d.hash));
        assert!(c.body(&b.hash).is_none());
        assert!(c.body(&d.hash).is_some());
    }

    #[test]
    fn references_are_found_and_deduped() {
        let refs = find_references(
            "please @skill:review-pr this, then @skill:deploy. Also @skill:review-pr again and @skill:",
            "@skill:",
        );
        assert_eq!(refs, vec!["review-pr", "deploy"]);
        assert!(find_references("nothing here", "@skill:").is_empty());
        assert_eq!(find_references("use +s:x/y.", "+s:"), vec!["x/y"]);
        assert_eq!(
            prompt_messages_text(&[
                json!({"content": "a"}),
                json!({"content": [{"type": "text", "text": "b"}, {"type": "image"}]})
            ]),
            "a\n\nb"
        );
    }
}
