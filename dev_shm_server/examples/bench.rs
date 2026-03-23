//! Unified benchmark comparing three IPC transports:
//!   1. Shared memory (lock-free ring buffer)
//!   2. Unix domain socket
//!   3. TCP loopback (127.0.0.1)
//!
//! **Every received message is verified for data integrity.**
//! Each payload byte is deterministic (derived from sequence number + position),
//! so any corruption — ring wrap bugs, partial reads, memory ordering issues —
//! is caught immediately.
//!
//! Tests payload sizes from 64B to 10MiB (inference-scale).

use dev_shm_server::protocol::{Message, MessageType};
use dev_shm_server::ring::RingBuf;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const RING_SIZE: usize = 128 + 64 * 1024 * 1024; // header + 64MB

const UDS_PATH: &str = "/tmp/shm_bench.sock";
const TCP_ADDR: &str = "127.0.0.1:0";

const PAYLOAD_SIZES: &[usize] = &[
    64,                    // cache-line
    1024,                  // 1 KiB
    64 * 1024,             // 64 KiB: L2 boundary
    256 * 1024,            // 256 KiB
    1024 * 1024,           // 1 MiB
    4 * 1024 * 1024,       // 4 MiB
    10 * 1024 * 1024,      // 10 MiB: inference-scale
];

// ─── Deterministic payload generation & verification ─────────────────────────

/// Build a payload where every byte is determined by (seq, position).
/// Any single-byte corruption is detectable.
fn make_payload(seq: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    // Write seq as the first 8 bytes (or fewer if payload < 8)
    let seq_bytes = seq.to_le_bytes();
    let header = len.min(8);
    buf[..header].copy_from_slice(&seq_bytes[..header]);
    // Fill remaining with position-dependent pattern seeded by seq
    // Use a simple xorshift-like mixing so different seqs produce different patterns
    let seed = seq.wrapping_mul(0x517cc1b727220a95).wrapping_add(0x6c62272e07bb0142);
    for i in 8..len {
        buf[i] = (seed.wrapping_add(i as u64).wrapping_mul(0x2545F4914F6CDD1D) >> 56) as u8;
    }
    buf
}

/// Verify a received message: decode it, check type, check payload matches expected.
/// Panics with detailed info on corruption.
fn verify_message(raw: &[u8], expected_seq: u64, expected_payload_len: usize) {
    let msg = Message::decode(raw).unwrap_or_else(|e| {
        panic!(
            "DECODE FAILURE at seq={}: {} (raw len={}, first 32 bytes={:?})",
            expected_seq,
            e,
            raw.len(),
            &raw[..raw.len().min(32)]
        );
    });

    assert_eq!(
        msg.msg_type,
        MessageType::Request,
        "WRONG MSG TYPE at seq={}: expected Request, got {:?}",
        expected_seq,
        msg.msg_type
    );

    assert_eq!(
        msg.payload.len(),
        expected_payload_len,
        "WRONG PAYLOAD LEN at seq={}: expected {}, got {}",
        expected_seq,
        expected_payload_len,
        msg.payload.len()
    );

    let expected = make_payload(expected_seq, expected_payload_len);
    if msg.payload != expected {
        // Find first mismatch for diagnosis
        let pos = msg
            .payload
            .iter()
            .zip(expected.iter())
            .position(|(a, b)| a != b)
            .unwrap();
        panic!(
            "DATA CORRUPTION at seq={}, byte {}/{}: got 0x{:02X}, expected 0x{:02X}",
            expected_seq, pos, expected_payload_len, msg.payload[pos], expected[pos]
        );
    }
}

