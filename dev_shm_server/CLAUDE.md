# CLAUDE.md — dev_shm_server

Shared memory data plane library for high-throughput IPC. Replaces socket-based
data transfer with `/dev/shm` ring buffers while keeping gRPC as the control plane.

## Quick reference

```bash
cargo build --release          # build everything
cargo test --release           # 19 unit tests (ring, protocol, channel)
cargo run --release --example grpc_transfer    # generic gRPC+shm demo (50×10MiB)
cargo run --release --example ez_bridge_demo   # EZ isolate bridge demo (50×10MiB)
cargo run --release --example bench            # SHM vs UDS vs TCP benchmarks
cargo run --release --example resource_bench   # RAM/CPU measurement
```

Requires `protoc` installed (`brew install protobuf` on macOS).

## Architecture

Two-plane design: small control messages over gRPC, bulk data over shared memory.

```
Client                              Server
  │                                   │
  │── write payload to /dev/shm ────▶│  data plane  (memcpy, zero-kernel-copy)
  │                                   │
  │══ gRPC "data ready" ═══════════▶│  control plane (~200 bytes protobuf)
  │◀═ gRPC "done" ═════════════════│
  │                                   │
  │◀── read response from /dev/shm ──│  data plane
```

## Module map

| Module | File | Purpose |
|--------|------|---------|
| `ring` | `src/ring.rs` | Lock-free SPSC ring buffer. Power-of-2 capacity, bulk memcpy, cache-line-padded atomics. The lowest-level primitive — everything else builds on this. |
| `shm` | `src/shm.rs` | `ShmRegion` — creates/opens memory-mapped files on tmpfs. Thin wrapper over `memmap2`. |
| `protocol` | `src/protocol.rs` | Wire format. `Frame` (extensible TLV headers + raw payload, 14-byte fixed overhead) and legacy `Message` (13-byte fixed header). |
| `channel` | `src/channel.rs` | `ShmChannel` — bidirectional channel (two rings: `{name}_c2s` + `{name}_s2c`). Server creates, client opens. Frame-level `send()`/`recv()` with timeout. |
| `service` | `src/service.rs` | Generic `ShmTransferService` — tonic gRPC service with pluggable `Fn(Frame) -> Frame` handler. Manages channel lifecycle via `CreateChannel`/`DestroyChannel` RPCs. |
| `ez_bridge` | `src/ez_bridge.rs` | `EzIsolateBridgeShm` — implements the `EzIsolateBridge` gRPC service from encrypted-zone-node, routing `EzPayloadData.datagrams` through shm. Includes `EzShmClient` helper. |

### Dependency graph

```
ring  ←  channel  ←  service      (generic gRPC data plane)
  ↑        ↑
  shm      └──────  ez_bridge     (encrypted-zone-node integration)
                        ↑
                   proto/enforcer/v1/*.proto
protocol (Frame)  ←  channel, service, ez_bridge
```

## Proto files

