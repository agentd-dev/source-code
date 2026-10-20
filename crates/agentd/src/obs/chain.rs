// SPDX-License-Identifier: AGPL-3.0-only
//! The **hash-chained audit log** (RFC 0044 §7 P2a, T32 and T26): every
//! audit record, delegation's and consent's among them, appended to a file
//! the supervisor alone holds open with `O_APPEND`, each line carrying the
//! SHA-256 of the line before it. A record edited, removed, inserted or
//! moved breaks the chain at the next start; so does a tail cut off behind
//! the head the store keeps.
//!
//! **A line** is one JSON object: the audit record's own fields, plus `seq`
//! (1, 2, …, never reused) and `prev`, the lowercase hex SHA-256 of the
//! previous line's bytes without its newline (sixty-four zeros before the
//! first). Lines are written whole, by one `write`, and the security
//! records — anything delegation or consent records, a segment's header,
//! the register — are flushed to the disk before [`Chain::append`] returns.
//! The store is handed only a head whose records are on the disk: the open
//! segment is flushed before a head names a record written unflushed, and a
//! rotation's rename and new segment before a record after them is, so a
//! power cut never looks like a cut tail.
//!
//! **Segments and retention.** The file rotates at `rotate_at` bytes: the
//! full one is renamed `<path>.<first seq, 20 digits>` and the new one opens
//! with an `audit.chain.segment` header, whose `prev` continues the chain
//! and whose `register` is the tombstone register as it stands. A closed
//! segment whose last record is older than `retention` is deleted, after an
//! `audit.chain.retired` record naming it and its last record; the header
//! after it is where verification then starts, held to that record — a
//! chain that starts past its first record with no retirement to say why
//! had a segment removed — and the register it carries keeps every
//! revocation, so retention never forgets one.
//!
//! **The anchored register.** `delegation.register` records carry a grant's
//! revoked epoch and latest `rt_version`. The vault takes the higher of what
//! the store's register says and what the chain says, so a store restored
//! from a backup taken before a revocation — register and all — still
//! refuses the revoked grant and the rotated-out refresh token.
//!
//! **Verification at start** reads every kept segment. Anything that does
//! not hold — a line that is not JSON, a `seq` out of order, a `prev` that
//! is not the line before's hash, a segment that does not begin with its
//! header, an oldest kept record no retirement explains, a head the store
//! keeps that the chain does not reach — is a typed refusal to start,
//! naming the file and the line. A last line cut short by a crash (no
//! newline) is the one thing repaired: cut back to the last whole record,
//! and an `audit.chain.repaired` record says how much.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The `prev` of the first record.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// How long a closed segment is kept, by its last record (RFC 0044 §6.4).
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(400 * 24 * 60 * 60);
/// How large a segment grows before it is closed.
pub const DEFAULT_ROTATE_AT: u64 = 64 << 20;
/// How often a head is handed to the store for records not flushed.
const HEAD_EVERY_MS: u64 = 5_000;

/// The header every segment begins with.
pub const SEGMENT: &str = "audit.chain.segment";
/// A grant's floor in the tombstone register.
pub const REGISTER: &str = "delegation.register";
const REPAIRED: &str = "audit.chain.repaired";
const RETIRED: &str = "audit.chain.retired";

/// One grant's entry in the register.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// The highest epoch revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_epoch: Option<u64>,
    /// The latest `rt_version` stored.
    #[serde(default)]
    pub rt_version: u64,
}

impl Anchor {
    /// Raise to `o` where it is higher: never lowered.
    pub fn merge(&mut self, o: &Anchor) {
        self.rt_version = self.rt_version.max(o.rt_version);
        self.revoked_epoch = self.revoked_epoch.max(o.revoked_epoch);
    }
}

/// The last record: what the store keeps to catch a cut tail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub seq: u64,
    pub hash: String,
}

/// Where the chain is, and its rules.
#[derive(Debug, Clone)]
pub struct Options {
    pub path: PathBuf,
    pub rotate_at: u64,
    pub retention: Duration,
}

/// Where each new head goes: the durable store.
pub type HeadSink = Box<dyn Fn(&Head) + Send + Sync>;

struct Inner {
    file: std::fs::File,
    /// The first `seq` of the open segment.
    first: u64,
    seq: u64,
    last: [u8; 32],
    size: u64,
    anchors: BTreeMap<String, Anchor>,
    /// Closed segments kept, oldest first.
    closed: Vec<Closed>,
    /// The last record known to be on the disk: the only head the store is
    /// handed.
    synced: Head,
    head_put_ms: u64,
}

