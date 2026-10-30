//! TIFF inspection, plane readers and whole-dataset loading.

use super::layout::{
    imagej_value, infer_dataset_layout, page_index, reorder_pages_to_tczyx, resolve_layout,
    DatasetArgs, ResolvedLayout,
};
use crate::{tiff_io, QUIET};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;
use std::time::Instant;
use tiff::decoder::Decoder;
use tiff::tags::Tag;

fn read_imagej_contiguous_u16(
    dec: &mut Decoder<BufReader<File>>,
    width: usize,
    height: usize,
    logical_pages: usize,
) -> Result<Vec<u16>, String> {
    // ImageJ/tifffile hyperstacks may store all pixels contiguously after the
    // first IFD and omit IFDs for the remaining logical planes.  This reader
    // intentionally supports only the safe/simple case used for such stacks:
    // uncompressed, single-sample, 8-bit or 16-bit grayscale data whose strips for the
    // first plane are themselves contiguous.
    let compression = dec.get_tag_u64(Tag::Compression).unwrap_or(1);
    if compression != 1 {
        return Err(format!(
            "ImageJ metadata describes {logical_pages} logical planes but the TIFF has one IFD; contiguous ImageJ stacks are currently supported only when uncompressed (Compression=1), found {compression}"
        ));
    }

    let bits = dec
        .get_tag_u16_vec(Tag::BitsPerSample)
        .map_err(|e| e.to_string())?;
    if !matches!(bits.as_slice(), [8] | [16]) {
        return Err(format!(
            "contiguous ImageJ stack must be 8-bit or 16-bit grayscale; BitsPerSample={bits:?}"
        ));
    }
    let samples = dec.get_tag_u64(Tag::SamplesPerPixel).unwrap_or(1);
    if samples != 1 {
        return Err(format!(
            "contiguous ImageJ stack must have SamplesPerPixel=1, found {samples}"
        ));
    }
    if let Ok(sample_format) = dec.get_tag_u16_vec(Tag::SampleFormat) {
        if sample_format.iter().any(|&v| v != 1) {
            return Err(format!(
                "contiguous ImageJ stack must contain unsigned integer samples; SampleFormat={sample_format:?}"
            ));
        }
    }

    let offsets = dec
        .get_tag_u64_vec(Tag::StripOffsets)
        .map_err(|e| e.to_string())?;
    let counts = dec
        .get_tag_u64_vec(Tag::StripByteCounts)
        .map_err(|e| e.to_string())?;
    if offsets.is_empty() || offsets.len() != counts.len() {
        return Err("invalid StripOffsets/StripByteCounts in contiguous ImageJ TIFF".into());
    }

    let sample_bytes = tiff_io::sample_bytes(dec)?;
    let plane_samples = width
        .checked_mul(height)
        .ok_or("TIFF plane size overflow")?;
    let plane_bytes = plane_samples
        .checked_mul(sample_bytes)
        .ok_or("TIFF plane byte size overflow")?;
    let first_plane_bytes: u64 = counts
        .iter()
        .try_fold(0u64, |a, &b| a.checked_add(b).ok_or(()))
        .map_err(|_| "TIFF strip-byte-count overflow")?;
    if first_plane_bytes != plane_bytes as u64 {
        return Err(format!(
            "contiguous ImageJ TIFF first IFD describes {first_plane_bytes} bytes of pixel data, expected {plane_bytes} for one {width}x{height} plane"
        ));
    }
    for i in 1..offsets.len() {
        let expected = offsets[i - 1]
            .checked_add(counts[i - 1])
            .ok_or("TIFF strip-offset overflow")?;
        if offsets[i] != expected {
            return Err(
                "contiguous ImageJ TIFF has non-contiguous strips in the first plane; this layout is not yet supported"
                    .into(),
            );
        }
    }

    let total_samples = plane_samples
        .checked_mul(logical_pages)
        .ok_or("ImageJ logical stack size overflow")?;
    let total_bytes = total_samples
        .checked_mul(sample_bytes)
        .ok_or("ImageJ logical stack byte size overflow")?;
    let data_offset = offsets[0];

    let byte_order = dec.byte_order();
    let reader = dec.inner();
    let file_len = reader.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    let end = data_offset
        .checked_add(total_bytes as u64)
        .ok_or("ImageJ contiguous data offset overflow")?;
    if end > file_len {
        return Err(format!(
            "ImageJ metadata requires {logical_pages} contiguous planes ({total_bytes} bytes) starting at file offset {data_offset}, but the file ends at {file_len}"
        ));
    }
    reader
        .seek(SeekFrom::Start(data_offset))
        .map_err(|e| e.to_string())?;

    let mut out = vec![0u16; total_samples];
    tiff_io::read_unsigned(reader, &mut out, sample_bytes, byte_order)?;
    Ok(out)
}

