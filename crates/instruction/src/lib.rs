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
//! selected, includes transcluded, `${}` substituted last) with the §7.4
//! resolution manifest accounting for it ([`Manifest`], S7), computes §7.2
//! digests ([`digest()`]), reports the Appendix C advisories ([`advise()`]),
//! and — behind the `sign` feature — verifies author/delivery JWS
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
// Block-scoped delivery (§3.5): the renderer `doc::fold_full` drives.
mod deliver;
pub use api::{Context, Delivery, deliver, parse, tree_json, validate};

// Every refusal from `parse` to the API is one structured type carrying its
// Appendix B code (S20); `CODES` and `UNDETECTED` account for the catalogue.
mod refusal;
pub use refusal::{CODES, NON_CATALOGUE, Refusal, UNDETECTED};

// The Appendix C advisories (S19): read from a parsed document, never a
// refusal, and never a change to what it delivers.
mod advise;
pub use advise::{ADVISORY_CODES, Advisory, Severity, advise};

// The §7.4 manifest (S7) and the §7.2 digests it is made of are format, not
// crypto: both in every build, so one delivery has one manifest whatever the
// features. `sign` gates only JWS/Ed25519 verification.
mod digest;
mod manifest;
pub use digest::{author_digest, digest, front_matter_id, strip_front_matter_signature};
pub use manifest::{Authored, Fact, Include, Limits, Manifest, ParameterUse, Variants};
