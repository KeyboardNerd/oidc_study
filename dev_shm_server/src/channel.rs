//! High-level shared memory channel for request/response communication.
//!
//! `ShmChannel` wraps two ring buffers (send + receive) backed by shared memory
//! regions and provides a typed API using the `Frame` protocol.
//!
//! This is the building block for the gRPC data plane: the gRPC control plane
//! tells both sides which channel to use, and the channel carries the bulk data.
//!
//! ```text
//!   Client                          Server
//!     │                               │
//!     │── ShmChannel::send(frame) ──▶│  (data plane: /dev/shm ring)
//!     │◀── ShmChannel::recv()  ──────│
//!     │                               │
//!     │== gRPC: "process stream 1" ==▶│  (control plane: tiny protobuf)
//!     │◀= gRPC: "done, status ok" ===│
//! ```

use crate::protocol::Frame;
use crate::ring::RingBuf;
use crate::shm::ShmRegion;

use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// Configuration for creating a channel.
#[derive(Debug, Clone)]
pub struct ChannelConfig {
    /// Directory for shared memory files (e.g., "/dev/shm" or "/tmp").
    pub shm_dir: PathBuf,
    /// Name prefix for the shared memory files.
    pub name: String,
    /// Size of each ring buffer in bytes. Will be rounded to power of 2.
    /// Must be larger than the maximum frame size.
    pub ring_size: usize,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        ChannelConfig {
            shm_dir: if cfg!(target_os = "linux") {
                PathBuf::from("/dev/shm")
            } else {
                PathBuf::from("/tmp")
            },
            name: "shm_channel".to_string(),
            ring_size: 64 * 1024 * 1024, // 64 MiB
        }
    }
}

/// A bidirectional shared memory channel.
///
/// Owns two ShmRegions (send + receive) and provides Frame-level send/recv.
/// The server creates the channel; the client opens it.
pub struct ShmChannel {
    tx_ring: RingBuf,
    rx_ring: RingBuf,
    // Keep regions alive — drop order matters
    _tx_region: ShmRegion,
    _rx_region: ShmRegion,
    config: ChannelConfig,
    is_server: bool,
}

impl ShmChannel {
    /// Create a new channel (server side). Initializes the ring buffers.
    pub fn create(config: ChannelConfig) -> io::Result<Self> {
        let tx_name = format!("{}_s2c", config.name); // server-to-client
        let rx_name = format!("{}_c2s", config.name); // client-to-server

        let mut tx_region = ShmRegion::create(&config.shm_dir, &tx_name, config.ring_size)?;
        let mut rx_region = ShmRegion::create(&config.shm_dir, &rx_name, config.ring_size)?;

        let tx_ring = unsafe { RingBuf::new(tx_region.as_mut_ptr(), tx_region.len()) };
        let rx_ring = unsafe { RingBuf::new(rx_region.as_mut_ptr(), rx_region.len()) };

        tx_ring.init();
        rx_ring.init();

        Ok(ShmChannel {
            tx_ring,
            rx_ring,
            _tx_region: tx_region,
            _rx_region: rx_region,
            config,
            is_server: true,
        })
    }

    /// Open an existing channel (client side). Does NOT initialize rings.
    pub fn open(config: ChannelConfig) -> io::Result<Self> {
        // Client's TX is server's RX and vice versa
        let tx_name = format!("{}_c2s", config.name); // client writes to c2s
        let rx_name = format!("{}_s2c", config.name); // client reads from s2c

        let mut tx_region = ShmRegion::open(&config.shm_dir, &tx_name)?;
        let mut rx_region = ShmRegion::open(&config.shm_dir, &rx_name)?;

        let tx_ring = unsafe { RingBuf::new(tx_region.as_mut_ptr(), tx_region.len()) };
        let rx_ring = unsafe { RingBuf::new(rx_region.as_mut_ptr(), rx_region.len()) };

        Ok(ShmChannel {
            tx_ring,
            rx_ring,
            _tx_region: tx_region,
            _rx_region: rx_region,
            config,
            is_server: false,
        })
    }

