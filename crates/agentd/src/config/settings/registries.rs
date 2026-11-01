// SPDX-License-Identifier: AGPL-3.0-only
//! MCP registries (RFC 0055 §3.1, §4): `mcp.registries[]` — the registries
//! the operator trusts as directories of MCP servers, and the server names it
//! approves from each.
//!
//! A registry is a directory, not an authority. What it serves is untrusted
//! input about where to connect: the operator's `servers` globs are the
//! approval (a missing or empty list approves nothing), its `tags` the trust
//! (`trusted` is refused here; the operator vouches per server with an
//! `mcp.servers` entry), and its `allow`/`exclude` the widest tool surface any
//! server from it offers.
//!
//! A registry server is an `mcp.servers` entry with `registry` + `server`
//! (+ `version`) in place of an `endpoint`: written by the operator, made at
//! load for a document's `:::mcp{registry server}` that a registry approves,
//! or made when first used for a workflow step that names one
//! ([`implicit_name`], [`implicit_server`]). Its endpoint is learned when it
//! is dialed (`mcp::registry`), always on use unless its entry says
//! `connect: boot`.

use super::{Auth, Diagnostics, Dur, McpServer, Settings};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;

/// How long a resolution is reused when `refresh` says nothing.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(3600);
/// How long a registry request may take when `timeout` says nothing.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The version a registry server resolves to when none is named.
pub const LATEST: &str = "latest";

/// One `mcp.registries[]` entry.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpRegistry {
    pub name: String,
    /// The API base (`https://registry.modelcontextprotocol.io`): the
    /// `v0.1` paths are appended to it.
    pub url: String,
    /// The credential the registry's API is reached with — never a
    /// server's: a registry supplies no credential.
    #[serde(default)]
    pub auth: Option<Auth>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Globs over registry names (`io.github.github/*`): the approval. A
    /// name outside them is never resolved; missing or empty approves
    /// nothing; `*` alone approves every name.
    #[serde(default)]
    pub servers: Vec<String>,
    /// The trifecta tags every server from it counts as; default
    /// `{"*": [untrusted_input]}`. `trusted` is refused.
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
    /// The widest tool surface a server from it offers.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Per-instance pacing of the calls to each server from it.
    #[serde(default)]
    pub rate: Option<String>,
    /// The registry request's timeout, and its servers' default.
    #[serde(default)]
    pub timeout: Option<Dur>,
    /// How long a resolution is reused (default `1h`).
    #[serde(default)]
    pub refresh: Option<Dur>,
    /// Admit remotes on loopback, private and link-local addresses (and
    /// loopback `http://`): an internal registry's servers.
    #[serde(default)]
    pub allow_private: bool,
    /// Give the model `mcp.registry.search` and `mcp.registry.connect` over
    /// the names `servers` approves (RFC 0055 §8.2). Off by default.
    #[serde(default)]
    pub discover: bool,
}

impl McpRegistry {
    /// Whether the operator approves the registry name `name` from here.
    pub fn approves(&self, name: &str) -> bool {
        self.servers.iter().any(|g| name_matches(g, name))
    }

    /// The tags its servers count as.
    pub fn tags(&self) -> BTreeMap<String, Vec<String>> {
        if self.tags.values().all(Vec::is_empty) {
            default_tags()
        } else {
            self.tags.clone()
        }
    }

    /// How long a resolution is reused.
    pub fn refresh(&self) -> Duration {
        self.refresh.map_or(DEFAULT_REFRESH, |d| d.0)
    }

    /// The registry request's timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout.map_or(DEFAULT_TIMEOUT, |d| d.0)
    }

    /// The API base, without a trailing `/`.
    pub fn base(&self) -> &str {
        self.url.trim_end_matches('/')
    }

    /// Whether a document's `registry=` URL names this registry: the same
    /// URL, a trailing `/` aside.
    pub fn is_url(&self, url: &str) -> bool {
        self.base() == url.trim().trim_end_matches('/')
    }
}

/// `{"*": [untrusted_input]}`: what a registry's servers count as by default.
pub fn default_tags() -> BTreeMap<String, Vec<String>> {
    BTreeMap::from([("*".to_string(), vec!["untrusted_input".to_string()])])
}

