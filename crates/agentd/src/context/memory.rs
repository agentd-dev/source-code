// SPDX-License-Identifier: AGPL-3.0-only
//! **Agent memory**: a durable JSON key/value space in the instance's store
//! namespace — `memory/<key>` — with optional TTL, size caps, and prefix
//! listing. Listing uses the store's own `list` when it has one; a store
//! without one is served from the `memory/_index` record this module
//! maintains, so listing works on every backend rather than only the ones that
//! can enumerate. Overridable by an MCP memory server through the registry.

use crate::state::{Durable, Kind, now_ms};
use crate::store::StoreError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The index record's id (never a user key: keys may not start with `_`).
pub const INDEX_ID: &str = "_index";

/// One memory record (`state` of the envelope).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub value: Value,
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

impl Record {
    pub fn expired(&self, now: u64) -> bool {
        self.ttl_ms
            .is_some_and(|ttl| now >= self.ts.saturating_add(ttl))
    }
    pub fn meta(&self) -> Value {
        json!({"ts": self.ts, "ttl_ms": self.ttl_ms, "by": self.by})
    }
}

/// The memory façade over the store.
pub struct Memory {
    max_value_bytes: usize,
    list_default_limit: usize,
    /// `Some(index)` when the store has no `list` (probed lazily).
    index: Option<BTreeMap<String, u64>>,
    probed: bool,
}

impl Memory {
    pub fn new(max_value_bytes: usize, list_default_limit: usize) -> Memory {
        Memory {
            max_value_bytes,
            list_default_limit,
            index: None,
            probed: false,
        }
    }

    /// Validate a key: non-empty, no whitespace, not reserved, bounded.
    pub fn check_key(key: &str) -> Result<(), String> {
        if key.is_empty() || key.len() > 256 {
            return Err("memory key must be 1..=256 chars".into());
        }
        if key.starts_with('_') {
            return Err("memory keys starting with '_' are reserved".into());
        }
        if key.chars().any(char::is_whitespace) {
            return Err("memory key must not contain whitespace".into());
        }
        Ok(())
    }

    fn probe(&mut self, d: &Durable) {
        if self.probed {
            return;
        }
        self.probed = true;
        match d.list(Kind::Memory) {
            Ok(_) => self.index = None,
            Err(StoreError::Unsupported(_)) => {
                let mut idx = BTreeMap::new();
                if let Ok(Some(env)) = d.get(Kind::Memory, INDEX_ID)
                    && let Some(m) = env.state.get("keys").and_then(Value::as_object)
                {
                    for (k, v) in m {
                        idx.insert(k.clone(), v.as_u64().unwrap_or(0));
                    }
                }
                self.index = Some(idx);
            }
            Err(_) => {}
        }
    }

    fn write_index(&self, d: &Durable) -> Result<(), StoreError> {
        if let Some(idx) = &self.index {
            d.put(Kind::Memory, INDEX_ID, json!({"keys": idx}), None)?;
        }
        Ok(())
    }

    /// `memory.set {key, value, ttl?}` → the record's meta.
    pub fn set(
        &mut self,
        d: &Durable,
        key: &str,
        value: Value,
        ttl_ms: Option<u64>,
        by: Option<&str>,
    ) -> Result<Value, String> {
        Self::check_key(key)?;
        let bytes = value.to_string().len();
        if bytes > self.max_value_bytes {
            return Err(format!(
                "memory value is {bytes} bytes; memory.max_value_bytes is {}",
                self.max_value_bytes
            ));
        }
        self.probe(d);
        let rec = Record {
            value,
            ts: now_ms(),
            ttl_ms,
            by: by.map(str::to_string),
        };
        d.put(
            Kind::Memory,
            key,
            serde_json::to_value(&rec).unwrap_or(Value::Null),
            None,
        )
        .map_err(|e| e.to_string())?;
        if let Some(idx) = &mut self.index {
            idx.insert(key.to_string(), rec.ts);
            self.write_index(d).map_err(|e| e.to_string())?;
        }
        Ok(json!({"ok": true, "key": key, "meta": rec.meta()}))
    }

