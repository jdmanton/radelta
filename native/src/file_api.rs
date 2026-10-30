//! File-oriented C ABI used by the ImageJ/Fiji virtual-stack reader/writer.
//!
//! This API deliberately hides the on-disk Radelta container variants. A
//! caller opens a file, queries its TCZYX shape, and requests individual
//! uint16 planes, or creates a streaming writer and feeds planes sequentially.
//! RDS3/RQS3 files are indexed without loading their payloads; only the
//! compressed chunk containing a requested plane is decoded.

use super::{
    choose_stream_chunk_depth, compress_lossy_u16, compress_u16, decompress_u16_expecting,
    inspect_dims, Dims, Dims5, LossyOptions, Options, Reader, DEFAULT_BLOCK_DEPTH,
    DEFAULT_STREAM_MEMORY_MIB, LOSSY_VERSION, MAGIC, MAGIC_LOSSY, MAGIC_ND, MAGIC_ND_LOSSY,
    ND_VERSION, RADELTA_CODEC_ERROR, RADELTA_INVALID_ARGUMENT, RADELTA_OK, STREAM_MAGIC_LOSSLESS,
    STREAM_MAGIC_LOSSY, STREAM_VERSION, VERSION,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
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

/// An independently decoded volume or streaming chunk within one open file.
/// Keys identify units only within that handle. The last chunk can be shorter.
#[derive(Clone, Copy, Debug)]
pub struct RadeltaDecodeUnit {
    pub key: usize,
    pub z0: usize,
    pub depth: usize,
    pub encoded_bytes: u64,
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

/// Budget for retaining decoded volumes/chunks across interleaved plane reads.
/// A single oversized entry is still retained, as in the original reader.
pub const DEFAULT_FILE_CACHE_BYTES: usize = 2 * 1024 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 128;
// Additional decoded pixels plus compressed payloads admitted by one batch.
// Codec scratch and the existing retention cache are separate; one oversized
// unit must always be able to proceed.
const BATCH_WORKING_BYTES: u64 = 256 * 1024 * 1024;

struct CachedPixels {
    key: usize,
    pixels: Vec<u16>,
}

struct PixelCache {
    entries: VecDeque<CachedPixels>,
    bytes: usize,
    limit: usize,
    last_read: Option<(usize, usize)>,
    recent_keys: VecDeque<usize>,
    reuse_detected: bool,
}
impl PixelCache {
    fn new(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit,
            last_read: None,
            recent_keys: VecDeque::new(),
            reuse_detected: false,
        }
    }
    fn note_read(&mut self, key: usize, z: usize) {
        if !self.reuse_detected {
            if let Some((previous, previous_z)) = self.last_read {
                if previous != key {
                    // Same-Z channel switches are the common Fiji case.
                    // Revisiting an older key also enables caching for less
                    // regular browsing, with bounded bookkeeping.
                    if previous_z == z || self.recent_keys.contains(&key) {
                        self.reuse_detected = true;
                        self.recent_keys.clear();
                    } else {
                        if self.recent_keys.len() == MAX_CACHE_ENTRIES {
                            self.recent_keys.pop_front();
                        }
                        self.recent_keys.push_back(previous);
                    }
                }
            }
        }
        self.last_read = Some((key, z));
    }
    fn prepare_read(&mut self, bytes: usize) {
        // A first sequential pass has no reuse between entries. Preserve the
        // original single-entry memory footprint until reuse is observed.
        if !self.reuse_detected {
            while !self.entries.is_empty() {
                self.evict_oldest();
            }
        }
        self.prepare(bytes);
    }
    #[cfg(test)]
    fn contains(&self, key: usize) -> bool {
        self.entries.iter().any(|entry| entry.key == key)
    }
    fn evict_oldest(&mut self) {
        if let Some(entry) = self.entries.pop_back() {
            self.bytes -= entry.pixels.capacity() * std::mem::size_of::<u16>();
        }
    }
    // Evict before allocating another decoded volume, rather than allowing
    // old cached data to inflate the decoder's peak working set.
    fn prepare(&mut self, bytes: usize) {
        while !self.entries.is_empty()
            && (self.entries.len() >= MAX_CACHE_ENTRIES
                || self.bytes.checked_add(bytes).is_none_or(|n| n > self.limit))
        {
            self.evict_oldest();
        }
    }
    fn insert(&mut self, key: usize, pixels: Vec<u16>) {
        // A batch and another reader can finish decoding the same key. Keep
        // one retained copy and account for its actual allocation capacity.
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.bytes -=
                self.entries.remove(index).unwrap().pixels.capacity() * std::mem::size_of::<u16>();
        }
        let bytes = pixels.capacity() * std::mem::size_of::<u16>();
        self.prepare(bytes);
        self.bytes += bytes;
        self.entries.push_front(CachedPixels { key, pixels });
    }
    fn get(&mut self, key: usize) -> Option<&[u16]> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        if index != 0 {
            let entry = self.entries.remove(index).unwrap();
            self.entries.push_front(entry);
        }
        Some(&self.entries.front().unwrap().pixels)
    }
    fn set_limit(&mut self, bytes: usize) {
        self.limit = bytes;
        // Zero restores single-entry behavior. A lone oversized volume/chunk
        // remains usable without decoding it again for every plane.
        while self.entries.len() > 1 && self.bytes > self.limit {
            self.evict_oldest();
        }
    }
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
struct DecodeUnit {
    key: usize,
    segment: Segment,
    z0: usize,
    depth: usize,
    context: &'static str,
}

