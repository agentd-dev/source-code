//! Manifest-driven policy (RFC §16).
//!
//! [`tools::policy`](crate::tools::policy) owns the [`Policy`] trait.
//! This module ships the one production implementation — a
//! [`ManifestPolicy`] built from a declarative allowlist that the
//! workflow's `[policy]` TOML block (or an external `--policy-file`)
//! populates.
//!
//! Matching is intentionally narrow: exact names, prefix globs
//! (`/workspace/**`, `/tmp/agent/*`), and the universal `*`. No
//! regex. Deliberately spartan so operators read one of these three
//! forms and know exactly what's allowed.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::mcp::allowlist::McpAllowlist;
use crate::tools::policy::{Decision, Policy};

// ---------------------------------------------------------------------------
// Manifest types
// ---------------------------------------------------------------------------

/// The whole allowlist the operator wants to enforce. Every sub-field
/// defaults to "empty" (deny-everything) so forgetting a section
/// fails closed rather than open.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyManifest {
    #[serde(default)]
    pub fs: FsPolicy,
    #[serde(default)]
    pub env: EnvPolicy,
    #[serde(default)]
    pub mcp: McpPolicy,
    #[serde(default)]
    pub http: HttpPolicy,
    #[serde(default)]
    pub shell: ShellPolicy,
    /// Optional `[policy.rego]` block. When present,
    /// evaluation is a logical AND with the static allowlist —
    /// Rego must also return `data.agent.allow = true` for the
    /// request to pass. Requires the `policy-rego` Cargo feature.
    #[serde(default)]
    pub rego: Option<RegoConfig>,
}

/// `[policy.rego]` block.
///
/// Rego policy MUST declare `package agent` and export a `default
/// allow = false` rule. The runtime queries `data.agent.allow`
/// per check with an input document matching this shape:
///
/// ```json
/// {
///   "tool": "fs.read" | "fs.write" | "fs.delete" | "fs.list"
///         | "env.read" | "http.request" | "shell.run",
///   "args": { /* tool-specific; see docs/capabilities.md */ }
/// }
/// ```
///
/// Extra `data` declared here is merged at load time into
/// `data.agent` so policies can parameterise without hard-coding.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegoConfig {
    /// Inline Rego source. Mutually exclusive with `file`.
    #[serde(default)]
    pub inline: Option<String>,
    /// Filesystem path to a `.rego` module.
    #[serde(default)]
    pub file: Option<std::path::PathBuf>,
    /// Extra static data merged into `data.agent.*`. Useful for
    /// parameterising a shared policy module across deployments
    /// (e.g. `{ region: "eu-west-1", tenant: "acme" }`).
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    /// Policy rule to query. Defaults to `data.agent.allow`. Most
    /// operators never override this.
    #[serde(default)]
    pub query: Option<String>,
}

impl RegoConfig {
    /// Resolve the Rego source text — either inline or read from
    /// `file`. Returns `Ok(None)` when no Rego is configured (the
    /// block was present but both fields empty; caller treats this
    /// as "no extra pass").
    pub fn source(&self) -> std::result::Result<Option<String>, String> {
        match (&self.inline, &self.file) {
            (None, None) => Ok(None),
            (Some(_), Some(_)) => Err("policy.rego: inline and file are mutually exclusive".into()),
            (Some(s), None) => Ok(Some(s.clone())),
            (None, Some(p)) => std::fs::read_to_string(p)
                .map(Some)
                .map_err(|e| format!("read policy.rego.file {}: {e}", p.display())),
        }
    }