    /// `memory.push {key, value}` — append to the ARRAY at `key`, creating it.
    /// The durable queue primitive: producers push, a consumer shifts, and the
    /// list survives restarts. Read-modify-write is atomic here because the
    /// reactor is the single writer.
    pub fn push(
        &mut self,
        d: &Durable,
        key: &str,
        value: Value,
        by: Option<&str>,
    ) -> Result<Value, String> {
        let cur = self.get(d, key)?;
        let mut arr = if cur["found"] == json!(true) {
            match cur["value"].as_array() {
                Some(a) => a.clone(),
                None => return Err(format!("memory.push: the value at {key:?} is not an array")),
            }
        } else {
            Vec::new()
        };
        arr.push(value);
        let length = arr.len();
        self.set(d, key, Value::Array(arr), None, by)?;
        Ok(json!({"ok": true, "key": key, "length": length}))
    }

    /// `memory.shift {key}` — remove and return the FIRST element
    /// (`{found: false}` on empty or absent — never an error, so a drain loop
    /// can just stop).
    pub fn shift(&mut self, d: &Durable, key: &str, by: Option<&str>) -> Result<Value, String> {
        self.take(d, key, by, true)
    }

    /// `memory.pop {key}` — remove and return the LAST element.
    pub fn pop(&mut self, d: &Durable, key: &str, by: Option<&str>) -> Result<Value, String> {
        self.take(d, key, by, false)
    }

    fn take(
        &mut self,
        d: &Durable,
        key: &str,
        by: Option<&str>,
        first: bool,
    ) -> Result<Value, String> {
        let cur = self.get(d, key)?;
        if cur["found"] != json!(true) {
            return Ok(json!({"found": false, "key": key, "remaining": 0}));
        }
        let mut arr = match cur["value"].as_array() {
            Some(a) => a.clone(),
            None => {
                return Err(format!(
                    "memory.{}: the value at {key:?} is not an array",
                    if first { "shift" } else { "pop" }
                ));
            }
        };
        if arr.is_empty() {
            return Ok(json!({"found": false, "key": key, "remaining": 0}));
        }
        let value = if first {
            arr.remove(0)
        } else {
            arr.pop().expect("non-empty")
        };
        let remaining = arr.len();
        self.set(d, key, Value::Array(arr), None, by)?;
        Ok(json!({"found": true, "key": key, "value": value, "remaining": remaining}))
    }

    /// `memory.get {key}` → `{value?, meta?, found}` (expired ⇒ not found).
    pub fn get(&mut self, d: &Durable, key: &str) -> Result<Value, String> {
        Self::check_key(key)?;
        match d.get(Kind::Memory, key).map_err(|e| e.to_string())? {
            None => Ok(json!({"found": false, "key": key})),
            Some(env) => {
                let rec: Record = serde_json::from_value(env.state)
                    .map_err(|e| format!("memory record {key}: {e}"))?;
                if rec.expired(now_ms()) {
                    let _ = self.delete(d, key);
                    return Ok(json!({"found": false, "key": key, "expired": true}));
                }
                Ok(json!({"found": true, "key": key, "value": rec.value, "meta": rec.meta()}))
            }
        }
    }

    /// `memory.delete {key}`.
    pub fn delete(&mut self, d: &Durable, key: &str) -> Result<Value, String> {
        Self::check_key(key)?;
        self.probe(d);
        d.delete(Kind::Memory, key).map_err(|e| e.to_string())?;
        if let Some(idx) = &mut self.index {
            idx.remove(key);
            self.write_index(d).map_err(|e| e.to_string())?;
        }
        Ok(json!({"ok": true, "key": key}))
    }