/// Lighter verification for latency echo tests: check the raw bytes match exactly.
fn verify_echo(received: &[u8], sent: &[u8], iteration: usize) {
    assert_eq!(
        received.len(),
        sent.len(),
        "ECHO LEN MISMATCH at iter={}: sent {} bytes, received {}",
        iteration,
        sent.len(),
        received.len()
    );
    if received != sent {
        let pos = received
            .iter()
            .zip(sent.iter())
            .position(|(a, b)| a != b)
            .unwrap();
        panic!(
            "ECHO CORRUPTION at iter={}, byte {}/{}: got 0x{:02X}, expected 0x{:02X}",
            iteration, pos, sent.len(), received[pos], sent[pos]
        );
    }
}

// ─── Config ──────────────────────────────────────────────────────────────────

fn msg_count(payload_size: usize) -> usize {
    let total_bytes_target = 2_u64 * 1024 * 1024 * 1024;
    let count = total_bytes_target / payload_size as u64;
    (count as usize).clamp(100, 1_000_000)
}

fn latency_count(payload_size: usize) -> usize {
    if payload_size >= 4 * 1024 * 1024 {
        1_000
    } else if payload_size >= 256 * 1024 {
        5_000
    } else {
        50_000
    }
}

// ─── Formatting ──────────────────────────────────────────────────────────────

fn format_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{}MiB", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{}KiB", bytes / 1024)
    } else {
        format!("{}B", bytes)
    }
}

fn format_bandwidth(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1e9 {
        format!("{:.2} GB/s", bytes_per_sec / 1e9)
    } else {
        format!("{:.1} MB/s", bytes_per_sec / 1e6)
    }
}

struct LatencyStats {
    avg: Duration,
    p50: Duration,
    p99: Duration,
    p999: Duration,
}

fn compute_latency_stats(mut latencies: Vec<Duration>) -> LatencyStats {
    let n = latencies.len();
    latencies.sort();
    LatencyStats {
        avg: latencies.iter().sum::<Duration>() / n as u32,
        p50: latencies[n / 2],
        p99: latencies[n * 99 / 100],
        p999: latencies[n * 999 / 1000],
    }
}

struct ThroughputResult {
    payload_size: usize,
    msg_count: usize,
    elapsed: Duration,
    verified: u64,
}

impl ThroughputResult {
    fn msgs_per_sec(&self) -> f64 {
        self.msg_count as f64 / self.elapsed.as_secs_f64()
    }
    fn bytes_per_sec(&self) -> f64 {
        (self.msg_count as f64 * self.payload_size as f64) / self.elapsed.as_secs_f64()
    }
}

fn print_throughput_row(r: &ThroughputResult) {
    let check = if r.verified == r.msg_count as u64 {
        "✓"
    } else {
        "FAIL"
    };
    println!(
        "  {:>6}  {:>10.0} msg/s  {:>12}  ({:.1}ms)  [{}verified {}]",
        format_size(r.payload_size),
        r.msgs_per_sec(),
        format_bandwidth(r.bytes_per_sec()),
        r.elapsed.as_secs_f64() * 1000.0,
        check,
        r.verified,
    );
}

// ─── Socket I/O ──────────────────────────────────────────────────────────────

fn sock_write_msg(stream: &mut impl Write, data: &[u8]) {
    let len = (data.len() as u32).to_le_bytes();
    stream.write_all(&len).unwrap();
    stream.write_all(data).unwrap();
}

fn sock_read_msg(stream: &mut impl Read, buf: &mut Vec<u8>) {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).unwrap();
    let len = u32::from_le_bytes(len_bytes) as usize;
    buf.resize(len, 0);
    stream.read_exact(buf).unwrap();
}

// ─── SHM benchmarks ─────────────────────────────────────────────────────────

struct SharedMem {
    ptr: *mut u8,
    len: usize,
}
unsafe impl Send for SharedMem {}
unsafe impl Sync for SharedMem {}

impl SharedMem {
    fn new(size: usize) -> (Self, Vec<u8>) {
        let mut v = vec![0u8; size];
        let ptr = v.as_mut_ptr();
        (SharedMem { ptr, len: size }, v)
    }
    fn ring(&self) -> RingBuf {
        unsafe { RingBuf::new(self.ptr, self.len) }
    }
}

