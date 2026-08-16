// SPDX-License-Identifier: AGPL-3.0-only
//! The in-process store (RFC 0025 §4.3) — `store.kind: memory`. Keeps per-key
//! history (so `get(key, seq)` works like a history-keeping server), enforces
//! the seq CAS, supports `list`/`delete`, and offers **fault injection** for
//! tests: fail the next N operations, add latency, or refuse a specific key.
//! Not durable across the process (dev/test only — the loader warns).

use super::{KeySeq, PutOutcome, Store, StoreError};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Default)]
struct Inner {
    /// key → seq → envelope
    data: BTreeMap<String, BTreeMap<u64, Value>>,
    /// Fault injection: remaining operations to fail with `Io`.
    fail_next: u32,
    /// Latency added to every operation.
    latency: Duration,
    /// Every operation performed (op, key) — for assertions.
    log: Vec<(String, String)>,
}

pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    pub fn new() -> MemoryStore {
        MemoryStore {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Fail the next `n` operations with `StoreError::Io`.
    pub fn fail_next(&self, n: u32) {
        self.lock().fail_next = n;
    }

    /// Add `latency` to every operation.
    pub fn set_latency(&self, latency: Duration) {
        self.lock().latency = latency;
    }

    /// The operations performed so far (op, key).
    pub fn ops(&self) -> Vec<(String, String)> {
        self.lock().log.clone()
    }

    /// The number of stored keys (live, i.e. non-tombstone latest record).
    pub fn len(&self) -> usize {
        self.lock()
            .data
            .values()
            .filter(|h| {
                h.values()
                    .next_back()
                    .is_some_and(|v| !v.get("state").is_some_and(Value::is_null))
            })
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every stored key with its latest seq (test helper — includes tombstones).
    pub fn dump(&self) -> Vec<(String, u64)> {
        self.lock()
            .data
            .iter()
            .filter_map(|(k, h)| h.keys().next_back().map(|s| (k.clone(), *s)))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn begin(&self, op: &str, key: &str) -> Result<std::sync::MutexGuard<'_, Inner>, StoreError> {
        let mut g = self.lock();
        g.log.push((op.to_string(), key.to_string()));
        if g.fail_next > 0 {
            g.fail_next -= 1;
            return Err(StoreError::Io(format!("injected failure on {op} {key}")));
        }
        if !g.latency.is_zero() {
            let d = g.latency;
            drop(g);
            std::thread::sleep(d);
            g = self.lock();
        }
        Ok(g)
    }
}

impl Store for MemoryStore {
    fn put(&self, key: &str, seq: u64, envelope: &Value) -> Result<PutOutcome, StoreError> {
        let mut g = self.begin("put", key)?;
        let hist = g.data.entry(key.to_string()).or_default();
        let latest = hist.keys().next_back().copied();
        if let Some(l) = latest
            && seq <= l
        {
            return Ok(PutOutcome::Conflict {
                latest_seq: Some(l),
            });
        }
        hist.insert(seq, envelope.clone());
        Ok(PutOutcome::Ok)
    }

    fn get(&self, key: &str, seq: Option<u64>) -> Result<Option<Value>, StoreError> {
        let g = self.begin("get", key)?;
        let Some(hist) = g.data.get(key) else {
            return Ok(None);
        };
        let picked = match seq {
            Some(s) => hist.get(&s),
            None => hist.values().next_back(),
        };
        // A tombstone (latest state null) reads as absent.
        Ok(picked
            .filter(|v| !v.get("state").is_some_and(Value::is_null))
            .cloned())
    }

    fn list(&self, prefix: &str) -> Result<Vec<KeySeq>, StoreError> {
        let g = self.begin("list", prefix)?;
        Ok(g.data
            .iter()
            .filter(|(k, h)| {
                k.starts_with(prefix)
                    && h.values()
                        .next_back()
                        .is_some_and(|v| !v.get("state").is_some_and(Value::is_null))
            })
            .map(|(k, h)| KeySeq {
                key: k.clone(),
                seq: h.keys().next_back().copied(),
            })
            .collect())
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let mut g = self.begin("delete", key)?;
        g.data.remove(key);
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "memory"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cas_history_list_delete_and_faults() {
        let s = MemoryStore::new();
        assert_eq!(
            s.put("a/k", 1, &json!({"state": 1})).unwrap(),
            PutOutcome::Ok
        );
        assert_eq!(
            s.put("a/k", 2, &json!({"state": 2})).unwrap(),
            PutOutcome::Ok
        );
        // CAS: an equal or lower seq conflicts, naming the latest.
        assert_eq!(
            s.put("a/k", 2, &json!({"state": 9})).unwrap(),
            PutOutcome::Conflict {
                latest_seq: Some(2)
            }
        );
        assert_eq!(
            s.put("a/k", 1, &json!({"state": 9})).unwrap(),
            PutOutcome::Conflict {
                latest_seq: Some(2)
            }
        );
        // Latest and pinned reads.
        assert_eq!(s.get("a/k", None).unwrap(), Some(json!({"state": 2})));
        assert_eq!(s.get("a/k", Some(1)).unwrap(), Some(json!({"state": 1})));
        assert_eq!(s.get("a/k", Some(5)).unwrap(), None);
        assert_eq!(s.get("a/none", None).unwrap(), None);
        // list by prefix with latest seq.
        s.put("a/j", 1, &json!({"state": 0})).unwrap();
        s.put("b/x", 1, &json!({"state": 0})).unwrap();
        let l = s.list("a/").unwrap();
        assert_eq!(l.len(), 2);
        assert!(l.iter().any(|e| e.key == "a/k" && e.seq == Some(2)));
        // A tombstone reads as absent and is not listed.
        s.put("a/j", 2, &json!({"state": null})).unwrap();
        assert_eq!(s.get("a/j", None).unwrap(), None);
        assert_eq!(s.list("a/").unwrap().len(), 1);
        // delete removes history.
        s.delete("a/k").unwrap();
        assert_eq!(s.get("a/k", None).unwrap(), None);
        assert_eq!(
            s.put("a/k", 1, &json!({"state": "again"})).unwrap(),
            PutOutcome::Ok
        );
        // Fault injection.
        s.fail_next(2);
        assert!(matches!(s.get("a/k", None), Err(StoreError::Io(_))));
        assert!(matches!(
            s.put("a/k", 5, &json!({})),
            Err(StoreError::Io(_))
        ));
        assert!(s.get("a/k", None).is_ok());
        assert!(s.ops().iter().any(|(op, k)| op == "delete" && k == "a/k"));
    }
}