struct PlaneBatch<'a> {
    unit: DecodeUnit,
    targets: Vec<(usize, &'a mut [u16])>,
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

fn decode_unit(
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

#[derive(Clone, Copy)]
enum WriterMode {
    Lossless(Options),
    Lossy(LossyOptions),
}

/// Bounded streaming encoder shared by the CLI and file-writer C ABI.
/// Queued chunks retain their order; `finish` waits for every compression job
/// before appending metadata and publishing the completed file.
pub struct RadeltaStreamWriter {
    dims: Dims5,
    plane_voxels: usize,
    chunk_z: usize,
    expected_chunks: u64,
    written_chunks: u64,
    payload_bytes: usize,
    raw_bytes: usize,
    pending: VecDeque<std::sync::mpsc::Receiver<std::result::Result<EncodedChunk, String>>>,
    pending_limit: usize,
    failed: Option<String>,
    next_t: usize,
    next_c: usize,
    next_z: usize,
    chunk_t: usize,
    chunk_c: usize,
    chunk_z0: usize,
    chunk_pixels: Vec<u16>,
    mode: WriterMode,
    out: BufWriter<File>,
    pending_output: crate::output::PendingOutput,
    finished: bool,
    metadata: Vec<u8>,
}

struct EncodedChunk {
    t: usize,
    c: usize,
    z0: usize,
    depth: usize,
    pixels: Vec<u16>,
    payload: Vec<u8>,
}

fn wait_chunk(
    receiver: std::sync::mpsc::Receiver<std::result::Result<EncodedChunk, String>>,
) -> std::result::Result<EncodedChunk, String> {
    // A Rust caller can feed/finish a writer from a Rayon worker, including a
    // one-thread pool. Cooperate with that pool instead of blocking its worker.
    if rayon::current_thread_index().is_some() {
        loop {
            match receiver.try_recv() {
                Ok(result) => return result,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err("compression worker disconnected".into())
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    rayon::yield_now();
                    std::thread::yield_now();
                }
            }
        }
    }
    receiver
        .recv()
        .map_err(|_| "compression worker disconnected")?
}

/// Divide the existing approximate working budget between encode slots.
/// Four raw-chunk sizes per slot cover input buffers,
/// completed payloads and an estimate of codec working memory. Recycle each
/// completed input into the feeder. Keep enough codec blocks in a chunk to
/// amortize models; use additional slots for naturally shallow chunks.
fn stream_pipeline_plan(
    d: Dims5,
    memory_mib: usize,
    block: usize,
    threads: usize,
) -> std::result::Result<(usize, usize), String> {
    let serial = choose_stream_chunk_depth(d, memory_mib, block).map_err(|e| e.to_string())?;
    let budget = memory_mib
        .checked_mul(1024 * 1024)
        .ok_or("memory budget overflow")?;
    let plane =
        d.x.checked_mul(d.y)
            .and_then(|n| n.checked_mul(2))
            .ok_or("plane size overflow")?;
    let threads = threads.max(1);
    let block = block.max(1);
    let parallel_units = threads.div_ceil(serial.div_ceil(block)).clamp(1, 8);
    for pending in (1..=parallel_units).rev() {
        let depth = choose_stream_chunk_depth(d, memory_mib / pending, block)
            .map_err(|e| e.to_string())?
            .min(block.saturating_mul(threads));
        // Parallelize naturally shallow units, rather than subdividing a
        // memory-limited unit into many separately modelled chunks.
        if pending > 1 && depth < serial.min(block.saturating_mul(threads)) {
            continue;
        }
        if depth < block.min(d.z) && serial >= block.min(d.z) {
            continue;
        }
        if pending > threads.div_ceil(depth.div_ceil(block)) {
            continue;
        }
        if d.t * d.c * d.z.div_ceil(depth) < 2 {
            continue;
        }
        let fits = plane
            .checked_mul(depth)
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| n.checked_mul(pending))
            .is_some_and(|n| n <= budget);
        if fits {
            return Ok((depth, pending));
        }
    }
    Ok((serial, 0))
}

impl Drop for RadeltaStreamWriter {
    fn drop(&mut self) {
        // No codec task may outlive a closed/failed/unfinished writer.
        for receiver in self.pending.drain(..) {
            let _ = wait_chunk(receiver);
        }
    }
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

fn read_payload(file: &mut File, seg: Segment) -> std::result::Result<Vec<u8>, String> {
    let len = usize::try_from(seg.len).map_err(|_| "compressed payload exceeds platform limits")?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|_| "cannot allocate compressed Radelta payload")?;
    buf.resize(len, 0);
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

    /// Nominal Z depth decoded by one indexed read. The final streaming chunk
    /// in a volume can be shorter. Useful for avoiding batches that cannot
    /// expose parallelism across stored units.
    pub fn decode_unit_depth(&self) -> std::result::Result<usize, String> {
        let backing = self
            .backing
            .lock()
            .map_err(|_| "Radelta file handle mutex is poisoned")?;
        Ok(match &*backing {
            FileBacking::Stream { chunk_z, .. } => *chunk_z,
            FileBacking::Single { .. } | FileBacking::Nd { .. } => self.dims.z,
        })
    }

    fn unit_at(&self, t: usize, c: usize, z: usize) -> std::result::Result<DecodeUnit, String> {
        if t >= self.dims.t || c >= self.dims.c || z >= self.dims.z {
            return Err("requested T/C/Z plane is out of range".into());
        }
        let backing = self
            .backing
            .lock()
            .map_err(|_| "Radelta file handle mutex is poisoned")?;
        decode_unit(&backing, self.dims, t, c, z)
    }

    /// Query the stored unit containing a TCZ plane without reading its payload.
    pub fn decode_unit_info(
        &self,
        t: usize,
        c: usize,
        z: usize,
    ) -> std::result::Result<RadeltaDecodeUnit, String> {
        let unit = self.unit_at(t, c, z)?;
        Ok(RadeltaDecodeUnit {
            key: unit.key,
            z0: unit.z0,
            depth: unit.depth,
            encoded_bytes: unit.segment.len,
        })
    }

