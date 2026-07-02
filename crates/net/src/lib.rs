// SPDX-License-Identifier: Apache-2.0
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

#[cfg(feature = "vsock")]
pub mod vsock;
