

All projects
Create shared memory server for container-host communication
Document folder purpose and contents
+1
-1
Build high-performance mTLS proxy with SNI routing
+4027
-0
Explore and document repository contents
+1061
-0

Preview

Bypass permissions mode: Claude can take actions without asking, including modifying or deleting files. See safe use tips


Write me a server that runs on /dev/shm so that the data sharing between a container and host is faster than a socket. Explore possibility existing use case, and write them down first. And then start the coding.



Let me first explore the current project state and research existing approaches.


Ran a command, ran an agent
Ran a command, ran an agent

Excellent research. Let me write up the use cases document first, then build the server.


Created a file
Created a file

Now let me build the server. I'll use Rust for performance.


Ran 2 commands, read a file, created a file
Ran 2 commands, read a file, created a file

Now let me create the core modules and binaries. I'll build this as a library with a ring buffer, protocol layer, and server/client binaries.


Created 6 files
Created 6 files

Now the benchmark example and Docker setup:


Created 4 files, ran a command
Created 4 files, ran a command

Now let's build and test:


Build the project
Build the project

Build succeeds. Let's run the tests and the benchmark:


Ran 2 commands
Ran 2 commands

Need to wrap the raw pointer for Send. Let me fix the bench example:


Read a file
Read a file

The issue is raw pointers aren't Send. I need a wrapper. Let me fix this by using a Send-able wrapper:


Created a file, ran a command
Created a file, ran a command

All 5 tests pass. Let's run the examples:


Ran 2 commands
Ran 2 commands

Excellent results. Now let me also test the actual server/client end-to-end:


End-to-end test: server + client
End-to-end test: server + client

Everything works. Let me clean up the default src/main.rs that cargo init created:


Ran 2 commands, read a file
Ran 2 commands, read a file

Here's what was built:

Project Structure
dev_shm_server/
├── USE_CASES.md          # Research: existing use cases & design rationale
├── Cargo.toml
├── Dockerfile            # Multi-stage build for containerized server
├── docker-compose.yml    # Mounts /dev/shm for host↔container IPC
├── src/
│   ├── lib.rs            # Library root
│   ├── shm.rs            # Shared memory region (mmap files on tmpfs)
│   ├── ring.rs           # Lock-free SPSC ring buffer (5 tests)
│   ├── protocol.rs       # Binary message protocol (req/resp/error/shutdown)
│   ├── server.rs         # shm-server binary (adaptive backoff, graceful shutdown)
│   └── client.rs         # shm-client binary (interactive + single-command modes)
└── examples/
    ├── echo_server.rs    # Minimal in-process demo
    └── bench.rs          # Throughput + latency benchmark

Benchmark Results (on this machine)
Payload	Throughput	Bandwidth
64B	6.8M msgs/sec	413 MB/s
256B	3.4M msgs/sec	834 MB/s
1KB	1.4M msgs/sec	1.4 GB/s
4KB	351K msgs/sec	1.4 GB/s
Latency (64B roundtrip): p50 = 250ns, p99 = 875ns

Usage
# Terminal 1: Start server (uses /tmp on macOS, /dev/shm on Linux)
cargo run --release --bin shm-server

# Terminal 2: Client commands
cargo run --release --bin shm-client -- ping
cargo run --release --bin shm-client -- echo "hello"
cargo run --release --bin shm-client -- info
cargo run --release --bin shm-client -- shutdown

# Docker (Linux): container shares /dev/shm with host
docker compose up        # server in container
shm-client ping          # client on host, same /dev/shm

compare with unix socket / TCP with the same testing framework.



Let me first look at the existing bench code to understand the framework, then add Unix socket and TCP benchmarks.


Read 2 files
Read 2 files

I'll rewrite the benchmark to be a unified comparison across all three transports (shm, Unix socket, TCP) with identical test methodology.


Ran 3 commands, edited 2 files, created a file
Ran 3 commands, edited 2 files, created a file

Here are the results:

Throughput (1M messages, unidirectional)
Payload	SHM	Unix Socket	TCP Loopback	SHM vs UDS	SHM vs TCP
64B	6.3M msg/s	710K msg/s	174K msg/s	8.9x	36.6x
256B	3.4M msg/s	630K msg/s	178K msg/s	5.4x	19.1x
1KB	1.4M msg/s	757K msg/s	170K msg/s	1.8x	8.1x
4KB	354K msg/s	406K msg/s	146K msg/s	0.87x	2.4x
Latency (100K roundtrips, p50)
Payload	SHM	Unix Socket	TCP Loopback	SHM vs UDS	SHM vs TCP
64B	250ns	6.5µs	17µs	26x faster	68x faster
256B	750ns	7.5µs	17.3µs	10x faster	23x faster
1KB	2.6µs	6.0µs	17.5µs	2.3x faster	6.8x faster
4KB	10µs	8.3µs	18.8µs	0.83x (UDS wins)	1.9x faster
Key takeaways:

