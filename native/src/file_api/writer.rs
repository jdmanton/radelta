//! Bounded streaming encoding, ordered chunk writes and output publication.
//!
//! Queued codec jobs are joined before metadata is appended or an unfinished
//! writer is dropped. Completed files are published through PendingOutput.

use crate::{
    choose_stream_chunk_depth, compress_lossy_u16, compress_u16, Dims, Dims5, LossyOptions,
    Options, DEFAULT_BLOCK_DEPTH, DEFAULT_STREAM_MEMORY_MIB, STREAM_MAGIC_LOSSLESS,
    STREAM_MAGIC_LOSSY, STREAM_VERSION,
};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

#[derive(Clone, Copy)]
pub(super) enum WriterMode {
    Lossless(Options),
    Lossy(LossyOptions),
}

/// Bounded streaming encoder shared by the CLI and file-writer C ABI.
/// Queued chunks retain their order; `finish` waits for every compression job
/// before appending metadata and publishing the completed file.
pub struct RadeltaStreamWriter {
    dims: Dims5,
    pub(super) plane_voxels: usize,
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
    pub(super) finished: bool,
    pub(super) metadata: Vec<u8>,
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

/// Divide the approximate working budget between encode slots.
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

fn write_u16<W: Write>(w: &mut W, v: u16) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_u32<W: Write>(w: &mut W, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_u64<W: Write>(w: &mut W, v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
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

    pub(super) fn new(
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

#[cfg(test)]
mod tests {
    use super::super::index::{
        open_impl, RADELTA_FILE_FLAG_MULTIDIMENSIONAL, RADELTA_FILE_FLAG_STREAMING,
        RADELTA_FILE_FORMAT_RDS3, RADELTA_FILE_FORMAT_RQS3,
    };
    use super::super::test_support::{plane_values, temp_path};
    use super::*;
    use crate::{
        choose_stream_chunk_depth, compress_lossy_u16, compress_u16, Dims, Dims5, LossyOptions,
        Options, STREAM_MAGIC_LOSSLESS, STREAM_MAGIC_LOSSY,
    };
    use std::fs;

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
                let q = crate::codec::lossy_quantize(
                    input,
                    opts.offset_adu,
                    opts.gain_e_per_adu,
                    opts.noise_step,
                );
                assert!(q <= u8::MAX as u16);
                let expected = crate::codec::lossy_reconstruct(
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
}
