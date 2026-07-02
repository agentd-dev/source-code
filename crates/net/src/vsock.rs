// SPDX-License-Identifier: Apache-2.0
//! AF_VSOCK transport for enclave/microVM intelligence. RFC 0006 §transports.
//! [feature: vsock]
//!
//! For agentd running inside a microVM / confidential enclave, reaching a model
//! gateway on the host across the virtio-socket boundary — no TCP stack exposed
//! in the guest. `VsockStream` is `Read + Write`, so it drops into the
//! transport-agnostic HTTP client ([`crate::http`]).

use std::io;
use std::time::Duration;
use vsock::VsockStream;

/// Connect to `(cid, port)` over vsock, applying read/write timeouts so a
/// wedged host gateway can't hang a request forever.
pub fn connect(cid: u32, port: u32, timeout: Duration) -> io::Result<VsockStream> {
    let stream = VsockStream::connect_with_cid_port(cid, port)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}
