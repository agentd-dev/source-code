// SPDX-License-Identifier: AGPL-3.0-only
//! **Conversations as the A2A surface sees them: one `contextId` namespace
//! per principal.**
//!
//! A `contextId` is the caller's to choose (spec: a server MAY accept and
//! preserve one), and a flat keyspace made that a door into somebody else's
//! conversation: naming it joined it, read its history back through the
//! model, and charged its owner's budget. So a non-operator's `contextId` is
//! never a key. It is bound, per principal, to a key of the runtime's own —
//! `ctx-<32 hex>` from OS randomness — and two callers who pick the same id,
//! `root` included, hold two conversations. Nothing a caller can send depends
//! on another principal's conversation existing, so no answer reveals one.
//!
//! An operator addresses conversations by those keys directly (the `status`
//! op lists them), the root context included.
//!
//! The binding is claimed at INGRESS, before the message is written ahead, so
//! a second message racing the first turn lands in the same conversation. It
//! is durable through what already persists: the task a send creates records
//! both names (`Task.context_id`, `Task.conversation`), and the context,
//! once its first turn runs, its owner and `wire_id`. [`ConversationIndex::rebuild`]
//! reads both back at restore.

use crate::a2a::Task;
use crate::context::Contexts;
use crate::runtime::reactor::Runtime;
use std::collections::BTreeMap;

/// The per-principal `contextId` → conversation-key bindings.
#[derive(Debug, Default)]
pub struct ConversationIndex {
    /// `(principal id, contextId)` → the conversation's key.
    by_wire: BTreeMap<(String, String), String>,
    /// The other way: key → `(principal id, contextId)`, so a task created
    /// in a conversation says it by its owner's name.
    by_key: BTreeMap<String, (String, String)>,
}

impl ConversationIndex {
    /// The key `principal`'s `wire` is bound to, binding a fresh one the first
    /// time. `Err` only when the OS could not supply the randomness, which
    /// binds nothing.
    pub fn claim(&mut self, principal: &str, wire: &str) -> std::io::Result<String> {
        if let Some(key) = self.key_of(principal, wire) {
            return Ok(key.to_string());
        }
        // Unguessable, and minted fresh on every claim: a key names no one's
        // conversation until it is bound, so none can be predicted before it
        // is in use either.
        let key = format!("ctx-{}", crate::sec::random::hex_token(16)?);
        self.bind(principal, wire, &key);
        Ok(key)
    }

    /// The key `principal`'s `wire` is bound to, if it is.
    pub fn key_of(&self, principal: &str, wire: &str) -> Option<&str> {
        self.by_wire
            .get(&(principal.to_string(), wire.to_string()))
            .map(String::as_str)
    }

    /// The `contextId` a conversation's owner knows it by, when the key is
    /// bound (an operator's, a run's and the root's are their own names).
    pub fn wire_of(&self, key: &str) -> Option<&str> {
        self.by_key.get(key).map(|(_, wire)| wire.as_str())
    }

    /// The principal whose `contextId` the key is bound from, when it is.
    pub fn owner_of(&self, key: &str) -> Option<&str> {
        self.by_key.get(key).map(|(owner, _)| owner.as_str())
    }

    /// Forget the binding of `key`, both ways.
    fn release(&mut self, key: &str) {
        if let Some(name) = self.by_key.remove(key) {
            self.by_wire.remove(&name);
        }
    }

    /// Record a binding; the first one recorded for a name or a key stands.
    fn bind(&mut self, principal: &str, wire: &str, key: &str) {
        self.by_wire
            .entry((principal.to_string(), wire.to_string()))
            .or_insert_with(|| key.to_string());
        self.by_key
            .entry(key.to_string())
            .or_insert_with(|| (principal.to_string(), wire.to_string()));
    }

    /// The bindings a previous life made, from what it persisted.
    ///
    /// A context records its owner and `wire_id` once its first turn has run;
    /// before that, only the task the send created knows the binding — which
    /// is why the tasks are read too, and a restart between a send and its
    /// first turn still finds the conversation the send claimed. Contexts go
    /// first: they are the conversation itself, and a task only points at it.
    pub fn rebuild(contexts: &Contexts, tasks: &BTreeMap<String, Task>) -> ConversationIndex {
        let mut index = ConversationIndex::default();
        for id in contexts.ids() {
            if let Some(c) = contexts.get(&id)
                && let (Some(owner), Some(wire)) = (&c.principal, &c.wire_id)
            {
                index.bind(owner, wire, &id);
            }
        }
        for t in tasks.values() {
            if let Some(owner) = &t.principal
                && t.conversation != t.context_id
            {
                index.bind(owner, &t.context_id, &t.conversation);
            }
        }
        index
    }