fn shm_throughput(payload_size: usize) -> ThroughputResult {
    let count = msg_count(payload_size);
    let (shared, _mem) = SharedMem::new(RING_SIZE);
    let shared = Arc::new(shared);
    shared.ring().init();

    let verified = Arc::new(AtomicU64::new(0));
    let verified2 = verified.clone();
    let shared2 = shared.clone();

    let consumer = thread::spawn(move || {
        let ring = shared2.ring();
        let mut buf = Vec::new();
        for seq in 0..count as u64 {
            loop {
                if ring.pop(&mut buf) {
                    break;
                }
                std::hint::spin_loop();
            }
            verify_message(&buf, seq, payload_size);
            verified2.fetch_add(1, AtomicOrdering::Relaxed);
        }
    });

    let ring = shared.ring();
    // Pre-encode each message with its unique seq-dependent payload
    // For throughput we encode per-seq to ensure each message is unique & verifiable
    let start = Instant::now();

    for seq in 0..count as u64 {
        let payload = make_payload(seq, payload_size);
        let encoded = Message::request(seq, payload).encode();
        while !ring.push(&encoded) {
            std::hint::spin_loop();
        }
    }

    consumer.join().unwrap();
    ThroughputResult {
        payload_size,
        msg_count: count,
        elapsed: start.elapsed(),
        verified: verified.load(AtomicOrdering::Relaxed),
    }
}

fn shm_latency(payload_size: usize) -> Vec<Duration> {
    let count = latency_count(payload_size);
    let (req_s, _rm) = SharedMem::new(RING_SIZE);
    let (resp_s, _pm) = SharedMem::new(RING_SIZE);
    let req_s = Arc::new(req_s);
    let resp_s = Arc::new(resp_s);
    req_s.ring().init();
    resp_s.ring().init();

    let rq = req_s.clone();
    let rp = resp_s.clone();
    let server = thread::spawn(move || {
        let req = rq.ring();
        let resp = rp.ring();
        let mut buf = Vec::new();
        for _ in 0..count {
            while !req.pop(&mut buf) {
                std::hint::spin_loop();
            }
            while !resp.push(&buf) {
                std::hint::spin_loop();
            }
        }
    });

    let req_ring = req_s.ring();
    let resp_ring = resp_s.ring();
    let payload = make_payload(0xA7E9C100, payload_size);
    let encoded = Message::request(0, payload).encode();
    let mut latencies = Vec::with_capacity(count);
    let mut buf = Vec::new();

    for i in 0..count {
        let t = Instant::now();
        while !req_ring.push(&encoded) {
            std::hint::spin_loop();
        }
        while !resp_ring.pop(&mut buf) {
            std::hint::spin_loop();
        }
        latencies.push(t.elapsed());
        verify_echo(&buf, &encoded, i);
    }

    server.join().unwrap();
    latencies
}

// ─── Unix domain socket benchmarks ──────────────────────────────────────────

fn uds_throughput(payload_size: usize) -> ThroughputResult {
    let count = msg_count(payload_size);
    let _ = std::fs::remove_file(UDS_PATH);
    let listener = UnixListener::bind(UDS_PATH).unwrap();

    let verified = Arc::new(AtomicU64::new(0));
    let verified2 = verified.clone();

    let consumer = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        for seq in 0..count as u64 {
            sock_read_msg(&mut conn, &mut buf);
            verify_message(&buf, seq, payload_size);
            verified2.fetch_add(1, AtomicOrdering::Relaxed);
        }
    });

    let mut stream = UnixStream::connect(UDS_PATH).unwrap();
    let start = Instant::now();

    for seq in 0..count as u64 {
        let payload = make_payload(seq, payload_size);
        let encoded = Message::request(seq, payload).encode();
        sock_write_msg(&mut stream, &encoded);
    }

    consumer.join().unwrap();
    let elapsed = start.elapsed();
    let _ = std::fs::remove_file(UDS_PATH);
    ThroughputResult {
        payload_size,
        msg_count: count,
        elapsed,
        verified: verified.load(AtomicOrdering::Relaxed),
    }
}

