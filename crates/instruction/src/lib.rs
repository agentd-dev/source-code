// SPDX-License-Identifier: AGPL-3.0-only
//! **instruction-core** — the reference implementation of the
//! [Instruction Specification](https://github.com/instruction-md/specification)
//! as a library.
//!
//! One Markdown file defines a whole agent. This crate turns such a document
//! into a typed block tree ([`doc::parse`]) — author notes set aside unread,
//! end matter split off as the document's record ([`doc::split_end_matter`])
//! — validates it against the spec's own vendored JSON Schema registry
//! (kinds, forms, grants, attribute values, the semantic rules, Appendix B
//! refusal shapes), runs the §3.5 delivery pipeline
//! byte-exactly ([`deliver`] — prose degraded, machinery acknowledged, `when`
//! selected, includes transcluded, `${}` substituted last), and — behind the
//! `sign` feature — computes §7 digests and verifies author/delivery JWS
//! attestations ([`sign`]).
//!
//! agentd is the first consumer (its `config::idoc` module is a re-export of
//! [`doc`], with the agentd-specific configuration folding layered on top);
//! the platform's publish-time validator and resolved-read path are the
//! second. A TypeScript twin (`@instruction-md/spec`) runs the same fixture
//! corpus, and [`tree_json`] emits the spec §9.1 block-tree shape both
//! implementations are compared by.

pub mod doc;
#[cfg(feature = "sign")]
pub mod sign;
pub mod yaml;

mod api;
pub use api::{Context, Delivery, deliver, parse, tree_json, validate};

// Every refusal from `parse` to the API is one structured type carrying its
// Appendix B code (S20); `CODES` and `UNDETECTED` account for the catalogue.
mod refusal;
pub use refusal::{CODES, Refusal, UNDETECTED};

// The §7.4 manifest types are format, not crypto: always available, digest
// STRINGS filled only when the `sign` feature computes them.
pub use api::{Authored, Manifest, Variants};
