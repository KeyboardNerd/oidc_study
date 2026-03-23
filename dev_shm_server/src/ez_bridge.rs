//! EZ Isolate Bridge with shared memory data plane.
//!
//! Implements the `EzIsolateBridge` gRPC service from encrypted-zone-node,
//! but routes `EzPayloadData` datagrams through /dev/shm ring buffers
//! instead of serializing them into protobuf messages.
//!
//! Architecture:
//! ```text
//!   Enforcer                              Isolate
//!     │                                     │
//!     │── write datagrams to /dev/shm ────▶│  (data plane: bulk bytes)
//!     │                                     │
//!     │══ gRPC InvokeIsolate ═════════════▶│  (control plane: metadata +
//!     │   ControlPlaneMetadata              │   scope + shm_handles only,
//!     │   + EzPayloadIsolateScope           │   datagrams field is EMPTY)
//!     │   + shared_memory_handles           │
//!     │                                     │── read datagrams from /dev/shm
//!     │                                     │── process
//!     │                                     │── write response datagrams to /dev/shm
//!     │◀═ gRPC InvokeIsolateResponse ══════│  (control plane)
//!     │                                     │
//!     │◀── read response from /dev/shm ────│  (data plane)
//! ```
//!
//! The `ControlPlaneMetadata.shared_memory_handles` field (already in the
//! original proto!) carries the shm channel name so the receiver knows
//! where to read from.

use crate::channel::{ChannelConfig, ShmChannel};
use crate::protocol::{flags, Frame};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub mod proto {
    tonic::include_proto!("enforcer.v1");
}

use proto::ez_isolate_bridge_server::EzIsolateBridge;
use proto::*;
use tonic::{Request, Response, Status};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Handler callback: receives datagrams + metadata, returns response datagrams.
pub type IsolateHandler = Box<
    dyn Fn(&ControlPlaneMetadata, &EzPayloadIsolateScope, Vec<Vec<u8>>) -> Vec<Vec<u8>>
        + Send
        + Sync,
>;

/// gRPC service that implements `EzIsolateBridge` with shm data plane for datagrams.
///
/// The original EZ design sends `EzPayloadData { repeated bytes datagrams }` inside
/// protobuf over UDS. This implementation instead:
/// 1. Writes each datagram as a Frame to the shm ring buffer
/// 2. Sends only metadata + scope + shm handle over gRPC (tiny message)
/// 3. Receiver reads datagrams from shm
///
/// This avoids protobuf serialization overhead for bulk payloads and removes
/// the kernel copy path that UDS requires.
pub struct EzIsolateBridgeShm {
    channels: Arc<Mutex<HashMap<String, ShmChannel>>>,
    shm_dir: PathBuf,
    ring_size: usize,
    handler: Arc<IsolateHandler>,
}

impl EzIsolateBridgeShm {
    pub fn new(
        handler: impl Fn(&ControlPlaneMetadata, &EzPayloadIsolateScope, Vec<Vec<u8>>) -> Vec<Vec<u8>>
            + Send
            + Sync
            + 'static,
        shm_dir: impl Into<PathBuf>,
        ring_size: usize,
    ) -> Self {
        EzIsolateBridgeShm {
            channels: Arc::new(Mutex::new(HashMap::new())),
            shm_dir: shm_dir.into(),
            ring_size,
            handler: Arc::new(Box::new(handler)),
        }
    }

    /// Get or create a shm channel for the given isolate.
    fn get_or_create_channel(&self, name: &str) -> Result<(), Status> {
        let mut channels = self.channels.lock().unwrap();
        if !channels.contains_key(name) {
            let config = ChannelConfig {
                shm_dir: self.shm_dir.clone(),
                name: name.to_string(),
                ring_size: self.ring_size,
            };
            let channel = ShmChannel::create(config)
                .map_err(|e| Status::internal(format!("failed to create shm channel: {}", e)))?;
            channels.insert(name.to_string(), channel);
        }
        Ok(())
    }

    /// Read all datagrams from the shm channel.
    /// Each datagram was sent as a separate Frame with stream_id = datagram index.
    fn read_datagrams(channel: &ShmChannel, count: usize) -> Result<Vec<Vec<u8>>, Status> {
        let mut datagrams = Vec::with_capacity(count);
        for _ in 0..count {
            let frame = channel.recv(DEFAULT_TIMEOUT).map_err(|e| {
                Status::deadline_exceeded(format!("failed to read datagram from shm: {}", e))
            })?;
            datagrams.push(frame.payload);
        }
        Ok(datagrams)
    }