- `proto/shm_transfer.proto` — Generic shm transfer service (CreateChannel, Send, Stream, DestroyChannel).
- `proto/enforcer/v1/*.proto` — Copied from [encrypted-zone-node](https://github.com/private-compute-infra-toolkit/encrypted-zone-node). EzIsolateBridge and IsolateEzBridge services. **Do not modify these** — they mirror upstream.

Proto compilation is in `build.rs`. Generated code lands in `target/*/build/dev_shm_server-*/out/`.

## Key types and how to use them

### Ring buffer (`ring::RingBuf`)

Lowest level. You almost never use this directly — use `ShmChannel` instead.

```rust
// Created internally by ShmChannel
let ring = unsafe { RingBuf::new(ptr, total_size) };
ring.init(); // zero positions — call once on the creator side
ring.push(data); // returns false if full
ring.pop(&mut buf); // returns false if empty
```

- SPSC only (single producer, single consumer).
- Capacity is rounded down to the nearest power of 2.
- Messages are length-prefixed (4-byte LE). Max message size = capacity - 4.

### Channel (`channel::ShmChannel`)

The primary API for data transfer.

```rust
// Server side — creates shm files
let server = ShmChannel::create(ChannelConfig {
    shm_dir: PathBuf::from("/dev/shm"),
    name: "my_channel".to_string(),
    ring_size: 64 * 1024 * 1024,
})?;

// Client side — opens existing files
let client = ShmChannel::open(config)?;

// Transfer frames
client.send(&frame, Duration::from_secs(5))?;
let response = server.recv(Duration::from_secs(5))?;
```

- Creates two files: `{shm_dir}/{name}_c2s` and `{shm_dir}/{name}_s2c`.
- Server unlinks files on drop.
- `send()`/`recv()` spin with `thread::yield_now()` until timeout.

### Frame (`protocol::Frame`)

The wire format for all data through channels.

```rust
let frame = Frame::new(stream_id)
    .flags(flags::END_STREAM)
    .header_str(":method", "POST")
    .header_str("x-tensor-shape", "1,3,224,224")
    .payload(tensor_bytes);

let encoded = frame.encode();       // Vec<u8>
let decoded = Frame::decode(&buf)?;  // Frame
decoded.get_header_str(":method");   // Option<&str>
```

- 14-byte fixed overhead + TLV headers + raw payload.
- Flags: `END_STREAM` (0x01), `COMPRESSED` (0x02), `ERROR` (0x04), `CONTROL` (0x08).
- Payload is raw bytes — never protobuf-serialized.

### Generic gRPC service (`service::ShmTransferService`)

```rust
let service = ShmTransferService::new(|frame: Frame| {
    // Process request frame, return response frame
    Frame::new(frame.stream_id)
        .header_str(":status", "200")
        .payload(frame.payload)
})
.with_defaults("/dev/shm", 64 * 1024 * 1024);

Server::builder()
    .add_service(ShmTransferServer::new(service))
    .serve(addr).await?;
```

Flow: client calls `CreateChannel` → writes data to shm → calls `Send` (tiny gRPC) → server reads from shm, calls handler, writes response to shm → client reads response from shm.

### EZ bridge (`ez_bridge::EzIsolateBridgeShm`)

Drop-in implementation of the `EzIsolateBridge` gRPC service from encrypted-zone-node.

```rust
let service = EzIsolateBridgeShm::new(
    |metadata, iscope, datagrams: Vec<Vec<u8>>| {
        // Process datagrams, return response datagrams
        datagrams  // echo
    },
    "/dev/shm",
    64 * 1024 * 1024,
);
```

Convention for shm-mode requests:
- `ControlPlaneMetadata.shared_memory_handles[0]` = shm channel name.
- `ControlPlaneMetadata.metadata_headers["x-shm-datagram-count"]` = number of frames in shm.
- `EzPayloadData.datagrams` is left **empty** — data is in shm.

Client-side helper:

```rust
let shm_client = EzShmClient::open("/dev/shm", "ez_isolate_0", ring_size)?;
let request = shm_client.prepare_invoke(metadata, iscope, datagrams)?;
let response = grpc_client.invoke_isolate(request).await?;
let result_datagrams = shm_client.read_response(&response.into_inner())?;
```

## Integration patterns

### Adding a new gRPC service that uses shm

1. Define your `.proto` in `proto/`. Add it to `build.rs` `compile_protos()`.
2. Create a module in `src/` that `tonic::include_proto!()` your package.
3. In your service impl, use `ShmChannel` for bulk data. Keep only metadata in protobuf.
4. Pattern: server manages `HashMap<String, ShmChannel>`. Channel name comes from the client via a protobuf field. Server creates on first use, client opens.

### Using as a library from another crate

```toml
[dependencies]
dev_shm_server = { path = "../dev_shm_server" }
```

Available modules: `ring`, `shm`, `protocol`, `channel`, `service`, `ez_bridge`.

### Container deployment

For `/dev/shm` between containers:
```yaml
# docker-compose.yml
services:
  server:
    ipc: host  # or shm_size: '256m' for isolated /dev/shm
  client:
    ipc: host
    volumes:
      - /dev/shm:/dev/shm  # alternative: explicit mount
```

On macOS (no `/dev/shm`), the library falls back to `/tmp`. This works for development but is not tmpfs-backed — performance will differ from Linux.

## Constraints and gotchas

- **SPSC only**: Each ring buffer supports exactly one writer and one reader. Multiple concurrent writers will corrupt data. Use one `ShmChannel` per producer-consumer pair.
- **Spin-wait**: `send()`/`recv()` busy-loop until timeout. No sleep, no futex. This burns CPU when idle.
- **No crash recovery**: If a process dies mid-write, the ring may contain partial frames. The other side will timeout or read garbage. There is no sequence number or checksum.
- **Power-of-2 sizing**: `ring_size` is rounded down. A 100 MiB request becomes 64 MiB usable capacity. Size the ring at least 2x your max frame size.
- **Max frame size**: Must fit in the ring in one piece (capacity - 4 bytes for the length prefix). A 64 MiB ring holds at most a ~64 MiB frame minus overhead.
- **File cleanup**: Server-side `ShmChannel::drop` unlinks files. If the server crashes, files persist in `/dev/shm` and consume RAM until manually removed.
- **macOS vs Linux**: On macOS, `/tmp` is used instead of `/dev/shm`. The mmap is file-backed, not tmpfs. Benchmarks on macOS do not reflect Linux production performance.

See `CAVEATS.md` for security risks specific to the EZ integration (isolate containment, TOCTOU, scope enforcement gaps).

## Test structure

All tests are in-module (`#[cfg(test)]`):
- `ring::tests` — 8 tests: roundtrip, wrap-around, sustained integrity (50K msgs), large payloads (100 KiB), power-of-2 rounding.
- `protocol::tests` — 6 tests: Frame encode/decode, headers, flags, legacy Message compat.
- `channel::tests` — 4 tests: create/open roundtrip, large payloads, timeouts, multi-stream ordering.
- Examples double as integration tests: `bench`, `grpc_transfer`, `ez_bridge_demo` all verify data integrity.
