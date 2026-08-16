// SPDX-License-Identifier: AGPL-3.0-only
//! The **durable state model** (RFC 0025 §3, §5–§7): entity kinds, the
//! manifest, the write-ahead inbox, timers, the checkpoint policy and the
//! restore protocol — one façade ([`Durable`]) over a [`crate::store::Store`]
//! that the runtime (RFC 0026) is the single writer of.
//!
//! Every entity is a versioned [`Envelope`] under `<prefix>/<instance>/<kind>/<id>`;
//! `Durable::put` allocates the next `seq` per key and treats a CAS conflict
//! on a key it already owns as **fatal** (a second writer). The manifest indexes
//! the live entities so a store without `list` can still be restored; it is
//! flushed **debounced** (`store.checkpoint.debounce_ms`) and at drain.

pub mod ulid;

use crate::obs::log::Logger;
use crate::store::{Envelope, KeySeq, PutOutcome, SharedStore, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(crate) use crate::store::now_ms;

/// The entity kinds (RFC 0025 §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Manifest,
    Inbox,
    Context,
    Run,
    Subagent,
    Task,
    Memory,
    Artifact,
    Timer,
    Audit,
    /// A cached endpoint credential (RFC 0031): an OAuth/OIDC/AWS/SPIFFE access +
    /// refresh token with its expiry, keyed by a hash of (endpoint, provider,
    /// principal). Redaction-excluded — never logged, audited, or read-surfaced.
    Cred,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Manifest => "manifest",
            Kind::Inbox => "inbox",
            Kind::Context => "context",
            Kind::Run => "run",
            Kind::Subagent => "subagent",
            Kind::Task => "task",
            Kind::Memory => "memory",
            Kind::Artifact => "artifact",
            Kind::Timer => "timer",
            Kind::Audit => "audit",
            Kind::Cred => "cred",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "manifest" => Kind::Manifest,
            "inbox" => Kind::Inbox,
            "context" => Kind::Context,
            "run" => Kind::Run,
            "subagent" => Kind::Subagent,
            "task" => Kind::Task,
            "memory" => Kind::Memory,
            "artifact" => Kind::Artifact,
            "timer" => Kind::Timer,
            "audit" => Kind::Audit,
            "cred" => Kind::Cred,
            _ => return None,
        })
    }
    /// Kinds the manifest indexes (restorable without `list`). Memory keys keep
    /// their own index record; audit records are append-only history; cred records
    /// are a self-keyed credential cache (not manifest-indexed).
    pub fn indexed(self) -> bool {
        !matches!(
            self,
            Kind::Manifest | Kind::Memory | Kind::Audit | Kind::Cred
        )
    }
}

/// One live entity in the manifest index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRef {
    pub kind: String,
    pub id: String,
    pub seq: u64,
}

/// The instance manifest (RFC 0025 §3.3 `manifest`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Manifest {
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub entities: Vec<EntityRef>,
    /// Start-node state per `<workflow>.<node>` (last fired, iteration, missed).
    #[serde(default)]
    pub starts: BTreeMap<String, Value>,
    /// Budget counters per window/scope (RFC 0026 §7).
    #[serde(default)]
    pub budget: Value,
    #[serde(default)]
    pub lifecycle: Value,
}

impl Manifest {
    fn upsert(&mut self, kind: &str, id: &str, seq: u64) {
        match self
            .entities
            .iter_mut()
            .find(|e| e.kind == kind && e.id == id)
        {
            Some(e) => e.seq = seq,
            None => self.entities.push(EntityRef {
                kind: kind.to_string(),
                id: id.to_string(),
                seq,
            }),
        }
    }
    fn remove(&mut self, kind: &str, id: &str) {
        self.entities.retain(|e| !(e.kind == kind && e.id == id));
    }
}

/// A write-ahead inbox event (RFC 0025 §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxEvent {
    pub id: String,
    pub kind: String,
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    pub payload: Value,
    #[serde(default)]
    pub status: InboxStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InboxStatus {
    #[default]
    Pending,
    Done,
}

impl InboxEvent {
    pub fn new(kind: &str, principal: Option<String>, payload: Value) -> InboxEvent {
        InboxEvent {
            id: ulid::new(),
            kind: kind.to_string(),
            ts: now_ms(),
            principal,
            payload,
            status: InboxStatus::Pending,
        }
    }
}

