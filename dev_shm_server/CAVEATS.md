# Caveats: Shared Memory Data Plane for EZ Isolate Bridge

This document covers risks and limitations of replacing the UDS+protobuf data plane
with `/dev/shm` ring buffers in the encrypted-zone-node architecture.

---

## 1. Security: Shared Memory Breaks Isolate Containment

**This is the most critical risk.**

EZ's entire security model relies on the **Policy Enforcer** mediating _all_ data
that enters and leaves an isolate. The enforcer inspects `EzPayloadData` datagrams,
enforces `DataScopeType` policies, and prevents unauthorized data exfiltration.

With UDS+protobuf, the enforcer sits on the socket path — it _must_ see every byte.
With shared memory:

- The shm files (`/dev/shm/ez_isolate_0_c2s`, `_s2c`) are mapped into both the
  enforcer and the isolate's address space. **A compromised isolate can read/write
  the ring buffer at any time**, not just when the enforcer grants permission.
- The ring buffer has no access control within the mapped region. Once a process
  can `mmap` the file, it has full read/write to the entire ring, including the
  atomic position headers. A malicious isolate could:
  - Read datagrams before the enforcer inspects them
  - Overwrite datagrams after the enforcer approves them (TOCTOU)
  - Corrupt the ring header (write_pos/read_pos) to cause the enforcer to
    misparse frames or skip policy checks
  - Observe timing of writes to side-channel other isolates' request patterns

- `/dev/shm` files are visible to any process with the right UID inside the mount
  namespace. If isolates share a PID namespace or `/dev/shm` mount (which is common
  with `--ipc=host` or `-v /dev/shm:/dev/shm`), one isolate could access another
  isolate's channel files.

**Mitigation**: This design should only be used when the isolate is already trusted
(e.g., `is_ratified_isolate = true`), or when the shm region is used _only_ between
the enforcer and a co-located component that is already inside the TEE trust boundary.
For untrusted isolates, keep UDS+protobuf where the enforcer fully controls the pipe.

---

## 2. TOCTOU Between Control Plane and Data Plane

The design splits one atomic operation into two:

1. Write datagrams to shm (data plane)
2. Send gRPC notification with `x-shm-datagram-count` (control plane)

Between steps 1 and 2, or between the gRPC arrival and the shm read, the data in
the ring buffer could be:

- **Overwritten by a subsequent write** if the producer doesn't wait for the
  consumer to finish reading. Our SPSC ring handles this correctly for a single
  producer/consumer pair, but if the channel is reused for concurrent requests
  (multiplexing), frames could be consumed out of order.
- **Modified by a malicious actor** with access to the shm file (see §1).
- **Stale** if the writer process crashes between writing to shm and sending the
  gRPC notification. The ring buffer will contain data that nobody requests, and
  subsequent frames will be misaligned.

**Mitigation**: The current design is strictly SPSC — one channel per
enforcer-isolate pair, one request at a time. This avoids multiplexing races.
For crash recovery, the ring positions would need to be re-synchronized (not
currently implemented).

---

## 3. SPSC Limits Concurrency

The ring buffer is single-producer, single-consumer. This means:

- **One in-flight request per channel.** The EZ enforcer processes requests
  sequentially per isolate, so this matches the current UDS model. But if EZ
  evolves to pipeline multiple requests to the same isolate (e.g., batched
  inference), a single SPSC ring becomes the bottleneck.
- **`StreamInvokeIsolate` is serialized.** Even though gRPC streaming allows
  concurrent messages, the shm channel processes them one at a time. With UDS,
  the kernel socket buffer provides natural backpressure and multiplexing.
- Adding more rings (one per stream) is possible but adds complexity to channel
  lifecycle management.

---

## 4. Spin-Loop CPU Burn

The ring buffer uses spin-waiting (`thread::yield_now()` in a loop) when the
consumer is waiting for data or the producer is waiting for space. On a busy
system:

- An idle channel still burns CPU if a `recv()` is pending with a long timeout.
- In a TEE environment, CPU cycles are metered. Spin-waiting wastes confidential
  compute budget compared to the kernel's `epoll`/`poll` wait on UDS which puts
  the thread to sleep.
- With multiple isolates, N spin-waiting channels = N threads burning CPU.

**Mitigation**: A futex-based or eventfd-based notification could replace the
spin loop. The ring buffer write side would signal a futex after pushing data,
and the reader would `futex_wait` instead of spinning. This preserves the
zero-copy data path while eliminating idle CPU burn.

---

## 5. No Flow Control or Backpressure

The ring buffer is fixed-size (64 MiB default). If the producer writes faster
than the consumer reads:

- `send()` spins until space is available (blocking the caller) or times out.
- There is no gRPC-style flow control window. A fast enforcer can fill the ring
  and stall, with no way for the isolate to signal "slow down" through the
  control plane.
