//! Resource usage benchmark: measures RAM (RSS), CPU time, and shared memory
//! overhead for each IPC transport.
//!
//! For each transport and payload size:
//!   - Measures process RSS before, during, and after the transfer
//!   - Tracks peak RSS (via getrusage maxrss)
//!   - Measures user + system CPU time consumed
//!   - For SHM: reports the actual mmap'd file sizes
//!   - All data is integrity-verified

use dev_shm_server::protocol::{Message, MessageType};
use dev_shm_server::ring::RingBuf;
use dev_shm_server::shm::ShmRegion;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Wrapper to send raw pointer across threads.
struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

const UDS_PATH: &str = "/tmp/shm_resource_bench.sock";
const TCP_ADDR: &str = "127.0.0.1:0";
const SHM_DIR: &str = "/tmp";
const SHM_NAME: &str = "resource_bench";

const PAYLOAD_SIZES: &[usize] = &[
    64,
    1024,
    64 * 1024,
    1 * 1024 * 1024,
    10 * 1024 * 1024,
];

/// Number of messages per test — enough to be meaningful, not so many it takes forever.
fn msg_count(payload_size: usize) -> usize {
    let target = 256 * 1024 * 1024_u64; // 256 MiB total
    (target / payload_size as u64).clamp(50, 200_000) as usize
}

// ─── Resource measurement ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ResourceSnapshot {
    rss_bytes: u64,
    user_time: Duration,
    sys_time: Duration,
}

fn get_resources() -> ResourceSnapshot {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
    }

    // On macOS, ru_maxrss is in bytes. On Linux, it's in kilobytes.
    // We'll measure current RSS via task_info on macOS.
    let rss = get_current_rss();

    let user_time = Duration::new(
        usage.ru_utime.tv_sec as u64,
        usage.ru_utime.tv_usec as u32 * 1000,
    );
    let sys_time = Duration::new(
        usage.ru_stime.tv_sec as u64,
        usage.ru_stime.tv_usec as u32 * 1000,
    );

    ResourceSnapshot {
        rss_bytes: rss,
        user_time,
        sys_time,
    }
}

#[cfg(target_os = "macos")]
fn get_current_rss() -> u64 {
    use std::mem;
    let mut info: libc::mach_task_basic_info_data_t = unsafe { mem::zeroed() };
    let mut count = (mem::size_of::<libc::mach_task_basic_info_data_t>()
        / mem::size_of::<libc::natural_t>()) as libc::mach_msg_type_number_t;
    let ret = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as *mut _,
            &mut count,
        )
    };
    if ret == libc::KERN_SUCCESS {
        info.resident_size as u64
    } else {
        0
    }
}

#[cfg(target_os = "linux")]
fn get_current_rss() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|pages| pages * 4096)
        .unwrap_or(0)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn get_current_rss() -> u64 {
    0
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn format_size_short(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{}MiB", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{}KiB", bytes / 1024)
    } else {
        format!("{}B", bytes)
    }
}

#[derive(Debug)]
struct ResourceResult {
    transport: String,
    payload_size: usize,
    msg_count: usize,
    wall_time: Duration,
    rss_before: u64,
    rss_peak: u64,
    rss_after: u64,
    cpu_user: Duration,
    cpu_sys: Duration,
    shm_file_bytes: u64, // only for SHM transport
    verified: u64,
}

// ─── Payload helpers ─────────────────────────────────────────────────────────

fn make_payload(seq: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let seq_bytes = seq.to_le_bytes();
    let header = len.min(8);
    buf[..header].copy_from_slice(&seq_bytes[..header]);
    let seed = seq.wrapping_mul(0x517cc1b727220a95).wrapping_add(0x6c62272e07bb0142);
    for i in 8..len {
        buf[i] = (seed.wrapping_add(i as u64).wrapping_mul(0x2545F4914F6CDD1D) >> 56) as u8;
    }
    buf
}

