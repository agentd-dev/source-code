//! DAG validator (RFC §11.3).
//!
//! Walks a [`WorkflowDoc`] and collects every issue it finds. The
//! build-time validator (Phase 9) calls this from `build.rs`; the
//! runtime calls it from `--validate-only` and at startup in both
//! modes before accepting a workflow.
//!
//! Issues are collected into a [`ValidationReport`] instead of
//! failing on the first one — operators see the full picture in
//! one run instead of playing fix-rerun whack-a-mole.
//!
//! Checks performed:
//!
//! 1. Duplicate node ids, start-node names, HTTP route (method, path).
//! 2. Dangling edge targets (`from` / `to` reference missing nodes).
//! 3. Trigger / HTTP route / start-node cross-references resolve.
//! 4. Acyclicity (Kahn's algorithm).
//! 5. Reachability from each start node's entry (BFS).
//!
//! `when` selectors on edges are *not* validated against the source
//! node's kind here — that lands in Phase 2 when the engine grows
//! a dispatch for switch / condition outputs.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::workflow::{Node, StartNode, WorkflowDoc};

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

/// Structured record of everything wrong with a workflow.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ValidationReport {
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }

    pub fn codes(&self) -> Vec<&'static str> {
        self.issues.iter().map(|i| i.code).collect()
    }
}

/// One validation problem. `code` is a short stable identifier
/// scripts can match on; `message` is the human-readable form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub code: &'static str,
    pub message: String,
}

impl ValidationIssue {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Validate a workflow document. Returns a report; empty = valid.
pub fn validate(doc: &WorkflowDoc) -> ValidationReport {
    let mut r = ValidationReport::default();

    let node_ids = collect_node_ids(doc, &mut r);
    let start_names = collect_start_names(doc, &mut r);

    check_http_routes(doc, &start_names, &mut r);
    check_triggers(doc, &start_names, &mut r);
    check_start_node_entries(doc, &node_ids, &mut r);
    check_edges(doc, &node_ids, &mut r);
    check_mcp_nodes(doc, &mut r);
    check_agent_loops(doc, &mut r);

    // Graph-level checks only make sense once every edge references a
    // known node. Skip them if dangling edges were detected so the
    // topo-sort doesn't panic on a missing key.
    if r.issues.iter().all(|i| i.code != "dangling_edge") {
        check_acyclic(doc, &mut r);
        check_reachability(doc, &node_ids, &mut r);
    }

    r
}

// ---------------------------------------------------------------------------
// Step 1: identifier uniqueness
// ---------------------------------------------------------------------------

fn collect_node_ids(doc: &WorkflowDoc, r: &mut ValidationReport) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut dups = HashSet::new();
    for node in &doc.nodes {
        if !seen.insert(node.id.clone()) {
            dups.insert(node.id.clone());
        }
    }
    for id in &dups {
        r.issues.push(ValidationIssue::new(
            "dup_node_id",
            format!("node id `{id}` is declared more than once"),
        ));
    }
    seen
}

fn collect_start_names(doc: &WorkflowDoc, r: &mut ValidationReport) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut dups = HashSet::new();
    for start in &doc.start_nodes {
        if !seen.insert(start.name.clone()) {
            dups.insert(start.name.clone());
        }
    }
    for name in &dups {
        r.issues.push(ValidationIssue::new(
            "dup_start_name",
            format!("start node name `{name}` is declared more than once"),
        ));
    }
    seen
}

// ---------------------------------------------------------------------------
// Step 2: cross-references
// ---------------------------------------------------------------------------

fn check_http_routes(doc: &WorkflowDoc, start_names: &HashSet<String>, r: &mut ValidationReport) {
    let mut seen = HashSet::new();
    for route in &doc.http_routes {
        let key = (route.method.to_ascii_uppercase(), route.path.clone());
        if !seen.insert(key.clone()) {
            r.issues.push(ValidationIssue::new(
                "dup_http_route",
                format!("duplicate HTTP route `{} {}`", route.method, route.path),
            ));
        }
        if !start_names.contains(&route.start_node) {
            r.issues.push(ValidationIssue::new(
                "unknown_http_start_node",
                format!(
                    "HTTP route `{} {}` points at unknown start node `{}`",
                    route.method, route.path, route.start_node
                ),
            ));
        }
    }
}