    /// [`ConversationIndex::claim`], adopting first a conversation from
    /// before the namespace existed.
    ///
    /// Up to 1.16 a caller's conversation was kept under the very id it
    /// sent, and records no `wire_id`, so nothing binds it and a plain claim
    /// would start a fresh one beside its history. It is adopted — bound
    /// under that id, to itself — when it is `principal`'s own: an owned
    /// conversation, not the root, and not already some binding's key. An id
    /// that names another principal's conversation, the root, or nothing gets
    /// the fresh key any unknown id gets, so the answer still reveals nothing.
    /// Done here, when the name is used, rather than at restore: the rule is
    /// then the same before a restart and after one.
    pub fn claim_or_adopt(
        &mut self,
        contexts: &Contexts,
        principal: &str,
        wire: &str,
    ) -> std::io::Result<String> {
        if self.key_of(principal, wire).is_none()
            && wire != crate::context::ROOT
            && !self.by_key.contains_key(wire)
            && contexts.get(wire).is_some_and(|c| {
                c.kind == crate::context::ContextKind::Conversation
                    && c.wire_id.is_none()
                    && c.principal.as_deref() == Some(principal)
            })
        {
            self.bind(principal, wire, wire);
        }
        self.claim(principal, wire)
    }

    pub fn len(&self) -> usize {
        self.by_wire.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_wire.is_empty()
    }
}

impl Runtime {
    /// The key of the conversation `principal` calls `id`, for a read that
    /// names one: an operator's `id` is the key; anyone else's is looked up
    /// in its own namespace, and names nothing it has not bound. So a caller
    /// reads its conversations by the ids it sent, and cannot read anyone
    /// else's by the key the runtime chose.
    pub(crate) fn conversation_named(
        &self,
        principal: &crate::a2a::Principal,
        id: &str,
    ) -> Option<String> {
        if principal.is_operator() {
            return Some(id.to_string());
        }
        self.conv_index
            .key_of(&principal.id, id)
            .map(str::to_string)
    }

    /// Drop the binding of `key` when nothing records it — no context holds
    /// the conversation and no task names it — so a claim that ended in a
    /// refusal leaves nothing behind that a restart would not also forget.
    #[cfg(feature = "a2a")]
    pub(crate) fn release_unused_conversation(&mut self, key: &str) {
        if self.contexts.get(key).is_none() && !self.tasks.values().any(|t| t.conversation == key) {
            self.conv_index.release(key);
        }
    }