/// Number of samples stored for each pixel of the decoder's current page.
///
/// Planar (`PlanarConfiguration=2`) multi-sample pages are accepted: each
/// sample is a complete XY plane and is read as a separate channel.
/// Interleaved multi-sample pages (RGB and similar) are rejected.
pub(crate) fn page_samples(dec: &mut Decoder<BufReader<File>>) -> Result<usize, String> {
    let samples = dec
        .find_tag_unsigned::<u64>(Tag::SamplesPerPixel)
        .map_err(|e| e.to_string())?
        .unwrap_or(1);
    if samples == 0 {
        return Err("TIFF page has SamplesPerPixel=0".into());
    }
    if samples > 1 {
        let planar = dec
            .find_tag_unsigned::<u64>(Tag::PlanarConfiguration)
            .map_err(|e| e.to_string())?
            .unwrap_or(1);
        if planar != 2 {
            return Err(format!(
                "TIFF pages interleave {samples} samples per pixel (PlanarConfiguration={planar}); interleaved RGB/multisample pages are not supported by the CLI. Planar (separate) multisample pages are read as channels"
            ));
        }
    }
    usize::try_from(samples).map_err(|_| "TIFF SamplesPerPixel overflow".into())
}

/// Read one sample plane of a planar multi-sample TIFF page.
///
/// Each sample of a planar page is stored in its own run of strips or tiles,
/// so only the chunks belonging to `sample` are decoded.
fn read_sample_plane_into(
    dec: &mut Decoder<BufReader<File>>,
    width: usize,
    height: usize,
    sample: usize,
    samples: usize,
    plane: &mut [u16],
) -> Result<(), String> {
    use tiff::decoder::{ChunkType, DecodingSampleType};
    if width.checked_mul(height) != Some(plane.len()) {
        return Err("TIFF plane size mismatch".into());
    }
    if tiff_io::read_raw_sample_plane(dec, plane, sample)? {
        return Ok(());
    }
    if tiff_io::read_tiled_sample(dec, plane, sample)? {
        return Ok(());
    }
    let chunk_type = dec.get_chunk_type();
    let chunks = match chunk_type {
        ChunkType::Strip => dec.strip_count(),
        ChunkType::Tile => dec.tile_count(),
    }
    .map_err(|e| e.to_string())? as usize;
    if chunks == 0 || !chunks.is_multiple_of(samples) || sample >= samples {
        return Err(format!(
            "planar TIFF page has {chunks} strips/tiles, which do not divide into {samples} samples"
        ));
    }
    let per_plane = chunks / samples;
    let (chunk_w, chunk_h) = dec.chunk_dimensions();
    let (chunk_w, chunk_h) = (chunk_w as usize, chunk_h as usize);
    if chunk_w == 0 || chunk_h == 0 {
        return Err("TIFF strip/tile dimensions must be positive".into());
    }
    let across = match chunk_type {
        ChunkType::Strip => 1,
        ChunkType::Tile => width.div_ceil(chunk_w),
    };
    let sample_bytes = tiff_io::sample_bytes(dec)?;
    let mut scratch = Vec::<u16>::new();
    let mut byte_scratch = Vec::<u8>::new();
    for k in 0..per_plane {
        let index = sample
            .checked_mul(per_plane)
            .and_then(|v| v.checked_add(k))
            .and_then(|v| u32::try_from(v).ok())
            .ok_or("TIFF chunk index overflow")?;
        let in_plane = u32::try_from(k).map_err(|_| "TIFF chunk index overflow")?;
        // The geometry of chunk `k` is the same in every sample plane. Take
        // the visible extent from the first plane's chunk and the decoded
        // extent from the chunk actually read, which may include padding.
        let (data_w, data_h) = dec.chunk_data_dimensions(in_plane);
        let (read_w, read_h) = dec.chunk_data_dimensions(index);
        let (data_w, data_h) = (data_w as usize, data_h as usize);
        let (read_w, read_h) = (read_w as usize, read_h as usize);
        let x0 = (k % across) * chunk_w;
        let y0 = (k / across) * chunk_h;
        if read_w < data_w || read_h < data_h || x0 + data_w > width || y0 + data_h > height {
            return Err("planar TIFF strip/tile geometry does not match the page".into());
        }
        let layout = dec
            .image_chunk_buffer_layout(index)
            .map_err(|e| e.to_string())?;
        if !matches!(
            layout.sample_type,
            Some(DecodingSampleType::U8 | DecodingSampleType::U16)
        ) || read_w
            .checked_mul(read_h)
            .and_then(|n| n.checked_mul(sample_bytes))
            != Some(layout.len)
        {
            return Err("expected uint8 or uint16 samples in planar multisample TIFF page".into());
        }
        if sample_bytes == 1 {
            byte_scratch.resize(layout.len, 0);
            dec.read_chunk_bytes(index, &mut byte_scratch)
                .map_err(|e| e.to_string())?;
            for row in 0..data_h {
                let src = row * read_w;
                let dst = (y0 + row) * width + x0;
                for (out, &value) in plane[dst..dst + data_w]
                    .iter_mut()
                    .zip(&byte_scratch[src..src + data_w])
                {
                    *out = u16::from(value);
                }
            }
        } else if x0 == 0 && data_w == width && read_w == width && read_h == data_h {
            let dst = &mut plane[y0 * width..(y0 + data_h) * width];
            dec.read_chunk_bytes(index, tiff_io::u16_bytes_mut(dst))
                .map_err(|e| e.to_string())?;
        } else {
            // Edge tiles can contain padding. Reuse one chunk buffer and copy
            // only its visible rows, rather than allocating for every tile.
            scratch.resize(read_w * read_h, 0);
            dec.read_chunk_bytes(index, tiff_io::u16_bytes_mut(&mut scratch))
                .map_err(|e| e.to_string())?;
            for row in 0..data_h {
                let src = row * read_w;
                let dst = (y0 + row) * width + x0;
                plane[dst..dst + data_w].copy_from_slice(&scratch[src..src + data_w]);
            }
        }
    }
    Ok(())
}

