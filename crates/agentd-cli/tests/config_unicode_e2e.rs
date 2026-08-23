// SPDX-License-Identifier: AGPL-3.0-only
//! **Non-ASCII text in a JSON/JSONC config must survive the comment stripper
//! byte for byte.**
//!
//! `.json`/`.jsonc` documents go through `config::file::strip_jsonc` before
//! serde_json sees them, because the RFC set shows jsonc and the moat forbids a
//! jsonc *crate*. That stripper scans bytes — correctly, since every byte it
//! matches on (`"`, `\`, `/`, `*`, `\n`) is ASCII and can never appear inside a
//! multibyte UTF-8 sequence — but it used to *emit* bytes too, via `byte as
//! char`. That cast is a Latin-1 reinterpretation: `0xE2 as char` is 'â', so an
//! em-dash, an accented name or any CJK text inside a string literal came out
//! mojibake'd.
//!
//! The failure mode is the worst one available: mojibake is still valid JSON, so
//! there is no parse error, no validation failure, no log line — the agent just
//! runs on a subtly wrong instruction. These tests therefore assert both halves:
//! the parsed document is byte-identical to what was written (`documents`), and
//! the instruction the MODEL actually receives is the one the operator typed
//! (`the daemon`), proved by making the mock LLM answer differently for the
//! corrupted spelling.

mod common;

use std::process::{Command, Stdio};

/// The instruction under test: an em-dash (3-byte), accented Latin (2-byte),
/// CJK (3-byte) and an emoji (4-byte) — one string covering every UTF-8
/// sequence length the stripper can split.
const INSTRUCTION: &str = "Résumé the brief — 日本語で요약 ✅ (τέλος)";

/// The Latin-1 mojibake the byte-wise stripper produced: exactly `byte as char`
/// over the UTF-8 encoding. Computed rather than pasted so the probe can never
/// drift from what the bug actually did.
fn mojibake(s: &str) -> String {
    s.bytes().map(|b| b as char).collect()
}

fn write_file(tag: &str, ext: &str, body: &str) -> String {
    let path = common::unique_path(tag, ext);
    std::fs::write(&path, body).expect("write test file");
    path
}

/// A plain `.json` config — no comments at all. The stripper still runs over it
/// (format is decided by extension, not by whether comments are present), so
/// this is the case where a byte-wise copy corrupts a document that never asked
/// for jsonc in the first place.
fn json_config(intel: &str) -> String {
    format!(
        r#"{{
  "config_version": "1",
  "agent": {{ "name": "unicode", "instruction": "{INSTRUCTION}" }},
  "intelligence": {{ "endpoints": "{intel}", "model": "mock" }},
  "observability": {{ "log_level": "error" }}
}}"#
    )
}

/// The same document as `.jsonc`, with comments placed exactly where a byte-wise
/// stripper breaks: multibyte text INSIDE a comment (whose bytes are skipped by
/// a byte counter that must land back on a char boundary), a `/* */` welded to
/// the closing quote of a value that ENDS in a multibyte char, and a `//` line
/// comment opening on multibyte text.
fn jsonc_config(intel: &str) -> String {
    format!(
        r#"{{
  // 設定 — the agent identity, commented in 日本語
  "config_version": "1",
  /* 註釋: a block comment carrying 4-byte text 🎌 before the key */
  "agent": {{ "name": "unicode", "instruction": "{INSTRUCTION}"/* — glued to the quote */ }},
  "intelligence": {{ "endpoints": "{intel}", "model": "mock" }}, // ✅ 日本語 trailing
  "observability": {{ "log_level": "error" }}
}}"#
    )
}