    /// Decode the whole unit containing a TCZ plane, bypassing retention caches.
    /// The returned pixels are Z,Y,X ordered and owned by the caller. Concurrent
    /// calls share the Rayon codec pool; file reads alone hold the handle lock.
    pub fn decode_unit(
        &self,
        t: usize,
        c: usize,
        z: usize,
    ) -> std::result::Result<Vec<u16>, String> {
        self.decode_pixels(self.unit_at(t, c, z)?)
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

    /// Set the decoded volume/chunk retention budget in bytes. Zero restores
    /// the original single-entry cache. This does not cap decoder working
    /// memory: one oversized entry is always allowed, as before.
    pub fn set_cache_limit(&self, bytes: usize) -> std::result::Result<(), String> {
        let mut backing = self
            .backing
            .lock()
            .map_err(|_| "Radelta file handle mutex is poisoned")?;
        match &mut *backing {
            FileBacking::Nd { cache, .. } | FileBacking::Stream { cache, .. } => {
                cache.set_limit(bytes)
            }
            FileBacking::Single { .. } => (),
        }
        Ok(())
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
        let unit = {
            let mut backing = self
                .backing
                .lock()
                .map_err(|_| "Radelta file handle mutex is poisoned")?;
            let unit = decode_unit(&backing, d, t, c, z)?;
            let cached = match &mut *backing {
                FileBacking::Single { cache, .. } => cache.as_deref(),
                FileBacking::Nd { cache, .. } | FileBacking::Stream { cache, .. } => {
                    cache.note_read(unit.key, z);
                    cache.get(unit.key)
                }
            };
            if let Some(pixels) = cached {
                let start = (z - unit.z0) * plane;
                out[..plane].copy_from_slice(&pixels[start..start + plane]);
                return Ok(());
            }
            if let FileBacking::Nd { cache, .. } | FileBacking::Stream { cache, .. } = &mut *backing
            {
                let bytes = plane
                    .checked_mul(unit.depth)
                    .and_then(|n| n.checked_mul(std::mem::size_of::<u16>()))
                    .ok_or("decoded unit size overflow")?;
                cache.prepare_read(bytes);
            }
            unit
        };
        // Never hold the backing lock while entering Rayon: a concurrent batch
        // may occupy its workers waiting for the same lock. decode_pixels locks
        // only to read the compressed payload, then decodes outside the lock.
        let pixels = self.decode_pixels(unit)?;
        let start = (z - unit.z0) * plane;
        out[..plane].copy_from_slice(&pixels[start..start + plane]);
        let mut backing = self
            .backing
            .lock()
            .map_err(|_| "Radelta file handle mutex is poisoned")?;
        match &mut *backing {
            FileBacking::Single { cache, .. } => *cache = Some(pixels),
            FileBacking::Nd { cache, .. } | FileBacking::Stream { cache, .. } => {
                // Other readers may have changed the cache while decoding.
                cache.prepare_read(pixels.capacity() * std::mem::size_of::<u16>());
                cache.insert(unit.key, pixels);
            }
        }
        Ok(())
    }

    /// Read TCZ coordinates into consecutive planes in the requested order.
    /// Repeated coordinates are supported. Each missing volume/chunk is decoded
    /// once per call, using the current Rayon pool across independent units as
    /// well as codec blocks. Additional in-flight pixels and compressed payloads
    /// are bounded to 256 MiB per call, except for one oversized unit. The cache
    /// budget is unchanged. On a decode/I/O error, output may be partly written.
    pub fn read_planes(
        &self,
        planes: &[(usize, usize, usize)],
        out: &mut [u16],
    ) -> std::result::Result<(), String> {
        self.read_planes_with_budget(planes, out, BATCH_WORKING_BYTES)
    }

    /// Batched reads with a caller-selected in-flight pixel/payload allowance.
    /// Codec scratch, retained cache and caller output are separate; one unit
    /// larger than the allowance still proceeds. The C ABI keeps its default.
    pub fn read_planes_with_budget(
        &self,
        planes: &[(usize, usize, usize)],
        out: &mut [u16],
        working_bytes: u64,
    ) -> std::result::Result<(), String> {
        use rayon::prelude::*;
        let d = self.dims;
        let plane = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
        let needed = planes
            .len()
            .checked_mul(plane)
            .ok_or("plane count overflow")?;
        if out.len() < needed {
            return Err("output plane buffer is too small".into());
        }
        if planes
            .iter()
            .any(|&(t, c, z)| t >= d.t || c >= d.c || z >= d.z)
        {
            return Err("requested T/C/Z plane is out of range".into());
        }
        if planes.is_empty() {
            return Ok(());
        }
        if let &[(t, c, z)] = planes {
            return self.read_plane(t, c, z, out);
        }
        let mut jobs: Vec<PlaneBatch<'_>> = Vec::new();
        {
            let mut backing = self
                .backing
                .lock()
                .map_err(|_| "Radelta file handle mutex is poisoned")?;
            let mut index = HashMap::new();
            for (&(t, c, z), dst) in planes.iter().zip(out[..needed].chunks_exact_mut(plane)) {
                let unit = decode_unit(&backing, d, t, c, z)?;
                let cached = match &mut *backing {
                    FileBacking::Single { cache, .. } => cache.as_deref(),
                    FileBacking::Nd { cache, .. } | FileBacking::Stream { cache, .. } => {
                        cache.note_read(unit.key, z);
                        cache.get(unit.key)
                    }
                };
                let start = (z - unit.z0) * plane;
                if let Some(pixels) = cached {
                    dst.copy_from_slice(&pixels[start..start + plane]);
                } else {
                    let i = *index.entry(unit.key).or_insert_with(|| {
                        jobs.push(PlaneBatch {
                            unit,
                            targets: Vec::new(),
                        });
                        jobs.len() - 1
                    });
                    jobs[i].targets.push((start, dst));
                }
            }
        }
        // Limit each wave by both memory and pool width. Nested codec work
        // shares this pool: no extra OS threads or nested thread pools.
        let mut jobs = jobs.into_iter().peekable();
        let workers = rayon::current_num_threads();
        while jobs.peek().is_some() {
            let mut bytes = 0u64;
            let mut wave = Vec::new();
            while wave.len() < workers {
                let Some(job) = jobs.peek() else { break };
                let size = (plane as u64)
                    .checked_mul(job.unit.depth as u64)
                    .and_then(|n| n.checked_mul(2))
                    .and_then(|n| n.checked_add(job.unit.segment.len))
                    .ok_or("batch working size overflow")?;
                if !wave.is_empty() && size > working_bytes.saturating_sub(bytes) {
                    break;
                }
                bytes = bytes
                    .checked_add(size)
                    .ok_or("batch working size overflow")?;
                wave.push(jobs.next().unwrap());
            }
            let decoded: std::result::Result<Vec<_>, String> = wave
                .into_par_iter()
                .map(|job| self.read_batch(job))
                .collect();
            // Retain results in REQUEST order, not worker completion order.
            // Sequential caching keeps one unit, which must be the trailing
            // requested unit when the next call continues in the same chunk.
            let decoded = decoded?;
            let mut backing = self
                .backing
                .lock()
                .map_err(|_| "Radelta file handle mutex is poisoned")?;
            for (unit, pixels) in decoded {
                match &mut *backing {
                    FileBacking::Single { cache, .. } => *cache = Some(pixels),
                    FileBacking::Nd { cache, .. } | FileBacking::Stream { cache, .. } => {
                        cache.prepare_read(pixels.capacity() * std::mem::size_of::<u16>());
                        cache.insert(unit.key, pixels);
                    }
                }
            }
        }
        Ok(())
    }

    fn read_batch(
        &self,
        job: PlaneBatch<'_>,
    ) -> std::result::Result<(DecodeUnit, Vec<u16>), String> {
        let unit = job.unit;
        let pixels = self.decode_pixels(unit)?;
        for (start, dst) in job.targets {
            dst.copy_from_slice(&pixels[start..start + dst.len()]);
        }
        Ok((unit, pixels))
    }

    fn decode_pixels(&self, unit: DecodeUnit) -> std::result::Result<Vec<u16>, String> {
        let payload = {
            let mut backing = self
                .backing
                .lock()
                .map_err(|_| "Radelta file handle mutex is poisoned")?;
            let file = match &mut *backing {
                FileBacking::Single { file, .. }
                | FileBacking::Nd { file, .. }
                | FileBacking::Stream { file, .. } => file,
            };
            read_payload(file, unit.segment)?
        };
        decompress_u16_expecting(
            &payload,
            Dims {
                x: self.dims.x,
                y: self.dims.y,
                z: unit.depth,
            },
        )
        .map_err(|e| format!("{}: {e}", unit.context))
    }
}

impl RadeltaStreamWriter {
    pub fn create_lossless(
        path: impl AsRef<std::path::Path>,
        dims: Dims5,
        options: Options,
        memory_mib: usize,
    ) -> std::result::Result<Self, String> {
        Self::new(
            path.as_ref().to_path_buf(),
            dims,
            WriterMode::Lossless(options),
            memory_mib,
        )
    }

    pub fn create_lossy(
        path: impl AsRef<std::path::Path>,
        dims: Dims5,
        options: LossyOptions,
        memory_mib: usize,
    ) -> std::result::Result<Self, String> {
        Self::new(
            path.as_ref().to_path_buf(),
            dims,
            WriterMode::Lossy(options),
            memory_mib,
        )
    }

    pub fn chunk_depth(&self) -> usize {
        self.chunk_z
    }
    pub fn pending_limit(&self) -> usize {
        self.pending_limit
    }
    pub fn chunks_written(&self) -> u64 {
        self.written_chunks
    }
    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }
    pub fn raw_bytes_written(&self) -> usize {
        self.raw_bytes
    }
    pub fn set_metadata(&mut self, bytes: Vec<u8>) -> std::result::Result<(), String> {
        if self.finished {
            return Err("Radelta writer has already been finished".into());
        }
        if bytes.len() > crate::metadata_limit() {
            return Err("metadata exceeds configured byte limit".into());
        }
        self.metadata = bytes;
        Ok(())
    }

