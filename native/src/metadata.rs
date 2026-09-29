//! Opaque, losslessly compressed metadata, independent of the pixel codec.
use crate::{RadeltaError, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default uncompressed metadata allowance: 1024 MiB.
pub const DEFAULT_METADATA_LIMIT_BYTES: usize = 1024 * 1024 * 1024;
static METADATA_LIMIT_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_METADATA_LIMIT_BYTES);

/// Return the process-wide metadata limit in bytes.
pub fn metadata_limit() -> usize {
    METADATA_LIMIT_BYTES.load(Ordering::Relaxed)
}
/// Configure the process-wide metadata limit in bytes before starting I/O.
/// Zero permits only empty metadata. This is an allocation policy, not a file
/// format constraint or a cap on total memory used during compression.
/// Existing file handles retain their already-loaded metadata; streaming
/// writers are checked again when they finish.
pub fn set_metadata_limit(max_bytes: usize) {
    METADATA_LIMIT_BYTES.store(max_bytes, Ordering::Relaxed);
}
const FLAG: u16 = 0x8000;
const FOOTER: &[u8; 8] = b"RDMETA01";
const HEADER: usize = 13;
fn error(message: &str) -> RadeltaError {
    RadeltaError(message.into())
}
fn flagged(header: &[u8]) -> Result<bool> {
    if header.len() < 8
        || !matches!(
            &header[..4],
            b"RDL1" | b"RDLQ" | b"RDM2" | b"RDQ2" | b"RDS3" | b"RQS3"
        )
    {
        return Err(error("invalid Radelta metadata container"));
    }
    Ok(u16::from_le_bytes([header[6], header[7]]) & FLAG != 0)
}
fn block_start(len: u64, footer: &[u8; 16]) -> Result<u64> {
    if &footer[8..] != FOOTER {
        return Err(error("invalid metadata footer"));
    }
    let size = u64::from_le_bytes(footer[..8].try_into().unwrap());
    if size < HEADER as u64 || size > (metadata_limit() as u64).saturating_add(HEADER as u64) {
        return Err(error("metadata stored length exceeds limits"));
    }
    len.checked_sub(16)
        .and_then(|n| n.checked_sub(size))
        .filter(|&n| n >= 8)
        .ok_or_else(|| error("truncated metadata block"))
}
/// Separate the image container and compressed metadata without inflating it.
pub(crate) fn split(data: &[u8]) -> Result<(&[u8], &[u8])> {
    if !flagged(data)? {
        return Ok((data, &[]));
    }
    let footer = data
        .get(data.len().saturating_sub(16)..)
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| error("truncated metadata footer"))?;
    let start = block_start(data.len() as u64, footer)? as usize;
    Ok((&data[..start], &data[start..data.len() - 16]))
}
fn pack(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() > metadata_limit() {
        return Err(error("metadata exceeds configured byte limit"));
    }
    let scratch_size = (data.len() as u128 * 110 / 100) + 20;
    if scratch_size > isize::MAX as u128 || data.len().checked_add(HEADER + 16).is_none() {
        return Err(error("metadata exceeds platform allocation limits"));
    }
    let compressed = lz4_flex::block::compress(data);
    let (codec, payload) = if compressed.len() < data.len() {
        (1, compressed.as_slice())
    } else {
        (0, data)
    };
    let mut out = Vec::with_capacity(HEADER + payload.len() + 16);
    out.push(codec);
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
    out.extend_from_slice(payload);
    let size = out.len() as u64;
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(FOOTER);
    Ok(out)
}
fn unpack(block: &[u8]) -> Result<Vec<u8>> {
    if block.is_empty() {
        return Ok(Vec::new());
    }
    if block.len() < HEADER {
        return Err(error("truncated metadata header"));
    }
    // Reject invalid encodings before trusting a length that could request a
    // large allocation, even when that length is within the configured limit.
    if block[0] > 1 {
        return Err(error("invalid metadata codec"));
    }
    let len = u64::from_le_bytes(block[1..9].try_into().unwrap());
    if len > metadata_limit() as u64 {
        return Err(error("metadata exceeds configured byte limit"));
    }
    let len = usize::try_from(len)
        .ok()
        .filter(|&n| n <= isize::MAX as usize)
        .ok_or_else(|| error("metadata exceeds platform allocation limits"))?;
    if block[0] == 0 && block.len() - HEADER != len {
        return Err(error("invalid raw metadata length"));
    }
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| error("could not allocate metadata output"))?;
    out.resize(len, 0);
    match block[0] {
        0 if block.len() - HEADER == out.len() => out.copy_from_slice(&block[HEADER..]),
        1 => {
            let written = lz4_flex::block::decompress_into(&block[HEADER..], &mut out)
                .map_err(|e| error(&format!("invalid LZ4 metadata: {e}")))?;
            if written != out.len() {
                return Err(error("metadata decoded length mismatch"));
            }
        }
        _ => return Err(error("invalid metadata codec or raw length")),
    }
    let checksum = u32::from_le_bytes(block[9..13].try_into().unwrap());
    if crc32fast::hash(&out) != checksum {
        return Err(error("metadata checksum mismatch"));
    }
    Ok(out)
}
/// Read opaque metadata. Absence is represented by an empty vector.
pub fn read_metadata(encoded: &[u8]) -> Result<Vec<u8>> {
    unpack(split(encoded)?.1)
}
/// Attach or replace opaque metadata; an empty payload removes metadata.
/// This does not decode or recompress the pixels.
pub fn set_metadata(encoded: &mut Vec<u8>, metadata: &[u8]) -> Result<()> {
    let core_len = split(encoded)?.0.len();
    let block = if metadata.is_empty() {
        Vec::new()
    } else {
        pack(metadata)?
    };
    encoded.truncate(core_len);
    let flags = u16::from_le_bytes([encoded[6], encoded[7]]) & !FLAG;
    encoded[6..8].copy_from_slice(&(flags | if block.is_empty() { 0 } else { FLAG }).to_le_bytes());
    encoded.extend_from_slice(&block);
    Ok(())
}
fn file_parts(file: &mut File) -> Result<(u64, Vec<u8>)> {
    let io = |e: std::io::Error| error(&e.to_string());
    let len = file.metadata().map_err(io)?.len();
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut header = [0; 8];
    file.read_exact(&mut header).map_err(io)?;
    if !flagged(&header)? {
        return Ok((len, Vec::new()));
    }
    if len < 24 {
        return Err(error("truncated metadata footer"));
    }
    file.seek(SeekFrom::End(-16)).map_err(io)?;
    let mut footer = [0; 16];
    file.read_exact(&mut footer).map_err(io)?;
    let start = block_start(len, &footer)?;
    let block_len = usize::try_from(len - start - 16)
        .ok()
        .filter(|&n| n <= isize::MAX as usize)
        .ok_or_else(|| error("metadata exceeds platform allocation limits"))?;
    let mut block = vec![0; block_len];
    file.seek(SeekFrom::Start(start)).map_err(io)?;
    file.read_exact(&mut block).map_err(io)?;
    Ok((start, block))
}
pub(crate) fn file_metadata(file: &mut File) -> Result<(u64, Vec<u8>)> {
    let (len, block) = file_parts(file)?;
    Ok((len, unpack(&block)?))
}
/// Read metadata without loading image payloads.
pub fn read_file_metadata(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let mut file = File::open(path).map_err(|e| error(&e.to_string()))?;
    file_metadata(&mut file).map(|(_, data)| data)
}
pub(crate) fn set_file_handle_metadata(file: &mut File, metadata: &[u8]) -> Result<()> {
    let (len, _) = file_parts(file)?;
    let block = if metadata.is_empty() {
        Vec::new()
    } else {
        pack(metadata)?
    };
    let io = |e: std::io::Error| error(&e.to_string());
    file.seek(SeekFrom::Start(6)).map_err(io)?;
    let mut flags = [0; 2];
    file.read_exact(&mut flags).map_err(io)?;
    let flags = (u16::from_le_bytes(flags) & !FLAG) | if block.is_empty() { 0 } else { FLAG };
    file.seek(SeekFrom::Start(len)).map_err(io)?;
    file.write_all(&block).map_err(io)?;
    file.set_len(len + block.len() as u64).map_err(io)?;
    file.seek(SeekFrom::Start(6)).map_err(io)?;
    file.write_all(&flags.to_le_bytes()).map_err(io)?;
    file.flush().map_err(io)
}
/// Attach or replace metadata without loading/recompressing pixels.
pub fn set_file_metadata(path: impl AsRef<Path>, metadata: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| error(&e.to_string()))?;
    set_file_handle_metadata(&mut file, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    #[test]
    fn opaque_metadata_roundtrips_all_memory_containers() {
        let d = Dims { x: 3, y: 2, z: 2 };
        let d5 = Dims5 {
            x: 3,
            y: 2,
            z: 2,
            c: 1,
            t: 1,
        };
        let pixels = vec![123; 12];
        for mut encoded in [
            compress_u16(&pixels, d, Options::default()).unwrap(),
            compress_lossy_u16(&pixels, d, LossyOptions::default()).unwrap(),
            compress_u16_nd(&pixels, d5, Options::default()).unwrap(),
            compress_lossy_u16_nd(&pixels, d5, LossyOptions::default()).unwrap(),
        ] {
            let expected = decompress_u16_nd(&encoded).unwrap();
            let bare = encoded.clone();
            for metadata in [
                b"<xml>anything</xml>".repeat(1000),
                (0..=255).collect(),
                vec![0, 255, 0, 128],
            ] {
                set_metadata(&mut encoded, &metadata).unwrap();
                assert_eq!(read_metadata(&encoded).unwrap(), metadata);
                assert_eq!(decompress_u16_nd(&encoded).unwrap(), expected);
                let mut out = vec![0; 12];
                assert_eq!(decompress_u16_nd_into(&encoded, &mut out).unwrap(), d5);
                assert_eq!(out, expected.0);
            }
            set_metadata(&mut encoded, &[]).unwrap();
            assert_eq!(encoded, bare);
        }
    }
    #[test]
    fn invalid_metadata_headers_are_rejected_before_allocation() {
        let mut block = vec![0; HEADER];
        block[1..9].copy_from_slice(&(DEFAULT_METADATA_LIMIT_BYTES as u64).to_le_bytes());
        assert_eq!(unpack(&block).unwrap_err().0, "invalid raw metadata length");
        block[0] = 99;
        assert_eq!(unpack(&block).unwrap_err().0, "invalid metadata codec");
    }

    #[test]
    fn metadata_corruption_is_rejected() {
        let mut encoded =
            compress_u16(&[1], Dims { x: 1, y: 1, z: 1 }, Options::default()).unwrap();
        set_metadata(&mut encoded, &vec![b'x'; 10000]).unwrap();
        let (core, _) = split(&encoded).unwrap();
        let start = core.len();
        assert!(encoded.len() < 10000);
        for offset in [start, start + 1, start + 9, encoded.len() - 1] {
            let mut bad = encoded.clone();
            bad[offset] ^= 255;
            assert!(read_metadata(&bad).is_err());
        }
        for end in [7, encoded.len() - 1, encoded.len() - 16] {
            assert!(read_metadata(&encoded[..end]).is_err());
        }
    }
}
