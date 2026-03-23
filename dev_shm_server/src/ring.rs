//! Lock-free SPSC (single-producer, single-consumer) ring buffer
//! that lives in shared memory.
//!
//! Memory layout:
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │ Header (128 bytes, cache-line padded)            │
//! │   write_pos: AtomicU64  (offset 0)               │
//! │   _pad1:     [u8; 56]                            │
//! │   read_pos:  AtomicU64  (offset 64)              │
//! │   _pad2:     [u8; 56]                            │
//! ├─────────────────────────────────────────────────┤
//! │ Data ring (capacity bytes, MUST be power of 2)   │
//! │   Messages are length-prefixed (4-byte LE)       │
//! │   [len: u32][payload: len bytes][len][payload]   │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! Key design choices:
//! - write_pos and read_pos on separate cache lines (no false sharing)
//! - Power-of-2 capacity → bitwise AND mask instead of modulo
//! - Bulk memcpy (ptr::copy_nonoverlapping) — at most 2 copies per
//!   push/pop to handle wrap-around, zero per-byte overhead
//! - Atomic Release on position update provides the memory fence;
//!   data writes before Release are visible after the reader's Acquire

use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

const HEADER_SIZE: usize = 128;
const CACHE_LINE: usize = 64;

/// View into a ring buffer that lives in shared memory.
/// Does NOT own the memory — the ShmRegion owns it.
pub struct RingBuf {
    base: *mut u8,
    /// Always a power of 2
    capacity: usize,
    /// capacity - 1, used as bitmask
    mask: usize,
}

// SAFETY: The ring buffer is designed for cross-process shared memory use.
// Synchronization is handled by atomic operations on write_pos/read_pos.
unsafe impl Send for RingBuf {}
unsafe impl Sync for RingBuf {}

impl RingBuf {
    /// Wrap a raw shared memory pointer as a ring buffer.
    ///
    /// The data capacity is rounded down to the largest power of 2 that fits
    /// in `total_size - HEADER_SIZE`.
    ///
    /// # Safety
    /// - `base` must point to at least `total_size` bytes of mapped memory
    /// - `total_size` must be > HEADER_SIZE + 1
    /// - The memory must remain valid for the lifetime of this RingBuf
    pub unsafe fn new(base: *mut u8, total_size: usize) -> Self {
        let raw_cap = total_size - HEADER_SIZE;
        assert!(raw_cap >= 2, "ring buffer too small");
        // Round down to power of 2
        let capacity = raw_cap.next_power_of_two() >> (if raw_cap.is_power_of_two() { 0 } else { 1 });
        let mask = capacity - 1;
        RingBuf {
            base,
            capacity,
            mask,
        }
    }

    /// Initialize header to zeros. Call once when creating (not opening).
    pub fn init(&self) {
        self.write_pos().store(0, Ordering::Release);
        self.read_pos().store(0, Ordering::Release);
    }

