// SPDX-License-Identifier: AGPL-3.0-only
//! The supervisor's own process hardening (RFC 0044 §5.13 item 1): what it
//! holds in memory — a sealed store's key, a client's signing key, a token
//! it renews — stays in the process. The supervisor is made non-dumpable
//! (`PR_SET_DUMPABLE` 0), so a process of the same uid can neither attach to
//! it nor read its memory or environment through `/proc/<pid>/`, and its
//! core limit is set to zero, so no core dump ever holds a secret. Every
//! child inherits the core limit; `execve` makes a child dumpable again,
//! and none is handed a secret of the supervisor's to read.
//!
//! **What a child's environment holds** (RFC 0044 P2, an allow-list). Every
//! child it starts — turn worker, subagent, think child, instance — is
//! started with ([`child_env`]):
//!
//! - the baseline ([`BASELINE`]): `PATH`, `HOME`, `TMPDIR`, `TZ`, `LANG`,
//!   `AGENTD_CRED_DIR`, `RUST_LOG`, `NO_PROXY`, and every `XDG_*`, `LC_*`,
//!   `SSL_CERT_*`, `OTEL_*` and `*_PROXY`;
//! - the variables its own spawn names: each `{{secret:NAME}}` its payload's
//!   connections — never its conversation — (or an instance's composed
//!   configuration) reference, `AWS_*` with the `aws` sources' endpoint
//!   overrides when an `aws` credential is routed to it, and the
//!   `AGENTD_INTELLIGENCE_TOKEN*` credentials of the top-level model
//!   connection, which a worker reads itself as it dials;
//! - the operator's `spawn.env_passthrough` ([`set_passthrough`]);
//!
//! each only when this process has it, and never a variable a
//! supervisor-only secret is read from ([`set_reserved`]). Nothing else
//! crosses: a variable a child read that is in none of these is added to
//! `spawn.env_passthrough`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

use serde_json::Value;

/// Make this process non-dumpable and unable to dump core. Best effort: a
/// kernel that refuses either is reported in the returned list, and the
/// process runs on.
pub fn supervisor() -> Vec<String> {
    let mut refused = Vec::new();
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl with integer arguments only.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            refused.push(format!(
                "PR_SET_DUMPABLE: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    #[cfg(unix)]
    {
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: setrlimit with a valid pointer to a stack value.
        if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) } != 0 {
            refused.push(format!("RLIMIT_CORE: {}", std::io::Error::last_os_error()));
        }
    }
    refused
}