    fn new(
        path: PathBuf,
        dims: Dims5,
        mode: WriterMode,
        memory_mib: usize,
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
            DEFAULT_STREAM_MEMORY_MIB
        } else {
            memory_mib
        };
        let (chunk_z, pending_limit) =
            stream_pipeline_plan(dims, memory_mib, block_depth, rayon::current_num_threads())?;
        let chunks_per_volume = dims.z.div_ceil(chunk_z);
        let expected_chunks = (dims.t as u64)
            .checked_mul(dims.c as u64)
            .and_then(|v| v.checked_mul(chunks_per_volume as u64))
            .ok_or("chunk count overflow")?;
        let mut chunk_pixels = Vec::new();
        chunk_pixels
            .try_reserve_exact(
                plane_voxels
                    .checked_mul(chunk_z)
                    .ok_or("chunk buffer size overflow")?,
            )
            .map_err(|_| "cannot allocate Radelta writer chunk buffer")?;
        let (pending_output, file) = crate::output::PendingOutput::create(&path)
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
            payload_bytes: 0,
            raw_bytes: 0,
            pending: VecDeque::new(),
            pending_limit,
            failed: None,
            next_t: 0,
            next_c: 0,
            next_z: 0,
            chunk_t: 0,
            chunk_c: 0,
            chunk_z0: 0,
            chunk_pixels,
            mode,
            out,
            pending_output,
            finished: false,
            metadata: Vec::new(),
        })
    }

    pub fn write_plane(
        &mut self,
        t: usize,
        c: usize,
        z: usize,
        plane: &[u16],
    ) -> std::result::Result<(), String> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
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
            let needed = self
                .plane_voxels
                .checked_mul(self.chunk_z)
                .ok_or("chunk size overflow")?;
            self.chunk_pixels
                .try_reserve_exact(needed)
                .map_err(|_| "cannot allocate Radelta writer chunk buffer")?;
            self.chunk_t = t;
            self.chunk_c = c;
            self.chunk_z0 = z;
        }
        self.chunk_pixels.extend_from_slice(plane);
        let local_depth = self.chunk_pixels.len() / self.plane_voxels;
        let end_of_volume = z + 1 == self.dims.z;
        self.advance_expected();
        if local_depth == self.chunk_z || end_of_volume {
            if let Err(e) = self.flush_chunk() {
                self.failed = Some(e.clone());
                return Err(e);
            }
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
        let dims = Dims {
            x: self.dims.x,
            y: self.dims.y,
            z: depth,
        };
        let mode = self.mode;
        let encode = move |pixels: Vec<u16>, t, c, z0| {
            let payload = match mode {
                WriterMode::Lossless(o) => compress_u16(&pixels, dims, o),
                WriterMode::Lossy(o) => compress_lossy_u16(&pixels, dims, o),
            }
            .map_err(|e| e.to_string())?;
            Ok(EncodedChunk {
                t,
                c,
                z0,
                depth,
                pixels,
                payload,
            })
        };
        if self.pending_limit == 0 {
            let chunk = encode(
                std::mem::take(&mut self.chunk_pixels),
                self.chunk_t,
                self.chunk_c,
                self.chunk_z0,
            )?;
            return self.write_encoded(chunk);
        }
        // Wait for the oldest result BEFORE admitting another encode. Its
        // payload can then be written while the newly submitted chunk compresses.
        let ready = if self.pending.len() == self.pending_limit {
            Some(wait_chunk(self.pending.pop_front().unwrap())?)
        } else {
            None
        };
        let pixels = std::mem::take(&mut self.chunk_pixels);
        let (t, c, z0) = (self.chunk_t, self.chunk_c, self.chunk_z0);
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        rayon::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| encode(pixels, t, c, z0)))
                    .unwrap_or_else(|_| Err("Radelta compression worker panicked".into()));
            let _ = send.send(result);
        });
        self.pending.push_back(receive);
        if let Some(chunk) = ready {
            self.write_encoded(chunk)?;
        }
        Ok(())
    }

    fn write_encoded(&mut self, mut chunk: EncodedChunk) -> std::result::Result<(), String> {
        write_u32(&mut self.out, chunk.t as u32).map_err(|e| e.to_string())?;
        write_u32(&mut self.out, chunk.c as u32).map_err(|e| e.to_string())?;
        write_u32(&mut self.out, chunk.z0 as u32).map_err(|e| e.to_string())?;
        write_u32(&mut self.out, chunk.depth as u32).map_err(|e| e.to_string())?;
        write_u64(&mut self.out, chunk.payload.len() as u64).map_err(|e| e.to_string())?;
        self.out
            .write_all(&chunk.payload)
            .map_err(|e| e.to_string())?;
        self.written_chunks += 1;
        self.payload_bytes += chunk.payload.len();
        self.raw_bytes += chunk.pixels.len() * 2;
        chunk.pixels.clear();
        if self.chunk_pixels.is_empty() && self.chunk_pixels.capacity() < chunk.pixels.capacity() {
            self.chunk_pixels = chunk.pixels;
        }
        Ok(())
    }

    /// Drain submitted chunks without publishing the file.
    pub fn flush_pending(&mut self) -> std::result::Result<(), String> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        while let Some(receiver) = self.pending.pop_front() {
            if let Err(e) = wait_chunk(receiver).and_then(|chunk| self.write_encoded(chunk)) {
                self.failed = Some(e.clone());
                return Err(e);
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> std::result::Result<(), String> {
        self.finish_impl(true)
    }

    /// Finish without replacing an existing destination, including one created
    /// while encoding was in progress. Other writer APIs retain replacement.
    pub fn finish_no_replace(&mut self) -> std::result::Result<(), String> {
        self.finish_impl(false)
    }

    fn finish_impl(&mut self, replace: bool) -> std::result::Result<(), String> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        if self.finished {
            return Ok(());
        }
        if self.next_t != self.dims.t || self.next_c != 0 || self.next_z != 0 {
            return Err(format!(
                "not all planes were written before finish; next expected plane would be ({},{},{})",
                self.next_t, self.next_c, self.next_z
            ));
        }
        if let Err(e) = self.flush_chunk() {
            self.failed = Some(e.clone());
            return Err(e);
        }
        self.flush_pending()?;
        if self.written_chunks != self.expected_chunks {
            return Err(format!(
                "streaming writer emitted {} chunks, but {} were expected",
                self.written_chunks, self.expected_chunks
            ));
        }
        let result = crate::metadata::finish_new_file_metadata(&mut self.out, &self.metadata)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                if replace {
                    self.pending_output.publish()
                } else {
                    self.pending_output.publish_no_replace()
                }
                .map_err(|e| e.to_string())
            });
        if let Err(e) = result {
            self.failed = Some(e.clone());
            return Err(e);
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
    match std::panic::catch_unwind(|| open_impl(path)) {
        Ok(Ok(handle)) => {
            *out_handle = Box::into_raw(Box::new(handle));
            RADELTA_OK
        }
        Ok(Err(e)) => {
            set_last_error(e);
            RADELTA_IO_ERROR
        }
        Err(_) => {
            set_last_error("unexpected panic while opening Radelta file");
            RADELTA_CODEC_ERROR
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

/// Set the decoded retention budget. Zero selects the original single-entry cache.
///
/// # Safety
/// `handle` must be a live file handle returned by `radelta_file_open`.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_set_cache_limit(
    handle: *const RadeltaFileHandle,
    max_bytes: u64,
) -> i32 {
    if handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let Ok(bytes) = usize::try_from(max_bytes) else {
        return RADELTA_INVALID_ARGUMENT;
    };
    match (&*handle).set_cache_limit(bytes) {
        Ok(()) => RADELTA_OK,
        Err(e) => {
            set_last_error(e);
            RADELTA_CODEC_ERROR
        }
    }
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

/// Read TCZ triples into consecutive output planes, in request order.
/// Invalid coordinates/capacity are rejected before writing output. Decode/I/O
/// errors may leave partially written output. An empty request is a no-op.
///
/// # Safety
/// `handle` must be live. For nonzero `count`, `tcz` references `3 * count`
/// readable `u32` values and `output` at least `output_capacity_voxels` writable
/// `u16` values. These buffers must not overlap each other or the handle.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_read_planes_u16(
    handle: *const RadeltaFileHandle,
    count: u64,
    tcz: *const u32,
    output: *mut u16,
    output_capacity_voxels: u64,
) -> i32 {
    if handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    if count == 0 {
        return RADELTA_OK;
    }
    let h = &*handle;
    let sizes = usize::try_from(count).ok().and_then(|count| {
        let coords = count.checked_mul(3)?;
        let voxels = h.dims.x.checked_mul(h.dims.y)?.checked_mul(count)?;
        (coords <= isize::MAX as usize / std::mem::size_of::<u32>()
            && voxels <= isize::MAX as usize / std::mem::size_of::<u16>())
        .then_some((count, coords, voxels))
    });
    let Some((count, coords, voxels)) = sizes else {
        set_last_error("batch size exceeds platform limits");
        return RADELTA_INVALID_ARGUMENT;
    };
    if tcz.is_null() || output.is_null() || output_capacity_voxels < voxels as u64 {
        set_last_error("invalid batch buffers or output capacity");
        return RADELTA_INVALID_ARGUMENT;
    }
    let mut planes = Vec::new();
    if planes.try_reserve_exact(count).is_err() {
        set_last_error("cannot allocate plane coordinates");
        return RADELTA_CODEC_ERROR;
    }
    for &[t, c, z] in std::slice::from_raw_parts(tcz, coords).as_chunks::<3>().0 {
        if t as usize >= h.dims.t || c as usize >= h.dims.c || z as usize >= h.dims.z {
            set_last_error("requested T/C/Z plane is out of range");
            return RADELTA_INVALID_ARGUMENT;
        }
        planes.push((t as usize, c as usize, z as usize));
    }
    let out = std::slice::from_raw_parts_mut(output, voxels);
    match h.read_planes(&planes, out) {
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
        memory_mib as usize,
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
    match RadeltaStreamWriter::new(path, dims, WriterMode::Lossy(opts), memory_mib as usize) {
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
    #[test]
    fn decoded_cache_adapts_to_sequential_and_interleaved_plane_orders() {
        let mut cache = super::PixelCache::new(1024);
        cache.note_read(0, 0);
        cache.prepare_read(4);
        cache.insert(0, vec![1, 2]);
        cache.note_read(0, 31);
        cache.note_read(1, 0);
        cache.prepare_read(4);
        cache.insert(1, vec![3, 4]);
        assert_eq!(cache.entries.len(), 1);
        assert!(!cache.reuse_detected);
        // Revisiting an evicted channel at another Z enables reuse too.
        cache.note_read(0, 1);
        cache.prepare_read(4);
        cache.insert(0, vec![1, 2]);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.reuse_detected);

        let mut cache = super::PixelCache::new(1024);
        cache.note_read(0, 0);
        cache.prepare_read(4);
        cache.insert(0, vec![1, 2]);
        cache.note_read(1, 0);
        cache.prepare_read(4);
        cache.insert(1, vec![3, 4]);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.reuse_detected);
    }
    #[test]
    fn decoded_cache_evicts_least_recently_used_data_within_budget() {
        let mut cache = super::PixelCache::new(8);
        cache.insert(1, vec![11, 12]);
        cache.insert(2, vec![21, 22]);
        assert_eq!(cache.get(1).unwrap(), &[11, 12]);
        cache.prepare(4);
        cache.insert(3, vec![31, 32]);
        assert!(cache.contains(1));
        assert!(!cache.contains(2));
        assert_eq!(cache.get(3).unwrap(), &[31, 32]);
        assert_eq!(cache.bytes, 8);
        cache.set_limit(0);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.contains(3));
        cache.insert(4, vec![41, 42]);
        assert!(!cache.contains(3));
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn decoded_cache_accounts_for_capacity_and_allows_one_oversized_entry() {
        let mut cache = super::PixelCache::new(8);
        cache.insert(1, vec![1, 2]);
        let mut pixels = Vec::with_capacity(16);
        pixels.push(99);
        let bytes = pixels.capacity() * 2;
        cache.prepare(bytes);
        assert!(cache.entries.is_empty());
        cache.insert(2, pixels);
        assert_eq!(cache.bytes, bytes);
        assert_eq!(cache.get(2).unwrap(), &[99]);
        cache.set_limit(0);
        assert_eq!(cache.entries.len(), 1);
        cache.insert(3, vec![3]);
        assert_eq!(cache.bytes, 2);
        assert!(!cache.contains(2));
    }

    #[test]
    fn decoded_cache_bounds_entry_overhead_even_with_a_large_byte_budget() {
        let mut cache = super::PixelCache::new(usize::MAX);
        for key in 0..=super::MAX_CACHE_ENTRIES {
            cache.insert(key, vec![key as u16]);
        }
        assert_eq!(cache.entries.len(), super::MAX_CACHE_ENTRIES);
        assert!(!cache.contains(0));
        assert_eq!(cache.bytes, super::MAX_CACHE_ENTRIES * 2);
    }

    use super::*;

    #[test]
    fn unfinished_writers_preserve_existing_destinations_and_remove_temporaries() {
        let path = temp_path("abort-writer");
        for existing in [false, true] {
            if existing {
                fs::write(&path, b"previous image").unwrap();
            }
            let temporary;
            {
                let mut writer = RadeltaStreamWriter::new(
                    path.clone(),
                    Dims5 {
                        x: 2,
                        y: 2,
                        z: 2,
                        c: 1,
                        t: 1,
                    },
                    WriterMode::Lossless(Options::default()),
                    1,
                )
                .unwrap();
                temporary = writer.pending_output.path().to_path_buf();
                writer.write_plane(0, 0, 0, &[1; 4]).unwrap();
                assert!(writer.finish().is_err());
                if existing {
                    assert_eq!(fs::read(&path).unwrap(), b"previous image");
                } else {
                    assert!(!path.exists());
                }
            }
            assert!(!temporary.exists());
            if existing {
                assert_eq!(fs::read(&path).unwrap(), b"previous image");
                fs::remove_file(&path).unwrap();
            }
        }
        fs::write(&path, b"previous image").unwrap();
        {
            let mut writer = RadeltaStreamWriter::new(
                path.clone(),
                Dims5 {
                    x: 2,
                    y: 2,
                    z: 1,
                    c: 1,
                    t: 1,
                },
                WriterMode::Lossless(Options::default()),
                1,
            )
            .unwrap();
            writer.write_plane(0, 0, 0, &[42; 4]).unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"previous image");
            writer.finish().unwrap();
            writer.finish().unwrap();
        }
        let reader = open_impl(path.clone()).unwrap();
        let mut pixels = [0; 4];
        reader.read_plane(0, 0, 0, &mut pixels).unwrap();
        assert_eq!(pixels, [42; 4]);
        drop(reader);
        fs::remove_file(path).unwrap();
    }
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
    fn streaming_finish_no_replace_preserves_a_competing_output() {
        let path = temp_path("stream-no-replace");
        let dims = Dims5 {
            x: 2,
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
            writer.write_plane(0, 0, 0, &[1, 2, 3, 4]).unwrap();
            fs::write(&path, b"competing output").unwrap();
            let error = writer.finish_no_replace().unwrap_err();
            assert!(error.contains("without replacing"), "{error}");
            assert_eq!(writer.finish_no_replace().unwrap_err(), error);
            assert_eq!(fs::read(&path).unwrap(), b"competing output");
        }
        fs::remove_file(path).unwrap();
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
    #[test]
    fn batch_reads_match_all_container_variants_and_cache_budgets() {
        let d = Dims5 {
            x: 73,
            y: 61,
            z: 9,
            c: 2,
            t: 2,
        };
        let plane = d.x * d.y;
        let original: Vec<u16> = (0..d.voxels().unwrap())
            .map(|i| ((i * 17 + i / plane * 37) % 50000) as u16)
            .collect();
        let path = temp_path("batch-containers");
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        for lossy in [false, true] {
            for kind in 0..3 {
                let dims = if kind == 0 {
                    Dims5 { c: 1, t: 1, ..d }
                } else {
                    d
                };
                if kind == 2 {
                    let mode = if lossy {
                        WriterMode::Lossy(LossyOptions::default())
                    } else {
                        WriterMode::Lossless(Options::default())
                    };
                    let mut writer = RadeltaStreamWriter::new(path.clone(), dims, mode, 1).unwrap();
                    for (i, pixels) in original.chunks_exact(plane).enumerate() {
                        writer
                            .write_plane(i / (d.c * d.z), i / d.z % d.c, i % d.z, pixels)
                            .unwrap();
                    }
                    writer.finish().unwrap();
                    let handle = RadeltaFileHandle::open(&path).unwrap();
                    assert!(
                        matches!(&*handle.backing.lock().unwrap(), FileBacking::Stream { chunks_per_volume, .. } if *chunks_per_volume > 1)
                    );
                } else {
                    let input = &original[..dims.voxels().unwrap()];
                    let encoded = match (kind, lossy) {
                        (0, false) => {
                            crate::compress_u16(input, dims.volume_dims(), Options::default())
                        }
                        (0, true) => crate::compress_lossy_u16(
                            input,
                            dims.volume_dims(),
                            LossyOptions::default(),
                        ),
                        (_, false) => crate::compress_u16_nd(input, dims, Options::default()),
                        (_, true) => {
                            crate::compress_lossy_u16_nd(input, dims, LossyOptions::default())
                        }
                    }
                    .unwrap();
                    fs::write(&path, encoded).unwrap();
                }
                // Establish the reconstruction independently through single reads,
                // including lossy quantization, then reverse/repeat ImageJ order.
                let single = RadeltaFileHandle::open(&path).unwrap();
                let coords: Vec<_> = (0..dims.t)
                    .flat_map(|t| {
                        (0..dims.z).flat_map(move |z| (0..dims.c).map(move |c| (t, c, z)))
                    })
                    .collect();
                let mut expected = vec![0; coords.len() * plane];
                for (&(t, c, z), dst) in coords.iter().zip(expected.chunks_exact_mut(plane)) {
                    single.read_plane(t, c, z, dst).unwrap();
                }
                for index in [0, coords.len() - 1] {
                    let (t, c, z) = coords[index];
                    let info = single.decode_unit_info(t, c, z).unwrap();
                    let unit = single.decode_unit(t, c, z).unwrap();
                    assert_eq!(unit.len(), info.depth * plane);
                    let offset = (z - info.z0) * plane;
                    assert_eq!(
                        &unit[offset..offset + plane],
                        &expected[index * plane..(index + 1) * plane]
                    );
                }
                assert!(single.decode_unit_info(dims.t, 0, 0).is_err());
                assert!(single.decode_unit(0, dims.c, 0).is_err());
                drop(single);
                for budget in [0, 1, DEFAULT_FILE_CACHE_BYTES] {
                    let reader = RadeltaFileHandle::open(&path).unwrap();
                    reader.set_cache_limit(budget).unwrap();
                    let request: Vec<_> = coords
                        .iter()
                        .copied()
                        .rev()
                        .chain(coords.iter().copied())
                        .collect();
                    for batch in [1, 3, 11, request.len()] {
                        let mut done = 0;
                        for group in request.chunks(batch) {
                            let mut out = vec![65535; group.len() * plane + 1];
                            pool.install(|| reader.read_planes(group, &mut out))
                                .unwrap();
                            for (i, &(t, c, z)) in group.iter().enumerate() {
                                let index = (t * dims.z + z) * dims.c + c;
                                assert_eq!(
                                    &out[i * plane..(i + 1) * plane],
                                    &expected[index * plane..(index + 1) * plane],
                                    "kind={kind}, lossy={lossy}, budget={budget}, index={}",
                                    done + i
                                );
                            }
                            assert_eq!(out[group.len() * plane], 65535, "tail overwritten");
                            done += group.len();
                        }
                    }
                    let mut sentinel = vec![17; 2 * plane];
                    assert!(reader
                        .read_planes(&[(0, 0, 0), (0, 0, dims.z)], &mut sentinel)
                        .is_err());
                    assert!(sentinel.iter().all(|&p| p == 17));
                    assert!(reader
                        .read_planes(&[(0, 0, 0), (0, 0, 0)], &mut sentinel[..plane])
                        .is_err());
                    reader.read_planes(&[], &mut []).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn batch_c_api_rejects_invalid_sizes_and_coordinates_without_writing() {
        let path = temp_path("batch-ffi");
        fs::write(
            &path,
            crate::compress_u16(&[3, 4], Dims { x: 2, y: 1, z: 1 }, Options::default()).unwrap(),
        )
        .unwrap();
        let handle = RadeltaFileHandle::open(&path).unwrap();
        let coords = [0, 0, 0, 0, 0, 1];
        let mut output = [19u16; 4];
        unsafe {
            assert_eq!(
                radelta_file_read_planes_u16(&handle, 0, ptr::null(), ptr::null_mut(), 0),
                RADELTA_OK
            );
            for (count, input, capacity) in [
                (u64::MAX, coords.as_ptr(), u64::MAX),
                (2, coords.as_ptr(), 3),
                (2, coords.as_ptr(), 4),
                (1, ptr::null(), 4),
            ] {
                assert_eq!(
                    radelta_file_read_planes_u16(
                        &handle,
                        count,
                        input,
                        output.as_mut_ptr(),
                        capacity
                    ),
                    RADELTA_INVALID_ARGUMENT
                );
                assert_eq!(output, [19; 4]);
            }
            assert_eq!(
                radelta_file_read_planes_u16(&handle, 1, coords.as_ptr(), output.as_mut_ptr(), 4),
                RADELTA_OK
            );
            assert_eq!(output, [3, 4, 19, 19]);
        }
        drop(handle);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_cold_plane_and_batch_reads_finish_in_small_pools() {
        // Run behind a timeout so a lock/pool regression fails instead of
        // hanging the entire test suite. The worker owns all borrowed data.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let d = Dims5 {
                x: 32,
                y: 17,
                z: 7,
                c: 3,
                t: 2,
            };
            let plane = d.x * d.y;
            let data: Vec<_> = (0..d.voxels().unwrap())
                .map(|i| ((i * 31) % 50000) as u16)
                .collect();
            // Multiple codec blocks are needed to exercise nested Rayon work.
            let options = Options {
                block_depth: 1,
                ..Options::default()
            };
            let lossy_options = LossyOptions {
                block_depth: 1,
                ..LossyOptions::default()
            };
            for lossy in [false, true] {
                for kind in 0..3 {
                    let dims = if kind == 0 {
                        Dims5 { c: 1, t: 1, ..d }
                    } else {
                        d
                    };
                    let path = temp_path("concurrent-small-pool");
                    let input = &data[..dims.voxels().unwrap()];
                    if kind == 2 {
                        let mode = if lossy {
                            WriterMode::Lossy(lossy_options)
                        } else {
                            WriterMode::Lossless(options)
                        };
                        let mut writer =
                            RadeltaStreamWriter::new(path.clone(), dims, mode, 1).unwrap();
                        for (i, pixels) in input.chunks_exact(plane).enumerate() {
                            writer
                                .write_plane(
                                    i / (dims.c * dims.z),
                                    i / dims.z % dims.c,
                                    i % dims.z,
                                    pixels,
                                )
                                .unwrap();
                        }
                        writer.finish().unwrap();
                    } else {
                        let encoded = match (kind, lossy) {
                            (0, false) => crate::compress_u16(input, dims.volume_dims(), options),
                            (0, true) => {
                                crate::compress_lossy_u16(input, dims.volume_dims(), lossy_options)
                            }
                            (_, false) => crate::compress_u16_nd(input, dims, options),
                            (_, true) => crate::compress_lossy_u16_nd(input, dims, lossy_options),
                        }
                        .unwrap();
                        fs::write(&path, encoded).unwrap();
                    }
                    let coords = [(0, 0, 0), (dims.t - 1, dims.c - 1, dims.z - 1), (0, 0, 0)];
                    let reference = RadeltaFileHandle::open(&path).unwrap();
                    let mut expected = vec![0; coords.len() * plane];
                    for (&(t, c, z), dst) in coords.iter().zip(expected.chunks_exact_mut(plane)) {
                        reference.read_plane(t, c, z, dst).unwrap();
                    }
                    drop(reference);
                    for threads in [1, 2] {
                        let pool = rayon::ThreadPoolBuilder::new()
                            .num_threads(threads)
                            .build()
                            .unwrap();
                        // Fresh handles ensure cache misses even for single-volume files.
                        let readers: Vec<_> = (0..16)
                            .map(|_| {
                                let reader = RadeltaFileHandle::open(&path).unwrap();
                                reader.set_cache_limit(0).unwrap();
                                reader
                            })
                            .collect();
                        let start = std::sync::Barrier::new(4);
                        std::thread::scope(|scope| {
                            for index in 0..4 {
                                let (pool, readers, start, expected) =
                                    (&pool, &readers, &start, &expected);
                                scope.spawn(move || {
                                    for reader in readers {
                                        start.wait();
                                        let mut out = vec![65535; expected.len() + 1];
                                        pool.install(|| {
                                            if index == 0 {
                                                for (&(t, c, z), dst) in coords.iter().zip(
                                                    out[..expected.len()].chunks_exact_mut(plane),
                                                ) {
                                                    reader.read_plane(t, c, z, dst).unwrap();
                                                }
                                            } else {
                                                reader.read_planes(&coords, &mut out).unwrap();
                                            }
                                        });
                                        assert_eq!(
                                            &out[..expected.len()],
                                            expected.as_slice(),
                                            "kind={kind}, lossy={lossy}, threads={threads}"
                                        );
                                        assert_eq!(out[expected.len()], 65535, "tail overwritten");
                                    }
                                });
                            }
                        });
                    }
                    fs::remove_file(path).unwrap();
                }
            }
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("concurrent single/batch reads failed or deadlocked");
        worker.join().unwrap();
    }

    #[test]
    fn concurrent_batch_and_single_reads_keep_one_cached_copy() {
        let d = Dims5 {
            x: 32,
            y: 17,
            z: 7,
            c: 3,
            t: 2,
        };
        let data: Vec<_> = (0..d.voxels().unwrap()).map(|i| (i * 31) as u16).collect();
        let path = temp_path("batch-concurrent");
        fs::write(
            &path,
            crate::compress_u16_nd(&data, d, Options::default()).unwrap(),
        )
        .unwrap();
        let reader = RadeltaFileHandle::open(&path).unwrap();
        let plane = d.x * d.y;
        std::thread::scope(|s| {
            for worker in 0..4 {
                let (reader, data) = (&reader, &data);
                s.spawn(move || {
                    for i in 0..12 {
                        let c = (i + worker) % d.c;
                        let coords = [(i % d.t, c, i % d.z), (0, c, 0), (i % d.t, c, i % d.z)];
                        let mut out = vec![0; 3 * plane];
                        if worker == 0 {
                            for (&(t, c, z), dst) in coords.iter().zip(out.chunks_exact_mut(plane))
                            {
                                reader.read_plane(t, c, z, dst).unwrap();
                            }
                        } else {
                            reader.read_planes(&coords, &mut out).unwrap();
                        }
                        for (j, &(t, c, z)) in coords.iter().enumerate() {
                            let start = ((t * d.c + c) * d.z + z) * plane;
                            assert_eq!(
                                &out[j * plane..(j + 1) * plane],
                                &data[start..start + plane]
                            );
                        }
                    }
                });
            }
        });
        let backing = reader.backing.lock().unwrap();
        let FileBacking::Nd { cache, .. } = &*backing else {
            panic!()
        };
        let keys: std::collections::HashSet<_> = cache.entries.iter().map(|e| e.key).collect();
        assert_eq!(keys.len(), cache.entries.len());
        assert_eq!(
            cache.bytes,
            cache.entries.iter().map(|e| e.pixels.capacity() * 2).sum()
        );
        drop(backing);
        drop(reader);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn failed_batch_decode_does_not_poison_other_reads() {
        let d = Dims5 {
            x: 7,
            y: 5,
            z: 2,
            c: 2,
            t: 1,
        };
        let data: Vec<_> = (0..d.voxels().unwrap()).map(|i| (i * 23) as u16).collect();
        let path = temp_path("batch-damaged");
        fs::write(
            &path,
            crate::compress_u16_nd(&data, d, Options::default()).unwrap(),
        )
        .unwrap();
        let reader = RadeltaFileHandle::open(&path).unwrap();
        let segment = {
            let backing = reader.backing.lock().unwrap();
            let FileBacking::Nd { streams, .. } = &*backing else {
                panic!()
            };
            streams[1]
        };
        drop(reader);
        let mut bytes = fs::read(&path).unwrap();
        bytes[(segment.offset + segment.len - 1) as usize] ^= 1;
        fs::write(&path, bytes).unwrap();
        let reader = RadeltaFileHandle::open(&path).unwrap();
        let mut out = vec![0; d.x * d.y * 2];
        assert!(reader
            .read_planes(&[(0, 0, 0), (0, 1, 0)], &mut out)
            .is_err());
        reader.read_plane(0, 0, 1, &mut out).unwrap();
        assert_eq!(&out[..35], &data[35..70]);
        drop(reader);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn pipeline_plan_accounts_for_all_slots_and_falls_back_for_tiny_budgets() {
        for d in [
            Dims5 {
                x: 7,
                y: 5,
                z: 9,
                c: 3,
                t: 2,
            },
            Dims5 {
                x: 1024,
                y: 1024,
                z: 44,
                c: 1,
                t: 1,
            },
            Dims5 {
                x: 32768,
                y: 32768,
                z: 16,
                c: 2,
                t: 1,
            },
        ] {
            for memory in [1usize, 8, 64, 2048] {
                for threads in [1, 4, 32] {
                    let (depth, pending) = stream_pipeline_plan(d, memory, 4, threads).unwrap();
                    assert!((1..=d.z).contains(&depth));
                    if pending > 0 {
                        let reserved =
                            (d.x as u64) * (d.y as u64) * 2 * (depth as u64) * 4 * pending as u64;
                        assert!(reserved <= (memory as u64) * 1024 * 1024);
                        assert!(pending <= 8 && pending <= threads);
                        assert!(depth <= 4 * threads);
                    } else {
                        assert_eq!(depth, choose_stream_chunk_depth(d, memory, 4).unwrap());
                    }
                }
            }
        }
    }

    #[test]
    fn pipeline_preserves_serial_chunk_bytes_in_single_thread_rayon_pool() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        pool.install(|| {
            let d = Dims5 {
                x: 17,
                y: 11,
                z: 9,
                c: 2,
                t: 2,
            };
            let plane = d.x * d.y;
            let data: Vec<_> = (0..d.voxels().unwrap())
                .map(|i| ((i * 17 + i / plane * 33) % 6000) as u16)
                .collect();
            for mode in [
                WriterMode::Lossless(Options::default()),
                WriterMode::Lossy(LossyOptions::default()),
            ] {
                let path = temp_path("pipeline-one-worker");
                let mut writer = RadeltaStreamWriter::new(path.clone(), d, mode, 64).unwrap();
                assert_eq!(writer.pending_limit, 1);
                let depth = writer.chunk_z;
                let count = (d.t * d.c * d.z.div_ceil(depth)) as u64;
                let mut reference = Vec::new();
                stream_header(
                    &mut reference,
                    match mode {
                        WriterMode::Lossless(_) => STREAM_MAGIC_LOSSLESS,
                        WriterMode::Lossy(_) => STREAM_MAGIC_LOSSY,
                    },
                    d,
                    depth,
                    count,
                )
                .unwrap();
                for t in 0..d.t {
                    for c in 0..d.c {
                        for z0 in (0..d.z).step_by(depth) {
                            let n = depth.min(d.z - z0);
                            let start = ((t * d.c + c) * d.z + z0) * plane;
                            let dims = Dims {
                                x: d.x,
                                y: d.y,
                                z: n,
                            };
                            let payload = match mode {
                                WriterMode::Lossless(o) => {
                                    compress_u16(&data[start..start + n * plane], dims, o)
                                }
                                WriterMode::Lossy(o) => {
                                    compress_lossy_u16(&data[start..start + n * plane], dims, o)
                                }
                            }
                            .unwrap();
                            for value in [t, c, z0, n] {
                                write_u32(&mut reference, value as u32).unwrap();
                            }
                            write_u64(&mut reference, payload.len() as u64).unwrap();
                            reference.extend_from_slice(&payload);
                        }
                        for z in 0..d.z {
                            let start = ((t * d.c + c) * d.z + z) * plane;
                            writer
                                .write_plane(t, c, z, &data[start..start + plane])
                                .unwrap();
                            if z == 1 {
                                writer.flush_pending().unwrap();
                            } // does not discard a partial feeder chunk
                        }
                    }
                }
                writer.finish().unwrap();
                writer.finish().unwrap();
                assert_eq!(fs::read(&path).unwrap(), reference);
                drop(writer);
                fs::remove_file(path).unwrap();
            }
        });
    }

    #[test]
    fn asynchronous_codec_errors_stay_failed_and_never_publish() {
        let path = temp_path("pipeline-codec-error");
        fs::write(&path, b"existing destination").unwrap();
        let d = Dims5 {
            x: 7,
            y: 5,
            z: 1,
            c: 1,
            t: 2,
        };
        let options = Options {
            context_mode: 99,
            ..Options::default()
        };
        let mut writer = RadeltaStreamWriter::create_lossless(&path, d, options, 64).unwrap();
        assert!(writer.pending_limit > 0);
        let temporary = writer.pending_output.path().to_owned();
        let result = writer
            .write_plane(0, 0, 0, &[3; 35])
            .and_then(|()| writer.write_plane(1, 0, 0, &[3; 35]))
            .and_then(|()| writer.finish());
        assert!(result.is_err());
        assert!(writer.finish().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing destination");
        drop(writer);
        assert!(!temporary.exists());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dropping_a_writer_waits_for_queued_chunks_and_removes_output() {
        let path = temp_path("pipeline-drop");
        let d = Dims5 {
            x: 128,
            y: 64,
            z: 1,
            c: 1,
            t: 16,
        };
        let mut writer =
            RadeltaStreamWriter::create_lossless(&path, d, Options::default(), 64).unwrap();
        assert!(writer.pending_limit > 0);
        let temporary = writer.pending_output.path().to_owned();
        for t in 0..3 {
            writer.write_plane(t, 0, 0, &vec![123; d.x * d.y]).unwrap();
        }
        drop(writer);
        assert!(!path.exists());
        assert!(!temporary.exists());
    }
    #[test]
    fn sequential_batch_cache_keeps_the_trailing_requested_unit() {
        let d = Dims5 {
            x: 17,
            y: 11,
            z: 3,
            c: 2,
            t: 1,
        };
        let pixels = vec![123; d.voxels().unwrap()];
        let path = temp_path("pipeline-cache-order");
        fs::write(
            &path,
            crate::compress_u16_nd(&pixels, d, Options::default()).unwrap(),
        )
        .unwrap();
        let reader = RadeltaFileHandle::open(&path).unwrap();
        let mut output = vec![0; d.x * d.y * 3];
        reader
            .read_planes(&[(0, 0, 0), (0, 0, 1), (0, 1, 0)], &mut output)
            .unwrap();
        let backing = reader.backing.lock().unwrap();
        let FileBacking::Nd { cache, .. } = &*backing else {
            panic!()
        };
        assert!(!cache.reuse_detected);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].key, 1);
        drop(backing);
        drop(reader);
        fs::remove_file(path).unwrap();
    }
}
