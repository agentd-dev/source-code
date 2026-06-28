// SPDX-License-Identifier: Apache-2.0
//! Unix-domain-socket transport. RFC 0006 §transports.
//!
//! The common same-pod case: a model gateway (or an MCP server) listening on
//! a unix socket, spoken to with the same hand-rolled HTTP/1.1 client as TCP
//! ([`crate::net::http`]) — the socket is just the byte stream.

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// Connect to a unix socket at `path`, applying read/write timeouts so a
    /// wedged gateway can't hang a request forever.
    pub fn connect(path: &str, timeout: Duration) -> io::Result<UnixStream> {
        let s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(timeout))?;
        Ok(s)
    }
}

#[cfg(unix)]
pub use imp::connect;

#[cfg(not(unix))]
pub fn connect(_path: &str, _timeout: std::time::Duration) -> std::io::Result<std::net::TcpStream> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unix-socket transport is Unix-only; use https:// on this platform",
    ))
}
