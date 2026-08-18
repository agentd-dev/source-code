// SPDX-License-Identifier: Apache-2.0
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
use std::collections::BTreeSet;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
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
    /// The digest of the settings that shaped this state (RFC 0033 §3.3),
    /// section name -> hex. A **signal, not a key**: a mismatch is reported at
    /// restore and the state is resumed anyway. Empty on a manifest written
    /// before this existed, which is why an empty side never compares (an
    /// upgrade must not announce that everything moved).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config_digest: BTreeMap<String, String>,
    /// Records an earlier `--fresh` abandoned (RFC 0033 §3.2). They are still in
    /// the store — `--fresh` deletes nothing — but they belong to a superseded
    /// generation, so the `list` reconciliation in [`Durable::restore`] must not
    /// re-adopt them and undo the flag one boot later.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<EntityRef>,
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

// ---- startup intent (RFC 0033 §3.2–§3.3) ------------------------------------
//
// Two facts belong to *this process's life* rather than to the settings
// document: "do not resume prior state" (`--fresh`) and "here is the
// configuration we are about to run under" (the digest). Neither is a setting —
// a file or an env var that pinned an instance to never resuming would be a
// footgun, and the digest is derived, not authored — so neither has a document
// path to bind to. The entry point knows both before the reactor exists; the
// reactor reaches `restore()` holding only a store and a policy. Rather than
// threading an argv fact through constructors that have no other reason to know
// about argv, `main` records them here once, and `Durable::new` reads them.
// Everything downstream works off the per-`Durable` copy, so a library embedder
// (and every unit test) can set them explicitly instead.

static FRESH: AtomicBool = AtomicBool::new(false);
static CONFIG_DIGEST: OnceLock<BTreeMap<String, String>> = OnceLock::new();

/// `--fresh` was given: the next [`Durable`] opened in this process starts a new
/// generation instead of resuming (RFC 0033 §3.2).
pub fn request_fresh() {
    FRESH.store(true, Ordering::Relaxed);
}

/// Whether `--fresh` was given.
pub fn fresh_requested() -> bool {
    FRESH.load(Ordering::Relaxed)
}

/// Record the digest of the configuration this process runs under, for
/// [`Durable::restore`] to compare against the manifest's (RFC 0033 §3.3).
/// First call wins — the configuration is loaded once, before any side effect.
pub fn record_config_digest(settings: &crate::config::v2::Settings) {
    let _ = CONFIG_DIGEST.set(config_digest(settings));
}

fn recorded_config_digest() -> BTreeMap<String, String> {
    CONFIG_DIGEST.get().cloned().unwrap_or_default()
}

/// The digest of the settings that **shaped the durable state** (RFC 0033 §3.3):
/// section name → SHA-256 hex of that section's canonical JSON.
///
/// Deliberately *not* the whole document. Only the three sections whose meaning
/// the stored records depend on are digested — a different `intelligence.model`
/// or a new MCP server does not make yesterday's inbox mean something else, and
/// including them would make the signal fire on every ordinary edit until an
/// operator learned to ignore it.
///
/// Nothing secret-bearing goes in. `store.http.headers` and the endpoint URLs
/// that can carry credentials in userinfo or a query are excluded by
/// construction (see [`store_shape`]) — they are auth, not layout, and a digest
/// of a low-entropy secret is a secret. The hash uses the crate's dependency-free
/// SHA-256 ([`crate::sha::sha256_hex`], already the workflow/artifact content
/// hash), so this adds no dependency and no feature gate.
pub fn config_digest(settings: &crate::config::v2::Settings) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    // serde_json's Map is a BTreeMap here (no `preserve_order`), so `to_string`
    // is already canonical: key order cannot make an unchanged config look moved.
    let digest = |v: &Value| crate::sha::sha256_hex(v.to_string().as_bytes());
    out.insert(
        "workflows".to_string(),
        digest(&Value::Array(settings.workflows.clone())),
    );
    out.insert("store".to_string(), digest(&store_shape(&settings.store)));
    out.insert(
        "limits".to_string(),
        digest(&limits_shape(&settings.limits)),
    );
    out
}

