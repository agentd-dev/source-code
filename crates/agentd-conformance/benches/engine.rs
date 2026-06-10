//! Engine micro-benchmarks — the quantified form of the appliance
//! claim. A single native binary with no runtime, no interpreter, and
//! no network on the hot path should walk a graph in microseconds and
//! cold-start to first node in well under a millisecond.
//!
//! Run with: `cargo bench -p agentd-conformance`
//!
//!   - engine_throughput  — steady-state cost of walking a 20-node
//!     graph on a pre-built engine (pure interpreter throughput).
//!   - cold_start         — build the handler registry + engine and
//!     execute the first node from scratch, every iteration (the
//!     start-up latency a one-shot `--mode once` invocation pays).

use std::time::Duration;

use agentd::engine::{Engine, HandlerRegistry, RunOptions, StubHandler, TriggerMeta};
use agentd::workflow::WorkflowDoc;
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::Value;
use std::hint::black_box;

/// A linear `merge → … → terminate` chain of `n` nodes.
fn linear_workflow(n: usize) -> WorkflowDoc {
    let mut src = String::from(
        "name = \"bench_linear\"\n\
         [[start_nodes]]\nname = \"main\"\nsource = \"manual\"\nentry_node = \"m0\"\n",
    );
    for i in 0..n {
        let ty = if i == n - 1 { "terminate" } else { "merge" };
        src.push_str(&format!("[[nodes]]\nid = \"m{i}\"\ntype = \"{ty}\"\n"));
    }
    for i in 0..n.saturating_sub(1) {
        src.push_str(&format!(
            "[[edges]]\nfrom = \"m{i}\"\nto = \"m{}\"\n",
            i + 1
        ));
    }
    WorkflowDoc::from_toml(&src).expect("valid bench workflow")
}

fn build_engine() -> Engine {
    let mut registry = HandlerRegistry::with_builtin_controls();
    agentd::tools::register_default_tools(
        &mut registry,
        agentd::tools::policy::allow_all(),
        agentd::budget::unbounded(),
    );
    registry.set_fallback(Box::new(StubHandler));
    Engine::new(registry)
}

fn opts() -> RunOptions {
    RunOptions {
        timeout: Duration::from_secs(5),
        dry_run: false,
    }
}

fn bench_throughput(c: &mut Criterion) {
    let engine = build_engine();
    let doc = linear_workflow(20);
    c.bench_function("engine_throughput_20_nodes", |b| {
        b.iter(|| {
            let (outcome, _trace) = engine
                .run_with_trace(
                    black_box(&doc),
                    "main",
                    TriggerMeta::manual(Value::Null),
                    opts(),
                )
                .expect("run");
            black_box(outcome);
        });
    });
}

fn bench_cold_start(c: &mut Criterion) {
    let doc = linear_workflow(1);
    c.bench_function("cold_start_to_first_node", |b| {
        b.iter(|| {
            // The whole start-up path: registry + tool registration +
            // engine, then execute the first node.
            let engine = build_engine();
            let (outcome, _trace) = engine
                .run_with_trace(
                    black_box(&doc),
                    "main",
                    TriggerMeta::manual(Value::Null),
                    opts(),
                )
                .expect("run");
            black_box(outcome);
        });
    });
}

criterion_group!(benches, bench_throughput, bench_cold_start);
criterion_main!(benches);
