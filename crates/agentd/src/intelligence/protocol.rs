//! Intelligence wire protocol.
//!
//! The harness emits JSON-RPC 2.0 requests over a length-framed
//! transport (4-byte little-endian length prefix + UTF-8 JSON),
//! matching the ecosystem's `sandbox::intelligence_server` so an
//! operator can run the existing host-side server and plug the agent
//! at it without glue code.
//!
//! Kept deliberately narrow — only the methods the workflow runtime
//! exercises today. `reason`, `embed`, `generate`, `tokenize`,
//! `models`, `health` live on the same trait surface but are
//! out-of-scope for Phase 4.

use serde::{Deserialize, Serialize};

/// A request the engine submits to an intelligence backend.
///
/// Flat shape (no JSON-RPC envelope) — the transport adds the
/// envelope before writing the frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    /// Logical model class (`fast`, `reasoning`, `code`). Maps to a
    /// concrete model in the backend.
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    /// `"system"` / `"user"` / `"assistant"`.
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub content: String,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelope types (used by the Unix transport)
// ---------------------------------------------------------------------------

#[cfg(any(unix, feature = "intel-http"))]
#[derive(Debug, Serialize)]
pub(crate) struct RpcRequest<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: &'a Request,
}

#[cfg(any(unix, feature = "intel-http"))]
#[derive(Debug, Deserialize)]
pub(crate) struct RpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: String,
    #[allow(dead_code)]
    pub id: Option<u64>,
    pub result: Option<Response>,
    pub error: Option<RpcError>,
}

#[cfg(any(unix, feature = "intel-http"))]
#[derive(Debug, Deserialize)]
pub(crate) struct RpcError {
    pub code: i32,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Length-framed I/O helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
use std::io::{self, Read, Write};

/// Write one length-framed JSON frame: 4-byte little-endian byte
/// count followed by the payload.
#[cfg(unix)]
pub(crate) fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame exceeds u32::MAX bytes"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Read one length-framed JSON frame. Hard cap at 16 MiB to avoid
/// unbounded allocation on a misbehaving server.
#[cfg(unix)]
pub(crate) fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    const MAX_FRAME: usize = 16 * 1024 * 1024;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("intelligence frame too large: {len} bytes > {MAX_FRAME}"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, any(unix, feature = "intel-http")))]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::Cursor;

    #[cfg(unix)]
    #[test]
    fn frame_round_trip() {
        let payload = br#"{"hello":"world"}"#;
        let mut buf = Vec::new();
        write_frame(&mut buf, payload).unwrap();
        assert_eq!(&buf[..4], &(payload.len() as u32).to_le_bytes()[..]);

        let mut cursor = Cursor::new(buf);
        let read = read_frame(&mut cursor).unwrap();
        assert_eq!(read, payload);
    }

    #[cfg(unix)]
    #[test]
    fn oversize_frame_rejected() {
        // Hand-craft a frame header claiming 32 MiB of body.
        let fake_len: u32 = 32 * 1024 * 1024;
        let mut buf = fake_len.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 128]);
        let err = read_frame(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_serializes_without_optionals() {
        let req = Request {
            model: "fast".into(),
            messages: vec![Message {
                role: "user".into(),
                content: "hi".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("max_tokens"));
        assert!(!s.contains("temperature"));
    }

    #[test]
    fn response_parses_minimal_shape() {
        let src = r#"{"content":"ok"}"#;
        let resp: Response = serde_json::from_str(src).unwrap();
        assert_eq!(resp.content, "ok");
        assert_eq!(resp.usage, Usage::default());
    }
}
