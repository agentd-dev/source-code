//! Build-time validator for an optional embedded workflow config.
//!
//! Reads `AGENTD_EMBED_CONFIG=/path/to/workflow.toml` (Mode B from
//! RFC §11.2 / §11.3). When set, parses the file, runs a lightweight
//! DAG integrity check, then tells the compiler two things:
//!
//! - `cargo:rustc-env=AGENTD_EMBEDDED_CONFIG_PATH=<abs>` — so
//!   `include_str!(env!(...))` in `src/embedded.rs` can bake the
//!   file into the binary.
//! - `cargo:rustc-cfg=embed_config` — a cfg flag the runtime uses
//!   to pick between the `Some(..)` / `None` branches in a way the
//!   compiler can see at macro-expansion time.
//!
//! Unset env var is the common case (generic runtime, Mode A) —
//! the build is a no-op. Parse or validation failures fail the
//! build loudly with the first offending issue.
//!
//! The validator here is deliberately a strict subset of
//! `crate::workflow::validator`: duplicating every check would
//! invite drift. What stays in build.rs are the cheap structural
//! checks that typical typos hit first; the runtime still runs
//! the full validator on every load.

use std::collections::HashSet;
use std::path::PathBuf;

fn main() {
    // Validate cfg flag names so Rust 1.80+'s unexpected-cfg lint
    // doesn't fire inside src/embedded.rs.
    println!("cargo:rustc-check-cfg=cfg(embed_config)");

    // Always rerun when the env var toggles.
    println!("cargo:rerun-if-env-changed=AGENTD_EMBED_CONFIG");

    let Ok(raw_path) = std::env::var("AGENTD_EMBED_CONFIG") else {
        // No embedded config. Generic runtime build.
        return;
    };
    if raw_path.trim().is_empty() {
        // Setting to the empty string is treated as "off" so CI
        // pipelines can clear the var without unsetting it.
        return;
    }

    let path = PathBuf::from(&raw_path);
    let abs = match path.canonicalize() {
        Ok(p) => p,
        Err(e) => fail(format!("AGENTD_EMBED_CONFIG={raw_path}: {e}")),
    };

    // Retrigger the build whenever the embedded file changes.
    println!("cargo:rerun-if-changed={}", abs.display());

    let src = match std::fs::read_to_string(&abs) {
        Ok(s) => s,
        Err(e) => fail(format!(
            "AGENTD_EMBED_CONFIG={}: read failed: {e}",
            abs.display()
        )),
    };

    let parsed: toml::Value = match toml::from_str(&src) {
        Ok(v) => v,
        Err(e) => fail(format!(
            "AGENTD_EMBED_CONFIG={}: invalid TOML: {e}",
            abs.display()
        )),
    };

    // Accept both the bare and `[[workflows]]`-wrapped shapes the
    // runtime parser accepts.
    let workflow: &toml::Value =
        if let Some(arr) = parsed.get("workflows").and_then(|v| v.as_array()) {
            match arr.len() {
                1 => &arr[0],
                n => fail(format!(
                    "AGENTD_EMBED_CONFIG={}: expected exactly one [[workflows]] entry, found {n}",
                    abs.display()
                )),
            }
        } else {
            &parsed
        };

    if let Err(reason) = validate(workflow) {
        fail(format!("AGENTD_EMBED_CONFIG={}: {reason}", abs.display()));
    }

    // Good. Emit the path + cfg for the runtime.
    println!(
        "cargo:rustc-env=AGENTD_EMBEDDED_CONFIG_PATH={}",
        abs.display()
    );
    println!("cargo:rustc-cfg=embed_config");

}

