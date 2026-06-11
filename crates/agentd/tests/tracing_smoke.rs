//! Tracing + metrics end-to-end smoke tests.
//!
//! Drives the engine directly (not the binary) so the assertions can
//! read the captured log stream and the metrics counters.
//!
//! Serialised: `tracing::subscriber::set_default` installs a thread-
//! local subscriber; the engine's process-wide EXEC_ID static counter
//! is shared across tests. Under parallel libtest scheduling these
//! two facts can conspire so one test observes partial captures
//! from a sibling test's run. A process-wide `Mutex` forces tests
//! in this file to execute sequentially so each owns the tracing
//! dispatch for its engine.run.

use std::sync::{Mutex, MutexGuard};

use agentd::engine::{Engine, HandlerRegistry, RunOptions, TriggerMeta};
use agentd::observability::{CapturingWriter, Metrics};
use agentd::workflow::model::{Edge, Node, NodeKind, StartNode, StartSource, WorkflowDoc};
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};

fn serial_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    // A poisoned lock from a previous panicking test shouldn't
    // cascade; unwrap the poison to keep the next test runnable.
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

fn capturing_subscriber() -> (CapturingWriter, impl tracing::Subscriber + Send + Sync) {
    let writer = CapturingWriter::new();
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(writer.clone())
        .with_target(true);
    let filter = EnvFilter::try_new("debug").unwrap();
    let subscriber = Registry::default().with(filter).with(layer);
    (writer, subscriber)
}

fn linear_workflow() -> WorkflowDoc {
    WorkflowDoc {
        name: "obs".into(),
        start_nodes: vec![StartNode {
            name: "main".into(),
            source: StartSource::Manual,
            entry_node: Some("a".into()),
        }],
        nodes: vec![
            Node {
                id: "a".into(),
                retry: None,
                kind: NodeKind::Merge,
            },
            Node {
                id: "b".into(),
                retry: None,
                kind: NodeKind::Terminate,
            },
        ],
        edges: vec![Edge {
            from: "a".into(),
            to: "b".into(),
            when: None,
            max_iterations: None,
        }],
        ..Default::default()
    }
}

#[test]
fn linear_run_emits_workflow_and_node_spans() {
    let _serial = serial_lock();
    let (writer, subscriber) = capturing_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let engine = Engine::new(HandlerRegistry::with_builtin_controls());
    let outcome = engine
        .run(
            &linear_workflow(),
            "main",
            TriggerMeta::manual(json!({})),
            RunOptions::default(),
        )
        .unwrap();
    assert!(matches!(
        outcome,
        agentd::engine::ExecutionOutcome::Completed { .. }
    ));

    let captured = writer.captured_string();
    assert!(
        captured.contains("workflow.started"),
        "missing workflow.started: {captured}"
    );
    assert!(
        captured.contains("workflow.completed"),
        "missing workflow.completed: {captured}"
    );
    assert!(
        captured.contains("workflow.run"),
        "missing workflow.run span: {captured}"
    );
    assert!(
        captured.contains("node.execute"),
        "missing node.execute span: {captured}"
    );
    assert!(
        captured.contains("\"execution_id\""),
        "execution_id field missing: {captured}"
    );
    assert!(
        captured.contains("agentd::audit"),
        "audit target missing: {captured}"
    );
}

#[test]
fn metrics_counters_advance_across_runs() {
    let _serial = serial_lock();
    let engine = Engine::new(HandlerRegistry::with_builtin_controls());
    let metrics = engine.metrics();

    for _ in 0..3 {
        engine
            .run(
                &linear_workflow(),
                "main",
                TriggerMeta::manual(json!({})),
                RunOptions::default(),
            )
            .unwrap();
    }

    let snap = metrics.snapshot();
    assert_eq!(snap.workflow_starts, 3);
    assert_eq!(snap.workflow_completions, 3);
    assert_eq!(snap.workflow_failures, 0);
    assert_eq!(snap.node_executions, 6); // 2 nodes × 3 runs
    assert_eq!(snap.policy_denials, 0);
}

#[test]
fn failed_workflow_increments_failure_counter() {
    let _serial = serial_lock();
    let fail_wf = WorkflowDoc {
        name: "boom".into(),
        start_nodes: vec![StartNode {
            name: "main".into(),
            source: StartSource::Manual,
            entry_node: Some("f".into()),
        }],
        nodes: vec![Node {
            id: "f".into(),
            retry: None,
            kind: NodeKind::Fail {
                reason: Some("nope".into()),
            },
        }],
        ..Default::default()
    };

    let engine = Engine::new(HandlerRegistry::with_builtin_controls());
    let metrics = engine.metrics();
    engine
        .run(
            &fail_wf,
            "main",
            TriggerMeta::manual(json!({})),
            RunOptions::default(),
        )
        .unwrap();
    let snap = metrics.snapshot();
    assert_eq!(snap.workflow_failures, 1);
    assert_eq!(snap.workflow_completions, 0);
}

#[test]
fn shared_metrics_aggregate_across_engines() {
    let _serial = serial_lock();
    // Useful for `agent serve` where many requests hit the same
    // counter set.
    let shared = Metrics::new();
    let e1 = Engine::with_metrics(HandlerRegistry::with_builtin_controls(), shared.clone());
    let e2 = Engine::with_metrics(HandlerRegistry::with_builtin_controls(), shared.clone());
    let wf = linear_workflow();

    e1.run(
        &wf,
        "main",
        TriggerMeta::manual(json!({})),
        RunOptions::default(),
    )
    .unwrap();
    e2.run(
        &wf,
        "main",
        TriggerMeta::manual(json!({})),
        RunOptions::default(),
    )
    .unwrap();

    assert_eq!(shared.snapshot().workflow_starts, 2);
}