/// A durable timer (RFC 0025 §3.3 `timer`): an absolute deadline + who owns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimerRecord {
    pub id: String,
    pub deadline_ms: u64,
    pub owner: Value,
    #[serde(default)]
    pub payload: Value,
}

/// The checkpoint policy knobs (`store.checkpoint`, `store.durability`,
/// `store.on_error`).
#[derive(Debug, Clone)]
pub struct Policy {
    pub debounce: Duration,
    pub on_error: crate::config::v2::StoreOnError,
    pub retries: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            debounce: Duration::from_millis(250),
            on_error: crate::config::v2::StoreOnError::Halt,
            retries: 3,
        }
    }
}

impl Policy {
    pub fn from_settings(s: &crate::config::v2::Store) -> Policy {
        Policy {
            debounce: Duration::from_millis(s.checkpoint.debounce_ms.unwrap_or(250)),
            on_error: s.on_error,
            retries: 3,
        }
    }
}

/// What a restore found (RFC 0025 §6).
#[derive(Debug, Default)]
pub struct Restored {
    /// `None` ⇒ a fresh instance (no manifest).
    pub manifest: Option<Manifest>,
    /// Live entities by kind (tombstones excluded), each the latest envelope.
    pub entities: BTreeMap<String, Vec<Envelope>>,
    /// Indexed but missing from the store.
    pub lost: Vec<EntityRef>,
    /// Entities found by `list` that the manifest did not index (written after
    /// the last flush — entity-first write order).
    pub unindexed: Vec<EntityRef>,
}