fn verify_message(raw: &[u8], expected_seq: u64, expected_payload_len: usize) {
    let msg = Message::decode(raw).unwrap_or_else(|e| {
        panic!("DECODE FAILURE at seq={}: {}", expected_seq, e);
    });
    assert_eq!(msg.msg_type, MessageType::Request);
    assert_eq!(msg.payload.len(), expected_payload_len);
    let expected = make_payload(expected_seq, expected_payload_len);
    assert_eq!(msg.payload, expected, "DATA CORRUPTION at seq={}", expected_seq);
}

// ─── Socket helpers ──────────────────────────────────────────────────────────

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

// ─── SHM test ────────────────────────────────────────────────────────────────

fn run_shm(payload_size: usize) -> ResourceResult {
    let count = msg_count(payload_size);
    // Ring must fit at least one max message: 13 + payload_size + 4 (ring framing)
    // Use 3x message size, rounded up to power of 2, minimum 1 MiB
    let ring_data_size = ((payload_size + 64) * 3).next_power_of_two().max(1024 * 1024);
    let ring_total = 128 + ring_data_size;

    let before = get_resources();

    let req_name = format!("{}_req", SHM_NAME);
    let resp_name = format!("{}_resp", SHM_NAME);
    let shm_dir = Path::new(SHM_DIR);

    let mut req_region = ShmRegion::create(shm_dir, &req_name, ring_total).unwrap();
    let mut resp_region = ShmRegion::create(shm_dir, &resp_name, ring_total).unwrap();

    let shm_file_bytes = (req_region.len() + resp_region.len()) as u64;

    // Wrap pointers in SendPtr for cross-thread sharing.
    // The ShmRegion (and its mmap) must outlive both threads — we keep them alive below.
    let req_sp = Arc::new(SendPtr(req_region.as_mut_ptr()));
    let resp_sp = Arc::new(SendPtr(resp_region.as_mut_ptr()));
    let req_len = req_region.len();
    let resp_len = resp_region.len();

    {
        let req_ring = unsafe { RingBuf::new(req_sp.0, req_len) };
        let resp_ring = unsafe { RingBuf::new(resp_sp.0, resp_len) };
        req_ring.init();
        resp_ring.init();
    }

    let verified = Arc::new(AtomicU64::new(0));
    let v2 = verified.clone();
    let peak_rss = Arc::new(AtomicU64::new(0));
    let peak2 = peak_rss.clone();

    let req_sp2 = req_sp.clone();
    let resp_sp2 = resp_sp.clone();

    let consumer = thread::spawn(move || {
        let req = unsafe { RingBuf::new(req_sp2.0, req_len) };
        let resp = unsafe { RingBuf::new(resp_sp2.0, resp_len) };
        let mut buf = Vec::new();
        for seq in 0..count as u64 {
            loop {
                if req.pop(&mut buf) { break; }
                std::hint::spin_loop();
            }
            verify_message(&buf, seq, payload_size);
            v2.fetch_add(1, Ordering::Relaxed);

            while !resp.push(&buf) {
                std::hint::spin_loop();
            }

            if seq % 1000 == 0 {
                peak2.fetch_max(get_current_rss(), Ordering::Relaxed);
            }
        }
    });

    let req_ring = unsafe { RingBuf::new(req_sp.0, req_len) };
    let resp_ring = unsafe { RingBuf::new(resp_sp.0, resp_len) };

    let start = Instant::now();

    let mut buf = Vec::new();
    for seq in 0..count as u64 {
        let payload = make_payload(seq, payload_size);
        let encoded = Message::request(seq, payload).encode();
        while !req_ring.push(&encoded) {
            std::hint::spin_loop();
        }
        while !resp_ring.pop(&mut buf) {
            std::hint::spin_loop();
        }

        if seq % 1000 == 0 {
            peak_rss.fetch_max(get_current_rss(), Ordering::Relaxed);
        }
    }

    consumer.join().unwrap();
    let wall_time = start.elapsed();

    let after = get_resources();

    // Final peak sample
    let final_rss = get_current_rss();
    peak_rss.fetch_max(final_rss, Ordering::Relaxed);

    // Cleanup
    let _ = req_region.unlink();
    let _ = resp_region.unlink();

    let rss_after_cleanup = get_current_rss();

    ResourceResult {
        transport: "SHM".to_string(),
        payload_size,
        msg_count: count,
        wall_time,
        rss_before: before.rss_bytes,
        rss_peak: peak_rss.load(Ordering::Relaxed),
        rss_after: rss_after_cleanup,
        cpu_user: after.user_time - before.user_time,
        cpu_sys: after.sys_time - before.sys_time,
        shm_file_bytes,
        verified: verified.load(Ordering::Relaxed),
    }
}

