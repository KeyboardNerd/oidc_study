//! gRPC service implementation for shared memory transfer.
//!
//! Architecture:
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │  gRPC (tonic) — control plane                            │
//! │  Small protobuf messages: channel setup, send notify,    │
//! │  stream coordination, teardown                           │
//! ├──────────────────────────────────────────────────────────┤
//! │  ShmChannel — data plane                                 │
//! │  Ring buffer on /dev/shm with Frame protocol:            │
//! │  extensible headers + raw bulk payload (no protobuf)     │
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! The gRPC service manages channel lifecycle. Actual data flows through
//! `/dev/shm` ring buffers — the protobuf messages carry only metadata.

use crate::channel::{ChannelConfig, ShmChannel};
use crate::protocol::Frame;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub mod proto {
    tonic::include_proto!("shm_transfer");
}

use proto::shm_transfer_server::ShmTransfer;
use proto::*;
use tonic::{Request, Response, Status};

/// Default timeout for shm operations.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Handler callback type: receives a Frame, returns a response Frame.
pub type RequestHandler = Box<dyn Fn(Frame) -> Frame + Send + Sync>;

/// The gRPC service that manages shared memory channels and dispatches requests.
pub struct ShmTransferService {
    channels: Arc<Mutex<HashMap<String, ShmChannel>>>,
    default_shm_dir: PathBuf,
    default_ring_size: usize,
    handler: Arc<RequestHandler>,
}

impl ShmTransferService {
    pub fn new(handler: impl Fn(Frame) -> Frame + Send + Sync + 'static) -> Self {
        ShmTransferService {
            channels: Arc::new(Mutex::new(HashMap::new())),
            default_shm_dir: if cfg!(target_os = "linux") {
                PathBuf::from("/dev/shm")
            } else {
                PathBuf::from("/tmp")
            },
            default_ring_size: 64 * 1024 * 1024,
            handler: Arc::new(Box::new(handler)),
        }
    }

    pub fn with_defaults(
        mut self,
        shm_dir: impl Into<PathBuf>,
        ring_size: usize,
    ) -> Self {
        self.default_shm_dir = shm_dir.into();
        self.default_ring_size = ring_size;
        self
    }

    fn get_channel(&self, name: &str) -> Result<(), Status> {
        let channels = self.channels.lock().unwrap();
        if channels.contains_key(name) {
            Ok(())
        } else {
            Err(Status::not_found(format!("channel '{}' not found", name)))
        }
    }
}

#[tonic::async_trait]
impl ShmTransfer for ShmTransferService {
    async fn create_channel(
        &self,
        request: Request<CreateChannelRequest>,
    ) -> Result<Response<CreateChannelResponse>, Status> {
        let req = request.into_inner();

        let shm_dir = if req.shm_dir.is_empty() {
            self.default_shm_dir.clone()
        } else {
            PathBuf::from(&req.shm_dir)
        };

        let ring_size = if req.ring_size == 0 {
            self.default_ring_size
        } else {
            req.ring_size as usize
        };

        let config = ChannelConfig {
            shm_dir: shm_dir.clone(),
            name: req.name.clone(),
            ring_size,
        };

        let channel = ShmChannel::create(config).map_err(|e| {
            Status::internal(format!("failed to create channel: {}", e))
        })?;

        let c2s_path = shm_dir.join(format!("{}_c2s", req.name));
        let s2c_path = shm_dir.join(format!("{}_s2c", req.name));

        let mut channels = self.channels.lock().unwrap();
        channels.insert(req.name.clone(), channel);

        Ok(Response::new(CreateChannelResponse {
            name: req.name,
            shm_dir: shm_dir.to_string_lossy().to_string(),
            ring_size: ring_size as u64,
            client_to_server_path: c2s_path.to_string_lossy().to_string(),
            server_to_client_path: s2c_path.to_string_lossy().to_string(),
        }))
    }

    async fn send(
        &self,
        request: Request<SendRequest>,
    ) -> Result<Response<SendResponse>, Status> {
        let req = request.into_inner();
        self.get_channel(&req.channel_name)?;

        // Read the frame from the shm data plane
        let channels = self.channels.lock().unwrap();
        let channel = channels.get(&req.channel_name).unwrap();

        let frame = channel.recv(DEFAULT_TIMEOUT).map_err(|e| {
            Status::deadline_exceeded(format!("failed to read from shm: {}", e))
        })?;

        // Dispatch to handler
        let handler = self.handler.clone();
        let response_frame = handler(frame);
        let response_size = response_frame.payload.len() as u64;

        // Write response to shm data plane
        channel.send(&response_frame, DEFAULT_TIMEOUT).map_err(|e| {
            Status::internal(format!("failed to write response to shm: {}", e))
        })?;

        Ok(Response::new(SendResponse {
            stream_id: req.stream_id,
            status: 0,
            message: "ok".to_string(),
            response_payload_size: response_size,
        }))
    }

    type StreamStream = tokio_stream::wrappers::ReceiverStream<Result<StreamMessage, Status>>;

    async fn stream(
        &self,
        request: Request<tonic::Streaming<StreamMessage>>,
    ) -> Result<Response<Self::StreamStream>, Status> {
        let mut in_stream = request.into_inner();
        let channels = self.channels.clone();
        let handler = self.handler.clone();

        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            while let Some(msg) = in_stream.message().await.unwrap_or(None) {
                let channel_name = msg
                    .headers
                    .get("channel")
                    .and_then(|v| String::from_utf8(v.clone()).ok())
                    .unwrap_or_default();

                // Scope the lock so it's dropped before the .await
                let maybe_resp = {
                    let chans = channels.lock().unwrap();
                    if let Some(channel) = chans.get(&channel_name) {
                        if let Ok(frame) = channel.recv(DEFAULT_TIMEOUT) {
                            let response_frame = handler(frame);
                            let resp_size = response_frame.payload.len() as u64;
                            let _ = channel.send(&response_frame, DEFAULT_TIMEOUT);
                            Some(StreamMessage {
                                stream_id: msg.stream_id,
                                headers: HashMap::new(),
                                payload_size: resp_size,
                                end_stream: msg.end_stream,
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                if let Some(resp) = maybe_resp {
                    if tx.send(Ok(resp)).await.is_err() {
                        break;
                    }
                }

                if msg.end_stream {
                    break;
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn destroy_channel(
        &self,
        request: Request<DestroyChannelRequest>,
    ) -> Result<Response<DestroyChannelResponse>, Status> {
        let name = request.into_inner().channel_name;
        let mut channels = self.channels.lock().unwrap();

        if channels.remove(&name).is_some() {
            // ShmChannel::drop will unlink the files
            Ok(Response::new(DestroyChannelResponse { ok: true }))
        } else {
            Ok(Response::new(DestroyChannelResponse { ok: false }))
        }
    }
}
