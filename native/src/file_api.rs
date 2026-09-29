//! File-oriented C ABI used by the ImageJ/Fiji virtual-stack reader/writer.
//!
//! This API deliberately hides the on-disk Radelta container variants. A
//! caller opens a file, queries its TCZYX shape, and requests individual
//! uint16 planes, or creates a streaming writer and feeds planes sequentially.
//! RDS3/RQS3 files are indexed without loading their payloads; only the
//! compressed chunk containing a requested plane is decoded.

use super::{
    choose_stream_chunk_depth, compress_lossy_u16, compress_u16, decompress_u16, Dims, Dims5,
    LossyOptions, Options, DEFAULT_BLOCK_DEPTH, DEFAULT_STREAM_MEMORY_MIB, LOSSY_VERSION, MAGIC,
    MAGIC_LOSSY, MAGIC_ND, MAGIC_ND_LOSSY, ND_VERSION, RADELTA_CODEC_ERROR,
    RADELTA_INVALID_ARGUMENT, RADELTA_OK, STREAM_MAGIC_LOSSLESS, STREAM_MAGIC_LOSSY,
    STREAM_VERSION, VERSION,
};
use std::cell::RefCell;
use std::ffi::CStr;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::raw::c_char;
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;

pub const RADELTA_FILE_FORMAT_RDL1: u32 = 1;
pub const RADELTA_FILE_FORMAT_RDLQ: u32 = 2;
pub const RADELTA_FILE_FORMAT_RDM2: u32 = 3;
pub const RADELTA_FILE_FORMAT_RDQ2: u32 = 4;
pub const RADELTA_FILE_FORMAT_RDS3: u32 = 5;
pub const RADELTA_FILE_FORMAT_RQS3: u32 = 6;

pub const RADELTA_FILE_FLAG_LOSSY: u32 = 1;
pub const RADELTA_FILE_FLAG_MULTIDIMENSIONAL: u32 = 2;
pub const RADELTA_FILE_FLAG_STREAMING: u32 = 4;

const RADELTA_IO_ERROR: i32 = -3;

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_last_error(msg: impl Into<String>) {
    LAST_ERROR.with(|s| *s.borrow_mut() = msg.into());
}

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

#[derive(Clone, Copy)]
struct Segment {
    offset: u64,
    len: u64,
}

#[derive(Clone, Copy)]
struct ChunkSegment {
    t: usize,
    c: usize,
    z0: usize,
    depth: usize,
    offset: u64,
    len: u64,
}

struct CachedVolume {
    key: usize,
    pixels: Vec<u16>,
}

struct CachedChunk {
    key: usize,
    z0: usize,
    depth: usize,
    pixels: Vec<u16>,
}

enum FileBacking {
    Single {
        file: File,
        len: u64,
        cache: Option<Vec<u16>>,
    },
    Nd {
        file: File,
        streams: Vec<Segment>,
        cache: Option<CachedVolume>,
    },
    Stream {
        file: File,
        chunks: Vec<ChunkSegment>,
        chunk_z: usize,
        chunks_per_volume: usize,
        cache: Option<CachedChunk>,
    },
}

pub struct RadeltaFileHandle {
    dims: Dims5,
    format: u32,
    flags: u32,
    backing: Mutex<FileBacking>,
    metadata: Vec<u8>,
}

pub struct RadeltaWriterHandle {
    writer: RadeltaStreamWriter,
}

enum WriterMode {
    Lossless(Options),
    Lossy(LossyOptions),
}

