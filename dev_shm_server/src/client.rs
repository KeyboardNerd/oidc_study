//! shm-client: Interactive client for the shared memory server.
//!
//! Usage:
//!   shm-client [--shm-dir /dev/shm] [--name myserver] [COMMAND]
//!
//! Commands:
//!   ping                   Send a ping
//!   echo <message>         Echo a message
//!   info                   Get server info
//!   send <raw bytes>       Send raw payload
//!   shutdown               Tell the server to stop
//!   interactive            Enter interactive mode (default)

use dev_shm_server::protocol::{Message, MessageType};
use dev_shm_server::ring::RingBuf;
use dev_shm_server::shm::ShmRegion;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_SHM_DIR: &str = if cfg!(target_os = "linux") {
    "/dev/shm"
} else {
    "/tmp"
};
const DEFAULT_NAME: &str = "shm_server";
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

struct Client {
    req_ring: RingBuf,
    resp_ring: RingBuf,
    next_id: AtomicU64,
    // Keep regions alive
    _req_region: ShmRegion,
    _resp_region: ShmRegion,
}

impl Client {
    fn open(shm_dir: &PathBuf, name: &str) -> io::Result<Self> {
        let req_name = format!("{}_req", name);
        let resp_name = format!("{}_resp", name);

        let mut req_region = ShmRegion::open(shm_dir, &req_name)?;
        let mut resp_region = ShmRegion::open(shm_dir, &resp_name)?;

        let req_ring = unsafe { RingBuf::new(req_region.as_mut_ptr(), req_region.len()) };
        let resp_ring = unsafe { RingBuf::new(resp_region.as_mut_ptr(), resp_region.len()) };

        Ok(Client {
            req_ring,
            resp_ring,
            next_id: AtomicU64::new(1),
            _req_region: req_region,
            _resp_region: resp_region,
        })
    }

    fn send(&self, msg: Message) -> io::Result<Option<Message>> {
        let encoded = msg.encode();
        if !self.req_ring.push(&encoded) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "request ring full"));
        }

        if msg.msg_type == MessageType::Shutdown {
            return Ok(None);
        }

        // Wait for response
        let start = Instant::now();
        let mut buf = Vec::new();
        loop {
            if self.resp_ring.pop(&mut buf) {
                match Message::decode(&buf) {
                    Ok(resp) => return Ok(Some(resp)),
                    Err(e) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("decode error: {}", e),
                        ))
                    }
                }
            }

            if start.elapsed() > RESPONSE_TIMEOUT {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "response timeout"));
            }

            thread::yield_now();
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn send_json_action(&self, action: &str, data: Option<&str>) -> io::Result<()> {
        let id = self.next_id();
        let payload = if let Some(d) = data {
            serde_json::json!({ "action": action, "data": d }).to_string()
        } else {
            serde_json::json!({ "action": action }).to_string()
        };

        let msg = Message::request(id, payload.into_bytes());
        let start = Instant::now();
        match self.send(msg)? {
            Some(resp) => {
                let elapsed = start.elapsed();
                match resp.msg_type {
                    MessageType::Response => {
                        let text = String::from_utf8_lossy(&resp.payload);
                        println!("[{:.3}ms] {}", elapsed.as_secs_f64() * 1000.0, text);
                    }
                    MessageType::Error => {
                        let text = String::from_utf8_lossy(&resp.payload);
                        eprintln!("[{:.3}ms] ERROR: {}", elapsed.as_secs_f64() * 1000.0, text);
                    }
                    _ => {
                        println!("[{:.3}ms] Unexpected response type", elapsed.as_secs_f64() * 1000.0);
                    }
                }
            }
            None => {}
        }
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut shm_dir = PathBuf::from(DEFAULT_SHM_DIR);
    let mut name = DEFAULT_NAME.to_string();
    let mut command_args: Vec<String> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--shm-dir" => {
                i += 1;
                shm_dir = PathBuf::from(&args[i]);
            }
            "--name" => {
                i += 1;
                name = args[i].clone();
            }
            "--help" | "-h" => {
                eprintln!("Usage: shm-client [--shm-dir DIR] [--name NAME] [COMMAND [ARGS...]]");
                eprintln!("Commands: ping, echo <msg>, info, shutdown, interactive (default)");
                std::process::exit(0);
            }
            _ => {
                command_args = args[i..].to_vec();
                break;
            }
        }
        i += 1;
    }

    let client = match Client::open(&shm_dir, &name) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to connect to server '{}': {}", name, e);
            eprintln!("Is the server running? Start it with: shm-server --name {}", name);
            std::process::exit(1);
        }
    };

    if command_args.is_empty() {
        // Interactive mode
        interactive_mode(&client);
    } else {
        // Single command mode
        let cmd = command_args[0].as_str();
        match cmd {
            "ping" => client.send_json_action("ping", None).unwrap(),
            "echo" => {
                let msg = command_args[1..].join(" ");
                client.send_json_action("echo", Some(&msg)).unwrap();
            }
            "info" => client.send_json_action("info", None).unwrap(),
            "shutdown" => {
                let msg = Message::shutdown();
                client.send(msg).unwrap();
                println!("Shutdown signal sent.");
            }
            "send" => {
                let payload = command_args[1..].join(" ");
                let id = client.next_id();
                let msg = Message::request(id, payload.into_bytes());
                match client.send(msg) {
                    Ok(Some(resp)) => {
                        println!("{}", String::from_utf8_lossy(&resp.payload));
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("Error: {}", e),
                }
            }
            _ => {
                eprintln!("Unknown command: {}", cmd);
                std::process::exit(1);
            }
        }
    }
}

fn interactive_mode(client: &Client) {
    println!("Connected to shm server. Type 'help' for commands, 'quit' to exit.");
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("shm> ");
        stdout.flush().unwrap();

        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap() == 0 {
            break; // EOF
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        match parts[0] {
            "quit" | "exit" => break,
            "help" => {
                println!("Commands:");
                println!("  ping              Check server is alive");
                println!("  echo <message>    Echo a message back");
                println!("  info              Get server info");
                println!("  shutdown          Stop the server");
                println!("  quit              Exit client");
            }
            "ping" => {
                if let Err(e) = client.send_json_action("ping", None) {
                    eprintln!("Error: {}", e);
                }
            }
            "echo" => {
                let data = parts.get(1).unwrap_or(&"");
                if let Err(e) = client.send_json_action("echo", Some(data)) {
                    eprintln!("Error: {}", e);
                }
            }
            "info" => {
                if let Err(e) = client.send_json_action("info", None) {
                    eprintln!("Error: {}", e);
                }
            }
            "shutdown" => {
                let msg = Message::shutdown();
                match client.send(msg) {
                    Ok(_) => println!("Shutdown signal sent."),
                    Err(e) => eprintln!("Error: {}", e),
                }
                break;
            }
            _ => {
                eprintln!("Unknown command: {}. Type 'help' for available commands.", parts[0]);
            }
        }
    }
}
