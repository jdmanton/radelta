//! Prefetched TIFF planes feeding the bounded streaming writer.

use super::layout::DatasetArgs;
use super::reporting::{gb_per_s, mib};
use super::tiff_input::{PlaneReader, StreamingDataset};
use crate::tiff_io::pixel_buffer;
use crate::{prefetch, tiff_metadata, FORCE};
use radelta_native::{LossyOptions, Options};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

pub(super) fn encode_streaming_lossless(
    input: &Path,
    output: &Path,
    opts: Options,
    ds: &DatasetArgs,
    info: &StreamingDataset,
) -> Result<(), String> {
    let writer = radelta_native::RadeltaStreamWriter::create_lossless(
        output,
        info.layout.dims,
        opts,
        ds.memory_mib,
    )?;
    encode_streaming(input, output, ds, info, writer)
}

pub(super) fn encode_streaming_lossy(
    input: &Path,
    output: &Path,
    opts: LossyOptions,
    ds: &DatasetArgs,
    info: &StreamingDataset,
) -> Result<(), String> {
    let writer = radelta_native::RadeltaStreamWriter::create_lossy(
        output,
        info.layout.dims,
        opts,
        ds.memory_mib,
    )?;
    encode_streaming(input, output, ds, info, writer)?;
    report!(
        "offset={:.6} ADU, gain={:.6} e-/ADU, noise-step={:.6}",
        opts.offset_adu,
        opts.gain_e_per_adu,
        opts.noise_step
    );
    Ok(())
}

fn encode_streaming(
    input: &Path,
    output: &Path,
    ds: &DatasetArgs,
    info: &StreamingDataset,
    mut writer: radelta_native::RadeltaStreamWriter,
) -> Result<(), String> {
    let d = info.layout.dims;
    let chunk_z = writer.chunk_depth();
    let chunk_count = (d.t as u64) * (d.c as u64) * d.z.div_ceil(chunk_z) as u64;
    report!("Streaming layout: {} chunk(s), up to {} Z plane(s) per chunk; input has {} physical / {} logical TIFF plane(s).",chunk_count,chunk_z,info.physical_pages,info.logical_pages);
    report!("Reading first chunk...");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut last_progress = Instant::now();
    let mut report = |writer: &radelta_native::RadeltaStreamWriter,
                      force: bool|
     -> Result<(), String> {
        if force || last_progress.elapsed().as_secs_f64() >= 1.0 {
            let elapsed = start.elapsed().as_secs_f64();
            report!("  chunk {}/{} ({:.1}%)  raw {:.3} GiB  elapsed {:.1} s  {:.3} GB/s including TIFF I/O",
                writer.chunks_written(),chunk_count,100.*writer.chunks_written() as f64/chunk_count as f64,
                (writer.raw_bytes_written() / 2 * info.sample_bytes) as f64/(1024.*1024.*1024.),elapsed,gb_per_s(writer.raw_bytes_written() / 2 * info.sample_bytes,elapsed));
            std::io::stdout().flush().map_err(|e| e.to_string())?;
            last_progress = Instant::now();
        }
        Ok(())
    };
    let plane = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
    let total_planes = d.t * d.c * d.z;
    {
        let source = input.to_path_buf();
        let source_info = info.clone();
        let mut reader = None;
        let mut next = 0;
        let mut buffers = vec![(0, pixel_buffer(plane)?)];
        if writer.pending_limit() > 0 {
            buffers.push((0, pixel_buffer(plane)?));
        }
        let mut ahead = prefetch::Prefetch::new(buffers, move |(index, pixels)| {
            if next == total_planes {
                return Ok(false);
            }
            if reader.is_none() {
                reader = Some(PlaneReader::open(&source, &source_info)?);
            }
            reader
                .as_mut()
                .unwrap()
                .read_plane_into(source_info.canonical_to_source[next], pixels)?;
            *index = next;
            next += 1;
            Ok(true)
        })?;
        let mut fed = 0;
        while let Some(buffer) = ahead.next()? {
            let (canonical, pixels) = &buffer;
            if *canonical != fed {
                return Err("TIFF read pipeline returned planes out of order".into());
            }
            writer.write_plane(
                canonical / (d.c * d.z),
                (canonical / d.z) % d.c,
                canonical % d.z,
                pixels,
            )?;
            ahead.recycle(buffer);
            fed += 1;
            report(&writer, false)?;
        }
        if fed != total_planes {
            return Err("TIFF read pipeline ended early".into());
        }
    } // The TIFF reader is joined before metadata capture or publication.
    writer.flush_pending()?;
    report(&writer, true)?;
    report!("Saving TIFF metadata...");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    writer.set_metadata(tiff_metadata::capture(input, &info.layout)?)?;
    if FORCE.load(std::sync::atomic::Ordering::Relaxed) {
        writer.finish()?;
    } else {
        writer.finish_no_replace()?;
    }
    let total = fs::metadata(output).map_err(|e| e.to_string())?.len() as usize;
    let secs = start.elapsed().as_secs_f64();
    report!("Wrote {} ({:.3} MiB)", output.display(), mib(total));
    report!(
        "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
        d.t,
        d.c,
        d.z,
        d.y,
        d.x
    );
    report!(
        "Out-of-core chunks: {} (up to {} Z planes each)",
        writer.chunks_written(),
        chunk_z
    );
    report!("RAM budget:       {} MiB", ds.memory_mib);
    report!("Payload bytes:    {:.3} MiB", mib(writer.payload_bytes()));
    report!(
        "Compression ratio:{:.2}:1",
        (writer.raw_bytes_written() / 2 * info.sample_bytes) as f64 / total.max(1) as f64
    );
    report!(
        "Encode rate:      {:.3} GB/s (TIFF I/O included)",
        gb_per_s(writer.raw_bytes_written() / 2 * info.sample_bytes, secs)
    );
    Ok(())
}