/// Whether the glob `glob` matches the registry name `name`, exactly: a
/// name is `namespace/server`, a glob has as many `/`-separated segments,
/// and `*` matches any run of characters within one segment (never a `/`).
/// `*` alone matches every name.
pub fn name_matches(glob: &str, name: &str) -> bool {
    let glob = glob.trim();
    if glob == "*" {
        return !name.is_empty();
    }
    let g: Vec<&str> = glob.split('/').collect();
    let n: Vec<&str> = name.split('/').collect();
    g.len() == n.len() && g.iter().zip(&n).all(|(g, n)| segment_matches(g, n))
}

/// `*`-wildcard match of one segment.
fn segment_matches(glob: &str, text: &str) -> bool {
    let parts: Vec<&str> = glob.split('*').collect();
    if parts.len() == 1 {
        return glob == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    if !text.ends_with(last) {
        return false;
    }
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// The name a registry server a workflow step or a document names is known
/// by: `<registry>/<server>`, and `@<version>` when one other than `latest`
/// is named. Deterministic, so every step naming the same server shares one
/// connection, and outside the `[a-zA-Z0-9_-]` names an operator gives.
pub fn implicit_name(registry: &str, server: &str, version: Option<&str>) -> String {
    match version.filter(|v| !v.is_empty() && *v != LATEST) {
        Some(v) => format!("{registry}/{server}@{v}"),
        None => format!("{registry}/{server}"),
    }
}

/// The `mcp.registries` entry named `name`.
pub fn registry<'a>(s: &'a Settings, name: &str) -> Option<&'a McpRegistry> {
    s.mcp.registries.iter().find(|r| r.name == name)
}

/// The registry server named `server` from the registry `registry`, as the
/// operator's own `mcp.servers` entry for it when there is one (its
/// credential, headers, tags and ceiling), renamed `name`, and otherwise as
/// the registry alone makes it — refused when the registry is not declared
/// or does not approve the name.
pub fn implicit_server(
    s: &Settings,
    name: &str,
    registry_name: &str,
    server: &str,
    version: Option<&str>,
) -> Result<McpServer, String> {
    let reg = registry(s, registry_name).ok_or_else(|| {
        format!("mcp registry {registry_name:?} is not declared under mcp.registries")
    })?;
    if !reg.approves(server) {
        return Err(not_approved(reg, server));
    }
    let version = version.filter(|v| !v.is_empty()).map(str::to_string);
    let same_version =
        |v: &Option<String>| v.as_deref().unwrap_or(LATEST) == version.as_deref().unwrap_or(LATEST);
    let mut srv = s
        .mcp
        .servers
        .iter()
        .find(|o| {
            o.registry.as_deref() == Some(registry_name)
                && o.server.as_deref() == Some(server)
                && same_version(&o.version)
        })
        .cloned()
        .unwrap_or_else(|| McpServer::from_registry(name, registry_name, server, version.clone()));
    srv.name = name.to_string();
    srv.version = version;
    srv.events = Vec::new();
    srv.event_grants = Vec::new();
    complete_one(reg, &mut srv, &mut Vec::new());
    Ok(srv)
}

/// The refusal for a name outside a registry's approval.
pub fn not_approved(reg: &McpRegistry, server: &str) -> String {
    if reg.servers.is_empty() {
        format!(
            "mcp registry {:?} approves no server (its `servers` list is empty), so {server:?} is not resolved — list the names you approve",
            reg.name
        )
    } else {
        format!(
            "mcp server {server:?} is not approved by mcp registry {:?}: it matches none of its `servers` globs {:?}",
            reg.name, reg.servers
        )
    }
}

/// Fill each registry server's tags, ceiling, pacing and timeout from its
/// registry, before validation and the trifecta gate judge it.
pub fn complete(s: &mut Settings) {
    let registries = s.mcp.registries.clone();
    for srv in &mut s.mcp.servers {
        let Some(reg) = srv
            .registry
            .as_deref()
            .and_then(|r| registries.iter().find(|x| x.name == r))
        else {
            continue;
        };
        complete_one(reg, srv, &mut Vec::new());
    }
}