    /// Send a frame. Spins until space is available or timeout.
    pub fn send(&self, frame: &Frame, timeout: Duration) -> io::Result<()> {
        let encoded = frame.encode();
        let deadline = Instant::now() + timeout;

        loop {
            if self.tx_ring.push(&encoded) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "send timeout: ring buffer full",
                ));
            }
            thread::yield_now();
        }
    }

    /// Try to send a frame without blocking. Returns false if ring is full.
    pub fn try_send(&self, frame: &Frame) -> bool {
        let encoded = frame.encode();
        self.tx_ring.push(&encoded)
    }

    /// Receive a frame. Spins until data is available or timeout.
    pub fn recv(&self, timeout: Duration) -> io::Result<Frame> {
        let deadline = Instant::now() + timeout;
        let mut buf = Vec::new();

        loop {
            if self.rx_ring.pop(&mut buf) {
                return Frame::decode(&buf).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
                });
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "recv timeout: no data",
                ));
            }
            thread::yield_now();
        }
    }

    /// Try to receive a frame without blocking. Returns None if empty.
    pub fn try_recv(&self) -> Option<Frame> {
        let mut buf = Vec::new();
        if self.rx_ring.pop(&mut buf) {
            Frame::decode(&buf).ok()
        } else {
            None
        }
    }

    /// Send raw pre-encoded bytes directly (avoids double-encode for forwarding).
    pub fn send_raw(&self, data: &[u8], timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.tx_ring.push(data) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "send_raw timeout"));
            }
            thread::yield_now();
        }
    }

    /// Available bytes to read.
    pub fn available(&self) -> usize {
        self.rx_ring.available()
    }

    /// Free space in the send ring.
    pub fn send_free(&self) -> usize {
        self.tx_ring.free_space()
    }

    /// Ring buffer capacity.
    pub fn capacity(&self) -> usize {
        self.tx_ring.capacity()
    }

    /// The shm directory this channel uses.
    pub fn shm_dir(&self) -> &Path {
        &self.config.shm_dir
    }

    /// The channel name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Clean up shared memory files. Call when done (server side).
    pub fn unlink(&self) -> io::Result<()> {
        let s2c = self.config.shm_dir.join(format!("{}_s2c", self.config.name));
        let c2s = self.config.shm_dir.join(format!("{}_c2s", self.config.name));
        let r1 = std::fs::remove_file(&s2c);
        let r2 = std::fs::remove_file(&c2s);
        r1.and(r2)
    }
}

impl Drop for ShmChannel {
    fn drop(&mut self) {
        // Server cleans up files on drop
        if self.is_server {
            let _ = self.unlink();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::flags;

    fn test_config(name: &str) -> ChannelConfig {
        ChannelConfig {
            shm_dir: PathBuf::from("/tmp"),
            name: name.to_string(),
            ring_size: 1024 * 1024, // 1 MiB
        }
    }

    #[test]
    fn test_channel_create_open_roundtrip() {
        let config = test_config("test_chan_rt");
        let server = ShmChannel::create(config.clone()).unwrap();
        let client = ShmChannel::open(config).unwrap();

        // Client sends, server receives
        let frame = Frame::new(1)
            .header_str(":method", "POST")
            .header_str(":path", "/model/predict")
            .payload(b"tensor data here".to_vec());

        client.send(&frame, Duration::from_secs(1)).unwrap();
        let received = server.recv(Duration::from_secs(1)).unwrap();

        assert_eq!(received.stream_id, 1);
        assert_eq!(received.get_header_str(":method"), Some("POST"));
        assert_eq!(received.get_header_str(":path"), Some("/model/predict"));
        assert_eq!(received.payload, b"tensor data here");

        // Server responds
        let resp = Frame::new(1)
            .flags(flags::END_STREAM)
            .header_str(":status", "200")
            .payload(b"result".to_vec());

        server.send(&resp, Duration::from_secs(1)).unwrap();
        let got = client.recv(Duration::from_secs(1)).unwrap();

        assert!(got.is_end_stream());
        assert_eq!(got.get_header_str(":status"), Some("200"));
        assert_eq!(got.payload, b"result");
    }

    #[test]
    fn test_channel_large_payload() {
        let config = test_config("test_chan_large");
        let server = ShmChannel::create(config.clone()).unwrap();
        let client = ShmChannel::open(config).unwrap();

        let payload = vec![0xAA; 100_000];
        let frame = Frame::new(7)
            .header_str("x-dtype", "float32")
            .header_str("x-shape", "1,3,224,224")
            .payload(payload.clone());

        client.send(&frame, Duration::from_secs(1)).unwrap();
        let received = server.recv(Duration::from_secs(1)).unwrap();

        assert_eq!(received.payload.len(), 100_000);
        assert_eq!(received.payload, payload);
        assert_eq!(received.get_header_str("x-dtype"), Some("float32"));
    }

    #[test]
    fn test_channel_timeout() {
        let config = test_config("test_chan_timeout");
        let _server = ShmChannel::create(config.clone()).unwrap();
        let client = ShmChannel::open(config).unwrap();

        // No data — should timeout
        let result = client.recv(Duration::from_millis(50));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn test_channel_multiple_streams() {
        let config = test_config("test_chan_multi");
        let server = ShmChannel::create(config.clone()).unwrap();
        let client = ShmChannel::open(config).unwrap();

        // Send frames on different streams
        for stream_id in 0..10 {
            let frame = Frame::new(stream_id)
                .header_str(":path", &format!("/stream/{}", stream_id))
                .payload(format!("data-{}", stream_id).into_bytes());
            client.send(&frame, Duration::from_secs(1)).unwrap();
        }

        // Receive and verify ordering + content
        for stream_id in 0..10 {
            let frame = server.recv(Duration::from_secs(1)).unwrap();
            assert_eq!(frame.stream_id, stream_id);
            assert_eq!(
                frame.get_header_str(":path"),
                Some(format!("/stream/{}", stream_id).as_str())
            );
            assert_eq!(frame.payload, format!("data-{}", stream_id).as_bytes());
        }
    }
}
