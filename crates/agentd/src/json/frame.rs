//! Two framings over a byte stream, sharing the codec above.
//!
//! - **NDJSON** (`read_line` / `write_line`): one JSON value per line,
//!   no embedded newlines. The MCP stdio transport framing (RFC 0004).
//! - **Length-prefix** (`read_frame` / `write_frame`): a 4-byte big-endian
//!   length followed by that many payload bytes. The private supervisor↔
//!   subagent control channel (RFC 0005) — robust to payloads (instructions,
//!   context seeds, distilled results) that legitimately contain newlines.
//!
//! Both are generic over `Read`/`Write` so they drop onto pipes, unix
//! sockets, TLS streams, and vsock alike. Lifted/adapted from the retired
//! `intelligence/protocol.rs` length-framing (salvage list, PLAN.md).

use std::io::{self, BufRead, Read, Write};

/// Hard cap on a single frame/line, for both framings. A peer claiming more
/// is a protocol error, not an allocation. 16 MiB matches the MCP-side cap.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

// ---- NDJSON (MCP stdio) ----

/// Serialize `value` as compact JSON plus a trailing `\n`. Errors if the
/// encoded form contains a newline (it cannot for valid compact JSON, but we
/// assert the invariant the transport relies on).
pub fn write_line<W: Write, T: serde::Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    debug_assert!(!buf.contains(&b'\n'), "compact JSON must not contain newlines");
    w.write_all(&buf)?;
    w.write_all(b"\n")?;
    w.flush()
}

/// Read one newline-delimited frame. Returns `Ok(None)` on clean EOF (the
/// peer closed the stream between messages — an orderly shutdown signal).
/// A line longer than [`MAX_FRAME`] is an error.
pub fn read_line<R: BufRead>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match r.read(&mut byte)? {
            0 => {
                // EOF. Mid-line EOF is a truncated frame; clean EOF is None.
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF mid-line"))
                };
            }
            _ => {
                if byte[0] == b'\n' {
                    return Ok(Some(buf));
                }
                if buf.len() >= MAX_FRAME {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line exceeds MAX_FRAME"));
                }
                buf.push(byte[0]);
            }
        }
    }
}

// ---- Length-prefix (control channel) ----

/// Write a 4-byte big-endian length prefix followed by the JSON payload.
pub fn write_frame<W: Write, T: serde::Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    if buf.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds MAX_FRAME"));
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
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length exceeds MAX_FRAME"));
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
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF mid-frame"))
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
    use crate::json::{Id, Response};
    use std::io::Cursor;

    #[test]
    fn line_roundtrip() {
        let mut buf = Vec::new();
        write_line(&mut buf, &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(buf.last(), Some(&b'\n'));
        let mut cur = Cursor::new(buf);
        let line = read_line(&mut cur).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(v["a"], 1);
        // clean EOF -> None
        assert!(read_line(&mut cur).unwrap().is_none());
    }

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