fn uds_latency(payload_size: usize) -> Vec<Duration> {
    let count = latency_count(payload_size);
    let _ = std::fs::remove_file(UDS_PATH);
    let listener = UnixListener::bind(UDS_PATH).unwrap();

    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        for _ in 0..count {
            sock_read_msg(&mut conn, &mut buf);
            sock_write_msg(&mut conn, &buf);
        }
    });

    let mut stream = UnixStream::connect(UDS_PATH).unwrap();
    let payload = make_payload(0xA7E9C100, payload_size);
    let encoded = Message::request(0, payload).encode();
    let mut latencies = Vec::with_capacity(count);
    let mut buf = Vec::new();

    for i in 0..count {
        let t = Instant::now();
        sock_write_msg(&mut stream, &encoded);
        sock_read_msg(&mut stream, &mut buf);
        latencies.push(t.elapsed());
        verify_echo(&buf, &encoded, i);
    }

    server.join().unwrap();
    let _ = std::fs::remove_file(UDS_PATH);
    latencies
}

// ─── TCP loopback benchmarks ────────────────────────────────────────────────

fn tcp_throughput(payload_size: usize) -> ThroughputResult {
    let count = msg_count(payload_size);
    let listener = TcpListener::bind(TCP_ADDR).unwrap();
    let addr = listener.local_addr().unwrap();

    let verified = Arc::new(AtomicU64::new(0));
    let verified2 = verified.clone();

    let consumer = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        for seq in 0..count as u64 {
            sock_read_msg(&mut conn, &mut buf);
            verify_message(&buf, seq, payload_size);
            verified2.fetch_add(1, AtomicOrdering::Relaxed);
        }
    });

    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_nodelay(true).unwrap();
    let start = Instant::now();

    for seq in 0..count as u64 {
        let payload = make_payload(seq, payload_size);
        let encoded = Message::request(seq, payload).encode();
        sock_write_msg(&mut stream, &encoded);
    }

    consumer.join().unwrap();
    ThroughputResult {
        payload_size,
        msg_count: count,
        elapsed: start.elapsed(),
        verified: verified.load(AtomicOrdering::Relaxed),
    }
}

fn tcp_latency(payload_size: usize) -> Vec<Duration> {
    let count = latency_count(payload_size);
    let listener = TcpListener::bind(TCP_ADDR).unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        conn.set_nodelay(true).unwrap();
        let mut buf = Vec::new();
        for _ in 0..count {
            sock_read_msg(&mut conn, &mut buf);
            sock_write_msg(&mut conn, &buf);
        }
    });

    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_nodelay(true).unwrap();
    let payload = make_payload(0xA7E9C100, payload_size);
    let encoded = Message::request(0, payload).encode();
    let mut latencies = Vec::with_capacity(count);
    let mut buf = Vec::new();

    for i in 0..count {
        let t = Instant::now();
        sock_write_msg(&mut stream, &encoded);
        sock_read_msg(&mut stream, &mut buf);
        latencies.push(t.elapsed());
        verify_echo(&buf, &encoded, i);
    }

    server.join().unwrap();
    latencies
}

// ─── Main ───────────────────────────────────────────────────────────────────