    /// `memory.list {prefix?, limit?}` → `{keys: [{key, ts?}], truncated}`.
    pub fn list(
        &mut self,
        d: &Durable,
        prefix: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, String> {
        self.probe(d);
        let limit = limit.unwrap_or(self.list_default_limit).max(1);
        let prefix = prefix.unwrap_or("");
        let mut keys: Vec<Value> = match &self.index {
            Some(idx) => idx
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, ts)| json!({"key": k, "ts": ts}))
                .collect(),
            None => {
                let listed = d.list(Kind::Memory).map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for ks in listed {
                    let Some((_, id)) = crate::store::parse_key(d.prefix(), d.instance(), &ks.key)
                    else {
                        continue;
                    };
                    if id == INDEX_ID || !id.starts_with(prefix) {
                        continue;
                    }
                    out.push(json!({"key": id, "seq": ks.seq}));
                }
                out
            }
        };
        keys.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
        let truncated = keys.len() > limit;
        keys.truncate(limit);
        Ok(json!({"keys": keys, "truncated": truncated}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Policy;
    use crate::store::memory::MemoryStore;
    use std::sync::Arc;

    #[test]
    fn set_get_list_delete_ttl_and_caps() {
        let mem = Arc::new(MemoryStore::new());
        let d = Durable::new(mem, "agentd", "i", Policy::default(), None);
        let mut m = Memory::new(64, 10);
        m.set(&d, "user/name", json!("andrii"), None, Some("root"))
            .unwrap();
        m.set(&d, "user/tz", json!("Europe/Kyiv"), Some(1), None)
            .unwrap();
        m.set(&d, "other", json!({"a": 1}), None, None).unwrap();
        let g = m.get(&d, "user/name").unwrap();
        assert_eq!(g["found"], json!(true));
        assert_eq!(g["value"], json!("andrii"));
        assert_eq!(g["meta"]["by"], json!("root"));
        std::thread::sleep(std::time::Duration::from_millis(3));
        let g = m.get(&d, "user/tz").unwrap();
        assert_eq!(g["found"], json!(false), "expired: {g}");
        assert_eq!(g["expired"], json!(true));
        let l = m.list(&d, Some("user/"), None).unwrap();
        assert_eq!(l["keys"].as_array().unwrap().len(), 1, "{l}");
        assert_eq!(l["keys"][0]["key"], json!("user/name"));
        let all = m.list(&d, None, Some(1)).unwrap();
        assert_eq!(all["truncated"], json!(true));
        m.delete(&d, "user/name").unwrap();
        assert_eq!(m.get(&d, "user/name").unwrap()["found"], json!(false));
        // Caps + key rules.
        assert!(
            m.set(&d, "big", json!("x".repeat(100)), None, None)
                .is_err()
        );
        assert!(m.set(&d, "_reserved", json!(1), None, None).is_err());
        assert!(m.set(&d, "has space", json!(1), None, None).is_err());
        assert!(m.get(&d, "").is_err());
    }

    #[test]
    fn index_record_is_kept_when_the_store_cannot_list() {
        // A store whose list is Unsupported.
        struct NoList(MemoryStore);
        impl crate::store::Store for NoList {
            fn put(
                &self,
                k: &str,
                s: u64,
                e: &Value,
            ) -> Result<crate::store::PutOutcome, StoreError> {
                self.0.put(k, s, e)
            }
            fn get(&self, k: &str, s: Option<u64>) -> Result<Option<Value>, StoreError> {
                self.0.get(k, s)
            }
            fn list(&self, _p: &str) -> Result<Vec<crate::store::KeySeq>, StoreError> {
                Err(StoreError::Unsupported("list"))
            }
            fn delete(&self, k: &str) -> Result<(), StoreError> {
                self.0.delete(k)
            }
            fn kind(&self) -> &'static str {
                "nolist"
            }
        }
        let d = Durable::new(
            Arc::new(NoList(MemoryStore::new())),
            "agentd",
            "i",
            Policy::default(),
            None,
        );
        let mut m = Memory::new(1024, 10);
        m.set(&d, "a", json!(1), None, None).unwrap();
        m.set(&d, "b", json!(2), None, None).unwrap();
        let idx = d
            .get(Kind::Memory, INDEX_ID)
            .unwrap()
            .expect("index record");
        assert!(idx.state["keys"].get("a").is_some());
        assert_eq!(
            m.list(&d, None, None).unwrap()["keys"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        m.delete(&d, "a").unwrap();
        // A fresh façade rebuilds its index from the record.
        let mut m2 = Memory::new(1024, 10);
        let l = m2.list(&d, None, None).unwrap();
        assert_eq!(l["keys"].as_array().unwrap().len(), 1);
        assert_eq!(l["keys"][0]["key"], json!("b"));
    }
}
