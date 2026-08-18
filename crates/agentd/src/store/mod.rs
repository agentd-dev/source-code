// SPDX-License-Identifier: AGPL-3.0-only
//! The **state store** contract and adapters (RFC 0025 §2, §4).
//!
//! agentd's durability rests on four operations —
//! `put(key, seq, envelope) / get(key[, seq]) / list(prefix) / delete(key)` —
//! implemented by an adapter chosen in `store.kind`: [`mcp`] (any MCP server's
//! tools, mapped), [`http`] (plain HTTP), [`file`] (the local filesystem, RFC
//! 0033 — durable for one host, single-writer), or [`memory`] (in-process;
//! tests and dev). `put` is a **compare-and-set on `seq`**: the stored seq
//! must be lower, else `Conflict` — the split-brain guard every caller treats
//! as fatal.
//! agentd links no database client and defines no schema beyond the
//! [`Envelope`].

pub mod file;
pub mod http;
pub mod mapping;
pub mod mcp;
pub mod memory;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// The envelope major this build writes and accepts (RFC 0025 §3.2).
pub const ENVELOPE_VERSION: u32 = 2;

/// A versioned store record: `state` is the kind-specific payload; a tombstone
/// carries `state: null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub kind: String,
    pub id: String,
    pub seq: u64,
    /// Unix ms.
    pub ts: u64,
    pub instance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    pub state: Value,
}

impl Envelope {
    pub fn new(
        kind: &str,
        id: &str,
        seq: u64,
        instance: &str,
        hash: Option<String>,
        state: Value,
    ) -> Envelope {
        Envelope {
            v: ENVELOPE_VERSION,
            kind: kind.to_string(),
            id: id.to_string(),
            seq,
            ts: now_ms(),
            instance: instance.to_string(),
            hash,
            state,
        }
    }

    pub fn is_tombstone(&self) -> bool {
        self.state.is_null()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Parse a stored value; refuses an unknown envelope major.
    pub fn from_value(v: Value) -> Result<Envelope, StoreError> {
        let env: Envelope = serde_json::from_value(v)
            .map_err(|e| StoreError::Corrupt(format!("envelope does not parse: {e}")))?;
        if env.v != ENVELOPE_VERSION {
            return Err(StoreError::Corrupt(format!(
                "envelope version {} is not supported (this build writes {})",
                env.v, ENVELOPE_VERSION
            )));
        }
        Ok(env)
    }
}

/// `<prefix>/<instance>/<kind>/<id>` (RFC 0025 §3.1).
pub fn key(prefix: &str, instance: &str, kind: &str, id: &str) -> String {
    format!("{prefix}/{instance}/{kind}/{id}")
}

/// Split a key produced by [`key`] back into `(kind, id)` for the given
/// prefix/instance; `None` for a foreign key.
pub fn parse_key<'a>(prefix: &str, instance: &str, k: &'a str) -> Option<(&'a str, &'a str)> {
    let rest = k.strip_prefix(&format!("{prefix}/{instance}/"))?;
    let (kind, id) = rest.split_once('/')?;
    Some((kind, id))
}

/// The outcome of a `put`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    Ok,
    /// Another writer owns the key (a stored seq ≥ ours). Fatal for the writer.
    Conflict {
        latest_seq: Option<u64>,
    },
}