fn main() {
    println!("================================================================");
    println!("  IPC Transport Benchmark (with data integrity verification)");
    println!("  SHM vs Unix Socket vs TCP | 64B → 10 MiB");
    println!("  Every message verified: seq-dependent payload, byte-by-byte");
    println!("================================================================\n");

    // ── Throughput ──
    println!("── Throughput (unidirectional, each message uniquely verified) ──\n");

    let mut shm_results = Vec::new();
    let mut uds_results = Vec::new();
    let mut tcp_results = Vec::new();

    println!("[SHM] Shared memory ring buffer:");
    for &size in PAYLOAD_SIZES {
        let r = shm_throughput(size);
        print_throughput_row(&r);
        shm_results.push(r);
    }

    println!("\n[UDS] Unix domain socket:");
    for &size in PAYLOAD_SIZES {
        let r = uds_throughput(size);
        print_throughput_row(&r);
        uds_results.push(r);
    }

    println!("\n[TCP] TCP loopback (127.0.0.1, nodelay):");
    for &size in PAYLOAD_SIZES {
        let r = tcp_throughput(size);
        print_throughput_row(&r);
        tcp_results.push(r);
    }

    // ── Bandwidth comparison ──
    println!("\n── Bandwidth comparison (payload data only) ──\n");
    println!(
        "  {:>6}  {:>12}  {:>12}  {:>12}  {:>8}  {:>8}",
        "Size", "SHM", "UDS", "TCP", "SHM/UDS", "SHM/TCP"
    );
    println!(
        "  {:->6}  {:->12}  {:->12}  {:->12}  {:->8}  {:->8}",
        "", "", "", "", "", ""
    );
    for i in 0..PAYLOAD_SIZES.len() {
        let shm_bw = shm_results[i].bytes_per_sec();
        let uds_bw = uds_results[i].bytes_per_sec();
        let tcp_bw = tcp_results[i].bytes_per_sec();
        println!(
            "  {:>6}  {:>12}  {:>12}  {:>12}  {:>7.1}x  {:>7.1}x",
            format_size(PAYLOAD_SIZES[i]),
            format_bandwidth(shm_bw),
            format_bandwidth(uds_bw),
            format_bandwidth(tcp_bw),
            shm_bw / uds_bw,
            shm_bw / tcp_bw,
        );
    }

    // ── Latency ──
    println!("\n── Latency (roundtrip, echo verified byte-for-byte) ──\n");
    println!(
        "  {:>6}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}",
        "Size", "SHM p50", "SHM p99", "UDS p50", "UDS p99", "TCP p50", "TCP p99"
    );
    println!(
        "  {:->6}  {:->12}  {:->12}  {:->12}  {:->12}  {:->12}  {:->12}",
        "", "", "", "", "", "", ""
    );

    for &size in PAYLOAD_SIZES {
        let shm = compute_latency_stats(shm_latency(size));
        let uds = compute_latency_stats(uds_latency(size));
        let tcp = compute_latency_stats(tcp_latency(size));
        println!(
            "  {:>6}  {:>10.3?}  {:>10.3?}  {:>10.3?}  {:>10.3?}  {:>10.3?}  {:>10.3?}",
            format_size(size),
            shm.p50, shm.p99,
            uds.p50, uds.p99,
            tcp.p50, tcp.p99,
        );
    }

    // ── Summary ──
    println!("\n── Integrity summary ──\n");
    let total_shm: u64 = shm_results.iter().map(|r| r.verified).sum();
    let total_uds: u64 = uds_results.iter().map(|r| r.verified).sum();
    let total_tcp: u64 = tcp_results.iter().map(|r| r.verified).sum();
    let expected_shm: u64 = PAYLOAD_SIZES.iter().map(|&s| msg_count(s) as u64).sum();
    println!(
        "  SHM: {}/{} messages verified",
        total_shm, expected_shm
    );
    println!(
        "  UDS: {}/{} messages verified",
        total_uds, expected_shm
    );
    println!(
        "  TCP: {}/{} messages verified",
        total_tcp, expected_shm
    );

    if total_shm == expected_shm && total_uds == expected_shm && total_tcp == expected_shm {
        println!("\n  ALL DATA INTEGRITY CHECKS PASSED");
    } else {
        println!("\n  WARNING: SOME INTEGRITY CHECKS FAILED");
        std::process::exit(1);
    }

    println!("\n================================================================");
}
