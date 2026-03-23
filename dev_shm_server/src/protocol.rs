//! Wire protocol for shared memory ring buffers.
//!
//! Two layers:
//!
//! 1. **`Frame`** (generic) — extensible frame format with arbitrary key-value
//!    headers and a raw payload. Designed for the gRPC data plane.
//!
//! 2. **`Message`** (simple) — the original fixed-format message for backward
//!    compatibility and simple use cases.
//!
//! ## Frame wire format
//!
//! ```text
//! ┌─────────────────────────────────────────────────────┐
//! │ version:    u8     (currently 1)                     │
//! │ flags:      u8     (bit 0 = END_STREAM, etc.)        │
//! │ stream_id:  u32 LE (for multiplexing)                │
//! │ header_len: u32 LE (total bytes of headers section)  │
//! │ payload_len:u32 LE (total bytes of payload)          │
//! ├─ headers (header_len bytes) ─────────────────────────┤
//! │  repeated:                                           │
//! │    key_len: u16 LE                                   │
//! │    key:     [u8; key_len]                            │
//! │    val_len: u16 LE                                   │
//! │    val:     [u8; val_len]                            │
//! ├─ payload (payload_len bytes) ────────────────────────┤
//! │  raw bytes — NOT serialized, no protobuf             │
//! └─────────────────────────────────────────────────────┘
//! ```
//!
//! Fixed overhead: 14 bytes. Headers are optional (header_len = 0).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("message too short: need {need} bytes, have {have}")]
    TooShort { need: usize, have: usize },
    #[error("unknown message type: {0}")]
    UnknownType(u8),
    #[error("unsupported frame version: {0}")]
    UnsupportedVersion(u8),
    #[error("header section truncated")]
    HeaderTruncated,
    #[error("decode error: {0}")]
    Decode(String),
}

// ─── Frame (generic, extensible) ─────────────────────────────────────────────

pub const FRAME_VERSION: u8 = 1;
pub const FRAME_HEADER_SIZE: usize = 14; // version + flags + stream_id + header_len + payload_len

/// Bit flags for Frame.
pub mod flags {
    /// Last frame in this stream.
    pub const END_STREAM: u8 = 0x01;
    /// Payload is compressed.
    pub const COMPRESSED: u8 = 0x02;
    /// This is an error/status frame.
    pub const ERROR: u8 = 0x04;
    /// Control frame (shutdown, ping, etc.)
    pub const CONTROL: u8 = 0x08;
}

/// A generic frame with extensible key-value headers and a raw payload.
///
/// Headers are arbitrary byte key-value pairs — use them for:
/// - `:method`, `:path` (gRPC-style routing)
/// - `content-type`, `x-request-id`
/// - Any custom metadata
///
/// Payload is raw bytes — no serialization imposed. The caller decides
/// the format (protobuf, flatbuffers, raw tensor, JPEG, whatever).
#[derive(Debug, Clone)]
pub struct Frame {
    pub version: u8,
    pub flags: u8,
    pub stream_id: u32,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(stream_id: u32) -> Self {
        Frame {
            version: FRAME_VERSION,
            flags: 0,
            stream_id,
            headers: Vec::new(),
            payload: Vec::new(),
        }
    }

    /// Set a header (key and value as bytes).
    pub fn header(mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Set a string header.
    pub fn header_str(self, key: &str, value: &str) -> Self {
        self.header(key.as_bytes().to_vec(), value.as_bytes().to_vec())
    }

    /// Set the payload (raw bytes, no serialization).
    pub fn payload(mut self, data: Vec<u8>) -> Self {
        self.payload = data;
        self
    }

    /// Set flags.
    pub fn flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }

