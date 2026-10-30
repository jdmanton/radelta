//! File opening, header validation and stored-unit indexing.

use super::cache::{PixelCache, DEFAULT_FILE_CACHE_BYTES};
use super::RadeltaFileHandle;
use crate::format::{
    Reader, LOSSY_VERSION, MAGIC, MAGIC_LOSSY, MAGIC_ND, MAGIC_ND_LOSSY, ND_VERSION, VERSION,
};
use crate::{inspect_dims, Dims, Dims5, STREAM_MAGIC_LOSSLESS, STREAM_MAGIC_LOSSY, STREAM_VERSION};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;

pub(super) const RADELTA_FILE_FORMAT_RDL1: u32 = 1;

pub(super) const RADELTA_FILE_FORMAT_RDLQ: u32 = 2;

pub(super) const RADELTA_FILE_FORMAT_RDM2: u32 = 3;

pub(super) const RADELTA_FILE_FORMAT_RDQ2: u32 = 4;

pub(super) const RADELTA_FILE_FORMAT_RDS3: u32 = 5;

pub(super) const RADELTA_FILE_FORMAT_RQS3: u32 = 6;

pub(super) const RADELTA_FILE_FLAG_LOSSY: u32 = 1;

pub(super) const RADELTA_FILE_FLAG_MULTIDIMENSIONAL: u32 = 2;

pub(super) const RADELTA_FILE_FLAG_STREAMING: u32 = 4;

#[derive(Clone, Copy)]
pub(super) struct Segment {
    pub(super) offset: u64,
    pub(super) len: u64,
}

#[derive(Clone, Copy)]
pub(super) struct ChunkSegment {
    pub(super) t: usize,
    pub(super) c: usize,
    pub(super) z0: usize,
    pub(super) depth: usize,
    pub(super) offset: u64,
    pub(super) len: u64,
}

pub(super) enum FileBacking {
    Single {
        file: File,
        len: u64,
        cache: Option<Vec<u16>>,
    },
    Nd {
        file: File,
        streams: Vec<Segment>,
        cache: PixelCache,
    },
    Stream {
        file: File,
        chunks: Vec<ChunkSegment>,
        chunk_z: usize,
        chunks_per_volume: usize,
        cache: PixelCache,
    },
}

#[derive(Clone, Copy)]
pub(super) struct DecodeUnit {
    pub(super) key: usize,
    pub(super) segment: Segment,
    pub(super) z0: usize,
    pub(super) depth: usize,
    pub(super) context: &'static str,
}

fn stream_chunk_key(
    chunks: &[ChunkSegment],
    chunk_z: usize,
    chunks_per_volume: usize,
    d: Dims5,
    t: usize,
    c: usize,
    z: usize,
) -> std::result::Result<usize, String> {
    let key = (t * d.c + c) * chunks_per_volume + z / chunk_z;
    let matches = |ch: &ChunkSegment| ch.t == t && ch.c == c && z >= ch.z0 && z < ch.z0 + ch.depth;
    if chunks.get(key).is_some_and(matches) {
        Ok(key)
    } else {
        chunks
            .iter()
            .position(matches)
            .ok_or_else(|| "could not locate requested streaming Radelta chunk".into())
    }
}

pub(super) fn decode_unit(
    backing: &FileBacking,
    d: Dims5,
    t: usize,
    c: usize,
    z: usize,
) -> std::result::Result<DecodeUnit, String> {
    Ok(match backing {
        FileBacking::Single { len, .. } => DecodeUnit {
            key: 0,
            segment: Segment {
                offset: 0,
                len: *len,
            },
            z0: 0,
            depth: d.z,
            context: "Radelta volume",
        },
        FileBacking::Nd { streams, .. } => {
            let key = t * d.c + c;
            DecodeUnit {
                key,
                segment: streams[key],
                z0: 0,
                depth: d.z,
                context: "RDM2/RDQ2 substream",
            }
        }
        FileBacking::Stream {
            chunks,
            chunk_z,
            chunks_per_volume,
            ..
        } => {
            let key = stream_chunk_key(chunks, *chunk_z, *chunks_per_volume, d, t, c, z)?;
            let ch = chunks[key];
            DecodeUnit {
                key,
                segment: Segment {
                    offset: ch.offset,
                    len: ch.len,
                },
                z0: ch.z0,
                depth: ch.depth,
                context: "RDS3/RQS3 chunk",
            }
        }
    })
}