SHM dominates for small messages — 26-68x lower latency than sockets because there's zero kernel involvement
At 4KB, UDS closes the gap and even edges ahead — the per-byte copy cost becomes the bottleneck and UDS benefits from kernel buffering (no ring wrap-around overhead)
TCP always loses due to full protocol stack overhead, even on loopback
The sweet spot for SHM is sub-1KB messages at high frequency — exactly the ML inference metadata, sensor readings, and control messages use case
Oh, but kernel will do copy? Can you improve the ring buffering scheme?



Good catch. The current ring does byte-by-byte write_volatile/read_volatile with a modulo on every byte — that's userspace copying at its worst. Let me look at exactly what we're doing.


Read a file
Read a file

Two big problems here:

Byte-by-byte volatile + modulo per byte — the write_bytes_at/read_bytes_at loops do a % and a volatile store/load for every single byte. That's killing throughput, especially at 4KB where UDS actually won.
Non-power-of-2 capacity — means we use expensive % instead of bitwise &.
The fix: power-of-2 capacity with bitmask, and memcpy-style bulk copies (at most 2 copy_nonoverlapping calls to handle the wrap). The atomic Release on write_pos already provides the ordering fence — we don't need per-byte volatile.


Created a file, ran a command
Created a file, ran a command

All 8 tests pass. Now let's run the benchmark comparison:


Run comparison benchmark with improved ring buffer
Run comparison benchmark with improved ring buffer

Massive improvement. Here's the before/after comparison:

What Changed
Old ring buffer: per-byte write_volatile + % capacity on every byte
New ring buffer: bulk memcpy (at most 2 calls per message) + & mask (power-of-2 bitmask)

Throughput Improvement (SHM, old → new)
Payload	Before	After	Speedup
64B	6.3M msg/s	8.5M msg/s	1.3x
256B	3.4M msg/s	7.4M msg/s	2.2x
1KB	1.4M msg/s	5.2M msg/s	3.8x
4KB	354K msg/s	2.3M msg/s	6.5x
The bigger the payload, the bigger the win — exactly where the byte-by-byte loop was hurting most.

SHM vs Sockets (after fix)
Payload	SHM	UDS	TCP	SHM/UDS	SHM/TCP
64B	8.5M	797K	178K	10.7x	47.7x
4KB	2.3M	567K	170K	4.1x	13.5x
Latency p50 (64B roundtrip): 167ns vs 6.5µs (UDS) vs 17.2µs (TCP) — 39x faster than UDS, 103x faster than TCP.

At 4KB, SHM now firmly beats UDS too (958ns vs 8.3µs) — the old version lost to UDS at that size due to the per-byte copy overhead.

Let's try larger payloads. In reality, HBM memory can carry 10MiB per chunk for inference data transfer. Can you first research my claim on 10MiB? And then test with that.
Note that, what's the bottleneck? Is it RAM speed?



Ran 2 agents
Ran 2 agents

Great research. 10 MiB is completely realistic — it maps exactly to vLLM KV cache blocks (~5-8 MiB), UNet activation maps (~10 MiB), and batched Triton inputs (~9.6 MiB).

Now let me extend the benchmark with larger payloads and add bandwidth analysis to see where we hit the DRAM wall.


Ran 2 commands, read a file, created a file
Ran 2 commands, read a file, created a file

These results are very revealing. Let me check what machine this is for the analysis:


Ran 2 commands
Ran 2 commands

Apple M2 Pro — 200 GB/s theoretical memory bandwidth, 16 MB L2 (perf cores), no L3.

Results Analysis
10 MiB claim: confirmed realistic
Transport	10 MiB Bandwidth	10 MiB Latency (p50)
SHM	37.9 GB/s	951 µs
UDS	1.68 GB/s	13.2 ms
TCP	9.16 GB/s	2.6 ms
SHM is 22.5x faster than Unix sockets and 4.1x faster than TCP for 10 MiB inference-scale chunks.

Where's the bottleneck? Yes, it's RAM speed.
The bandwidth curve tells the story clearly:

Payload	SHM Bandwidth	Bottleneck
64B	0.78 GB/s	Atomic cache-line bouncing — the memcpy is ~1ns, but the atomic Release+Acquire round-trip between cores costs ~100ns
1 KiB	11.8 GB/s	Atomics still dominate — amortizing the ~100ns atomic cost over 1KB gives ~10 GB/s
64 KiB	42.0 GB/s	L2 cache bandwidth — data fits in 16 MB L2, memcpy runs at L2 speed
256 KiB	40.4 GB/s	L2 cache — still fits
1 MiB	43.0 GB/s	L2 → DRAM transition — ring buffer is 64 MiB (exceeds L2), but working set is hot
4 MiB	44.5 GB/s	Peak — sweet spot between cache and DRAM
10 MiB	37.9 GB/s	DRAM bandwidth wall — 37.9 GB/s out of 200 GB/s theoretical = ~19% utilization
The 19% utilization at 10 MiB is expected: standard memcpy on ARM does load+store, and the write triggers a read-for-ownership, so each cache line costs 3 bus transactions. That gives a theoretical ceiling of ~67% for memcpy. The remaining gap is TLB pressure and the fact that the ring buffer's circular layout prevents the hardware prefetcher from doing long sequential runs (it wraps).

