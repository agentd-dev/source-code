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
    pub http: HttpPolicy,
    #[serde(default)]
    pub shell: ShellPolicy,
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

// ---------------------------------------------------------------------------
// ManifestPolicy impl
// ---------------------------------------------------------------------------

/// [`Policy`] implementation driven by a [`PolicyManifest`].
#[derive(Debug)]
pub struct ManifestPolicy {
    manifest: PolicyManifest,
}

impl ManifestPolicy {
    pub fn new(manifest: PolicyManifest) -> Self {
        Self { manifest }
    }

    pub fn manifest(&self) -> &PolicyManifest {
        &self.manifest
    }
}

impl Policy for ManifestPolicy {
    fn check_fs_read(&self, path: &Path) -> Decision {
        gate_path("fs_read", path, &self.manifest.fs.read)
    }

    fn check_fs_write(&self, path: &Path) -> Decision {
        gate_path("fs_write", path, &self.manifest.fs.write)
    }

    fn check_fs_delete(&self, path: &Path) -> Decision {
        gate_path("fs_delete", path, &self.manifest.fs.delete)
    }

    fn check_fs_list(&self, path: &Path) -> Decision {
        // Fall back to the read set when `list` is empty — listing a
        // directory you can read is almost always fine.
        let patterns = if self.manifest.fs.list.is_empty() {
            &self.manifest.fs.read
        } else {
            &self.manifest.fs.list
        };
        gate_path("fs_list", path, patterns)
    }

    fn check_env_read(&self, key: &str) -> Decision {
        if name_matches_any(&self.manifest.env.read_keys, key) {
            Decision::Allow
        } else {
            Decision::Deny(format!(
                "env var `{key}` is not in the allowlist ({} configured)",
                self.manifest.env.read_keys.len()
            ))
        }
    }

    fn check_http_request(&self, method: &str, url: &str) -> Decision {
        check_http_static(&self.manifest.http, method, url)
    }

    fn check_shell_run(&self, command: &Path) -> Decision {
        if self.manifest.shell.commands.is_empty() {
            Decision::Deny("shell_run denied: no `shell.commands` allowlist configured".into())
        } else if path_matches_any(&self.manifest.shell.commands, command) {
            Decision::Allow
        } else {
            Decision::Deny(format!(
                "shell_run command `{}` not covered by any allowlist pattern",
                command.display()
            ))
        }
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
    }

    #[test]
    fn default_manifest_denies_everything() {
        let p = ManifestPolicy::new(PolicyManifest::default());
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
        });
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
        "#;
        let manifest: PolicyManifest = toml::from_str(toml_src).unwrap();
        assert_eq!(manifest.fs.read, vec!["/workspace/**"]);
        assert_eq!(manifest.env.read_keys, vec!["DOCS_ROOT"]);
    }

    #[test]
    fn deny_message_names_the_op() {
        let p = ManifestPolicy::new(PolicyManifest::default());
        match p.check_fs_write(&PathBuf::from("/tmp/x")) {
            Decision::Deny(msg) => assert!(msg.contains("fs_write"), "msg: {msg}"),
            Decision::Allow => panic!(),
        }
    }
}