/// The secret-free projection of `store` that shapes the state: where records
/// go and how they are checkpointed. `Store` is deserialize-only, so this is
/// spelled out field by field — which is the point: a new secret-bearing field
/// cannot silently join the digest.
fn store_shape(s: &crate::config::v2::Store) -> Value {
    json!({
        "kind": format!("{:?}", s.kind),
        "prefix": s.prefix(),
        "on_error": format!("{:?}", s.on_error),
        "audit": s.audit,
        "checkpoint_debounce_ms": s.checkpoint.debounce_ms,
        "durability": format!("{:?}", s.durability),
        "timeout_ms": s.timeout.map(|d| d.0.as_millis() as u64),
        // The MCP server *name* is a config-local label, never a credential; the
        // HTTP adapter contributes only its presence (base_url and headers are
        // auth surface, §7).
        "mcp_server": s.mcp.as_ref().map(|m| m.server.clone()),
        "http": s.http.is_some(),
    })
}

/// The projection of `limits` — all numbers and durations, nothing secret. The
/// resolved values (not the `Option`s) so that writing a default explicitly does
/// not read as a change.
fn limits_shape(s: &crate::config::v2::Limits) -> Value {
    json!({
        "max_runs": s.max_runs,
        "run_steps": s.run.steps(),
        "run_tokens": s.run.tokens(),
        "run_deadline_ms": s.run.deadline().as_millis() as u64,
        "subagents": format!("{:?}", s.subagents),
        "inline_max_bytes": s.inline_max_bytes,
        "step_timeout_ms": s.step_timeout.map(|d| d.0.as_millis() as u64),
    })
}