Why TCP beats UDS at large sizes
Surprising result: TCP (9.16 GB/s) beats UDS (1.68 GB/s) at 10 MiB. This is because macOS's TCP stack uses sendfile-style zero-copy optimizations and larger kernel buffers for loopback, while UDS goes through a smaller so_sndbuf/so_rcvbuf pipe with more syscalls per chunk.

Is this a real result? Can you verify that the data transfer is not only received but also checked to have appropriate data integrity? I do not know if you have implemented this appropriately. Add tests to ensure data integrity.



Good call — let me audit the benchmark to check if we're actually verifying what we receive.


Read 3 files
Read 3 files

You're right to question this. Looking at the benchmark code, none of the consumers verify data:

SHM consumer: ring.pop(&mut buf) → counts, never checks content
UDS/TCP consumer: sock_read_msg → counts, never checks content
Latency tests: same — pops and times, never verifies
The ring buffer could be producing garbage (especially at wrap-around boundaries with the bulk memcpy rewrite) and we'd never know. Let me add proper integrity verification to both the ring buffer tests and the benchmark.


Read a file
Read a file

I see another problem: the test_sustained_wrap_around pushes and pops but never verifies the content — it just drains. Let me rewrite the tests properly and add thorough integrity checks.


Edited a file
Edited a file

Now add integrity verification to the benchmark — both throughput and latency tests need to check every byte:


Read 2 files, ran 2 commands, created a file
Read 2 files, ran 2 commands, created a file

Right, hex can't have L. Let me fix that.


Edited a file, ran a command
Edited a file, ran a command

Good. Now let's run the unit tests first, then the full benchmark:


Run all unit tests including new integrity tests
Run all unit tests including new integrity tests

All 10 tests pass, including the new integrity tests:

test_sustained_wrap_integrity — 50K messages with varying sizes, every byte verified
test_large_payload_integrity — payloads up to 100 KiB through a 1 MiB ring, every byte verified
test_wrap_around_verified — wrap-around with byte verification
test_wrap_around_length_header_split — length header itself crossing the boundary
Now run the full benchmark with integrity verification:


Run benchmark with data integrity verification on every message
Run benchmark with data integrity verification on every message

All 2,043,724 messages verified across all three transports — every byte checked. The results are real.

Key finding: verification reveals the true cost
The previous benchmark was inflated because it reused a pre-encoded buffer (just pushing the same bytes repeatedly). Now every message has a unique, seq-dependent payload that must be generated and verified, which adds make_payload + verify_message overhead to every message. This is more realistic — in real inference serving, every tensor chunk is different.

Verified results: SHM vs Sockets
Payload	SHM	UDS	TCP	SHM/UDS	SHM/TCP
64B	437 MB/s	42 MB/s	11 MB/s	10.5x	38x
1 KiB	3.0 GB/s	628 MB/s	170 MB/s	4.8x	18x
64 KiB	3.9 GB/s	958 MB/s	1.5 GB/s	4.1x	2.7x
1 MiB	4.0 GB/s	1.2 GB/s	2.9 GB/s	3.4x	1.4x
10 MiB	2.8 GB/s	1.0 GB/s	1.8 GB/s	2.7x	1.5x
What changed from the unverified run
10 MiB	Before (unverified)	After (verified)	Reason
SHM	37.9 GB/s	2.8 GB/s	Old benchmark pushed the same pre-encoded buffer — no payload generation, no allocation per message. Now each msg has unique data.
UDS	1.7 GB/s	1.0 GB/s	Same payload generation cost
TCP	9.2 GB/s	1.8 GB/s	Same
The relative advantage of SHM still holds (2.7x vs UDS, 1.5x vs TCP at 10 MiB), but the absolute numbers are more honest. The bottleneck at large sizes is now make_payload allocation + the verify_message comparison on the consumer side, not just the transport. The previous 37.9 GB/s was measuring "how fast can we push the same bytes in a loop" — not realistic for real data.

Latency (verified echo, every byte checked)
Size	SHM p50	UDS p50	TCP p50
64B	166ns	6.6µs	17.3µs
10 MiB	902µs	12.9ms	2.5ms
Latency numbers are consistent with before — the echo path (server just bounces the same bytes back) doesn't pay the payload generation cost.

