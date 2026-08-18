// SPDX-License-Identifier: AGPL-3.0-only
//! Config admission, end to end — the four ways the loader used to let a bad or
//! untrusted configuration through the door.
//!
//! Admission is the one place a mistake is still cheap: nothing has been dialed,
//! no credential has left the process, no effect has been checkpointed. So the
//! contract is narrow and absolute — a configuration agentd cannot honour EXITS
//! 2, before any side effect, with a message that names what is wrong. These
//! tests hold that line for the four regressions:
//!
//! 1. an `intelligence.headers` `{{secret:…}}` ref that does not resolve was
//!    dropped at dial time, so the daemon started and talked to the model with
//!    NO credential (the request goes out, unauthenticated, on a header the
//!    operator believes is set);
//! 2. a `.agentd.yml` merely FOUND in the working directory could lift the
//!    lethal-trifecta gate — `cd` into a repo you cloned, run a flags-only
//!    `agentd`, and that repo's dotfile governs your grant;
//! 3. a `reset` value with a multi-byte character sliced a `str` off a char
//!    boundary and panicked (exit 101) instead of exiting 2;
//! 4. the "both dotfile spellings" refusal — which exists because DISCOVERY
//!    cannot choose between them — also fired on `--config` paths the operator
//!    had explicitly named and ordered.

mod common;

use std::path::Path;
use std::process::Command;

