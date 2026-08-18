// SPDX-License-Identifier: AGPL-3.0-only
//! The local-filesystem store (RFC 0033) — `store.kind: file`. One file per
//! key, atomic writes, an exclusive instance lock, and traversal closed at the
//! adapter.
//!
//! This is the adapter that lets a laptop satisfy durability without standing
//! up a coordination backend, so it is the default for a long-lived instance
//! (RFC 0033 §5). It is deliberately NOT a fleet store: a directory has no
//! compare-and-set a second process would respect, so instead of pretending,
//! `open` takes an exclusive `flock` and a second instance fails at startup
//! with the holder's pid. Finding that out at startup is much cheaper than
//! finding it out from interleaved runs.
//!
//! What it is not: a breach of "agentd runs no code of its own". That rule is
//! about the AGENT's tools and the trust boundary — there is no `fs` tool, and
//! this adapter is not reachable by anything the model can call. It is the
//! runtime's own ledger, on the same side of the boundary as the credential
//! cache in `auth/cache.rs`, whose directory convention it reuses.

use super::{KeySeq, PutOutcome, Store, StoreError};
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The file a key's latest envelope lives in, relative to the root.
const EXT: &str = "json";
/// The lock that makes the single-writer property enforced rather than assumed.
const LOCK: &str = ".lock";

/// `Debug` prints the root only: a store handle appears in test assertions and
/// in error context, and the lock's file descriptor is noise there.
#[derive(Debug)]
pub struct FileStore {
    root: PathBuf,
    /// Held for the life of the store: dropping it releases the `flock`.
    _lock: LockFile,
}