    pub fn effective_query(&self) -> &str {
        self.query.as_deref().unwrap_or("data.agent.allow")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FsPolicy {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    #[serde(default)]
    pub delete: Vec<String>,
    /// Directories the workflow is allowed to list. Defaults to `read`
    /// when empty.
    #[serde(default)]
    pub list: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvPolicy {
    #[serde(default)]
    pub read_keys: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpPolicy {
    #[serde(default)]
    pub servers: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub resources: Vec<String>,
}

/// Outbound HTTP policy (RFC §10.5, §16.2). The handler looks up the
/// request URL's host + scheme here before opening a socket.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HttpPolicy {
    /// URL pattern allowlist. `http://host.example/*`,
    /// `https://*.internal/**`, or bare domain prefixes. Matched
    /// against the request URL string.
    #[serde(default)]
    pub urls: Vec<String>,
    /// Allowed methods (case-insensitive). Empty = inherit the
    /// handler's default (GET + POST).
    #[serde(default)]
    pub methods: Vec<String>,
}

/// Shell / sub-process policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShellPolicy {
    /// Absolute paths or prefix patterns (e.g. `/usr/local/bin/*`).
    /// Matched against the canonicalised command path.
    #[serde(default)]
    pub commands: Vec<String>,
}

impl PolicyManifest {
    /// Build an [`McpAllowlist`] from this manifest.
    pub fn mcp_allowlist(&self) -> McpAllowlist {
        McpAllowlist {
            allowed_tools: self.mcp.tools.clone(),
            allowed_resource_patterns: self.mcp.resources.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// ManifestPolicy impl
// ---------------------------------------------------------------------------

/// [`Policy`] implementation driven by a [`PolicyManifest`].
#[derive(Debug)]
pub struct ManifestPolicy {
    manifest: PolicyManifest,
    /// Rego policy source + auxiliary data shared across threads.
    /// The actual `regorus::Engine` isn't `Send` (uses `Rc`
    /// internally), so each thread lazily compiles its own from
    /// this `Arc`'d spec on first use via [`REGO_ENGINE`] thread
    /// locals. `None` when `[policy.rego]` is absent or the
    /// feature is off.
    #[cfg(feature = "policy-rego")]
    rego_spec: Option<std::sync::Arc<RegoSpec>>,
}

#[cfg(feature = "policy-rego")]
#[derive(Debug)]
struct RegoSpec {
    source: String,
    data: Option<serde_json::Value>,
    query: String,
    /// Stable fingerprint of (source, data, query) used to detect
    /// config swaps — a thread-local engine whose `spec_id` no
    /// longer matches the current policy drops + recompiles. We
    /// hold the `Arc<RegoSpec>` by pointer identity so checking
    /// is a cheap pointer comparison.
    #[allow(dead_code)]
    id: u64,
}

#[cfg(feature = "policy-rego")]
std::thread_local! {
    /// Per-thread Rego engine cache. The tuple stores
    /// (spec-ptr-id, evaluator); we recompile when the current
    /// policy's `Arc<RegoSpec>` pointer disagrees.
    static REGO_ENGINE: std::cell::RefCell<Option<(u64, RegoEvaluator)>> =
        const { std::cell::RefCell::new(None) };
}

impl ManifestPolicy {
    /// Build a policy from a manifest, compiling the Rego module if
    /// one is declared. Rego compile errors surface as Err — the
    /// caller (usually `runtime::build_engine`) turns that into a
    /// startup failure so misconfigured policies never get past
    /// first request.
    ///
    /// When the Rego block is present, this builds the **spec**
    /// (source + data + query) synchronously and validates it by
    /// compiling a one-shot engine; individual evaluator threads
    /// lazily compile their own per-thread engines on first use.
    pub fn new(manifest: PolicyManifest) -> std::result::Result<Self, String> {
        #[cfg(feature = "policy-rego")]
        {
            let rego_spec = match &manifest.rego {
                None => None,
                Some(cfg) => match cfg.source()? {
                    None => None,
                    Some(source) => {
                        let query = cfg.effective_query().to_string();
                        // Validate by compiling once up front — bad
                        // Rego fails the startup, not the first
                        // request.
                        let _probe = RegoEvaluator::compile(&source, cfg.data.as_ref(), &query)?;
                        Some(std::sync::Arc::new(RegoSpec {
                            source,
                            data: cfg.data.clone(),
                            query,
                            id: fresh_spec_id(),
                        }))
                    }
                },
            };
            Ok(Self {
                manifest,
                rego_spec,
            })
        }
        #[cfg(not(feature = "policy-rego"))]
        {
            if manifest.rego.is_some() {
                return Err("workflow declares [policy.rego] but this build lacks the \
                     `policy-rego` Cargo feature; rebuild with --features policy-rego"
                    .into());
            }
            Ok(Self { manifest })
        }
    }

    pub fn manifest(&self) -> &PolicyManifest {
        &self.manifest
    }

    /// Run the Rego pass, if configured. Returns `Allow` when Rego
    /// is not configured so callers can use `gate_and_rego`
    /// uniformly. The actual evaluator is thread-local (regorus is
    /// `!Send` because it uses `Rc` internally); first call on a
    /// thread compiles, subsequent calls reuse.
    #[cfg(feature = "policy-rego")]
    fn rego_decision(&self, input: serde_json::Value) -> Decision {
        let Some(spec) = &self.rego_spec else {
            return Decision::Allow;
        };
        REGO_ENGINE.with(|cell| {
            let mut slot = cell.borrow_mut();
            let needs_compile = match slot.as_ref() {
                Some((id, _)) => *id != spec.id,
                None => true,
            };
            if needs_compile {
                match RegoEvaluator::compile(&spec.source, spec.data.as_ref(), &spec.query) {
                    Ok(evaluator) => {
                        *slot = Some((spec.id, evaluator));
                    }
                    Err(e) => {
                        return Decision::Deny(format!("rego thread-init: {e}"));
                    }
                }
            }
            let (_, evaluator) = slot.as_mut().expect("just populated above");
            match evaluator.eval(input) {
                Ok(true) => Decision::Allow,
                Ok(false) => Decision::Deny("rego.allow returned false".into()),
                Err(e) => Decision::Deny(format!("rego eval error: {e}")),
            }
        })
    }

    #[cfg(not(feature = "policy-rego"))]
    fn rego_decision(&self, _input: serde_json::Value) -> Decision {
        Decision::Allow
    }

    /// Combine a static-allowlist decision with the Rego pass.
    /// Short-circuits when the static side denies — Rego never
    /// gets a chance to override a deny. When the static side
    /// allows AND Rego is configured, Rego's decision becomes the
    /// overall outcome.
    fn gate_and_rego(
        &self,
        static_decision: Decision,
        input: impl FnOnce() -> serde_json::Value,
    ) -> Decision {
        match static_decision {
            Decision::Deny(_) => static_decision,
            Decision::Allow => self.rego_decision(input()),
        }
    }
}

// ---------------------------------------------------------------------------
// Rego evaluator (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "policy-rego")]
fn fresh_spec_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(feature = "policy-rego")]
#[derive(Debug)]
struct RegoEvaluator {
    engine: regorus::Engine,
    query: String,
}

#[cfg(feature = "policy-rego")]
impl RegoEvaluator {
    fn compile(
        src: &str,
        data: Option<&serde_json::Value>,
        query: &str,
    ) -> std::result::Result<Self, String> {
        let mut engine = regorus::Engine::new();
        engine
            .add_policy("policy.rego".to_string(), src.to_string())
            .map_err(|e| format!("rego compile: {e}"))?;
        if let Some(extra) = data {
            let v: regorus::Value = serde_json::from_value(extra.clone())
                .map_err(|e| format!("rego data json: {e}"))?;
            engine
                .add_data(v)
                .map_err(|e| format!("rego add_data: {e}"))?;
        }
        Ok(Self {
            engine,
            query: query.to_string(),
        })
    }

    /// Evaluate the configured query with the given input document.
    /// Returns the boolean truthiness of the result. Non-boolean
    /// results (e.g. undefined / an object) count as deny — Rego
    /// policies always stage their decision as a `default allow =
    /// false` rule in practice.
    fn eval(&mut self, input: serde_json::Value) -> std::result::Result<bool, String> {
        let input_val: regorus::Value =
            serde_json::from_value(input).map_err(|e| format!("input json -> rego value: {e}"))?;
        self.engine.set_input(input_val);
        let result = self
            .engine
            .eval_query(self.query.clone(), false)
            .map_err(|e| format!("eval: {e}"))?;
        // `Results` contains a list of expressions; a simple
        // `data.agent.allow` query returns one expression with a
        // single boolean value.
        let allow = result
            .result
            .first()
            .and_then(|r| r.expressions.first())
            .map(|e| matches!(e.value, regorus::Value::Bool(true)))
            .unwrap_or(false);
        Ok(allow)
    }
}

impl Policy for ManifestPolicy {
    fn check_fs_read(&self, path: &Path) -> Decision {
        let static_d = gate_path("fs_read", path, &self.manifest.fs.read);
        self.gate_and_rego(static_d, || {
            serde_json::json!({
                "tool": "fs.read",
                "args": { "path": path.display().to_string() }
            })
        })
    }

    fn check_fs_write(&self, path: &Path) -> Decision {
        let static_d = gate_path("fs_write", path, &self.manifest.fs.write);
        self.gate_and_rego(static_d, || {
            serde_json::json!({
                "tool": "fs.write",
                "args": { "path": path.display().to_string() }
            })
        })
    }

    fn check_fs_delete(&self, path: &Path) -> Decision {
        let static_d = gate_path("fs_delete", path, &self.manifest.fs.delete);
        self.gate_and_rego(static_d, || {
            serde_json::json!({
                "tool": "fs.delete",
                "args": { "path": path.display().to_string() }
            })
        })
    }

    fn check_fs_list(&self, path: &Path) -> Decision {
        // Fall back to the read set when `list` is empty — listing a
        // directory you can read is almost always fine.
        let patterns = if self.manifest.fs.list.is_empty() {
            &self.manifest.fs.read
        } else {
            &self.manifest.fs.list
        };
        let static_d = gate_path("fs_list", path, patterns);
        self.gate_and_rego(static_d, || {
            serde_json::json!({
                "tool": "fs.list",
                "args": { "path": path.display().to_string() }
            })
        })
    }

    fn check_env_read(&self, key: &str) -> Decision {
        let static_d = if name_matches_any(&self.manifest.env.read_keys, key) {
            Decision::Allow
        } else {
            Decision::Deny(format!(
                "env var `{key}` is not in the allowlist ({} configured)",
                self.manifest.env.read_keys.len()
            ))
        };
        self.gate_and_rego(static_d, || {
            serde_json::json!({
                "tool": "env.read",
                "args": { "key": key }
            })
        })
    }

    fn check_http_request(&self, method: &str, url: &str) -> Decision {
        let static_d = check_http_static(&self.manifest.http, method, url);
        self.gate_and_rego(static_d, || {
            serde_json::json!({
                "tool": "http.request",
                "args": { "method": method, "url": url }
            })
        })
    }

    fn check_shell_run(&self, command: &Path) -> Decision {
        let static_d = if self.manifest.shell.commands.is_empty() {
            Decision::Deny("shell_run denied: no `shell.commands` allowlist configured".into())
        } else if path_matches_any(&self.manifest.shell.commands, command) {
            Decision::Allow
        } else {
            Decision::Deny(format!(
                "shell_run command `{}` not covered by any allowlist pattern",
                command.display()
            ))
        };
        self.gate_and_rego(static_d, || {
            serde_json::json!({
                "tool": "shell.run",
                "args": { "command": command.display().to_string() }
            })
        })
    }
}

fn check_http_static(http: &HttpPolicy, method: &str, url: &str) -> Decision {
    if http.urls.is_empty() {
        return Decision::Deny("http_request denied: no `http.urls` allowlist configured".into());
    }
    if !http.methods.is_empty() && !http.methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
        return Decision::Deny(format!("http_request method `{method}` not in allowlist"));
    }
    if http.urls.iter().any(|p| path_matches(p, url)) {
        Decision::Allow
    } else {
        Decision::Deny(format!(
            "http_request URL `{url}` not covered by any allowlist pattern"
        ))
    }
}

// ---------------------------------------------------------------------------
// Matchers
// ---------------------------------------------------------------------------

fn gate_path(op: &str, path: &Path, patterns: &[String]) -> Decision {
    if patterns.is_empty() {
        return Decision::Deny(format!(
            "{op} denied: no `{op}` allowlist patterns are configured"
        ));
    }
    if path_matches_any(patterns, path) {
        Decision::Allow
    } else {
        Decision::Deny(format!(
            "{op} on `{}` not covered by any allowlist pattern",
            path.display()
        ))
    }
}

fn path_matches_any(patterns: &[String], path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    patterns.iter().any(|p| path_matches(p, &path_str))
}

/// Three-pattern matcher:
/// - `"*"` — match anything.
/// - `"prefix/**"` or `"prefix/*"` — prefix match after stripping the
///   trailing `/**` or `/*`.
/// - literal — exact equality.
///
/// Leaves no ambiguity for operators reading the manifest.
fn path_matches(pattern: &str, candidate: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern
        .strip_suffix("/**")
        .or_else(|| pattern.strip_suffix("/*"))
    {
        // `/workspace/**` matches `/workspace/foo/bar` AND `/workspace`.
        return candidate == prefix || candidate.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return candidate.starts_with(prefix);
    }
    pattern == candidate
}

fn name_matches_any(patterns: &[String], candidate: &str) -> bool {
    patterns.iter().any(|p| {
        if p == "*" {
            return true;
        }
        if let Some(prefix) = p.strip_suffix('*') {
            return candidate.starts_with(prefix);
        }
        p == candidate
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk(write: &[&str], read: &[&str], env: &[&str]) -> ManifestPolicy {
        ManifestPolicy::new(PolicyManifest {
            fs: FsPolicy {
                read: read.iter().map(|s| s.to_string()).collect(),
                write: write.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            env: EnvPolicy {
                read_keys: env.iter().map(|s| s.to_string()).collect(),
            },
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn default_manifest_denies_everything() {
        let p = ManifestPolicy::new(PolicyManifest::default()).unwrap();
        assert!(matches!(
            p.check_fs_read(&PathBuf::from("/any")),
            Decision::Deny(_)
        ));
        assert!(matches!(
            p.check_fs_write(&PathBuf::from("/any")),
            Decision::Deny(_)
        ));
        assert!(matches!(p.check_env_read("HOME"), Decision::Deny(_)));
    }

    #[test]
    fn prefix_double_star_matches_nested_paths() {
        let p = mk(&[], &["/workspace/**"], &[]);
        assert!(matches!(
            p.check_fs_read(&PathBuf::from("/workspace/a/b/c")),
            Decision::Allow
        ));
        assert!(matches!(
            p.check_fs_read(&PathBuf::from("/workspace")),
            Decision::Allow
        ));
        assert!(matches!(
            p.check_fs_read(&PathBuf::from("/workspaceother")),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn single_star_suffix_is_prefix_match() {
        assert!(path_matches("/tmp/*", "/tmp/anything"));
        assert!(path_matches("/tmp/*", "/tmp"));
        assert!(!path_matches("/tmp/*", "/var/tmp/x"));
    }

    #[test]
    fn env_key_exact_and_prefix() {
        let p = mk(&[], &[], &["GITHUB_TOKEN", "AGENTD_*"]);
        assert!(matches!(p.check_env_read("GITHUB_TOKEN"), Decision::Allow));
        assert!(matches!(p.check_env_read("AGENTD_MODE"), Decision::Allow));
        assert!(matches!(p.check_env_read("AGENTD_"), Decision::Allow));
        assert!(matches!(p.check_env_read("HOME"), Decision::Deny(_)));
    }

    #[test]
    fn write_denied_when_only_read_configured() {
        let p = mk(&[], &["/workspace/**"], &[]);
        assert!(matches!(
            p.check_fs_write(&PathBuf::from("/workspace/out.txt")),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn list_falls_back_to_read_when_empty() {
        let p = mk(&[], &["/docs/**"], &[]);
        assert!(matches!(
            p.check_fs_list(&PathBuf::from("/docs/a")),
            Decision::Allow
        ));
    }

    #[test]
    fn list_uses_own_allowlist_when_set() {
        let p = ManifestPolicy::new(PolicyManifest {
            fs: FsPolicy {
                read: vec!["/docs/**".into()],
                list: vec!["/public/**".into()],
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        // /docs/ reads OK but list is only allowed on /public/.
        assert!(matches!(
            p.check_fs_read(&PathBuf::from("/docs/a")),
            Decision::Allow
        ));
        assert!(matches!(
            p.check_fs_list(&PathBuf::from("/docs/a")),
            Decision::Deny(_)
        ));
        assert!(matches!(
            p.check_fs_list(&PathBuf::from("/public/x")),
            Decision::Allow
        ));
    }

    #[test]
    fn star_allows_anything() {
        let p = mk(&["*"], &["*"], &["*"]);
        assert!(matches!(
            p.check_fs_read(&PathBuf::from("/whatever")),
            Decision::Allow
        ));
        assert!(matches!(
            p.check_fs_write(&PathBuf::from("/whatever")),
            Decision::Allow
        ));
        assert!(matches!(p.check_env_read("ANYKEY"), Decision::Allow));
    }

    #[test]
    fn manifest_parses_from_toml() {
        let toml_src = r#"
            [fs]
            read = ["/workspace/**"]
            write = ["/tmp/out/**"]

            [env]
            read_keys = ["DOCS_ROOT"]

            [mcp]
            tools = ["comment_on_page"]
            resources = ["docs://pages/*"]
        "#;
        let manifest: PolicyManifest = toml::from_str(toml_src).unwrap();
        assert_eq!(manifest.fs.read, vec!["/workspace/**"]);
        assert_eq!(manifest.env.read_keys, vec!["DOCS_ROOT"]);
        assert_eq!(manifest.mcp.tools, vec!["comment_on_page"]);
    }

    #[test]
    fn mcp_allowlist_from_manifest() {
        let m = PolicyManifest {
            mcp: McpPolicy {
                tools: vec!["t1".into()],
                resources: vec!["docs://pages/*".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let allow = m.mcp_allowlist();
        assert!(allow.tool_allowed("t1"));
        assert!(!allow.tool_allowed("t2"));
        assert!(allow.resource_allowed("docs://pages/42"));
    }

    #[test]
    fn deny_message_names_the_op() {
        let p = ManifestPolicy::new(PolicyManifest::default()).unwrap();
        match p.check_fs_write(&PathBuf::from("/tmp/x")) {
            Decision::Deny(msg) => assert!(msg.contains("fs_write"), "msg: {msg}"),
            Decision::Allow => panic!(),
        }
    }

    // -----------------------------------------------------------------
    // Rego policy (feature-gated)
    // -----------------------------------------------------------------
    #[cfg(feature = "policy-rego")]
    mod rego {
        use super::super::*;
        use serde_json::json;
        use std::path::PathBuf;

        fn mk_with_rego(static_fs_read: &[&str], rego_src: &str) -> ManifestPolicy {
            ManifestPolicy::new(PolicyManifest {
                fs: FsPolicy {
                    read: static_fs_read.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
                rego: Some(RegoConfig {
                    inline: Some(rego_src.into()),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap()
        }

        #[test]
        fn rego_compile_error_surfaces_at_new() {
            let err = ManifestPolicy::new(PolicyManifest {
                rego: Some(RegoConfig {
                    inline: Some("garbage that is not rego".into()),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap_err();
            assert!(err.contains("rego"), "err: {err}");
        }

        #[test]
        fn rego_and_static_both_must_allow() {
            // Static says /data/* is readable; Rego says only paths
            // with "safe" in them are allowed. AND semantics: only
            // /data/safe-* passes.
            let rego = r#"
                package agent
                default allow = false
                allow if {
                    contains(input.args.path, "safe")
                }
            "#;
            let p = mk_with_rego(&["/data/**"], rego);
            assert!(matches!(
                p.check_fs_read(&PathBuf::from("/data/safe-123")),
                Decision::Allow
            ));
            // Static allows /data/other but Rego says no.
            assert!(matches!(
                p.check_fs_read(&PathBuf::from("/data/other")),
                Decision::Deny(_)
            ));
            // Static denies /etc/shadow regardless of Rego.
            assert!(matches!(
                p.check_fs_read(&PathBuf::from("/etc/shadow")),
                Decision::Deny(_)
            ));
        }

        #[test]
        fn rego_sees_tool_and_args_fields() {
            // Policy that only allows http.request if method == POST.
            let rego = r#"
                package agent
                default allow = false
                allow if {
                    input.tool == "http.request"
                    input.args.method == "POST"
                }
            "#;
            let p = ManifestPolicy::new(PolicyManifest {
                http: HttpPolicy {
                    urls: vec!["https://*".into()],
                    ..Default::default()
                },
                rego: Some(RegoConfig {
                    inline: Some(rego.into()),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
            assert!(matches!(
                p.check_http_request("POST", "https://example.com"),
                Decision::Allow
            ));
            assert!(matches!(
                p.check_http_request("GET", "https://example.com"),
                Decision::Deny(_)
            ));
        }

        #[test]
        fn rego_extra_data_is_reachable() {
            // regorus merges `add_data` at the root of `data`, so
            // operators reference it as `data.<key>`. Parameterising
            // through `data` lets one shared `.rego` module be
            // imported by many agent deployments with per-deploy
            // tenant / region values.
            let rego = r#"
                package agent
                default allow = false
                allow if {
                    input.args.key == data.allowed_env_var
                }
            "#;
            let p = ManifestPolicy::new(PolicyManifest {
                env: EnvPolicy {
                    read_keys: vec!["*".into()],
                },
                rego: Some(RegoConfig {
                    inline: Some(rego.into()),
                    data: Some(json!({ "allowed_env_var": "MY_TOKEN" })),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
            assert!(matches!(p.check_env_read("MY_TOKEN"), Decision::Allow));
            assert!(matches!(p.check_env_read("OTHER"), Decision::Deny(_)));
        }

        #[test]
        fn rego_absent_is_allow_passthrough() {
            // No [policy.rego] block at all — static allowlist is
            // authoritative.
            let p = ManifestPolicy::new(PolicyManifest {
                fs: FsPolicy {
                    read: vec!["/data/**".into()],
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
            assert!(matches!(
                p.check_fs_read(&PathBuf::from("/data/x")),
                Decision::Allow
            ));
        }

        #[test]
        fn rego_default_allow_false_denies_when_no_rule_matches() {
            let rego = "package agent\ndefault allow = false\n";
            let p = mk_with_rego(&["/data/**"], rego);
            // Static allows but Rego default is false.
            match p.check_fs_read(&PathBuf::from("/data/x")) {
                Decision::Deny(msg) => assert!(msg.contains("rego")),
                _ => panic!("should deny"),
            }
        }

        #[test]
        fn rego_config_file_and_inline_exclusive() {
            let err = ManifestPolicy::new(PolicyManifest {
                rego: Some(RegoConfig {
                    inline: Some("package agent\nallow = true".into()),
                    file: Some(PathBuf::from("/tmp/unused.rego")),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap_err();
            assert!(err.contains("mutually exclusive"));
        }
    }
}