/// A fresh, empty directory to run in. Every case here is sensitive to what is
/// (and is not) in the working directory — discovery reads it — so no test may
/// inherit the repository's own dotfiles.
fn workdir(tag: &str) -> String {
    let dir = common::unique_path(tag, "dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

fn write(dir: &str, name: &str, body: &str) -> String {
    let path = Path::new(dir).join(name);
    std::fs::write(&path, body).expect("write config");
    path.to_string_lossy().into_owned()
}

/// Run agentd in `dir` with a scrubbed environment: every `AGENTD_*`/`AGENT_*`
/// name the loader reads is removed, so a case's outcome comes from its config
/// and its `env` overrides alone — not from whatever the developer exported.
/// Returns `(exit code, stderr)`; the code is `-1` only if a signal killed it,
/// which would itself be the bug.
fn agentd(dir: &str, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.current_dir(dir).args(args);
    for (k, _) in std::env::vars() {
        if k.starts_with("AGENTD_") || k.starts_with("AGENT_") {
            cmd.env_remove(k);
        }
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn agentd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A configuration whose only credential is a `{{secret:…}}` reference — the
/// shape RFC 0017 §6 asks for (the file is secret-free; the value lives in the
/// environment).
const HEADER_REF: &str = "config_version: \"2\"\nstore: {kind: memory}\n\
     intelligence:\n  endpoints: [\"https://intel.invalid/v1\"]\n  model: m\n\
     \x20 headers:\n    authorization: \"Bearer {{secret:AGENTD_TEST_ABSENT_INTEL_KEY}}\"\n";

/// DEFECT 1 — an unresolvable secret ref in `intelligence.headers` must be
/// exit 2 naming the ref, not a silent drop.
///
/// The old behaviour resolved the header with `.ok()` and filtered the failures
/// out of the list, which is the worst possible failure mode for a credential:
/// the daemon comes up healthy and dials the model with the `authorization`
/// header simply absent.
#[test]
fn an_unresolvable_intelligence_header_ref_is_exit_2_and_names_the_ref() {
    let dir = workdir("cfg-secret");
    let cfg = write(&dir, "intel.yaml", HEADER_REF);

    // The env var is not set (the harness scrubs AGENTD_*/AGENT_*).
    let (code, err) = agentd(&dir, &["--config", &cfg, "--validate-config"], &[]);
    assert_eq!(code, 2, "unresolvable secret ref must be exit 2:\n{err}");
    assert!(
        err.contains("AGENTD_TEST_ABSENT_INTEL_KEY"),
        "the message must name the unresolved reference:\n{err}"
    );
    assert!(
        err.contains("intelligence.headers"),
        "the message must name the setting:\n{err}"
    );

    // …and a real start refuses too — the admission gate and `--validate-config`
    // are the same check, so the daemon can never come up in a state the
    // pre-flight would have rejected.
    let (code, err) = agentd(&dir, &["--config", &cfg], &[]);
    assert_eq!(code, 2, "startup must refuse the same config:\n{err}");
    assert!(err.contains("AGENTD_TEST_ABSENT_INTEL_KEY"), "{err}");

    // The control: with the referenced secret present, the same file is valid.
    // Without this the test would pass for a loader that rejects every ref.
    let (code, err) = agentd(
        &dir,
        &["--config", &cfg, "--validate-config"],
        &[("AGENTD_TEST_ABSENT_INTEL_KEY", "sk-test")],
    );
    assert_eq!(code, 0, "a resolvable ref must validate clean:\n{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A grant that is the whole lethal trifecta by tags alone: an untrusted-input
/// source, a sensitive+egress sink. Refused unless the operator lifts the gate.
const TRIFECTA_MCP: &str = "config_version: \"2\"\nstore: {kind: memory}\n\
     mcp:\n  servers:\n\
     \x20   - name: web\n      endpoint: https://mcp-web.invalid/mcp\n      tags: {\"*\": [untrusted_input]}\n\
     \x20   - name: vault\n      endpoint: https://mcp-vault.invalid/mcp\n      tags: {\"*\": [sensitive, egress]}\n";

/// DEFECT 2 — a DISCOVERED `.agentd.yml` may not relax a security control.
///
/// Discovery is a convenience for the operator's own project directory, but the
/// invocation that gets it never named it: a flags-only `agentd --prompt …` in
/// a directory you did not write is enough. So the dotfile keeps every power
/// that only narrows or configures — and loses the two that widen. Naming the
/// same file with `--config` restores them: naming it IS the deliberate act.
#[test]
fn a_discovered_dotfile_cannot_lift_the_trifecta_gate() {
    let dir = workdir("cfg-discover");
    let dotfile = write(
        &dir,
        ".agentd.yml",
        &format!("{TRIFECTA_MCP}security:\n  allow_trifecta: true\n"),
    );

    // Discovered: the file is adopted for its endpoints and servers, but the
    // `security.allow_trifecta` in it is refused outright — exit 2, naming both
    // the file and the setting so the operator can see what was in the
    // directory they happened to be standing in.
    let (code, err) = agentd(&dir, &["--validate-config"], &[]);
    assert_eq!(
        code, 2,
        "a discovered dotfile must not lift the trifecta gate:\n{err}"
    );
    assert!(
        err.contains("security.allow_trifecta") && err.contains(".agentd.yml"),
        "the refusal must name the setting and the file:\n{err}"
    );

    // The gate itself is still ON for a discovered file: strip the relaxation
    // and the same grant is refused as the lethal trifecta it is. (Without
    // this, the case above would pass for a loader that ignored the file.)
    let plain = workdir("cfg-discover-plain");
    write(&plain, ".agentd.yml", TRIFECTA_MCP);
    let (code, err) = agentd(&plain, &["--validate-config"], &[]);
    assert_eq!(code, 2, "the trifecta grant itself is refused:\n{err}");
    assert!(err.contains("lethal-trifecta"), "{err}");

    // …and the explicitly NAMED file keeps its full power: same bytes, same
    // grant, but the operator pointed at it, so `allow_trifecta` applies.
    let named = workdir("cfg-named");
    let (code, err) = agentd(&named, &["--config", &dotfile, "--validate-config"], &[]);
    assert_eq!(
        code, 0,
        "--config must keep the power discovery does not have:\n{err}"
    );

    for d in [&dir, &plain, &named] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// DEFECT 3 — a config value must never panic the process.
///
/// `HH:MMZ` was matched by byte-slicing a `str` whose LENGTH had been checked in
/// bytes, so a six-byte value carrying a multi-byte character (`0é:0Z`) sliced
/// through the middle of a character and panicked: exit 101, a stack trace, and
/// a config error reported as a crash.
#[test]
fn a_multibyte_budget_reset_is_exit_2_not_a_panic() {
    let dir = workdir("cfg-reset");
    let cfg = write(
        &dir,
        "budget.yaml",
        "config_version: \"2\"\nstore: {kind: memory}\n\
         intelligence:\n  budget:\n    windows:\n      - per: day\n        tokens: 100\n        reset: \"0é:0Z\"\n",
    );

    let (code, err) = agentd(&dir, &["--config", &cfg, "--validate-config"], &[]);
    assert_ne!(code, 101, "a config error must not panic:\n{err}");
    assert_eq!(code, 2, "a bad reset value is a config error:\n{err}");
    assert!(
        err.contains("reset"),
        "the message must name the field:\n{err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// DEFECT 4 — the both-spellings refusal belongs to DISCOVERY only.
///
/// Two dotfiles in one directory are ambiguous because nothing states an order;
/// whichever agentd picked, somebody would be editing the other. Two `--config`
/// flags state their order explicitly, so the fact that they happen to be named
/// `.agentd.yml` and `.agentd.yaml` is nobody's business but the operator's.
#[test]
fn the_both_spellings_refusal_applies_only_to_discovery() {
    // Discovery still refuses: no order was stated.
    let ambiguous = workdir("cfg-ambiguous");
    write(&ambiguous, ".agentd.yml", "config_version: \"2\"\n");
    write(&ambiguous, ".agentd.yaml", "config_version: \"2\"\n");
    let (code, err) = agentd(&ambiguous, &["--validate-config"], &[]);
    assert_eq!(code, 2, "two discovered spellings must refuse:\n{err}");
    assert!(err.contains(".agentd.yaml"), "{err}");

    // Named: two layers, in the order given, whatever they are called. The
    // later file wins the key both set — proof they were really layered and
    // not merely accepted.
    let base = workdir("cfg-layer-base");
    let over = workdir("cfg-layer-over");
    let a = write(
        &base,
        ".agentd.yml",
        "config_version: \"2\"\nstore: {kind: memory}\nintelligence:\n  model: from-base\n",
    );
    let b = write(
        &over,
        ".agentd.yaml",
        "intelligence:\n  model: from-overlay\n",
    );

    let run = workdir("cfg-layer-run");
    let (code, err) = agentd(
        &run,
        &["--config", &a, "--config", &b, "--validate-config"],
        &[],
    );
    assert_eq!(
        code, 0,
        "explicitly named files may be spelled anything:\n{err}"
    );
    assert!(
        err.contains("config.valid"),
        "the pre-flight must report the layered set as valid:\n{err}"
    );

    for d in [&ambiguous, &base, &over, &run] {
        let _ = std::fs::remove_dir_all(d);
    }
}
