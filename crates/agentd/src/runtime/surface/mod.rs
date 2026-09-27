// SPDX-License-Identifier: AGPL-3.0-only
//! **What this instance offers over A2A** — the command ops and the extension
//! URIs that declare them.
//!
//! Always compiled, even in a build without the `a2a` feature, because
//! `--capabilities` reports this surface and that report is not feature-gated.
//! It lived inside the gated listener module until the feature matrix caught
//! it: the manifest referenced a module that a no-`a2a` build does not have.

// One file per list, so the units that change a list own its file and never
// this one. The globs keep every item at its `surface::` path, whichever file
// it lives in.
pub mod auth;
pub mod events;
pub mod ext;
/// What the `agentd tui` / `agentd ui` launcher is allowed to do.
pub mod launch;
pub mod manifest;
pub mod methods;
pub mod ops;

pub use ext::*;
pub use methods::*;
pub use ops::*;
