// SPDX-License-Identifier: AGPL-3.0-only
//! **The shipped examples still load.**
//!
//! `examples/` is documentation people copy. When a config setting or a flag
//! changes — and this project removes rather than deprecating — an example
//! written against the old spelling stops working, silently, until somebody
//! runs it.
//!
//! So: every example that IS an agentd config goes through `--validate-config`,
//! the same authority startup runs, and every flag a shipped script or unit
//! file passes is held to the table the loader reads.
#![cfg(unix)]

#[cfg(all(feature = "cel", feature = "sign"))]
mod common;

use std::path::{Path, PathBuf};
#[cfg(all(feature = "cel", feature = "sign"))]
use std::process::Command;

fn examples_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// Every `.yaml`/`.yml` under `examples/`, recursively.
#[cfg(all(feature = "cel", feature = "sign"))]
fn yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            yaml_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "yaml" || e == "yml") {
            out.push(p);
        }
    }
}

/// Every `{{secret:NAME}}` an example references.
#[cfg(all(feature = "cel", feature = "sign"))]
fn secret_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("{{secret:") {
        rest = &rest[i + "{{secret:".len()..];
        if let Some(end) = rest.find("}}") {
            out.push(rest[..end].trim().to_string());
            rest = &rest[end..];
        }
    }
    out
}

