// SPDX-License-Identifier: AGPL-3.0-only
//! **net** — hand-rolled transport primitives shared by the `mcp` crate and
//! agentd's intel client. One blocking HTTP/1.1 client over any `Read + Write`
//! (the single highest-leverage minimalism decision — avoids the url→IDNA→ICU and
//! async-runtime taxes) with buffered + streaming/SSE request paths, plus
//! unix-socket and the feature-gated tls/vsock connects, and an SSRF egress
//! classifier. Deliberately serde-free: nothing here parses a payload, so the
//! transport layer adds no deserialization attack surface.

pub mod http;
pub mod ssrf;
pub mod unixsock;

#[cfg(feature = "tls")]
pub mod tls;

// A minimal X.509 field extractor (subject CN + SANs) that surfaces an mTLS
// peer's already-verified identity for principal matching. Only needed under
// TLS; pure DER parsing, no new dependency.
#[cfg(feature = "tls")]
pub mod x509;

#[cfg(feature = "vsock")]
pub mod vsock;