/// Minimal base64 decoder — build-time only, avoids a dep.
fn decode_base64(s: &str) -> Result<Vec<u8>, String> {
    const TABLE: &[i8] = &{
        let mut t = [-1i8; 256];
        let mut i = 0;
        while i < 26 {
            t[b'A' as usize + i] = i as i8;
            t[b'a' as usize + i] = (i + 26) as i8;
            i += 1;
        }
        i = 0;
        while i < 10 {
            t[b'0' as usize + i] = (i + 52) as i8;
            i += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t
    };
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let trimmed = bytes
        .iter()
        .take_while(|b| **b != b'=')
        .copied()
        .collect::<Vec<_>>();
    if trimmed.len() % 4 == 1 {
        return Err("length mod 4 == 1 is impossible for base64".into());
    }
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut chunk = 0u32;
    let mut bits = 0u8;
    for (i, &b) in trimmed.iter().enumerate() {
        let v = TABLE[b as usize];
        if v < 0 {
            return Err(format!("non-base64 byte 0x{b:02x} at offset {i}"));
        }
        chunk = (chunk << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((chunk >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Lightweight DAG validator
// ---------------------------------------------------------------------------

/// Strict-subset validator: catches the errors a typo usually
/// produces (empty name, duplicate ids, dangling edges, unknown
/// start-node entries). The runtime validator covers the rest
/// (acyclicity, reachability, etc.) and keeps running at load time.
fn validate(doc: &toml::Value) -> Result<(), String> {
    // `name` required + non-empty.
    let name = doc
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "workflow is missing a non-empty `name`".to_string())?;

    // Collect node ids.
    let mut ids = HashSet::new();
    if let Some(nodes) = doc.get("nodes").and_then(|v| v.as_array()) {
        for (i, n) in nodes.iter().enumerate() {
            let id = n
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("workflow `{name}`: node #{i} missing `id`"))?;
            if id.trim().is_empty() {
                return Err(format!("workflow `{name}`: node #{i} has empty `id`"));
            }
            if !ids.insert(id.to_string()) {
                return Err(format!("workflow `{name}`: duplicate node id `{id}`"));
            }
        }
    }

    // Edge targets must reference declared nodes.
    if let Some(edges) = doc.get("edges").and_then(|v| v.as_array()) {
        for (i, e) in edges.iter().enumerate() {
            let from = e
                .get("from")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("workflow `{name}`: edge #{i} missing `from`"))?;
            let to = e
                .get("to")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("workflow `{name}`: edge #{i} missing `to`"))?;
            if !ids.contains(from) {
                return Err(format!(
                    "workflow `{name}`: edge #{i}: source `{from}` is not a declared node id"
                ));
            }
            if !ids.contains(to) {
                return Err(format!(
                    "workflow `{name}`: edge #{i}: target `{to}` is not a declared node id"
                ));
            }
        }
    }

    // Start nodes: entry_node, if set, must point at a real node.
    let mut start_names = HashSet::new();
    if let Some(starts) = doc.get("start_nodes").and_then(|v| v.as_array()) {
        for (i, s) in starts.iter().enumerate() {
            let name_str = s
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("workflow `{name}`: start_node #{i} missing `name`"))?;
            if !start_names.insert(name_str.to_string()) {
                return Err(format!(
                    "workflow `{name}`: duplicate start_node name `{name_str}`"
                ));
            }
            if let Some(entry) = s.get("entry_node").and_then(|v| v.as_str()) {
                if !ids.contains(entry) {
                    return Err(format!(
                        "workflow `{name}`: start_node `{name_str}` references unknown entry `{entry}`"
                    ));
                }
            }
        }
    }

    // HTTP routes reference declared start nodes.
    if let Some(routes) = doc.get("http_routes").and_then(|v| v.as_array()) {
        for (i, r) in routes.iter().enumerate() {
            let sn = r
                .get("start_node")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    format!("workflow `{name}`: http_route #{i} missing `start_node`")
                })?;
            if !start_names.contains(sn) {
                return Err(format!(
                    "workflow `{name}`: http_route #{i} points at unknown start_node `{sn}`"
                ));
            }
        }
    }

    // Triggers reference declared start nodes.
    if let Some(triggers) = doc.get("triggers").and_then(|v| v.as_array()) {
        for (i, t) in triggers.iter().enumerate() {
            let sn = t
                .get("start_node")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("workflow `{name}`: trigger #{i} missing `start_node`"))?;
            if !start_names.contains(sn) {
                return Err(format!(
                    "workflow `{name}`: trigger #{i} points at unknown start_node `{sn}`"
                ));
            }
        }
    }

    Ok(())
}

fn fail(msg: String) -> ! {
    // cargo:warning makes the message obvious in the build output
    // even when someone runs `cargo build -q`; the panic is what
    // actually fails the build.
    eprintln!("cargo:warning=agent: {msg}");
    panic!("{msg}");
}