type TiffPages = (Vec<u16>, usize, usize, usize, usize, Option<String>);

fn read_tiff_pages_u16(path: &Path) -> Result<TiffPages, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut dec = Decoder::new(BufReader::new(file))
        .map_err(|e| e.to_string())?
        .with_limits(tiff_limits());
    let mut data = Vec::<u16>::new();
    let mut width = 0usize;
    let mut height = 0usize;
    let mut pages = 0usize;
    let mut samples = 1usize;
    let mut image_description: Option<String> = None;

    loop {
        let (w, h) = dec.dimensions().map_err(|e| e.to_string())?;
        let page_sample_count = page_samples(&mut dec)?;
        if pages == 0 {
            width = w as usize;
            height = h as usize;
            samples = page_sample_count;
            image_description = dec.get_tag_ascii_string(Tag::ImageDescription).ok();

            // Some ImageJ/tifffile hyperstacks deliberately have a single IFD
            // even though ImageDescription reports many logical images.  When
            // that layout is detected, bypass normal IFD iteration and read the
            // contiguous pixel block directly.
            if !dec.more_images() && samples == 1 {
                if let Some(desc) = image_description.as_deref() {
                    let is_imagej = desc
                        .lines()
                        .any(|line| line.trim_start().starts_with("ImageJ="));
                    let logical_pages = imagej_value(desc, "images").unwrap_or(1);
                    if is_imagej && logical_pages > 1 {
                        let v = read_imagej_contiguous_u16(&mut dec, width, height, logical_pages)?;
                        return Ok((v, width, height, logical_pages, 1, image_description));
                    }
                }
            }
        } else if width != w as usize || height != h as usize {
            return Err(
                "all TIFF pages in one dense dataset must have the same X/Y dimensions".into(),
            );
        } else if page_sample_count != samples {
            return Err(format!(
                "all TIFF pages in one dataset must have the same SamplesPerPixel; page 0 has {samples}, page {pages} has {page_sample_count}"
            ));
        }
        if samples > 1 {
            let plane_len = width
                .checked_mul(height)
                .ok_or("TIFF plane size overflow")?;
            for sample in 0..samples {
                let start = data.len();
                data.resize(start.checked_add(plane_len).ok_or("TIFF size overflow")?, 0);
                read_sample_plane_into(
                    &mut dec,
                    width,
                    height,
                    sample,
                    samples,
                    &mut data[start..],
                )?;
            }
            pages += 1;
            if !dec.more_images() {
                break;
            }
            dec.next_image().map_err(|e| e.to_string())?;
            continue;
        }
        let expected = width
            .checked_mul(height)
            .ok_or("TIFF plane size overflow")?;
        let start = data.len();
        data.resize(start.checked_add(expected).ok_or("TIFF size overflow")?, 0);
        tiff_io::read_plane(&mut dec, &mut data[start..])?;
        pages += 1;
        if !dec.more_images() {
            break;
        }
        dec.next_image().map_err(|e| e.to_string())?;
    }

    Ok((data, width, height, pages, samples, image_description))
}