// ─── UDS test ────────────────────────────────────────────────────────────────

fn run_uds(payload_size: usize) -> ResourceResult {
    let count = msg_count(payload_size);
    let before = get_resources();

    let _ = std::fs::remove_file(UDS_PATH);
    let listener = UnixListener::bind(UDS_PATH).unwrap();

    let verified = Arc::new(AtomicU64::new(0));
    let v2 = verified.clone();
    let peak_rss = Arc::new(AtomicU64::new(0));
    let peak2 = peak_rss.clone();

    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        for seq in 0..count as u64 {
            sock_read_msg(&mut conn, &mut buf);
            verify_message(&buf, seq, payload_size);
            v2.fetch_add(1, Ordering::Relaxed);
            sock_write_msg(&mut conn, &buf); // echo

            if seq % 1000 == 0 {
                peak2.fetch_max(get_current_rss(), Ordering::Relaxed);
            }
        }
    });

    let mut stream = UnixStream::connect(UDS_PATH).unwrap();
    let start = Instant::now();
    let mut buf = Vec::new();

    for seq in 0..count as u64 {
        let payload = make_payload(seq, payload_size);
        let encoded = Message::request(seq, payload).encode();
        sock_write_msg(&mut stream, &encoded);
        sock_read_msg(&mut stream, &mut buf); // wait for echo

        if seq % 1000 == 0 {
            peak_rss.fetch_max(get_current_rss(), Ordering::Relaxed);
        }
    }

    server.join().unwrap();
    let wall_time = start.elapsed();
    let after = get_resources();
    let _ = std::fs::remove_file(UDS_PATH);

    ResourceResult {
        transport: "UDS".to_string(),
        payload_size,
        msg_count: count,
        wall_time,
        rss_before: before.rss_bytes,
        rss_peak: peak_rss.load(Ordering::Relaxed),
        rss_after: get_current_rss(),
        cpu_user: after.user_time - before.user_time,
        cpu_sys: after.sys_time - before.sys_time,
        shm_file_bytes: 0,
        verified: verified.load(Ordering::Relaxed),
    }
}

// ─── TCP test ────────────────────────────────────────────────────────────────

fn run_tcp(payload_size: usize) -> ResourceResult {
    let count = msg_count(payload_size);
    let before = get_resources();

    let listener = TcpListener::bind(TCP_ADDR).unwrap();
    let addr = listener.local_addr().unwrap();

    let verified = Arc::new(AtomicU64::new(0));
    let v2 = verified.clone();
    let peak_rss = Arc::new(AtomicU64::new(0));
    let peak2 = peak_rss.clone();

    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        conn.set_nodelay(true).unwrap();
        let mut buf = Vec::new();
        for seq in 0..count as u64 {
            sock_read_msg(&mut conn, &mut buf);
            verify_message(&buf, seq, payload_size);
            v2.fetch_add(1, Ordering::Relaxed);
            sock_write_msg(&mut conn, &buf);

            if seq % 1000 == 0 {
                peak2.fetch_max(get_current_rss(), Ordering::Relaxed);
            }
        }
    });

    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_nodelay(true).unwrap();
    let start = Instant::now();
    let mut buf = Vec::new();

    for seq in 0..count as u64 {
        let payload = make_payload(seq, payload_size);
        let encoded = Message::request(seq, payload).encode();
        sock_write_msg(&mut stream, &encoded);
        sock_read_msg(&mut stream, &mut buf);

        if seq % 1000 == 0 {
            peak_rss.fetch_max(get_current_rss(), Ordering::Relaxed);
        }
    }

    server.join().unwrap();
    let wall_time = start.elapsed();
    let after = get_resources();

    ResourceResult {
        transport: "TCP".to_string(),
        payload_size,
        msg_count: count,
        wall_time,
        rss_before: before.rss_bytes,
        rss_peak: peak_rss.load(Ordering::Relaxed),
        rss_after: get_current_rss(),
        cpu_user: after.user_time - before.user_time,
        cpu_sys: after.sys_time - before.sys_time,
        shm_file_bytes: 0,
        verified: verified.load(Ordering::Relaxed),
    }
}