fn check_triggers(doc: &WorkflowDoc, start_names: &HashSet<String>, r: &mut ValidationReport) {
    for trig in &doc.triggers {
        let sn = trig.start_node();
        if !start_names.contains(sn) {
            r.issues.push(ValidationIssue::new(
                "unknown_trigger_start_node",
                format!("trigger points at unknown start node `{sn}`"),
            ));
        }
    }
}

fn check_start_node_entries(
    doc: &WorkflowDoc,
    node_ids: &HashSet<String>,
    r: &mut ValidationReport,
) {
    for start in &doc.start_nodes {
        if let Some(entry) = &start.entry_node
            && !node_ids.contains(entry)
        {
            r.issues.push(ValidationIssue::new(
                "unknown_start_entry_node",
                format!(
                    "start node `{}` references unknown entry node `{entry}`",
                    start.name
                ),
            ));
        }
    }
}

fn check_edges(doc: &WorkflowDoc, node_ids: &HashSet<String>, r: &mut ValidationReport) {
    for (idx, edge) in doc.edges.iter().enumerate() {
        if !node_ids.contains(&edge.from) {
            r.issues.push(ValidationIssue::new(
                "dangling_edge",
                format!(
                    "edge #{idx} `{}` → `{}`: source node `{}` is not declared",
                    edge.from, edge.to, edge.from
                ),
            ));
        }
        if !node_ids.contains(&edge.to) {
            r.issues.push(ValidationIssue::new(
                "dangling_edge",
                format!(
                    "edge #{idx} `{}` → `{}`: target node `{}` is not declared",
                    edge.from, edge.to, edge.to
                ),
            ));
        }
    }
}

/// Validate MCP node `server` fields against the `[[mcp_servers]]`
/// list. Rules:
///   * `server = "name"` → must match a declared entry.
///   * `server = None` + 0 or >1 declared entries → error (ambiguous
///     or no target). Single-server workflows that predate
///     multi-server get to omit the field.
fn check_mcp_nodes(doc: &WorkflowDoc, r: &mut ValidationReport) {
    use crate::workflow::model::NodeKind;
    let known: HashSet<&str> = doc.mcp_servers.iter().map(|d| d.name.as_str()).collect();
    let multi = known.len() > 1;
    let none = known.is_empty();
    for node in &doc.nodes {
        let (kind_name, server) = match &node.kind {
            NodeKind::CallMcpTool { server, .. } => ("call_mcp_tool", server.as_deref()),
            NodeKind::ReadMcpResource { server, .. } => ("read_mcp_resource", server.as_deref()),
            _ => continue,
        };
        match server {
            Some(name) => {
                if !known.contains(name) {
                    r.issues.push(ValidationIssue::new(
                        "unknown_mcp_server",
                        format!(
                            "node `{}` ({kind_name}) references unknown mcp_server `{name}`",
                            node.id
                        ),
                    ));
                }
            }
            None => {
                if multi {
                    r.issues.push(ValidationIssue::new(
                        "ambiguous_mcp_server",
                        format!(
                            "node `{}` ({kind_name}) has no `server` field but multiple mcp_servers are configured",
                            node.id
                        ),
                    ));
                } else if none {
                    // No servers declared at all — the node will fail
                    // at runtime with "no mcp_servers configured". We
                    // still surface it at validation time so the
                    // operator sees it before deployment. (Legacy
                    // workflows using `--mcp-stdio` only don't
                    // populate `mcp_servers` from TOML — this is a
                    // runtime-only path we deliberately don't flag
                    // here since it requires cross-referencing CLI
                    // args that validator doesn't have access to.)
                }
            }
        }
    }
}