fn read_exact_array<const N: usize>(file: &mut File) -> std::io::Result<[u8; N]> {
    let mut b = [0u8; N];
    file.read_exact(&mut b)?;
    Ok(b)
}

fn read_u16(file: &mut File) -> std::io::Result<u16> {
    Ok(u16::from_le_bytes(read_exact_array::<2>(file)?))
}

fn read_u32(file: &mut File) -> std::io::Result<u32> {
    Ok(u32::from_le_bytes(read_exact_array::<4>(file)?))
}

fn read_u64(file: &mut File) -> std::io::Result<u64> {
    Ok(u64::from_le_bytes(read_exact_array::<8>(file)?))
}

pub(super) fn dims5(x: u32, y: u32, z: u32, c: u32, t: u32) -> std::result::Result<Dims5, String> {
    if x == 0 || y == 0 || z == 0 || c == 0 || t == 0 {
        return Err("Radelta dimensions must all be non-zero".into());
    }
    let dims = Dims5 {
        x: x as usize,
        y: y as usize,
        z: z as usize,
        c: c as usize,
        t: t as usize,
    };
    dims.validate().map_err(|e| e.to_string())?;
    Ok(dims)
}

/// Bytes needed to read an embedded RDL1/RDLQ header up to its dimensions.
const PAYLOAD_HEADER_BYTES: u64 = 20;

/// Check the RDL1/RDLQ header at the start of an embedded payload against the
/// container. A file whose container and payload shapes disagree is rejected
/// at open, before any caller sizes plane buffers from the container header.
fn check_payload_header(
    header: &[u8],
    payload_len: u64,
    lossy: bool,
    expected: Dims,
) -> std::result::Result<(), String> {
    if payload_len < PAYLOAD_HEADER_BYTES {
        return Err("truncated Radelta payload header".into());
    }
    let magic = if lossy { MAGIC_LOSSY } else { MAGIC };
    if &header[..4] != magic {
        return Err("Radelta payload mode does not match its container".into());
    }
    let found = inspect_dims(header).map_err(|e| e.to_string())?;
    if found != expected {
        return Err(format!(
            "Radelta payload dimensions {}x{}x{} do not match container {}x{}x{}",
            found.x, found.y, found.z, expected.x, expected.y, expected.z
        ));
    }
    Ok(())
}

/// Read exactly a record header plus its embedded pixel header. Positioned
/// reads avoid cursor queries/seeks and do not prefetch any pixel payload.
fn index_prefix<const N: usize>(file: &mut File, offset: u64) -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    #[cfg(unix)]
    std::os::unix::fs::FileExt::read_exact_at(file, &mut bytes, offset)?;
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < N {
            match std::os::windows::fs::FileExt::seek_read(
                file,
                &mut bytes[done..],
                offset + done as u64,
            ) {
                Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
    }
    Ok(bytes)
}

