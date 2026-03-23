//! Example: gRPC server + client transferring 10 MiB "tensors" via shared memory.
//!
//! The gRPC channel carries tiny control messages (~100 bytes).
//! The actual 10 MiB payload travels through /dev/shm ring buffers.
//!
//! Architecture:
//!   Client                              Server
//!     │                                   │
//!     │─── write 10 MiB to /dev/shm ───▶ │  (data plane)
//!     │                                   │
//!     │=== gRPC Send("data ready") ═════▶│  (control plane, ~100 bytes)
//!     │                                   │─── read 10 MiB from /dev/shm
//!     │                                   │─── process (echo in this example)
//!     │                                   │─── write response to /dev/shm
//!     │◀══ gRPC Response("done") ════════│  (control plane)
//!     │                                   │
//!     │◀── read response from /dev/shm ──│  (data plane)

use dev_shm_server::channel::{ChannelConfig, ShmChannel};
use dev_shm_server::protocol::{flags, Frame};
use dev_shm_server::service::proto::shm_transfer_client::ShmTransferClient;
use dev_shm_server::service::proto::shm_transfer_server::ShmTransferServer;
use dev_shm_server::service::proto::*;
use dev_shm_server::service::ShmTransferService;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tonic::transport::Server;

const CHANNEL_NAME: &str = "tensor_pipe";
const SHM_DIR: &str = "/tmp";
const RING_SIZE: usize = 64 * 1024 * 1024;
const TENSOR_SIZE: usize = 10 * 1024 * 1024; // 10 MiB
const NUM_TRANSFERS: usize = 50;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("================================================================");
    println!("  gRPC + Shared Memory Transfer Demo");
    println!("  {} x {} MiB tensor transfers", NUM_TRANSFERS, TENSOR_SIZE / (1024 * 1024));
    println!("================================================================\n");

    // ── Start gRPC server ──
    let addr = "127.0.0.1:50051".parse()?;

    let service = ShmTransferService::new(|frame: Frame| {
        // Handler: echo back with a response header
        Frame::new(frame.stream_id)
            .flags(flags::END_STREAM)
            .header_str(":status", "200")
            .header_str("x-processed", "true")
            .payload(frame.payload)
    })
    .with_defaults(SHM_DIR, RING_SIZE);

    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(ShmTransferServer::new(service))
            .serve(addr)
            .await
            .unwrap();
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── Client side ──
    let mut grpc_client = ShmTransferClient::connect("http://127.0.0.1:50051").await?;

    // Step 1: Create channel via gRPC (control plane)
    println!("[ctrl] Creating shared memory channel...");
    let create_resp = grpc_client
        .create_channel(CreateChannelRequest {
            name: CHANNEL_NAME.to_string(),
            ring_size: RING_SIZE as u64,
            shm_dir: SHM_DIR.to_string(),
        })
        .await?
        .into_inner();

    println!("[ctrl] Channel created:");
    println!("  c2s: {}", create_resp.client_to_server_path);
    println!("  s2c: {}", create_resp.server_to_client_path);
    println!("  ring: {} MiB\n", create_resp.ring_size / (1024 * 1024));

    // Step 2: Open the shm channel (data plane)
    let shm_channel = ShmChannel::open(ChannelConfig {
        shm_dir: PathBuf::from(SHM_DIR),
        name: CHANNEL_NAME.to_string(),
        ring_size: RING_SIZE,
    })?;

    // Step 3: Transfer tensors
    println!("[data] Sending {} tensors of {} MiB each...\n", NUM_TRANSFERS, TENSOR_SIZE / (1024 * 1024));

    let mut total_grpc_time = Duration::ZERO;
    let mut total_shm_write_time = Duration::ZERO;
    let mut total_shm_read_time = Duration::ZERO;
    let mut total_wall_time = Duration::ZERO;
    let mut total_bytes = 0u64;

    for i in 0..NUM_TRANSFERS {
        let wall_start = Instant::now();

        // Build a "tensor" with deterministic data
        let tensor_data: Vec<u8> = (0..TENSOR_SIZE)
            .map(|j| ((i * 37 + j * 13) & 0xFF) as u8)
            .collect();

        // Write tensor to shm (data plane)
        let shm_start = Instant::now();
        let frame = Frame::new(i as u32)
            .header_str(":method", "POST")
            .header_str(":path", "/model/predict")
            .header_str("x-tensor-dtype", "float16")
            .header_str("x-tensor-shape", "1,3,224,224")
            .header_str("x-request-id", &format!("req-{}", i))
            .payload(tensor_data);

        shm_channel
            .send(&frame, Duration::from_secs(5))
            .expect("shm send failed");
        total_shm_write_time += shm_start.elapsed();

        // Notify server via gRPC (control plane — tiny message!)
        let grpc_start = Instant::now();
        let _resp = grpc_client
            .send(SendRequest {
                channel_name: CHANNEL_NAME.to_string(),
                stream_id: i as u32,
                headers: HashMap::new(),
                payload_size: TENSOR_SIZE as u64,
            })
            .await?
            .into_inner();
        total_grpc_time += grpc_start.elapsed();

        // Read response from shm (data plane)
        let read_start = Instant::now();
        let response = shm_channel
            .recv(Duration::from_secs(5))
            .expect("shm recv failed");
        total_shm_read_time += read_start.elapsed();

        // Verify
        assert_eq!(response.payload.len(), TENSOR_SIZE);
        assert_eq!(response.get_header_str(":status"), Some("200"));
        assert_eq!(response.get_header_str("x-processed"), Some("true"));

        total_bytes += TENSOR_SIZE as u64 * 2; // send + receive
        total_wall_time += wall_start.elapsed();

        if (i + 1) % 10 == 0 {
            println!("  [{}/{}] done", i + 1, NUM_TRANSFERS);
        }
    }

    // ── Results ──
    println!("\n── Results ──\n");

    let total_payload_gb = total_bytes as f64 / 1e9;
    let wall_secs = total_wall_time.as_secs_f64();

    println!("  Total data transferred: {:.2} GB ({} roundtrips × {} MiB × 2)",
        total_payload_gb, NUM_TRANSFERS, TENSOR_SIZE / (1024 * 1024));
    println!("  Wall time:       {:>10.1}ms", total_wall_time.as_secs_f64() * 1000.0);
    println!("  Throughput:      {:>10.2} GB/s", total_payload_gb / wall_secs);
    println!();
    println!("  Time breakdown:");
    println!("    SHM write:     {:>10.1}ms  ({:.1}%)",
        total_shm_write_time.as_secs_f64() * 1000.0,
        total_shm_write_time.as_secs_f64() / wall_secs * 100.0);
    println!("    SHM read:      {:>10.1}ms  ({:.1}%)",
        total_shm_read_time.as_secs_f64() * 1000.0,
        total_shm_read_time.as_secs_f64() / wall_secs * 100.0);
    println!("    gRPC control:  {:>10.1}ms  ({:.1}%)",
        total_grpc_time.as_secs_f64() * 1000.0,
        total_grpc_time.as_secs_f64() / wall_secs * 100.0);
    println!();

    let grpc_bytes = NUM_TRANSFERS * 200; // ~200 bytes per gRPC roundtrip
    println!("  gRPC wire bytes: ~{} bytes ({} × ~200B control messages)",
        grpc_bytes, NUM_TRANSFERS);
    println!("  SHM data bytes:  {} bytes", total_bytes);
    println!("  Ratio:           {:.0}x less data over gRPC",
        total_bytes as f64 / grpc_bytes as f64);

    // Cleanup
    grpc_client
        .destroy_channel(DestroyChannelRequest {
            channel_name: CHANNEL_NAME.to_string(),
        })
        .await?;

    println!("\n[ctrl] Channel destroyed. Done.");

    server_handle.abort();
    Ok(())
}