- With UDS, the kernel socket buffer provides automatic backpressure — a slow
  reader causes the writer's `write()` to block, and gRPC/HTTP2 flow control
  propagates this upstream.

---

## 6. Crash Recovery and Cleanup

If either side crashes:

- **Shm files persist on disk** (`/dev/shm/ez_isolate_0_c2s`, etc.) until
  explicitly unlinked. Our `ShmChannel::drop` unlinks on the server side, but
  if the server (enforcer) crashes, the files remain and consume RAM.
- **Ring buffer state is inconsistent.** The atomic positions may point to
  partially written frames. The surviving side has no way to detect this — a
  `pop()` could read garbage framing bytes and return corrupted data.
- **The gRPC connection drops** on crash, but the shm channel doesn't know.
  The surviving side may spin-wait indefinitely on a ring that will never
  be written to.

**Mitigation**: A heartbeat or epoch counter in the ring header could detect
stale channels. The gRPC control plane could carry a channel-reset message
to re-initialize the ring after detecting a peer crash.

---

## 7. Container and Namespace Complications

The original EZ architecture uses UDS between enforcer and isolate containers.
Sharing `/dev/shm` between containers requires one of:

- `--ipc=host`: Shares the host's `/dev/shm` mount. **Breaks container isolation** —
  all containers can see each other's shm files.
- `-v /dev/shm:/dev/shm`: Same effect, explicit volume mount.
- `--ipc=container:<enforcer>`: Shares IPC namespace with the enforcer container
  specifically. Better, but requires orchestration support.
- Named POSIX shared memory (`shm_open`): Requires the same IPC namespace.

In Kubernetes, pods share an IPC namespace by default (`shareProcessNamespace`),
but `/dev/shm` is often a tmpfs with a small default size (64 MiB). A 64 MiB
ring buffer for each isolate would exhaust this immediately.

**Mitigation**: Use file-backed mmap on a shared volume (e.g., emptyDir with
`medium: Memory`) instead of `/dev/shm`. This gives control over the mount
point and size without requiring IPC namespace sharing.

---

## 8. Memory Accounting

Shared memory regions consume physical RAM but are **not attributed to any
single container's cgroup memory limit** when using `--ipc=host` or shared
mounts. This means:

- A 64 MiB ring per isolate × N isolates could consume significant RAM that
  the orchestrator doesn't know about.
- OOM killer may target the wrong container because the shm usage is invisible
  to cgroup accounting.
- In a TEE with encrypted memory, the shm regions consume EPC (Enclave Page
  Cache) pages that are scarce and expensive.

---

## 9. Datagram Ordering and Framing Assumptions

The current implementation assumes:

- Datagrams arrive in order (ring buffer is FIFO). The EZ proto's `repeated bytes
  datagrams` field is ordered, and the shm ring preserves insertion order. This
  works for the current sequential model.
- `x-shm-datagram-count` in the gRPC control message matches exactly the number
  of frames written to shm. If these go out of sync (partial write, retried gRPC,
  network timeout + retry), the receiver will either block waiting for frames
  that don't exist or read frames from a subsequent request.
- There is no per-datagram integrity check. The ring buffer delivers raw bytes
  with no checksum. A bit flip in the shm region (hardware error, or malicious
  modification) would be silently passed through.

---

## 10. Scope Enforcement Gap

In the original EZ design, `EzPayloadIsolateScope.datagram_iscopes[i]` is
paired 1:1 with `EzPayloadData.datagrams[i]`. The enforcer validates this
pairing _before_ forwarding to the isolate.

With shm, the datagrams leave the enforcer's control as soon as they're written
to the ring buffer. The scope metadata travels separately on gRPC. If the isolate
reads the ring buffer directly (bypassing the gRPC control message), it gets
datagrams without scope annotations — there is no enforcement that the isolate
respects the scope of each datagram.

This is fundamentally different from UDS, where the enforcer serializes scope +
data together in one protobuf message and the isolate cannot get one without the
other.

---

## Summary

| Risk | Severity | When it matters |
|------|----------|-----------------|
| Breaks isolate containment | **Critical** | Untrusted isolates |
| TOCTOU on split planes | High | Concurrent/pipelined requests |
| SPSC limits concurrency | Medium | Multi-stream or batched inference |
| Spin-loop CPU waste | Medium | Many idle channels, TEE metering |
| No backpressure | Medium | Bursty producers |
| Crash leaves stale state | Medium | Long-running production services |
| Container namespace setup | Medium | Kubernetes/orchestrated deploys |
| Memory accounting invisible | Medium | Resource-constrained TEE |
| Count mismatch corrupts stream | High | Retries, partial failures |
| Scope enforcement gap | **Critical** | Privacy-sensitive workloads |

**Bottom line**: This design trades security isolation for throughput. It is
appropriate for trusted, co-located components where the enforcer and isolate
are in the same trust domain (e.g., both inside a TEE). It is **not** a safe
drop-in replacement for the UDS+protobuf path when isolates are untrusted.
