// SPDX-License-Identifier: Apache-2.0
//! The supervision tree — the in-memory record of the subagent process tree.
//! RFC 0002 §supervision-record, RFC 0003 §accounting, RFC 0009 §caps.
//!
//! This module owns the *bookkeeping*: who spawned whom, each node's depth and
//! `agent_path`, hierarchical token accounting to the root, the tree-wide
//! `draining` flag, and the **spawn chokepoint** that enforces the fork-bomb
//! caps. It is pure logic — no processes, pipes, or signals (those are
//! `spawn.rs`/`reap.rs`/`kill.rs`). Depth is **minted here** from the parent's
//! record, never trusted from a child's request (RFC 0009).

use std::collections::HashMap;
use std::time::Instant;

/// A std-only token bucket for the tree-wide **spawn-rate** cap (RFC 0009 §3.6:
/// 8 burst, 2 tokens/s refill). Hand-rolled — a rate limiter is `Instant` +
/// arithmetic, never a crate. Refill is **lazy**: every `try_take` first credits
/// the tokens that have accrued since the last call, then spends one if it can.
/// This catches a *fast churn loop* that stays under the absolute subagent count
/// — a wedged child hammering `subagent.spawn` just keeps getting refusals.
#[derive(Debug, Clone, Copy)]
pub struct TokenBucket {
    /// Burst ceiling — tokens never accrue past this.
    capacity: f64,
    /// Tokens currently available (fractional; whole tokens are spendable).
    tokens: f64,
    /// Steady-state refill rate, tokens per second.
    refill_per_sec: f64,
    /// When the bucket was last refilled (the lazy-refill anchor).
    last: Instant,
}

impl TokenBucket {
    /// A bucket that starts full (`burst` tokens) and refills `per_sec` tokens
    /// each second up to `burst`.
    pub fn new(burst: u32, per_sec: f64) -> TokenBucket {
        let capacity = f64::from(burst);
        TokenBucket {
            capacity,
            tokens: capacity,
            refill_per_sec: per_sec,
            last: Instant::now(),
        }
    }

    /// Lazy-refill to `now`, then spend one token if one is available. Returns
    /// whether a token was taken (i.e. whether the spawn may proceed). `now` is
    /// explicit so the refill is deterministically unit-testable; `last` only
    /// ever moves forward (a non-monotonic `now` simply adds no tokens).
    pub fn try_take_at(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Spend one token against the wall clock (the production entry point).
    pub fn try_take(&mut self) -> bool {
        self.try_take_at(Instant::now())
    }
}

/// Stable per-tree node id (the root is `0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// Spawned, awaiting the child's `Ready` frame.
    Spawning,
    Running,
    /// Reached a terminal status cleanly.
    Done,
    /// Crashed / killed / fatal-failed.
    Failed,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub depth: u32,
    /// Dotted tree path for log correlation (`0`, `0.2`, `0.2.1`).
    pub agent_path: String,
    pub status: NodeStatus,
    /// Tokens charged to this node alone.
    pub tokens: u64,
    pub children: Vec<NodeId>,
}

/// Fork-bomb / runaway-recursion caps, enforced at the one spawn chokepoint
/// (RFC 0009). Conservative defaults; a spawn exceeding any of these is
/// **refused as a tool result**, never a crash.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub max_depth: u32,
    pub max_children: u32,
    pub max_total: u32,
    pub tree_token_ceiling: u64,
    /// Spawn-rate token bucket burst (RFC 0009 §3.6).
    pub spawn_rate_burst: u32,
    /// Spawn-rate token bucket refill, tokens per second (RFC 0009 §3.6).
    pub spawn_rate_per_sec: f64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            max_depth: 4,
            max_children: 8,
            max_total: 64,
            tree_token_ceiling: 2_000_000,
            spawn_rate_burst: 8,
            spawn_rate_per_sec: 2.0,
        }
    }
}

/// Why a spawn was refused. Surfaced to the requesting agent as a tool result
/// so its model can adapt (RFC 0009) — not an error that crashes the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnRefused {
    Draining,
    MaxDepth,
    MaxChildren,
    MaxTotal,
    RateExceeded,
    TreeBudget,
    UnknownParent,
}

impl SpawnRefused {
    pub fn as_str(self) -> &'static str {
        match self {
            SpawnRefused::Draining => "tree is draining; no new subagents",
            SpawnRefused::MaxDepth => "max subagent depth reached",
            SpawnRefused::MaxChildren => "parent has too many children",
            SpawnRefused::MaxTotal => "max total subagents reached",
            SpawnRefused::RateExceeded => "spawn rate exceeded",
            SpawnRefused::TreeBudget => "tree token budget exhausted",
            SpawnRefused::UnknownParent => "unknown parent handle",
        }
    }
}

pub struct Tree {
    nodes: HashMap<NodeId, Node>,
    next_id: u64,
    root: Option<NodeId>,
    draining: bool,
    /// Tree-wide token total (source of truth for the ceiling).
    total_tokens: u64,
    /// Tree-wide spawn-rate limiter (RFC 0009 §3.6), enforced in `mint_child`.
    spawn_bucket: TokenBucket,
    caps: Caps,
}

