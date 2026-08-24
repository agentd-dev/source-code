// SPDX-License-Identifier: AGPL-3.0-only
//! The endpoint-credential cache: access + refresh tokens with their expiry,
//! keyed by a hash of the login target (e.g. `mcp:github`, `intelligence`).
//!
//! Two backings share one record shape. The **file** cache is the path the
//! daemon uses: an interactive login writes a per-user `0600` file, the daemon
//! reads it at startup to seed a provider, refreshes in memory during a run,
//! and re-reads it (re-refreshing from the refresh token) after a restart. The
//! **durable** helpers ([`load`] / [`store`] / [`evict`]) hold the same record
//! in the durable store under [`Kind::Cred`] — the store-backed equivalent for
//! an embedder that keeps credentials alongside the rest of its state.
//!
//! **Redaction:** a cred record holds live tokens, so it is excluded from all
//! logs, audit records, and the `agent://` read surface. [`Kind::Cred`] is also
//! deliberately non-indexed — it never appears in the manifest, so nothing that
//! walks the manifest to enumerate or replicate state can reach a credential.

use crate::sha::sha256_hex;
use crate::state::{Durable, Kind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch (for `expires_at_ms`).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A cached credential for one endpoint. Never logged.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CachedCred {
    #[serde(default)]
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Absolute expiry (ms since the Unix epoch); `0` = unknown / non-expiring.
    #[serde(default)]
    pub expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    /// Provider-specific extras — e.g. temporary AWS credentials from an SSO
    /// login (`aws_access_key_id` / `aws_secret_access_key` / `aws_session_token`).
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl CachedCred {
    /// Whether the access token is still usable at `now_ms`, keeping `skew_ms`
    /// of headroom so an in-flight request never rides a just-expired token.
    pub fn valid_at(&self, now_ms: u64, skew_ms: u64) -> bool {
        self.expires_at_ms == 0 || now_ms.saturating_add(skew_ms) < self.expires_at_ms
    }
}

/// The stable, filesystem-safe record id for a login `target`.
pub fn cred_id(target: &str) -> String {
    sha256_hex(target.as_bytes())
}

/// Load the cached credential for `target`, if present and parseable.
pub fn load(durable: &Durable, target: &str) -> Option<CachedCred> {
    let env = durable.get(Kind::Cred, &cred_id(target)).ok()??;
    serde_json::from_value(env.state).ok()
}

/// Store (or replace) the credential for `target`.
pub fn store(durable: &Durable, target: &str, cred: &CachedCred) -> Result<(), String> {
    let value = serde_json::to_value(cred).map_err(|e| e.to_string())?;
    durable
        .put(Kind::Cred, &cred_id(target), value, None)
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

/// Evict the credential for `target` (`agentd logout`).
pub fn evict(durable: &Durable, target: &str) -> Result<(), String> {
    durable
        .delete(Kind::Cred, &cred_id(target))
        .map_err(|e| format!("{e}"))
}

// --- file-backed cache (the interactive `agentd login` handoff) --------------
//
// `agentd login` runs on a human's machine, where the configured durable store
// may be a remote backend the login has no business opening a connection to.
// The obtained token is cached in a per-user file (0600), the same path the
// daemon reads at startup to seed a provider — the pattern `aws`, `gcloud` and
// `kubectl` use for OAuth tokens.

/// The default per-user credential directory, in precedence order:
/// `$AGENTD_CRED_DIR`, else `$XDG_STATE_HOME/agentd/creds`, else
/// `$HOME/.local/state/agentd/creds`, else the OS temp dir. The temp-dir
/// fallback only applies to an environment with no `HOME` at all; the files
/// written there are still `0600`.
pub fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AGENTD_CRED_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(d).join("agentd").join("creds");
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h)
            .join(".local")
            .join("state")
            .join("agentd")
            .join("creds");
    }
    std::env::temp_dir().join("agentd").join("creds")
}

fn file_path(dir: &std::path::Path, target: &str) -> PathBuf {
    dir.join(format!("{}.json", cred_id(target)))
}

/// Load a credential from the file cache, if present and parseable.
pub fn load_file(dir: &std::path::Path, target: &str) -> Option<CachedCred> {
    let bytes = std::fs::read(file_path(dir, target)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write a credential to the file cache (creating the dir; the file is `0600`).
pub fn store_file(dir: &std::path::Path, target: &str, cred: &CachedCred) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cred dir {}: {e}", dir.display()))?;
    let path = file_path(dir, target);
    let json = serde_json::to_vec_pretty(cred).map_err(|e| e.to_string())?;
    std::fs::write(&path, &json).map_err(|e| format!("write {}: {e}", path.display()))?;
    // Owner-only (0600) — the file holds live tokens. The mode is applied
    // after the write, so the create itself still honours the process umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Evict a credential from the file cache (`agentd logout`). Absent = success.
pub fn evict_file(dir: &std::path::Path, target: &str) -> Result<(), String> {
    match std::fs::remove_file(file_path(dir, target)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cred_id_is_stable_and_distinct() {
        assert_eq!(cred_id("mcp:github"), cred_id("mcp:github"));
        assert_ne!(cred_id("mcp:github"), cred_id("intelligence"));
        // 64 hex chars (sha-256).
        assert_eq!(cred_id("x").len(), 64);
    }

    #[test]
    fn valid_at_honours_expiry_and_skew() {
        let c = CachedCred {
            access_token: "t".into(),
            expires_at_ms: 1_000,
            ..Default::default()
        };
        assert!(c.valid_at(0, 0));
        assert!(c.valid_at(900, 50));
        assert!(!c.valid_at(960, 50)); // inside the skew window
        assert!(!c.valid_at(1_000, 0));
        // Non-expiring (0) is always valid.
        let never = CachedCred {
            access_token: "t".into(),
            expires_at_ms: 0,
            ..Default::default()
        };
        assert!(never.valid_at(u64::MAX - 1, 10));
    }
}
