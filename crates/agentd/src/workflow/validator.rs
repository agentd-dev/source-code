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
//!
//! `when` selectors on edges are *not* validated against the source
//! node's kind here — that lands in Phase 2 when the engine grows
//! a dispatch for switch / condition outputs.

use std::collections::HashSet;

use crate::workflow::WorkflowDoc;

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
        if let Some(entry) = &start.entry_node {
            if !node_ids.contains(entry) {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{Edge, HttpRoute, Node, NodeKind, StartNode, StartSource, Trigger};

    fn n(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.into(),
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
        }
    }

    fn start(name: &str, source: StartSource, entry: Option<&str>) -> StartNode {
        StartNode {
            name: name.into(),
            source,
            entry_node: entry.map(Into::into),
        }
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
                },
                HttpRoute {
                    method: "post".into(), // case-insensitive match
                    path: "/x".into(),
                    start_node: "main".into(),
                    input_schema: None,
                },
            ],
            ..Default::default()
        };
        let r = validate(&doc);
        assert!(r.codes().contains(&"dup_http_route"));
    }
}