/// A closed segment kept.
#[derive(Clone)]
struct Closed {
    path: PathBuf,
    /// Its last record's `ts`, which retention goes by.
    ts: u64,
    /// Its last record: what the record of its retirement names.
    last: Head,
}

/// The open chain.
pub struct Chain {
    opts: Options,
    inner: Mutex<Inner>,
    sink: Option<HeadSink>,
}

/// What opening found.
#[derive(Debug, Default)]
pub struct Opened {
    pub records: u64,
    pub segments: usize,
    /// Bytes of a torn last line cut off.
    pub repaired: Option<u64>,
    /// Closed segments deleted by retention.
    pub retired: usize,
}

fn hex(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> u64 {
    crate::state::now_ms()
}

/// The closed segments beside `path`, by their first `seq`.
fn closed_segments(path: &Path) -> Result<Vec<(u64, PathBuf)>, String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: not a file name", path.display()))?;
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    for e in entries.flatten() {
        let n = e.file_name();
        let Some(n) = n.to_str() else { continue };
        if let Some(seq) = n
            .strip_prefix(name)
            .and_then(|r| r.strip_prefix('.'))
            .filter(|d| d.len() == 20 && d.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|d| d.parse::<u64>().ok())
        {
            out.push((seq, e.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// Walking the chain: the state carried from one line to the next.
#[derive(Default)]
struct Walk {
    seq: u64,
    last: Option<[u8; 32]>,
    anchors: BTreeMap<String, Anchor>,
    records: u64,
    head_seen: bool,
    /// The oldest record kept: its `seq` and `prev`.
    oldest: Option<(u64, String)>,
    /// The last record of each segment retention deleted, as the
    /// `audit.chain.retired` records name it.
    retired: Vec<Head>,
}

impl Walk {
    /// The last record read.
    fn head(&self) -> Head {
        Head {
            seq: self.seq,
            hash: self.last.as_ref().map_or_else(|| GENESIS.to_string(), hex),
        }
    }

    /// Hold `line` of `file` (1-based `n`) to the chain; `start` is the seq
    /// its segment's name says it begins at.
    fn line(
        &mut self,
        file: &Path,
        n: usize,
        line: &[u8],
        start: Option<u64>,
        head: Option<&Head>,
    ) -> Result<u64, String> {
        let at = || format!("audit chain broken: {}:{n}", file.display());
        let v: Value = serde_json::from_slice(line)
            .map_err(|_| format!("{}: the line is not a JSON record", at()))?;
        let seq = v["seq"]
            .as_u64()
            .ok_or_else(|| format!("{}: no seq", at()))?;
        let prev = v["prev"]
            .as_str()
            .ok_or_else(|| format!("{}: no prev", at()))?;
        let action = v["action"].as_str().unwrap_or_default();
        if n == 1 {
            if action != SEGMENT {
                return Err(format!(
                    "{}: a segment begins with its {SEGMENT} header, and this one does not — records were cut from its front",
                    at()
                ));
            }
            if let Some(s) = start
                && s != seq
            {
                return Err(format!(
                    "{}: the segment is named for seq {s} and begins at {seq}",
                    at()
                ));
            }
        }
        match self.last {
            Some(last) => {
                if seq != self.seq + 1 {
                    return Err(format!(
                        "{}: seq {seq} follows {} — a record is missing or moved",
                        at(),
                        self.seq
                    ));
                }
                if prev != hex(&last) {
                    return Err(format!(
                        "{}: prev is not the hash of the record before — a record was changed",
                        at()
                    ));
                }
            }
            // The oldest record kept: the genesis, or a header whose
            // predecessors retention deleted — which the chain says after it
            // (checked once every record is read).
            None if seq == 1 && prev != GENESIS => {
                return Err(format!("{}: the first record's prev is not genesis", at()));
            }
            None => self.oldest = Some((seq, prev.to_string())),
        }
        if action == SEGMENT
            && let Some(reg) = v["target"]["register"].as_object()
        {
            for (g, a) in reg {
                if let Ok(a) = serde_json::from_value::<Anchor>(a.clone()) {
                    self.anchors.entry(g.clone()).or_default().merge(&a);
                }
            }
        }
        if action == REGISTER
            && let Some(g) = v["target"]["grant_id"].as_str()
        {
            let a = Anchor {
                revoked_epoch: v["target"]["revoked_epoch"].as_u64(),
                rt_version: v["target"]["rt_version"].as_u64().unwrap_or(0),
            };
            self.anchors.entry(g.to_string()).or_default().merge(&a);
        }
        if action == RETIRED
            && let (Some(seq), Some(hash)) = (
                v["target"]["last_seq"].as_u64(),
                v["target"]["last_hash"].as_str(),
            )
        {
            self.retired.push(Head {
                seq,
                hash: hash.to_string(),
            });
        }
        let hash = crate::sha::sha256(line);
        if let Some(h) = head
            && h.seq == seq
        {
            if h.hash != hex(&hash) {
                return Err(format!(
                    "{}: record {seq} is not the one the store's head names — the record was rewritten",
                    at()
                ));
            }
            self.head_seen = true;
        }
        self.seq = seq;
        self.last = Some(hash);
        self.records += 1;
        Ok(v["ts"].as_u64().unwrap_or(0))
    }
}

/// Split `bytes` into whole lines and the bytes after the last newline.
fn lines_of(bytes: &[u8]) -> (Vec<&[u8]>, &[u8]) {
    let mut out = Vec::new();
    let mut rest = bytes;
    while let Some(i) = rest.iter().position(|b| *b == b'\n') {
        out.push(&rest[..i]);
        rest = &rest[i + 1..];
    }
    (out, rest)
}

/// Flush the directory `path` is in: a file made or renamed there stays.
fn sync_dir(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| format!("audit chain {}: {e}", dir.display()))
}

fn create_append(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

impl Chain {
    /// Open the chain `opts` names: every kept segment verified, the head
    /// the store keeps (`head`) reached, a torn last line cut back, closed
    /// segments past retention deleted, and a new chain begun where there
    /// is none. `sink` is handed each new head.
    pub fn open(
        opts: Options,
        head: Option<Head>,
        sink: Option<HeadSink>,
    ) -> Result<(Chain, Opened), String> {
        if let Some(dir) = opts.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let mut b = std::fs::DirBuilder::new();
            b.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(dir)
                .map_err(|e| format!("audit chain {}: {e}", dir.display()))?;
        }
        let mut walk = Walk::default();
        let mut opened = Opened::default();
        let mut closed = Vec::new();
        for (start, p) in closed_segments(&opts.path)? {
            let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let (lines, rest) = lines_of(&bytes);
            if !rest.is_empty() || lines.is_empty() {
                return Err(format!(
                    "audit chain broken: {}: a closed segment ends inside a record",
                    p.display()
                ));
            }
            let mut ts = 0;
            for (i, l) in lines.iter().enumerate() {
                ts = walk.line(&p, i + 1, l, Some(start), head.as_ref())?;
            }
            closed.push(Closed {
                path: p,
                ts,
                last: walk.head(),
            });
            opened.segments += 1;
        }
        let mut first = walk.seq + 1;
        let mut size = 0u64;
        let current = match std::fs::read(&opts.path) {
            Ok(bytes) => {
                let (lines, rest) = lines_of(&bytes);
                for (i, l) in lines.iter().enumerate() {
                    if i == 0 {
                        first = serde_json::from_slice::<Value>(l)
                            .ok()
                            .and_then(|v| v["seq"].as_u64())
                            .unwrap_or(first);
                    }
                    walk.line(&opts.path, i + 1, l, None, head.as_ref())?;
                }
                size = (bytes.len() - rest.len()) as u64;
                if !rest.is_empty() {
                    opened.repaired = Some(rest.len() as u64);
                }
                opened.segments += 1;
                !lines.is_empty()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(format!("audit chain {}: {e}", opts.path.display())),
        };
        if let Some(h) = &head
            && !walk.head_seen
            && walk.seq < h.seq
        {
            return Err(format!(
                "audit chain broken: {}: the store's head is record {} and the chain ends at {} — its tail was cut",
                opts.path.display(),
                h.seq,
                walk.seq
            ));
        }
        // Records before the oldest kept are gone only as retention deleted
        // them, and each deletion is a record naming the last one it took.
        if let Some((seq, prev)) = &walk.oldest
            && *seq > 1
            && !walk
                .retired
                .iter()
                .any(|r| r.seq + 1 == *seq && &r.hash == prev)
        {
            return Err(format!(
                "audit chain broken: {}: the oldest record kept is {seq}, and no {RETIRED} record says retention deleted the ones before it — a segment was removed",
                opts.path.display()
            ));
        }
        let file = create_append(&opts.path)
            .map_err(|e| format!("audit chain {}: {e}", opts.path.display()))?;
        if opened.repaired.is_some() {
            file.set_len(size)
                .map_err(|e| format!("audit chain {}: {e}", opts.path.display()))?;
        }
        // What was read may still be only in the page cache — a previous
        // life that stopped without flushing it: on the disk before any
        // head names it.
        file.sync_data()
            .map_err(|e| format!("audit chain {}: {e}", opts.path.display()))?;
        opened.records = walk.records;
        let synced = walk.head();
        let chain = Chain {
            inner: Mutex::new(Inner {
                file,
                first,
                seq: walk.seq,
                last: walk.last.unwrap_or([0u8; 32]),
                size,
                anchors: walk.anchors,
                closed,
                synced,
                head_put_ms: 0,
            }),
            opts,
            sink,
        };
        {
            let mut g = chain.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !current {
                chain.header(&mut g)?;
                sync_dir(&chain.opts.path)?;
            }
            if let Some(n) = opened.repaired {
                chain.write(
                    &mut g,
                    json!({"ts": now_ms(), "action": REPAIRED, "outcome": "ok",
                           "target": {"dropped_bytes": n, "why": "the last record was cut short, as a crash mid-write leaves it"}}),
                    true,
                )?;
            }
            opened.retired = chain.retire(&mut g)?;
        }
        chain.put_head(true);
        Ok((chain, opened))
    }

    /// Append `record`; `sync` flushes it to the disk before returning.
    pub fn append(&self, record: Value, sync: bool) -> Result<Head, String> {
        let head = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if g.size >= self.opts.rotate_at {
                self.rotate(&mut g)?;
            }
            self.write(&mut g, record, sync)?
        };
        self.put_head(sync);
        Ok(head)
    }

    /// Anchor a grant's floor in the chain.
    pub fn anchor(&self, grant_id: &str, a: Anchor) -> Result<(), String> {
        let mut target = json!({"grant_id": grant_id, "rt_version": a.rt_version});
        if let Some(e) = a.revoked_epoch {
            target["revoked_epoch"] = json!(e);
        }
        self.append(
            json!({"ts": now_ms(), "action": REGISTER, "outcome": "ok", "target": target}),
            true,
        )
        .map(|_| ())
    }

    /// The register as the chain holds it.
    pub fn anchors(&self) -> BTreeMap<String, Anchor> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .anchors
            .clone()
    }

    pub fn head(&self) -> Head {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Head {
            seq: g.seq,
            hash: hex(&g.last),
        }
    }

    /// Hand the store the head now: at a clean stop, so a tail cut while
    /// agentd is down is caught at the next start.
    pub fn flush_head(&self) {
        self.put_head(true);
    }

    pub fn path(&self) -> &Path {
        &self.opts.path
    }

    /// Hand the store the head — the last record on the disk, the open
    /// segment flushed first when records after it are not: a head the disk
    /// does not hold would make a power cut look like a cut tail.
    fn put_head(&self, now: bool) {
        let Some(sink) = &self.sink else { return };
        let head = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let t = now_ms();
            if !now && t.saturating_sub(g.head_put_ms) < HEAD_EVERY_MS {
                return;
            }
            g.head_put_ms = t;
            if g.synced.seq < g.seq && g.file.sync_data().is_ok() {
                g.synced = Head {
                    seq: g.seq,
                    hash: hex(&g.last),
                };
            }
            g.synced.clone()
        };
        sink(&head);
    }

    /// Write one line; a write that fails is cut back off the file, so the
    /// next line follows a whole one.
    fn write(&self, g: &mut Inner, record: Value, sync: bool) -> Result<Head, String> {
        let mut obj = match record {
            Value::Object(o) => o,
            other => {
                let mut m = Map::new();
                m.insert("record".into(), other);
                m
            }
        };
        let seq = g.seq + 1;
        obj.insert("seq".into(), json!(seq));
        obj.insert("prev".into(), json!(hex(&g.last)));
        let line = Value::Object(obj);
        let mut bytes = line.to_string().into_bytes();
        let hash = crate::sha::sha256(&bytes);
        bytes.push(b'\n');
        let written = g
            .file
            .write_all(&bytes)
            .and_then(|()| if sync { g.file.sync_data() } else { Ok(()) });
        if let Err(e) = written {
            let _ = g.file.set_len(g.size);
            return Err(format!("audit chain {}: {e}", self.opts.path.display()));
        }
        if line["action"] == REGISTER
            && let Some(id) = line["target"]["grant_id"].as_str()
        {
            let a = Anchor {
                revoked_epoch: line["target"]["revoked_epoch"].as_u64(),
                rt_version: line["target"]["rt_version"].as_u64().unwrap_or(0),
            };
            g.anchors.entry(id.to_string()).or_default().merge(&a);
        }
        g.seq = seq;
        g.last = hash;
        g.size += bytes.len() as u64;
        let head = Head {
            seq,
            hash: hex(&hash),
        };
        if sync {
            g.synced = head.clone();
        }
        Ok(head)
    }

    /// The header a segment begins with.
    fn header(&self, g: &mut Inner) -> Result<(), String> {
        g.first = g.seq + 1;
        let previous = (g.seq > 0).then(|| hex(&g.last));
        let register = serde_json::to_value(&g.anchors).unwrap_or_else(|_| json!({}));
        self.write(
            g,
            json!({"ts": now_ms(), "action": SEGMENT, "outcome": "ok",
                   "target": {"first": g.seq + 1, "previous": previous, "register": register}}),
            true,
        )
        .map(|_| ())
    }

    /// Close the open segment and begin the next; the rename and the new
    /// segment are on the disk before a record after them is.
    fn rotate(&self, g: &mut Inner) -> Result<(), String> {
        let closed = PathBuf::from(format!("{}.{:020}", self.opts.path.display(), g.first));
        g.file
            .sync_all()
            .and_then(|()| std::fs::rename(&self.opts.path, &closed))
            .map_err(|e| format!("audit chain {}: rotate: {e}", self.opts.path.display()))?;
        g.synced = Head {
            seq: g.seq,
            hash: hex(&g.last),
        };
        g.file = create_append(&self.opts.path)
            .map_err(|e| format!("audit chain {}: {e}", self.opts.path.display()))?;
        g.size = 0;
        g.closed.push(Closed {
            path: closed,
            ts: now_ms(),
            last: g.synced.clone(),
        });
        self.header(g)?;
        sync_dir(&self.opts.path)?;
        self.retire(g).map(|_| ())
    }

    /// Delete closed segments whose last record is past retention, each
    /// one recorded — naming its last record, which the next oldest kept
    /// one follows — before it is deleted.
    fn retire(&self, g: &mut Inner) -> Result<usize, String> {
        let horizon = now_ms().saturating_sub(self.opts.retention.as_millis() as u64);
        let mut n = 0;
        while let Some(c) = g.closed.first().cloned() {
            if c.ts >= horizon {
                break;
            }
            self.write(
                g,
                json!({"ts": now_ms(), "action": RETIRED, "outcome": "ok",
                       "target": {"segment": c.path.file_name().and_then(|f| f.to_str()), "last_ts": c.ts,
                                  "last_seq": c.last.seq, "last_hash": c.last.hash}}),
                true,
            )?;
            std::fs::remove_file(&c.path)
                .map_err(|e| format!("audit chain {}: retire: {e}", c.path.display()))?;
            g.closed.remove(0);
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(all(test, feature = "xaa"))]
impl Chain {
    /// Make every write fail, as a full disk or a segment made append-only
    /// under agentd would.
    pub(crate) fn refuse_writes(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.file = std::fs::File::open(&self.opts.path).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dir: &Path) -> Options {
        Options {
            path: dir.join("audit").join("audit.log"),
            rotate_at: DEFAULT_ROTATE_AT,
            retention: DEFAULT_RETENTION,
        }
    }

    fn rec(action: &str) -> Value {
        json!({"ts": now_ms(), "action": action, "outcome": "ok", "target": {}})
    }

    /// Every line carries the hash of the one before; a restart continues
    /// the chain, and a chain verified from start to end opens.
    #[test]
    fn records_chain_and_a_restart_continues_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path());
        let (c, opened) = Chain::open(o.clone(), None, None).unwrap();
        assert_eq!(opened.records, 0);
        c.append(rec("a2a.SendMessage"), false).unwrap();
        let h = c.append(rec("delegation.consent"), true).unwrap();
        assert_eq!(h.seq, 3, "the header is record 1");
        drop(c);
        let text = std::fs::read_to_string(&o.path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for w in lines.windows(2) {
            let next: Value = serde_json::from_str(w[1]).unwrap();
            assert_eq!(next["prev"], hex(&crate::sha::sha256(w[0].as_bytes())));
        }
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            (first["action"].as_str(), first["prev"].as_str()),
            (Some(SEGMENT), Some(GENESIS))
        );
        let (c, opened) = Chain::open(o.clone(), Some(h.clone()), None).unwrap();
        assert_eq!(opened.records, 3);
        assert_eq!(c.append(rec("x"), false).unwrap().seq, 4);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&o.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// A changed byte, a removed record, a record moved, the front cut off
    /// and the tail cut behind the store's head are each a refusal naming
    /// the line.
    #[test]
    fn a_tampered_chain_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path());
        let (c, _) = Chain::open(o.clone(), None, None).unwrap();
        for i in 0..4 {
            c.append(
                json!({"ts": i, "action": "delegation.used", "outcome": "ok", "target": {"n": i}}),
                false,
            )
            .unwrap();
        }
        let head = c.head();
        drop(c);
        let good = std::fs::read_to_string(&o.path).unwrap();
        let lines: Vec<String> = good.lines().map(str::to_string).collect();
        let reopen = |text: String, head: Option<Head>| {
            std::fs::write(&o.path, text).unwrap();
            Chain::open(o.clone(), head, None).map(|_| ()).unwrap_err()
        };
        // One field of one record changed.
        let edited = good.replacen("\"n\":2", "\"n\":9", 1);
        let e = reopen(edited, None);
        assert!(e.contains("audit.log:5") && e.contains("changed"), "{e}");
        // A record removed.
        let mut removed = lines.clone();
        removed.remove(2);
        let e = reopen(removed.join("\n") + "\n", None);
        assert!(e.contains("missing or moved"), "{e}");
        // Two records swapped.
        let mut swapped = lines.clone();
        swapped.swap(2, 3);
        assert!(reopen(swapped.join("\n") + "\n", None).contains("audit.log:3"));
        // The header and the first record cut from the front.
        let e = reopen(lines[2..].join("\n") + "\n", None);
        assert!(e.contains("header"), "{e}");
        // The last two records cut off: only the head the store keeps says so.
        let cut = lines[..3].join("\n") + "\n";
        std::fs::write(&o.path, &cut).unwrap();
        assert!(
            Chain::open(o.clone(), None, None).is_ok(),
            "a cut tail alone verifies"
        );
        let e = reopen(cut, Some(head.clone()));
        assert!(e.contains("tail was cut"), "{e}");
        // The untouched chain opens with its head.
        std::fs::write(&o.path, &good).unwrap();
        assert!(Chain::open(o.clone(), Some(head), None).is_ok());
    }

    /// A last line cut short is cut back and said, and the chain goes on.
    #[test]
    fn a_torn_last_line_is_repaired_and_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path());
        let (c, _) = Chain::open(o.clone(), None, None).unwrap();
        c.append(rec("delegation.used"), true).unwrap();
        drop(c);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&o.path)
            .unwrap();
        f.write_all(b"{\"seq\":3,\"prev\":\"ab").unwrap();
        drop(f);
        let (c, opened) = Chain::open(o.clone(), None, None).unwrap();
        assert_eq!(opened.repaired, Some(19));
        let h = c.append(rec("x"), false).unwrap();
        drop(c);
        let text = std::fs::read_to_string(&o.path).unwrap();
        assert!(text.contains(REPAIRED), "{text}");
        assert!(Chain::open(o.clone(), Some(h), None).is_ok());
    }

    /// Segments rotate with a header that continues the chain and carries
    /// the register; a segment past retention is deleted and said, and the
    /// register it held is kept.
    #[test]
    fn segments_rotate_and_retention_keeps_the_register() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = opts(dir.path());
        o.rotate_at = 1;
        let (c, _) = Chain::open(o.clone(), None, None).unwrap();
        c.anchor(
            "g1",
            Anchor {
                revoked_epoch: Some(3),
                rt_version: 7,
            },
        )
        .unwrap();
        c.append(rec("a"), false).unwrap();
        c.append(rec("b"), false).unwrap();
        let head = c.head();
        drop(c);
        // Every append found the open segment full: a header alone, the
        // header and the register, the header and `a`; `b` is in the open one.
        let closed = closed_segments(&o.path).unwrap();
        assert_eq!(closed.len(), 3, "{closed:?}");
        let (c, opened) = Chain::open(o.clone(), Some(head), None).unwrap();
        assert_eq!(opened.segments, 4);
        assert_eq!(
            c.anchors()["g1"],
            Anchor {
                revoked_epoch: Some(3),
                rt_version: 7
            }
        );
        drop(c);
        // Retention of zero: every closed segment goes at the next start.
        o.retention = Duration::ZERO;
        o.rotate_at = DEFAULT_ROTATE_AT;
        std::thread::sleep(Duration::from_millis(5));
        let (c, opened) = Chain::open(o.clone(), None, None).unwrap();
        assert_eq!(opened.retired, 3);
        assert!(closed_segments(&o.path).unwrap().is_empty());
        assert_eq!(
            c.anchors()["g1"].revoked_epoch,
            Some(3),
            "the header kept it"
        );
        let head = c.head();
        drop(c);
        let text = std::fs::read_to_string(&o.path).unwrap();
        assert!(text.contains(RETIRED), "{text}");
        let (c, _) = Chain::open(o.clone(), Some(head), None).unwrap();
        assert_eq!(c.anchors()["g1"].rt_version, 7);
    }

    /// A closed segment deleted by hand, while retention keeps it, is
    /// refused: a chain that starts past its first record is explained only
    /// by the retirement it records.
    #[test]
    fn a_segment_removed_by_hand_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = opts(dir.path());
        o.rotate_at = 1;
        let (c, _) = Chain::open(o.clone(), None, None).unwrap();
        c.append(rec("a"), false).unwrap();
        c.append(rec("b"), false).unwrap();
        let head = c.head();
        drop(c);
        let closed = closed_segments(&o.path).unwrap();
        assert_eq!(closed.len(), 2, "{closed:?}");
        assert!(Chain::open(o.clone(), Some(head.clone()), None).is_ok());
        std::fs::remove_file(&closed[0].1).unwrap();
        let e = Chain::open(o.clone(), Some(head), None)
            .map(|_| ())
            .unwrap_err();
        assert!(e.contains("a segment was removed"), "{e}");
    }

    /// The store is handed only a head the disk holds: a record written
    /// unflushed is named once a flush covers it, and never while flushing
    /// fails.
    #[test]
    fn the_head_names_only_records_on_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let heads = std::sync::Arc::new(Mutex::new(Vec::<Head>::new()));
        let seen = std::sync::Arc::clone(&heads);
        let sink: HeadSink = Box::new(move |h| seen.lock().unwrap().push(h.clone()));
        let (c, _) = Chain::open(opts(dir.path()), None, Some(sink)).unwrap();
        let last = || heads.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last().seq, 1, "the header");
        c.append(rec("a2a.SendMessage"), false).unwrap();
        assert_eq!(last().seq, 1, "not flushed, not named yet");
        c.flush_head();
        assert_eq!(last(), c.head(), "flushed, then named");
        // A segment the disk takes no flush of (a pipe: fsync is EINVAL).
        let (_r, w) = std::io::pipe().unwrap();
        c.inner.lock().unwrap().file = std::fs::File::from(std::os::fd::OwnedFd::from(w));
        c.append(rec("a2a.SendMessage"), false).unwrap();
        c.flush_head();
        assert_eq!(last().seq, 2, "{:?}", last());
    }

    /// A record the store's head names, rewritten in place with a matching
    /// chain after it, is still caught by the head's hash.
    #[test]
    fn the_head_pins_its_record() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path());
        let (c, _) = Chain::open(o.clone(), None, None).unwrap();
        let h = c.append(rec("delegation.revoked"), true).unwrap();
        drop(c);
        // Rebuild the whole chain from the genesis with another record.
        std::fs::remove_file(&o.path).unwrap();
        let (c, _) = Chain::open(o.clone(), None, None).unwrap();
        c.append(rec("delegation.used"), true).unwrap();
        drop(c);
        let e = Chain::open(o.clone(), Some(h), None)
            .map(|_| ())
            .unwrap_err();
        assert!(e.contains("rewritten"), "{e}");
    }
}
