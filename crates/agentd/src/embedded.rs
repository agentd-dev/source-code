//! Access to the workflow baked into the binary at build time.
//!
//! When `AGENTD_EMBED_CONFIG=/path/to/wf.toml` is set at build time,
//! [`build.rs`](../../build.rs) validates the file and emits two
//! rustc directives: an env var carrying the resolved path and a
//! `cfg(embed_config)` flag. This module reads both: under
//! `cfg(embed_config)` it uses `include_str!` to bake the TOML
//! bytes into the binary; otherwise [`EMBEDDED_CONFIG`] is `None`.
//!
//! Consumers never touch the compile-time machinery directly —
//! they ask `agentd::embedded::EMBEDDED_CONFIG` at runtime and
//! branch on the `Option`.

/// The embedded workflow TOML source, if the build was performed
/// with `AGENTD_EMBED_CONFIG` set.
#[cfg(embed_config)]
pub const EMBEDDED_CONFIG: Option<&str> = Some(include_str!(env!("AGENTD_EMBEDDED_CONFIG_PATH")));

#[cfg(not(embed_config))]
pub const EMBEDDED_CONFIG: Option<&str> = None;

/// Embedded workflow signature bytes, if the build was performed with
/// `AGENTD_EMBED_CONFIG_SIG` set alongside `AGENTD_EMBED_CONFIG`. The
/// content is the raw decoded signature (NOT base64) — `build.rs`
/// decodes the base64 `.sig` file at build time so the runtime can
/// verify without touching base64.
#[cfg(embed_config_sig)]
pub const EMBEDDED_CONFIG_SIG: Option<&[u8]> =
    Some(include_bytes!(env!("AGENTD_EMBEDDED_CONFIG_SIG_PATH")));

#[cfg(not(embed_config_sig))]
pub const EMBEDDED_CONFIG_SIG: Option<&[u8]> = None;

/// Convenience: `true` iff the build embedded a workflow.
pub const fn has_embedded_config() -> bool {
    EMBEDDED_CONFIG.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_config_matches_flag() {
        assert_eq!(EMBEDDED_CONFIG.is_some(), has_embedded_config());
    }

    // The CI default build has no embedded config, so this test is
    // expected to pass as-is. A build with `AGENTD_EMBED_CONFIG=...`
    // exercises the other branch via the tests/embedded_build.rs
    // integration test.
    #[test]
    #[cfg(not(embed_config))]
    fn ci_default_has_no_embedded_config() {
        assert!(EMBEDDED_CONFIG.is_none());
    }
}
