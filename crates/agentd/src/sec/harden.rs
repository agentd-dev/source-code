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
//! And every child it starts — turn worker, subagent, think child, instance
//! — is started without the environment variables a supervisor-only secret
//! is read from ([`set_reserved`], [`reserved`]): the parent environment
//! minus those names, nothing else taken away.

use std::collections::BTreeSet;
use std::sync::RwLock;

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

/// The names of the environment variables only the supervisor reads.
static RESERVED: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

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

/// Remove the reserved variables from `cmd`'s environment.
pub fn strip(cmd: &mut std::process::Command) {
    for k in reserved() {
        cmd.env_remove(k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child started through [`strip`] has the parent environment less
    /// the reserved names, and nothing else less.
    #[test]
    fn a_child_is_started_without_the_reserved_names() {
        // SAFETY: names no other test reads.
        unsafe {
            std::env::set_var("HARDEN_TEST_KEY", "k");
            std::env::set_var("HARDEN_TEST_KEEP", "v");
        }
        set_reserved(["HARDEN_TEST_KEY".to_string()].into_iter().collect());
        let mut cmd = std::process::Command::new("env");
        strip(&mut cmd);
        let out = cmd.output().unwrap();
        let env = String::from_utf8_lossy(&out.stdout);
        assert!(!env.contains("HARDEN_TEST_KEY="), "{env}");
        assert!(env.contains("HARDEN_TEST_KEEP=v"), "{env}");
        set_reserved(BTreeSet::new());
    }
}
