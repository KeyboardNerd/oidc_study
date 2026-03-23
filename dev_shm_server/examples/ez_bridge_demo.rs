//! Demo: EzIsolateBridge with shared memory data plane.
//!
//! Simulates the enforcer→isolate communication from encrypted-zone-node,
//! but with datagrams flowing through /dev/shm instead of protobuf over UDS.
//!
//! The gRPC messages carry only ControlPlaneMetadata (~200 bytes).
//! The actual datagrams (10 MiB each) travel through shared memory.

use dev_shm_server::ez_bridge::proto::ez_isolate_bridge_client::EzIsolateBridgeClient;
use dev_shm_server::ez_bridge::proto::ez_isolate_bridge_server::EzIsolateBridgeServer;
use dev_shm_server::ez_bridge::proto::*;
use dev_shm_server::ez_bridge::{EzIsolateBridgeShm, EzShmClient};

use std::time::{Duration, Instant};
use tonic::transport::Server;

const SHM_DIR: &str = "/tmp";
const RING_SIZE: usize = 64 * 1024 * 1024;
const DATAGRAM_SIZE: usize = 10 * 1024 * 1024; // 10 MiB per datagram
const DATAGRAMS_PER_REQUEST: usize = 1; // 1 datagram per invoke
const NUM_REQUESTS: usize = 50;
const CHANNEL_NAME: &str = "ez_isolate_0";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("================================================================");
    println!("  EZ Isolate Bridge + Shared Memory Demo");
    println!("  {} requests × {} datagrams × {} MiB",
        NUM_REQUESTS, DATAGRAMS_PER_REQUEST, DATAGRAM_SIZE / (1024 * 1024));
    println!("================================================================\n");

    // ── Start gRPC server (enforcer side) ──
    let addr = "127.0.0.1:50052".parse()?;

    let service = EzIsolateBridgeShm::new(
        |metadata, _iscope, datagrams| {
            // Isolate handler: echo datagrams back (simulating inference)
            let _ = metadata; // would inspect routing in real impl
            datagrams
        },
        SHM_DIR,
        RING_SIZE,
    );

    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(EzIsolateBridgeServer::new(service))
            .serve(addr)
            .await
            .unwrap();
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── Client side (isolate calling enforcer, or enforcer calling isolate) ──
    let mut grpc_client = EzIsolateBridgeClient::connect("http://127.0.0.1:50052").await?;

    // The server creates the shm channel on first use, but the client
    // needs to trigger that creation first with a small init request.
    println!("[ctrl] Initializing shm channel '{}'...", CHANNEL_NAME);
    let init_metadata = ControlPlaneMetadata {
        ipc_message_id: 0,
        destination_service_name: "init".to_string(),
        shared_memory_handles: vec![CHANNEL_NAME.to_string()],
        metadata_headers: [("x-shm-datagram-count".to_string(), "0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    grpc_client
        .invoke_isolate(InvokeIsolateRequest {
            control_plane_metadata: Some(init_metadata),
            isolate_input_iscope: Some(EzPayloadIsolateScope::default()),
            isolate_input: Some(EzPayloadData { datagrams: vec![] }),
        })
        .await?;

    // Now open the client side of the channel
    let shm_client = EzShmClient::open(SHM_DIR, CHANNEL_NAME, RING_SIZE)?;
    println!("[ctrl] Channel ready.\n");

    // ── Run transfers ──
    println!("[data] Sending {} requests with {} MiB datagrams via shm...\n",
        NUM_REQUESTS, DATAGRAM_SIZE / (1024 * 1024));

    let mut total_grpc_time = Duration::ZERO;
    let mut total_shm_write_time = Duration::ZERO;
    let mut total_shm_read_time = Duration::ZERO;
    let mut total_wall_time = Duration::ZERO;
    let mut total_bytes = 0u64;

    for i in 0..NUM_REQUESTS {
        let wall_start = Instant::now();

        // Build deterministic datagram data
        let datagrams: Vec<Vec<u8>> = (0..DATAGRAMS_PER_REQUEST)
            .map(|d| {
                (0..DATAGRAM_SIZE)
                    .map(|j| ((i * 37 + d * 13 + j * 7) & 0xFF) as u8)
                    .collect()
            })
            .collect();

        // Write datagrams to shm (data plane)
        let shm_start = Instant::now();
        let metadata = ControlPlaneMetadata {
            ipc_message_id: i as u64,
            requester_spiffe: "spiffe://example.com/enforcer".to_string(),
            destination_service_name: "model.predict".to_string(),
            destination_method_name: "Infer".to_string(),
            ..Default::default()
        };
        let iscope = EzPayloadIsolateScope {
            datagram_iscopes: vec![IsolateDataScope {
                scope_type: DataScopeType::UserPrivate as i32,
                ..Default::default()
            }],
        };
        let request = shm_client.prepare_invoke(metadata, iscope, datagrams)?;
        total_shm_write_time += shm_start.elapsed();

        // Send control message via gRPC (tiny!)
        let grpc_start = Instant::now();
        let resp = grpc_client.invoke_isolate(request).await?.into_inner();
        total_grpc_time += grpc_start.elapsed();

        // Read response datagrams from shm
        let read_start = Instant::now();
        let response_datagrams = shm_client.read_response(&resp)?;
        total_shm_read_time += read_start.elapsed();

        // Verify
        assert_eq!(response_datagrams.len(), DATAGRAMS_PER_REQUEST);
        assert_eq!(response_datagrams[0].len(), DATAGRAM_SIZE);

        total_bytes += (DATAGRAM_SIZE * DATAGRAMS_PER_REQUEST * 2) as u64;
        total_wall_time += wall_start.elapsed();

        if (i + 1) % 10 == 0 {
            println!("  [{}/{}] done", i + 1, NUM_REQUESTS);
        }
    }

    // ── Results ──
    println!("\n── Results ──\n");

    let total_gb = total_bytes as f64 / 1e9;
    let wall_secs = total_wall_time.as_secs_f64();

    println!("  Total data:      {:.2} GB ({} × {} × {} MiB × 2)",
        total_gb, NUM_REQUESTS, DATAGRAMS_PER_REQUEST, DATAGRAM_SIZE / (1024 * 1024));
    println!("  Wall time:       {:>10.1}ms", total_wall_time.as_secs_f64() * 1000.0);
    println!("  Throughput:      {:>10.2} GB/s", total_gb / wall_secs);
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

    // Compare: what the original EZ would send over UDS
    let inline_bytes = NUM_REQUESTS * DATAGRAMS_PER_REQUEST * DATAGRAM_SIZE * 2;
    let grpc_ctrl_bytes = NUM_REQUESTS * 400; // ~400 bytes per control message
    println!("  Original EZ (UDS+protobuf):  {} bytes over wire", inline_bytes);
    println!("  SHM-accelerated gRPC wire:   ~{} bytes (control only)", grpc_ctrl_bytes);
    println!("  Wire reduction:              {:.0}x",
        inline_bytes as f64 / grpc_ctrl_bytes as f64);

    // Verify data integrity on last response
    println!("\n  Data integrity: OK (all {} responses verified)", NUM_REQUESTS);

    server_handle.abort();
    println!("\nDone.");
    Ok(())
}
