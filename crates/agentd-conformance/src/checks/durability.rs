// SPDX-License-Identifier: AGPL-3.0-only
//! The crash-durability contract (RFC 0025 / RFC 0026 §4.4): a SIGKILL at a kill
//! point loses no progress. Life 1 dies right after the inbox event is durable
//! (before the run); life 2 restores that event, starts the run, and dies mid-
//! step; life 3 restores the running run and finishes the job. The durable state
//! lives in a mock MCP store that outlives each agentd process.
//!
//! Kill points are armed by `AGENTD_TEST_KILL_AT` (a debug/test hook the runtime
//! honours); the store profile is the built-in mock MCP server.

use crate::checks::util::{mock_llm, write_file};
use crate::{Category, Check, Harness, Outcome};
use serde_json::json;

pub fn checks() -> Vec<Check> {
    vec![Check {
        id: "durability/sigkill-restore-and-finish",
        category: Category::Durability,
        desc: "a SIGKILL before and during a step is recovered; a later life restores and completes",
        run: sigkill_restore_and_finish,
    }]
}

fn chaos_config(llm: &str, store_endpoint: &str) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: chaos\n  instruction: finish the job\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         mcp:\n  servers:\n    - name: store\n      endpoint: {store_endpoint}\n\
         store:\n  kind: mcp\n  mcp:\n    server: store\n\
         observability:\n  log_level: info\n"
    )
}

fn run_count(r: &crate::harness::RunResult, ev: &str) -> usize {
    r.events().iter().filter(|e| e["event"] == ev).count()
}

fn sigkill_restore_and_finish(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let addr_file = tmp.path().join("store.addr");
    let store = h.spawn_mock_mcp(&addr_file, "mock://noop", false);
    let llm = mock_llm(
        h,
        &tmp,
        &json!({"turns": [{"content": "done after restart"}]}),
    );
    let cfg = write_file(
        &tmp,
        "agentd.yaml",
        &chaos_config(&llm.uri, &store.endpoint()),
    );

    // Life 1: die right after the inbox event is durable (before the run).
    let r1 = h.run_env(
        &["--config", &cfg],
        &[("AGENTD_TEST_KILL_AT", "inbox.after_put")],
    );
    if r1.code == Some(0) {
        return Outcome::fail(format!(
            "life 1 should die at the kill point, not exit 0; stderr:\n{}",
            r1.stderr
        ));
    }

    // Life 2: restore the pending event, start the run, die mid-step.
    let r2 = h.run_env(
        &["--config", &cfg],
        &[("AGENTD_TEST_KILL_AT", "step.running")],
    );
    if r2.code == Some(0) {
        return Outcome::fail(format!(
            "life 2 should die at the kill point, not exit 0; stderr:\n{}",
            r2.stderr
        ));
    }
    if run_count(&r2, "restore.done") != 1 {
        return Outcome::fail(format!(
            "life 2 should restore the pending inbox event (restore.done); stderr:\n{}",
            r2.stderr
        ));
    }
    if run_count(&r2, "run.start") != 1 {
        return Outcome::fail(format!(
            "the replayed event should start the run once on life 2; stderr:\n{}",
            r2.stderr
        ));
    }

    // Life 3: restore the running run, replay the step, finish.
    let r3 = h.run(&["--config", &cfg]);
    Outcome::require(
        r3.code == Some(0),
        format!(
            "life 3 should complete and exit 0, got {:?}; stderr:\n{}",
            r3.code, r3.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            run_count(&r3, "restore.done") == 1,
            format!(
                "life 3 should restore the running run; stderr:\n{}",
                r3.stderr
            ),
        )
    })
    .and(|| {
        let done: Vec<_> = r3
            .events()
            .into_iter()
            .filter(|e| e["event"] == "run.done")
            .collect();
        Outcome::require(
            done.len() == 1 && done[0]["status"] == "completed",
            format!("the restored run should complete: {done:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            r3.stdout.contains("done after restart"),
            format!(
                "the job's result should print on the finishing life: {:?}",
                r3.stdout
            ),
        )
    })
}
