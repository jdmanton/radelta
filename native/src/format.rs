//! Container identifiers, little-endian fields and checksum framing.

use crate::types::{RadeltaError, Result};

pub(crate) const MAGIC: &[u8; 4] = b"RDL1";

pub(crate) const MAGIC_LOSSY: &[u8; 4] = b"RDLQ";

pub(crate) const MAGIC_ND: &[u8; 4] = b"RDM2";

pub(crate) const MAGIC_ND_LOSSY: &[u8; 4] = b"RDQ2";

#[doc(hidden)]
pub const STREAM_MAGIC_LOSSLESS: &[u8; 4] = b"RDS3";

#[doc(hidden)]
pub const STREAM_MAGIC_LOSSY: &[u8; 4] = b"RQS3";

#[doc(hidden)]
pub const STREAM_VERSION: u16 = 1;

pub(crate) const VERSION: u16 = 1;

pub(crate) const ND_VERSION: u16 = 1;

pub(crate) const LOSSY_VERSION: u16 = 1;

// Format version 1 supports both entropy-coded and raw block records.
pub(crate) const RAW_BLOCK_MARKER: u32 = u32::MAX;

pub(crate) const RAW_RECORD_BYTES: usize = 20;

pub(crate) fn append_raw_u16(out: &mut Vec<u8>, pixels: &[u16]) {
    #[cfg(target_endian = "little")]
    // SAFETY: every byte in a live u16 slice is initialized and can be read
    // through u8. The borrowed view does not outlive pixels.
    out.extend_from_slice(unsafe {
        std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), std::mem::size_of_val(pixels))
    });
    #[cfg(target_endian = "big")]
    for &pixel in pixels {
        out.extend_from_slice(&pixel.to_le_bytes());
    }
}

/// Byte range of the u16 `flags` field in every Radelta header. Metadata can
/// be attached or removed without re-encoding pixels by changing its flag, so
/// header checksums skip these two bytes; the metadata footer is checked
/// separately.
const FLAGS_FIELD: std::ops::Range<usize> = 6..8;

/// CRC-32 of a stream header and models, `bytes[..end]`, excluding `flags`.
pub(crate) fn header_crc(bytes: &[u8], end: usize) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&bytes[..FLAGS_FIELD.start]);
    hasher.update(&bytes[FLAGS_FIELD.end..end]);
    hasher.finalize()
}

/// Append the header CRC-32 after the serialized models.
pub(crate) fn put_header_crc(out: &mut Vec<u8>) {
    let crc = header_crc(out, out.len());
    put_u32(out, crc);
}

/// Append the CRC-32 of the block record written since `start`.
pub(crate) fn put_block_crc(out: &mut Vec<u8>, start: usize) {
    let crc = crc32fast::hash(&out[start..]);
    put_u32(out, crc);
}

/// Read and check the header CRC-32 that follows the serialized models.
pub(crate) fn check_header_crc(rd: &mut Reader<'_>, what: &str) -> Result<()> {
    let end = rd.pos;
    let stored = rd.u32()?;
    if header_crc(rd.data, end) != stored {
        return Err(RadeltaError(format!("{what} header checksum mismatch")));
    }
    Ok(())
}

pub(crate) fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) struct Reader<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| RadeltaError("input offset overflow".into()))?;
        if end > self.data.len() {
            return Err(RadeltaError("truncated input".into()));
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub(crate) fn f64(&mut self) -> Result<f64> {
        let b = self.take(8)?;
        Ok(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub(crate) fn is_done(&self) -> bool {
        self.pos == self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_format_versions_are_one() {
        assert_eq!(VERSION, 1);
        assert_eq!(ND_VERSION, 1);
        assert_eq!(LOSSY_VERSION, 1);
        assert_eq!(STREAM_VERSION, 1);
    }
}
