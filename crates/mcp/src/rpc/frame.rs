// SPDX-License-Identifier: AGPL-3.0-only
//! Length-prefix framing over a byte stream (`read_frame` / `write_frame`),
//! sharing the JSON-RPC codec in the parent module ([`crate::rpc`]): a 4-byte
//! big-endian length followed by that many payload bytes. The private
//! supervisor↔subagent control channel rides it — robust to payloads
//! (instructions, context seeds, distilled results) that legitimately contain
//! newlines. Generic over `Read`/`Write`, so it drops onto pipes, unix sockets
//! and TLS streams alike.

use std::io::{self, Read, Write};

/// Hard cap on a single frame. A peer claiming more is a protocol error, not an
/// allocation.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Write a 4-byte big-endian length prefix followed by the JSON payload.
pub fn write_frame<W: Write, T: serde::Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    if buf.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME",
        ));
    }
    w.write_all(&(buf.len() as u32).to_be_bytes())?;
    w.write_all(&buf)?;
    w.flush()
}

/// Read one length-prefixed frame. Returns `Ok(None)` on clean EOF before the
/// length prefix (orderly shutdown). A declared length over [`MAX_FRAME`] is
/// rejected before allocation.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    if !read_exact_or_eof(r, &mut len_buf)? {
        return Ok(None); // clean EOF before any length byte
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame length exceeds MAX_FRAME",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Like `read_exact`, but distinguishes clean EOF (no bytes read → `false`)
/// from a truncated read (some bytes then EOF → error).
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => {
                return if filled == 0 {
                    Ok(false)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "EOF mid-frame",
                    ))
                };
            }
            n => filled += n,
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{Id, Response};
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        let resp = Response::ok(Id::Num(1), serde_json::json!({"ok": true}));
        write_frame(&mut buf, &resp).unwrap();
        let mut cur = Cursor::new(buf);
        let frame = read_frame(&mut cur).unwrap().unwrap();
        let back: Response = serde_json::from_slice(&frame).unwrap();
        assert_eq!(back.id, Id::Num(1));
        assert!(read_frame(&mut cur).unwrap().is_none());
    }

    #[test]
    fn frame_with_newline_payload_survives() {
        // The whole point of length-framing for the control channel.
        let mut buf = Vec::new();
        write_frame(&mut buf, &serde_json::json!({"text": "line1\nline2"})).unwrap();
        let mut cur = Cursor::new(buf);
        let frame = read_frame(&mut cur).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(v["text"], "line1\nline2");
    }

    #[test]
    fn oversize_length_rejected() {
        let mut bytes = (MAX_FRAME as u32 + 1).to_be_bytes().to_vec();
        bytes.push(0);
        let mut cur = Cursor::new(bytes);
        assert!(read_frame(&mut cur).is_err());
    }
}
