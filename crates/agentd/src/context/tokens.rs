// SPDX-License-Identifier: Apache-2.0
//! Token **estimates** (RFC 0026 §5.2, §7). agentd never counts provider tokens
//! itself — the provider's `usage` is the truth once a call returns — but the
//! governor's reservation and the compaction trigger need a number *before*
//! the call. The heuristic is the usual `chars / 4` (English prose ≈ 4 chars a
//! token; JSON and code run denser, which errs on the safe side for a
//! reservation) plus a small per-message overhead. Deliberately simple and
//! dependency-free; the estimate is only ever compared against generous
//! thresholds.

use serde_json::Value;

/// Per-message framing overhead (role, delimiters).
pub const MESSAGE_OVERHEAD: u64 = 4;

/// Estimate the tokens of a text.
pub fn estimate(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// Estimate the tokens of a JSON value (its compact serialization).
pub fn estimate_value(v: &Value) -> u64 {
    match v {
        Value::String(s) => estimate(s),
        Value::Null => 1,
        other => estimate(&other.to_string()),
    }
}

/// The default context window when the model is unknown (a conservative
/// modern default; `intelligence.model_window` overrides).
pub const DEFAULT_MODEL_WINDOW: u64 = 128_000;

/// A best-effort window from the model name (kept tiny and obviously
/// approximate; unknown models get the default).
pub fn window_for_model(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    if m.contains("claude") {
        200_000
    } else if m.contains("gpt-4o")
        || m.contains("gpt-4.1")
        || m.contains("gpt-5")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
    {
        if m.contains("gpt-4.1") {
            1_000_000
        } else {
            128_000
        }
    } else if m.contains("gemini") {
        1_000_000
    } else {
        DEFAULT_MODEL_WINDOW
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_are_monotone_and_rounded_up() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("abcd"), 1);
        assert_eq!(estimate("abcde"), 2);
        assert!(estimate("a much longer sentence with many words") > estimate("short"));
        assert_eq!(estimate_value(&serde_json::json!(null)), 1);
        assert_eq!(estimate_value(&serde_json::json!("abcd")), 1);
        assert!(estimate_value(&serde_json::json!({"k": "vvvv"})) >= 2);
        assert_eq!(window_for_model("claude-sonnet-5"), 200_000);
        assert_eq!(window_for_model("gpt-4.1-mini"), 1_000_000);
        assert_eq!(window_for_model("something-else"), DEFAULT_MODEL_WINDOW);
    }
}