    /// Get a header value by key.
    pub fn get_header(&self, key: &[u8]) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(k, _)| k.as_slice() == key)
            .map(|(_, v)| v.as_slice())
    }

    /// Get a header as a UTF-8 string.
    pub fn get_header_str(&self, key: &str) -> Option<&str> {
        self.get_header(key.as_bytes())
            .and_then(|v| std::str::from_utf8(v).ok())
    }

    pub fn is_end_stream(&self) -> bool {
        self.flags & flags::END_STREAM != 0
    }

    pub fn is_error(&self) -> bool {
        self.flags & flags::ERROR != 0
    }

    pub fn is_control(&self) -> bool {
        self.flags & flags::CONTROL != 0
    }

    /// Total encoded size in bytes.
    pub fn encoded_size(&self) -> usize {
        let headers_size: usize = self
            .headers
            .iter()
            .map(|(k, v)| 2 + k.len() + 2 + v.len())
            .sum();
        FRAME_HEADER_SIZE + headers_size + self.payload.len()
    }

    /// Encode into a byte buffer.
    pub fn encode(&self) -> Vec<u8> {
        let headers_size: usize = self
            .headers
            .iter()
            .map(|(k, v)| 2 + k.len() + 2 + v.len())
            .sum();

        let total = FRAME_HEADER_SIZE + headers_size + self.payload.len();
        let mut buf = Vec::with_capacity(total);

        // Fixed header
        buf.push(self.version);
        buf.push(self.flags);
        buf.extend_from_slice(&self.stream_id.to_le_bytes());
        buf.extend_from_slice(&(headers_size as u32).to_le_bytes());
        buf.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());

        // Headers (TLV)
        for (key, val) in &self.headers {
            buf.extend_from_slice(&(key.len() as u16).to_le_bytes());
            buf.extend_from_slice(key);
            buf.extend_from_slice(&(val.len() as u16).to_le_bytes());
            buf.extend_from_slice(val);
        }

        // Payload
        buf.extend_from_slice(&self.payload);

        buf
    }

    /// Decode from a byte buffer.
    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < FRAME_HEADER_SIZE {
            return Err(ProtocolError::TooShort {
                need: FRAME_HEADER_SIZE,
                have: data.len(),
            });
        }

        let version = data[0];
        if version != FRAME_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        let flags = data[1];
        let stream_id = u32::from_le_bytes(data[2..6].try_into().unwrap());
        let header_len = u32::from_le_bytes(data[6..10].try_into().unwrap()) as usize;
        let payload_len = u32::from_le_bytes(data[10..14].try_into().unwrap()) as usize;

        let total_needed = FRAME_HEADER_SIZE + header_len + payload_len;
        if data.len() < total_needed {
            return Err(ProtocolError::TooShort {
                need: total_needed,
                have: data.len(),
            });
        }

        // Parse headers
        let header_data = &data[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + header_len];
        let headers = decode_headers(header_data)?;

        // Payload
        let payload_start = FRAME_HEADER_SIZE + header_len;
        let payload = data[payload_start..payload_start + payload_len].to_vec();

        Ok(Frame {
            version,
            flags,
            stream_id,
            headers,
            payload,
        })
    }
}

fn decode_headers(mut data: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ProtocolError> {
    let mut headers = Vec::new();
    while data.len() >= 4 {
        let key_len = u16::from_le_bytes(data[0..2].try_into().unwrap()) as usize;
        data = &data[2..];
        if data.len() < key_len {
            return Err(ProtocolError::HeaderTruncated);
        }
        let key = data[..key_len].to_vec();
        data = &data[key_len..];

        if data.len() < 2 {
            return Err(ProtocolError::HeaderTruncated);
        }
        let val_len = u16::from_le_bytes(data[0..2].try_into().unwrap()) as usize;
        data = &data[2..];
        if data.len() < val_len {
            return Err(ProtocolError::HeaderTruncated);
        }
        let val = data[..val_len].to_vec();
        data = &data[val_len..];

        headers.push((key, val));
    }
    Ok(headers)
}

// ─── Message (simple, backward-compatible) ───────────────────────────────────

/// Original simple message type — kept for backward compatibility.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageType {
    Request,
    Response,
    Error,
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub request_id: u64,
    pub msg_type: MessageType,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn request(id: u64, payload: Vec<u8>) -> Self {
        Message { request_id: id, msg_type: MessageType::Request, payload }
    }

    pub fn response(id: u64, payload: Vec<u8>) -> Self {
        Message { request_id: id, msg_type: MessageType::Response, payload }
    }

    pub fn error(id: u64, msg: &str) -> Self {
        Message { request_id: id, msg_type: MessageType::Error, payload: msg.as_bytes().to_vec() }
    }

    pub fn shutdown() -> Self {
        Message { request_id: 0, msg_type: MessageType::Shutdown, payload: Vec::new() }
    }

    pub fn encode(&self) -> Vec<u8> {
        let type_byte = match self.msg_type {
            MessageType::Request => 0x01,
            MessageType::Response => 0x02,
            MessageType::Error => 0x03,
            MessageType::Shutdown => 0xFF,
        };
        let mut buf = Vec::with_capacity(13 + self.payload.len());
        buf.extend_from_slice(&self.request_id.to_le_bytes());
        buf.push(type_byte);
        buf.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < 13 {
            return Err(ProtocolError::TooShort { need: 13, have: data.len() });
        }
        let request_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let type_byte = data[8];
        let payload_len = u32::from_le_bytes(data[9..13].try_into().unwrap()) as usize;
        if data.len() < 13 + payload_len {
            return Err(ProtocolError::TooShort { need: 13 + payload_len, have: data.len() });
        }
        let msg_type = match type_byte {
            0x01 => MessageType::Request,
            0x02 => MessageType::Response,
            0x03 => MessageType::Error,
            0xFF => MessageType::Shutdown,
            other => return Err(ProtocolError::UnknownType(other)),
        };
        Ok(Message { request_id, msg_type, payload: data[13..13 + payload_len].to_vec() })
    }
}