impl FileStore {
    /// Open `root`, creating it, and take the exclusive instance lock.
    pub fn open(root: &Path) -> Result<FileStore, StoreError> {
        fs::create_dir_all(root)
            .map_err(|e| StoreError::Io(format!("store dir {}: {e}", root.display())))?;
        restrict_dir(root);
        let lock = LockFile::acquire(&root.join(LOCK)).map_err(|e| StoreError::Io(e))?;
        Ok(FileStore {
            root: root.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<encoded segment>/…/<encoded id>.json`.
    ///
    /// Ids reach this adapter from run ids, task ids and context ids, so a
    /// traversal is closed HERE rather than assumed to have been closed
    /// upstream: every segment is percent-encoded, which makes `.`, `..` and a
    /// separator unrepresentable in the encoded form.
    fn path_of(&self, key: &str) -> Result<PathBuf, StoreError> {
        let mut p = self.root.clone();
        let segs: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return Err(StoreError::Mapping("empty store key".into()));
        }
        for (i, seg) in segs.iter().enumerate() {
            if seg.contains('\0') {
                return Err(StoreError::Mapping("store key contains NUL".into()));
            }
            let enc = encode(seg);
            if i + 1 == segs.len() {
                p.push(format!("{enc}.{EXT}"));
            } else {
                p.push(enc);
            }
        }
        Ok(p)
    }

    /// Read the envelope at `path`, if the file exists and parses.
    fn read(&self, path: &Path) -> Result<Option<Value>, StoreError> {
        match fs::read(path) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| StoreError::Corrupt(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io(format!("read {}: {e}", path.display()))),
        }
    }
}

impl Store for FileStore {
    fn put(&self, key: &str, seq: u64, envelope: &Value) -> Result<PutOutcome, StoreError> {
        let path = self.path_of(key)?;
        // CAS against what is on disk. Safe without a per-key lock because the
        // instance lock makes this process the only writer of this root.
        if let Some(cur) = self.read(&path)?
            && let Some(l) = cur.get("seq").and_then(Value::as_u64)
            && seq <= l
        {
            return Ok(PutOutcome::Conflict {
                latest_seq: Some(l),
            });
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| StoreError::Io(format!("mkdir {}: {e}", parent.display())))?;
            restrict_dir(parent);
        }
        let body = serde_json::to_vec(envelope)
            .map_err(|e| StoreError::Mapping(format!("envelope: {e}")))?;
        write_atomic(&path, &body).map_err(StoreError::Io)?;
        Ok(PutOutcome::Ok)
    }

    fn get(&self, key: &str, _seq: Option<u64>) -> Result<Option<Value>, StoreError> {
        // Latest-only, like the http adapter: a pinned seq reads as the latest.
        let v = self.read(&self.path_of(key)?)?;
        // A tombstone (latest state null) reads as absent, per RFC 0025 §3.
        Ok(v.filter(|v| !v.get("state").is_some_and(Value::is_null)))
    }

    fn list(&self, prefix: &str) -> Result<Vec<KeySeq>, StoreError> {
        let mut dir = self.root.clone();
        for seg in prefix.split('/').filter(|s| !s.is_empty()) {
            dir.push(encode(seg));
        }
        let mut out = Vec::new();
        walk(&dir, &self.root, &mut out)?;
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        match fs::remove_file(self.path_of(key)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(format!("delete {key}: {e}"))),
        }
    }

    fn kind(&self) -> &'static str {
        "file"
    }
}

/// Recursively collect every `*.json` under `dir`, keyed by its path relative
/// to `root` with the segments decoded back to the original key.
fn walk(dir: &Path, root: &Path, out: &mut Vec<KeySeq>) -> Result<(), StoreError> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(StoreError::Io(format!("list {}: {e}", dir.display()))),
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() {
            walk(&p, root, out)?;
        } else if p.extension().is_some_and(|e| e == EXT) {
            let Ok(rel) = p.strip_prefix(root) else {
                continue;
            };
            let mut segs: Vec<String> = rel
                .components()
                .map(|c| decode(&c.as_os_str().to_string_lossy()))
                .collect();
            if let Some(last) = segs.last_mut()
                && let Some(stem) = last.strip_suffix(&format!(".{EXT}"))
            {
                *last = stem.to_string();
            }
            let seq = fs::read(&p)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v.get("seq").and_then(Value::as_u64));
            out.push(KeySeq {
                key: segs.join("/"),
                seq,
            });
        }
    }
    Ok(())
}

/// Write `body` to `path` so a crash leaves either the old bytes or the new
/// ones and never a partial file: a temp file in the same directory, fsync'd,
/// renamed over the target, then the directory itself fsync'd so the rename is
/// durable too.
fn write_atomic(path: &Path, body: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| "no parent dir".to_string())?;
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        restrict_file(&f);
        f.write_all(body)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename into {}: {e}", path.display())
    })?;
    // The rename itself is only durable once the directory entry is synced.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// `0700` on a state directory: it holds conversation content and tool results.
fn restrict_dir(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = p;
}

/// `0600` on a state file, set before any bytes are written.
fn restrict_file(f: &fs::File) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = f;
}

/// Percent-encode everything outside an unreserved set. `.`/`..`/`/` become
/// unrepresentable, which is what closes traversal.
fn encode(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(seg: &str) -> String {
    let b = seg.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An exclusive `flock` held for the life of the store.
#[derive(Debug)]
struct LockFile {
    _file: fs::File,
}

impl LockFile {
    fn acquire(path: &Path) -> Result<LockFile, String> {
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("lock {}: {e}", path.display()))?;
        restrict_file(&file);
        #[cfg(unix)]
        {
            use std::io::{Read, Seek, Write};
            use std::os::unix::io::AsRawFd;
            // Non-blocking: a held lock must fail fast and say who holds it,
            // not stall a startup that is never going to succeed.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let mut holder = String::new();
                let mut f = &file;
                let _ = f.rewind();
                let _ = f.read_to_string(&mut holder);
                let who = holder.trim();
                let who = if who.is_empty() {
                    "another process".to_string()
                } else {
                    format!("pid {who}")
                };
                return Err(format!(
                    "{} is locked by {who} — another agentd is using this state \
                     directory; give this instance its own agent.name or store.file.path",
                    path.parent().unwrap_or(path).display()
                ));
            }
            let mut f = &file;
            let _ = f.rewind();
            let _ = f.set_len(0);
            let _ = write!(f, "{}", std::process::id());
            let _ = f.flush();
        }
        Ok(LockFile { _file: file })
    }
}