#[derive(Clone)]
pub(super) struct StreamingDataset {
    pub(super) layout: ResolvedLayout,
    pub(super) canonical_to_source: Vec<usize>,
    pub(super) physical_pages: usize,
    pub(super) logical_pages: usize,
    /// Planar samples per TIFF page; logical plane `p` is sample
    /// `p % samples` of page `p / samples`.
    pub(super) samples: usize,
    pub(super) sample_bytes: usize,
    pub(super) description: Option<String>,
}

#[allow(clippy::large_enum_variant)]
pub(super) enum PlaneReader {
    Ifd {
        dec: Decoder<BufReader<File>>,
        width: usize,
        height: usize,
        samples: usize,
    },
    ImageJContiguous {
        reader: BufReader<File>,
        data_offset: u64,
        byte_order: tiff::tags::ByteOrder,
        plane_samples: usize,
        plane_bytes: usize,
        sample_bytes: usize,
        logical_pages: usize,
    },
}

impl PlaneReader {
    pub(super) fn open(path: &Path, info: &StreamingDataset) -> Result<Self, String> {
        let d = info.layout.dims;
        if info.physical_pages == 1 && info.logical_pages > 1 && info.samples == 1 {
            let desc = info.description.as_deref().unwrap_or("");
            let is_imagej = desc
                .lines()
                .any(|line| line.trim_start().starts_with("ImageJ="));
            if is_imagej {
                let file = File::open(path).map_err(|e| e.to_string())?;
                let mut dec = Decoder::new(BufReader::new(file))
                    .map_err(|e| e.to_string())?
                    .with_limits(tiff_limits());
                let compression = dec.get_tag_u64(Tag::Compression).unwrap_or(1);
                if compression != 1 {
                    return Err(format!(
                        "out-of-core contiguous ImageJ stacks currently require uncompressed TIFF data (Compression=1), found {compression}"
                    ));
                }
                let bits = dec
                    .get_tag_u16_vec(Tag::BitsPerSample)
                    .map_err(|e| e.to_string())?;
                if !matches!(bits.as_slice(), [8] | [16]) {
                    return Err(format!(
                        "contiguous ImageJ stack must be uint8 or uint16 grayscale; BitsPerSample={bits:?}"
                    ));
                }
                let samples = dec.get_tag_u64(Tag::SamplesPerPixel).unwrap_or(1);
                if samples != 1 {
                    return Err(format!(
                        "contiguous ImageJ stack must have SamplesPerPixel=1, found {samples}"
                    ));
                }
                let offsets = dec
                    .get_tag_u64_vec(Tag::StripOffsets)
                    .map_err(|e| e.to_string())?;
                let counts = dec
                    .get_tag_u64_vec(Tag::StripByteCounts)
                    .map_err(|e| e.to_string())?;
                if offsets.is_empty() || offsets.len() != counts.len() {
                    return Err(
                        "invalid StripOffsets/StripByteCounts in contiguous ImageJ TIFF".into(),
                    );
                }
                for i in 1..offsets.len() {
                    let expected = offsets[i - 1]
                        .checked_add(counts[i - 1])
                        .ok_or("TIFF strip-offset overflow")?;
                    if offsets[i] != expected {
                        return Err(
                            "contiguous ImageJ TIFF has non-contiguous first-plane strips".into(),
                        );
                    }
                }
                let sample_bytes = tiff_io::sample_bytes(&mut dec)?;
                let plane_samples = d.x.checked_mul(d.y).ok_or("TIFF plane size overflow")?;
                let plane_bytes = plane_samples
                    .checked_mul(sample_bytes)
                    .ok_or("TIFF plane byte size overflow")?;
                let first_bytes: u64 = counts
                    .iter()
                    .try_fold(0u64, |a, &b| a.checked_add(b).ok_or(()))
                    .map_err(|_| "TIFF strip-byte-count overflow")?;
                if first_bytes != plane_bytes as u64 {
                    return Err(format!(
                        "first ImageJ plane occupies {first_bytes} bytes, expected {plane_bytes}"
                    ));
                }
                let data_offset = offsets[0];
                let byte_order = dec.byte_order();
                drop(dec);
                let reader = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
                return Ok(Self::ImageJContiguous {
                    reader,
                    data_offset,
                    byte_order,
                    plane_samples,
                    plane_bytes,
                    sample_bytes,
                    logical_pages: info.logical_pages,
                });
            }
        }

        let file = File::open(path).map_err(|e| e.to_string())?;
        let dec = Decoder::new(BufReader::new(file))
            .map_err(|e| e.to_string())?
            .with_limits(tiff_limits());
        Ok(Self::Ifd {
            dec,
            width: d.x,
            height: d.y,
            samples: info.samples,
        })
    }

