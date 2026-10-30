//! Single-plane, batch and owned-unit reads.
//!
//! The backing mutex protects file I/O and cache bookkeeping. Release it before
//! entering the codec: Rayon workers must never wait for decode work while
//! holding the same mutex that other read jobs need.

use super::index::{decode_unit, open_impl, DecodeUnit, FileBacking, Segment};
use super::{RadeltaDecodeUnit, RadeltaFileHandle, RadeltaFileInfo};
use crate::codec::decompress_u16_expecting;
use crate::Dims;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

// Additional decoded pixels plus compressed payloads admitted by one batch.
// Codec scratch and the retention cache are separate; one oversized
// unit must always be able to proceed.
const BATCH_WORKING_BYTES: u64 = 256 * 1024 * 1024;

struct PlaneBatch<'a> {
    unit: DecodeUnit,
    targets: Vec<(usize, &'a mut [u16])>,
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

    /// Set the decoded volume/chunk retention budget in bytes. Zero selects
    /// a single-entry cache. This does not cap decoder working memory:
    /// one entry larger than the retention budget is permitted.
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
    /// budget is separate. On a decode/I/O error, output may be partly written.
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

#[cfg(test)]
mod tests {
    use super::super::cache::DEFAULT_FILE_CACHE_BYTES;
    use super::super::index::{open_impl, FileBacking};
    use super::super::test_support::temp_path;
    use super::super::writer::{RadeltaStreamWriter, WriterMode};
    use super::super::RadeltaFileHandle;
    use crate::{Dims5, LossyOptions, Options};
    use std::fs;

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
    fn concurrent_cold_plane_and_batch_reads_finish_in_small_pools() {
        // Run behind a timeout so a lock/pool deadlock fails instead of
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
}