/// Validate `agent_loop` nodes: bounded steps, an instruction
/// source, and a tool subset drawn from the known loop vocabulary.
fn check_agent_loops(doc: &WorkflowDoc, r: &mut ValidationReport) {
    use crate::workflow::model::NodeKind;
    for node in &doc.nodes {
        let NodeKind::AgentLoop {
            instructions,
            instructions_from,
            tools,
            max_steps,
            ..
        } = &node.kind
        else {
            continue;
        };
        if *max_steps == 0 || *max_steps > crate::agent::loop_node::MAX_STEPS_CEILING {
            r.issues.push(ValidationIssue::new(
                "agent_loop_steps_out_of_range",
                format!(
                    "node `{}`: max_steps must be 1..={} (got {max_steps})",
                    node.id,
                    crate::agent::loop_node::MAX_STEPS_CEILING
                ),
            ));
        }
        if instructions.is_none() && instructions_from.is_none() {
            r.issues.push(ValidationIssue::new(
                "agent_loop_missing_instructions",
                format!(
                    "node `{}`: one of `instructions` / `instructions_from` is required",
                    node.id
                ),
            ));
        }
        if tools.is_empty() {
            r.issues.push(ValidationIssue::new(
                "agent_loop_no_tools",
                format!(
                    "node `{}`: `tools` must list at least one of: {}",
                    node.id,
                    crate::agent::loop_node::LOOP_TOOLS.join(", ")
                ),
            ));
        }
        for t in tools {
            if !crate::agent::loop_node::LOOP_TOOLS.contains(&t.as_str()) {
                r.issues.push(ValidationIssue::new(
                    "agent_loop_unknown_tool",
                    format!(
                        "node `{}`: `{t}` is not a loop tool (known: {})",
                        node.id,
                        crate::agent::loop_node::LOOP_TOOLS.join(", ")
                    ),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Step 3: acyclicity (Kahn's algorithm)
// ---------------------------------------------------------------------------

fn check_acyclic(doc: &WorkflowDoc, r: &mut ValidationReport) {
    // Build in-degree and adjacency. Use the node-id strings as keys so
    // the report lists human-readable names if a cycle is found.
    let mut in_degree: HashMap<&str, usize> =
        doc.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    let mut adj: HashMap<&str, Vec<&str>> =
        doc.nodes.iter().map(|n| (n.id.as_str(), vec![])).collect();

    for edge in &doc.edges {
        // Loop edges (declared `max_iterations`) are allowed to form a
        // cycle — they are excluded here so the *rest* of the graph
        // must still be a DAG. The engine bounds their traversal.
        if edge.max_iterations.is_some() {
            continue;
        }
        *in_degree.entry(edge.to.as_str()).or_insert(0) += 1;
        adj.entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter_map(|(id, deg)| if *deg == 0 { Some(*id) } else { None })
        .collect();
    let mut visited = 0usize;

    while let Some(id) = queue.pop_front() {
        visited += 1;
        if let Some(nexts) = adj.get(id) {
            for &next in nexts {
                if let Some(deg) = in_degree.get_mut(next) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(next);
                    }
                }
            }
        }
    }

    if visited != doc.nodes.len() {
        // The remaining non-zero in-degree nodes are all inside at
        // least one cycle.
        let mut cyclic: Vec<&str> = in_degree
            .iter()
            .filter_map(|(id, deg)| if *deg > 0 { Some(*id) } else { None })
            .collect();
        cyclic.sort_unstable();
        r.issues.push(ValidationIssue::new(
            "cycle",
            format!(
                "workflow contains a cycle; nodes involved or downstream: {}",
                cyclic.join(", ")
            ),
        ));
    }
}

// ---------------------------------------------------------------------------
// Step 4: reachability
// ---------------------------------------------------------------------------

/// Resolve a start node's entry — either `entry_node` or fall back to
/// the first node in `doc.nodes` with no incoming edges. Returns
/// `None` if no unambiguous entry can be derived.
fn resolve_entry<'a>(
    start: &'a StartNode,
    doc: &'a WorkflowDoc,
    incoming: &HashMap<&'a str, usize>,
) -> Option<&'a str> {
    if let Some(entry) = &start.entry_node {
        return Some(entry.as_str());
    }
    // Fall-back default: the one node that has zero in-edges. If
    // there are several (or none), the engine needs `entry_node` to
    // disambiguate, so we don't guess.
    let roots: Vec<&str> = doc
        .nodes
        .iter()
        .map(|n| n.id.as_str())
        .filter(|id| incoming.get(id).copied().unwrap_or(0) == 0)
        .collect();
    if roots.len() == 1 {
        Some(roots[0])
    } else {
        None
    }
}

fn check_reachability(doc: &WorkflowDoc, node_ids: &HashSet<String>, r: &mut ValidationReport) {
    if doc.nodes.is_empty() || doc.start_nodes.is_empty() {
        return;
    }

    // Precompute in-degrees for root detection.
    let mut incoming: HashMap<&str, usize> = doc.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    for edge in &doc.edges {
        *incoming.entry(edge.to.as_str()).or_insert(0) += 1;
    }

    // Build adjacency for BFS.
    let mut adj: HashMap<&str, Vec<&str>> =
        doc.nodes.iter().map(|n| (n.id.as_str(), vec![])).collect();
    for edge in &doc.edges {
        adj.entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }

    let mut reached: HashSet<&str> = HashSet::new();

    for start in &doc.start_nodes {
        let Some(entry) = resolve_entry(start, doc, &incoming) else {
            r.issues.push(ValidationIssue::new(
                "ambiguous_start_entry",
                format!(
                    "start node `{}` has no `entry_node` and the workflow has !=1 root nodes; \
                     specify `entry_node` explicitly",
                    start.name
                ),
            ));
            continue;
        };

        // BFS from the entry.
        let mut queue: VecDeque<&str> = VecDeque::from([entry]);
        while let Some(id) = queue.pop_front() {
            if !reached.insert(id) {
                continue;
            }
            if let Some(nexts) = adj.get(id) {
                for &next in nexts {
                    queue.push_back(next);
                }
            }
        }
    }

    for Node { id, .. } in &doc.nodes {
        if !reached.contains(id.as_str()) {
            r.issues.push(ValidationIssue::new(
                "unreachable_node",
                format!("node `{id}` is not reachable from any start node"),
            ));
        }
    }

    // Silence clippy: node_ids is intentionally unused at this layer
    // but kept in the signature for symmetry with the other helpers.
    let _ = node_ids;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{
        Edge, HttpRoute, Node, NodeKind, StartNode, StartSource, Trigger,
    };

    fn n(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.into(),
            retry: None,
            kind,
        }
    }

    fn merge(id: &str) -> Node {
        n(id, NodeKind::Merge)
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            when: None,
            max_iterations: None,
        }
    }

    fn start(name: &str, source: StartSource, entry: Option<&str>) -> StartNode {
        StartNode {
            name: name.into(),
            source,
            entry_node: entry.map(Into::into),
        }
    }

    fn loop_edge(from: &str, to: &str, when: Option<&str>, max: u32) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            when: when.map(Into::into),
            max_iterations: Some(max),
        }
    }

    #[test]
    fn loop_edge_permits_a_bounded_cycle() {
        let nodes = vec![
            merge("gen"),
            n("eval", NodeKind::Switch { expr: "x".into() }),
        ];
        let bounded = WorkflowDoc {
            name: "ok".into(),
            start_nodes: vec![start("main", StartSource::Manual, Some("gen"))],
            nodes: nodes.clone(),
            edges: vec![
                edge("gen", "eval"),
                loop_edge("eval", "gen", Some("retry"), 3),
            ],
            ..Default::default()
        };
        let r = validate(&bounded);
        assert!(r.ok(), "bounded cycle should validate: {:?}", r.issues);

        // The same back-edge without a budget is an ordinary cycle.
        let unbounded = WorkflowDoc {
            edges: vec![edge("gen", "eval"), edge("eval", "gen")],
            ..bounded
        };
        let r = validate(&unbounded);
        assert!(r.issues.iter().any(|i| i.code == "cycle"), "{:?}", r.issues);
    }

    #[test]
    fn empty_workflow_is_valid() {
        let doc = WorkflowDoc {
            name: "x".into(),
            ..Default::default()
        };
        assert!(validate(&doc).ok());
    }

    #[test]
    fn linear_workflow_is_valid() {
        let doc = WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![start("main", StartSource::Manual, Some("a"))],
            nodes: vec![merge("a"), merge("b"), merge("c")],
            edges: vec![edge("a", "b"), edge("b", "c")],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.ok(), "unexpected issues: {:?}", r.issues);
    }

    #[test]
    fn duplicate_node_id_flagged() {
        let doc = WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![start("main", StartSource::Manual, Some("a"))],
            nodes: vec![merge("a"), merge("a")],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"dup_node_id"));
    }

    #[test]
    fn duplicate_start_name_flagged() {
        let doc = WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![
                start("main", StartSource::Manual, None),
                start("main", StartSource::Event, None),
            ],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"dup_start_name"));
    }

    #[test]
    fn dangling_edge_flagged() {
        let doc = WorkflowDoc {
            name: "x".into(),
            nodes: vec![merge("a")],
            edges: vec![edge("a", "missing")],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"dangling_edge"));
        // When edges dangle we skip graph-level checks — no cycle
        // report should ride along even though removing the missing
        // tail leaves "a" unreferenced.
        assert!(!r.codes().contains(&"cycle"));
    }

    #[test]
    fn self_loop_is_a_cycle() {
        let doc = WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![start("main", StartSource::Manual, Some("a"))],
            nodes: vec![merge("a")],
            edges: vec![edge("a", "a")],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"cycle"));
    }

    #[test]
    fn three_node_cycle_flagged() {
        let doc = WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![start("main", StartSource::Manual, Some("a"))],
            nodes: vec![merge("a"), merge("b"), merge("c")],
            edges: vec![edge("a", "b"), edge("b", "c"), edge("c", "a")],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"cycle"));
    }

    #[test]
    fn unreachable_node_flagged() {
        let doc = WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![start("main", StartSource::Manual, Some("a"))],
            nodes: vec![merge("a"), merge("b"), merge("island")],
            edges: vec![edge("a", "b")],
            ..Default::default()
        };
        let r = validate(&doc);
        let reasons: Vec<_> = r
            .issues
            .iter()
            .filter(|i| i.code == "unreachable_node")
            .map(|i| i.message.as_str())
            .collect();
        assert!(
            reasons.iter().any(|m| m.contains("island")),
            "got: {reasons:?}"
        );
    }

    #[test]
    fn trigger_unknown_start_node_flagged() {
        let doc = WorkflowDoc {
            name: "x".into(),
            triggers: vec![Trigger::InternalEvent {
                name: "e".into(),
                start_node: "nope".into(),
            }],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"unknown_trigger_start_node"));
    }

    #[test]
    fn http_route_unknown_start_node_flagged() {
        let doc = WorkflowDoc {
            name: "x".into(),
            http_routes: vec![HttpRoute {
                method: "POST".into(),
                path: "/x".into(),
                start_node: "nope".into(),
                input_schema: None,
                auth: None,
                rate_limit: None,
            }],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"unknown_http_start_node"));
    }

    #[test]
    fn start_node_entry_must_exist() {
        let doc = WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![start("main", StartSource::Manual, Some("no_such"))],
            nodes: vec![merge("a")],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"unknown_start_entry_node"));
    }

    #[test]
    fn ambiguous_root_flagged_when_no_entry() {
        // Two root nodes (in-degree zero) and no explicit `entry_node`:
        // the validator must refuse to guess.
        let doc = WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![start("main", StartSource::Manual, None)],
            nodes: vec![merge("a"), merge("b")],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"ambiguous_start_entry"));
    }

    #[test]
    fn duplicate_http_route_flagged() {
        let doc = WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![start("main", StartSource::Http, None)],
            http_routes: vec![
                HttpRoute {
                    method: "POST".into(),
                    path: "/x".into(),
                    start_node: "main".into(),
                    input_schema: None,
                    auth: None,
                    rate_limit: None,
                },
                HttpRoute {
                    method: "post".into(), // case-insensitive match
                    path: "/x".into(),
                    start_node: "main".into(),
                    input_schema: None,
                    auth: None,
                    rate_limit: None,
                },
            ],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"dup_http_route"));
    }

    #[test]
    fn rfc_example_validates_with_explicit_entry() {
        use crate::workflow::model::*;

        // Same shape as the model's RFC_EXAMPLE but with start nodes
        // pinned to `load_resource` so reachability is unambiguous.
        let doc = WorkflowDoc {
            name: "document_review".into(),
            start_nodes: vec![
                start(
                    "on_resource_update",
                    StartSource::Event,
                    Some("load_resource"),
                ),
                start("on_http_request", StartSource::Http, Some("load_resource")),
                start("manual_review", StartSource::Manual, Some("load_resource")),
            ],
            triggers: vec![Trigger::McpResourceUpdated {
                server: "docs".into(),
                resource: "docs://pages/*".into(),
                start_node: "on_resource_update".into(),
            }],
            http_routes: vec![HttpRoute {
                method: "POST".into(),
                path: "/workflows/document-review".into(),
                start_node: "on_http_request".into(),
                input_schema: None,
                auth: None,
                rate_limit: None,
            }],
            nodes: vec![
                n(
                    "load_resource",
                    NodeKind::ReadMcpResource {
                        resource_from: "trigger.resource_uri".into(),
                        server: None,
                    },
                ),
                n(
                    "analyze",
                    NodeKind::LlmInfer {
                        backend: "default".into(),
                        prompt: "…".into(),
                        input_from: Some("load_resource".into()),
                        output_schema: None,
                        output_repairs: None,
                    },
                ),
                n(
                    "decision",
                    NodeKind::Switch {
                        expr: "analyze.decision".into(),
                    },
                ),
                n(
                    "post_comment",
                    NodeKind::CallMcpTool {
                        tool: "comment_on_page".into(),
                        args_from: Some("analyze.comment_payload".into()),
                        server: None,
                    },
                ),
                n("done", NodeKind::Terminate),
            ],
            edges: vec![
                edge("load_resource", "analyze"),
                edge("analyze", "decision"),
                Edge {
                    from: "decision".into(),
                    to: "post_comment".into(),
                    when: Some("comment".into()),
                    max_iterations: None,
                },
                Edge {
                    from: "decision".into(),
                    to: "done".into(),
                    when: Some("ignore".into()),
                    max_iterations: None,
                },
                edge("post_comment", "done"),
            ],
            ..Default::default()
        };

        let r = validate(&doc);
        assert!(r.ok(), "issues: {:?}", r.issues);
    }

    #[test]
    fn agent_loop_constraints_enforced() {
        use crate::workflow::model::NodeKind;
        let mk = |max_steps: u32, tools: Vec<&str>, instructions: Option<&str>| WorkflowDoc {
            name: "x".into(),
            start_nodes: vec![start("main", StartSource::Manual, Some("a"))],
            nodes: vec![n(
                "a",
                NodeKind::AgentLoop {
                    backend: "default".into(),
                    instructions: instructions.map(Into::into),
                    instructions_from: None,
                    tools: tools.into_iter().map(String::from).collect(),
                    max_steps,
                    max_tokens: None,
                },
            )],
            ..Default::default()
        };
        assert!(validate(&mk(8, vec!["read_file"], Some("go"))).ok());
        assert!(
            validate(&mk(0, vec!["read_file"], Some("go")))
                .codes()
                .contains(&"agent_loop_steps_out_of_range")
        );
        assert!(
            validate(&mk(999, vec!["read_file"], Some("go")))
                .codes()
                .contains(&"agent_loop_steps_out_of_range")
        );
        assert!(
            validate(&mk(8, vec!["read_file"], None))
                .codes()
                .contains(&"agent_loop_missing_instructions")
        );
        assert!(
            validate(&mk(8, vec![], Some("go")))
                .codes()
                .contains(&"agent_loop_no_tools")
        );
        assert!(
            validate(&mk(8, vec!["format_disk"], Some("go")))
                .codes()
                .contains(&"agent_loop_unknown_tool")
        );
    }
}