/// Overwrite `b` with zeros, in writes the compiler may not drop: key
/// material is wiped once it has been used.
pub fn wipe(b: &mut [u8]) {
    for x in b.iter_mut() {
        // SAFETY: a valid, aligned reference to a byte of `b`.
        unsafe { std::ptr::write_volatile(x, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// The baseline every child is started with, when this process has it:
/// names, then `PREFIX*` and `*SUFFIX` patterns.
pub const BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TZ",
    "LANG",
    "AGENTD_CRED_DIR",
    "RUST_LOG",
    "NO_PROXY",
    "no_proxy",
    "XDG_*",
    "LC_*",
    "SSL_CERT_*",
    "OTEL_*",
    "*_PROXY",
    "*_proxy",
];

/// What an `aws` credential routed to a child reads besides `AWS_*`: the
/// endpoints its sources may be pointed at.
const AWS_OVERRIDES: &[&str] = &[
    "AGENTD_IMDS_ENDPOINT",
    "AGENTD_STS_ENDPOINT",
    "AGENTD_SSO_OIDC",
    "AGENTD_SSO_PORTAL",
];

/// The names of the environment variables only the supervisor reads.
static RESERVED: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());
/// `spawn.env_passthrough`.
static PASSTHROUGH: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Set the variables no child is started with: the configuration's
/// supervisor-only secret references ([`crate::config::settings::Settings::supervisor_only_env`]).
/// Called at boot and by every reload that applies.
pub fn set_reserved(names: BTreeSet<String>) {
    *RESERVED.write().unwrap_or_else(|e| e.into_inner()) = names;
}

/// The variables no child is started with.
pub fn reserved() -> BTreeSet<String> {
    RESERVED.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Set `spawn.env_passthrough`: called at boot and by every reload that
/// applies.
pub fn set_passthrough(entries: Vec<String>) {
    *PASSTHROUGH.write().unwrap_or_else(|e| e.into_inner()) = entries;
}

/// Whether `name` is `pattern`: the name itself, `PREFIX*` or `*SUFFIX`.
pub fn matches(pattern: &str, name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        name.ends_with(suffix)
    } else {
        pattern == name
    }
}

/// The variables a child's spawn names in `doc` — its payload, or an
/// instance's composed configuration: every `{{secret:NAME}}` it
/// references, and when an `aws` credential is routed to it, `AWS_*` and the
/// `aws` sources' endpoint overrides.
pub fn wanted_by(doc: &Value) -> BTreeSet<String> {
    fn aws(v: &Value) -> bool {
        match v {
            Value::Object(o) => {
                o.get("kind").and_then(Value::as_str) == Some("aws") || o.values().any(aws)
            }
            Value::Array(a) => a.iter().any(aws),
            _ => false,
        }
    }
    let mut out: BTreeSet<String> = crate::sec::secret::secret_env_names(doc)
        .into_iter()
        .collect();
    if aws(doc) {
        out.insert("AWS_*".into());
        out.extend(AWS_OVERRIDES.iter().map(|s| s.to_string()));
    }
    out
}

/// The per-endpoint intelligence credentials a worker reads itself when it
/// dials the top-level connection (`intel::endpoints`):
/// `AGENTD_INTELLIGENCE_TOKEN`, `_FILE`, and `_<N>`/`_<N>_FILE` of each
/// failover endpoint.
const INTELLIGENCE_TOKENS: &[&str] = &["AGENTD_INTELLIGENCE_TOKEN", "AGENTD_INTELLIGENCE_TOKEN_*"];

/// The variables a turn worker's or subagent's spawn names: [`wanted_by`]
/// its connections alone — the model's, the MCP servers', the A2A peers',
/// the TLS anchor and the AAuth identity — never its conversation. An
/// instruction, a message, a seed or a tool result that spells
/// `{{secret:NAME}}` is text from outside, and brings nothing. And the
/// intelligence credentials ([`INTELLIGENCE_TOKENS`]): every such child
/// dials a model, and a reload or a budget's degrade can move it onto the
/// top-level connection after it started.
pub fn wanted_by_spawn(p: &crate::subagent::protocol::SpawnPayload) -> BTreeSet<String> {
    let connections = serde_json::json!({
        "intelligence": p.intelligence,
        "mcp_servers": p.mcp_servers,
        "a2a_peers": p.a2a_peers,
        "tls_ca": p.tls_ca,
        "aauth": p.aauth,
        "escalate": p.turn.as_ref().and_then(|t| t.decision.as_ref()).map(|d| &d.escalate),
    });
    let mut out = wanted_by(&connections);
    out.extend(INTELLIGENCE_TOKENS.iter().map(|s| s.to_string()));
    out
}

/// The environment a child is started with, from `parent`: the baseline,
/// `wanted` ([`wanted_by`]) and `spawn.env_passthrough`, never a reserved
/// name.
pub fn child_env_of(
    parent: impl Iterator<Item = (String, String)>,
    wanted: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let reserved = reserved();
    let passthrough = PASSTHROUGH
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let test_hooks = cfg!(any(feature = "internal-mocks", debug_assertions));
    parent
        .filter(|(k, _)| {
            !reserved.contains(k)
                && (BASELINE.iter().any(|p| matches(p, k))
                    || wanted.iter().any(|p| matches(p, k))
                    || passthrough.iter().any(|p| matches(p, k))
                    // The test hooks a debug or `internal-mocks` build reads.
                    || (test_hooks && k.starts_with("AGENTD_TEST_")))
        })
        .collect()
}

/// [`child_env_of`] this process's environment.
pub fn child_env(wanted: &BTreeSet<String>) -> BTreeMap<String, String> {
    child_env_of(std::env::vars(), wanted)
}

/// Start `cmd` with [`child_env`] and whatever `cmd` was given itself — no
/// other variable of this process. The environment it will start with is
/// returned.
pub fn confine(
    cmd: &mut std::process::Command,
    wanted: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let given: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .filter_map(|(k, v)| {
            Some((
                k.to_str()?.to_string(),
                v.and_then(|v| v.to_str()).map(str::to_string),
            ))
        })
        .collect();
    let mut env = child_env(wanted);
    for (k, v) in given {
        match v {
            Some(v) => {
                env.insert(k, v);
            }
            None => {
                env.remove(&k);
            }
        }
    }
    cmd.env_clear().envs(&env);
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child started through [`confine`] has the baseline, what its spawn
    /// names and the operator's passthrough — and nothing else: not another
    /// variable of the parent's, not a reserved name even when its spawn
    /// names it.
    #[test]
    fn a_child_sees_only_its_allow_listed_environment() {
        // SAFETY: names no other test reads.
        unsafe {
            std::env::set_var("HARDEN_TEST_KEY", "k");
            std::env::set_var("HARDEN_TEST_ROUTED", "r");
            std::env::set_var("HARDEN_TEST_PASS_1", "p");
            std::env::set_var("HARDEN_TEST_STRAY", "s");
            std::env::set_var("HARDEN_TEST_PROXY", "x");
        }
        set_reserved(["HARDEN_TEST_KEY".to_string()].into_iter().collect());
        set_passthrough(vec!["HARDEN_TEST_PASS_*".into()]);
        let payload = serde_json::json!({
            "mcp_servers": [{"headers": {"a": "Bearer {{secret:HARDEN_TEST_ROUTED}}"}}],
            "intelligence": {"token": "{{secret:HARDEN_TEST_KEY}}"},
        });
        let mut cmd = std::process::Command::new("env");
        cmd.env("HARDEN_TEST_GIVEN", "g");
        let said = confine(&mut cmd, &wanted_by(&payload));
        let out = cmd.output().unwrap();
        let env = String::from_utf8_lossy(&out.stdout);
        set_reserved(BTreeSet::new());
        set_passthrough(Vec::new());
        for kept in [
            "HARDEN_TEST_ROUTED=r",
            "HARDEN_TEST_PASS_1=p",
            "HARDEN_TEST_GIVEN=g",
            "HARDEN_TEST_PROXY=x",
        ] {
            assert!(env.contains(kept), "{kept} missing: {env}");
        }
        assert!(env.lines().any(|l| l.starts_with("PATH=")), "{env}");
        for gone in ["HARDEN_TEST_KEY=", "HARDEN_TEST_STRAY="] {
            assert!(!env.contains(gone), "{gone} crossed: {env}");
        }
        let lines: BTreeSet<&str> = env.lines().collect();
        let said: BTreeSet<String> = said.iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert_eq!(
            lines,
            said.iter().map(String::as_str).collect::<BTreeSet<_>>(),
            "the environment returned is the child's"
        );
    }

    /// An `aws` credential routed to a child brings `AWS_*` and its sources'
    /// overrides; none routed, none.
    #[test]
    fn an_aws_credential_brings_its_variables() {
        let parent = || {
            [
                ("AWS_ACCESS_KEY_ID", "a"),
                ("AGENTD_IMDS_ENDPOINT", "http://127.0.0.1:1"),
                ("AWS_PROFILE", "p"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
        };
        let routed = wanted_by(&serde_json::json!({"intelligence": {"aws_auth": {"kind": "aws"}}}));
        let env = child_env_of(parent(), &routed);
        assert_eq!(env.len(), 3, "{env:?}");
        let none = child_env_of(
            parent(),
            &wanted_by(&serde_json::json!({"intelligence": {}})),
        );
        assert!(none.is_empty(), "{none:?}");
        assert!(matches("*_PROXY", "HTTPS_PROXY") && !matches("*_PROXY", "PROXY"));
        assert!(matches("OTEL_*", "OTEL_X") && !matches("OTEL_*", "XOTEL_"));
    }
}