    pub(super) fn read_plane_into(
        &mut self,
        source_page: usize,
        out: &mut [u16],
    ) -> Result<(), String> {
        match self {
            Self::Ifd {
                dec,
                width,
                height,
                samples,
            } => {
                dec.seek_to_image(source_page / *samples)
                    .map_err(|e| e.to_string())?;
                let (w, h) = dec.dimensions().map_err(|e| e.to_string())?;
                if w as usize != *width || h as usize != *height {
                    return Err("TIFF plane dimensions changed within dataset".into());
                }
                if *samples > 1 {
                    return read_sample_plane_into(
                        dec,
                        *width,
                        *height,
                        source_page % *samples,
                        *samples,
                        out,
                    );
                }
                tiff_io::read_plane(dec, out)
            }
            Self::ImageJContiguous {
                reader,
                data_offset,
                byte_order,
                plane_samples,
                plane_bytes,
                sample_bytes,
                logical_pages,
            } => {
                if source_page >= *logical_pages {
                    return Err(format!("logical page {source_page} is out of range"));
                }
                let offset = data_offset
                    .checked_add(
                        (source_page as u64)
                            .checked_mul(*plane_bytes as u64)
                            .ok_or("contiguous TIFF page offset overflow")?,
                    )
                    .ok_or("contiguous TIFF offset overflow")?;
                reader
                    .seek(SeekFrom::Start(offset))
                    .map_err(|e| e.to_string())?;
                if out.len() != *plane_samples {
                    return Err("TIFF plane size mismatch".into());
                }
                tiff_io::read_unsigned(reader, out, *sample_bytes, *byte_order)
            }
        }
    }
}

