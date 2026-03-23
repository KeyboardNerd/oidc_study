//! Shared memory region management.
//!
//! Creates and maps files on /dev/shm (or any tmpfs path) for use as
//! lock-free ring buffers between processes.

use memmap2::MmapMut;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// A mapped shared memory region backed by a file on tmpfs.
pub struct ShmRegion {
    pub path: PathBuf,
    pub mmap: MmapMut,
}

impl ShmRegion {
    /// Create (or open) a shared memory region of `size` bytes.
    ///
    /// `base_dir` is typically "/dev/shm" on Linux or any tmpfs mount.
    /// `name` is the region name (becomes the filename).
    pub fn create(base_dir: &Path, name: &str, size: usize) -> io::Result<Self> {
        let path = base_dir.join(name);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        // Set file to desired size
        file.set_len(size as u64)?;

        // SAFETY: We own this file and control access via the protocol.
        // Multiple processes map the same file — synchronization is handled
        // by atomic operations in the ring buffer header.
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        Ok(ShmRegion { path, mmap })
    }

    /// Open an existing shared memory region.
    pub fn open(base_dir: &Path, name: &str) -> io::Result<Self> {
        let path = base_dir.join(name);

        let file = OpenOptions::new().read(true).write(true).open(&path)?;

        let mmap = unsafe { MmapMut::map_mut(&file)? };

        Ok(ShmRegion { path, mmap })
    }

    /// Remove the backing file.
    pub fn unlink(&self) -> io::Result<()> {
        fs::remove_file(&self.path)
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.mmap.as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.mmap.as_mut_ptr()
    }

    pub fn len(&self) -> usize {
        self.mmap.len()
    }
}