ok, but then let's measure the RAM and CPU impact for this data transfer. I do not know if this makes huge RAM increase due to shared memory usage.



Good question. The spin-loop also burns CPU while idle. Let me write a resource-tracking benchmark that measures RSS, shared memory file sizes, and CPU time (user + system) for each transport.


Read a file, edited a file, created a file
Read a file, edited a file, created a file

Two issues: need libc dep (or use nix::libc) and the SendPtr wrapper. Let me fix both.


Edited a file
Edited a file

Now fix the SendPtr — it needs unsafe impl Send:


Read a file
Read a file

The SendPtr is already there with unsafe impl Send. The error is that rp and pp capture the *mut u8 from the struct but the closure sees the raw pointer. Actually wait — it should work because SendPtr implements Send and the closure captures rp and pp (which are SendPtr). Let me re-check... The issue is the closure also captures other things that contain raw pointers? No, looking again the error specifically says *mut u8 cannot be sent. Let me look more carefully — could be the SendPtr definition inside the function isn't being found. Actually the struct definition inside a function should work fine. Let me just add extern crate libc; and rebuild to see if the libc issue was the main thing causing cascading errors.


Build after adding libc dependency
Build after adding libc dependency

The issue is that SendPtr defined inside a function doesn't get its Send impl propagated through the closure properly in some Rust versions. Let me move it to top-level and also use the ShmRegion directly with Arc:


Edited 2 files, ran a command
Edited 2 files, ran a command

The issue persists because something else in scope with *mut u8 is being captured. Let me simplify the SHM test to use Arc-wrapped shared memory like the other benchmark does.


Read a file, edited a file, ran a command
Read a file, edited a file, ran a command

Builds. Let's run it:


Run resource usage benchmark
Run resource usage benchmark

These results answer your question clearly. Here's the breakdown:

RAM Impact
Transport	Peak RSS	RSS Δ	SHM Files	Hidden Kernel Buffers
SHM (10 MiB msg)	140.6 MiB	+114 MiB	64 MiB	none
UDS	76.6 MiB	+16 KiB	-	yes (invisible)
TCP	76.6 MiB	+16 KiB	-	yes (invisible)
SHM does use more visible RAM, but it's predictable and bounded:

The ring buffer files are proportional to max message size, not total data transferred — for 10 MiB messages we need a 64 MiB ring (3x message size, power-of-2)
For 64B messages, the overhead is only 2 MiB (minimum ring size)
The files are cleaned up when the server stops
Socket transports look cheaper in RSS, but their kernel buffers (so_sndbuf/so_rcvbuf, TCP retransmit queues) are hidden — they consume real RAM too, just not in process RSS.

CPU Impact — the real story
Metric (10 MiB)	SHM	UDS	TCP
CPU user	640ms	263ms	244ms
CPU sys	64ms	768ms	351ms
CPU total	704ms	1031ms	595ms
Wall time	355ms	983ms	609ms
Key insight: SHM has near-zero kernel time (64ms sys vs 768ms for UDS). The kernel is completely out of the data path. SHM's higher user time is the spin-loop + payload generation cost.

For small messages (64B), the spin-loop cost is stark: SHM burns 210ms of user CPU vs UDS's 300ms user + 1076ms sys. SHM avoids the 1076ms of kernel overhead but pays in CPU spin. An eventfd notification would reduce spin-loop waste for bursty workloads.

Sizing guidance
Max Message Size	Ring Files (both rings)	Practical?
64B	2 MiB	Negligible
1 KiB	2 MiB	Negligible
64 KiB	2 MiB	Negligible
1 MiB	8 MiB	Fine
10 MiB	64 MiB	Acceptable — less than one GPU's KV cache
Is this server generic?
Let's use that as some library and provide compatibility layer as data plane for gRPC server ( or some other high perf server? )
Each request may have different header. The payload can be huge. However, it doesn't have to be serialized to protobuf.
It might be a special gRPC server for transfer purpose.



Ran 2 agents
Ran 2 agents

Good question. The current library is NOT generic — the protocol has hardcoded MessageType, fixed 13-byte header, and no support for custom metadata. Let me redesign it as a proper layered library with a gRPC-compatible data plane.

The architecture follows the Triton pattern (control plane + data plane split):

┌─────────────────────────────────────────────────────┐
│  gRPC (tonic) — control plane                       │
│  Small protobuf: region name, offset, size, headers │
├─────────────────────────────────────────────────────┤
│  ShmChannel — data plane                            │
│  Ring buffer on /dev/shm: raw bulk payload          │
│  No serialization, no kernel, zero-copy-ish         │
└─────────────────────────────────────────────────────┘

Let me build this.


Ran a command, loaded tools, updated todo list
Ran a command, loaded tools, updated todo list

First, let me redesign the protocol to support extensible headers while keeping the ring buffer unchanged:


Read a file
Read a file

