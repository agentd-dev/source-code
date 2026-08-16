// SPDX-License-Identifier: AGPL-3.0-only
//! **net** — hand-rolled transport primitives shared by the `mcp` crate and
//! agentd's intel client. One blocking HTTP/1.1 client over any `Read + Write`
//! (the single highest-leverage minimalism decision — avoids the url→IDNA→ICU and
//! async-runtime taxes) with buffered + streaming/SSE request paths, plus
//! unix-socket and the feature-gated tls/vsock connects, and an SSRF egress
//! classifier. RFC 0006 §transports. serde-free.

pub mod http;
pub mod ssrf;
pub mod unixsock;

#[cfg(feature = "tls")]
pub mod tls;

// A minimal X.509 field extractor (subject CN + SANs) for surfacing an mTLS
// peer's verified identity to principal matching (RFC 0029 §10.3). Only needed
// under TLS; pure DER parsing, no new dependency.
#[cfg(feature = "tls")]
pub mod x509;

#[cfg(feature = "vsock")]
pub mod vsock;
