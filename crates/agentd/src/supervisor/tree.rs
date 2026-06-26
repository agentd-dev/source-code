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
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            max_depth: 4,
            max_children: 8,
            max_total: 64,
            tree_token_ceiling: 2_000_000,
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
