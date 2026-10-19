// SPDX-License-Identifier: AGPL-3.0-only
//! A child is started without the environment variables a supervisor-only
//! secret is read from (RFC 0044 §5.13 item 1, T4): the companies preview's
//! seal key, named `{{secret:NAME}}`, is the supervisor's alone, so the turn
//! worker a workflow's agent step runs in is spawned with the parent
//! environment less that name — and nothing else less. What crossed to the
//! child is read from the frames the supervisor wrote
//! (`AGENTD_TEST_FRAMES_FILE`).
#![cfg(all(unix, feature = "companies"))]

mod common;

use std::process::{Command, Stdio};

use serde_json::Value;

#[test]
fn a_worker_is_spawned_without_the_supervisor_only_variables() {
    let dir = common::unique_path("supervisor-env", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    let frames = format!("{dir}/frames.jsonl");
    std::fs::write(
        &cfg,
        "agent: { name: env, preflight: never }\n\
         intelligence: { endpoints: \"mock:final\", model: mock }\n\
         store: { kind: memory }\n\
         security: { companies: { seal_key: \"{{secret:SUPERVISOR_ENV_SEAL}}\" } }\n\
         lifecycle: { run_until: idle, idle_grace: 500ms }\n\
         observability: { log_level: info }\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     t: { kind: agent, depends_on: [s], instruction: \"say done\" }\n\
        \x20     f: { kind: finish, depends_on: [t], status: completed }\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("SUPERVISOR_ENV_SEAL", "ab".repeat(32))
        .env("SUPERVISOR_ENV_KEEP", "kept")
        .env("AGENTD_TEST_FRAMES_FILE", &frames)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{log}");
    let spawned: Vec<Value> = std::fs::read_to_string(&frames)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|f| f["dir"] == "spawn")
        .collect();
    assert!(!spawned.is_empty(), "no child was spawned:\n{log}");
    for f in &spawned {
        let env = &f["frame"]["env"];
        assert!(
            env.get("SUPERVISOR_ENV_SEAL").is_none(),
            "a child was handed the seal key's variable: {env}"
        );
        assert_eq!(env["SUPERVISOR_ENV_KEEP"], "kept", "{env}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