/// [`complete`] for one server: tags default to the registry's; the server's
/// `allow` narrows the registry's ceiling (a pattern outside it is dropped,
/// said in `warnings`), its `exclude` adds; `rate` and `timeout` default to
/// the registry's.
fn complete_one(reg: &McpRegistry, srv: &mut McpServer, warnings: &mut Vec<String>) {
    if srv.tags.values().all(Vec::is_empty) {
        srv.tags = reg.tags();
    }
    if let Some(ceiling) = &reg.allow {
        srv.allow = Some(match srv.allow.take() {
            None => ceiling.clone(),
            Some(own) => own
                .into_iter()
                .filter(|p| {
                    let kept = ceiling.iter().any(|c| super::pattern_subsumes(p, c));
                    if !kept {
                        warnings.push(format!(
                            "mcp server '{}': allow {p:?} is outside mcp registry {:?}'s allow and is dropped",
                            srv.name, reg.name
                        ));
                    }
                    kept
                })
                .collect(),
        });
    }
    for e in &reg.exclude {
        if !srv.exclude.contains(e) {
            srv.exclude.push(e.clone());
        }
    }
    if srv.rate.is_none() {
        srv.rate = reg.rate.clone();
    }
    if srv.timeout.is_none() {
        srv.timeout = reg.timeout;
    }
}

/// `mcp.registries` and the registry servers.
pub(super) fn validate(s: &Settings, d: &mut Diagnostics) {
    let mut err = |m: String| d.errors.push(m);
    let mut names = std::collections::BTreeSet::new();
    for r in &s.mcp.registries {
        let at = format!("mcp.registries[{}]", r.name);
        if r.name.is_empty()
            || !r
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            err(format!(
                "mcp.registries: entry name '{}' must be [a-zA-Z0-9_-]+",
                r.name
            ));
        }
        if !names.insert(r.name.as_str()) {
            err(format!("mcp.registries: duplicate name '{}'", r.name));
        }
        if let Err(e) = super::super::mcp_endpoint_scheme_ok(&r.url) {
            err(format!(
                "{at}.url: {}",
                e.to_string().replace("mcp endpoint", "registry url")
            ));
        }
        for (pattern, tags) in &r.tags {
            for t in tags {
                if t == crate::sec::scope::TRUSTED {
                    err(format!(
                        "{at}.tags[{pattern:?}]: `trusted` is refused on a registry — a registry is a directory, not an authority; vouch for one server with an mcp.servers entry of your own"
                    ));
                } else if crate::sec::scope::TrifectaTag::parse(t).is_none() {
                    err(format!(
                        "{at}.tags[{pattern:?}]: unknown trifecta tag '{t}'"
                    ));
                }
            }
        }
        for g in &r.servers {
            if g.trim().is_empty() || g.contains("**") {
                err(format!(
                    "{at}.servers: {g:?} is not a name glob (`namespace/server`, `*` within a segment)"
                ));
            }
        }
        for (h, v) in &r.headers {
            if super::super::is_secret_shaped_key(h) && !crate::sec::secret::has_secret_ref(v) {
                err(format!(
                    "{at} header '{h}' looks like a credential but has an inline value; use a {{{{secret:…}}}} reference"
                ));
            } else if let Some(e) = super::unresolved_secret_ref(v) {
                err(format!("{at} header '{h}': {e}"));
            }
        }
        if let Some(auth) = &r.auth {
            for e in super::validate_auth_block(auth, s.security.client.as_ref(), &at, false) {
                err(e);
            }
        }
        if let Some(rate) = &r.rate
            && let Err(e) = crate::supervisor::tree::parse_rate(rate)
        {
            err(format!("{at}.rate: {e}"));
        }
        if r.servers.is_empty() {
            d.warnings.push(format!(
                "{at}: `servers` is empty, so it approves nothing — no server from it is resolved"
            ));
        }
    }
    let mut err = |m: String| d.errors.push(m);
    for srv in &s.mcp.servers {
        let at = format!("mcp server '{}'", srv.name);
        match (&srv.registry, &srv.server) {
            (Some(r), Some(name)) => {
                if !srv.endpoint.is_empty() {
                    err(format!(
                        "{at}: `endpoint` and `registry` are two ways to name one server — keep one"
                    ));
                }
                match registry(s, r) {
                    None => err(format!(
                        "{at}: registry {r:?} is not declared under mcp.registries"
                    )),
                    Some(reg) if !reg.approves(name) => {
                        err(format!("{at}: {}", not_approved(reg, name)))
                    }
                    Some(_) => {}
                }
                if !name.contains('/') {
                    err(format!(
                        "{at}: server {name:?} is not a registry name (`namespace/server`)"
                    ));
                }
            }
            (Some(_), None) => err(format!(
                "{at}: `registry` names the registry; `server` names the server in it — both are required"
            )),
            (None, Some(_)) => err(format!(
                "{at}: `server` names a server in a registry — add `registry`"
            )),
            (None, None) => {
                if srv.version.is_some() {
                    err(format!("{at}: `version` belongs to a registry server"));
                }
            }
        }
    }
}