impl Tree {
    pub fn new(caps: Caps) -> Tree {
        Tree {
            nodes: HashMap::new(),
            next_id: 0,
            root: None,
            draining: false,
            total_tokens: 0,
            spawn_bucket: TokenBucket::new(caps.spawn_rate_burst, caps.spawn_rate_per_sec),
            caps,
        }
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }
    pub fn is_draining(&self) -> bool {
        self.draining
    }
    pub fn total_tokens(&self) -> u64 {
        self.total_tokens
    }
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }
    pub fn root(&self) -> Option<NodeId> {
        self.root
    }

    /// Mint the root node (depth 0, path `0`). The one-shot/loop root agent.
    pub fn mint_root(&mut self) -> Result<NodeId, SpawnRefused> {
        if self.draining {
            return Err(SpawnRefused::Draining);
        }
        let id = self.alloc(None, 0, "0".to_string());
        self.root = Some(id);
        Ok(id)
    }

    /// Mint a child of `parent`, enforcing every cap. **Depth and path are
    /// derived from the parent here** — the chokepoint (RFC 0009). The caller
    /// then attaches the OS handle to the returned node.
    pub fn mint_child(&mut self, parent: NodeId) -> Result<NodeId, SpawnRefused> {
        if self.draining {
            return Err(SpawnRefused::Draining);
        }
        if self.total_tokens >= self.caps.tree_token_ceiling {
            return Err(SpawnRefused::TreeBudget);
        }
        if self.nodes.len() as u32 >= self.caps.max_total {
            return Err(SpawnRefused::MaxTotal);
        }
        let (depth, child_index, parent_path) = {
            let p = self.nodes.get(&parent).ok_or(SpawnRefused::UnknownParent)?;
            if p.depth + 1 > self.caps.max_depth {
                return Err(SpawnRefused::MaxDepth);
            }
            if p.children.len() as u32 >= self.caps.max_children {
                return Err(SpawnRefused::MaxChildren);
            }
            (p.depth + 1, p.children.len(), p.agent_path.clone())
        };
        // Spawn-rate cap (RFC 0009 §3.6): catches a fast churn loop that stays
        // under the absolute depth/breadth/total counts. Last gate before the
        // node is minted, so a refused spawn costs no token and no id.
        if !self.spawn_bucket.try_take() {
            return Err(SpawnRefused::RateExceeded);
        }
        let path = format!("{parent_path}.{child_index}");
        let id = self.alloc(Some(parent), depth, path);
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.push(id);
        }
        Ok(id)
    }

    fn alloc(&mut self, parent: Option<NodeId>, depth: u32, agent_path: String) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        self.nodes.insert(
            id,
            Node {
                id,
                parent,
                depth,
                agent_path,
                status: NodeStatus::Spawning,
                tokens: 0,
                children: Vec::new(),
            },
        );
        id
    }

    pub fn set_status(&mut self, id: NodeId, status: NodeStatus) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.status = status;
        }
    }

    /// Charge tokens to a node and the tree root. Returns true if the
    /// tree-wide ceiling is now exceeded (caller drains the tree).
    pub fn charge_tokens(&mut self, id: NodeId, tokens: u64) -> bool {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.tokens = n.tokens.saturating_add(tokens);
        }
        self.total_tokens = self.total_tokens.saturating_add(tokens);
        self.total_tokens >= self.caps.tree_token_ceiling
    }

    /// Flip the one-way draining flag (SIGTERM / tree-budget breach). After
    /// this, `mint_*` refuses — a parent can't spawn replacements mid-teardown
    /// (RFC 0003 §kill-ladder).
    pub fn set_draining(&mut self) {
        self.draining = true;
    }

    /// Node ids ordered **deepest-first** — the kill-ladder teardown order so
    /// children die before parents (RFC 0003).
    pub fn deepest_first(&self) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        ids.sort_by(|a, b| {
            let da = self.nodes[a].depth;
            let db = self.nodes[b].depth;
            db.cmp(&da).then(b.cmp(a))
        });
        ids
    }

    pub fn remove(&mut self, id: NodeId) -> Option<Node> {
        let node = self.nodes.remove(&id)?;
        if let Some(p) = node.parent.and_then(|parent| self.nodes.get_mut(&parent)) {
            p.children.retain(|c| *c != id);
        }
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_then_child_depth_and_path() {
        let mut t = Tree::new(Caps::default());
        let root = t.mint_root().unwrap();
        assert_eq!(t.get(root).unwrap().depth, 0);
        assert_eq!(t.get(root).unwrap().agent_path, "0");
        let c0 = t.mint_child(root).unwrap();
        let c1 = t.mint_child(root).unwrap();
        assert_eq!(t.get(c0).unwrap().depth, 1);
        assert_eq!(t.get(c0).unwrap().agent_path, "0.0");
        assert_eq!(t.get(c1).unwrap().agent_path, "0.1");
        assert_eq!(t.get(root).unwrap().children.len(), 2);
    }

    #[test]
    fn depth_cap_refuses() {
        let caps = Caps {
            max_depth: 2,
            ..Caps::default()
        };
        let mut t = Tree::new(caps);
        let root = t.mint_root().unwrap(); // depth 0
        let a = t.mint_child(root).unwrap(); // depth 1
        let b = t.mint_child(a).unwrap(); // depth 2
        assert_eq!(t.mint_child(b).unwrap_err(), SpawnRefused::MaxDepth);
    }

    #[test]
    fn children_cap_refuses() {
        let caps = Caps {
            max_children: 2,
            ..Caps::default()
        };
        let mut t = Tree::new(caps);
        let root = t.mint_root().unwrap();
        t.mint_child(root).unwrap();
        t.mint_child(root).unwrap();
        assert_eq!(t.mint_child(root).unwrap_err(), SpawnRefused::MaxChildren);
    }

    #[test]
    fn total_cap_refuses() {
        let caps = Caps {
            max_total: 2,
            max_children: 10,
            ..Caps::default()
        };
        let mut t = Tree::new(caps);
        let root = t.mint_root().unwrap(); // count 1
        t.mint_child(root).unwrap(); // count 2
        assert_eq!(t.mint_child(root).unwrap_err(), SpawnRefused::MaxTotal);
    }

    #[test]
    fn draining_refuses_new_spawns() {
        let mut t = Tree::new(Caps::default());
        let root = t.mint_root().unwrap();
        t.set_draining();
        assert_eq!(t.mint_child(root).unwrap_err(), SpawnRefused::Draining);
    }

    #[test]
    fn token_accounting_rolls_to_root_and_trips_ceiling() {
        let caps = Caps {
            tree_token_ceiling: 100,
            ..Caps::default()
        };
        let mut t = Tree::new(caps);
        let root = t.mint_root().unwrap();
        let c = t.mint_child(root).unwrap();
        assert!(!t.charge_tokens(c, 60));
        assert_eq!(t.total_tokens(), 60);
        assert!(t.charge_tokens(c, 40)); // hits ceiling
        assert_eq!(t.get(c).unwrap().tokens, 100);
    }

    #[test]
    fn token_bucket_burst_then_refill() {
        use std::time::Duration;
        // 8 burst, 2/s refill (the RFC 0009 §3.6 spawn-rate defaults).
        let mut b = TokenBucket::new(8, 2.0);
        let t0 = Instant::now();
        // The full burst of 8 is spendable without any time passing…
        for i in 0..8 {
            assert!(b.try_take_at(t0), "burst token {i} should be available");
        }
        // …and the 9th in the same instant is refused (empty bucket).
        assert!(
            !b.try_take_at(t0),
            "9th take with no refill must be refused"
        );
        // After 0.4s only 0.8 tokens have refilled (< 1) → still refused.
        assert!(!b.try_take_at(t0 + Duration::from_millis(400)));
        // After 0.5s from t0, 1.0 tokens have accrued → one take succeeds, then
        // the bucket is empty again.
        let t1 = t0 + Duration::from_millis(500);
        assert!(b.try_take_at(t1), "one token refills after the interval");
        assert!(!b.try_take_at(t1), "and only one — the refill is metered");
    }

    #[test]
    fn token_bucket_caps_at_burst() {
        use std::time::Duration;
        // Idle for a long time: accrued tokens never exceed the burst ceiling.
        let mut b = TokenBucket::new(8, 2.0);
        let t0 = Instant::now();
        let far = t0 + Duration::from_secs(3600);
        for _ in 0..8 {
            assert!(b.try_take_at(far));
        }
        assert!(
            !b.try_take_at(far),
            "no more than `burst` tokens ever accrue"
        );
    }

    #[test]
    fn spawn_rate_cap_refuses_after_burst() {
        // A wide breadth + a low burst isolates the rate cap as the binding one:
        // the first 3 children mint, the 4th is refused for rate (not breadth),
        // and once a token refills it is allowed again.
        let caps = Caps {
            max_children: 100,
            max_total: 100,
            spawn_rate_burst: 3,
            spawn_rate_per_sec: 1000.0, // fast refill so the wait below is tiny
            ..Caps::default()
        };
        let mut t = Tree::new(caps);
        let root = t.mint_root().unwrap();
        t.mint_child(root).unwrap();
        t.mint_child(root).unwrap();
        t.mint_child(root).unwrap();
        assert_eq!(
            t.mint_child(root).unwrap_err(),
            SpawnRefused::RateExceeded,
            "the 4th rapid spawn is rate-limited, not breadth-limited"
        );
        // A whole token refills in ~1ms at 1000/s; after a short wait it allows again.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(
            t.mint_child(root).is_ok(),
            "a refilled token re-admits a spawn"
        );
    }

    #[test]
    fn deepest_first_orders_children_before_parents() {
        let mut t = Tree::new(Caps::default());
        let root = t.mint_root().unwrap();
        let a = t.mint_child(root).unwrap();
        let b = t.mint_child(a).unwrap();
        let order = t.deepest_first();
        let pos = |id: NodeId| order.iter().position(|x| *x == id).unwrap();
        assert!(pos(b) < pos(a));
        assert!(pos(a) < pos(root));
    }
}