pub(super) fn open_impl(path: PathBuf) -> std::result::Result<RadeltaFileHandle, String> {
    let mut file = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    if len < 4 {
        return Err("truncated Radelta file".into());
    }
    let full_len = len;
    let (len, metadata) = crate::metadata::file_metadata(&mut file).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let magic = read_exact_array::<4>(&mut file).map_err(|e| e.to_string())?;

    if &magic == MAGIC || &magic == MAGIC_LOSSY {
        let version = read_u16(&mut file).map_err(|e| e.to_string())?;
        let expected = if &magic == MAGIC {
            VERSION
        } else {
            LOSSY_VERSION
        };
        if version != expected {
            return Err(format!("unsupported Radelta version {version}"));
        }
        let _flags = read_u16(&mut file).map_err(|e| e.to_string())?;
        let d = dims5(
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
            1,
            1,
        )?;
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let lossy = &magic == MAGIC_LOSSY;
        return Ok(RadeltaFileHandle {
            metadata,
            dims: d,
            format: if lossy {
                RADELTA_FILE_FORMAT_RDLQ
            } else {
                RADELTA_FILE_FORMAT_RDL1
            },
            flags: if lossy { RADELTA_FILE_FLAG_LOSSY } else { 0 },
            backing: Mutex::new(FileBacking::Single {
                file,
                len: full_len,
                cache: None,
            }),
        });
    }

    if &magic == MAGIC_ND || &magic == MAGIC_ND_LOSSY {
        let version = read_u16(&mut file).map_err(|e| e.to_string())?;
        if version != ND_VERSION {
            return Err(format!(
                "unsupported multidimensional Radelta version {version}"
            ));
        }
        let _flags = read_u16(&mut file).map_err(|e| e.to_string())?;
        let d = dims5(
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
        )?;
        let nvol = read_u32(&mut file).map_err(|e| e.to_string())? as usize;
        let expected = d.t.checked_mul(d.c).ok_or("T*C overflow")?;
        if nvol != expected {
            return Err("RDM2/RDQ2 volume count does not match T*C".into());
        }
        let lossy = &magic == MAGIC_ND_LOSSY;
        // Each record needs a length and at least an embedded volume header.
        // Check file bounds before trusting a count as an allocation size.
        let mut cursor = 32u64;
        let remaining = len.saturating_sub(cursor);
        if nvol as u64 > remaining / (8 + PAYLOAD_HEADER_BYTES) {
            return Err("multidimensional volume count exceeds available file records".into());
        }
        let mut streams = Vec::new();
        streams
            .try_reserve_exact(nvol)
            .map_err(|_| "cannot allocate Radelta volume index")?;
        for _ in 0..nvol {
            let prefix = index_prefix::<28>(&mut file, cursor).map_err(|e| e.to_string())?;
            let mut rd = Reader::new(&prefix);
            let slen = rd.u64().map_err(|e| e.to_string())?;
            let off = cursor
                .checked_add(8)
                .ok_or("substream file offset overflow")?;
            let end = off
                .checked_add(slen)
                .ok_or("substream file offset overflow")?;
            if end > len {
                return Err("truncated multidimensional Radelta substream".into());
            }
            let seg = Segment {
                offset: off,
                len: slen,
            };
            check_payload_header(&prefix[8..], slen, lossy, d.volume_dims())?;
            streams.push(seg);
            cursor = end;
        }
        if cursor != len {
            return Err("trailing bytes after Radelta volumes".into());
        }
        return Ok(RadeltaFileHandle {
            metadata,
            dims: d,
            format: if lossy {
                RADELTA_FILE_FORMAT_RDQ2
            } else {
                RADELTA_FILE_FORMAT_RDM2
            },
            flags: RADELTA_FILE_FLAG_MULTIDIMENSIONAL
                | if lossy { RADELTA_FILE_FLAG_LOSSY } else { 0 },
            backing: Mutex::new(FileBacking::Nd {
                file,
                streams,
                cache: PixelCache::new(DEFAULT_FILE_CACHE_BYTES),
            }),
        });
    }

    if &magic == STREAM_MAGIC_LOSSLESS || &magic == STREAM_MAGIC_LOSSY {
        let version = read_u16(&mut file).map_err(|e| e.to_string())?;
        if version != STREAM_VERSION {
            return Err(format!("unsupported streaming Radelta version {version}"));
        }
        let _flags = read_u16(&mut file).map_err(|e| e.to_string())?;
        let d = dims5(
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
            read_u32(&mut file).map_err(|e| e.to_string())?,
        )?;
        let chunk_z = read_u32(&mut file).map_err(|e| e.to_string())? as usize;
        if chunk_z == 0 {
            return Err("streaming Radelta chunk depth is zero".into());
        }
        let nchunks = read_u64(&mut file).map_err(|e| e.to_string())?;
        let nchunks_usize =
            usize::try_from(nchunks).map_err(|_| "chunk count exceeds platform limits")?;
        let chunks_per_volume = d.z.div_ceil(chunk_z);
        let expected =
            d.t.checked_mul(d.c)
                .and_then(|v| v.checked_mul(chunks_per_volume))
                .ok_or("chunk count overflow")?;
        if nchunks_usize != expected {
            return Err(format!(
                "streaming chunk count {nchunks_usize} does not match expected {expected}"
            ));
        }
        let lossy = &magic == STREAM_MAGIC_LOSSY;
        let mut cursor = 40u64;
        let remaining = len.saturating_sub(cursor);
        if nchunks > remaining / (24 + PAYLOAD_HEADER_BYTES) {
            return Err("streaming chunk count exceeds available file records".into());
        }
        let mut chunks = Vec::new();
        chunks
            .try_reserve_exact(nchunks_usize)
            .map_err(|_| "cannot allocate Radelta chunk index")?;
        for index in 0..nchunks_usize {
            let prefix = index_prefix::<44>(&mut file, cursor).map_err(|e| e.to_string())?;
            let mut rd = Reader::new(&prefix);
            let t = rd.u32().map_err(|e| e.to_string())? as usize;
            let c = rd.u32().map_err(|e| e.to_string())? as usize;
            let z0 = rd.u32().map_err(|e| e.to_string())? as usize;
            let depth = rd.u32().map_err(|e| e.to_string())? as usize;
            let plen = rd.u64().map_err(|e| e.to_string())?;
            let off = cursor.checked_add(24).ok_or("chunk file offset overflow")?;
            let end = off.checked_add(plen).ok_or("chunk file offset overflow")?;
            if end > len {
                return Err("truncated streaming Radelta chunk".into());
            }
            if t >= d.t || c >= d.c || depth == 0 || z0 >= d.z || z0 + depth > d.z {
                return Err("invalid streaming Radelta chunk coordinates".into());
            }
            let volume = index / chunks_per_volume;
            let expected_z = (index % chunks_per_volume) * chunk_z;
            if (t, c, z0, depth)
                != (
                    volume / d.c,
                    volume % d.c,
                    expected_z,
                    chunk_z.min(d.z - expected_z),
                )
            {
                return Err("streaming Radelta chunks must follow canonical TCZ layout".into());
            }
            check_payload_header(
                &prefix[24..],
                plen,
                lossy,
                Dims {
                    x: d.x,
                    y: d.y,
                    z: depth,
                },
            )?;
            chunks.push(ChunkSegment {
                t,
                c,
                z0,
                depth,
                offset: off,
                len: plen,
            });
            cursor = end;
        }
        if cursor != len {
            return Err("trailing bytes after Radelta chunks".into());
        }
        return Ok(RadeltaFileHandle {
            metadata,
            dims: d,
            format: if lossy {
                RADELTA_FILE_FORMAT_RQS3
            } else {
                RADELTA_FILE_FORMAT_RDS3
            },
            flags: RADELTA_FILE_FLAG_MULTIDIMENSIONAL
                | RADELTA_FILE_FLAG_STREAMING
                | if lossy { RADELTA_FILE_FLAG_LOSSY } else { 0 },
            backing: Mutex::new(FileBacking::Stream {
                file,
                chunks,
                chunk_z,
                chunks_per_volume,
                cache: PixelCache::new(DEFAULT_FILE_CACHE_BYTES),
            }),
        });
    }

    Err(format!(
        "unrecognized Radelta magic {:?}",
        String::from_utf8_lossy(&magic)
    ))
}
