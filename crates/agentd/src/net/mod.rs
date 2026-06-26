//! Transports. One hand-rolled HTTP/1.1 client over `Read + Write` (the single
//! highest-leverage minimalism decision — avoids the url→IDNA→ICU and
//! async-runtime taxes), plus unix-socket and the feature-gated tls/vsock
//! transports. RFC 0006 §transports. The client is non-streaming
//! request/response; SSE is an unbuilt v2 surface (RFC 0013).

pub mod http;
pub mod ssrf;
pub mod unixsock;

#[cfg(feature = "tls")]
pub mod tls;

#[cfg(feature = "vsock")]
pub mod vsock;
