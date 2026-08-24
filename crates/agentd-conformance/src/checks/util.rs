// SPDX-License-Identifier: AGPL-3.0-only
//! Shared helpers for the conformance families: write config/playbook files
//! into a check-scoped temp dir, and drive the built-in mock LLM from a JSON
//! playbook (the `{"turns":[…]}` format the runtime e2e uses). Scoping every
//! file to the check's own temp dir is what lets checks run concurrently
//! without one clobbering another's config.

use crate::harness::{Harness, MockLlm, TempDir};
use serde_json::Value;

/// Write `contents` to `name` inside `tmp`; return the absolute path.
pub fn write_file(tmp: &TempDir, name: &str, contents: &str) -> String {
    let p = tmp.path().join(name);
    std::fs::write(&p, contents).expect("write temp file");
    p.to_str().expect("utf8 path").to_string()
}

/// Launch the built-in mock LLM driven by a JSON `playbook` (`{"turns":[…]}` +
/// optional `"match"`), written into `tmp`. The intelligence endpoint is
/// [`MockLlm::uri`].
pub fn mock_llm(h: &Harness, tmp: &TempDir, playbook: &Value) -> MockLlm {
    let pb = write_file(tmp, "playbook.json", &playbook.to_string());
    h.mock_llm(&format!("file:{pb}"))
}
