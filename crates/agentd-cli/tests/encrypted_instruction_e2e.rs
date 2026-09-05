// SPDX-License-Identifier: AGPL-3.0-only
//! **Encrypted instruction documents, end to end** (RFC 0041): a document that
//! carries machinery is encrypted to the agent's recipient key, handed to the
//! real binary as an inline armored envelope, a binary `--instruction-file`,
//! and a compact JWE — and in each case the plaintext parses, the trust ladder
//! applies, and the machinery registers in `--capabilities`. The fail-closed
//! half is pinned too: an envelope with no configured key is a refusal that
//! names `instruction.decrypt.keys`, never ciphertext delivered as prose.
#![cfg(all(unix, feature = "decrypt", feature = "workflow"))]

mod common;

use std::process::Command;

use agentd::config::decrypt::{agefile, jwe, x25519};
use serde_json::{Value, json};

/// A plaintext document with observable machinery.
const DOC: &str = "---\nspec: \"1\"\n---\n# Sealed agent\n\nBe discreet.\n\n:::!workflow{name=sealed-drain}\nsteps:\n  start: { kind: manual }\n  done:  { kind: finish, depends_on: [start] }\n:::\n";

fn run_cfg(cfg: &Value) -> (bool, String, Value) {
    let cfg_path = common::unique_path("enc-instr", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(cfg).unwrap()).unwrap();
    let v = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", &cfg_path, "--validate-config"])
        .output()
        .unwrap();
    let errtext = format!(
        "{}{}",
        String::from_utf8_lossy(&v.stdout),
        String::from_utf8_lossy(&v.stderr)
    );
    let caps = if v.status.success() {
        let c = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["-c", &cfg_path, "--capabilities"])
            .output()
            .unwrap();
        serde_json::from_slice(&c.stdout).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let _ = std::fs::remove_file(&cfg_path);
    (v.status.success(), errtext, caps)
}

fn base_cfg(instruction: &str, key_file: Option<&str>) -> Value {
    let mut cfg = json!({
        "config_version": "1",
        "agent": {"name": "sealed", "preflight": "never", "instruction": instruction},
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
    });
    if let Some(k) = key_file {
        cfg["instruction"] = json!({"decrypt": {"keys": [k]}});
    }
    cfg
}

fn workflow_names(caps: &Value) -> Vec<String> {
    caps["workflows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w["name"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn an_armored_age_envelope_decrypts_and_its_machinery_loads() {
    let sk = [21u8; 32];
    let pk = x25519::public_key(&sk);
    let enc = agefile::encrypt(DOC.as_bytes(), &[pk], None).unwrap();
    let armored = agentd::config::envelope::armor(&enc);

    let key_path = common::unique_path("age-key", "txt");
    std::fs::write(&key_path, agefile::encode_identity(&sk)).unwrap();

    let (ok, err, caps) = run_cfg(&base_cfg(&armored, Some(&key_path)));
    assert!(ok, "encrypted instruction refused:\n{err}");
    assert_eq!(
        workflow_names(&caps),
        ["sealed-drain"],
        "the decrypted machinery registered"
    );
    let _ = std::fs::remove_file(&key_path);
}

#[test]
fn a_compact_jwe_decrypts_the_same_way() {
    let sk = [22u8; 32];
    let pk = x25519::public_key(&sk);
    let compact = jwe::encrypt_ecdh_es(DOC.as_bytes(), &pk).unwrap();

    let key_path = common::unique_path("jwe-key", "txt");
    let hex: String = sk.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&key_path, hex).unwrap();

    let (ok, err, caps) = run_cfg(&base_cfg(&compact, Some(&key_path)));
    assert!(ok, "JWE instruction refused:\n{err}");
    assert_eq!(workflow_names(&caps), ["sealed-drain"]);
    let _ = std::fs::remove_file(&key_path);
}

#[test]
fn a_binary_age_file_via_instruction_file_is_armored_and_decrypted() {
    let sk = [23u8; 32];
    let pk = x25519::public_key(&sk);
    let enc = agefile::encrypt(DOC.as_bytes(), &[pk], None).unwrap();
    let doc_path = common::unique_path("enc-doc", "age");
    std::fs::write(&doc_path, &enc).unwrap(); // BINARY, not armored

    let key_path = common::unique_path("age-key-b", "txt");
    std::fs::write(&key_path, agefile::encode_identity(&sk)).unwrap();
    let cfg_path = common::unique_path("enc-instr-b", "json");
    std::fs::write(
        &cfg_path,
        serde_json::to_vec(&json!({
            "config_version": "1",
            "agent": {"name": "sealed", "preflight": "never"},
            "instruction": {"decrypt": {"keys": [key_path]}},
            "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
            "store": {"kind": "memory"},
        }))
        .unwrap(),
    )
    .unwrap();
    let c = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "-c",
            &cfg_path,
            "--instruction-file",
            &doc_path,
            "--capabilities",
        ])
        .output()
        .unwrap();
    assert!(
        c.status.success(),
        "binary envelope via --instruction-file refused: {}",
        String::from_utf8_lossy(&c.stderr)
    );
    let caps: Value = serde_json::from_slice(&c.stdout).unwrap();
    assert_eq!(workflow_names(&caps), ["sealed-drain"]);
    for p in [doc_path, cfg_path] {
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn an_envelope_with_no_key_is_refused_naming_the_config() {
    let pk = x25519::public_key(&[24u8; 32]);
    let enc = agefile::encrypt(DOC.as_bytes(), &[pk], None).unwrap();
    let armored = agentd::config::envelope::armor(&enc);
    let (ok, err, _) = run_cfg(&base_cfg(&armored, None));
    assert!(!ok, "an undecryptable envelope must refuse startup");
    assert!(
        err.contains("instruction.decrypt.keys"),
        "the refusal names the fix:\n{err}"
    );
    // And the WRONG key refuses too — never ciphertext-as-prose.
    let key_path = common::unique_path("wrong-key", "txt");
    std::fs::write(&key_path, agefile::encode_identity(&[25u8; 32])).unwrap();
    let (ok, err, _) = run_cfg(&base_cfg(&armored, Some(&key_path)));
    assert!(!ok);
    assert!(err.contains("no configured identity"), "{err}");
    let _ = std::fs::remove_file(&key_path);
}