struct RadeltaStreamWriter {
    dims: Dims5,
    plane_voxels: usize,
    chunk_z: usize,
    expected_chunks: u64,
    written_chunks: u64,
    next_t: usize,
    next_c: usize,
    next_z: usize,
    chunk_t: usize,
    chunk_c: usize,
    chunk_z0: usize,
    chunk_pixels: Vec<u16>,
    mode: WriterMode,
    out: BufWriter<File>,
    finished: bool,
    metadata: Vec<u8>,
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

fn write_u16<W: Write>(w: &mut W, v: u16) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn write_u32<W: Write>(w: &mut W, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn write_u64<W: Write>(w: &mut W, v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn dims5(x: u32, y: u32, z: u32, c: u32, t: u32) -> std::result::Result<Dims5, String> {
    if x == 0 || y == 0 || z == 0 || c == 0 || t == 0 {
        return Err("Radelta dimensions must all be non-zero".into());
    }
    Ok(Dims5 {
        x: x as usize,
        y: y as usize,
        z: z as usize,
        c: c as usize,
        t: t as usize,
    })
}

fn stream_header<W: Write>(
    w: &mut W,
    magic: &[u8; 4],
    d: Dims5,
    chunk_z: usize,
    chunks: u64,
) -> std::result::Result<(), String> {
    w.write_all(magic).map_err(|e| e.to_string())?;
    write_u16(w, STREAM_VERSION).map_err(|e| e.to_string())?;
    write_u16(w, 0).map_err(|e| e.to_string())?;
    for v in [d.x, d.y, d.z, d.c, d.t] {
        write_u32(w, u32::try_from(v).map_err(|_| "dimension exceeds u32")?)
            .map_err(|e| e.to_string())?;
    }
    write_u32(
        w,
        u32::try_from(chunk_z).map_err(|_| "chunk depth exceeds u32")?,
    )
    .map_err(|e| e.to_string())?;
    write_u64(w, chunks).map_err(|e| e.to_string())?;
    Ok(())
}

fn open_impl(path: PathBuf) -> std::result::Result<RadeltaFileHandle, String> {
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
        let mut streams = Vec::with_capacity(nvol);
        for _ in 0..nvol {
            let slen = read_u64(&mut file).map_err(|e| e.to_string())?;
            let off = file.stream_position().map_err(|e| e.to_string())?;
            let end = off
                .checked_add(slen)
                .ok_or("substream file offset overflow")?;
            if end > len {
                return Err("truncated multidimensional Radelta substream".into());
            }
            streams.push(Segment {
                offset: off,
                len: slen,
            });
            file.seek(SeekFrom::Start(end)).map_err(|e| e.to_string())?;
        }
        if file.stream_position().map_err(|e| e.to_string())? != len {
            return Err("trailing bytes after Radelta volumes".into());
        }
        let lossy = &magic == MAGIC_ND_LOSSY;
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
                cache: None,
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
        let mut chunks = Vec::with_capacity(nchunks_usize);
        for index in 0..nchunks_usize {
            let t = read_u32(&mut file).map_err(|e| e.to_string())? as usize;
            let c = read_u32(&mut file).map_err(|e| e.to_string())? as usize;
            let z0 = read_u32(&mut file).map_err(|e| e.to_string())? as usize;
            let depth = read_u32(&mut file).map_err(|e| e.to_string())? as usize;
            let plen = read_u64(&mut file).map_err(|e| e.to_string())?;
            let off = file.stream_position().map_err(|e| e.to_string())?;
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
            chunks.push(ChunkSegment {
                t,
                c,
                z0,
                depth,
                offset: off,
                len: plen,
            });
            file.seek(SeekFrom::Start(end)).map_err(|e| e.to_string())?;
        }
        if file.stream_position().map_err(|e| e.to_string())? != len {
            return Err("trailing bytes after Radelta chunks".into());
        }
        let lossy = &magic == STREAM_MAGIC_LOSSY;
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
                cache: None,
            }),
        });
    }

    Err(format!(
        "unrecognized Radelta magic {:?}",
        String::from_utf8_lossy(&magic)
    ))
}

