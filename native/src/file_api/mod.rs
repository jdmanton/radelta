//! Indexed file reading and bounded streaming writing.
//!
//! This API deliberately hides the on-disk Radelta container variants. A
//! caller opens a file, queries its TCZYX shape, and requests individual
//! uint16 planes, or creates a streaming writer and feeds planes sequentially.
//! RDS3/RQS3 files are indexed without loading their payloads; only the
//! compressed chunk containing a requested plane is decoded.

mod cache;
mod ffi;
mod index;
mod reader;
mod writer;

pub use cache::DEFAULT_FILE_CACHE_BYTES;
pub use writer::RadeltaStreamWriter;

use crate::Dims5;
use index::FileBacking;
use std::sync::Mutex;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RadeltaFileInfo {
    pub nx: u32,
    pub ny: u32,
    pub nz: u32,
    pub nc: u32,
    pub nt: u32,
    pub format: u32,
    pub flags: u32,
}

/// An independently decoded volume or streaming chunk within one open file.
/// Keys identify units only within that handle. The last chunk can be shorter.
#[derive(Clone, Copy, Debug)]
pub struct RadeltaDecodeUnit {
    pub key: usize,
    pub z0: usize,
    pub depth: usize,
    pub encoded_bytes: u64,
}

/// An open image with synchronized file I/O and decoded-data retention.
/// Codec work runs outside the backing mutex so concurrent readers can share
/// the Rayon pool without holding a lock needed by another read job.
pub struct RadeltaFileHandle {
    dims: Dims5,
    format: u32,
    flags: u32,
    backing: Mutex<FileBacking>,
    metadata: Vec<u8>,
}

#[cfg(test)]
mod test_support;
