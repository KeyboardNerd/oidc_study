//! shm-server: A shared memory request/response server.
//!
//! Reads requests from a ring buffer on /dev/shm, dispatches to a handler,
//! and writes responses back on a separate ring buffer.
//!
//! Usage:
//!   shm-server [--shm-dir /dev/shm] [--size 16777216] [--name myserver]

use dev_shm_server::protocol::{Message, MessageType};
use dev_shm_server::ring::RingBuf;
use dev_shm_server::shm::ShmRegion;
use log::{error, info, warn};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const DEFAULT_SHM_DIR: &str = if cfg!(target_os = "linux") {
    "/dev/shm"
} else {
    "/tmp"
};
const DEFAULT_SIZE: usize = 16 * 1024 * 1024; // 16 MB per ring
const DEFAULT_NAME: &str = "shm_server";

struct Config {
    shm_dir: PathBuf,
    size: usize,
    name: String,
}

fn parse_args() -> Config {
    let args: Vec<String> = std::env::args().collect();
    let mut config = Config {
        shm_dir: PathBuf::from(DEFAULT_SHM_DIR),
        size: DEFAULT_SIZE,
        name: DEFAULT_NAME.to_string(),
    };

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--shm-dir" => {
                i += 1;
                config.shm_dir = PathBuf::from(&args[i]);
            }
            "--size" => {
                i += 1;
                config.size = args[i].parse().expect("invalid size");
            }
            "--name" => {
                i += 1;
                config.name = args[i].clone();
            }
            "--help" | "-h" => {
                eprintln!("Usage: shm-server [--shm-dir DIR] [--size BYTES] [--name NAME]");
                eprintln!("  --shm-dir  Shared memory directory (default: {})", DEFAULT_SHM_DIR);
                eprintln!("  --size     Ring buffer size in bytes (default: {})", DEFAULT_SIZE);
                eprintln!("  --name     Server name / shm file prefix (default: {})", DEFAULT_NAME);
                std::process::exit(0);
            }
            other => {
                eprintln!("Unknown argument: {}", other);
                std::process::exit(1);
            }
        }
        i += 1;
    }
    config
}

/// Default request handler: echoes the payload back as JSON with metadata.
fn handle_request(msg: &Message) -> Message {
    // Try to parse as JSON command
    if let Ok(text) = std::str::from_utf8(&msg.payload) {
        if let Ok(cmd) = serde_json::from_str::<serde_json::Value>(text) {
            let action = cmd.get("action").and_then(|a| a.as_str()).unwrap_or("echo");

            match action {
                "echo" => {
                    let response = serde_json::json!({
                        "status": "ok",
                        "echo": cmd.get("data").unwrap_or(&serde_json::Value::Null),
                    });
                    return Message::response(
                        msg.request_id,
                        response.to_string().into_bytes(),
                    );
                }
                "ping" => {
                    let response = serde_json::json!({ "status": "pong" });
                    return Message::response(
                        msg.request_id,
                        response.to_string().into_bytes(),
                    );
                }
                "info" => {
                    let response = serde_json::json!({
                        "status": "ok",
                        "server": "dev_shm_server",
                        "version": env!("CARGO_PKG_VERSION"),
                        "pid": std::process::id(),
                    });
                    return Message::response(
                        msg.request_id,
                        response.to_string().into_bytes(),
                    );
                }
                unknown => {
                    return Message::error(
                        msg.request_id,
                        &format!("unknown action: {}", unknown),
                    );
                }
            }
        }
    }

    // Fallback: raw echo
    Message::response(msg.request_id, msg.payload.clone())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = parse_args();

    let req_name = format!("{}_req", config.name);
    let resp_name = format!("{}_resp", config.name);

    info!(
        "Creating shared memory rings in {} (size: {} bytes each)",
        config.shm_dir.display(),
        config.size
    );

    // Create request and response ring buffer regions
    let mut req_region =
        ShmRegion::create(&config.shm_dir, &req_name, config.size).expect("failed to create request shm");
    let mut resp_region =
        ShmRegion::create(&config.shm_dir, &resp_name, config.size).expect("failed to create response shm");

    let req_ring = unsafe { RingBuf::new(req_region.as_mut_ptr(), req_region.len()) };
    let resp_ring = unsafe { RingBuf::new(resp_region.as_mut_ptr(), resp_region.len()) };

    // Initialize ring headers
    req_ring.init();
    resp_ring.init();

    info!("Server '{}' ready. Waiting for requests...", config.name);
    info!("  Request ring:  {}", config.shm_dir.join(&req_name).display());
    info!("  Response ring: {}", config.shm_dir.join(&resp_name).display());

    // Graceful shutdown on Ctrl+C
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        info!("Shutting down...");
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let mut buf = Vec::new();
    let mut idle_count = 0u32;

    while running.load(Ordering::Relaxed) {
        if req_ring.pop(&mut buf) {
            idle_count = 0;

            match Message::decode(&buf) {
                Ok(msg) => {
                    if msg.msg_type == MessageType::Shutdown {
                        info!("Received shutdown message");
                        break;
                    }

                    let response = handle_request(&msg);
                    let encoded = response.encode();

                    if !resp_ring.push(&encoded) {
                        warn!(
                            "Response ring full, dropping response for request {}",
                            msg.request_id
                        );
                    }
                }
                Err(e) => {
                    error!("Failed to decode message: {}", e);
                }
            }
        } else {
            // Adaptive backoff: spin briefly, then yield, then sleep
            idle_count = idle_count.saturating_add(1);
            if idle_count < 100 {
                std::hint::spin_loop();
            } else if idle_count < 1000 {
                thread::yield_now();
            } else {
                thread::sleep(Duration::from_micros(100));
            }
        }
    }

    // Cleanup
    info!("Cleaning up shared memory files...");
    if let Err(e) = req_region.unlink() {
        warn!("Failed to unlink request shm: {}", e);
    }
    if let Err(e) = resp_region.unlink() {
        warn!("Failed to unlink response shm: {}", e);
    }
    info!("Server stopped.");
}