fn read_payload(file: &mut File, seg: Segment) -> std::result::Result<Vec<u8>, String> {
    let len = usize::try_from(seg.len).map_err(|_| "compressed payload exceeds platform limits")?;
    let mut buf = vec![0u8; len];
    file.seek(SeekFrom::Start(seg.offset))
        .map_err(|e| e.to_string())?;
    file.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

impl RadeltaFileHandle {
    /// Open an indexed image file without reading all image payloads.
    pub fn open(path: impl AsRef<std::path::Path>) -> std::result::Result<Self, String> {
        open_impl(path.as_ref().to_path_buf())
    }
    pub fn metadata(&self) -> &[u8] {
        &self.metadata
    }

    pub fn info(&self) -> RadeltaFileInfo {
        RadeltaFileInfo {
            nx: self.dims.x as u32,
            ny: self.dims.y as u32,
            nz: self.dims.z as u32,
            nc: self.dims.c as u32,
            nt: self.dims.t as u32,
            format: self.format,
            flags: self.flags,
        }
    }

    pub fn read_plane(
        &self,
        t: usize,
        c: usize,
        z: usize,
        out: &mut [u16],
    ) -> std::result::Result<(), String> {
        let d = self.dims;
        if t >= d.t || c >= d.c || z >= d.z {
            return Err("requested T/C/Z plane is out of range".into());
        }
        let plane = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
        if out.len() < plane {
            return Err("output plane buffer is too small".into());
        }
        let mut backing = self
            .backing
            .lock()
            .map_err(|_| "Radelta file handle mutex is poisoned")?;
        match &mut *backing {
            FileBacking::Single { file, len, cache } => {
                if cache.is_none() {
                    let payload = read_payload(
                        file,
                        Segment {
                            offset: 0,
                            len: *len,
                        },
                    )?;
                    let (pixels, dd) = decompress_u16(&payload).map_err(|e| e.to_string())?;
                    if dd
                        != (Dims {
                            x: d.x,
                            y: d.y,
                            z: d.z,
                        })
                    {
                        return Err("single-volume dimensions changed during decode".into());
                    }
                    *cache = Some(pixels);
                }
                let pixels = cache.as_ref().unwrap();
                let start = z * plane;
                out[..plane].copy_from_slice(&pixels[start..start + plane]);
            }
            FileBacking::Nd {
                file,
                streams,
                cache,
            } => {
                let key = t * d.c + c;
                let miss = cache.as_ref().is_none_or(|v| v.key != key);
                if miss {
                    let payload = read_payload(file, streams[key])?;
                    let (pixels, dd) = decompress_u16(&payload).map_err(|e| e.to_string())?;
                    if dd
                        != (Dims {
                            x: d.x,
                            y: d.y,
                            z: d.z,
                        })
                    {
                        return Err("RDM2/RDQ2 substream dimensions do not match container".into());
                    }
                    *cache = Some(CachedVolume { key, pixels });
                }
                let pixels = &cache.as_ref().unwrap().pixels;
                let start = z * plane;
                out[..plane].copy_from_slice(&pixels[start..start + plane]);
            }
            FileBacking::Stream {
                file,
                chunks,
                chunk_z,
                chunks_per_volume,
                cache,
            } => {
                let mut key = (t * d.c + c) * *chunks_per_volume + z / *chunk_z;
                let valid = chunks
                    .get(key)
                    .is_some_and(|ch| ch.t == t && ch.c == c && z >= ch.z0 && z < ch.z0 + ch.depth);
                if !valid {
                    key = chunks
                        .iter()
                        .position(|ch| ch.t == t && ch.c == c && z >= ch.z0 && z < ch.z0 + ch.depth)
                        .ok_or("could not locate requested streaming Radelta chunk")?;
                }
                let miss = cache.as_ref().is_none_or(|v| v.key != key);
                if miss {
                    let ch = chunks[key];
                    let payload = read_payload(
                        file,
                        Segment {
                            offset: ch.offset,
                            len: ch.len,
                        },
                    )?;
                    let (pixels, dd) = decompress_u16(&payload).map_err(|e| e.to_string())?;
                    if dd
                        != (Dims {
                            x: d.x,
                            y: d.y,
                            z: ch.depth,
                        })
                    {
                        return Err("RDS3/RQS3 chunk dimensions do not match container".into());
                    }
                    *cache = Some(CachedChunk {
                        key,
                        z0: ch.z0,
                        depth: ch.depth,
                        pixels,
                    });
                }
                let cv = cache.as_ref().unwrap();
                if z < cv.z0 || z >= cv.z0 + cv.depth {
                    return Err("internal streaming chunk cache mismatch".into());
                }
                let local_z = z - cv.z0;
                let start = local_z * plane;
                out[..plane].copy_from_slice(&cv.pixels[start..start + plane]);
            }
        }
        Ok(())
    }
}

impl RadeltaStreamWriter {
    fn new(
        path: PathBuf,
        dims: Dims5,
        mode: WriterMode,
        memory_mib: u32,
    ) -> std::result::Result<Self, String> {
        dims.validate().map_err(|e| e.to_string())?;
        let plane_voxels = dims
            .x
            .checked_mul(dims.y)
            .ok_or("plane voxel count overflow")?;
        let block_depth = match mode {
            WriterMode::Lossless(o) => {
                if o.block_depth == 0 {
                    DEFAULT_BLOCK_DEPTH
                } else {
                    o.block_depth as usize
                }
            }
            WriterMode::Lossy(o) => {
                if o.block_depth == 0 {
                    DEFAULT_BLOCK_DEPTH
                } else {
                    o.block_depth as usize
                }
            }
        };
        let memory_mib = if memory_mib == 0 {
            DEFAULT_STREAM_MEMORY_MIB as u32
        } else {
            memory_mib
        };
        let chunk_z = choose_stream_chunk_depth(dims, memory_mib as usize, block_depth)
            .map_err(|e| e.to_string())?;
        let chunks_per_volume = dims.z.div_ceil(chunk_z);
        let expected_chunks = (dims.t as u64)
            .checked_mul(dims.c as u64)
            .and_then(|v| v.checked_mul(chunks_per_volume as u64))
            .ok_or("chunk count overflow")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut out = BufWriter::new(file);
        let magic = match mode {
            WriterMode::Lossless(_) => STREAM_MAGIC_LOSSLESS,
            WriterMode::Lossy(_) => STREAM_MAGIC_LOSSY,
        };
        stream_header(&mut out, magic, dims, chunk_z, expected_chunks)?;
        Ok(Self {
            dims,
            plane_voxels,
            chunk_z,
            expected_chunks,
            written_chunks: 0,
            next_t: 0,
            next_c: 0,
            next_z: 0,
            chunk_t: 0,
            chunk_c: 0,
            chunk_z0: 0,
            chunk_pixels: Vec::with_capacity(
                plane_voxels
                    .checked_mul(chunk_z)
                    .ok_or("chunk buffer size overflow")?,
            ),
            mode,
            out,
            finished: false,
            metadata: Vec::new(),
        })
    }

    fn write_plane(
        &mut self,
        t: usize,
        c: usize,
        z: usize,
        plane: &[u16],
    ) -> std::result::Result<(), String> {
        if self.finished {
            return Err("Radelta writer has already been finished".into());
        }
        if plane.len() != self.plane_voxels {
            return Err(format!(
                "input plane has {} pixels but {} were expected",
                plane.len(),
                self.plane_voxels
            ));
        }
        if t != self.next_t || c != self.next_c || z != self.next_z {
            return Err(format!(
                "planes must be written sequentially in T,C,Z order; expected ({},{},{}), got ({},{},{})",
                self.next_t, self.next_c, self.next_z, t, c, z
            ));
        }
        if self.chunk_pixels.is_empty() {
            self.chunk_t = t;
            self.chunk_c = c;
            self.chunk_z0 = z;
        }
        self.chunk_pixels.extend_from_slice(plane);
        let local_depth = self.chunk_pixels.len() / self.plane_voxels;
        let end_of_volume = z + 1 == self.dims.z;
        self.advance_expected();
        if local_depth == self.chunk_z || end_of_volume {
            self.flush_chunk()?;
        }
        Ok(())
    }

    fn advance_expected(&mut self) {
        self.next_z += 1;
        if self.next_z == self.dims.z {
            self.next_z = 0;
            self.next_c += 1;
            if self.next_c == self.dims.c {
                self.next_c = 0;
                self.next_t += 1;
            }
        }
    }

    fn flush_chunk(&mut self) -> std::result::Result<(), String> {
        if self.chunk_pixels.is_empty() {
            return Ok(());
        }
        let depth = self.chunk_pixels.len() / self.plane_voxels;
        if depth == 0 {
            return Ok(());
        }
        let d = Dims {
            x: self.dims.x,
            y: self.dims.y,
            z: depth,
        };
        let payload = match self.mode {
            WriterMode::Lossless(opts) => {
                compress_u16(&self.chunk_pixels, d, opts).map_err(|e| e.to_string())?
            }
            WriterMode::Lossy(opts) => {
                compress_lossy_u16(&self.chunk_pixels, d, opts).map_err(|e| e.to_string())?
            }
        };
        write_u32(&mut self.out, self.chunk_t as u32).map_err(|e| e.to_string())?;
        write_u32(&mut self.out, self.chunk_c as u32).map_err(|e| e.to_string())?;
        write_u32(&mut self.out, self.chunk_z0 as u32).map_err(|e| e.to_string())?;
        write_u32(&mut self.out, depth as u32).map_err(|e| e.to_string())?;
        write_u64(&mut self.out, payload.len() as u64).map_err(|e| e.to_string())?;
        self.out.write_all(&payload).map_err(|e| e.to_string())?;
        self.written_chunks += 1;
        self.chunk_pixels.clear();
        Ok(())
    }

    fn finish(&mut self) -> std::result::Result<(), String> {
        if self.finished {
            return Ok(());
        }
        if self.next_t != self.dims.t || self.next_c != 0 || self.next_z != 0 {
            return Err(format!(
                "not all planes were written before finish; next expected plane would be ({},{},{})",
                self.next_t, self.next_c, self.next_z
            ));
        }
        self.flush_chunk()?;
        if self.written_chunks != self.expected_chunks {
            return Err(format!(
                "streaming writer emitted {} chunks, but {} were expected",
                self.written_chunks, self.expected_chunks
            ));
        }
        self.out.flush().map_err(|e| e.to_string())?;
        if !self.metadata.is_empty() {
            crate::metadata::set_file_handle_metadata(self.out.get_mut(), &self.metadata)
                .map_err(|e| e.to_string())?;
        }
        self.finished = true;
        Ok(())
    }
}

/// Open a Radelta file for random plane access.
///
/// # Safety
/// `path_utf8` must point to a NUL-terminated UTF-8 string and `out_handle` must
/// be writable. The returned handle must eventually be passed to
/// [`radelta_file_close`].
#[no_mangle]
pub unsafe extern "C" fn radelta_file_open(
    path_utf8: *const c_char,
    out_handle: *mut *mut RadeltaFileHandle,
) -> i32 {
    if path_utf8.is_null() || out_handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    *out_handle = ptr::null_mut();
    let path = match CStr::from_ptr(path_utf8).to_str() {
        Ok(s) => PathBuf::from(s),
        Err(_) => {
            set_last_error("Radelta path is not valid UTF-8");
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    match open_impl(path) {
        Ok(handle) => {
            *out_handle = Box::into_raw(Box::new(handle));
            RADELTA_OK
        }
        Err(e) => {
            set_last_error(e);
            RADELTA_IO_ERROR
        }
    }
}

/// Close a file handle returned by [`radelta_file_open`].
///
/// # Safety
/// `handle` must be null or a live handle returned by `radelta_file_open`, and
/// it must not be used again after this call.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_close(handle: *mut RadeltaFileHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// Query shape, format and flags for an open Radelta file.
///
/// # Safety
/// `handle` must be a live file handle and `out_info` must be writable.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_get_info(
    handle: *const RadeltaFileHandle,
    out_info: *mut RadeltaFileInfo,
) -> i32 {
    if handle.is_null() || out_info.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    *out_info = (*handle).info();
    RADELTA_OK
}

/// Read one TCZ plane from an open Radelta file.
///
/// # Safety
/// `handle` must be live. `output` must reference at least
/// `output_capacity_voxels` writable `u16` values.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_read_plane_u16(
    handle: *const RadeltaFileHandle,
    t: u32,
    c: u32,
    z: u32,
    output: *mut u16,
    output_capacity_voxels: u64,
) -> i32 {
    if handle.is_null() || output.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let h = &*handle;
    let plane = match h.dims.x.checked_mul(h.dims.y) {
        Some(v) => v,
        None => {
            set_last_error("plane size overflow");
            return RADELTA_CODEC_ERROR;
        }
    };
    if output_capacity_voxels < plane as u64 {
        return RADELTA_INVALID_ARGUMENT;
    }
    let out = std::slice::from_raw_parts_mut(output, plane);
    match h.read_plane(t as usize, c as usize, z as usize, out) {
        Ok(()) => RADELTA_OK,
        Err(e) => {
            set_last_error(e);
            RADELTA_CODEC_ERROR
        }
    }
}

/// Create a streaming lossless RDS3 writer.
///
/// # Safety
/// `path_utf8` must point to a NUL-terminated UTF-8 string and `out_handle` must
/// be writable. The returned handle must eventually be closed.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_create_lossless_u16(
    path_utf8: *const c_char,
    nx: u32,
    ny: u32,
    nz: u32,
    nc: u32,
    nt: u32,
    memory_mib: u32,
    out_handle: *mut *mut RadeltaWriterHandle,
) -> i32 {
    if path_utf8.is_null() || out_handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    *out_handle = ptr::null_mut();
    let path = match CStr::from_ptr(path_utf8).to_str() {
        Ok(s) => PathBuf::from(s),
        Err(_) => {
            set_last_error("Radelta path is not valid UTF-8");
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    let dims = match dims5(nx, ny, nz, nc, nt) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(e);
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    match RadeltaStreamWriter::new(
        path,
        dims,
        WriterMode::Lossless(Options::default()),
        memory_mib,
    ) {
        Ok(writer) => {
            *out_handle = Box::into_raw(Box::new(RadeltaWriterHandle { writer }));
            RADELTA_OK
        }
        Err(e) => {
            set_last_error(e);
            RADELTA_IO_ERROR
        }
    }
}

/// Create a streaming calibrated-lossy RQS3 writer.
///
/// # Safety
/// `path_utf8` must point to a NUL-terminated UTF-8 string and `out_handle` must
/// be writable. The returned handle must eventually be closed.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_create_lossy_u16(
    path_utf8: *const c_char,
    nx: u32,
    ny: u32,
    nz: u32,
    nc: u32,
    nt: u32,
    offset_adu: f64,
    gain_e_per_adu: f64,
    noise_step: f64,
    memory_mib: u32,
    out_handle: *mut *mut RadeltaWriterHandle,
) -> i32 {
    if path_utf8.is_null() || out_handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    *out_handle = ptr::null_mut();
    let path = match CStr::from_ptr(path_utf8).to_str() {
        Ok(s) => PathBuf::from(s),
        Err(_) => {
            set_last_error("Radelta path is not valid UTF-8");
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    let dims = match dims5(nx, ny, nz, nc, nt) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(e);
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    let opts = LossyOptions {
        offset_adu,
        gain_e_per_adu,
        noise_step,
        ..LossyOptions::default()
    };
    if let Err(e) = opts.validate_calibration() {
        set_last_error(e.to_string());
        return RADELTA_INVALID_ARGUMENT;
    }
    match RadeltaStreamWriter::new(path, dims, WriterMode::Lossy(opts), memory_mib) {
        Ok(writer) => {
            *out_handle = Box::into_raw(Box::new(RadeltaWriterHandle { writer }));
            RADELTA_OK
        }
        Err(e) => {
            set_last_error(e);
            RADELTA_IO_ERROR
        }
    }
}

/// Append the next plane to a streaming writer in increasing T,C,Z order.
///
/// # Safety
/// `handle` must be a live writer handle. `input` must reference exactly
/// `input_voxels` readable `u16` values.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_write_plane_u16(
    handle: *mut RadeltaWriterHandle,
    t: u32,
    c: u32,
    z: u32,
    input: *const u16,
    input_voxels: u64,
) -> i32 {
    if handle.is_null() || input.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let h = &mut *handle;
    if input_voxels != h.writer.plane_voxels as u64 {
        set_last_error(format!(
            "input plane contains {} voxels, but {} were expected",
            input_voxels, h.writer.plane_voxels
        ));
        return RADELTA_INVALID_ARGUMENT;
    }
    let plane = std::slice::from_raw_parts(input, h.writer.plane_voxels);
    match h
        .writer
        .write_plane(t as usize, c as usize, z as usize, plane)
    {
        Ok(()) => RADELTA_OK,
        Err(e) => {
            set_last_error(e);
            RADELTA_CODEC_ERROR
        }
    }
}

/// Finish a streaming writer and flush its final chunk.
///
/// # Safety
/// `handle` must be a live writer handle returned by a writer-create function.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_finish(handle: *mut RadeltaWriterHandle) -> i32 {
    if handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let h = &mut *handle;
    match h.writer.finish() {
        Ok(()) => RADELTA_OK,
        Err(e) => {
            set_last_error(e);
            RADELTA_CODEC_ERROR
        }
    }
}

/// Close a streaming writer handle.
///
/// # Safety
/// `handle` must be null or a live writer handle, and it must not be used after
/// this call. Closing does not implicitly report an unfinished-stream error.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_close(handle: *mut RadeltaWriterHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// Copy the calling thread's latest file-API error message.
///
/// # Safety
/// If `buffer` is non-null, it must reference at least `capacity` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_last_error(buffer: *mut c_char, capacity: u64) -> u64 {
    let msg = LAST_ERROR.with(|s| s.borrow().clone());
    let needed = msg.len() as u64 + 1;
    if buffer.is_null() || capacity == 0 {
        return needed;
    }
    let cap = usize::try_from(capacity).unwrap_or(usize::MAX);
    let max_copy = cap.saturating_sub(1).min(msg.len());
    ptr::copy_nonoverlapping(msg.as_ptr(), buffer as *mut u8, max_copy);
    *(buffer as *mut u8).add(max_copy) = 0;
    needed
}

