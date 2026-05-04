//! Build-time validation (Phase 9) — drives the whole `cargo build`
//! pipeline to verify `build.rs` correctly accepts valid embedded
//! configs and rejects invalid ones.
//!
//! These tests spawn `cargo build` as a subprocess. They are marked
//! `#[ignore]` by default so the default `cargo test -p agent` stays
//! fast; run them explicitly with:
//!
//! ```bash
//! cargo test -p agent --test build_time_validation -- --ignored
//! ```
//!
//! Each test uses a unique target dir so the runs don't stomp on
//! each other's incremental build artifacts.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/agentd; root is two levels up.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // root
    p
}

fn run_cargo_build(
    workspace: &Path,
    target_dir: &Path,
    embed_config: Option<&Path>,
) -> std::process::Output {
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("-p")
        .arg("agentd")
        .arg("--bin")
        .arg("agentd")
        .arg("--target-dir")
        .arg(target_dir)
        .current_dir(workspace);
    if let Some(p) = embed_config {
        cmd.env("AGENTD_EMBED_CONFIG", p);
    } else {
        cmd.env_remove("AGENTD_EMBED_CONFIG");
    }
    cmd.output().expect("spawn cargo build")
}

#[test]
#[ignore]
fn build_accepts_valid_embedded_workflow() {
    let ws = workspace_root();
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("wf.toml");
    std::fs::write(
        &cfg,
        r#"
name = "embedded"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "a"

[[nodes]]
id = "a"
type = "merge"

[[nodes]]
id = "b"
type = "terminate"

[[edges]]
from = "a"
to = "b"
"#,
    )
    .unwrap();
    let target = tmp.path().join("target");

    let out = run_cargo_build(&ws, &target, Some(&cfg));
    assert!(
        out.status.success(),
        "expected build to succeed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
#[ignore]
fn build_rejects_duplicate_node_ids() {
    let ws = workspace_root();
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("dup.toml");
    std::fs::write(
        &cfg,
        r#"
name = "dup"

[[nodes]]
id = "a"
type = "merge"

[[nodes]]
id = "a"
type = "terminate"
"#,
    )
    .unwrap();
    let target = tmp.path().join("target");

    let out = run_cargo_build(&ws, &target, Some(&cfg));
    assert!(!out.status.success(), "expected build to fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("duplicate node id"), "stderr:\n{stderr}");
}

#[test]
#[ignore]
fn build_rejects_dangling_edge() {
    let ws = workspace_root();
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("dangle.toml");
    std::fs::write(
        &cfg,
        r#"
name = "dangle"

[[nodes]]
id = "a"
type = "merge"

[[edges]]
from = "a"
to = "nowhere"
"#,
    )
    .unwrap();
    let target = tmp.path().join("target");

    let out = run_cargo_build(&ws, &target, Some(&cfg));
    assert!(!out.status.success(), "expected build to fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("is not a declared node id"),
        "stderr:\n{stderr}"
    );
}

#[test]
#[ignore]
fn build_rejects_trigger_with_unknown_start_node() {
    let ws = workspace_root();
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("trig.toml");
    std::fs::write(
        &cfg,
        r#"
name = "trig"

[[start_nodes]]
name = "main"
source = "event"
entry_node = "a"

[[triggers]]
type = "internal.event"
name = "go"
start_node = "does_not_exist"

[[nodes]]
id = "a"
type = "merge"
"#,
    )
    .unwrap();
    let target = tmp.path().join("target");

    let out = run_cargo_build(&ws, &target, Some(&cfg));
    assert!(!out.status.success(), "expected build to fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown start_node"), "stderr:\n{stderr}");
}

#[test]
#[ignore]
fn embedded_build_serves_workflow_via_cli() {
    // Full round trip: build with AGENTD_EMBED_CONFIG, then run the
    // resulting binary with `--embedded --start main` and assert it
    // completes without a file path argument.
    let ws = workspace_root();
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("wf.toml");
    std::fs::write(
        &cfg,
        r#"
name = "baked"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "done"

[[nodes]]
id = "done"
type = "terminate"
"#,
    )
    .unwrap();
    let target = tmp.path().join("target");

    let out = run_cargo_build(&ws, &target, Some(&cfg));
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let binary = target.join("debug").join("agentd");
    assert!(binary.exists(), "binary not found at {}", binary.display());

    let run = Command::new(&binary)
        // Embedded config is picked up automatically when no
        // `--config` is passed and the build baked one in.
        .args(["--start", "main"])
        .output()
        .expect("spawn embedded agent");
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&run.stdout).expect("json body");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["last_node"], "done");
}
