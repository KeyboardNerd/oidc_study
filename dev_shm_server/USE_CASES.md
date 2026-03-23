# /dev/shm Server: Use Cases & Design Rationale

## Why /dev/shm Instead of Sockets?

Shared memory (`/dev/shm`) eliminates kernel-mediated copies. With Unix domain sockets,
data is copied twice (writer→kernel buffer→reader). With shared memory, both processes
access the same physical memory pages — zero copies for the data path.

| Method              | Throughput (msgs/sec) | Data Copies | Kernel Involvement |
|---------------------|-----------------------|-------------|--------------------|
| Shared memory       | ~4,700,000            | 0           | None (data path)   |
| Unix domain sockets | ~130,000              | 2           | Every read/write   |
| Named pipes         | ~130,000              | 2           | Every read/write   |
| TCP loopback        | ~80,000               | 2+          | Full TCP/IP stack  |

The win is especially large for **big payloads** (images, tensors, buffers) where copy
cost scales linearly with size.

## Existing Use Cases

### 1. ML Inference Serving (NVIDIA Triton)
Triton Inference Server lets clients register shared memory regions for input/output
tensors. Instead of serializing a 10MB image tensor over gRPC, the client writes it to
`/dev/shm`, tells Triton the region name + offset + size, and Triton reads directly.
Eliminates serialization and network copy overhead entirely.

### 2. Robotics / ROS2 (Fast DDS)
ROS2's default middleware (Fast DDS) has a shared memory transport for pub/sub between
nodes. Critical for high-bandwidth sensor data — a LiDAR producing 300K points/sec or
cameras at 30fps 1080p can't afford socket overhead between containerized nodes.

### 3. PyTorch DataLoader
PyTorch multiprocessing workers share image tensor batches via `/dev/shm` instead of
pickling them through pipes. This is why `--shm-size` must be increased when training
in Docker.

### 4. Database Shared Buffers
Oracle SGA and PostgreSQL shared_buffers live in `/dev/shm`. Multiple backend processes
access the same buffer pool without IPC overhead.

### 5. Browser IPC (Chromium)
Chrome uses shared memory to pass rendered frames between sandboxed renderer processes
and the browser process. Compositing 60fps at 4K resolution requires zero-copy.

### 6. Video/Camera Streaming
Raspberry Pi's picamera2 uses shared memory ring buffers to pass 1080p frames between
capture and processing processes at ~6ms per frame.

## Container ↔ Host Setup

### Docker
```bash
# Option 1: Mount host /dev/shm directly
docker run -v /dev/shm:/dev/shm myimage

# Option 2: Share host IPC namespace (gives access to ALL host shm)
docker run --ipc=host myimage

# Option 3: Just increase container's private shm
docker run --shm-size=2g myimage
```

### Kubernetes
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

## Design: What We're Building

A **shared memory request/response server** using a lock-free ring buffer on `/dev/shm`:

```
┌──────────┐     /dev/shm/ring_req      ┌──────────┐
│          │ ──── [ring buffer] ───────▶ │          │
│  Client  │                             │  Server  │
│  (host)  │ ◀─── [ring buffer] ──────── │ (container)│
│          │     /dev/shm/ring_resp      │          │
└──────────┘                             └──────────┘
        eventfd / futex for wakeup
```

**Components:**
- **Ring buffer**: Lock-free SPSC (single-producer, single-consumer) in shared memory
- **Protocol**: Length-prefixed messages with request IDs for correlation
- **Notification**: eventfd for low-latency wakeup (falls back to polling)
- **Server**: Reads requests from ring, dispatches handlers, writes responses

**Key properties:**
- Zero-copy for the data path
- Lock-free (no mutexes in the hot path)
- Works across container boundary via `-v /dev/shm:/dev/shm`
- Configurable ring size (default 16MB)

## Pitfalls to Be Aware Of

1. **No built-in sync**: We use atomics + eventfd, not mutexes
2. **Cleanup on crash**: shm files persist after death — server must clean up on start
3. **Security**: Any process with `/dev/shm` access can read/write — use file permissions
4. **Memory pressure**: Ring buffers consume real RAM (and swap)
5. **macOS**: No `/dev/shm` — use a configurable tmpfs path for portability
