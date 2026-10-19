// SPDX-License-Identifier: AGPL-3.0-only
//! The endpoint-credential cache: access + refresh tokens with their expiry,
//! keyed by a hash of the login target (e.g. `mcp:github`, `intelligence`),
//! each record bound to the authorization server that issued it.
//!
//! Two backings share one record shape. The **file** cache is the path the
//! daemon uses: an interactive login writes a per-user `0600` file, and every
//! process of the daemon reads it to seed a provider and writes each renewal
//! back, under the target's [`FileLock`], so a restart renews from the token
//! the server last issued. The
//! **durable** helpers ([`load`] / [`store`] / [`evict`]) hold the same record
//! in the durable store under [`Kind::Cred`] — the store-backed equivalent for
//! an embedder that keeps credentials alongside the rest of its state.
//!
//! **Redaction:** a cred record holds live tokens, so it is excluded from all
//! logs, audit records, and every status read. [`Kind::Cred`] is also
//! deliberately non-indexed — it never appears in the manifest, so nothing that
//! walks the manifest to enumerate or replicate state can reach a credential.

use crate::sha::sha256_hex;
use crate::state::{Durable, Kind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// The authorization server the token came from: its issuer, or its
    /// token endpoint where the login knew no issuer. The record is used,
    /// renewed and revoked only under that server, and one that names none
    /// under no server at all (MCP 2026-07-28, authorization server binding).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_server: Option<String>,
    /// The scope the token was granted, as the token response said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Scopes a server's `insufficient_scope` challenge asked for that a
    /// renewal could not get: the next `agentd --login` asks for them too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub step_up: Vec<String>,
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

/// Evict the credential for `target` (`agentd --logout`).
pub fn evict(durable: &Durable, target: &str) -> Result<(), String> {
    durable
        .delete(Kind::Cred, &cred_id(target))
        .map_err(|e| format!("{e}"))
}

// --- file-backed cache (the interactive `agentd --login` handoff) ------------
//
// `agentd --login` runs on a human's machine, where the configured durable store
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

fn file_path(dir: &Path, target: &str) -> PathBuf {
    dir.join(format!("{}.json", cred_id(target)))
}

/// Load a credential from the file cache, if present and parseable.
pub fn load_file(dir: &Path, target: &str) -> Option<CachedCred> {
    let bytes = std::fs::read(file_path(dir, target)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Names each temp file a process writes apart from its others.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write a credential to the file cache (creating the dir). The file is
/// replaced whole: the record goes to a `0600` temp file beside it, synced,
/// then renamed over it, so a reader in another process sees the old record
/// or the new one and never a part, and a crash or a full disk mid-write
/// leaves the old one.
pub fn store_file(dir: &Path, target: &str, cred: &CachedCred) -> Result<(), String> {
    use std::io::Write;
    std::fs::create_dir_all(dir).map_err(|e| format!("cred dir {}: {e}", dir.display()))?;
    let path = file_path(dir, target);
    let json = serde_json::to_vec_pretty(cred).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        cred_id(target),
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create(true).truncate(true);
        // Owner-only from the create on: the file holds live tokens.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        let mut f = open.open(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", path.display()));
    }
    // The rename is durable once the directory entry is synced.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Evict a credential from the file cache. Absent = success.
pub fn evict_file(dir: &Path, target: &str) -> Result<(), String> {
    match std::fs::remove_file(file_path(dir, target)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{e}")),
    }
}

/// Remove `target`'s credential from the file cache (`agentd --logout`)
/// under its [`FileLock`], and return the record it held. Absent =
/// `Ok(None)`.
pub fn take_file(dir: &Path, target: &str, wait: Duration) -> Result<Option<CachedCred>, String> {
    if !file_path(dir, target).exists() {
        return Ok(None);
    }
    let _held = lock_file(dir, target, wait)?;
    let cred = load_file(dir, target);
    evict_file(dir, target)?;
    Ok(cred)
}

/// An exclusive hold on one target's file-cache entry, across every process
/// that uses the directory: taken to renew a token and write it back, to
/// write a login and to delete one, so two processes never renew with the
/// same refresh token and a renewal in flight never writes back a login a
/// logout removed. Released when dropped. It is an advisory `flock` on a
/// `<id>.lock` file beside the record, which stays.
pub struct FileLock {
    _file: std::fs::File,
}

/// Take `target`'s [`FileLock`] in `dir`, waiting up to `wait` for a holder
/// to release it.
pub fn lock_file(dir: &Path, target: &str, wait: Duration) -> Result<FileLock, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cred dir {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.lock", cred_id(target)));
    let mut open = std::fs::OpenOptions::new();
    open.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    let file = open
        .open(&path)
        .map_err(|e| format!("lock {}: {e}", path.display()))?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(FileLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < wait => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "the login cache for {target} is held by another request or process, which did not release it within {}s",
                    wait.as_secs()
                ));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(format!("lock {}: {e}", path.display()));
            }
        }
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

    /// A record is replaced whole: a reader in another process, reading
    /// while it is written over and over, finds one record or the next and
    /// never none, the file is `0600`, and no temp file is left beside it.
    #[test]
    fn a_record_rewritten_is_never_read_in_part() {
        use std::sync::atomic::AtomicBool;
        let dir = tempfile::tempdir().unwrap();
        let cred = |n: usize| CachedCred {
            access_token: format!("at-{n}-{}", "x".repeat(8192)),
            ..Default::default()
        };
        store_file(dir.path(), "mcp:x", &cred(0)).unwrap();
        let done = AtomicBool::new(false);
        let torn = std::thread::scope(|s| {
            let reader = s.spawn(|| {
                let mut torn = 0;
                while !done.load(Ordering::SeqCst) {
                    if load_file(dir.path(), "mcp:x").is_none() {
                        torn += 1;
                    }
                }
                torn
            });
            for n in 1..=300 {
                store_file(dir.path(), "mcp:x", &cred(n)).unwrap();
            }
            done.store(true, Ordering::SeqCst);
            reader.join().unwrap()
        });
        assert_eq!(torn, 0, "reads that found no record");
        assert!(
            load_file(dir.path(), "mcp:x")
                .unwrap()
                .access_token
                .starts_with("at-300-")
        );
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [format!("{}.json", cred_id("mcp:x"))]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file_path(dir.path(), "mcp:x"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
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