/// The trifecta tags the registries' approved servers count as, whether or
/// not any is connected yet (RFC 0055 §4.4): approving a registry is judged
/// at load, not at first use.
pub fn approved_tags(s: &Settings) -> Vec<crate::sec::scope::TrifectaTag> {
    let mut out = Vec::new();
    for r in s.mcp.registries.iter().filter(|r| !r.servers.is_empty()) {
        for t in r.tags().values().flatten() {
            if let Some(t) = crate::sec::scope::TrifectaTag::parse(t)
                && !out.contains(&t)
            {
                out.push(t);
            }
        }
    }
    out
}

/// Whether any registry gives the model `mcp.registry.*`.
pub fn discovers(s: &Settings) -> bool {
    s.mcp
        .registries
        .iter()
        .any(|r| r.discover && !r.servers.is_empty())
}

/// The tags the `mcp.registry.*` tools carry: those of every registry that
/// gives them.
pub fn discover_tags(s: &Settings) -> Vec<crate::sec::scope::TrifectaTag> {
    let mut out = Vec::new();
    for r in s.mcp.registries.iter().filter(|r| r.discover) {
        for t in r.tags().values().flatten() {
            if let Some(t) = crate::sec::scope::TrifectaTag::parse(t)
                && !out.contains(&t)
            {
                out.push(t);
            }
        }
    }
    out
}

