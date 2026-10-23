// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd --build-info`: how this binary was built, as one JSON object.
//!
//! The features are this crate's own `[features]`, each read with `cfg!` in
//! the crate that is compiled with them, so the answer is the binary's and
//! not a copy of what someone meant to build. A feature another one implies
//! (`sign` turns on `aauth`) is listed, because it is compiled in. The
//! revision, whether the source was modified and the builder are what the
//! build was told (`AGENTD_REVISION`, `AGENTD_SOURCE_MODIFIED`,
//! `AGENTD_BUILDER`): `null` when nothing said, never a guess.

use serde_json::{Value, json};

/// Every cargo feature of agentd-cli, with whether this build has it.
/// `default` is not among them: it names the set `tls` belongs to rather than
/// a capability. A test holds this list to `Cargo.toml`'s `[features]`.
const FEATURES: &[(&str, bool)] = &[
    ("a2a", cfg!(feature = "a2a")),
    ("aauth", cfg!(feature = "aauth")),
    ("cel", cfg!(feature = "cel")),
    ("companies", cfg!(feature = "companies")),
    ("config-watch", cfg!(feature = "config-watch")),
    ("cron", cfg!(feature = "cron")),
    ("decrypt", cfg!(feature = "decrypt")),
    ("exec", cfg!(feature = "exec")),
    ("hot-reload", cfg!(feature = "hot-reload")),
    ("internal-mocks", cfg!(feature = "internal-mocks")),
    ("mcp-events", cfg!(feature = "mcp-events")),
    ("metrics", cfg!(feature = "metrics")),
    ("mock-models", cfg!(feature = "mock-models")),
    ("oauth", cfg!(feature = "oauth")),
    ("oci", cfg!(feature = "oci")),
    ("otel", cfg!(feature = "otel")),
    ("sign", cfg!(feature = "sign")),
    ("tls", cfg!(feature = "tls")),
    ("x402", cfg!(feature = "x402")),
    ("xaa", cfg!(feature = "xaa")),
];

/// The features compiled into this binary, sorted.
fn features() -> Vec<&'static str> {
    let mut on: Vec<&str> = FEATURES
        .iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| *name)
        .collect();
    on.sort_unstable();
    on
}

/// A value the build was given, or `None` when it was unset or empty.
fn given(v: Option<&'static str>) -> Option<&'static str> {
    v.filter(|s| !s.is_empty())
}

/// The `--build-info` document.
pub(crate) fn build_info() -> Value {
    json!({
        "name": "agentd",
        "version": agentd::VERSION,
        "revision": given(option_env!("AGENTD_REVISION")),
        // build.rs refuses any other spelling, so this is the whole range.
        "source_modified": given(option_env!("AGENTD_SOURCE_MODIFIED")).map(|v| v == "true"),
        "features": features(),
        "target": env!("AGENTD_BUILD_TARGET"),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "rustc": given(option_env!("AGENTD_BUILD_RUSTC")),
        "builder": given(option_env!("AGENTD_BUILDER")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys of `[features]` in this crate's manifest, `default` aside.
    fn declared() -> Vec<String> {
        let manifest =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        let mut out = Vec::new();
        let mut inside = false;
        for line in manifest.lines() {
            let t = line.trim();
            if t.starts_with('[') && !t.starts_with("[\"") {
                inside = t == "[features]";
                continue;
            }
            if inside
                && !t.starts_with('#')
                && let Some((name, _)) = t.split_once('=')
            {
                out.push(name.trim().to_string());
            }
        }
        out.retain(|f| f != "default");
        out.sort();
        out
    }

    /// A feature added to the manifest and not here would be compiled in and
    /// never reported, and the builder image's check that a build has what it
    /// asked for would then refuse every build that asks for it.
    #[test]
    fn every_cargo_feature_is_reported() {
        let mut listed: Vec<String> = FEATURES.iter().map(|(n, _)| n.to_string()).collect();
        listed.sort();
        let before = listed.len();
        listed.dedup();
        assert_eq!(before, listed.len(), "a feature is listed twice");
        let declared = declared();
        assert!(declared.len() > 10, "[features] parsed as {declared:?}");
        assert_eq!(
            listed, declared,
            "build_info::FEATURES must be agentd-cli's [features], `default` aside"
        );
    }

    #[test]
    fn the_document_has_its_shape() {
        let info = build_info();
        let keys: Vec<&String> = info.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "builder",
                "features",
                "name",
                "profile",
                "revision",
                "rustc",
                "source_modified",
                "target",
                "version"
            ]
        );
        assert_eq!(info["name"], "agentd");
        assert_eq!(info["version"], agentd::VERSION);
        assert_eq!(
            info["profile"],
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
        assert!(info["target"].as_str().is_some_and(|t| t.contains('-')));
        assert!(
            info["rustc"]
                .as_str()
                .is_some_and(|r| r.starts_with("rustc "))
        );
        let features: Vec<&str> = info["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        let mut sorted = features.clone();
        sorted.sort_unstable();
        assert_eq!(features, sorted, "features are reported sorted");
        assert_eq!(features.contains(&"tls"), cfg!(feature = "tls"));
    }
}