/// Which digested sections moved between the manifest's record and this run.
///
/// An empty side never compares: a manifest written before the digest existed
/// carries none, and a `Durable` built without settings (an embedder, a test)
/// computes none — reporting "everything changed" in either case would train
/// the operator to ignore the one event that matters.
fn changed_sections(
    recorded: &BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
) -> Vec<String> {
    if recorded.is_empty() || current.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = current
        .iter()
        .filter(|(k, v)| recorded.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect();
    out.extend(
        recorded
            .keys()
            .filter(|k| !current.contains_key(*k))
            .cloned(),
    );
    out.sort();
    out.dedup();
    out
}

/// The kinds the restore reconciles against `list` — the entity kinds a crash
/// can leave in the store ahead of the manifest that indexes them.
const RECONCILED: [Kind; 7] = [
    Kind::Inbox,
    Kind::Context,
    Kind::Run,
    Kind::Subagent,
    Kind::Task,
    Kind::Timer,
    Kind::Artifact,
];

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
    /// `--fresh`: open a new generation instead of resuming (RFC 0033 §3.2).
    fresh: bool,
    /// The digest of the configuration this life runs under (RFC 0033 §3.3);
    /// empty when nothing recorded one, which disables the comparison.
    config_digest: BTreeMap<String, String>,
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
            fresh: fresh_requested(),
            config_digest: recorded_config_digest(),
        }
    }

    /// Override the `--fresh` intent this `Durable` was built with — for an
    /// embedder that drives the façade directly, and for the tests, neither of
    /// which goes through the CLI that sets the process-wide default.
    pub fn with_fresh(mut self, fresh: bool) -> Durable {
        self.fresh = fresh;
        self
    }

    /// Override the configuration digest (see [`with_fresh`](Durable::with_fresh)).
    pub fn with_config_digest(mut self, digest: BTreeMap<String, String>) -> Durable {
        self.config_digest = digest;
        self
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
    ///
    /// Under `--fresh` (RFC 0033 §3.2) the middle is skipped: see
    /// [`restore_fresh`](Durable::restore_fresh).
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
        // `--fresh` reads the manifest and stops there: the generation counter is
        // the one thing a new life must inherit (otherwise "which life am I in?"
        // resets on every use of the flag), and knowing what is being left behind
        // is what lets the new generation retire it instead of deleting it.
        if self.fresh {
            return self.restore_fresh(manifest, fresh);
        }
        // The configuration digest (RFC 0033 §3.3) is a **signal, not a gate**.
        // Identity is `agent.name` (§3.1): keying it on a config hash would start
        // the agent fresh and orphan its in-flight workflows the first time
        // someone raised a limit or fixed a typo — silently, which is exactly the
        // outcome durability exists to prevent. So a difference is reported and
        // the state is resumed regardless; the operator decides.
        let moved = changed_sections(&manifest.config_digest, &self.config_digest);
        if !moved.is_empty() {
            self.log_event(
                "store.config_changed",
                json!({
                    "sections": moved,
                    "msg": "state was written under a different configuration — resuming anyway; --fresh to start a new generation",
                }),
            );
        }
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
        // manifest never indexed). `seen` doubles as the ground truth for
        // pruning the retired set below; it stays `None` on a store without
        // `list`, where "gone" and "invisible" cannot be told apart.
        let mut seen: Option<BTreeSet<(String, String)>> = None;
        for kind in RECONCILED {
            match self.list(kind) {
                Ok(keys) => {
                    let seen = seen.get_or_insert_with(BTreeSet::new);
                    for ks in keys {
                        let Some((_, id)) =
                            crate::store::parse_key(&self.prefix, &self.instance, &ks.key)
                        else {
                            continue;
                        };
                        seen.insert((kind.as_str().to_string(), id.to_string()));
                        let indexed = manifest
                            .entities
                            .iter()
                            .any(|e| e.kind == kind.as_str() && e.id == id);
                        if indexed {
                            continue;
                        }
                        // A record a previous `--fresh` retired (RFC 0033 §3.2):
                        // still on the store because nothing was deleted, but it
                        // belongs to an abandoned generation. Adopting it here
                        // would undo the flag on the next ordinary start.
                        if manifest
                            .retired
                            .iter()
                            .any(|r| r.kind == kind.as_str() && r.id == id)
                        {
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
        // Carry the digest of what we actually ran under, so the next life
        // compares against this configuration rather than re-reporting the same
        // move forever. An unrecorded digest leaves the manifest's alone — an
        // embedder must not erase an operator's signal.
        if !self.config_digest.is_empty() {
            m.config_digest = self.config_digest.clone();
        }
        // Prune the retired set to what the store still holds: once an operator
        // has cleaned out the abandoned generation, its ghost list should not be
        // carried forever.
        if let Some(seen) = &seen {
            m.retired
                .retain(|r| seen.contains(&(r.kind.clone(), r.id.clone())));
        }
        *self.manifest.lock().unwrap_or_else(|e| e.into_inner()) = m.clone();
        self.manifest_dirty.store(true, Ordering::Relaxed);
        self.flush(true)?;
        if fresh && out.count() == 0 {
            self.log_event("restore.fresh", json!({"generation": m.generation}));
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

    /// `--fresh` (RFC 0033 §3.2): open the NEXT generation without resuming.
    ///
    /// Nothing is unlinked. A flag that silently destroys durable state is a
    /// footgun — the operator who types `--fresh` to get past a wedged run is
    /// exactly the one who will want yesterday's conversation back — so the new
    /// generation starts *alongside* the old one:
    ///
    /// * the outgoing manifest is copied to `manifest/agent.gen<N>`, because it
    ///   is the index of the retired records and without it they are a heap of
    ///   ULIDs no one can map back to anything;
    /// * every record still in the store is named in the new manifest's
    ///   `retired`, so the next ordinary start does not re-adopt them through the
    ///   `list` reconciliation and quietly undo the flag one boot later;
    /// * the generation counter is inherited and bumped, so the log says which
    ///   life is live.
    fn restore_fresh(
        &self,
        prior: Manifest,
        no_prior_manifest: bool,
    ) -> Result<Restored, StoreError> {
        // What the new generation is walking away from: whatever `list` can see,
        // plus the prior index (a store without `list` still has one).
        let mut retired: Vec<EntityRef> = Vec::new();
        let mut push = |kind: &str, id: &str, seq: u64| {
            if !retired.iter().any(|r| r.kind == kind && r.id == id) {
                retired.push(EntityRef {
                    kind: kind.to_string(),
                    id: id.to_string(),
                    seq,
                });
            }
        };
        for kind in RECONCILED {
            match self.list(kind) {
                Ok(keys) => {
                    for ks in keys {
                        if let Some((_, id)) =
                            crate::store::parse_key(&self.prefix, &self.instance, &ks.key)
                        {
                            push(kind.as_str(), id, ks.seq.unwrap_or(0));
                        }
                    }
                }
                Err(StoreError::Unsupported(_)) => {}
                Err(e) => return Err(e),
            }
        }
        for e in &prior.entities {
            push(&e.kind, &e.id, e.seq);
        }
        for e in &prior.retired {
            push(&e.kind, &e.id, e.seq);
        }
        // Preserve the outgoing index BEFORE overwriting `manifest/agent`: dying
        // between the two writes then leaves a stray copy, never a lost one.
        if !no_prior_manifest {
            self.put(
                Kind::Manifest,
                &format!("agent.gen{}", prior.generation),
                serde_json::to_value(&prior).unwrap_or(Value::Null),
                None,
            )?;
        }
        // Every field spelled out rather than `..prior`: the whole point of the
        // flag is that nothing carries over except the counter and the birth date.
        let m = Manifest {
            generation: prior.generation + 1,
            created: if prior.created == 0 {
                now_ms()
            } else {
                prior.created
            },
            updated: now_ms(),
            entities: Vec::new(),
            starts: BTreeMap::new(),
            budget: Value::Null,
            lifecycle: Value::Null,
            config_digest: self.config_digest.clone(),
            retired,
        };
        *self.manifest.lock().unwrap_or_else(|e| e.into_inner()) = m.clone();
        self.manifest_dirty.store(true, Ordering::Relaxed);
        self.flush(true)?;
        self.log_event(
            "restore.fresh",
            json!({
                "generation": m.generation,
                "superseded": prior.generation,
                "retired": m.retired.len(),
                "msg": "--fresh: this generation starts empty; the previous one's records were kept, not deleted",
            }),
        );
        Ok(Restored::default())
    }

    fn log_event(&self, event: &str, fields: Value) {
        if let Some(l) = &self.log {
            match event {
                e if e.ends_with(".fail")
                    || e == "store.conflict"
                    // A resumed state written under a different configuration is
                    // the operator's cue to check what moved — warn, not info.
                    || e == "store.config_changed" =>
                {
                    l.warn(event, fields)
                }
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

    /// RFC 0033 §3.2: `--fresh` opens the NEXT generation without resuming, and
    /// destroys nothing — the abandoned records stay readable, the outgoing index
    /// is preserved, and a later ordinary start does not quietly re-adopt them
    /// through the `list` reconciliation.
    #[test]
    fn fresh_opens_a_new_generation_without_resuming_and_deletes_nothing() {
        let mem = Arc::new(MemoryStore::new());

        // Life 1: a run and a pending inbox event.
        let d = durable(mem.clone());
        assert!(d.restore().unwrap().manifest.is_none());
        d.put(Kind::Run, "r1", json!({"status": "running"}), None)
            .unwrap();
        let ev = InboxEvent::new("a2a.message", None, json!({"n": 1}));
        d.inbox_put(&ev).unwrap();
        assert_eq!(d.manifest().generation, 1);

        // Life 2, `--fresh`: a new generation that resumes none of it.
        let f = durable(mem.clone()).with_fresh(true);
        let r = f.restore().unwrap();
        assert!(
            r.manifest.is_none(),
            "a new generation reports no prior life"
        );
        assert_eq!(r.count(), 0);
        assert!(r.inbox_pending().is_empty(), "the inbox does not replay");
        let m = f.manifest();
        assert_eq!(m.generation, 2, "the counter is inherited, not reset");
        assert!(m.entities.is_empty());

        // Nothing was deleted, and the previous index is still findable.
        assert!(
            f.get(Kind::Run, "r1").unwrap().is_some(),
            "--fresh keeps the abandoned records"
        );
        let kept = f
            .get(Kind::Manifest, "agent.gen1")
            .unwrap()
            .expect("the outgoing manifest is preserved");
        let kept: Manifest = serde_json::from_value(kept.state).unwrap();
        assert_eq!(kept.generation, 1);
        assert!(m.retired.iter().any(|e| e.kind == "run" && e.id == "r1"));
        assert!(m.retired.iter().any(|e| e.kind == "inbox" && e.id == ev.id));

        // Life 3, ordinary: the retired generation is not re-adopted — otherwise
        // the flag would come undone one boot later.
        let d3 = durable(mem.clone());
        let r3 = d3.restore().unwrap();
        assert_eq!(r3.count(), 0, "retired records stay retired");
        assert!(r3.unindexed.is_empty());
        assert_eq!(d3.manifest().generation, 3);
    }

    /// RFC 0033 §3.3: the configuration digest is a **signal, not a key** — a
    /// difference is reported and the state is resumed anyway (§3.1: keying
    /// identity on a config hash would orphan a live workflow on a typo fix).
    #[test]
    fn a_moved_config_digest_reports_but_never_gates_the_resume() {
        let before: BTreeMap<String, String> = [
            ("workflows".to_string(), "aaa".to_string()),
            ("store".to_string(), "sss".to_string()),
        ]
        .into_iter()
        .collect();
        let mut after = before.clone();
        after.insert("workflows".to_string(), "bbb".to_string());
        assert_eq!(changed_sections(&before, &after), vec!["workflows"]);
        // An empty side never compares: a pre-digest manifest, or a `Durable`
        // built without settings, must not announce that everything moved.
        assert!(changed_sections(&BTreeMap::new(), &after).is_empty());
        assert!(changed_sections(&before, &BTreeMap::new()).is_empty());

        let mem = Arc::new(MemoryStore::new());
        let d = durable(mem.clone()).with_config_digest(before.clone());
        d.restore().unwrap();
        d.put(Kind::Run, "r1", json!({"status": "running"}), None)
            .unwrap();
        assert_eq!(d.manifest().config_digest, before);

        let d2 = durable(mem.clone()).with_config_digest(after.clone());
        let r = d2.restore().unwrap();
        assert_eq!(r.of(Kind::Run).len(), 1, "state is still resumed");
        assert_eq!(d2.manifest().generation, 2);
        assert_eq!(
            d2.manifest().config_digest,
            after,
            "the next life compares against what this one ran under"
        );
    }

    /// The digest covers only the sections whose meaning the stored records
    /// depend on (RFC 0033 §3.3) — an edit anywhere else must not fire it.
    #[test]
    fn the_digest_covers_workflows_store_and_limits_only() {
        let doc = json!({
            "config_version": "2",
            "agent": {"name": "a", "instruction": "one"},
            "workflows": [{"name": "w", "version": 3, "steps": {"s": {"kind": "once"}}}],
            "limits": {"run": {"steps": 10}},
        });
        let settings = |patch: &dyn Fn(&mut Value)| {
            let mut d = doc.clone();
            patch(&mut d);
            serde_json::from_value::<crate::config::v2::Settings>(d).expect("settings")
        };
        let base = config_digest(&settings(&|_| {}));
        assert_eq!(
            base.keys().collect::<Vec<_>>(),
            ["limits", "store", "workflows"]
        );

        let elsewhere = config_digest(&settings(&|d| d["agent"]["instruction"] = json!("two")));
        assert_eq!(base, elsewhere, "an instruction edit is not a state change");

        let wf = config_digest(&settings(&|d| {
            d["workflows"][0]["steps"]["t"] = json!({"kind": "noop"})
        }));
        assert_eq!(changed_sections(&base, &wf), vec!["workflows"]);

        let lim = config_digest(&settings(&|d| d["limits"]["run"]["steps"] = json!(11)));
        assert_eq!(changed_sections(&base, &lim), vec!["limits"]);
    }
}