// ─── Main ───────────────────────────────────────────────────────────────────

fn print_result(r: &ResourceResult) {
    let rss_delta = r.rss_peak as i64 - r.rss_before as i64;
    let cpu_total = r.cpu_user + r.cpu_sys;
    let bw = (r.msg_count as f64 * r.payload_size as f64) / r.wall_time.as_secs_f64();
    let bw_str = if bw >= 1e9 {
        format!("{:.2} GB/s", bw / 1e9)
    } else {
        format!("{:.1} MB/s", bw / 1e6)
    };

    // CPU efficiency: how much CPU time per byte transferred
    let total_bytes = r.msg_count as f64 * r.payload_size as f64;
    let cpu_ns_per_byte = cpu_total.as_nanos() as f64 / total_bytes;

    println!(
        "  {:>6}  {:>10}  {:>10}  {:>10}  {:>8.1}ms {:>8.1}ms  {:>7.2} ns/B  {:>10}  [✓{}]",
        format_size_short(r.payload_size),
        format_bytes(r.rss_peak),
        if rss_delta >= 0 {
            format!("+{}", format_bytes(rss_delta as u64))
        } else {
            format!("-{}", format_bytes((-rss_delta) as u64))
        },
        if r.shm_file_bytes > 0 {
            format_bytes(r.shm_file_bytes)
        } else {
            "-".to_string()
        },
        r.cpu_user.as_secs_f64() * 1000.0,
        r.cpu_sys.as_secs_f64() * 1000.0,
        cpu_ns_per_byte,
        bw_str,
        r.verified,
    );
}

