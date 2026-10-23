// SPDX-License-Identifier: AGPL-3.0-only
//! What `agentd --build-info` reports that only the build knows: the target
//! triple and the compiler that built the binary. It has no dependencies, on
//! purpose: a build script's dependencies are compiled into every build.
//!
//! The three values a builder passes — `AGENTD_REVISION`,
//! `AGENTD_SOURCE_MODIFIED` and `AGENTD_BUILDER` — are read where they are
//! reported (`src/build_info.rs`), and declared here so a change to any of
//! them rebuilds the binary instead of leaving an old answer in it.

use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let version = Command::new(&rustc)
        .arg("-V")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=AGENTD_BUILD_TARGET={target}");
    println!("cargo:rustc-env=AGENTD_BUILD_RUSTC={version}");

    // `source_modified` is a boolean or unknown; any other spelling would be
    // reported as unknown and look like an answer, so the build refuses it.
    if let Ok(v) = std::env::var("AGENTD_SOURCE_MODIFIED")
        && !matches!(v.as_str(), "" | "true" | "false")
    {
        panic!("AGENTD_SOURCE_MODIFIED must be true or false, not {v:?}");
    }
    for var in [
        "AGENTD_REVISION",
        "AGENTD_SOURCE_MODIFIED",
        "AGENTD_BUILDER",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