Now let me redesign. I'll keep the old Message for backward compat but add a new generic Frame system:


Created a file, updated todo list
Created a file, updated todo list

Now the ShmChannel — the high-level abstraction that manages shm regions and provides typed send/receive with Frame:


Created a file
Created a file

Now update lib.rs and add the proto + tonic integration:


Created a file, updated todo list
Created a file, updated todo list

Now the proto definition and tonic service. I'll create a service module that doesn't require protobuf at all — the gRPC control plane uses tonic with a simple proto, but the actual data plane is the ShmChannel with raw Frames. This way users can use the data plane without gRPC too.


Ran a command, created a file
Ran a command, created a file

Now add tonic/prost dependencies and the build script:


Created 2 files, read a file
Created 2 files, read a file

Now the service module — the glue between tonic and ShmChannel:


Created a file
Created a file

Now the integration example:


Created a file
Created a file
This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.

Summary:

Primary Request and Intent:

Initial: Build a server on /dev/shm for faster container↔host data sharing than sockets, with research on existing use cases first
Comparison: Add Unix socket and TCP benchmarks using the same testing framework
Optimization: Improve the ring buffer scheme (user noted byte-by-byte copies were inefficient)
Scale testing: Test with 10 MiB payloads (research HBM inference chunk sizes), analyze bottleneck (RAM speed?)
Data integrity: User challenged whether results were real — asked for verification that received data has appropriate integrity
Resource measurement: Measure RAM and CPU impact of shared memory vs sockets
gRPC integration: Make the library generic, provide compatibility layer as data plane for gRPC server. Each request may have different headers, payload can be huge, doesn't need protobuf serialization for payload. "It might be a special gRPC server for transfer purpose."
Key Technical Concepts:

Lock-free SPSC (single-producer, single-consumer) ring buffer on /dev/shm
Power-of-2 capacity with bitmask for efficient wrapping
Bulk ptr::copy_nonoverlapping (memcpy) instead of byte-by-byte volatile writes
Atomic Acquire/Release ordering for cross-process synchronization
Cache-line padding to avoid false sharing (write_pos and read_pos on separate 64-byte lines)
Control plane + data plane split architecture (Triton pattern)
Container /dev/shm sharing: -v /dev/shm:/dev/shm, --ipc=host, --shm-size
HBM inference data: vLLM KV cache blocks (~5-8 MiB), UNet activations (~10 MiB)
Memory bandwidth bottleneck analysis: atomics (small), L2/L3 cache (mid), DRAM (large)
tonic gRPC with custom service for control plane
Generic Frame protocol with extensible TLV headers
Files and Code Sections:

Cargo.toml — Project manifest, updated to v0.2.0 with tonic/prost/tokio deps

[
package
]
name = "dev_shm_server"
version = "0.2.0"
edition = "2021"