    /// Write datagrams to the shm channel as individual Frames.
    fn write_datagrams(channel: &ShmChannel, datagrams: &[Vec<u8>]) -> Result<(), Status> {
        for (i, datagram) in datagrams.iter().enumerate() {
            let mut frame = Frame::new(i as u32).payload(datagram.clone());
            if i == datagrams.len() - 1 {
                frame = frame.flags(flags::END_STREAM);
            }
            channel.send(&frame, DEFAULT_TIMEOUT).map_err(|e| {
                Status::internal(format!("failed to write datagram to shm: {}", e))
            })?;
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl EzIsolateBridge for EzIsolateBridgeShm {
    async fn invoke_isolate(
        &self,
        request: Request<InvokeIsolateRequest>,
    ) -> Result<Response<InvokeIsolateResponse>, Status> {
        let req = request.into_inner();
        let metadata = req
            .control_plane_metadata
            .ok_or_else(|| Status::invalid_argument("missing control_plane_metadata"))?;
        let iscope = req.isolate_input_iscope.unwrap_or_default();

        // Determine shm channel name from shared_memory_handles
        let channel_name = metadata
            .shared_memory_handles
            .first()
            .ok_or_else(|| {
                Status::invalid_argument(
                    "missing shared_memory_handles — caller must write datagrams \
                     to shm and set the channel name in shared_memory_handles[0]",
                )
            })?
            .clone();

        self.get_or_create_channel(&channel_name)?;

        // Figure out how many datagrams to read.
        // The original proto puts datagrams in EzPayloadData.datagrams,
        // but with shm the caller writes them to shm and sends the count
        // via a metadata header.
        let datagram_count: usize = metadata
            .metadata_headers
            .get("x-shm-datagram-count")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| {
                // Fall back: if datagrams were sent inline (non-shm client),
                // use them directly.
                req.isolate_input
                    .as_ref()
                    .map(|d| d.datagrams.len())
                    .unwrap_or(0)
            });

        let channels = self.channels.lock().unwrap();
        let channel = channels.get(&channel_name).unwrap();

        // Read datagrams from shm (or use inline if count is 0)
        let input_datagrams = if datagram_count > 0 {
            Self::read_datagrams(channel, datagram_count)?
        } else {
            req.isolate_input
                .map(|d| d.datagrams)
                .unwrap_or_default()
        };

        // Dispatch to handler
        let handler = self.handler.clone();
        let response_datagrams = handler(&metadata, &iscope, input_datagrams);

        // Write response datagrams to shm
        let resp_count = response_datagrams.len();
        if resp_count > 0 {
            Self::write_datagrams(channel, &response_datagrams)?;
        }

        drop(channels);

        // Return control-plane-only response (no datagrams in protobuf)
        let mut resp_metadata = ControlPlaneMetadata {
            ipc_message_id: metadata.ipc_message_id,
            shared_memory_handles: vec![channel_name],
            ..Default::default()
        };
        resp_metadata.metadata_headers.insert(
            "x-shm-datagram-count".to_string(),
            resp_count.to_string(),
        );

        Ok(Response::new(InvokeIsolateResponse {
            control_plane_metadata: Some(resp_metadata),
            isolate_output_iscope: Some(EzPayloadIsolateScope::default()),
            isolate_output: Some(EzPayloadData {
                datagrams: vec![], // empty — data is in shm
            }),
            status: Some(IsolateStatus {
                code: 0,
                message: "ok".to_string(),
            }),
        }))
    }

    type StreamInvokeIsolateStream =
        tokio_stream::wrappers::ReceiverStream<Result<InvokeIsolateResponse, Status>>;

    async fn stream_invoke_isolate(
        &self,
        request: Request<tonic::Streaming<InvokeIsolateRequest>>,
    ) -> Result<Response<Self::StreamInvokeIsolateStream>, Status> {
        let mut in_stream = request.into_inner();
        let channels = self.channels.clone();
        let handler = self.handler.clone();
        let shm_dir = self.shm_dir.clone();
        let ring_size = self.ring_size;

        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            while let Some(req) = in_stream.message().await.unwrap_or(None) {
                let metadata = match req.control_plane_metadata {
                    Some(m) => m,
                    None => continue,
                };
                let iscope = req.isolate_input_iscope.unwrap_or_default();

                let channel_name = match metadata.shared_memory_handles.first() {
                    Some(n) => n.clone(),
                    None => continue,
                };

                // Ensure channel exists
                {
                    let mut chans = channels.lock().unwrap();
                    if !chans.contains_key(&channel_name) {
                        let config = ChannelConfig {
                            shm_dir: shm_dir.clone(),
                            name: channel_name.clone(),
                            ring_size,
                        };
                        if let Ok(ch) = ShmChannel::create(config) {
                            chans.insert(channel_name.clone(), ch);
                        } else {
                            continue;
                        }
                    }
                }

                let datagram_count: usize = metadata
                    .metadata_headers
                    .get("x-shm-datagram-count")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);

                let maybe_resp = {
                    let chans = channels.lock().unwrap();
                    let channel = match chans.get(&channel_name) {
                        Some(c) => c,
                        None => continue,
                    };

                    let input_datagrams = if datagram_count > 0 {
                        match EzIsolateBridgeShm::read_datagrams(channel, datagram_count) {
                            Ok(d) => d,
                            Err(_) => continue,
                        }
                    } else {
                        req.isolate_input
                            .map(|d| d.datagrams)
                            .unwrap_or_default()
                    };

                    let response_datagrams = handler(&metadata, &iscope, input_datagrams);
                    let resp_count = response_datagrams.len();
                    if resp_count > 0 {
                        if EzIsolateBridgeShm::write_datagrams(channel, &response_datagrams)
                            .is_err()
                        {
                            continue;
                        }
                    }

                    let mut resp_metadata = ControlPlaneMetadata {
                        ipc_message_id: metadata.ipc_message_id,
                        shared_memory_handles: vec![channel_name.clone()],
                        ..Default::default()
                    };
                    resp_metadata.metadata_headers.insert(
                        "x-shm-datagram-count".to_string(),
                        resp_count.to_string(),
                    );

                    Some(InvokeIsolateResponse {
                        control_plane_metadata: Some(resp_metadata),
                        isolate_output_iscope: Some(EzPayloadIsolateScope::default()),
                        isolate_output: Some(EzPayloadData { datagrams: vec![] }),
                        status: Some(IsolateStatus {
                            code: 0,
                            message: "ok".to_string(),
                        }),
                    })
                };

                if let Some(resp) = maybe_resp {
                    if tx.send(Ok(resp)).await.is_err() {
                        break;
                    }
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn update_isolate_state(
        &self,
        request: Request<UpdateIsolateStateRequest>,
    ) -> Result<Response<UpdateIsolateStateResponse>, Status> {
        let req = request.into_inner();
        // Pass through — state management is orthogonal to data plane
        Ok(Response::new(UpdateIsolateStateResponse {
            current_state: req.move_to_state,
        }))
    }
}

/// Client-side helper: writes datagrams to shm and builds a slim
/// `InvokeIsolateRequest` with only metadata (no inline datagrams).
pub struct EzShmClient {
    channel: ShmChannel,
    channel_name: String,
}

impl EzShmClient {
    /// Open an existing shm channel (created by the server side).
    pub fn open(shm_dir: impl Into<PathBuf>, channel_name: &str, ring_size: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let config = ChannelConfig {
            shm_dir: shm_dir.into(),
            name: channel_name.to_string(),
            ring_size,
        };
        let channel = ShmChannel::open(config)?;
        Ok(EzShmClient {
            channel,
            channel_name: channel_name.to_string(),
        })
    }

    /// Write datagrams to shm and return a request with only control-plane data.
    pub fn prepare_invoke(
        &self,
        metadata: ControlPlaneMetadata,
        iscope: EzPayloadIsolateScope,
        datagrams: Vec<Vec<u8>>,
    ) -> Result<InvokeIsolateRequest, Box<dyn std::error::Error>> {
        let count = datagrams.len();

        // Write each datagram as a Frame to shm
        for (i, datagram) in datagrams.iter().enumerate() {
            let mut frame = Frame::new(i as u32).payload(datagram.clone());
            if i == count - 1 {
                frame = frame.flags(flags::END_STREAM);
            }
            self.channel.send(&frame, DEFAULT_TIMEOUT)?;
        }

        // Build the slim gRPC request: metadata + handles, NO datagrams
        let mut md = metadata;
        md.shared_memory_handles = vec![self.channel_name.clone()];
        md.metadata_headers
            .insert("x-shm-datagram-count".to_string(), count.to_string());

        Ok(InvokeIsolateRequest {
            control_plane_metadata: Some(md),
            isolate_input_iscope: Some(iscope),
            isolate_input: Some(EzPayloadData { datagrams: vec![] }), // empty!
        })
    }

    /// Read response datagrams from shm after getting the gRPC response.
    pub fn read_response(
        &self,
        response: &InvokeIsolateResponse,
    ) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
        let count: usize = response
            .control_plane_metadata
            .as_ref()
            .and_then(|m| m.metadata_headers.get("x-shm-datagram-count"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let mut datagrams = Vec::with_capacity(count);
        for _ in 0..count {
            let frame = self.channel.recv(DEFAULT_TIMEOUT)?;
            datagrams.push(frame.payload);
        }
        Ok(datagrams)
    }
}
