// SPDX-License-Identifier: AGPL-3.0-only
//! Every complete configuration sample in the documentation loads.
//!
//! A reader copies a sample and runs it; one that does not load teaches the
//! wrong key and costs the reader the debugging. So every ```yaml block in
//! `docs/` whose top-level keys are agentd's own is validated by the binary
//! (`--validate-config`), the way `examples/` is. A block that is part of a
//! configuration says so on its first line (`# fragment: under <path>`) and
//! is skipped; so is YAML that is not an agentd configuration at all (a
//! Kubernetes manifest, a workflow file). Each `{{secret:NAME}}` and `${VAR}`
//! the block references is given a placeholder value, and a sample that reads
//! files the page has the reader create names them on a first-line comment —
//! `# needs: ./agent.md, workflows/nightly.yaml` — and gets a placeholder of
//! each.
//!
//! The pages describe the release binary, so the samples are checked against
//! a build with the release feature set.
#![cfg(all(
    unix,
    feature = "a2a",
    feature = "metrics",
    feature = "cron",
    feature = "otel",
    feature = "hot-reload",
    feature = "config-watch",
    feature = "aauth",
    feature = "oauth",
    feature = "cel",
    feature = "sign",
    feature = "oci",
    feature = "decrypt",
    feature = "mcp-events",
    feature = "mock-models"
))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The top-level keys of agentd's configuration, from its own schema.
fn config_keys() -> BTreeSet<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .arg("--config-schema")
        .output()
        .expect("schema");
    let schema: serde_json::Value = serde_json::from_slice(&out.stdout).expect("schema json");
    schema["properties"]
        .as_object()
        .expect("properties")
        .keys()
        .cloned()
        .collect()
}

/// (file, first line of the block, the block) for every ```yaml block.
fn yaml_blocks(dir: &Path) -> Vec<(PathBuf, usize, String)> {
    let mut out = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        let mut block: Option<(usize, String)> = None;
        for (n, line) in text.lines().enumerate() {
            let fence = line.trim_start();
            match &mut block {
                None if fence.starts_with("```yaml") || fence.starts_with("```yml") => {
                    block = Some((n + 2, String::new()));
                }
                Some(_) if fence.starts_with("```") => {
                    let (at, body) = block.take().unwrap();
                    out.push((f.clone(), at, body));
                }
                Some((_, body)) => {
                    body.push_str(line);
                    body.push('\n');
                }
                None => {}
            }
        }
    }
    out
}

/// Every `{{secret:NAME}}` and `${VAR}` a block references.
fn referenced_vars(body: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (open, close) in [("{{secret:", "}}"), ("${", "}")] {
        let mut rest = body;
        while let Some(i) = rest.find(open) {
            rest = &rest[i + open.len()..];
            if let Some(j) = rest.find(close) {
                let name = rest[..j].split(":-").next().unwrap_or("").trim();
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

#[test]
fn every_complete_configuration_sample_in_the_docs_loads() {
    let keys = config_keys();
    let scratch = std::env::temp_dir().join(format!("agentd-docs-samples-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let mut checked = 0;
    let mut failures = Vec::new();
    for (file, line, body) in yaml_blocks(&workspace().join("docs")) {
        let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        if first.trim_start().starts_with("# fragment") {
            continue;
        }
        let Ok(doc) = serde_yaml_ng_value(&body) else {
            // A multi-document manifest (Kubernetes) is not a configuration.
            if !(body.contains("\n---") || body.trim_start().starts_with("apiVersion:")) {
                failures.push(format!("{}:{line}: not valid YAML", file.display()));
            }
            continue;
        };
        let Some(top) = doc.as_object() else { continue };
        if top.is_empty() || !top.keys().all(|k| keys.contains(k)) {
            continue; // not an agentd configuration
        }
        let dir = scratch.join(format!("sample-{checked}"));
        std::fs::create_dir_all(&dir).unwrap();
        for need in needs(&body) {
            placeholder(&dir, &need);
        }
        let cfg = dir.join("agentd.yaml");
        std::fs::write(&cfg, &body).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
        cmd.args(["--validate-config", "--config"])
            .arg(&cfg)
            .current_dir(&dir)
            .stdin(Stdio::null());
        for name in referenced_vars(&body) {
            cmd.env(name, "placeholder-value");
        }
        let out = cmd.output().expect("validate");
        checked += 1;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let why = err
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(4)
                .collect::<Vec<_>>()
                .join(" | ");
            failures.push(format!("{}:{line}: {why}", file.display()));
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(checked > 0, "no configuration sample was found to check");
    assert!(
        failures.is_empty(),
        "{} of {checked} configuration samples in docs/ do not load:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The files a sample's `# needs:` comment names.
fn needs(body: &str) -> Vec<String> {
    body.lines()
        .take_while(|l| l.trim_start().starts_with('#'))
        .filter_map(|l| l.trim_start().strip_prefix("# needs:"))
        .flat_map(|l| l.split(','))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// A stand-in for a file a page has the reader create: an instruction, a
/// workflow file, or a folder of instructions.
fn placeholder(dir: &Path, need: &str) {
    let path = dir.join(need.trim_start_matches("./"));
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if need.ends_with('/') || ext.is_empty() {
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("agent.md"), "Placeholder instruction.\n").unwrap();
        return;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("w");
    let content = match ext {
        "yaml" | "yml" => format!(
            "name: {stem}\nsteps:\n  s: {{kind: manual}}\n  f: {{kind: finish, depends_on: [s]}}\n"
        ),
        _ => "Placeholder instruction.\n".to_string(),
    };
    std::fs::write(path, content).unwrap();
}

/// YAML → JSON value through agentd's own YAML reader, so a sample is read
/// the way the binary reads it.
fn serde_yaml_ng_value(body: &str) -> Result<serde_json::Value, String> {
    agentd::config::yaml::parse(body).map_err(|e| e.to_string())
}
