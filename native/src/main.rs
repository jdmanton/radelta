mod cli;
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FORCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
macro_rules! report {
    ($($arg:tt)*) => {
        if !QUIET.load(std::sync::atomic::Ordering::Relaxed) {
            // A closed report pipe should not panic while processing a file.
            let _ = writeln!(std::io::stdout().lock(), $($arg)*);
        }
    };
}
mod ome;
mod prefetch;
mod tiff_export;
mod tiff_io;
mod tiff_metadata;
use clap::Parser;
use ome::infer_ome_layout;
use radelta_native::{
    compress_lossy_u16_nd_with_stats, compress_u16_nd_with_stats, decompress_u16_nd_into, Dims5,
    LossyOptions, Options, DEFAULT_STREAM_MEMORY_MIB,
};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;
use tiff::decoder::Decoder;
use tiff::encoder::{colortype, TiffEncoder, TiffKind, TiffKindBig};
use tiff::tags::Tag;

#[derive(Debug, Clone)]
struct DatasetArgs {
    t: Option<usize>,
    c: Option<usize>,
    z: Option<usize>,
    page_order: Option<String>,
    ignore_metadata: bool,
    memory_mib: usize,
    force_stream: bool,
}

impl Default for DatasetArgs {
    fn default() -> Self {
        Self {
            t: None,
            c: None,
            z: None,
            page_order: None,
            ignore_metadata: false,
            memory_mib: DEFAULT_STREAM_MEMORY_MIB,
            force_stream: false,
        }
    }
}

#[derive(Debug, Clone)]
struct InferredLayout {
    t: usize,
    c: usize,
    z: usize,
    page_order: String,
    source: String,
    /// Optional exact TIFF-IFD -> (T,C,Z) mapping, used for OME-TIFF
    /// files whose TiffData elements explicitly override DimensionOrder.
    ifd_coords: Option<Vec<(usize, usize, usize)>>,
}

#[derive(Debug, Clone)]
struct ResolvedLayout {
    dims: Dims5,
    page_order: String,
    source: String,
    ifd_coords: Option<Vec<(usize, usize, usize)>>,
}

fn validate_page_order(order: &str) -> Result<(), String> {
    let b = order.as_bytes();
    if b.len() != 3 {
        return Err("--page-order must be a permutation of TCZ".into());
    }
    let mut v = b.to_vec();
    v.sort_unstable();
    if v.as_slice() != b"CTZ" {
        return Err("--page-order must be a permutation of TCZ".into());
    }
    Ok(())
}

