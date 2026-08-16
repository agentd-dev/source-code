// SPDX-License-Identifier: AGPL-3.0-only
//! The durable-store contract (RFC 0025): agentd boots against a remote store
//! reached over MCP, runs its `once` job, and persists the outcome — so a second
//! life against the same store finds the run already complete and does not re-fire
//! the `once` start. Driven black-box: the built-in mock MCP server (a store
//! profile: `state.put`/`get`/`list`) stays alive across both agentd lives.

use crate::checks::util::{mock_llm, write_file};
use crate::{Category, Check, Harness, Outcome};
use serde_json::json;

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "store/boots-against-mcp-store",
            category: Category::Store,
            desc: "a job backed by an MCP store connects, runs, and completes",
            run: boots_against_store,
        },
        Check {
            id: "store/persists-completed-run-across-restart",
            category: Category::Store,
            desc: "a restarted instance restores the completed run from the store and does not re-fire the once start",
            run: persists_across_restart,
        },
    ]
}

fn store_config(llm: &str, store_endpoint: &str) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: store-conf\n  instruction: finish the job\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         mcp:\n  servers:\n    - name: store\n      endpoint: {store_endpoint}\n\
         store:\n  kind: mcp\n  mcp:\n    server: store\n\
         observability:\n  log_level: info\n"
    )
}

fn boots_against_store(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let addr_file = tmp.path().join("store.addr");
    let store = h.spawn_mock_mcp(&addr_file, "mock://noop", false);
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "stored and done"}]}));
    let cfg = write_file(
        &tmp,
        "agentd.yaml",
        &store_config(&llm.uri, &store.endpoint()),
    );
    let r = h.run(&["--config", &cfg]);
    Outcome::require(
        r.code == Some(0),
        format!(
            "a job against an MCP store should exit 0, got {:?}; stderr:\n{}",
            r.code, r.stderr
        ),
    )
    .and(|| {
        let done: Vec<_> = r
            .events()
            .into_iter()
            .filter(|e| e["event"] == "run.done")
            .collect();
        Outcome::require(
            done.len() == 1 && done[0]["status"] == "completed",
            format!("the run should complete: {done:?}"),
        )
    })
}

fn persists_across_restart(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let addr_file = tmp.path().join("store.addr");
    // One store, two agentd lives — its state persists across the restart.
    let store = h.spawn_mock_mcp(&addr_file, "mock://noop", false);
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "done once"}]}));
    let cfg = write_file(
        &tmp,
        "agentd.yaml",
        &store_config(&llm.uri, &store.endpoint()),
    );

    // Life 1: the once job runs to completion and its outcome is durable.
    let r1 = h.run(&["--config", &cfg]);
    if r1.code != Some(0) {
        return Outcome::fail(format!(
            "life 1 should exit 0, got {:?}; stderr:\n{}",
            r1.code, r1.stderr
        ));
    }

    // Life 2: nothing left to do — the completed once start is not re-fired.
    let r2 = h.run(&["--config", &cfg]);
    Outcome::require(
        r2.code == Some(0),
        format!("life 2 should exit 0, got {:?}; stderr:\n{}", r2.code, r2.stderr),
    )
    .and(|| {
        let started = r2.events().iter().filter(|e| e["event"] == "run.start").count();
        Outcome::require(
            started == 0,
            format!("the restored (already-complete) once run must not re-fire on life 2 (saw {started} run.start)"),
        )
    })
    .and(|| {
        Outcome::require(
            r2.saw_event("start.once.skipped"),
            format!(
                "life 2 should skip the completed once start; stderr:\n{}",
                r2.stderr
            ),
        )
    })
}