    /// Usable data capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn write_pos(&self) -> &AtomicU64 {
        unsafe { &*(self.base as *const AtomicU64) }
    }

    fn read_pos(&self) -> &AtomicU64 {
        unsafe { &*(self.base.add(CACHE_LINE) as *const AtomicU64) }
    }

    fn data_base(&self) -> *mut u8 {
        unsafe { self.base.add(HEADER_SIZE) }
    }

    /// Available bytes for reading.
    #[inline]
    pub fn available(&self) -> usize {
        let w = self.write_pos().load(Ordering::Acquire);
        let r = self.read_pos().load(Ordering::Acquire);
        (w.wrapping_sub(r)) as usize
    }

    /// Free space for writing.
    #[inline]
    pub fn free_space(&self) -> usize {
        self.capacity - self.available()
    }

    /// Write a length-prefixed message. Returns false if not enough space.
    #[inline]
    pub fn push(&self, data: &[u8]) -> bool {
        let msg_size = 4 + data.len();
        if msg_size > self.free_space() {
            return false;
        }

        let w = self.write_pos().load(Ordering::Relaxed) as usize;
        let len_bytes = (data.len() as u32).to_le_bytes();

        // Write length prefix (4 bytes), then payload — bulk memcpy
        self.copy_in(w, &len_bytes);
        self.copy_in(w + 4, data);

        // Release fence: all writes above are visible before we publish the new write_pos
        self.write_pos()
            .store((w + msg_size) as u64, Ordering::Release);
        true
    }

    /// Read the next length-prefixed message. Returns false if empty.
    #[inline]
    pub fn pop(&self, buf: &mut Vec<u8>) -> bool {
        if self.available() < 4 {
            return false;
        }

        let r = self.read_pos().load(Ordering::Relaxed) as usize;

        // Read length prefix
        let mut len_bytes = [0u8; 4];
        self.copy_out(r, &mut len_bytes);
        let len = u32::from_le_bytes(len_bytes) as usize;

        if self.available() < 4 + len {
            return false;
        }

        buf.resize(len, 0);
        self.copy_out(r + 4, buf);

        // Release: consumer is done reading, producer can now reuse this space
        self.read_pos()
            .store((r + 4 + len) as u64, Ordering::Release);
        true
    }

    /// Bulk copy `data` into the ring at logical position `pos`.
    /// Handles wrap-around with at most 2 memcpy calls.
    #[inline]
    fn copy_in(&self, pos: usize, data: &[u8]) {
        let base = self.data_base();
        let offset = pos & self.mask;
        let first = self.capacity - offset; // bytes until end of buffer

        if data.len() <= first {
            // No wrap
            unsafe {
                ptr::copy_nonoverlapping(data.as_ptr(), base.add(offset), data.len());
            }
        } else {
            // Wrap: copy [offset..end], then [0..remainder]
            unsafe {
                ptr::copy_nonoverlapping(data.as_ptr(), base.add(offset), first);
                ptr::copy_nonoverlapping(data.as_ptr().add(first), base, data.len() - first);
            }
        }
    }

    /// Bulk copy from the ring at logical position `pos` into `buf`.
    /// Handles wrap-around with at most 2 memcpy calls.
    #[inline]
    fn copy_out(&self, pos: usize, buf: &mut [u8]) {
        let base = self.data_base();
        let offset = pos & self.mask;
        let first = self.capacity - offset;

        if buf.len() <= first {
            unsafe {
                ptr::copy_nonoverlapping(base.add(offset), buf.as_mut_ptr(), buf.len());
            }
        } else {
            unsafe {
                ptr::copy_nonoverlapping(base.add(offset), buf.as_mut_ptr(), first);
                ptr::copy_nonoverlapping(base, buf.as_mut_ptr().add(first), buf.len() - first);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a deterministic payload: each byte is derived from the sequence
    /// number and its position, so any corruption is detectable.
    fn make_payload(seq: u64, len: usize) -> Vec<u8> {
        let seed = seq.to_le_bytes();
        (0..len)
            .map(|i| seed[i % 8].wrapping_add(i as u8))
            .collect()
    }

    /// Verify a payload matches what make_payload would produce.
    fn verify_payload(seq: u64, data: &[u8]) {
        let expected = make_payload(seq, data.len());
        assert_eq!(
            data, &expected[..],
            "DATA CORRUPTION at seq={}: first mismatch at byte {}",
            seq,
            data.iter()
                .zip(expected.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(0)
        );
    }

    #[test]
    fn test_push_pop_roundtrip() {
        let size = HEADER_SIZE + 4096;
        let mut mem = vec![0u8; size];
        let ring = unsafe { RingBuf::new(mem.as_mut_ptr(), size) };
        ring.init();

        let msg = b"hello world";
        assert!(ring.push(msg));

        let mut buf = Vec::new();
        assert!(ring.pop(&mut buf));
        assert_eq!(&buf, msg);
    }

    #[test]
    fn test_multiple_messages_verified() {
        let size = HEADER_SIZE + 4096;
        let mut mem = vec![0u8; size];
        let ring = unsafe { RingBuf::new(mem.as_mut_ptr(), size) };
        ring.init();

        for i in 0..10 {
            let payload = make_payload(i, 100);
            assert!(ring.push(&payload));
        }

        let mut buf = Vec::new();
        for i in 0..10 {
            assert!(ring.pop(&mut buf));
            verify_payload(i, &buf);
        }
    }

    #[test]
    fn test_full_ring() {
        let size = HEADER_SIZE + 64;
        let mut mem = vec![0u8; size];
        let ring = unsafe { RingBuf::new(mem.as_mut_ptr(), size) };
        ring.init();
        assert_eq!(ring.capacity(), 64);

        let big = make_payload(42, 50);
        assert!(ring.push(&big));
        assert!(!ring.push(&big));

        let mut buf = Vec::new();
        assert!(ring.pop(&mut buf));
        verify_payload(42, &buf);
    }

    #[test]
    fn test_wrap_around_verified() {
        // Small ring to force wrap-around, verify every byte
        let size = HEADER_SIZE + 64;
        let mut mem = vec![0u8; size];
        let ring = unsafe { RingBuf::new(mem.as_mut_ptr(), size) };
        ring.init();

        // Advance positions near the end: 2 * (4 + 20) = 48 bytes
        let filler0 = make_payload(0, 20);
        let filler1 = make_payload(1, 20);
        assert!(ring.push(&filler0));
        assert!(ring.push(&filler1));
        let mut buf = Vec::new();
        assert!(ring.pop(&mut buf));
        verify_payload(0, &buf);
        assert!(ring.pop(&mut buf));
        verify_payload(1, &buf);
        // write_pos = read_pos = 48, near 64-byte boundary

        // This message wraps: 4-byte len header starts at offset 48, payload crosses boundary
        let wrap_msg = make_payload(99, 20); // 4 + 20 = 24 bytes, wraps at offset 64
        assert!(ring.push(&wrap_msg));

        assert!(ring.pop(&mut buf));
        verify_payload(99, &buf);
    }

    #[test]
    fn test_wrap_around_length_header_split() {
        // Force the 4-byte length header itself to split across the wrap boundary
        let size = HEADER_SIZE + 64;
        let mut mem = vec![0u8; size];
        let ring = unsafe { RingBuf::new(mem.as_mut_ptr(), size) };
        ring.init();

        // Advance to offset 62: push 58-byte msg (4+54), then a 4+0 msg won't fit,
        // so push smaller ones to reach exactly 62
        // 4 + 25 = 29 bytes, two of them = 58
        let a = make_payload(0, 25);
        let b = make_payload(1, 25);
        assert!(ring.push(&a));
        assert!(ring.push(&b));
        let mut buf = Vec::new();
        assert!(ring.pop(&mut buf));
        verify_payload(0, &buf);
        assert!(ring.pop(&mut buf));
        verify_payload(1, &buf);
        // pos = 58

        // Now push 2 bytes payload: header at offset 58, bytes 58-61 = length prefix,
        // bytes 62-63 = payload. Header fits, payload wraps? No — 2 bytes at 62 fits in 64.
        // Let's push 4+2 = 6 bytes starting at 58 → length at 58..62, payload at 62..64. Just fits.
        let c = make_payload(2, 2);
        assert!(ring.push(&c));
        assert!(ring.pop(&mut buf));
        verify_payload(2, &buf);
        // pos = 64

        // Now we're at exactly 64 = 0 mod 64. Push something to verify clean wrap.
        let d = make_payload(3, 30);
        assert!(ring.push(&d));
        assert!(ring.pop(&mut buf));
        verify_payload(3, &buf);
    }

    #[test]
    fn test_sustained_wrap_integrity() {
        // 256-byte ring, thousands of verified messages wrapping many times
        let size = HEADER_SIZE + 256;
        let mut mem = vec![0u8; size];
        let ring = unsafe { RingBuf::new(mem.as_mut_ptr(), size) };
        ring.init();

        let mut buf = Vec::new();
        let mut next_pop_seq: u64 = 0;

        for i in 0u64..50_000 {
            // Vary payload sizes to hit different wrap alignments
            let payload_len = ((i * 7 + 3) % 50) as usize + 1; // 1..50 bytes
            let payload = make_payload(i, payload_len);

            while !ring.push(&payload) {
                // Must drain to make room — verify what we drain
                assert!(ring.pop(&mut buf), "ring empty but push failed at seq={}", i);
                let expected_len = ((next_pop_seq * 7 + 3) % 50) as usize + 1;
                assert_eq!(
                    buf.len(),
                    expected_len,
                    "wrong length at seq={}: got {} expected {}",
                    next_pop_seq,
                    buf.len(),
                    expected_len
                );
                verify_payload(next_pop_seq, &buf);
                next_pop_seq += 1;
            }
        }

        // Drain and verify remaining
        while ring.pop(&mut buf) {
            let expected_len = ((next_pop_seq * 7 + 3) % 50) as usize + 1;
            assert_eq!(buf.len(), expected_len);
            verify_payload(next_pop_seq, &buf);
            next_pop_seq += 1;
        }

        assert_eq!(next_pop_seq, 50_000, "didn't receive all messages");
    }

    #[test]
    fn test_large_payload_integrity() {
        // 1 MiB ring, payloads up to 100 KiB — tests bulk memcpy correctness
        let size = HEADER_SIZE + 1024 * 1024;
        let mut mem = vec![0u8; size];
        let ring = unsafe { RingBuf::new(mem.as_mut_ptr(), size) };
        ring.init();

        let mut buf = Vec::new();
        let mut next_pop: u64 = 0;
        let payload_sizes = [1024, 4096, 16384, 65536, 100_000];

        for i in 0u64..500 {
            let psize = payload_sizes[(i as usize) % payload_sizes.len()];
            let payload = make_payload(i, psize);

            while !ring.push(&payload) {
                assert!(ring.pop(&mut buf));
                let exp_size = payload_sizes[(next_pop as usize) % payload_sizes.len()];
                assert_eq!(buf.len(), exp_size, "wrong len at seq={}", next_pop);
                verify_payload(next_pop, &buf);
                next_pop += 1;
            }
        }

        while ring.pop(&mut buf) {
            let exp_size = payload_sizes[(next_pop as usize) % payload_sizes.len()];
            assert_eq!(buf.len(), exp_size);
            verify_payload(next_pop, &buf);
            next_pop += 1;
        }

        assert_eq!(next_pop, 500);
    }

    #[test]
    fn test_power_of_two_rounding() {
        let size = HEADER_SIZE + 100;
        let mut mem = vec![0u8; size];
        let ring = unsafe { RingBuf::new(mem.as_mut_ptr(), size) };
        assert_eq!(ring.capacity(), 64);

        let size = HEADER_SIZE + 512;
        let mut mem = vec![0u8; size];
        let ring = unsafe { RingBuf::new(mem.as_mut_ptr(), size) };
        assert_eq!(ring.capacity(), 512);
    }
}
