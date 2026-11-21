//! What a peer declares of an extension, read the way SEP-3392 asks.
//!
//! An extension's identifier never changes with its stage. A Beta or
//! Experimental extension that breaks compatibility adds or bumps a
//! `revision` string in its settings object instead; no `revision` means the
//! extension's initial revision, and a peer that receives a `revision` it
//! does not know SHOULD NOT enable the extension (SEP-3392, "Lifecycle",
//! seps/3392-extension-lifecycle.md L181-191). agentd implements the initial
//! revision of every extension it speaks, so settings that name any
//! `revision` leave the extension off.

use serde_json::Value;

/// The revisions agentd knows of an extension: none past the initial one.
const KNOWN_REVISIONS: &[&str] = &[];

/// How a peer's `extensions` object stands on one extension.
#[derive(Debug, Clone, PartialEq)]
pub enum Declared<'a> {
    /// Not declared, or its settings are not an object.
    No,
    /// Declared at a revision agentd does not know: not enabled.
    UnknownRevision(String),
    /// Declared at a revision agentd speaks; its settings.
    Yes(&'a Value),
}

/// What `extensions` (a capabilities object's `extensions` member) declares
/// of the extension `id`.
pub fn declared<'a>(extensions: Option<&'a Value>, id: &str) -> Declared<'a> {
    let Some(settings) = extensions.and_then(|e| e.get(id)).filter(|v| v.is_object()) else {
        return Declared::No;
    };
    match settings.get("revision") {
        None => Declared::Yes(settings),
        Some(Value::String(r)) if KNOWN_REVISIONS.contains(&r.as_str()) => Declared::Yes(settings),
        Some(other) => Declared::UnknownRevision(match other {
            Value::String(r) => r.clone(),
            v => v.to_string(),
        }),
    }
}

impl<'a> Declared<'a> {
    /// The settings, when the extension may be enabled.
    pub fn settings(&self) -> Option<&'a Value> {
        match self {
            Declared::Yes(s) => Some(s),
            _ => None,
        }
    }

    /// Why the extension is off, for a log line: `None` when it is on.
    pub fn refusal(&self, id: &str) -> Option<String> {
        match self {
            Declared::Yes(_) => None,
            Declared::No => Some(format!("the server does not declare {id}")),
            Declared::UnknownRevision(r) => Some(format!(
                "the server declares {id} at revision {r:?}, which agentd does not implement (SEP-3392)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "io.modelcontextprotocol/skills";

    #[test]
    fn no_revision_is_the_initial_one() {
        let e = json!({ID: {"directoryRead": true}});
        assert_eq!(
            declared(Some(&e), ID).settings(),
            Some(&json!({"directoryRead": true}))
        );
        assert_eq!(declared(Some(&e), ID).refusal(ID), None);
    }

    #[test]
    fn an_unknown_revision_leaves_the_extension_off() {
        let e = json!({ID: {"revision": "2026-12-01"}});
        let d = declared(Some(&e), ID);
        assert_eq!(d, Declared::UnknownRevision("2026-12-01".into()));
        assert!(d.settings().is_none());
        assert!(d.refusal(ID).unwrap().contains("2026-12-01"), "{d:?}");
        // a revision that is not a string is not one agentd knows either
        let e = json!({ID: {"revision": 2}});
        assert_eq!(
            declared(Some(&e), ID),
            Declared::UnknownRevision("2".into())
        );
    }

    #[test]
    fn absent_or_not_an_object_is_not_declared() {
        assert_eq!(declared(None, ID), Declared::No);
        assert_eq!(declared(Some(&json!({})), ID), Declared::No);
        assert_eq!(declared(Some(&json!({ID: true})), ID), Declared::No);
    }
}
