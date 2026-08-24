// SPDX-License-Identifier: AGPL-3.0-only
//! **Artifacts**: named pieces of content produced by turns and steps,
//! store-backed, delivered on A2A tasks and referenced by large step outputs
//! as `{"$artifact": id}` so a big payload travels by reference rather than
//! being copied into every message that mentions it. Content is stored inline
//! (JSON or text) up to [`MAX_INLINE_BYTES`]; the record carries
//! `{name, mime, size, sha256, content, created_by, sensitive}`.

use crate::state::{Durable, Kind, now_ms, ulid};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The inline content cap (bytes of the serialized content).
pub const MAX_INLINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub sha256: String,
    pub content: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub created: u64,
    /// The A2A task / run this artifact belongs to (delivery target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl Artifact {
    pub fn meta(&self) -> Value {
        json!({"id": self.id, "name": self.name, "mime": self.mime, "size": self.size, "sha256": self.sha256, "created_by": self.created_by, "sensitive": self.sensitive, "created": self.created, "owner": self.owner})
    }
}

/// The inputs of `artifact.create`.
pub struct NewArtifact<'a> {
    pub name: &'a str,
    pub mime: Option<&'a str>,
    pub content: Value,
    pub created_by: Option<&'a str>,
    pub sensitive: bool,
    pub owner: Option<&'a str>,
}

/// The artifact registry (in-memory index of the durable records).
#[derive(Default)]
pub struct Artifacts {
    map: BTreeMap<String, Artifact>,
}

impl Artifacts {
    pub fn new() -> Artifacts {
        Artifacts::default()
    }

    /// Adopt restored records.
    pub fn restore(&mut self, envelopes: &[crate::store::Envelope]) -> usize {
        let mut n = 0;
        for env in envelopes {
            if let Ok(a) = serde_json::from_value::<Artifact>(env.state.clone()) {
                self.map.insert(a.id.clone(), a);
                n += 1;
            }
        }
        n
    }

    /// `artifact.create`.
    pub fn create(&mut self, d: &Durable, spec: NewArtifact<'_>) -> Result<Value, String> {
        let NewArtifact {
            name,
            mime,
            content,
            created_by,
            sensitive,
            owner,
        } = spec;
        if name.trim().is_empty() {
            return Err("artifact.create: name must be non-empty".into());
        }
        let serialized = match &content {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if serialized.len() > MAX_INLINE_BYTES {
            return Err(format!(
                "artifact.create: content is {} bytes; the inline cap is {MAX_INLINE_BYTES}",
                serialized.len()
            ));
        }
        let mime = mime.map(str::to_string).unwrap_or_else(|| {
            if content.is_string() {
                "text/plain".into()
            } else {
                "application/json".into()
            }
        });
        let a = Artifact {
            id: ulid::new(),
            name: name.trim().to_string(),
            mime,
            size: serialized.len() as u64,
            sha256: crate::sha::sha256_hex(serialized.as_bytes()),
            content,
            created_by: created_by.map(str::to_string),
            sensitive,
            created: now_ms(),
            owner: owner.map(str::to_string),
        };
        d.put(
            Kind::Artifact,
            &a.id,
            serde_json::to_value(&a).unwrap_or(Value::Null),
            Some(a.sha256.clone()),
        )
        .map_err(|e| e.to_string())?;
        let meta = a.meta();
        self.map.insert(a.id.clone(), a);
        Ok(meta)
    }

    /// `artifact.get`.
    pub fn get(&self, id: &str) -> Option<&Artifact> {
        self.map.get(id)
    }

    /// `artifact.get` as a tool result: the metadata plus the inline content.
    /// The `sensitive` flag rides along on the metadata so the caller's own
    /// redaction rules can act on it.
    pub fn get_value(&self, id: &str) -> Result<Value, String> {
        let a = self
            .map
            .get(id)
            .ok_or_else(|| format!("no such artifact {id:?}"))?;
        let mut v = a.meta();
        v["content"] = a.content.clone();
        Ok(v)
    }

    /// `artifact.delete`.
    pub fn delete(&mut self, d: &Durable, id: &str) -> Result<Value, String> {
        if self.map.remove(id).is_none() {
            return Err(format!("no such artifact {id:?}"));
        }
        d.delete(Kind::Artifact, id).map_err(|e| e.to_string())?;
        Ok(json!({"ok": true, "id": id}))
    }

    /// `artifact.list`.
    pub fn list(&self, prefix: Option<&str>, limit: Option<usize>, owner: Option<&str>) -> Value {
        let limit = limit.unwrap_or(100).max(1);
        let mut items: Vec<Value> = self
            .map
            .values()
            .filter(|a| prefix.is_none_or(|p| a.name.starts_with(p)))
            .filter(|a| owner.is_none_or(|o| a.owner.as_deref() == Some(o)))
            .map(Artifact::meta)
            .collect();
        let truncated = items.len() > limit;
        items.truncate(limit);
        json!({"artifacts": items, "truncated": truncated})
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Policy;
    use crate::store::memory::MemoryStore;
    use std::sync::Arc;

    #[test]
    fn create_get_list_delete_and_restore() {
        let d = Durable::new(
            Arc::new(MemoryStore::new()),
            "agentd",
            "i",
            Policy::default(),
            None,
        );
        let mut a = Artifacts::new();
        let m = a
            .create(
                &d,
                NewArtifact {
                    name: "report.md",
                    mime: None,
                    content: json!("# hi"),
                    created_by: Some("root"),
                    sensitive: false,
                    owner: Some("task-1"),
                },
            )
            .unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        assert_eq!(m["mime"], json!("text/plain"));
        assert_eq!(m["size"], json!(4));
        let m2 = a
            .create(
                &d,
                NewArtifact {
                    name: "data.json",
                    mime: None,
                    content: json!({"a": 1}),
                    created_by: None,
                    sensitive: true,
                    owner: None,
                },
            )
            .unwrap();
        assert_eq!(m2["mime"], json!("application/json"));
        assert_eq!(a.get_value(&id).unwrap()["content"], json!("# hi"));
        assert_eq!(
            a.list(Some("rep"), None, None)["artifacts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            a.list(None, None, Some("task-1"))["artifacts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(a.list(None, Some(1), None)["truncated"], json!(true));
        assert!(
            a.create(
                &d,
                NewArtifact {
                    name: "",
                    mime: None,
                    content: json!(1),
                    created_by: None,
                    sensitive: false,
                    owner: None,
                },
            )
            .is_err()
        );
        // Restore.
        let mut b = Artifacts::new();
        assert_eq!(b.restore(d.restore().unwrap().of(Kind::Artifact)), 2);
        assert!(b.get(&id).is_some());
        b.delete(&d, &id).unwrap();
        assert!(b.delete(&d, &id).is_err());
        assert_eq!(d.restore().unwrap().of(Kind::Artifact).len(), 1);
    }
}