/// Every `{name, registry, server, version}` a workflow step names, at any
/// depth: `mcp.tool`, `mcp.resource` and `wait {on: resource}` with
/// `registry` (the step's `server` already the [`implicit_name`]).
pub fn steps_naming(w: &crate::engine::model::Workflow) -> Vec<StepServer> {
    fn walk(steps: &BTreeMap<String, crate::engine::model::Step>, out: &mut Vec<StepServer>) {
        for st in steps.values() {
            if let Some(n) = StepServer::of(&st.spec) {
                out.push(n);
            }
            if let Some(b) = &st.body {
                walk(&b.steps, out);
            }
            for b in st.branches.values() {
                walk(&b.steps, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&w.steps, &mut out);
    out
}

/// A registry server a step names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepServer {
    /// Its [`implicit_name`].
    pub name: String,
    pub registry: String,
    pub server: String,
    pub version: Option<String>,
}

impl StepServer {
    /// The registry server a step's `spec` names, if it names one.
    pub fn of(spec: &serde_json::Map<String, serde_json::Value>) -> Option<StepServer> {
        let s = |k: &str| spec.get(k).and_then(serde_json::Value::as_str);
        Some(StepServer {
            registry: s("registry")?.to_string(),
            server: s(REGISTRY_SERVER)?.to_string(),
            name: s("server")?.to_string(),
            version: s("version").map(str::to_string),
        })
    }
}

/// The spec field a step's registry name is kept in once its `server` is
/// the [`implicit_name`]: never written in a definition.
pub const REGISTRY_SERVER: &str = "registry_server";

/// A step naming a registry server: its `server` becomes the
/// [`implicit_name`], the name as written kept in [`REGISTRY_SERVER`].
/// Idempotent. `Err` for a registry, server or version that is templated or
/// not a string — what a step names decides the approval checked at load.
pub fn rewrite_step(
    kind: &str,
    spec: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    use serde_json::Value;
    let Some(reg) = spec.get("registry") else {
        if spec.contains_key("version") && kind != "workflow" {
            return Err("`version` belongs to a step naming a registry server".into());
        }
        return Ok(());
    };
    if kind == "wait" && spec.get("on").and_then(Value::as_str) != Some("resource") {
        return Err("`registry` belongs to `wait {on: resource}`".into());
    }
    let fixed = |v: Option<&Value>, what: &str| -> Result<Option<String>, String> {
        match v {
            None => Ok(None),
            Some(Value::String(s)) if s.contains("{{") => Err(format!(
                "`{what}` is fixed by the definition — it decides the registry approval, checked at load — so it cannot be templated"
            )),
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
            Some(_) => Err(format!("`{what}` must be a non-empty string")),
        }
    };
    let registry = fixed(Some(reg), "registry")?.unwrap_or_default();
    let server = match fixed(spec.get(REGISTRY_SERVER), REGISTRY_SERVER)? {
        Some(s) => s,
        None => fixed(spec.get("server"), "server")?
            .ok_or("a step naming a registry needs `server`: the server's registry name")?,
    };
    let version = fixed(spec.get("version"), "version")?;
    spec.insert(
        "server".into(),
        Value::String(implicit_name(&registry, &server, version.as_deref())),
    );
    spec.insert(REGISTRY_SERVER.into(), Value::String(server));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_glob_matches_within_a_segment_never_across() {
        assert!(name_matches(
            "io.github.github/*",
            "io.github.github/github-mcp-server"
        ));
        assert!(name_matches("io.github.*/*", "io.github.acme/x"));
        assert!(name_matches("com.deepwiki/mcp", "com.deepwiki/mcp"));
        assert!(name_matches(
            "*/github-*",
            "io.github.github/github-mcp-server"
        ));
        assert!(name_matches(
            "io.*.github/*-server",
            "io.github.github/github-mcp-server"
        ));
        assert!(!name_matches("io.github.github/*", "io.github.github/a/b"));
        assert!(!name_matches("io.github.*", "io.github.github/x"));
        assert!(!name_matches("io.github.github/*", "io.github.githubx/x"));
        assert!(!name_matches("com.deepwiki/mcp", "com.deepwiki/mcp2"));
        assert!(!name_matches("a*b/x", "ab/y"));
        assert!(
            !name_matches("ab*ba/x", "aba/x"),
            "overlapping prefix and suffix"
        );
        // `*` alone approves every name.
        assert!(name_matches("*", "io.github.github/github-mcp-server"));
        assert!(!name_matches("*", ""));
    }

    #[test]
    fn missing_or_empty_servers_approve_nothing() {
        let reg: McpRegistry = serde_json::from_value(
            serde_json::json!({"name": "official", "url": "https://registry.example"}),
        )
        .unwrap();
        assert!(!reg.approves("io.github.github/github-mcp-server"));
        assert_eq!(reg.tags(), default_tags());
        assert_eq!(reg.refresh(), DEFAULT_REFRESH);
        assert!(!reg.discover && !reg.allow_private);
    }

    #[test]
    fn the_implicit_name_carries_a_named_version_only() {
        assert_eq!(implicit_name("official", "io.a/b", None), "official/io.a/b");
        assert_eq!(
            implicit_name("official", "io.a/b", Some("latest")),
            "official/io.a/b"
        );
        assert_eq!(
            implicit_name("official", "io.a/b", Some("1.2.0")),
            "official/io.a/b@1.2.0"
        );
    }

    #[test]
    fn a_step_naming_a_registry_server_is_renamed_idempotently() {
        let mut spec = serde_json::json!({"registry": "official", "server": "io.a/b", "tool": "t"})
            .as_object()
            .cloned()
            .unwrap();
        rewrite_step("mcp.tool", &mut spec).unwrap();
        assert_eq!(spec["server"], "official/io.a/b");
        assert_eq!(spec[REGISTRY_SERVER], "io.a/b");
        let again = spec.clone();
        rewrite_step("mcp.tool", &mut spec).unwrap();
        assert_eq!(spec, again);
        let named = StepServer::of(&spec).unwrap();
        assert_eq!(
            (named.registry.as_str(), named.server.as_str()),
            ("official", "io.a/b")
        );
        // Templated names are refused: the approval is checked at load.
        let mut t = serde_json::json!({"registry": "official", "server": "{{inputs.s}}"})
            .as_object()
            .cloned()
            .unwrap();
        assert!(
            rewrite_step("mcp.tool", &mut t)
                .unwrap_err()
                .contains("templated")
        );
        // A wait names a registry server on `resource` alone.
        let mut w = serde_json::json!({"on": "signal", "registry": "official", "server": "io.a/b"})
            .as_object()
            .cloned()
            .unwrap();
        assert!(rewrite_step("wait", &mut w).is_err());
        let mut v = serde_json::json!({"server": "s", "version": "1"})
            .as_object()
            .cloned()
            .unwrap();
        assert!(rewrite_step("mcp.tool", &mut v).is_err());
    }
}