    /// The `contextId` principal `who` knows conversation `key` by: the name
    /// it was bound from when `who` is the one who bound it, else the key.
    ///
    /// Only the owner has a name of its own for a bound conversation. Anyone
    /// else who reaches it — an operator joining by the key `status` lists —
    /// addressed it by that key, and must be answered in it: handed the
    /// owner's name instead, a client following the task's `contextId` would
    /// land in a different conversation spelled like it (an operator's name IS
    /// a key), and a continuation naming both would be refused as a mismatch.
    pub(crate) fn conversation_wire(&self, who: &str, key: &str) -> String {
        match self.conv_index.owner_of(key) {
            Some(owner) if owner == who => self.conv_index.wire_of(key).unwrap_or(key),
            _ => key,
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::Link;
    use crate::context::{Msg, ROOT};

    fn is_key(k: &str) -> bool {
        k.strip_prefix("ctx-")
            .is_some_and(|h| h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit()))
    }

    /// A claim binds `(principal, contextId)` to a fresh unguessable key, the
    /// same one every time that principal says that id, and a different one
    /// for anybody else who says it — `root` included. And the bindings are
    /// all still there after a restart, whether the conversation's first turn
    /// ran (the context records the binding) or not yet (only the task the
    /// send created does): the claim is made at ingress, and has to survive
    /// from there.
    #[test]
    fn the_index_survives_a_restart_and_claims_at_ingress() {
        let mut index = ConversationIndex::default();
        let a = index.claim("user:a", "chat").unwrap();
        assert!(is_key(&a), "{a}");
        assert_eq!(
            index.claim("user:a", "chat").unwrap(),
            a,
            "the same name, the same key"
        );
        let b = index.claim("agent:b", "chat").unwrap();
        assert!(is_key(&b));
        assert_ne!(
            a, b,
            "another principal's same name is another conversation"
        );
        let b_root = index.claim("agent:b", ROOT).unwrap();
        assert_ne!(b_root, ROOT, "no caller's name is the root context");
        assert!(is_key(&b_root));
        let a_late = index.claim("user:a", "later").unwrap();
        assert_eq!(index.wire_of(&a), Some("chat"));
        assert_eq!(index.wire_of("root"), None, "the root is nobody's binding");
        assert_eq!(index.len(), 4);

        // What a previous life persisted: A's `chat` ran a turn (the context
        // holds the binding) and so did B's (and B's task agrees); B's `root`
        // and A's `later` were accepted and written ahead, but crashed before
        // their first turn — only the tasks know them. An operator's task
        // names its conversation by the key itself, and binds nothing.
        let mut contexts = Contexts::new(1000);
        contexts
            .conversation_for(&a, "user:a", Some("chat"))
            .unwrap()
            .append(Msg::user("SECRET-A-42", Some("user:a".into())));
        contexts
            .conversation_for(&b, "agent:b", Some("chat"))
            .unwrap();
        contexts.root();
        let task = |id: &str, owner: &str, key: &str, wire: &str| {
            let mut t = Task::new(id, key, Some(owner), Link::Turn { ctx: key.into() });
            t.set_conversation(key, wire);
            (id.to_string(), t)
        };
        let tasks: BTreeMap<String, Task> = [
            task("t1", "agent:b", &b, "chat"),
            task("t2", "agent:b", &b_root, ROOT),
            task("t3", "user:a", &a_late, "later"),
            task("t4", "operator", "ops-conv", "ops-conv"),
        ]
        .into_iter()
        .collect();

        let back = ConversationIndex::rebuild(&contexts, &tasks);
        assert_eq!(back.key_of("user:a", "chat"), Some(a.as_str()));
        assert_eq!(back.key_of("agent:b", "chat"), Some(b.as_str()));
        assert_eq!(back.key_of("agent:b", ROOT), Some(b_root.as_str()));
        assert_eq!(back.key_of("user:a", "later"), Some(a_late.as_str()));
        assert_eq!(back.key_of("operator", "ops-conv"), None);
        assert_eq!(back.len(), 4, "every binding, and nothing else");
        assert_eq!(back.wire_of(&b_root), Some(ROOT));

        // After the restart, a claim finds the old binding rather than
        // minting over it.
        let mut back = back;
        assert_eq!(back.claim("user:a", "chat").unwrap(), a);
        assert_eq!(back.claim("agent:b", ROOT).unwrap(), b_root);
    }

    /// A conversation kept under its caller's own id before the namespace
    /// existed (no `wire_id`) is its owner's to continue by that id — and
    /// nobody else's: another principal, the root and a context that is
    /// already some binding's key all get a fresh conversation instead.
    #[test]
    fn a_conversation_from_before_the_namespace_is_its_owners_by_its_old_id() {
        let mut contexts = Contexts::new(1000);
        contexts
            .conversation("chat", Some("user:a"))
            .append(Msg::user("SECRET-A-42", Some("user:a".into())));
        contexts.conversation("orphan", None);
        contexts.root();
        let mut index = ConversationIndex::default();
        let bound = index.claim("user:a", "bound").unwrap();
        // Its context was made without the name (a delivery, say), so only
        // the binding says whose key it is.
        contexts.conversation(&bound, Some("user:a"));

        assert_eq!(
            index.claim_or_adopt(&contexts, "user:a", "chat").unwrap(),
            "chat",
            "the owner continues its old conversation by its old id"
        );
        assert_eq!(
            index.claim_or_adopt(&contexts, "user:a", "chat").unwrap(),
            "chat"
        );
        assert_eq!(index.wire_of("chat"), Some("chat"));

        let b = index.claim_or_adopt(&contexts, "user:b", "chat").unwrap();
        assert!(is_key(&b), "another principal's old id is not a door: {b}");
        let orphan = index.claim_or_adopt(&contexts, "user:a", "orphan").unwrap();
        assert!(
            is_key(&orphan),
            "a conversation nobody owns is nobody's to adopt"
        );
        let root = index.claim_or_adopt(&contexts, "user:a", ROOT).unwrap();
        assert!(is_key(&root), "the root is no caller's conversation");
        let again = index.claim_or_adopt(&contexts, "user:a", &bound).unwrap();
        assert_ne!(again, bound, "a key is no one's name, even its owner's");
    }
}