impl Restored {
    pub fn inbox_pending(&self) -> Vec<InboxEvent> {
        let mut out: Vec<InboxEvent> = self
            .entities
            .get("inbox")
            .map(|v| {
                v.iter()
                    .filter_map(|e| serde_json::from_value::<InboxEvent>(e.state.clone()).ok())
                    .filter(|e| e.status == InboxStatus::Pending)
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.id.cmp(&b.id)));
        out
    }
    pub fn timers(&self) -> Vec<TimerRecord> {
        self.entities
            .get("timer")
            .map(|v| {
                v.iter()
                    .filter_map(|e| serde_json::from_value(e.state.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn of(&self, kind: Kind) -> &[Envelope] {
        self.entities
            .get(kind.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    pub fn count(&self) -> usize {
        self.entities.values().map(Vec::len).sum()
    }
}

/// The durability façade: the single writer's view of the store.
pub struct Durable {
    store: SharedStore,
    prefix: String,
    instance: String,
    policy: Policy,
    /// Last known seq per key (warmed by restore; a key not here starts at 1
    /// and adopts a stale record's seq once).
    seqs: Mutex<HashMap<String, u64>>,
    manifest: Mutex<Manifest>,
    manifest_dirty: AtomicBool,
    last_flush: Mutex<Instant>,
    degraded: AtomicBool,
    log: Option<Logger>,
}

impl Durable {
    pub fn new(
        store: SharedStore,
        prefix: &str,
        instance: &str,
        policy: Policy,
        log: Option<Logger>,
    ) -> Durable {
        Durable {
            store,
            prefix: prefix.to_string(),
            instance: instance.to_string(),
            policy,
            seqs: Mutex::new(HashMap::new()),
            manifest: Mutex::new(Manifest::default()),
            manifest_dirty: AtomicBool::new(false),
            last_flush: Mutex::new(Instant::now()),
            degraded: AtomicBool::new(false),
            log,
        }
    }

    pub fn store_kind(&self) -> &'static str {
        self.store.kind()
    }
    pub fn instance(&self) -> &str {
        &self.instance
    }
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
    pub fn key(&self, kind: Kind, id: &str) -> String {
        crate::store::key(&self.prefix, &self.instance, kind.as_str(), id)
    }
    /// Whether the store has failed persistently and the policy chose to go on.
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    // ---- entities -----------------------------------------------------------

    /// Write an entity: allocates the next seq for its key, CAS-puts the
    /// envelope, indexes it in the manifest (debounced flush). A conflict on a
    /// key this instance already owns is fatal (`StoreError::Conflict`); on a
    /// key first seen now, the stored seq is adopted once (a restore gap).
    pub fn put(
        &self,
        kind: Kind,
        id: &str,
        state: Value,
        hash: Option<String>,
    ) -> Result<u64, StoreError> {
        let key = self.key(kind, id);
        let mut adopted = false;
        let started = std::time::Instant::now();
        loop {
            let (seq, warmed) = {
                let seqs = self.seqs.lock().unwrap_or_else(|e| e.into_inner());
                match seqs.get(&key).copied() {
                    Some(s) => (s + 1, true),
                    None => (1, false),
                }
            };
            let env = Envelope::new(
                kind.as_str(),
                id,
                seq,
                &self.instance,
                hash.clone(),
                state.clone(),
            );
            kill_point("state.before_put");
            let outcome = crate::store::with_retry(
                || self.store.put(&key, seq, &env.to_value()),
                self.policy.retries,
            );
            match outcome {
                Ok(PutOutcome::Ok) => {
                    self.seqs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(key.clone(), seq);
                    if kind.indexed() {
                        let mut m = self.manifest.lock().unwrap_or_else(|e| e.into_inner());
                        m.upsert(kind.as_str(), id, seq);
                        m.updated = now_ms();
                        self.manifest_dirty.store(true, Ordering::Relaxed);
                    }
                    self.degraded.store(false, Ordering::Relaxed);
                    kill_point("state.after_put");
                    crate::obs::metrics::record_store_op(
                        "ok",
                        started.elapsed().as_millis() as u64,
                    );
                    return Ok(seq);
                }
                Ok(PutOutcome::Conflict { latest_seq }) => {
                    if !warmed && !adopted {
                        // First touch of a key that already exists in the store
                        // (a record written before a restore gap): adopt its
                        // seq and retry once.
                        if let Some(l) = latest_seq {
                            self.seqs
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(key.clone(), l);
                            adopted = true;
                            self.log_event("store.seq_adopted", json!({"key": key, "latest": l}));
                            continue;
                        }
                    }
                    self.log_event(
                        "store.conflict",
                        json!({"key": key, "seq": seq, "latest": latest_seq}),
                    );
                    crate::obs::metrics::record_store_op(
                        "conflict",
                        started.elapsed().as_millis() as u64,
                    );
                    return Err(StoreError::Conflict(format!(
                        "key {key}: another writer owns it (our seq {seq}, latest {latest_seq:?})"
                    )));
                }
                Err(e) => {
                    self.log_event("store.put.fail", json!({"key": key, "err": e.to_string()}));
                    crate::obs::metrics::record_store_op(
                        "error",
                        started.elapsed().as_millis() as u64,
                    );
                    if self.policy.on_error == crate::config::v2::StoreOnError::Degrade {
                        self.degraded.store(true, Ordering::Relaxed);
                        // Degraded: remember the seq we intended so a later put
                        // does not reuse it, and go on.
                        self.seqs
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(key.clone(), seq);
                        return Ok(seq);
                    }
                    return Err(e);
                }
            }
        }
    }

    /// The latest envelope of an entity (tombstones read as absent).
    pub fn get(&self, kind: Kind, id: &str) -> Result<Option<Envelope>, StoreError> {
        let key = self.key(kind, id);
        let v = crate::store::with_retry(|| self.store.get(&key, None), self.policy.retries)?;
        match v {
            None => Ok(None),
            Some(v) => {
                let env = Envelope::from_value(v)?;
                self.seqs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, env.seq);
                Ok(if env.is_tombstone() { None } else { Some(env) })
            }
        }
    }

    /// Remove an entity: `delete` when the store supports it, else a tombstone
    /// (a `put` with `state: null`); drops it from the manifest index.
    pub fn delete(&self, kind: Kind, id: &str) -> Result<(), StoreError> {
        let key = self.key(kind, id);
        match crate::store::with_retry(|| self.store.delete(&key), self.policy.retries) {
            Ok(()) => {
                self.seqs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
            }
            Err(StoreError::Unsupported(_)) => {
                self.put(kind, id, Value::Null, None)?;
            }
            Err(e) => return Err(e),
        }
        if kind.indexed() {
            let mut m = self.manifest.lock().unwrap_or_else(|e| e.into_inner());
            m.remove(kind.as_str(), id);
            m.updated = now_ms();
            self.manifest_dirty.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// The store's `list` for a kind (optional).
    pub fn list(&self, kind: Kind) -> Result<Vec<KeySeq>, StoreError> {
        let prefix = format!("{}/{}/{}/", self.prefix, self.instance, kind.as_str());
        self.store.list(&prefix)
    }

    // ---- inbox / timers -----------------------------------------------------

    /// Write-ahead an event (before it is acted on / acknowledged).
    pub fn inbox_put(&self, ev: &InboxEvent) -> Result<u64, StoreError> {
        let seq = self.put(
            Kind::Inbox,
            &ev.id,
            serde_json::to_value(ev).unwrap_or(Value::Null),
            None,
        )?;
        kill_point("inbox.after_put");
        Ok(seq)
    }

    /// Mark an event processed: deleted (or tombstoned) — it will not replay.
    pub fn inbox_done(&self, id: &str) -> Result<(), StoreError> {
        self.delete(Kind::Inbox, id)
    }

    pub fn timer_arm(&self, t: &TimerRecord) -> Result<u64, StoreError> {
        self.put(
            Kind::Timer,
            &t.id,
            serde_json::to_value(t).unwrap_or(Value::Null),
            None,
        )
    }

    pub fn timer_disarm(&self, id: &str) -> Result<(), StoreError> {
        self.delete(Kind::Timer, id)
    }

    // ---- manifest -----------------------------------------------------------

    pub fn manifest(&self) -> Manifest {
        self.manifest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Mutate the manifest (start-node state, budget counters, lifecycle) —
    /// flushed debounced.
    pub fn manifest_update(&self, f: impl FnOnce(&mut Manifest)) {
        let mut m = self.manifest.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut m);
        m.updated = now_ms();
        self.manifest_dirty.store(true, Ordering::Relaxed);
    }

    /// Flush the manifest if dirty and (forced or the debounce elapsed).
    pub fn flush(&self, force: bool) -> Result<bool, StoreError> {
        if !self.manifest_dirty.load(Ordering::Relaxed) {
            return Ok(false);
        }
        {
            let last = self.last_flush.lock().unwrap_or_else(|e| e.into_inner());
            if !force && last.elapsed() < self.policy.debounce {
                return Ok(false);
            }
        }
        let snapshot = self.manifest();
        self.put(
            Kind::Manifest,
            "agent",
            serde_json::to_value(&snapshot).unwrap_or(Value::Null),
            None,
        )?;
        self.manifest_dirty.store(false, Ordering::Relaxed);
        *self.last_flush.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        Ok(true)
    }

    // ---- restore ------------------------------------------------------------

    /// The restore protocol (RFC 0025 §6): read the manifest, then every indexed
    /// entity (verifying envelopes), reconcile with `list` where supported, warm
    /// the seq map, bump the generation. A fresh instance (no manifest) writes
    /// generation 1.
    pub fn restore(&self) -> Result<Restored, StoreError> {
        let mut out = Restored::default();
        let (manifest, fresh) = match self.get(Kind::Manifest, "agent")? {
            None => (
                Manifest {
                    generation: 0,
                    created: now_ms(),
                    updated: now_ms(),
                    ..Manifest::default()
                },
                true,
            ),
            Some(env) => (
                serde_json::from_value::<Manifest>(env.state.clone())
                    .map_err(|e| StoreError::Corrupt(format!("manifest does not parse: {e}")))?,
                false,
            ),
        };
        // Indexed entities.
        for r in &manifest.entities {
            let Some(kind) = Kind::parse(&r.kind) else {
                out.lost.push(r.clone());
                continue;
            };
            match self.get(kind, &r.id)? {
                Some(env) => out.entities.entry(r.kind.clone()).or_default().push(env),
                None => out.lost.push(r.clone()),
            }
        }
        // Reconcile with `list` (entity-first write order can leave records the
        // manifest never indexed).
        for kind in [
            Kind::Inbox,
            Kind::Context,
            Kind::Run,
            Kind::Subagent,
            Kind::Task,
            Kind::Timer,
            Kind::Artifact,
        ] {
            match self.list(kind) {
                Ok(keys) => {
                    for ks in keys {
                        let Some((_, id)) =
                            crate::store::parse_key(&self.prefix, &self.instance, &ks.key)
                        else {
                            continue;
                        };
                        let indexed = manifest
                            .entities
                            .iter()
                            .any(|e| e.kind == kind.as_str() && e.id == id);
                        if indexed {
                            continue;
                        }
                        if let Some(env) = self.get(kind, id)? {
                            out.unindexed.push(EntityRef {
                                kind: kind.as_str().to_string(),
                                id: id.to_string(),
                                seq: env.seq,
                            });
                            out.entities
                                .entry(kind.as_str().to_string())
                                .or_default()
                                .push(env);
                        }
                    }
                }
                Err(StoreError::Unsupported(_)) => {}
                Err(e) => return Err(e),
            }
        }
        // Adopt the manifest, re-index what we found, bump the generation. A
        // fresh instance (no manifest) starts at generation 1 — but any records
        // `list` found (a crash before the first flush) are adopted, not lost.
        let mut m = manifest.clone();
        m.entities.retain(|e| !out.lost.iter().any(|l| l == e));
        for u in &out.unindexed {
            m.upsert(&u.kind, &u.id, u.seq);
        }
        m.generation += 1;
        m.updated = now_ms();
        *self.manifest.lock().unwrap_or_else(|e| e.into_inner()) = m.clone();
        self.manifest_dirty.store(true, Ordering::Relaxed);
        self.flush(true)?;
        if fresh && out.count() == 0 {
            self.log_event("restore.fresh", json!({"generation": 1}));
            return Ok(out);
        }
        self.log_event(
            "restore.done",
            json!({
                "generation": m.generation,
                "fresh_manifest": fresh,
                "entities": out.count(),
                "lost": out.lost.len(),
                "unindexed": out.unindexed.len(),
                "inbox_pending": out.inbox_pending().len(),
            }),
        );
        out.manifest = Some(m);
        Ok(out)
    }

    fn log_event(&self, event: &str, fields: Value) {
        if let Some(l) = &self.log {
            match event {
                e if e.ends_with(".fail") || e == "store.conflict" => l.warn(event, fields),
                _ => l.info(event, fields),
            }
        }
    }
}

/// A test **kill point** (RFC test strategy §2): with `AGENTD_TEST_KILL_AT=<name>`
/// set (debug / `internal-mocks` builds only), the process SIGKILLs itself
/// here — the chaos suite's way of dying between two durable writes.
pub fn kill_point(name: &str) {
    #[cfg(any(feature = "internal-mocks", debug_assertions))]
    {
        if std::env::var("AGENTD_TEST_KILL_AT").as_deref() == Ok(name) {
            #[cfg(unix)]
            unsafe {
                libc::raise(libc::SIGKILL);
            }
            std::process::abort();
        }
    }
    #[cfg(not(any(feature = "internal-mocks", debug_assertions)))]
    {
        let _ = name;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::memory::MemoryStore;
    use std::sync::Arc;

    fn durable(store: Arc<MemoryStore>) -> Durable {
        Durable::new(
            store,
            "agentd",
            "inst",
            Policy {
                debounce: Duration::from_millis(0),
                ..Policy::default()
            },
            None,
        )
    }

    #[test]
    fn put_allocates_seqs_indexes_and_flushes_manifest() {
        let mem = Arc::new(MemoryStore::new());
        let d = durable(mem.clone());
        assert!(d.restore().unwrap().manifest.is_none(), "fresh");
        assert_eq!(
            d.put(
                Kind::Run,
                "r1",
                json!({"status": "running"}),
                Some("h".into())
            )
            .unwrap(),
            1
        );
        assert_eq!(
            d.put(Kind::Run, "r1", json!({"status": "done"}), Some("h".into()))
                .unwrap(),
            2
        );
        assert_eq!(
            d.put(Kind::Context, "root", json!({"v": 1}), None).unwrap(),
            1
        );
        let env = d.get(Kind::Run, "r1").unwrap().unwrap();
        assert_eq!(env.seq, 2);
        assert_eq!(env.state["status"], json!("done"));
        assert_eq!(env.hash.as_deref(), Some("h"));
        // Manifest indexes both, flushed on demand.
        assert!(d.flush(true).unwrap());
        let m = d.manifest();
        assert_eq!(m.entities.len(), 2);
        assert!(
            m.entities
                .iter()
                .any(|e| e.kind == "run" && e.id == "r1" && e.seq == 2)
        );
        assert!(!d.flush(true).unwrap(), "clean after a flush");
        // delete removes + un-indexes.
        d.delete(Kind::Context, "root").unwrap();
        assert!(d.get(Kind::Context, "root").unwrap().is_none());
        assert_eq!(d.manifest().entities.len(), 1);
    }

    #[test]
    fn conflicts_are_fatal_on_owned_keys_but_adopted_on_first_touch() {
        let mem = Arc::new(MemoryStore::new());
        // A record from a previous life the manifest never indexed.
        let stale = Envelope::new("run", "old", 5, "inst", None, json!({"x": 1}));
        mem.put("agentd/inst/run/old", 5, &stale.to_value())
            .unwrap();
        let d = durable(mem.clone());
        // First touch adopts seq 5 → writes 6.
        assert_eq!(d.put(Kind::Run, "old", json!({"x": 2}), None).unwrap(), 6);
        // A genuine second writer bumping the key behind our back is fatal.
        let other = Envelope::new("run", "old", 7, "other", None, json!({"x": 3}));
        mem.put("agentd/inst/run/old", 7, &other.to_value())
            .unwrap();
        assert!(matches!(
            d.put(Kind::Run, "old", json!({"x": 4}), None),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn inbox_write_ahead_timers_and_restore() {
        let mem = Arc::new(MemoryStore::new());
        {
            let d = durable(mem.clone());
            d.restore().unwrap();
            let e1 = InboxEvent::new(
                "a2a_message",
                Some("user:andrii".into()),
                json!({"text": "hi"}),
            );
            let e2 = InboxEvent::new("start_fired", None, json!({"workflow": "w"}));
            d.inbox_put(&e1).unwrap();
            d.inbox_put(&e2).unwrap();
            d.inbox_done(&e1.id).unwrap();
            d.timer_arm(&TimerRecord {
                id: "t1".into(),
                deadline_ms: 42,
                owner: json!({"run": "r"}),
                payload: Value::Null,
            })
            .unwrap();
            d.put(
                Kind::Run,
                "r",
                json!({"status": "running"}),
                Some("hash".into()),
            )
            .unwrap();
            d.put(Kind::Task, "task-1", json!({"state": "working"}), None)
                .unwrap();
            d.manifest_update(|m| {
                m.starts.insert("w.s".into(), json!({"last_fired": 1}));
            });
            // A lost entity: indexed but gone from the store.
            d.put(Kind::Subagent, "gone", json!({}), None).unwrap();
            d.flush(true).unwrap();
            mem.delete("agentd/inst/subagent/gone").unwrap();
            // An entity written AFTER the last flush (entity-first order): not
            // indexed; the restore's `list` reconciliation finds it.
            d.put(Kind::Run, "r2", json!({"status": "running"}), None)
                .unwrap();
        }
        // "restart": a fresh Durable over the same store.
        let d2 = durable(mem.clone());
        let r = d2.restore().unwrap();
        let m = r.manifest.as_ref().unwrap();
        assert_eq!(m.generation, 2, "generation bumped");
        assert_eq!(m.starts["w.s"]["last_fired"], json!(1));
        let pending = r.inbox_pending();
        assert_eq!(pending.len(), 1, "the done event does not replay");
        assert_eq!(pending[0].kind, "start_fired");
        assert_eq!(r.timers().len(), 1);
        assert_eq!(r.timers()[0].deadline_ms, 42);
        assert_eq!(
            r.of(Kind::Run).len(),
            2,
            "indexed + unindexed runs restored"
        );
        assert!(r.unindexed.iter().any(|u| u.id == "r2"));
        assert!(
            r.lost
                .iter()
                .any(|l| l.kind == "subagent" && l.id == "gone")
        );
        assert_eq!(r.of(Kind::Task).len(), 1);
        // The seq map is warm: the next put of `r` continues the sequence.
        assert_eq!(
            d2.put(
                Kind::Run,
                "r",
                json!({"status": "done"}),
                Some("hash".into())
            )
            .unwrap(),
            2
        );
        // And the re-indexed manifest no longer lists the lost entity.
        assert!(!d2.manifest().entities.iter().any(|e| e.id == "gone"));
    }

    #[test]
    fn degrade_policy_keeps_going_and_flags_it() {
        let mem = Arc::new(MemoryStore::new());
        let d = Durable::new(
            mem.clone(),
            "agentd",
            "inst",
            Policy {
                debounce: Duration::from_millis(0),
                on_error: crate::config::v2::StoreOnError::Degrade,
                retries: 1,
            },
            None,
        );
        mem.fail_next(1);
        assert_eq!(
            d.put(Kind::Run, "r", json!({}), None).unwrap(),
            1,
            "degraded write reports the intended seq"
        );
        assert!(d.is_degraded());
        assert_eq!(
            d.put(Kind::Run, "r", json!({}), None).unwrap(),
            2,
            "seq not reused"
        );
        assert!(!d.is_degraded(), "a successful write clears the flag");
        // Halt policy surfaces the error.
        let d2 = durable(mem.clone());
        mem.fail_next(5);
        assert!(matches!(
            d2.put(Kind::Run, "x", json!({}), None),
            Err(StoreError::Io(_))
        ));
    }
}