/// The examples are written against the RELEASE build. Two use a CEL `when:`
/// and one carries a `trust` pin, so a build without `cel` or `sign` refuses
/// them for a reason that has nothing to do with whether the example is
/// current. This asks its question only on a build that can answer it — if a
/// future example needs another feature, it says so by failing here.
#[test]
#[cfg(all(feature = "cel", feature = "sign"))]
fn every_shipped_example_config_validates() {
    let mut files = Vec::new();
    yaml_files(&examples_root(), &mut files);
    assert!(!files.is_empty(), "no examples found — wrong path?");

    let mut checked = 0;
    let mut failures = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        // Picked by SHAPE: Kubernetes manifests (a top-level `apiVersion:`)
        // and standalone workflow documents (a top-level `steps:`) live here
        // too and are not configs. Every other YAML file is one, so an example
        // cannot opt out of the check by what it leaves out.
        if text
            .lines()
            .any(|l| l.starts_with("apiVersion:") || l.starts_with("steps:"))
        {
            continue;
        }
        // A `services.yaml` beside it is a shared catalog, not a config: the
        // desks that use it are meant to be run as `-c services.yaml -c
        // <desk>.yaml`, and validating a desk alone reports every service it
        // references as undeclared. Layer it exactly as the README does.
        let catalog = f.with_file_name("services.yaml");
        let layered = catalog.exists() && catalog != *f;
        if !layered && f.file_name().is_some_and(|n| n == "services.yaml") {
            continue; // the catalog on its own declares no agent
        }
        checked += 1;
        // An example that references a secret is CORRECT — that is the
        // idiom. Supply each one so validation gets past the environment and
        // on to the question this test asks: does the config still parse, and
        // are its keys still keys?
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
        cmd.arg("--validate-config");
        if layered {
            cmd.arg("-c").arg(&catalog);
        }
        cmd.arg("-c").arg(f);
        for name in secret_names(&text).into_iter().chain(
            layered
                .then(|| std::fs::read_to_string(&catalog).unwrap_or_default())
                .map(|t| secret_names(&t))
                .unwrap_or_default(),
        ) {
            cmd.env(name, "test-value");
        }
        let out = cmd.output().unwrap();
        if !out.status.success() {
            failures.push(format!(
                "{}\n{}",
                f.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    assert!(
        checked >= 20,
        "only {checked} example configs found — wrong path, or a shape the sweep skips?"
    );
    assert!(
        failures.is_empty(),
        "shipped example configs no longer load:\n\n{}",
        failures.join("\n\n")
    );
}

/// The shipped FRAGMENT is a fragment, and its refusal is the lesson.
///
/// `examples/mcp-servers.fragment.json` carries `mcp.servers` and nothing else,
/// to be layered under a real config with a second `-c`. Its four servers hold
/// all three trifecta legs on purpose, so merging the whole thing into one
/// agent is refused — which is what SAMPLES.md teaches, and therefore something
/// a test should hold still. Note the file is `.json`: the config sweep above
/// collects only `.yaml`/`.yml`, so nothing covered it until now.
#[cfg(all(feature = "cel", feature = "sign"))]
#[test]
fn the_server_fragment_layers_under_a_config_and_its_trifecta_is_refused() {
    let frag = examples_root().join("mcp-servers.fragment.json");
    assert!(frag.exists(), "the fragment is shipped");

    // A base config the fragment layers under. Two of the four servers is a
    // legal agent; all four is not.
    let base = common::unique_path("frag-base", "yaml");
    std::fs::write(
        &base,
        "agent: { name: frag, instruction: \"be terse\", preflight: never }\n\
         intelligence: { endpoints: [\"mock:final\"], model: mock }\nstore: { kind: memory }\n",
    )
    .unwrap();

    let run = |args: &[&std::ffi::OsStr]| {
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .arg("--validate-config")
            .args(args)
            .env("FS_TOKEN", "test-value")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr).to_string()
    };

    // Merged into a config, the trifecta gate refuses it — by name, naming all
    // three legs, so the reader learns which tags collided.
    let merged = run(&[
        "-c".as_ref(),
        base.as_ref(),
        "-c".as_ref(),
        frag.as_os_str(),
    ]);
    assert!(
        merged.contains("lethal-trifecta refused")
            && merged.contains("untrusted_input")
            && merged.contains("sensitive")
            && merged.contains("egress"),
        "the refusal is the lesson, and must name the legs:\n{merged}"
    );
    let _ = std::fs::remove_file(&base);
}

/// The CLI's hidden re-exec and per-process flags: consumed by `main` before
/// the settings loader sees the argv, so no loader table lists them.
const MAIN_FLAGS: &[&str] = &[
    "--fresh",
    "--prompt-missing",
    "--env",
    "--internal-mock-mcp-http",
    "--internal-mock-llm",
    "--no-emit",
];

/// Every flag an agentd invocation in `text` passes, with the line it starts
/// on. An invocation is what follows the binary — `agentd`, `$AGENTD`, any
/// path ending in `/agentd`, or an `AGENTD_ARGS=` assignment — on one logical
/// line: shell continuations are
/// joined, and in Python the bracketed argv the binary opens. A Kubernetes
/// manifest's `args:` items are the container's, and every container shipped
/// here runs agentd.
fn invoked_flags(path: &Path, text: &str) -> Vec<(usize, String)> {
    let yaml = path.extension().is_some_and(|e| e == "yaml" || e == "yml");
    let python = path.extension().is_some_and(|e| e == "py");
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = i;
        let first = lines[i].trim_start();
        if first.starts_with('#') || first.starts_with("//") {
            i += 1;
            continue;
        }
        if yaml {
            if let Some(item) = first.strip_prefix("- ")
                && item.trim().starts_with("--")
            {
                out.push((start + 1, item.trim().to_string()));
            }
            i += 1;
            continue;
        }
        // One logical line: a trailing `\` continues it, and in Python an
        // open bracket does.
        let mut logical = lines[i].trim_end().to_string();
        let open = |l: &str| l.matches('[').count() as i64 - l.matches(']').count() as i64;
        let mut depth = open(&logical);
        while i + 1 < lines.len() && (logical.ends_with('\\') || (python && depth > 0)) {
            logical = logical.trim_end_matches('\\').to_string();
            i += 1;
            logical.push(' ');
            logical.push_str(lines[i].trim());
            depth += open(lines[i]);
        }
        i += 1;
        let tokens: Vec<&str> = logical
            .split(|c: char| c.is_whitespace() || c == '=' || c == ',' || c == '[' || c == ']')
            .map(|t| t.trim_matches(|c| c == '"' || c == '\'' || c == '\\'))
            .filter(|t| !t.is_empty())
            .collect();
        // `AGENTD_ARGS=` is an invocation too: the packaged unit expands it
        // into its `ExecStart`, so its value is argv the daemon parses.
        let Some(bin) = tokens.iter().position(|t| {
            *t == "agentd"
                || *t == "$AGENTD"
                || *t == "${AGENTD}"
                || *t == "AGENTD_ARGS"
                || t.ends_with("/agentd")
        }) else {
            continue;
        };
        // In Python only the argv the binary opens is its own.
        if python && !logical.contains("[agentd") {
            continue;
        }
        for t in &tokens[bin + 1..] {
            if t.starts_with("--") && t.len() > 2 {
                out.push((start + 1, (*t).to_string()));
            }
        }
    }
    out
}

/// The scripts and unit files people copy invoke the CLI directly, so a flag
/// the loader does not know breaks them exactly as an unknown config key
/// breaks a YAML example — and `--validate-config` cannot see them.
///
/// Every `--flag` an agentd invocation passes, in every shipped non-Markdown
/// file under `examples/`, `packaging/` and `bench/`, must be one the loader
/// accepts ([`agentd::config::settings::is_known_flag`], the same tables
/// `load` reads) or one `main` consumes first.
#[test]
fn every_flag_a_shipped_script_passes_is_accepted() {
    let root = examples_root();
    let mut files = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    walk(&root, &mut files);
    walk(&root.join("../packaging"), &mut files);
    walk(&root.join("../bench"), &mut files);

    let mut seen = 0;
    let mut hits = Vec::new();
    for f in &files {
        // Prose is the docs' to keep true; only what gets executed is here.
        if f.extension().is_some_and(|e| e == "md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for (line, flag) in invoked_flags(f, &text) {
            seen += 1;
            if !agentd::config::settings::is_known_flag(&flag) && !MAIN_FLAGS.contains(&&*flag) {
                hits.push(format!("{}:{line}: {flag}", f.display()));
            }
        }
    }
    assert!(
        seen >= 20,
        "only {seen} flags found in shipped agentd invocations — wrong path, or a shape the scan misses?"
    );
    assert!(
        hits.is_empty(),
        "shipped files pass flags the CLI does not accept:\n{}",
        hits.join("\n")
    );
}

/// The scan itself: a continued shell line, a Python argv, a manifest's args
/// and a flag BEFORE the binary (a `COPY --from=`) that is not agentd's.
#[test]
fn the_flag_scan_reads_each_invocation_shape() {
    let sh = "exec \"$AGENTD\" \\\n  --config x.yaml \\\n  --max-tokens 5\nCOPY --from=build /agentd /usr/local/bin/agentd\n# agentd --commented\n";
    assert_eq!(
        invoked_flags(Path::new("run.sh"), sh),
        vec![(1, "--config".into()), (1, "--max-tokens".into())]
    );
    let py = "argv = [agentd, \"--instruction\", x,\n        \"--log-level\", \"info\"]\nstub = [sys.executable, \"--addr-file\", f]\n";
    assert_eq!(
        invoked_flags(Path::new("run.py"), py),
        vec![(1, "--instruction".into()), (1, "--log-level".into())]
    );
    let yaml = "args:\n  - --mcp\n  - fs=https://x\n";
    assert_eq!(
        invoked_flags(Path::new("job.yaml"), yaml),
        vec![(2, "--mcp".into())]
    );
    let unit = "ExecStart=/usr/local/bin/agentd \\\n  --drain-timeout 25s\n";
    assert_eq!(
        invoked_flags(Path::new("a.service"), unit),
        vec![(1, "--drain-timeout".into())]
    );
    // The packaged unit: any absolute path to the binary, and the flags it
    // takes from its environment file.
    let unit = "EnvironmentFile=-/etc/default/agentd\nExecStart=/usr/bin/agentd --no-such-flag $AGENTD_ARGS\n";
    assert_eq!(
        invoked_flags(Path::new("agentd.service"), unit),
        vec![(2, "--no-such-flag".into())]
    );
    let env = "# flags in AGENTD_ARGS\nAGENTD_ARGS=--drain-timeout 25s\n";
    assert_eq!(
        invoked_flags(Path::new("agentd.env"), env),
        vec![(2, "--drain-timeout".into())]
    );
}