fn main() {
    println!("================================================================");
    println!("  Resource Usage Benchmark: RAM + CPU per IPC Transport");
    println!("  All messages echo'd (roundtrip) + integrity verified");
    println!("  Target: ~256 MiB total data per payload size");
    println!("================================================================\n");

    let baseline_rss = get_current_rss();
    println!("  Baseline process RSS: {}\n", format_bytes(baseline_rss));

    for transport in &["SHM", "UDS", "TCP"] {
        println!(
            "[{}]  {:>6}  {:>10}  {:>10}  {:>10}  {:>8}  {:>8}   {:>10}  {:>10}",
            transport, "Payload", "Peak RSS", "RSS Δ", "SHM files",
            "CPU usr", "CPU sys", "CPU/byte", "Bandwidth"
        );
        println!(
            "      {:->6}  {:->10}  {:->10}  {:->10}  {:->8}  {:->8}  {:->11}  {:->10}",
            "", "", "", "", "", "", "", ""
        );

        for &size in PAYLOAD_SIZES {
            let r = match *transport {
                "SHM" => run_shm(size),
                "UDS" => run_uds(size),
                "TCP" => run_tcp(size),
                _ => unreachable!(),
            };
            print_result(&r);
        }
        println!();
    }

    // ── Summary comparison at 10 MiB ──
    println!("── 10 MiB payload comparison (inference-scale) ──\n");
    let shm = run_shm(10 * 1024 * 1024);
    let uds = run_uds(10 * 1024 * 1024);
    let tcp = run_tcp(10 * 1024 * 1024);

    println!("  {:>12}  {:>12}  {:>12}  {:>12}", "", "SHM", "UDS", "TCP");
    println!("  {:->12}  {:->12}  {:->12}  {:->12}", "", "", "", "");
    println!(
        "  {:>12}  {:>12}  {:>12}  {:>12}",
        "Peak RSS",
        format_bytes(shm.rss_peak),
        format_bytes(uds.rss_peak),
        format_bytes(tcp.rss_peak),
    );
    println!(
        "  {:>12}  {:>12}  {:>12}  {:>12}",
        "SHM files",
        format_bytes(shm.shm_file_bytes),
        "-",
        "-",
    );
    println!(
        "  {:>12}  {:>12}  {:>12}  {:>12}",
        "CPU user",
        format!("{:.1}ms", shm.cpu_user.as_secs_f64() * 1000.0),
        format!("{:.1}ms", uds.cpu_user.as_secs_f64() * 1000.0),
        format!("{:.1}ms", tcp.cpu_user.as_secs_f64() * 1000.0),
    );
    println!(
        "  {:>12}  {:>12}  {:>12}  {:>12}",
        "CPU sys",
        format!("{:.1}ms", shm.cpu_sys.as_secs_f64() * 1000.0),
        format!("{:.1}ms", uds.cpu_sys.as_secs_f64() * 1000.0),
        format!("{:.1}ms", tcp.cpu_sys.as_secs_f64() * 1000.0),
    );
    let shm_cpu_total = shm.cpu_user + shm.cpu_sys;
    let uds_cpu_total = uds.cpu_user + uds.cpu_sys;
    let tcp_cpu_total = tcp.cpu_user + tcp.cpu_sys;
    println!(
        "  {:>12}  {:>12}  {:>12}  {:>12}",
        "CPU total",
        format!("{:.1}ms", shm_cpu_total.as_secs_f64() * 1000.0),
        format!("{:.1}ms", uds_cpu_total.as_secs_f64() * 1000.0),
        format!("{:.1}ms", tcp_cpu_total.as_secs_f64() * 1000.0),
    );
    println!(
        "  {:>12}  {:>12}  {:>12}  {:>12}",
        "Wall time",
        format!("{:.1}ms", shm.wall_time.as_secs_f64() * 1000.0),
        format!("{:.1}ms", uds.wall_time.as_secs_f64() * 1000.0),
        format!("{:.1}ms", tcp.wall_time.as_secs_f64() * 1000.0),
    );
    let total_data = shm.msg_count as f64 * shm.payload_size as f64;
    println!(
        "  {:>12}  {:>12}  {:>12}  {:>12}",
        "CPU/byte",
        format!(
            "{:.2} ns/B",
            shm_cpu_total.as_nanos() as f64 / total_data
        ),
        format!(
            "{:.2} ns/B",
            uds_cpu_total.as_nanos() as f64 / total_data
        ),
        format!(
            "{:.2} ns/B",
            tcp_cpu_total.as_nanos() as f64 / total_data
        ),
    );
    println!(
        "  {:>12}  {:>12}  {:>12}  {:>12}",
        "Verified",
        shm.verified,
        uds.verified,
        tcp.verified,
    );

    // ── Key insight ──
    println!("\n── Analysis ──\n");
    println!("  SHM memory overhead = ring buffer files (mmap'd, backed by RAM on tmpfs).");
    println!("  These files exist only while the server is running and are unlinked on cleanup.");
    println!("  The ring size is proportional to the max message size, NOT total data transferred.");
    println!();
    println!("  Socket transports use kernel buffers (so_sndbuf/so_rcvbuf) which also consume");
    println!("  RAM but are hidden from process RSS — they show up in kernel slab caches.");
    println!();
    println!("  CPU sys time = time spent in kernel. SHM should have near-zero sys time");
    println!("  since it bypasses the kernel entirely for the data path.");

    println!("\n================================================================");
}