/// Read the opaque metadata associated with a file. NULL output queries size.
/// # Safety
/// Handle and output_size must be valid; non-NULL output must have capacity bytes.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_get_metadata(
    handle: *mut RadeltaFileHandle,
    output: *mut u8,
    capacity: usize,
    output_size: *mut usize,
) -> i32 {
    if handle.is_null() || output_size.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let data = &(*handle).metadata;
    *output_size = data.len();
    if data.is_empty() {
        return RADELTA_OK;
    }
    if output.is_null() || capacity < data.len() {
        return super::RADELTA_BUFFER_TOO_SMALL;
    }
    ptr::copy_nonoverlapping(data.as_ptr(), output, data.len());
    RADELTA_OK
}

/// Set or replace opaque writer metadata before finish; zero length clears it.
/// # Safety
/// Handle must be valid, and data must point to len readable bytes unless len is zero.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_set_metadata(
    handle: *mut RadeltaWriterHandle,
    data: *const u8,
    len: usize,
) -> i32 {
    if handle.is_null() || (data.is_null() && len != 0) || len > crate::metadata::metadata_limit() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let writer = &mut (*handle).writer;
    if writer.finished {
        set_last_error("writer is already finished");
        return RADELTA_INVALID_ARGUMENT;
    }
    writer.metadata = if len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data, len).to_vec()
    };
    RADELTA_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(stem: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "radelta-{stem}-{}-{nonce}.rdlt",
            std::process::id()
        ))
    }

    fn plane_values(dims: Dims5, t: usize, c: usize, z: usize) -> Vec<u16> {
        let mut out = Vec::with_capacity(dims.x * dims.y);
        for y in 0..dims.y {
            for x in 0..dims.x {
                out.push(((t * 10000 + c * 3000 + z * 200 + y * 30 + x * 7) % 65536) as u16);
            }
        }
        out
    }

    #[test]
    fn single_volume_file_api_roundtrip() {
        let path = temp_path("single-volume");
        let dims = crate::Dims { x: 3, y: 2, z: 1 };
        let input = vec![0; dims.voxels().unwrap()];
        let lossless = crate::compress_u16(&input, dims, Options::default()).unwrap();
        let lossy = crate::compress_lossy_u16(&input, dims, LossyOptions::default()).unwrap();
        for encoded in [lossless, lossy] {
            fs::write(&path, &encoded).unwrap();
            let handle = open_impl(path.clone()).unwrap();
            let mut decoded = vec![1; input.len()];
            handle.read_plane(0, 0, 0, &mut decoded).unwrap();
            assert_eq!(decoded, input);
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn streaming_metadata_c_api_roundtrip_and_replacement() {
        let path = temp_path("stream-metadata");
        let dims = Dims5 {
            x: 3,
            y: 2,
            z: 3,
            c: 1,
            t: 1,
        };
        let metadata = b"arbitrary binary metadata\0\xff".repeat(1000);
        for mode in [
            WriterMode::Lossless(Options::default()),
            WriterMode::Lossy(LossyOptions::default()),
        ] {
            let mut handle = RadeltaWriterHandle {
                writer: RadeltaStreamWriter::new(path.clone(), dims, mode, 1).unwrap(),
            };
            unsafe {
                assert_eq!(
                    radelta_writer_set_metadata(&mut handle, metadata.as_ptr(), metadata.len()),
                    RADELTA_OK
                );
            }
            for z in 0..dims.z {
                handle.writer.write_plane(0, 0, z, &[0; 6]).unwrap();
            }
            handle.writer.finish().unwrap();
            let length = fs::metadata(&path).unwrap().len();
            handle.writer.finish().unwrap();
            assert_eq!(fs::metadata(&path).unwrap().len(), length);
            unsafe {
                assert_eq!(
                    radelta_writer_set_metadata(&mut handle, std::ptr::null(), 0),
                    RADELTA_INVALID_ARGUMENT
                );
            }
            drop(handle);
            let mut file = open_impl(path.clone()).unwrap();
            let mut size = 0;
            unsafe {
                assert_eq!(
                    radelta_file_get_metadata(&mut file, std::ptr::null_mut(), 0, &mut size),
                    super::super::RADELTA_BUFFER_TOO_SMALL
                );
            }
            let mut actual = vec![0; size];
            unsafe {
                assert_eq!(
                    radelta_file_get_metadata(
                        &mut file,
                        actual.as_mut_ptr(),
                        actual.len(),
                        &mut size
                    ),
                    RADELTA_OK
                );
            }
            assert_eq!(actual, metadata);
            let mut plane = [1; 6];
            file.read_plane(0, 0, 2, &mut plane).unwrap();
            assert_eq!(plane, [0; 6]);
            drop(file);
            crate::set_file_metadata(&path, b"replacement").unwrap();
            assert_eq!(crate::read_file_metadata(&path).unwrap(), b"replacement");
            crate::set_file_metadata(&path, &[]).unwrap();
            assert!(crate::read_file_metadata(&path).unwrap().is_empty());
            let file = open_impl(path.clone()).unwrap();
            file.read_plane(0, 0, 0, &mut plane).unwrap();
            assert_eq!(plane, [0; 6]);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn streaming_writer_roundtrip_and_random_plane_access() {
        let path = temp_path("stream-roundtrip");
        let dims = Dims5 {
            x: 7,
            y: 5,
            z: 5,
            c: 2,
            t: 2,
        };
        {
            let mut writer = RadeltaStreamWriter::new(
                path.clone(),
                dims,
                WriterMode::Lossless(Options::default()),
                1,
            )
            .unwrap();

            for t in 0..dims.t {
                for c in 0..dims.c {
                    for z in 0..dims.z {
                        let plane = plane_values(dims, t, c, z);
                        writer.write_plane(t, c, z, &plane).unwrap();
                    }
                }
            }
            writer.finish().unwrap();
        }

        let handle = open_impl(path.clone()).unwrap();
        let info = handle.info();
        assert_eq!(
            (info.nx, info.ny, info.nz, info.nc, info.nt),
            (7, 5, 5, 2, 2)
        );
        assert_eq!(info.format, RADELTA_FILE_FORMAT_RDS3);
        assert_eq!(
            info.flags,
            RADELTA_FILE_FLAG_MULTIDIMENSIONAL | RADELTA_FILE_FLAG_STREAMING
        );

        for &(t, c, z) in &[(1usize, 1usize, 4usize), (0, 1, 2), (1, 0, 0), (0, 0, 3)] {
            let mut decoded = vec![0u16; dims.x * dims.y];
            handle.read_plane(t, c, z, &mut decoded).unwrap();
            assert_eq!(decoded, plane_values(dims, t, c, z));
        }

        drop(handle);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn streaming_writer_rejects_out_of_order_planes() {
        let path = temp_path("out-of-order");
        let dims = Dims5 {
            x: 4,
            y: 3,
            z: 2,
            c: 1,
            t: 1,
        };
        {
            let mut writer = RadeltaStreamWriter::new(
                path.clone(),
                dims,
                WriterMode::Lossless(Options::default()),
                1,
            )
            .unwrap();
            let plane = plane_values(dims, 0, 0, 0);
            assert!(writer.write_plane(0, 0, 1, &plane).is_err());
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn streaming_writer_finish_requires_all_planes() {
        let path = temp_path("incomplete");
        let dims = Dims5 {
            x: 4,
            y: 3,
            z: 3,
            c: 1,
            t: 1,
        };
        {
            let mut writer = RadeltaStreamWriter::new(
                path.clone(),
                dims,
                WriterMode::Lossless(Options::default()),
                1,
            )
            .unwrap();
            let plane = plane_values(dims, 0, 0, 0);
            writer.write_plane(0, 0, 0, &plane).unwrap();
            assert!(writer.finish().is_err());
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn streaming_writer_finish_is_idempotent() {
        let path = temp_path("finish-idempotent");
        let dims = Dims5 {
            x: 3,
            y: 2,
            z: 1,
            c: 1,
            t: 1,
        };
        {
            let mut writer = RadeltaStreamWriter::new(
                path.clone(),
                dims,
                WriterMode::Lossless(Options::default()),
                1,
            )
            .unwrap();
            let plane = plane_values(dims, 0, 0, 0);
            writer.write_plane(0, 0, 0, &plane).unwrap();
            writer.finish().unwrap();
            writer.finish().unwrap();
        }
        let _ = fs::remove_file(path);
    }
    #[test]
    fn streaming_lossy_writer_roundtrip_matches_calibrated_reconstruction() {
        let path = temp_path("stream-lossy");
        let dims = Dims5 {
            x: 6,
            y: 4,
            z: 3,
            c: 1,
            t: 1,
        };
        let opts = LossyOptions {
            offset_adu: 50.0,
            gain_e_per_adu: 0.5,
            noise_step: 2.0,
            ..LossyOptions::default()
        };
        {
            let mut writer =
                RadeltaStreamWriter::new(path.clone(), dims, WriterMode::Lossy(opts), 1).unwrap();
            for z in 0..dims.z {
                let plane = plane_values(dims, 0, 0, z);
                writer.write_plane(0, 0, z, &plane).unwrap();
            }
            writer.finish().unwrap();
        }

        let handle = open_impl(path.clone()).unwrap();
        assert_eq!(handle.info().format, RADELTA_FILE_FORMAT_RQS3);
        for z in 0..dims.z {
            let source = plane_values(dims, 0, 0, z);
            let mut decoded = vec![0u16; dims.x * dims.y];
            handle.read_plane(0, 0, z, &mut decoded).unwrap();
            for (&input, &output) in source.iter().zip(decoded.iter()) {
                let q = crate::lossy_quantize(
                    input,
                    opts.offset_adu,
                    opts.gain_e_per_adu,
                    opts.noise_step,
                );
                assert!(q <= u8::MAX as u16);
                let expected = crate::lossy_reconstruct(
                    q as u8,
                    opts.offset_adu,
                    opts.gain_e_per_adu,
                    opts.noise_step,
                );
                assert_eq!(output, expected);
            }
        }
        drop(handle);
        let _ = fs::remove_file(path);
    }
}
