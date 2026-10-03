// SPDX-License-Identifier: AGPL-3.0-only
//! **instruction-core** — the reference implementation of the
//! [Instruction Specification](https://github.com/instruction-md/specification)
//! as a library: version 1, registry revision 1.1, with the S7 resolution
//! manifest and S27 end matter.
//!
//! One Markdown file defines a whole agent. This crate reads such a document
//! against the spec's own vendored JSON Schema registry (kinds, forms,
//! grants, keywords, labels, attribute values, the Appendix B refusals), so
//! the parser cannot drift from the specification it implements.
//!
//! **The pipeline.** [`parse`] reads the block tree, and [`deliver`] turns it
//! into the text one reader's model receives, byte-exactly (§3.5):
//!
//! - **Notes and end matter are removed.** An author note (`<!--` at column
//!   0, S9) is never parsed and never delivered. End matter (a closing
//!   `---` YAML block, S27) is the document's record: it is split off before
//!   any block is read ([`doc::split_end_matter`]) and kept on
//!   [`doc::Document::end_matter`].
//! - **Variants are selected.** `when` keeps its body when its condition
//!   matches the reader's facts and parameters, `unless` when it does not,
//!   and `otherwise` when nothing in the run before it was kept (S10). A key
//!   the reader was not given keeps the content.
//! - **Overrides apply.** `overrides="kind/name"` keeps the named rule, and
//!   its reason, out of the delivery (S24). A guardrail, or a rule stronger
//!   than its overrider, is never overridden: refused in the same document,
//!   kept in an included one.
//! - **Every block is rendered once, from its block, in its document's label
//!   style** (`bold`, `plain` or `tags`, S25). A rule delivers its label and
//!   condition but never its name (S12, S15), a reason follows its rule
//!   (S14), an example and an output stand their label on a line of their
//!   own (S16, S17), machinery becomes its one acknowledgement line, and an
//!   include is inlined in its own style.
//! - **Typed parameters are substituted last.** `${name}` takes a value that
//!   fits its declared type (S18), never inside fenced code, so a value is
//!   never read again as Markdown.
//!
//! The same walk accounts for what it delivered in the §7.4 resolution
//! manifest ([`Manifest`], S7): the authored digest, the parameters and
//! facts used, the variants kept and dropped, the includes, the limits
//! reached, and what was left unresolved or overridden.
//! [`Manifest::signed_form`] is what a delivery attestation embeds, and
//! [`Manifest::canonical`] its RFC 8785 bytes (S7 §3). The §7.2 digests
//! ([`digest()`], [`author_digest`]) are in every build. [`advise()`]
//! reports the Appendix C advisories, which never refuse a document and never
//! change its delivery. Behind the `sign` feature, the `sign` module verifies
//! author and delivery JWS attestations.
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
