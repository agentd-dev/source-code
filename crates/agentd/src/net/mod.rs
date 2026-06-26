//! Transports. One hand-rolled HTTP/1.1(+SSE) client over `Read + Write`
//! (the single highest-leverage minimalism decision — avoids the
//! url→IDNA→ICU and async-runtime taxes), plus unix-socket, and the
//! feature-gated tls/vsock transports. RFC 0006 §transports.

pub mod http;
pub mod ssrf;
pub mod unixsock;

#[cfg(feature = "tls")]
pub mod tls;

#[cfg(feature = "vsock")]
pub mod vsock;