pub(super) fn inspect_streaming_dataset(
    path: &Path,
    ds: &DatasetArgs,
) -> Result<StreamingDataset, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut dec = Decoder::new(BufReader::new(file))
        .map_err(|e| e.to_string())?
        .with_limits(tiff_limits());
    let (w, h) = dec.dimensions().map_err(|e| e.to_string())?;
    let x = w as usize;
    let y = h as usize;
    let description = dec.get_tag_ascii_string(Tag::ImageDescription).ok();
    let samples = page_samples(&mut dec)?;
    let sample_bytes = tiff_io::sample_bytes(&mut dec)?;

    let mut physical_pages = 1usize;
    let scan_start = Instant::now();
    let mut last_scan_report = Instant::now();
    while dec.more_images() {
        dec.next_image().map_err(|e| e.to_string())?;
        let (ww, hh) = dec.dimensions().map_err(|e| e.to_string())?;
        if ww as usize != x || hh as usize != y {
            return Err(
                "all TIFF pages in one dense dataset must have the same X/Y dimensions".into(),
            );
        }
        if tiff_io::sample_bytes(&mut dec)? != sample_bytes {
            return Err("mixed TIFF sample depths in one dense dataset are unsupported".into());
        }
        let page_sample_count = page_samples(&mut dec)?;
        if page_sample_count != samples {
            return Err(format!(
                "all TIFF pages in one dataset must have the same SamplesPerPixel; page 0 has {samples}, page {physical_pages} has {page_sample_count}"
            ));
        }
        physical_pages += 1;
        if !QUIET.load(std::sync::atomic::Ordering::Relaxed)
            && last_scan_report.elapsed().as_secs_f64() >= 2.0
        {
            eprintln!(
                "Scanning TIFF directories: {} physical pages found ({:.1} s elapsed)...",
                physical_pages,
                scan_start.elapsed().as_secs_f64()
            );
            last_scan_report = Instant::now();
        }
    }
    if !QUIET.load(std::sync::atomic::Ordering::Relaxed)
        && scan_start.elapsed().as_secs_f64() >= 2.0
    {
        eprintln!(
            "TIFF directory scan complete: {} physical pages in {:.1} s.",
            physical_pages,
            scan_start.elapsed().as_secs_f64()
        );
    }

    let mut logical_pages = physical_pages
        .checked_mul(samples)
        .ok_or("TIFF plane count overflow")?;
    if physical_pages == 1 && samples == 1 {
        if let Some(desc) = description.as_deref() {
            let is_imagej = desc
                .lines()
                .any(|line| line.trim_start().starts_with("ImageJ="));
            if is_imagej {
                logical_pages = imagej_value(desc, "images").unwrap_or(1).max(1);
            }
        }
    }

    let inferred = infer_dataset_layout(
        description.as_deref(),
        x,
        y,
        logical_pages / samples,
        samples,
        ds,
        path,
    )?;
    let layout = resolve_layout(x, y, logical_pages, ds, inferred)?;
    let d = layout.dims;

    let mut canonical_to_source = vec![usize::MAX; logical_pages];
    if let Some(ref coords) = layout.ifd_coords {
        if coords.len() != logical_pages {
            return Err("OME TiffData mapping length does not match logical plane count".into());
        }
        for (src, &(t, c, z)) in coords.iter().enumerate() {
            let canon = (t * d.c + c) * d.z + z;
            if canon >= canonical_to_source.len() || canonical_to_source[canon] != usize::MAX {
                return Err("OME TiffData mapping is incomplete or duplicates a TCZ plane".into());
            }
            canonical_to_source[canon] = src;
        }
    } else {
        for t in 0..d.t {
            for c in 0..d.c {
                for z in 0..d.z {
                    let canon = (t * d.c + c) * d.z + z;
                    canonical_to_source[canon] = page_index(&layout.page_order, d, t, c, z);
                }
            }
        }
    }
    if canonical_to_source
        .iter()
        .any(|&v| v == usize::MAX || v >= logical_pages)
    {
        return Err("could not construct a complete TIFF-page to TCZ mapping".into());
    }

    Ok(StreamingDataset {
        layout,
        canonical_to_source,
        physical_pages,
        logical_pages,
        samples,
        description,
        sample_bytes,
    })
}

pub(super) fn load_dataset(
    path: &Path,
    ds: &DatasetArgs,
) -> Result<(Vec<u16>, ResolvedLayout), String> {
    let (raw, x, y, pages, samples, description) = read_tiff_pages_u16(path)?;
    let inferred = infer_dataset_layout(description.as_deref(), x, y, pages, samples, ds, path)?;
    let planes = pages
        .checked_mul(samples)
        .ok_or("TIFF plane count overflow")?;
    let layout = resolve_layout(x, y, planes, ds, inferred)?;
    let data = reorder_pages_to_tczyx(raw, &layout);
    Ok((data, layout))
}

pub(crate) fn tiff_limits() -> tiff::decoder::Limits {
    // The CLI budgets image/chunk allocations separately. The decoder's
    // 256 MiB output and 128 MiB intermediate defaults reject valid large XY
    // planes even in streaming mode. Lift those limits, keeping metadata bounded.
    let mut limits = tiff::decoder::Limits::unlimited();
    limits.ifd_value_size = radelta_native::metadata_limit();
    limits
}