// ─── Conversions between Frame and Message ───────────────────────────────────

impl From<Message> for Frame {
    fn from(msg: Message) -> Frame {
        let flags = match msg.msg_type {
            MessageType::Shutdown => flags::CONTROL | flags::END_STREAM,
            MessageType::Error => flags::ERROR,
            _ => 0,
        };
        let type_str = match msg.msg_type {
            MessageType::Request => "request",
            MessageType::Response => "response",
            MessageType::Error => "error",
            MessageType::Shutdown => "shutdown",
        };
        Frame::new(0)
            .flags(flags)
            .header_str(":type", type_str)
            .header("request-id", msg.request_id.to_le_bytes().to_vec())
            .payload(msg.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_encode_decode_no_headers() {
        let frame = Frame::new(42).payload(b"hello world".to_vec());
        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).unwrap();
        assert_eq!(decoded.stream_id, 42);
        assert_eq!(decoded.payload, b"hello world");
        assert!(decoded.headers.is_empty());
    }

    #[test]
    fn test_frame_with_headers() {
        let frame = Frame::new(1)
            .header_str(":method", "POST")
            .header_str(":path", "/inference.Predict/Run")
            .header_str("content-type", "application/octet-stream")
            .header_str("x-tensor-shape", "1,3,224,224")
            .payload(vec![0xAA; 1024]);

        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).unwrap();

        assert_eq!(decoded.get_header_str(":method"), Some("POST"));
        assert_eq!(decoded.get_header_str(":path"), Some("/inference.Predict/Run"));
        assert_eq!(decoded.get_header_str("content-type"), Some("application/octet-stream"));
        assert_eq!(decoded.get_header_str("x-tensor-shape"), Some("1,3,224,224"));
        assert_eq!(decoded.payload.len(), 1024);
        assert!(decoded.payload.iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn test_frame_flags() {
        let frame = Frame::new(0)
            .flags(flags::END_STREAM | flags::ERROR)
            .header_str(":status", "500")
            .payload(b"internal error".to_vec());

        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).unwrap();

        assert!(decoded.is_end_stream());
        assert!(decoded.is_error());
        assert!(!decoded.is_control());
    }

    #[test]
    fn test_frame_empty() {
        let frame = Frame::new(0);
        let encoded = frame.encode();
        assert_eq!(encoded.len(), FRAME_HEADER_SIZE);
        let decoded = Frame::decode(&encoded).unwrap();
        assert!(decoded.headers.is_empty());
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn test_frame_large_payload() {
        let payload = vec![0xBB; 10 * 1024 * 1024]; // 10 MiB
        let frame = Frame::new(999)
            .header_str(":path", "/model/predict")
            .header_str("x-dtype", "float16")
            .payload(payload.clone());

        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).unwrap();
        assert_eq!(decoded.payload, payload);
        assert_eq!(decoded.stream_id, 999);
    }

    // Backward-compat Message tests
    #[test]
    fn test_message_encode_decode() {
        let msg = Message::request(42, b"hello".to_vec());
        let encoded = msg.encode();
        let decoded = Message::decode(&encoded).unwrap();
        assert_eq!(decoded.request_id, 42);
        assert_eq!(decoded.msg_type, MessageType::Request);
        assert_eq!(decoded.payload, b"hello");
    }

    #[test]
    fn test_message_to_frame() {
        let msg = Message::request(7, b"tensor data".to_vec());
        let frame: Frame = msg.into();
        assert_eq!(frame.get_header_str(":type"), Some("request"));
        assert_eq!(frame.payload, b"tensor data");
    }
}