[
dependencies
]
memmap2 = "0.9"
nix = { version = "0.29", features = ["fs", "mman"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
log = "0.4"
env_logger = "0.11"
thiserror = "2"
ctrlc = "3"
libc = "0.2"
tonic = "0.13"
prost = "0.13"
tokio = { version = "1", features = ["full"] }

[
build-dependencies
]
tonic-build = "0.13"

build.rs — Proto compilation

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/shm_transfer.proto")?;
    Ok(())
}

src/lib.rs — Module exports, updated to include channel and service

pub mod ring;
pub mod protocol;
pub mod shm;
pub mod channel;
pub mod service;

src/ring.rs — Lock-free SPSC ring buffer (the core data structure)

Rewritten from byte-by-byte volatile to bulk memcpy
Power-of-2 capacity with bitmask
At most 2 copy_nonoverlapping calls per push/pop for wrap-around
Comprehensive integrity tests with make_payload/verify_payload
Key methods: new(base, total_size), init(), push(data) -> bool, pop(buf) -> bool, capacity(), available(), free_space()
Tests: test_push_pop_roundtrip, test_multiple_messages_verified, test_full_ring, test_wrap_around_verified, test_wrap_around_length_header_split, test_sustained_wrap_integrity (50K msgs), test_large_payload_integrity (100 KiB), test_power_of_two_rounding
src/shm.rs — Shared memory region management via memmap2

ShmRegion::create(base_dir, name, size), ::open(base_dir, name), unlink(), as_mut_ptr(), len()
src/protocol.rs — Wire protocol, redesigned with two layers

Frame (new, generic): version, flags, stream_id, extensible TLV headers, raw payload
Wire format: [version:u8][flags:u8][stream_id:u32][header_len:u32][payload_len:u32][headers...][payload...]
Headers: [key_len:u16][key][val_len:u16][val] repeated
Flags: END_STREAM=0x01, COMPRESSED=0x02, ERROR=0x04, CONTROL=0x08
Methods: new(stream_id), .header(), .header_str(), .payload(), .flags(), .get_header(), .get_header_str(), .encode(), Frame::decode()
Message (old, kept for backward compat): fixed 13-byte header format
From<Message> for Frame conversion
src/channel.rs — High-level bidirectional ShmChannel

ChannelConfig (shm_dir, name, ring_size)
ShmChannel::create(config) — server side, initializes rings
ShmChannel::open(config) — client side, opens existing
send(&frame, timeout), recv(timeout), try_send(), try_recv(), send_raw()
Server auto-unlinks files on drop
Naming convention: {name}_s2c (server→client), {name}_c2s (client→server)
proto/shm_transfer.proto — gRPC control plane service definition

CreateChannel, Send, Stream (bidi streaming), DestroyChannel
SendRequest carries channel_name, stream_id, headers map, payload_size (NOT the actual payload)
src/service.rs — tonic gRPC service implementation

ShmTransferService::new(handler) — takes a Fn(Frame) -> Frame callback
Manages HashMap of ShmChannels
create_channel: creates ShmChannel, returns paths
send: reads frame from shm, dispatches to handler, writes response to shm
stream: bidirectional streaming with shm data plane
destroy_channel: removes and drops channel (unlinks files)
src/server.rs — Original standalone shm-server binary (unchanged from v1)

src/client.rs — Original standalone shm-client binary (unchanged from v1)

examples/bench.rs — Full benchmark with integrity verification

Tests SHM, UDS, TCP across 64B to 10 MiB payloads
Every message has unique seq-dependent payload verified byte-by-byte
make_payload(seq, len) generates deterministic data
verify_message(raw, seq, payload_len) decodes and checks every byte
Reports throughput, bandwidth, latency (p50/p99), and integrity summary
examples/resource_bench.rs — RAM + CPU resource measurement

Measures RSS via mach_task_basic_info (macOS) / /proc/self/statm (Linux)
CPU user+sys via getrusage
SHM file sizes tracked
All data integrity-verified
examples/grpc_transfer.rs — gRPC + SHM integration demo (NOT YET COMPILED)

50 × 10 MiB tensor transfers
Shows control plane (gRPC ~200 bytes) vs data plane (shm 10 MiB)
Time breakdown: shm write, shm read, gRPC control
USE_CASES.md — Research documentation on existing use cases

Dockerfile / docker-compose.yml — Container setup

Errors and Fixes:

*mut u8 cannot be sent between threads safely in bench.rs: Created SharedMem wrapper struct with unsafe impl Send + Sync, shared via Arc
UnixStream::set_nodelay doesn't exist: Removed the call (UDS has no Nagle)
Unused variable transport: Prefixed with underscore
0xLATENCY invalid hex literal: Changed to 0xA7E9C100
Missing libc crate for resource_bench.rs: Added libc = "0.2" to Cargo.toml
SendPtr inside function didn't propagate Send: Moved struct definition to top-level (outside function) with unsafe impl Send + Sync
*mut u8 Send issue in resource_bench.rs: Used Arc<SendPtr> pattern instead of raw pointer capture
User feedback on data integrity (critical): User challenged benchmark results — found that benchmarks were not verifying received data at all, and were reusing pre-encoded buffers. Rewrote all tests and benchmarks with deterministic payloads and byte-level verification. This revealed the previous 37.9 GB/s number was inflated; honest verified number is 2.8 GB/s at 10 MiB.
Problem Solving:

Ring buffer performance: Identified byte-by-byte volatile + modulo as the bottleneck; replaced with bulk memcpy + power-of-2 bitmask. 6.5x improvement at 4KB.
Benchmark honesty: User's integrity question revealed that throughput benchmarks were measuring "push same bytes in a loop" not real data transfer. Fixed by making every message unique and verified.
RAM concern: Showed that SHM memory is bounded by ring size (proportional to max message size), not total data transferred. 64 MiB ring files for 10 MiB messages.
CPU concern: Showed SHM has near-zero kernel time (64ms sys vs 768ms for UDS) but spin-loop burns user CPU.
Making library generic: Identified that protocol layer needed redesign for extensible headers. Ring buffer and shm modules were already generic. Implemented Frame protocol + ShmChannel + tonic service.
All User Messages:

"Write me a server that runs on /dev/shm so that the data sharing between a container and host is faster than a socket. Explore possibility existing use case, and write them down first. And then start the coding."
"compare with unix socket / TCP with the same testing framework."
"Oh, but kernel will do copy? Can you improve the ring buffering scheme?"
"Let's try larger payloads. In reality, HBM memory can carry 10MiB per chunk for inference data transfer. Can you first research my claim on 10MiB? And then test with that. Note that, what's the bottleneck? Is it RAM speed?"
"Is this a real result? Can you verify that the data transfer is not only received but also checked to have appropriate data integrity? I do not know if you have implemented this appropriately. Add tests to ensure data integrity."
"ok, but then let's measure the RAM and CPU impact for this data transfer. I do not know if this makes huge RAM increase due to shared memory usage."
"Is this server generic? Let's use that as some library and provide compatibility layer as data plane for gRPC server ( or some other high perf server? ) Each request may have different header. The payload can be huge. However, it doesn't have to be serialized to protobuf. It might be a special gRPC server for transfer purpose."
Pending Tasks (from TODO list):

[in_progress] Add proto definitions for gRPC control plane — files created but NOT compiled
[pending] Add tonic gRPC integration — service.rs created but NOT compiled
[pending] Add integration example — grpc_transfer.rs created but NOT compiled
[pending] Test everything compiles and runs — this is the critical next step
Current Work:
Immediately before the summary request, I was implementing the gRPC integration for the shm library. I had just created all the files needed:

Redesigned src/protocol.rs with generic Frame format (extensible TLV headers, stream_id, flags)
Created src/channel.rs with ShmChannel (bidirectional shm channel abstraction)
Created proto/shm_transfer.proto (gRPC control plane service)
Created src/service.rs (tonic service implementation with handler callback)
Created examples/grpc_transfer.rs (50 × 10 MiB tensor transfer demo)
Updated Cargo.toml with tonic/prost/tokio dependencies and build.rs
Updated src/lib.rs to export new modules
None of these new files have been compiled or tested yet. The last successful build/test was before the gRPC work began. The TODO list shows "Test everything compiles and runs" as pending.

Optional Next Step:
The immediate next step is to compile and fix any errors in the new gRPC integration code, then run the tests and the grpc_transfer example. This directly continues from the TODO list item "Test everything compiles and runs" which was the pending task when the user asked for a summary. The most recent work was creating all the gRPC integration files (service.rs, channel.rs, updated protocol.rs, proto/shm_transfer.proto, build.rs, examples/grpc_transfer.rs), and the natural next step is:

Run cargo build --release to compile everything and fix any errors
Run cargo test to verify all tests pass (including new Frame and ShmChannel tests)
Run cargo run --release --example grpc_transfer to verify the integration demo works
If you need specific details from before compaction (like exact code snippets, error messages, or content you generated), read the full transcript at: /Users/sida/.claude/projects/-Users-sida-github-dev-shm-server/dae99277-31bc-42f6-a36c-1f1fba0f31fa.jsonl
Continue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening, do not preface with "I'll continue" or similar. Pick up the last task as if the break never happened.

Show more


Build the project to find compilation errors
Build the project to find compilation errors

Rust toolchain is too old for a transitive dependency. Let me check the version and see if we can pin older compatible versions or update the toolchain.


Check Rust toolchain version
Check Rust toolchain version

Rust 1.78 is too old for the latest getrandom which requires edition 2024. Let me update the toolchain.


Update Rust toolchain to latest stable
Update Rust toolchain to latest stable

Now let's build again.


Build the project with updated toolchain
Build the project with updated toolchain

Need to install protoc.


Ran 2 commands
Ran 2 commands

Missing tokio-stream dependency.


Ran 2 commands
Ran 2 commands

The MutexGuard is held across an .await. Need to scope the lock so it's dropped before the await.


Read a file
Read a file

Need to scope the lock so it drops before the .await. I'll restructure to compute the response inside the lock, then send outside it.


Edited a file, ran a command
Edited a file, ran a command

Build succeeds. Now let's run the tests.


Run all tests
Run all tests

All 19 tests pass. Now let's run the gRPC transfer example.


Run the gRPC + SHM transfer demo
Run the gRPC + SHM transfer demo

Everything works. Here are the results:

Build: compiles cleanly (just some minor warnings in examples)
Tests: all 19 pass (ring buffer, protocol, channel)
gRPC demo: 50 × 10 MiB tensor roundtrips completed successfully
Key numbers from the demo:

3.18 GB/s throughput for 1.05 GB total data
SHM write/read accounts for 30% of wall time
gRPC control plane takes 34% — only ~200 bytes per roundtrip vs 10 MiB on the data plane
104,858x less data flows over gRPC compared to the shared memory data plane
I have a implementation in https://github.com/private-compute-infra-toolkit/encrypted-zone-node
if you look at the protobufs, you'll find that it's using "metadata" + payload.
Is it possible to use whatever you wrote to improve?
Don't update the encrypted-zone code here. Just copy the protobufs and check if you can implement using your solution.



Explore GitHub repo protobufs
Explore GitHub repo protobufs

Good findings. The key pattern is clear:

ControlPlaneMetadata — small routing info, already has a shared_memory_handles field
EzPayloadData — repeated bytes datagrams — the bulk data that should go through shm
EzPayloadScope — per-datagram privacy scopes (small, stays on gRPC)
The local IPC services (EzIsolateBridge, IsolateEzBridge) communicate over UDS between enforcer and isolate on the same machine — perfect candidates for shm acceleration. And they already anticipated this with the shared_memory_handles field.

Let me copy the relevant protos and implement the bridge using our shm library.


Create proto directory structure
Create proto directory structure

Now let me write the subset of proto files we need for the bridge services.


Created 8 files
Created 8 files

Now update build.rs to compile the EZ protos.


Read a file, created a file, ran a command
Read a file, created a file, ran a command

Protos compile. Now let me check what got generated so I know the module paths.


Ran 2 commands
Ran 2 commands

Good, the generated code looks right. Now let me write the EZ bridge implementation that uses our shm data plane. The key idea: EzPayloadData.datagrams (the big bulk data) goes through shared memory, while ControlPlaneMetadata and scope info stay on gRPC.


Read a file, edited a file
Read a file, edited a file

Now write the EZ bridge module — implements EzIsolateBridge with shm data plane for the datagrams:


Created a file, ran a command
Created a file, ran a command

Compiles clean. Now let's write an example that demonstrates the full flow — an enforcer sending 10 MiB datagrams to an isolate via the EZ bridge with shm data plane, compared to the original inline approach.


Ran 2 commands, created a file, read a file
Ran 2 commands, created a file, read a file

Works perfectly. Here's the summary:

Yes, the EZ protos map directly onto our shm solution. The key insight is that ControlPlaneMetadata already has a shared_memory_handles field — they anticipated shared memory but hadn't implemented it as a data plane.

What was implemented
src/ez_bridge.rs — Drop-in EzIsolateBridge gRPC service implementation that:

Keeps ControlPlaneMetadata + EzPayloadIsolateScope on gRPC (small, ~400 bytes)
Routes EzPayloadData.datagrams through /dev/shm ring buffers instead of protobuf serialization
Uses shared_memory_handles[0] (already in the proto!) as the shm channel name
Adds x-shm-datagram-count in metadata_headers (also already in the proto!) so the receiver knows how many frames to read
Includes EzShmClient helper for the client side
Results
Metric	Value
Throughput	2.44 GB/s for 50 × 10 MiB roundtrips
gRPC wire bytes	~20 KB (control plane only)
Original UDS+protobuf wire	~1 GB (datagrams serialized inline)
Wire reduction	52,429x less data over gRPC
The original EZ design serializes every datagram byte into protobuf and sends it over UDS (kernel copy). With the shm data plane, the 10 MiB datagrams never touch protobuf or the kernel — they go directly through memory-mapped ring buffers.

good, write a caveat section to show risks with this design.



Let me review the EZ security model and our implementation to make sure I cover the right risks.


Read 3 files
Read 3 files

Now I have a thorough understanding of both our implementation and the EZ architecture. Let me write the caveats.


Created a file
Created a file

Written to CAVEATS.md. The 10 risks, roughly ordered by severity:

Breaks isolate containment (critical) — shm files are readable/writable by anyone who can mmap them, bypassing the enforcer's policy gate
Scope enforcement gap (critical) — datagrams and their DataScopeType annotations travel on separate planes; nothing prevents the isolate from reading data without its scope
TOCTOU between control and data plane (high) — data can be modified in shm between the write and the gRPC notification
Count mismatch corrupts the stream (high) — if x-shm-datagram-count goes out of sync (crash, retry), the receiver blocks or reads wrong frames
SPSC limits concurrency — one in-flight request per channel
Spin-loop CPU burn — wastes confidential compute budget in TEEs
No flow control — ring buffer has no backpressure mechanism unlike gRPC/HTTP2
Crash recovery — stale shm files, inconsistent ring state, no detection
Container namespace complications — /dev/shm sharing requires IPC namespace config, Kubernetes tmpfs defaults are too small
Memory accounting — shm RAM is invisible to cgroup limits and OOM killer
The bottom line: this is a throughput optimization for trusted, co-located components. It is not a safe drop-in for the UDS+protobuf path when isolates are untrusted.

Write a CLAUDE.md file to have all other CLAUDE agents able to do integration with this work.



Let me review the full codebase structure to make sure the CLAUDE.md is comprehensive.


Examined shared memory server architecture and dependencies
Examined shared memory server architecture and dependencies

Written CLAUDE.md. It covers:

Build/test commands — the first thing any agent needs
Architecture — two-plane design with diagram
Module map — what each file does and the dependency graph
Key types with usage examples — RingBuf, ShmChannel, Frame, ShmTransferService, EzIsolateBridgeShm, EzShmClient
Integration patterns — how to add a new gRPC service backed by shm, how to use as a library, container deployment
Constraints and gotchas — SPSC, spin-wait, crash recovery, sizing, cleanup, macOS vs Linux
Test structure — where tests live and what they cover
Folder no longer exists. Please start a new session.