fn imagej_value(desc: &str, key: &str) -> Option<usize> {
    desc.lines().find_map(|line| {
        let line = line.trim();
        let (k, v) = line.split_once('=')?;
        if k.trim().eq_ignore_ascii_case(key) {
            v.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

fn infer_imagej_layout(desc: &str, pages: usize) -> Result<Option<InferredLayout>, String> {
    if !desc
        .lines()
        .any(|line| line.trim_start().starts_with("ImageJ="))
    {
        return Ok(None);
    }
    let c = imagej_value(desc, "channels").unwrap_or(1);
    let t = imagej_value(desc, "frames").unwrap_or(1);
    if c == 0 || t == 0 {
        return Err("ImageJ channels/frames must be positive".into());
    }
    let z = match imagej_value(desc, "slices") {
        Some(v) => v,
        None => {
            let total = imagej_value(desc, "images").unwrap_or(pages);
            let ct = c.checked_mul(t).ok_or("ImageJ C*T overflow")?;
            if ct == 0 || !total.is_multiple_of(ct) {
                return Err("cannot infer ImageJ slices from images/(channels*frames)".into());
            }
            total / ct
        }
    };
    if z == 0 {
        return Err("ImageJ slices must be positive".into());
    }
    let product = c
        .checked_mul(z)
        .and_then(|v| v.checked_mul(t))
        .ok_or("ImageJ T*C*Z overflow")?;
    if product != pages {
        return Err(format!(
            "ImageJ metadata says C*Z*T={product}, but TIFF contains {pages} pages"
        ));
    }
    // ImageJ hyperstacks are stored C fastest, then Z, then T (CZT).
    // Our order notation is slowest -> fastest, hence TZC.
    Ok(Some(InferredLayout {
        t,
        c,
        z,
        page_order: "TZC".into(),
        source: "ImageJ/Fiji".into(),
        ifd_coords: None,
    }))
}

fn infer_layout_from_description(
    desc: Option<&str>,
    x: usize,
    y: usize,
    pages: usize,
    path: &Path,
) -> Result<Option<InferredLayout>, String> {
    let Some(desc) = desc else {
        return Ok(None);
    };
    if let Some(v) = infer_ome_layout(desc, x, y, pages, path)? {
        return Ok(Some(v));
    }
    if let Some(v) = infer_imagej_layout(desc, pages)? {
        return Ok(Some(v));
    }
    Ok(None)
}

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
fn page_samples(dec: &mut Decoder<BufReader<File>>) -> Result<usize, String> {
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
    if tiff_io::read_u8_tiled_sample(dec, plane, sample)? {
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

/// Inferred layout when every TIFF page holds `samples` planar samples.
///
/// Logical planes are numbered page by page with the sample index varying
/// fastest; by default the samples become channels and the pages Z planes.
fn infer_multisample_layout(
    description: Option<&str>,
    physical_pages: usize,
    samples: usize,
    ds: &DatasetArgs,
) -> Result<InferredLayout, String> {
    if !ds.ignore_metadata {
        if let Some(desc) = description {
            let imagej = desc
                .lines()
                .any(|line| line.trim_start().starts_with("ImageJ="));
            if imagej || desc.contains("<OME") || desc.contains(":OME") {
                return Err(format!(
                    "TIFF pages store {samples} planar samples per pixel and also carry OME/ImageJ dimensional metadata; combining the two is not supported. Use --ignore-metadata to read the samples as channels, with --t/--c/--z/--page-order if needed"
                ));
            }
        }
    }
    Ok(InferredLayout {
        t: 1,
        c: samples,
        z: physical_pages,
        page_order: "TZC".into(),
        source: format!("planar multisample TIFF ({samples} samples per pixel read as channels)"),
        ifd_coords: None,
    })
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

/// Layout inference shared by in-memory and streaming encoding. `pages` counts
/// TIFF pages before planar samples are expanded into separate planes.
fn infer_dataset_layout(
    description: Option<&str>,
    x: usize,
    y: usize,
    pages: usize,
    samples: usize,
    ds: &DatasetArgs,
    path: &Path,
) -> Result<Option<InferredLayout>, String> {
    if samples > 1 {
        infer_multisample_layout(description, pages, samples, ds).map(Some)
    } else if ds.ignore_metadata {
        Ok(None)
    } else {
        infer_layout_from_description(description, x, y, pages, path)
    }
}

fn resolve_layout(
    x: usize,
    y: usize,
    pages: usize,
    ds: &DatasetArgs,
    inferred: Option<InferredLayout>,
) -> Result<ResolvedLayout, String> {
    let inferred_t = inferred.as_ref().map(|m| m.t);
    let inferred_c = inferred.as_ref().map(|m| m.c);
    let inferred_z = inferred.as_ref().map(|m| m.z);

    let t = ds.t.or(inferred_t).unwrap_or(1);
    let c = ds.c.or(inferred_c).unwrap_or(1);
    if t == 0 || c == 0 {
        return Err("T and C must be positive".into());
    }
    let tc = t.checked_mul(c).ok_or("T*C overflow")?;
    let z = match ds.z.or(inferred_z) {
        Some(z) if z > 0 => z,
        Some(_) => return Err("Z must be positive".into()),
        None => {
            if !pages.is_multiple_of(tc) {
                return Err(format!("{pages} TIFF pages are not divisible by T*C={tc}; specify --z/--t/--c explicitly"));
            }
            pages / tc
        }
    };
    let expected = tc.checked_mul(z).ok_or("T*C*Z overflow")?;
    if expected != pages {
        return Err(format!(
            "T*C*Z = {expected}, but TIFF contains {pages} pages"
        ));
    }
    let dims = Dims5 { x, y, z, c, t };

    let page_order = ds
        .page_order
        .clone()
        .or_else(|| inferred.as_ref().map(|m| m.page_order.clone()))
        .unwrap_or_else(|| "TCZ".to_string());
    validate_page_order(&page_order)?;

    let manual_shape_override = ds.t.is_some() || ds.c.is_some() || ds.z.is_some();
    let ifd_coords = if ds.page_order.is_none() && !manual_shape_override {
        inferred.as_ref().and_then(|m| m.ifd_coords.clone())
    } else {
        None
    };

    let source = match inferred {
        Some(m)
            if ds.t.is_some()
                || ds.c.is_some()
                || ds.z.is_some()
                || ds.page_order.is_some()
                || ds.ignore_metadata =>
        {
            format!("{} + CLI override", m.source)
        }
        Some(m) => m.source,
        None if ds.t.is_some()
            || ds.c.is_some()
            || ds.z.is_some()
            || ds.page_order.is_some()
            || ds.ignore_metadata =>
        {
            "CLI-specified".into()
        }
        None => "plain TIFF fallback".into(),
    };

    Ok(ResolvedLayout {
        dims,
        page_order,
        source,
        ifd_coords,
    })
}

fn axis_dim(axis: u8, d: Dims5) -> usize {
    match axis {
        b'T' => d.t,
        b'C' => d.c,
        b'Z' => d.z,
        _ => unreachable!(),
    }
}

fn axis_coord(axis: u8, t: usize, c: usize, z: usize) -> usize {
    match axis {
        b'T' => t,
        b'C' => c,
        b'Z' => z,
        _ => unreachable!(),
    }
}

/// Page order is specified slowest -> fastest. E.g. TZC means C changes
/// fastest, then Z, then T.
fn page_index(order: &str, d: Dims5, t: usize, c: usize, z: usize) -> usize {
    let mut idx = 0usize;
    for axis in order.bytes() {
        idx = idx * axis_dim(axis, d) + axis_coord(axis, t, c, z);
    }
    idx
}

fn reorder_pages_to_tczyx(raw: Vec<u16>, layout: &ResolvedLayout) -> Vec<u16> {
    let d = layout.dims;
    let plane = d.x * d.y;
    if let Some(ref coords) = layout.ifd_coords {
        let mut out = vec![0u16; raw.len()];
        for (src_page, &(t, c, z)) in coords.iter().enumerate() {
            let dst_page = (t * d.c + c) * d.z + z;
            let src = src_page * plane;
            let dst = dst_page * plane;
            out[dst..dst + plane].copy_from_slice(&raw[src..src + plane]);
        }
        return out;
    }
    if layout.page_order == "TCZ" {
        return raw;
    }
    let mut out = vec![0u16; raw.len()];
    for t in 0..d.t {
        for c in 0..d.c {
            for z in 0..d.z {
                let src_page = page_index(&layout.page_order, d, t, c, z);
                let dst_page = (t * d.c + c) * d.z + z;
                let src = src_page * plane;
                let dst = dst_page * plane;
                out[dst..dst + plane].copy_from_slice(&raw[src..src + plane]);
            }
        }
    }
    out
}

#[derive(Clone)]
struct StreamingDataset {
    layout: ResolvedLayout,
    canonical_to_source: Vec<usize>,
    physical_pages: usize,
    logical_pages: usize,
    /// Planar samples per TIFF page; logical plane `p` is sample
    /// `p % samples` of page `p / samples`.
    samples: usize,
    sample_bytes: usize,
    description: Option<String>,
}

#[allow(clippy::large_enum_variant)]
enum PlaneReader {
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
    fn open(path: &Path, info: &StreamingDataset) -> Result<Self, String> {
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

    fn read_plane_into(&mut self, source_page: usize, out: &mut [u16]) -> Result<(), String> {
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

fn inspect_streaming_dataset(path: &Path, ds: &DatasetArgs) -> Result<StreamingDataset, String> {
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

fn encode_streaming_lossless(
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

fn encode_streaming_lossy(
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

fn pixel_buffer(samples: usize) -> Result<Vec<u16>, String> {
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(samples)
        .map_err(|_| "cannot allocate pipeline pixel buffer")?;
    pixels.resize(samples, 0);
    Ok(pixels)
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

fn coords_from_page_index(mut page: usize, order: &str, d: Dims5) -> (usize, usize, usize) {
    let mut t = 0usize;
    let mut c = 0usize;
    let mut z = 0usize;
    for axis in order.bytes().rev() {
        let dim = axis_dim(axis, d);
        let coord = page % dim;
        page /= dim;
        match axis {
            b'T' => t = coord,
            b'C' => c = coord,
            b'Z' => z = coord,
            _ => unreachable!(),
        }
    }
    (t, c, z)
}

fn ome_dimension_order(page_order: &str) -> String {
    // Radelta page_order is slowest -> fastest (e.g. TCZ means Z varies fastest).
    // OME DimensionOrder is X,Y followed by fastest -> slowest non-spatial axes.
    let rev: String = page_order.chars().rev().collect();
    format!("XY{rev}")
}

fn ome_description(d: Dims5, order: &str) -> String {
    let dim_order = ome_dimension_order(order);
    let mut channels = String::new();
    for c in 0..d.c {
        channels.push_str(&format!(
            r#"<Channel ID="Channel:0:{}" SamplesPerPixel="1"/>"#,
            c
        ));
    }
    let plane_count = d.t.saturating_mul(d.c).saturating_mul(d.z);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06"><Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="{}" Type="uint16" SizeX="{}" SizeY="{}" SizeZ="{}" SizeC="{}" SizeT="{}">{}<TiffData IFD="0" PlaneCount="{}"/></Pixels></Image></OME>"#,
        dim_order, d.x, d.y, d.z, d.c, d.t, channels, plane_count
    )
}

fn write_encoded_with_metadata(
    input: &Path,
    output: &Path,
    encoded: &[u8],
    layout: &ResolvedLayout,
) -> Result<usize, String> {
    let metadata = tiff_metadata::capture(input, layout)?;
    let (mut pending, mut file) =
        radelta_native::output::PendingOutput::create(output).map_err(|e| e.to_string())?;
    let written = radelta_native::write_encoded_with_metadata(&mut file, encoded, &metadata)
        .map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    publish_output(&mut pending)?;
    Ok(written)
}

fn load_dataset(path: &Path, ds: &DatasetArgs) -> Result<(Vec<u16>, ResolvedLayout), String> {
    let (raw, x, y, pages, samples, description) = read_tiff_pages_u16(path)?;
    let inferred = infer_dataset_layout(description.as_deref(), x, y, pages, samples, ds, path)?;
    let planes = pages
        .checked_mul(samples)
        .ok_or("TIFF plane count overflow")?;
    let layout = resolve_layout(x, y, planes, ds, inferred)?;
    let data = reorder_pages_to_tczyx(raw, &layout);
    Ok((data, layout))
}

fn gb_per_s(raw_bytes: usize, seconds: f64) -> f64 {
    raw_bytes as f64 / 1_000_000_000.0 / seconds.max(1e-12)
}
fn mib(n: usize) -> f64 {
    n as f64 / 1048576.0
}

// TIFF I/O, output allocation for decoding, and verification are outside timing.
fn measure<T>(
    repeats: usize,
    mut run: impl FnMut() -> Result<T, String>,
) -> Result<(T, f64), String> {
    drop(run()?);
    let mut times = Vec::with_capacity(repeats);
    let mut result = None;
    for _ in 0..repeats {
        let start = Instant::now();
        let value = run()?;
        times.push(start.elapsed().as_secs_f64());
        result = Some(value);
    }
    times.sort_by(f64::total_cmp);
    let median = (times[(repeats - 1) / 2] + times[repeats / 2]) / 2.0;
    Ok((result.unwrap(), median))
}

fn print_shape(layout: &ResolvedLayout) {
    let d = layout.dims;
    report!(
        "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
        d.t,
        d.c,
        d.z,
        d.y,
        d.x
    );
    report!("Layout source:   {}", layout.source);
    if layout.ifd_coords.is_some() {
        report!("Input mapping:   explicit OME TiffData IFD mapping");
    } else {
        report!(
            "Input page order: {} (slowest -> fastest)",
            layout.page_order
        );
    }
}

fn benchmark(path: &Path, opts: Options, ds: DatasetArgs, repeats: usize) -> Result<(), String> {
    let io0 = Instant::now();
    let (data, layout) = load_dataset(path, &ds)?;
    let dims = layout.dims;
    let raw_bytes = data.len() * inspect_streaming_dataset(path, &ds)?.sample_bytes;
    report!("Input:           {}", path.display());
    report!("Codec options:   {opts:?}");
    report!(
        "Timing:          median of {repeats} runs after warm-up, {} workers",
        rayon::current_num_threads()
    );
    print_shape(&layout);
    report!("Raw bytes:       {:.3} MiB", mib(raw_bytes));
    report!("TIFF read/map:   {:.3} s", io0.elapsed().as_secs_f64());
    report!();

    let ((encoded, stats), encode_time) = measure(repeats, || {
        compress_u16_nd_with_stats(&data, dims, opts).map_err(|e| e.to_string())
    })?;
    report!("Lossless compression:");
    report!("  volumes (T*C): {}", stats.volumes);
    report!("  q max:         {}", stats.q_max);
    report!("  r max:         {}", stats.r_max);
    report!("  blocks:        {}", stats.blocks);
    report!("  q stream:      {:.3} MiB", mib(stats.q_stream_bytes));
    report!("  r stream:      {:.3} MiB", mib(stats.r_stream_bytes));
    report!("  model tables:  {:.3} MiB", mib(stats.model_bytes));
    report!("  total file:    {:.3} MiB", mib(stats.total_bytes));
    report!(
        "  ratio:         {:.2}:1",
        raw_bytes as f64 / stats.total_bytes as f64
    );
    report!("  encode time:   {:.4} s", encode_time);
    report!(
        "  encode rate:   {:.3} GB/s",
        gb_per_s(raw_bytes, encode_time)
    );

    let mut decoded = vec![0u16; data.len()];
    let (dd, ds_time) = measure(repeats, || {
        decompress_u16_nd_into(&encoded, &mut decoded).map_err(|e| e.to_string())
    })?;
    if dd != dims || decoded != data {
        return Err("round-trip verification FAILED".into());
    }
    report!();
    report!("Decompression:");
    report!("  decode time:   {:.4} s", ds_time);
    report!("  decode rate:   {:.3} GB/s", gb_per_s(raw_bytes, ds_time));
    report!("  exact:         yes");
    Ok(())
}

fn error_stats(original: &[u16], decoded: &[u16], opts: LossyOptions) -> (f64, f64, u16, f64) {
    let mut sse = 0.0f64;
    let mut sae = 0.0f64;
    let mut max_abs = 0u16;
    let mut sse_e = 0.0f64;
    for (&a, &b) in original.iter().zip(decoded.iter()) {
        let d = b as f64 - a as f64;
        sse += d * d;
        sae += d.abs();
        max_abs = max_abs.max((b as i32 - a as i32).unsigned_abs().min(u16::MAX as u32) as u16);
        let ea = ((a as f64 - opts.offset_adu) * opts.gain_e_per_adu).max(0.0);
        let eb = ((b as f64 - opts.offset_adu) * opts.gain_e_per_adu).max(0.0);
        let de = eb - ea;
        sse_e += de * de;
    }
    let n = original.len().max(1) as f64;
    ((sse / n).sqrt(), sae / n, max_abs, (sse_e / n).sqrt())
}

fn benchmark_lossy(
    path: &Path,
    opts: LossyOptions,
    ds: DatasetArgs,
    repeats: usize,
) -> Result<(), String> {
    let io0 = Instant::now();
    let (data, layout) = load_dataset(path, &ds)?;
    let dims = layout.dims;
    let raw_bytes = data.len() * inspect_streaming_dataset(path, &ds)?.sample_bytes;
    report!("Input:           {}", path.display());
    report!("Codec options:   {opts:?}");
    report!(
        "Timing:          median of {repeats} runs after warm-up, {} workers",
        rayon::current_num_threads()
    );
    print_shape(&layout);
    report!("Raw bytes:       {:.3} MiB", mib(raw_bytes));
    report!("TIFF read/map:   {:.3} s", io0.elapsed().as_secs_f64());
    report!();
    report!("Lossy calibration:");
    report!("  offset:        {:.6} ADU", opts.offset_adu);
    report!("  gain:          {:.6} e-/ADU", opts.gain_e_per_adu);
    report!("  noise step:    {:.6}", opts.noise_step);
    report!(
        "  quant RMS*:    {:.4} sigma",
        opts.noise_step / 12.0f64.sqrt()
    );
    report!();

    let ((encoded, stats), encode_time) = measure(repeats, || {
        compress_lossy_u16_nd_with_stats(&data, dims, opts).map_err(|e| e.to_string())
    })?;
    report!("Lossy compression:");
    report!("  volumes (T*C): {}", stats.volumes);
    report!("  q max:         {}", stats.q_max);
    report!("  blocks:        {}", stats.blocks);
    report!("  q stream:      {:.3} MiB", mib(stats.q_stream_bytes));
    report!("  model tables:  {:.3} MiB", mib(stats.model_bytes));
    report!("  total file:    {:.3} MiB", mib(stats.total_bytes));
    report!(
        "  ratio:         {:.2}:1",
        raw_bytes as f64 / stats.total_bytes as f64
    );
    report!("  encode time:   {:.4} s", encode_time);
    report!(
        "  encode rate:   {:.3} GB/s",
        gb_per_s(raw_bytes, encode_time)
    );

    let mut decoded = vec![0u16; data.len()];
    let (dd, ds_time) = measure(repeats, || {
        decompress_u16_nd_into(&encoded, &mut decoded).map_err(|e| e.to_string())
    })?;
    if dd != dims {
        return Err("lossy decoded dimensions do not match".into());
    }
    let (rmse, mae, max_abs, rmse_e) = error_stats(&data, &decoded, opts);
    report!();
    report!("Decompression:");
    report!("  decode time:   {:.4} s", ds_time);
    report!("  decode rate:   {:.3} GB/s", gb_per_s(raw_bytes, ds_time));
    report!("  RMSE:          {:.4} ADU", rmse);
    report!("  MAE:           {:.4} ADU", mae);
    report!("  max abs error: {} ADU", max_abs);
    report!("  RMSE signal:   {:.4} electrons", rmse_e);
    Ok(())
}

fn tiff_limits() -> tiff::decoder::Limits {
    // The CLI budgets image/chunk allocations separately. The decoder's
    // 256 MiB output and 128 MiB intermediate defaults reject valid large XY
    // planes even in streaming mode. Lift those limits, keeping metadata bounded.
    let mut limits = tiff::decoder::Limits::unlimited();
    limits.ifd_value_size = radelta_native::metadata_limit();
    limits
}

fn main() {
    let cli = cli::Cli::parse();
    QUIET.store(cli.quiet, std::sync::atomic::Ordering::Relaxed);
    FORCE.store(cli.force, std::sync::atomic::Ordering::Relaxed);
    radelta_native::set_metadata_limit(cli.metadata_limit_mib * 1024 * 1024);
    let result = if let Some(threads) = cli.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(|e| format!("cannot create codec worker pool: {e}"))
            .and_then(|pool| pool.install(|| run_command(cli.command)))
    } else {
        run_command(cli.command)
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn dataset(layout: cli::LayoutArgs, memory_mib: usize, force_stream: bool) -> DatasetArgs {
    DatasetArgs {
        t: layout.t,
        c: layout.c,
        z: layout.z,
        page_order: layout.page_order,
        ignore_metadata: layout.ignore_metadata,
        memory_mib,
        force_stream,
    }
}

fn run_command(command: cli::Command) -> Result<(), String> {
    use cli::Command::*;
    match command {
        Encode(args) => {
            let (input, output) = args.paths.resolve("rdlt");
            prepare_conversion(&input, &output)?;
            encode_lossless(
                &input,
                &output,
                args.codec.options(),
                dataset(args.layout, args.memory.memory_mib, args.stream),
            )
            .map_err(|e| {
                format!(
                    "cannot encode '{}' to '{}': {e}",
                    input.display(),
                    output.display()
                )
            })
        }
        EncodeLossy(args) => {
            let opts = args.calibration.options(&args.encode.codec);
            let args = args.encode;
            let (input, output) = args.paths.resolve("lossy.rdlt");
            prepare_conversion(&input, &output)?;
            encode_lossy(
                &input,
                &output,
                opts,
                dataset(args.layout, args.memory.memory_mib, args.stream),
            )
            .map_err(|e| {
                format!(
                    "cannot encode '{}' to '{}': {e}",
                    input.display(),
                    output.display()
                )
            })
        }
        Decode(args) => {
            let (input, output) = args.paths.resolve("tif");
            prepare_conversion(&input, &output)?;
            decode_file(&input, &output, args.page_order, args.memory.memory_mib).map_err(|e| {
                format!(
                    "cannot decode '{}' to '{}': {e}",
                    input.display(),
                    output.display()
                )
            })
        }
        Benchmark(args) => benchmark(
            &args.input,
            args.codec.options(),
            dataset(args.layout, DEFAULT_STREAM_MEMORY_MIB, false),
            args.repeats,
        )
        .map_err(|e| format!("cannot benchmark '{}': {e}", args.input.display())),
        BenchmarkLossy(args) => {
            let opts = args.calibration.options(&args.benchmark.codec);
            let args = args.benchmark;
            benchmark_lossy(
                &args.input,
                opts,
                dataset(args.layout, DEFAULT_STREAM_MEMORY_MIB, false),
                args.repeats,
            )
            .map_err(|e| format!("cannot benchmark '{}': {e}", args.input.display()))
        }
    }
}

fn prepare_conversion(input: &Path, output: &Path) -> Result<(), String> {
    let metadata =
        fs::metadata(input).map_err(|e| format!("cannot read input '{}': {e}", input.display()))?;
    if !metadata.is_file() {
        return Err(format!("input '{}' is not a file", input.display()));
    }
    radelta_native::output::ensure_distinct(input, output)
        .map_err(|e| format!("output '{}': {e}", output.display()))?;
    // symlink_metadata also catches dangling output symlinks.
    match fs::symlink_metadata(output) {
        Ok(m) if m.is_dir() => {
            return Err(format!(
                "output '{}' is a directory; specify a filename",
                output.display()
            ))
        }
        Ok(_) if !FORCE.load(std::sync::atomic::Ordering::Relaxed) => {
            return Err(format!(
                "output '{}' already exists; use --force to replace it or choose another output",
                output.display()
            ))
        }
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(format!("cannot access output '{}': {e}", output.display())),
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.is_dir() {
        return Err(format!(
            "output directory '{}' does not exist or is inaccessible",
            parent.display()
        ));
    }
    Ok(())
}

fn publish_output(pending: &mut radelta_native::output::PendingOutput) -> Result<(), String> {
    if FORCE.load(std::sync::atomic::Ordering::Relaxed) {
        pending.publish()
    } else {
        pending.publish_no_replace()
    }
    .map_err(|e| e.to_string())
}

fn encode_lossless(
    input: &Path,
    output: &Path,
    opts: Options,
    ds: DatasetArgs,
) -> Result<(), String> {
    match inspect_streaming_dataset(input, &ds) {
        Ok(info) => {
            let d = info.layout.dims;
            let raw =
                d.x.checked_mul(d.y)
                    .and_then(|v| v.checked_mul(d.z))
                    .and_then(|v| v.checked_mul(d.c))
                    .and_then(|v| v.checked_mul(d.t))
                    .and_then(|v| v.checked_mul(2))
                    .ok_or_else(|| "dataset size overflow".to_string());
            match raw {
                Ok(raw_bytes) => {
                    let budget = ds.memory_mib.saturating_mul(1024 * 1024);
                    let use_stream = ds.force_stream || raw_bytes.saturating_mul(4) > budget;
                    if use_stream {
                        report!(
                            "Using out-of-core RDS3 encoding ({}; RAM allowance {} MiB).",
                            if ds.force_stream {
                                "requested with --stream"
                            } else {
                                "estimated working set exceeds allowance"
                            },
                            ds.memory_mib
                        );
                        encode_streaming_lossless(input, output, opts, &ds, &info)
                    } else {
                        match load_dataset(input, &ds) {
                            Ok((data, layout)) => {
                                let dims = layout.dims;
                                match compress_u16_nd_with_stats(&data, dims, opts) {
                                    Ok((encoded, stats)) => {
                                        match write_encoded_with_metadata(
                                            input, output, &encoded, &layout,
                                        ) {
                                            Err(e) => Err(e),
                                            Ok(written) => {
                                                report!(
                                                    "Wrote {} ({:.3} MiB)",
                                                    output.display(),
                                                    mib(written)
                                                );
                                                report!(
                                                    "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
                                                    dims.t,
                                                    dims.c,
                                                    dims.z,
                                                    dims.y,
                                                    dims.x
                                                );
                                                report!("Layout source: {}", layout.source);
                                                report!(
                                                    "Lossless encode rate: {:.3} GB/s",
                                                    gb_per_s(
                                                        stats.raw_bytes / 2 * info.sample_bytes,
                                                        stats.seconds_total
                                                    )
                                                );
                                                Ok(())
                                            }
                                        }
                                    }
                                    Err(e) => Err(e.to_string()),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

fn encode_lossy(
    input: &Path,
    output: &Path,
    opts: LossyOptions,
    ds: DatasetArgs,
) -> Result<(), String> {
    match inspect_streaming_dataset(input, &ds) {
        Ok(info) => {
            let d = info.layout.dims;
            let raw =
                d.x.checked_mul(d.y)
                    .and_then(|v| v.checked_mul(d.z))
                    .and_then(|v| v.checked_mul(d.c))
                    .and_then(|v| v.checked_mul(d.t))
                    .and_then(|v| v.checked_mul(2))
                    .ok_or_else(|| "dataset size overflow".to_string());
            match raw {
                Ok(raw_bytes) => {
                    let budget = ds.memory_mib.saturating_mul(1024 * 1024);
                    let use_stream = ds.force_stream || raw_bytes.saturating_mul(4) > budget;
                    if use_stream {
                        report!(
                            "Using out-of-core RQS3 encoding ({}; RAM allowance {} MiB).",
                            if ds.force_stream {
                                "requested with --stream"
                            } else {
                                "estimated working set exceeds allowance"
                            },
                            ds.memory_mib
                        );
                        encode_streaming_lossy(input, output, opts, &ds, &info)
                    } else {
                        match load_dataset(input, &ds) {
                            Ok((data, layout)) => {
                                let dims = layout.dims;
                                match compress_lossy_u16_nd_with_stats(&data, dims, opts) {
                                    Ok((encoded, stats)) => {
                                        match write_encoded_with_metadata(
                                            input, output, &encoded, &layout,
                                        ) {
                                            Err(e) => Err(e),
                                            Ok(written) => {
                                                report!(
                                                    "Wrote {} ({:.3} MiB)",
                                                    output.display(),
                                                    mib(written)
                                                );
                                                report!(
                                                    "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
                                                    dims.t,
                                                    dims.c,
                                                    dims.z,
                                                    dims.y,
                                                    dims.x
                                                );
                                                report!("Layout source: {}", layout.source);
                                                report!(
                                                    "Lossy encode rate: {:.3} GB/s",
                                                    gb_per_s(
                                                        stats.raw_bytes / 2 * info.sample_bytes,
                                                        stats.seconds_total
                                                    )
                                                );
                                                report!("offset={:.6} ADU, gain={:.6} e-/ADU, noise-step={:.6}", opts.offset_adu, opts.gain_e_per_adu, opts.noise_step);
                                                Ok(())
                                            }
                                        }
                                    }
                                    Err(e) => Err(e.to_string()),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

fn decode_file(
    input: &Path,
    output: &Path,
    order: Option<String>,
    memory_mib: usize,
) -> Result<(), String> {
    radelta_native::RadeltaFileHandle::open(input).and_then(|file| {
        let info = file.info();
        let d = Dims5 {
            x: info.nx as usize,
            y: info.ny as usize,
            z: info.nz as usize,
            c: info.nc as usize,
            t: info.nt as usize,
        };
        let (mut pending, reserved) =
            radelta_native::output::PendingOutput::create(output).map_err(|e| e.to_string())?;
        drop(reserved);
        let metadata_len = file.metadata().len();
        tiff_export::write(
            std::sync::Arc::new(file),
            d,
            order.as_deref(),
            memory_mib,
            pending.path(),
        )?;
        publish_output(&mut pending)?;
        report!("Wrote {}", output.display());
        report!(
            "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
            d.t,
            d.c,
            d.z,
            d.y,
            d.x
        );
        report!("Metadata: {} bytes", metadata_len);
        Ok(())
    })
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn parses_imagej_hyperstack() {
        let d = "ImageJ=1.54f\nimages=24\nchannels=2\nslices=3\nframes=4\nhyperstack=true\n";
        let m = infer_imagej_layout(d, 24).unwrap().unwrap();
        assert_eq!((m.t, m.c, m.z), (4, 2, 3));
        assert_eq!(m.page_order, "TZC");
    }

    #[test]
    fn parses_ome_dimension_order() {
        let d = r#"<?xml version="1.0"?><OME><Image><Pixels DimensionOrder="XYZTC" SizeX="10" SizeY="7" SizeZ="3" SizeT="2" SizeC="2"><TiffData/></Pixels></Image></OME>"#;
        let m = infer_ome_layout(d, 10, 7, 12, Path::new("image.ome.tif"))
            .unwrap()
            .unwrap();
        assert_eq!((m.t, m.c, m.z), (2, 2, 3));
        assert_eq!(m.page_order, "CTZ");
        assert!(m.ifd_coords.is_none());
    }

    #[test]
    fn parses_ome_explicit_reverse_time_mapping() {
        let d = r#"<OME><Image><Pixels DimensionOrder="XYZTC" SizeX="10" SizeY="7" SizeZ="1" SizeC="1" SizeT="3"><TiffData IFD="0" FirstT="2"/><TiffData IFD="1" FirstT="1"/><TiffData IFD="2" FirstT="0"/></Pixels></Image></OME>"#;
        let m = infer_ome_layout(d, 10, 7, 3, Path::new("image.ome.tif"))
            .unwrap()
            .unwrap();
        let map = m.ifd_coords.unwrap();
        assert_eq!(map, vec![(2, 0, 0), (1, 0, 0), (0, 0, 0)]);
    }

    #[test]
    fn page_order_validation_accepts_all_permutations() {
        for order in ["TCZ", "TZC", "CTZ", "CZT", "ZTC", "ZCT"] {
            assert!(validate_page_order(order).is_ok(), "{order}");
        }
        for order in ["", "TC", "TTZ", "XYZ", "TCZZ"] {
            assert!(validate_page_order(order).is_err(), "{order}");
        }
    }

    #[test]
    fn page_index_and_inverse_are_consistent() {
        let d = Dims5 {
            x: 1,
            y: 1,
            z: 3,
            c: 2,
            t: 4,
        };
        for order in ["TCZ", "TZC", "CTZ", "CZT", "ZTC", "ZCT"] {
            for t in 0..d.t {
                for c in 0..d.c {
                    for z in 0..d.z {
                        let page = page_index(order, d, t, c, z);
                        assert_eq!(coords_from_page_index(page, order, d), (t, c, z), "{order}");
                    }
                }
            }
        }
    }

    #[test]
    fn imagej_metadata_page_count_must_match() {
        let d = "ImageJ=1.54f\nimages=24\nchannels=2\nslices=3\nframes=4\nhyperstack=true\n";
        assert!(infer_imagej_layout(d, 23).is_err());
    }
}