#[test]
fn json_and_jsonc_documents_round_trip_non_ascii_byte_for_byte() {
    // The direct half: read each file through the real config reader and compare
    // the parsed string to the constant, byte for byte. `assert_eq!` on `&str` IS
    // a byte comparison, and the extra byte-slice assertion makes the intent
    // (and a failure's diagnostics) explicit for a mojibake'd value.
    for (tag, ext, body) in [
        ("cfg-unicode", "json", json_config("https://intel.example")),
        (
            "cfg-unicode",
            "jsonc",
            jsonc_config("https://intel.example"),
        ),
    ] {
        let path = write_file(tag, ext, &body);
        let (doc, format) = agentd::config::file::read_document(&path).expect("config parses");
        assert_eq!(format, agentd::config::file::Format::Json, "{ext} is json");
        let got = doc["agent"]["instruction"]
            .as_str()
            .unwrap_or_else(|| panic!("{ext}: no agent.instruction in {doc}"));
        assert_eq!(
            got.as_bytes(),
            INSTRUCTION.as_bytes(),
            "{ext}: the instruction was corrupted in transit\n  wrote: {INSTRUCTION}\n  read : {got}"
        );
        // …and specifically NOT the Latin-1 shadow the old stripper produced.
        assert_ne!(got, mojibake(INSTRUCTION), "{ext}: mojibake");
        // The comments themselves left nothing behind: the neighbouring scalars
        // are intact, so the stripper resumed on a char boundary after each one.
        assert_eq!(doc["agent"]["name"], serde_json::json!("unicode"));
        assert_eq!(doc["intelligence"]["model"], serde_json::json!("mock"));
        assert_eq!(
            doc["observability"]["log_level"],
            serde_json::json!("error")
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn a_multibyte_char_welded_to_a_comment_survives() {
    // The precise shape a byte-wise stripper mangles: the multibyte char is the
    // LAST thing before a comment opens and the FIRST thing after it closes,
    // with no ASCII in between to resynchronise on.
    let body = r#"{
  "config_version": "1",
  "agent": { "instruction": "—"/*—*/ },
  "intelligence": { "endpoints": "https://intel.example", "model": "日本語"//—
 }
}"#;
    let path = write_file("cfg-welded", "jsonc", body);
    let (doc, _) = agentd::config::file::read_document(&path).expect("config parses");
    assert_eq!(doc["agent"]["instruction"], serde_json::json!("—"));
    assert_eq!(doc["intelligence"]["model"], serde_json::json!("日本語"));
    let _ = std::fs::remove_file(&path);
    // An escape sequence next to multibyte text must not eat the following byte
    // (the stripper skips `\` + one byte without looking at it).
    let body = r#"{ "config_version": "1",
  "agent": { "instruction": "a\"—\\éé" } }"#;
    let path = write_file("cfg-escape", "jsonc", body);
    let (doc, _) = agentd::config::file::read_document(&path).expect("config parses");
    assert_eq!(doc["agent"]["instruction"], serde_json::json!("a\"—\\éé"));
    let _ = std::fs::remove_file(&path);
}

/// A mock-LLM playbook that answers with a DIFFERENT marker depending on which
/// spelling of the instruction reached the model. The corrupted rule is listed
/// first so a regression names itself in the assertion output instead of merely
/// failing to match.
fn probe_playbook() -> serde_json::Value {
    serde_json::json!({
        "match": [
            {"when_contains": mojibake(INSTRUCTION), "content": "CORRUPTED-INSTRUCTION"},
            {"when_contains": INSTRUCTION, "content": "INTACT-INSTRUCTION"}
        ],
        "turns": [{"content": "MISSING-INSTRUCTION"}]
    })
}

struct MockLlm {
    child: std::process::Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}

fn spawn_mock_llm(playbook: &serde_json::Value) -> MockLlm {
    let pb = common::unique_path("playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

#[test]
fn the_daemon_sends_the_model_the_instruction_the_operator_wrote() {
    // End to end, through the real binary: a `.jsonc` config with comments welded
    // to multibyte text drives a real turn, and the mock LLM reports which
    // spelling of the instruction actually arrived in the request body. This is
    // the assertion that matters operationally — a unit test on the stripper
    // proves the bytes, this proves the AGENT runs on them.
    let llm = spawn_mock_llm(&probe_playbook());
    let cfg = write_file("cfg-unicode-e2e", "jsonc", &jsonc_config(&llm.uri));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(
        stdout.contains("INTACT-INSTRUCTION"),
        "the model received a different instruction than the config carries.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_file(&cfg);
}

#[test]
fn the_yaml_reader_round_trips_non_ascii_too() {
    // The sibling syntax, held to the same bar. The hand-rolled YAML subset
    // reader also scans bytes and also strips comments (`#`), so it is the other
    // place this class of bug could live — it does NOT (it slices rather than
    // casting), and this test is what keeps that true. "One document model, two
    // syntaxes" has to include the document's text.
    let body = format!(
        "config_version: \"1\"\n# 註釋 — a comment with 日本語\nagent:\n  instruction: \"{INSTRUCTION}\"  # ✅ trailing\n  name: unicode\nintelligence:\n  endpoints: https://intel.example\n  model: 日本語\n"
    );
    let path = write_file("cfg-unicode", "yaml", &body);
    let (doc, format) = agentd::config::file::read_document(&path).expect("yaml parses");
    assert_eq!(format, agentd::config::file::Format::Yaml);
    let got = doc["agent"]["instruction"]
        .as_str()
        .expect("an instruction");
    assert_eq!(
        got.as_bytes(),
        INSTRUCTION.as_bytes(),
        "yaml: the instruction was corrupted in transit\n  wrote: {INSTRUCTION}\n  read : {got}"
    );
    // A bare (unquoted) CJK scalar with a comment on the line above it, too.
    assert_eq!(doc["intelligence"]["model"], serde_json::json!("日本語"));
    let _ = std::fs::remove_file(&path);
}
