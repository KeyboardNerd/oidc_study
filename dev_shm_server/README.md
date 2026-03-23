# dev_shm_server

Shared memory data plane library for high-throughput IPC. Replaces socket-based data transfer with `/dev/shm` ring buffers while keeping gRPC as the control plane.

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

### Module map

| Module | File | Purpose |
|--------|------|---------|
| `ring` | `src/ring.rs` | Lock-free SPSC ring buffer (power-of-2, cache-line-padded atomics) |
| `shm` | `src/shm.rs` | `ShmRegion` — memory-mapped files on tmpfs via `memmap2` |
| `protocol` | `src/protocol.rs` | `Frame` (extensible TLV headers + raw payload, 14-byte overhead) |
| `channel` | `src/channel.rs` | `ShmChannel` — bidirectional channel (two rings: `{name}_c2s` + `{name}_s2c`) |
| `service` | `src/service.rs` | `ShmTransferService` — tonic gRPC service with pluggable handler |
| `ez_bridge` | `src/ez_bridge.rs` | `EzIsolateBridgeShm` — encrypted-zone-node integration |

## Quick start

```bash
cargo build --release
cargo test --release              # 19 unit tests
cargo run --release --example bench            # SHM vs UDS vs TCP benchmarks
cargo run --release --example resource_bench   # RAM/CPU measurement
cargo run --release --example grpc_transfer    # gRPC + shm demo (50x10MiB)
cargo run --release --example ez_bridge_demo   # EZ isolate bridge demo
```

Requires `protoc` (`brew install protobuf` on macOS).

## Performance

Benchmarked on macOS (Apple Silicon). On Linux with real `/dev/shm` (tmpfs-backed), SHM performance will be higher.

### Throughput (unidirectional, every message integrity-verified)

| Payload | SHM | UDS | TCP | SHM/UDS | SHM/TCP |
|---------|-----|-----|-----|---------|---------|
| 64 B | 440 MB/s | 33 MB/s | 11 MB/s | **13.2x** | **39.5x** |
| 1 KiB | 2.76 GB/s | 558 MB/s | 168 MB/s | **4.9x** | **16.4x** |
| 64 KiB | 3.94 GB/s | 682 MB/s | 879 MB/s | **5.8x** | **4.5x** |
| 256 KiB | 4.18 GB/s | 830 MB/s | 1.62 GB/s | **5.0x** | **2.6x** |
| 1 MiB | 4.26 GB/s | 1.00 GB/s | 2.44 GB/s | **4.3x** | **1.7x** |
| 10 MiB | 2.59 GB/s | 912 MB/s | 1.84 GB/s | **2.8x** | **1.4x** |

Peak throughput: **4.26 GB/s** at 1 MiB payloads (4.3x faster than UDS).

### Latency (roundtrip echo, byte-for-byte verified)

| Payload | SHM p50 | SHM p99 | UDS p50 | UDS p99 | TCP p50 | TCP p99 |
|---------|---------|---------|---------|---------|---------|---------|
| 64 B | 625 ns | 958 ns | 6.5 us | 13.0 us | 17.5 us | 52.1 us |
| 1 KiB | 834 ns | 2.3 us | 7.1 us | 16.3 us | 17.8 us | 52.4 us |
| 64 KiB | 6.9 us | 13.9 us | 73.8 us | 144.9 us | 37.2 us | 77.6 us |
| 1 MiB | 92.8 us | 135.1 us | 1.03 ms | 2.14 ms | 201.5 us | 500.7 us |
| 10 MiB | 922 us | 1.65 ms | 14.9 ms | 21.8 ms | 3.06 ms | 5.58 ms |

Sub-microsecond p50 latency for small messages. **10x lower** than UDS at 64 B, **16x lower** at 10 MiB.

### Resource usage (roundtrip, ~256 MiB total data)

| Metric | SHM | UDS | TCP |
|--------|-----|-----|-----|
| **10 MiB payload** | | | |
| Peak RSS | 141 MiB | 77 MiB | 77 MiB |
| SHM files (mmap) | 64 MiB | - | - |
| CPU user | 656 ms | 269 ms | 249 ms |
| CPU sys | 73 ms | 823 ms | 376 ms |
| CPU total | 729 ms | 1092 ms | 624 ms |
| Wall time | 369 ms | 1052 ms | 626 ms |
| CPU/byte | 1.39 ns/B | 2.08 ns/B | 1.19 ns/B |

SHM uses more process RSS (ring buffer files are mmap'd) but near-zero kernel CPU time — it bypasses the kernel entirely for the data path. Wall time is **2.8x faster** than UDS for 10 MiB payloads.

### Data integrity

All benchmarks verify every message byte-by-byte using seq-dependent deterministic payloads. In the throughput test, 2,043,724 messages were verified across all three transports with zero corruption.

## Container deployment

```yaml
# docker-compose.yml
services:
  server:
    ipc: host  # or shm_size: '256m'
  client:
    ipc: host
    volumes:
      - /dev/shm:/dev/shm
```

Kubernetes:
```yaml
volumes:
  - name: shm
    emptyDir:
      medium: Memory
      sizeLimit: 1Gi
volumeMounts:
  - name: shm
    mountPath: /dev/shm
```

## Caveats

- **SPSC only**: one writer, one reader per ring. Multiple concurrent writers corrupt data.
- **Spin-wait**: `send()`/`recv()` busy-loop until timeout. Burns CPU when idle.
- **No crash recovery**: partial writes leave inconsistent ring state.
- **Power-of-2 sizing**: `ring_size` is rounded down. A 100 MiB request becomes 64 MiB.
- **macOS**: uses `/tmp` instead of `/dev/shm` (not tmpfs-backed). Benchmarks don't reflect Linux production performance.
- **Security**: shared memory breaks isolate containment for untrusted workloads. See [CAVEATS.md](CAVEATS.md) for details.
