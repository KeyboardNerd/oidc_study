//! Minimal example: in-process echo server demonstrating the ring buffer API.

use dev_shm_server::protocol::Message;
use dev_shm_server::ring::RingBuf;

fn main() {
    // Simulate shared memory with a heap allocation
    let size = 128 + 64 * 1024; // header + 64KB
    let mut req_mem = vec![0u8; size];
    let mut resp_mem = vec![0u8; size];

    let req_ring = unsafe { RingBuf::new(req_mem.as_mut_ptr(), size) };
    let resp_ring = unsafe { RingBuf::new(resp_mem.as_mut_ptr(), size) };
    req_ring.init();
    resp_ring.init();

    // Client sends a request
    let request = Message::request(1, b"Hello from client!".to_vec());
    req_ring.push(&request.encode());
    println!("Client sent: {:?}", std::str::from_utf8(&request.payload).unwrap());

    // Server reads and responds
    let mut buf = Vec::new();
    req_ring.pop(&mut buf);
    let incoming = Message::decode(&buf).unwrap();
    let response = Message::response(
        incoming.request_id,
        format!("Echo: {}", std::str::from_utf8(&incoming.payload).unwrap())
            .into_bytes(),
    );
    resp_ring.push(&response.encode());

    // Client reads response
    resp_ring.pop(&mut buf);
    let resp = Message::decode(&buf).unwrap();
    println!(
        "Client received: {:?}",
        std::str::from_utf8(&resp.payload).unwrap()
    );
}