/// A `list` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySeq {
    pub key: String,
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// Transport / server failure (retryable at the caller's discretion).
    Io(String),
    /// The adapter does not implement this optional operation.
    Unsupported(&'static str),
    /// A mapping template or extraction failed (a config problem).
    Mapping(String),
    /// A stored record is unreadable.
    Corrupt(String),
    /// A `put` conflict surfaced as an error by a caller that treats it as fatal.
    Conflict(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(m) => write!(f, "store i/o: {m}"),
            StoreError::Unsupported(op) => {
                write!(f, "store: {op} is not supported by this adapter")
            }
            StoreError::Mapping(m) => write!(f, "store mapping: {m}"),
            StoreError::Corrupt(m) => write!(f, "store record: {m}"),
            StoreError::Conflict(m) => write!(f, "store conflict: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// The four-operation contract (RFC 0025 §2). Implementations are `Send +
/// Sync` so the runtime's executor pool can call them; every operation is
/// bounded by the adapter's timeout.
pub trait Store: Send + Sync {
    /// Compare-and-set write: `seq` must exceed the stored one.
    fn put(&self, key: &str, seq: u64, envelope: &Value) -> Result<PutOutcome, StoreError>;
    /// The latest record (or the pinned `seq` if the store keeps history).
    fn get(&self, key: &str, seq: Option<u64>) -> Result<Option<Value>, StoreError>;
    /// Keys under `prefix` (optional; `Unsupported` is a legal answer).
    fn list(&self, prefix: &str) -> Result<Vec<KeySeq>, StoreError>;
    /// Remove a key (optional; `Unsupported` ⇒ callers tombstone via `put`).
    fn delete(&self, key: &str) -> Result<(), StoreError>;
    /// The adapter kind (`mcp` / `http` / `file` / `memory`), for status and
    /// metrics.
    fn kind(&self) -> &'static str;
}

/// A shared store handle.
pub type SharedStore = Arc<dyn Store>;

/// The store timeout class: the management timeout (RFC 0016 §10) unless the
/// settings say otherwise.
pub fn default_timeout() -> Duration {
    crate::obs::health::management_timeout()
}

/// Bounded retry on `Io` errors (never on `Conflict`/`Mapping`/`Corrupt`).
pub fn with_retry<T>(
    mut op: impl FnMut() -> Result<T, StoreError>,
    attempts: u32,
) -> Result<T, StoreError> {
    let mut last = None;
    for n in 0..attempts.max(1) {
        match op() {
            Err(StoreError::Io(m)) => {
                last = Some(StoreError::Io(m));
                if n + 1 < attempts {
                    std::thread::sleep(Duration::from_millis(50 * (n as u64 + 1)));
                }
            }
            other => return other,
        }
    }
    Err(last.unwrap_or(StoreError::Io("no attempts".into())))
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Build the store an instance is configured with (RFC 0030 §3.5). `servers`
/// resolves the `mcp` adapter's coordination server by name; `kind: none`
/// yields no store (the caller decides whether that is allowed).
pub fn open(
    settings: &crate::config::v2::Store,
    servers: &dyn Fn(&str) -> Option<Arc<dyn mcp::McpCall>>,
) -> Result<Option<SharedStore>, StoreError> {
    use crate::config::v2::StoreKind;
    let timeout = settings
        .timeout
        .map(|d| d.0)
        .unwrap_or_else(default_timeout);
    match settings.kind {
        StoreKind::None => Ok(None),
        StoreKind::Memory => Ok(Some(Arc::new(memory::MemoryStore::new()))),
        StoreKind::Mcp => {
            let cfg = settings.mcp.as_ref().ok_or_else(|| {
                StoreError::Mapping("store.kind is mcp but store.mcp is not set".into())
            })?;
            let client = servers(&cfg.server).ok_or_else(|| {
                StoreError::Mapping(format!(
                    "store.mcp.server '{}' is not a connected MCP server",
                    cfg.server
                ))
            })?;
            Ok(Some(Arc::new(mcp::McpStore::new(
                client,
                cfg.clone(),
                timeout,
            ))))
        }
        StoreKind::File => {
            // The root is resolved by the config module (RFC 0033 §4) so the
            // startup log, `--capabilities` and this open all name the same
            // directory. `store.file` may be absent entirely — the chain then
            // runs on the environment alone.
            let root = crate::config::v2::file_store_root(settings);
            // `open` takes the exclusive instance lock, and a held lock arrives
            // as `Io` (RFC 0033 §4.1) carrying the holder's pid. Name the
            // adapter in front of it: the operator reads this at exit, where
            // "store i/o: …" alone would not say which store or which path.
            let store = file::FileStore::open(&root).map_err(|e| match e {
                StoreError::Io(m) => StoreError::Io(format!("store.file: {m}")),
                other => other,
            })?;
            Ok(Some(Arc::new(store)))
        }
        StoreKind::Http => {
            let cfg = settings.http.as_ref().ok_or_else(|| {
                StoreError::Mapping("store.kind is http but store.http is not set".into())
            })?;
            Ok(Some(Arc::new(http::HttpStore::new(cfg.clone(), timeout)?)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_round_trips_and_refuses_unknown_major() {
        let e = Envelope::new("run", "01J", 3, "inst", Some("abc".into()), json!({"a": 1}));
        let v = e.to_value();
        assert_eq!(v["v"], json!(2));
        assert_eq!(v["seq"], json!(3));
        let back = Envelope::from_value(v).unwrap();
        assert_eq!(back, e);
        let mut bad = e.to_value();
        bad["v"] = json!(9);
        assert!(matches!(
            Envelope::from_value(bad),
            Err(StoreError::Corrupt(_))
        ));
        assert!(!e.is_tombstone());
        assert!(Envelope::new("run", "x", 1, "i", None, Value::Null).is_tombstone());
    }

    #[test]
    fn keys_compose_and_parse() {
        let k = key("agentd", "inst-0", "run", "01J");
        assert_eq!(k, "agentd/inst-0/run/01J");
        assert_eq!(parse_key("agentd", "inst-0", &k), Some(("run", "01J")));
        assert_eq!(parse_key("agentd", "other", &k), None);
        // Ids may contain slashes (a2a contextId…): kind is the first segment.
        assert_eq!(
            parse_key("agentd", "i", "agentd/i/task/a/b"),
            Some(("task", "a/b"))
        );
    }

    #[test]
    fn retry_only_on_io() {
        let mut n = 0;
        let r: Result<(), StoreError> = with_retry(
            || {
                n += 1;
                Err(StoreError::Io("down".into()))
            },
            3,
        );
        assert!(matches!(r, Err(StoreError::Io(_))));
        assert_eq!(n, 3);
        let mut m = 0;
        let r: Result<(), StoreError> = with_retry(
            || {
                m += 1;
                Err(StoreError::Mapping("bad".into()))
            },
            3,
        );
        assert!(matches!(r, Err(StoreError::Mapping(_))));
        assert_eq!(m, 1);
    }
}
