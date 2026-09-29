mod tiff_metadata;
use radelta_native::{
    choose_stream_chunk_depth, compress_lossy_u16_nd_with_stats, compress_lossy_u16_with_stats,
    compress_u16_nd_with_stats, compress_u16_with_stats, decompress_u16_nd_into, ContextMode, Dims,
    Dims5, LossyOptions, Options, DEFAULT_BLOCK_DEPTH, DEFAULT_STREAM_MEMORY_MIB,
    STREAM_MAGIC_LOSSLESS, STREAM_MAGIC_LOSSY, STREAM_VERSION,
};
use std::env;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;
use tiff::decoder::{Decoder, DecodingResult};
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

fn usage() -> ! {
    eprintln!(
        "Usage:\n  \\
         radelta encode INPUT.tif OUTPUT.rdlt [codec options] [dataset options]\n  \\
         radelta encode-lossy INPUT.tif OUTPUT.rdlt --offset-adu O (--gain-e-per-adu G | --gain-adu-per-e G) [--noise-step D] [codec options] [dataset options]\n  \\
         radelta decode INPUT.rdlt OUTPUT.tif [--page-order TCZ]\n\n\
         Benchmarks (in memory, median of 3 runs after warm-up):\n  \
           radelta benchmark INPUT.tif [--repeats N] [codec options] [dataset options]\n  \
           radelta benchmark-lossy INPUT.tif --offset-adu O --gain-e-per-adu G [--noise-step D] [--repeats N] [codec options] [dataset options]\n\n\
         Codec options:\n  \\
           --block-depth N\n  \\
           --context mean|mean-signs|signed3|signs\n  \\
           --scale-bits N\n\n\
         Dataset options (normally unnecessary when metadata are present):\n  \\
           --t N                 override inferred number of timepoints\n  \\
           --c N                 override inferred number of channels\n  \\
           --z N                 override inferred number of Z planes\n  \\
           --page-order TCZ      override page axes, slowest to fastest\n  \\
           --ignore-metadata     do not use OME/ImageJ dimensional metadata\n  \
           --memory-mib N        approximate RAM budget for encoding (default 2048)\n  \
           --stream              force chunked out-of-core encoding\n\n\
         Metadata option (all commands): --metadata-limit-mib N (default 1024).\n\n\
         TIFF metadata is preserved; decode restores source page order by default.\n\n\
         Metadata inference:\n  \\
           * OME-TIFF: SizeT/SizeC/SizeZ + DimensionOrder from OME-XML.\n  \\
             Explicit single-file TiffData IFD mappings are honored.\n  \\
           * ImageJ/Fiji TIFF: channels/slices/frames; ImageJ CZT storage order.\n  \\
           * Plain TIFF fallback: T=1, C=1, Z=number of pages.\n\n\
         Examples:\n  \\
           radelta encode stack_or_hyperstack.tif output.rdlt\n  \\
           radelta encode odd_vendor.tif output.rdlt --ignore-metadata --t 20 --c 4 --page-order TZC\n\n\
         Canonical library memory order is T,C,Z,Y,X (X fastest).\n  \\
         Spatial contexts never cross channel or time boundaries.\n\n\
         Lossy transform:\n  \\
           electrons = max((ADU - offset_adu) * gain_e_per_adu, 0)\n  \\
           z         = 2*sqrt(electrons)\n  \\
           q         = round(z / noise_step)\n\n\
         Notes:\n  \\
         * X and Y may be rectangular; all pages in one dense TIFF dataset must share X/Y.\n  \\
         * The CLI currently expects grayscale uint16 pages. Channels may be separate TIFF pages.\n  \\
         * Multi-file OME-TIFF datasets are not yet assembled automatically.\n  \\
         * Single volumes use RDL1/RDLQ; multidimensional files use RDM2/RDQ2."
    );
    std::process::exit(2);
}

fn parse_usize_arg(args: &[String], i: &mut usize) -> usize {
    *i += 1;
    args.get(*i)
        .unwrap_or_else(|| usage())
        .parse()
        .unwrap_or_else(|_| usage())
}

fn parse_u32_arg(args: &[String], i: &mut usize) -> u32 {
    parse_usize_arg(args, i)
        .try_into()
        .unwrap_or_else(|_| usage())
}

fn parse_dataset_option(args: &[String], i: &mut usize, ds: &mut DatasetArgs) -> bool {
    match args[*i].as_str() {
        "--t" => {
            ds.t = Some(parse_usize_arg(args, i));
            true
        }
        "--c" => {
            ds.c = Some(parse_usize_arg(args, i));
            true
        }
        "--z" => {
            ds.z = Some(parse_usize_arg(args, i));
            true
        }
        "--page-order" => {
            *i += 1;
            ds.page_order = Some(args.get(*i).unwrap_or_else(|| usage()).to_ascii_uppercase());
            true
        }
        "--ignore-metadata" => {
            ds.ignore_metadata = true;
            true
        }
        "--memory-mib" => {
            ds.memory_mib = parse_usize_arg(args, i).max(64);
            true
        }
        "--stream" => {
            ds.force_stream = true;
            true
        }
        _ => false,
    }
}

fn parse_lossless_args(args: &[String]) -> (Options, DatasetArgs) {
    let mut o = Options::default();
    let mut ds = DatasetArgs::default();
    let mut i = 0usize;
    while i < args.len() {
        if parse_dataset_option(args, &mut i, &mut ds) {
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--block-depth" => o.block_depth = parse_u32_arg(args, &mut i),
            "--scale-bits" => o.scale_bits = parse_u32_arg(args, &mut i),
            "--context" => {
                i += 1;
                o.context_mode = match args.get(i).map(|s| s.as_str()) {
                    Some("signed3") => ContextMode::Signed3 as u32,
                    Some("signs") => ContextMode::Signs as u32,
                    Some("mean") => ContextMode::Mean as u32,
                    Some("mean-signs") => ContextMode::MeanSigns as u32,
                    _ => usage(),
                };
            }
            _ => usage(),
        }
        i += 1;
    }
    if let Some(ref order) = ds.page_order {
        validate_page_order(order).unwrap_or_else(|_| usage());
    }
    (o, ds)
}

fn parse_lossy_args(args: &[String]) -> (LossyOptions, DatasetArgs) {
    let mut o = LossyOptions::default();
    let mut ds = DatasetArgs::default();
    let mut have_offset = false;
    let mut have_gain = false;
    let mut i = 0usize;
    while i < args.len() {
        if parse_dataset_option(args, &mut i, &mut ds) {
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--block-depth" => o.block_depth = parse_u32_arg(args, &mut i),
            "--scale-bits" => o.scale_bits = parse_u32_arg(args, &mut i),
            "--context" => {
                i += 1;
                o.context_mode = match args.get(i).map(|s| s.as_str()) {
                    Some("signed3") => ContextMode::Signed3 as u32,
                    Some("signs") => ContextMode::Signs as u32,
                    Some("mean") => ContextMode::Mean as u32,
                    Some("mean-signs") => ContextMode::MeanSigns as u32,
                    _ => usage(),
                };
            }
            "--offset-adu" => {
                i += 1;
                o.offset_adu = args
                    .get(i)
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage());
                have_offset = true;
            }
            "--gain-e-per-adu" => {
                if have_gain {
                    usage();
                }
                i += 1;
                o.gain_e_per_adu = args
                    .get(i)
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage());
                have_gain = true;
            }
            "--gain-adu-per-e" => {
                if have_gain {
                    usage();
                }
                i += 1;
                let g: f64 = args
                    .get(i)
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage());
                if !g.is_finite() || g <= 0.0 {
                    usage();
                }
                o.gain_e_per_adu = 1.0 / g;
                have_gain = true;
            }
            "--noise-step" => {
                i += 1;
                o.noise_step = args
                    .get(i)
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            _ => usage(),
        }
        i += 1;
    }
    if !have_offset || !have_gain {
        usage();
    }
    if let Some(ref order) = ds.page_order {
        validate_page_order(order).unwrap_or_else(|_| usage());
    }
    (o, ds)
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

fn xml_start_tags<'a>(text: &'a str, local_name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(rel) = text[i..].find('<') else {
            break;
        };
        let start = i + rel;
        let rest = &text[start + 1..];
        if rest.starts_with('/') || rest.starts_with('!') || rest.starts_with('?') {
            i = start + 1;
            continue;
        }
        let token_end = rest
            .find(|ch: char| ch.is_ascii_whitespace() || ch == '>' || ch == '/')
            .unwrap_or(rest.len());
        let token = &rest[..token_end];
        let local = token.rsplit(':').next().unwrap_or(token);
        if local == local_name {
            if let Some(close) = text[start..].find('>') {
                out.push(&text[start..start + close + 1]);
                i = start + close + 1;
                continue;
            }
        }
        i = start + 1;
    }
    out
}

fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let mut pos = 0usize;
    while let Some(rel) = tag[pos..].find(name) {
        let p = pos + rel;
        let before_ok = p == 0 || !tag.as_bytes()[p - 1].is_ascii_alphanumeric();
        let after = p + name.len();
        if before_ok {
            let mut j = after;
            while j < tag.len() && tag.as_bytes()[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < tag.len() && tag.as_bytes()[j] == b'=' {
                j += 1;
                while j < tag.len() && tag.as_bytes()[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < tag.len() && (tag.as_bytes()[j] == b'"' || tag.as_bytes()[j] == b'\'') {
                    let quote = tag.as_bytes()[j];
                    let value_start = j + 1;
                    if let Some(k) = tag.as_bytes()[value_start..]
                        .iter()
                        .position(|&b| b == quote)
                    {
                        return Some(tag[value_start..value_start + k].to_string());
                    }
                }
            }
        }
        pos = after;
    }
    None
}

fn parse_usize_attr(tag: &str, name: &str) -> Result<Option<usize>, String> {
    match xml_attr(tag, name) {
        Some(v) => Ok(Some(
            v.parse::<usize>()
                .map_err(|_| format!("invalid OME {name}=\"{v}\""))?,
        )),
        None => Ok(None),
    }
}

fn coords_to_ome_linear(t: usize, c: usize, z: usize, fastest_to_slowest: &str, d: Dims5) -> usize {
    let mut idx = 0usize;
    let mut stride = 1usize;
    for axis in fastest_to_slowest.bytes() {
        let coord = axis_coord(axis, t, c, z);
        idx += coord * stride;
        stride *= axis_dim(axis, d);
    }
    idx
}

fn ome_linear_to_coords(
    mut idx: usize,
    fastest_to_slowest: &str,
    d: Dims5,
) -> (usize, usize, usize) {
    let mut t = 0usize;
    let mut c = 0usize;
    let mut z = 0usize;
    for axis in fastest_to_slowest.bytes() {
        let dim = axis_dim(axis, d);
        let coord = idx % dim;
        idx /= dim;
        match axis {
            b'T' => t = coord,
            b'C' => c = coord,
            b'Z' => z = coord,
            _ => unreachable!(),
        }
    }
    (t, c, z)
}

fn infer_ome_layout(
    desc: &str,
    x: usize,
    y: usize,
    pages: usize,
) -> Result<Option<InferredLayout>, String> {
    if !desc.contains("<OME") && !desc.contains(":OME") {
        return Ok(None);
    }
    let pixels_tags = xml_start_tags(desc, "Pixels");
    let Some(pixels) = pixels_tags.first().copied() else {
        return Ok(None);
    };

    let sx = parse_usize_attr(pixels, "SizeX")?.ok_or("OME Pixels missing SizeX")?;
    let sy = parse_usize_attr(pixels, "SizeY")?.ok_or("OME Pixels missing SizeY")?;
    let z = parse_usize_attr(pixels, "SizeZ")?.ok_or("OME Pixels missing SizeZ")?;
    let c = parse_usize_attr(pixels, "SizeC")?.ok_or("OME Pixels missing SizeC")?;
    let t = parse_usize_attr(pixels, "SizeT")?.ok_or("OME Pixels missing SizeT")?;
    if sx != x || sy != y {
        return Err(format!(
            "OME SizeX/SizeY={sx}x{sy}, but TIFF pages are {x}x{y}"
        ));
    }
    if z == 0 || c == 0 || t == 0 {
        return Err("OME SizeZ/SizeC/SizeT must be positive".into());
    }
    let product = z
        .checked_mul(c)
        .and_then(|v| v.checked_mul(t))
        .ok_or("OME T*C*Z overflow")?;
    if product != pages {
        return Err(format!(
            "OME metadata describes T*C*Z={product} planes but this TIFF contains {pages} IFDs; multi-file/partial OME-TIFF datasets are not yet supported by the single-file CLI"
        ));
    }

    let dimension_order = xml_attr(pixels, "DimensionOrder")
        .ok_or("OME Pixels missing DimensionOrder")?
        .to_ascii_uppercase();
    if dimension_order.len() != 5 || !dimension_order.starts_with("XY") {
        return Err(format!("unsupported OME DimensionOrder={dimension_order}; expected XY followed by a permutation of ZCT"));
    }
    let fastest_to_slowest = &dimension_order[2..];
    let mut axes = fastest_to_slowest.as_bytes().to_vec();
    axes.sort_unstable();
    if axes.as_slice() != b"CTZ" {
        return Err(format!("unsupported OME DimensionOrder={dimension_order}"));
    }
    let page_order: String = fastest_to_slowest.chars().rev().collect();
    let dims = Dims5 { x, y, z, c, t };

    let tiffdata_tags = xml_start_tags(desc, "TiffData");
    let has_explicit = tiffdata_tags.iter().any(|tag| {
        ["IFD", "FirstZ", "FirstC", "FirstT", "PlaneCount"]
            .iter()
            .any(|a| xml_attr(tag, a).is_some())
    });

    let ifd_coords = if has_explicit {
        let mut map: Vec<Option<(usize, usize, usize)>> = vec![None; pages];
        for tag in tiffdata_tags {
            let ifd = parse_usize_attr(tag, "IFD")?.unwrap_or(0);
            let first_z = parse_usize_attr(tag, "FirstZ")?.unwrap_or(0);
            let first_c = parse_usize_attr(tag, "FirstC")?.unwrap_or(0);
            let first_t = parse_usize_attr(tag, "FirstT")?.unwrap_or(0);
            let count = match parse_usize_attr(tag, "PlaneCount")? {
                Some(n) => n,
                None if xml_attr(tag, "IFD").is_some() => 1,
                None => pages,
            };
            if first_t >= t || first_c >= c || first_z >= z {
                return Err(
                    "OME TiffData FirstT/FirstC/FirstZ lies outside Pixels dimensions".into(),
                );
            }
            let start_linear =
                coords_to_ome_linear(first_t, first_c, first_z, fastest_to_slowest, dims);
            for j in 0..count {
                let dst_ifd = ifd.checked_add(j).ok_or("OME TiffData IFD overflow")?;
                let linear = start_linear
                    .checked_add(j)
                    .ok_or("OME TiffData coordinate overflow")?;
                if dst_ifd >= pages || linear >= product {
                    return Err(
                        "OME TiffData mapping exceeds this TIFF or Pixels dimensions".into(),
                    );
                }
                let coord = ome_linear_to_coords(linear, fastest_to_slowest, dims);
                if map[dst_ifd].replace(coord).is_some() {
                    return Err(format!("OME TiffData maps IFD {dst_ifd} more than once"));
                }
            }
        }
        if map.iter().any(Option::is_none) {
            return Err("OME TiffData mapping does not cover every TIFF IFD".into());
        }
        Some(map.into_iter().map(Option::unwrap).collect())
    } else {
        None
    };

    Ok(Some(InferredLayout {
        t,
        c,
        z,
        page_order,
        source: if ifd_coords.is_some() {
            "OME-TIFF (explicit TiffData mapping)".into()
        } else {
            "OME-TIFF".into()
        },
        ifd_coords,
    }))
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
) -> Result<Option<InferredLayout>, String> {
    let Some(desc) = desc else {
        return Ok(None);
    };
    if let Some(v) = infer_ome_layout(desc, x, y, pages)? {
        return Ok(Some(v));
    }
    if let Some(v) = infer_imagej_layout(desc, pages)? {
        return Ok(Some(v));
    }
    Ok(None)
}

#[allow(clippy::chunks_exact_to_as_chunks)]
fn read_imagej_contiguous_u16(
    dec: &mut Decoder<BufReader<File>>,
    width: usize,
    height: usize,
    logical_pages: usize,
) -> Result<Vec<u16>, String> {
    // ImageJ/tifffile hyperstacks may store all pixels contiguously after the
    // first IFD and omit IFDs for the remaining logical planes.  This reader
    // intentionally supports only the safe/simple case used for such stacks:
    // uncompressed, single-sample, 16-bit grayscale data whose strips for the
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
    if bits.as_slice() != [16] {
        return Err(format!(
            "contiguous ImageJ stack must be 16-bit grayscale; BitsPerSample={bits:?}"
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

    let plane_samples = width
        .checked_mul(height)
        .ok_or("TIFF plane size overflow")?;
    let plane_bytes = plane_samples
        .checked_mul(2)
        .ok_or("TIFF plane byte size overflow")?;
    let first_plane_bytes: u64 = counts
        .iter()
        .try_fold(0u64, |a, &b| a.checked_add(b).ok_or(()))
        .map_err(|_| "TIFF strip-byte-count overflow")?;
    if first_plane_bytes != plane_bytes as u64 {
        return Err(format!(
            "contiguous ImageJ TIFF first IFD describes {first_plane_bytes} bytes of pixel data, expected {plane_bytes} for one {width}x{height} uint16 plane"
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
        .checked_mul(2)
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

    let mut out = Vec::<u16>::with_capacity(total_samples);
    // Keep temporary memory bounded even for very large camera frames.
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    let mut remaining = total_bytes;
    while remaining != 0 {
        let take = remaining.min(buf.len());
        // total_bytes and buf.len() are even, so every chunk contains complete u16s.
        reader
            .read_exact(&mut buf[..take])
            .map_err(|e| e.to_string())?;
        for b in buf[..take].chunks_exact(2) {
            let pair = [b[0], b[1]];
            let value = match byte_order {
                tiff::tags::ByteOrder::LittleEndian => u16::from_le_bytes(pair),
                tiff::tags::ByteOrder::BigEndian => u16::from_be_bytes(pair),
            };
            out.push(value);
        }
        remaining -= take;
    }
    debug_assert_eq!(out.len(), total_samples);
    Ok(out)
}

type TiffPages = (Vec<u16>, usize, usize, usize, Option<String>);

fn read_tiff_pages_u16(path: &Path) -> Result<TiffPages, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut dec = Decoder::new(BufReader::new(file))
        .map_err(|e| e.to_string())?
        .with_limits(tiff_limits());
    let mut data = Vec::<u16>::new();
    let mut width = 0usize;
    let mut height = 0usize;
    let mut pages = 0usize;
    let mut image_description: Option<String> = None;

    loop {
        let (w, h) = dec.dimensions().map_err(|e| e.to_string())?;
        if pages == 0 {
            width = w as usize;
            height = h as usize;
            image_description = dec.get_tag_ascii_string(Tag::ImageDescription).ok();

            // Some ImageJ/tifffile hyperstacks deliberately have a single IFD
            // even though ImageDescription reports many logical images.  When
            // that layout is detected, bypass normal IFD iteration and read the
            // contiguous pixel block directly.
            if !dec.more_images() {
                if let Some(desc) = image_description.as_deref() {
                    let is_imagej = desc
                        .lines()
                        .any(|line| line.trim_start().starts_with("ImageJ="));
                    let logical_pages = imagej_value(desc, "images").unwrap_or(1);
                    if is_imagej && logical_pages > 1 {
                        let v = read_imagej_contiguous_u16(&mut dec, width, height, logical_pages)?;
                        return Ok((v, width, height, logical_pages, image_description));
                    }
                }
            }
        } else if width != w as usize || height != h as usize {
            return Err(
                "all TIFF pages in one dense dataset must have the same X/Y dimensions".into(),
            );
        }
        match dec.read_image().map_err(|e| e.to_string())? {
            DecodingResult::U16(v) => {
                let expected = width
                    .checked_mul(height)
                    .ok_or("TIFF plane size overflow")?;
                if v.len() != expected {
                    return Err(format!(
                        "expected a grayscale uint16 TIFF page with {expected} samples, got {}; interleaved RGB/multisample TIFF pages are not yet supported by the CLI",
                        v.len()
                    ));
                }
                data.extend_from_slice(&v);
            }
            other => return Err(format!("expected uint16 grayscale TIFF, got {other:?}")),
        }
        pages += 1;
        if !dec.more_images() {
            break;
        }
        dec.next_image().map_err(|e| e.to_string())?;
    }

    Ok((data, width, height, pages, image_description))
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

struct StreamingDataset {
    layout: ResolvedLayout,
    canonical_to_source: Vec<usize>,
    physical_pages: usize,
    logical_pages: usize,
    description: Option<String>,
}

#[allow(clippy::large_enum_variant)]
enum PlaneReader {
    Ifd {
        dec: Decoder<BufReader<File>>,
        width: usize,
        height: usize,
    },
    ImageJContiguous {
        reader: BufReader<File>,
        data_offset: u64,
        byte_order: tiff::tags::ByteOrder,
        plane_samples: usize,
        plane_bytes: usize,
        logical_pages: usize,
        byte_buf: Vec<u8>,
    },
}

impl PlaneReader {
    fn open(path: &Path, info: &StreamingDataset) -> Result<Self, String> {
        let d = info.layout.dims;
        if info.physical_pages == 1 && info.logical_pages > 1 {
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
                if bits.as_slice() != [16] {
                    return Err(format!(
                        "contiguous ImageJ stack must be uint16 grayscale; BitsPerSample={bits:?}"
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
                let plane_samples = d.x.checked_mul(d.y).ok_or("TIFF plane size overflow")?;
                let plane_bytes = plane_samples
                    .checked_mul(2)
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
                    logical_pages: info.logical_pages,
                    byte_buf: vec![0u8; plane_bytes],
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
        })
    }

    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn read_plane(&mut self, source_page: usize) -> Result<Vec<u16>, String> {
        match self {
            Self::Ifd { dec, width, height } => {
                dec.seek_to_image(source_page).map_err(|e| e.to_string())?;
                let (w, h) = dec.dimensions().map_err(|e| e.to_string())?;
                if w as usize != *width || h as usize != *height {
                    return Err("TIFF plane dimensions changed within dataset".into());
                }
                match dec.read_image().map_err(|e| e.to_string())? {
                    DecodingResult::U16(v) => {
                        let expected = width
                            .checked_mul(*height)
                            .ok_or("TIFF plane size overflow")?;
                        if v.len() != expected {
                            return Err(format!(
                                "expected {expected} uint16 samples, got {}",
                                v.len()
                            ));
                        }
                        Ok(v)
                    }
                    other => Err(format!("expected uint16 grayscale TIFF, got {other:?}")),
                }
            }
            Self::ImageJContiguous {
                reader,
                data_offset,
                byte_order,
                plane_samples,
                plane_bytes,
                logical_pages,
                byte_buf,
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
                reader.read_exact(byte_buf).map_err(|e| e.to_string())?;
                let mut out = Vec::with_capacity(*plane_samples);
                for b in byte_buf.chunks_exact(2) {
                    let pair = [b[0], b[1]];
                    out.push(match *byte_order {
                        tiff::tags::ByteOrder::LittleEndian => u16::from_le_bytes(pair),
                        tiff::tags::ByteOrder::BigEndian => u16::from_be_bytes(pair),
                    });
                }
                Ok(out)
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

    let mut physical_pages = 1usize;
    while dec.more_images() {
        dec.next_image().map_err(|e| e.to_string())?;
        let (ww, hh) = dec.dimensions().map_err(|e| e.to_string())?;
        if ww as usize != x || hh as usize != y {
            return Err(
                "all TIFF pages in one dense dataset must have the same X/Y dimensions".into(),
            );
        }
        physical_pages += 1;
    }

    let mut logical_pages = physical_pages;
    if physical_pages == 1 {
        if let Some(desc) = description.as_deref() {
            let is_imagej = desc
                .lines()
                .any(|line| line.trim_start().starts_with("ImageJ="));
            if is_imagej {
                logical_pages = imagej_value(desc, "images").unwrap_or(1).max(1);
            }
        }
    }

    let inferred = if ds.ignore_metadata {
        None
    } else {
        infer_layout_from_description(description.as_deref(), x, y, logical_pages)?
    };
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
        description,
    })
}

fn write_u16_le<W: Write>(w: &mut W, v: u16) -> Result<(), String> {
    w.write_all(&v.to_le_bytes()).map_err(|e| e.to_string())
}
fn write_u32_le<W: Write>(w: &mut W, v: u32) -> Result<(), String> {
    w.write_all(&v.to_le_bytes()).map_err(|e| e.to_string())
}
fn write_u64_le<W: Write>(w: &mut W, v: u64) -> Result<(), String> {
    w.write_all(&v.to_le_bytes()).map_err(|e| e.to_string())
}
fn stream_header<W: Write>(
    w: &mut W,
    magic: &[u8; 4],
    d: Dims5,
    chunk_z: usize,
    chunks: u64,
) -> Result<(), String> {
    w.write_all(magic).map_err(|e| e.to_string())?;
    write_u16_le(w, STREAM_VERSION)?;
    write_u16_le(w, 0)?;
    for v in [d.x, d.y, d.z, d.c, d.t] {
        write_u32_le(w, u32::try_from(v).map_err(|_| "dimension exceeds u32")?)?;
    }
    write_u32_le(
        w,
        u32::try_from(chunk_z).map_err(|_| "chunk depth exceeds u32")?,
    )?;
    write_u64_le(w, chunks)?;
    Ok(())
}

fn encode_streaming_lossless(
    input: &Path,
    output: &Path,
    opts: Options,
    ds: &DatasetArgs,
) -> Result<(), String> {
    let info = inspect_streaming_dataset(input, ds)?;
    let d = info.layout.dims;
    let block_depth = if opts.block_depth == 0 {
        DEFAULT_BLOCK_DEPTH
    } else {
        opts.block_depth as usize
    };
    let chunk_z =
        choose_stream_chunk_depth(d, ds.memory_mib, block_depth).map_err(|e| e.to_string())?;
    let chunks_per_volume = d.z.div_ceil(chunk_z);
    let chunk_count = (d.t as u64)
        .checked_mul(d.c as u64)
        .and_then(|v| v.checked_mul(chunks_per_volume as u64))
        .ok_or("chunk count overflow")?;

    let mut planes = PlaneReader::open(input, &info)?;
    let file = File::create(output).map_err(|e| e.to_string())?;
    let mut out = BufWriter::new(file);
    stream_header(&mut out, STREAM_MAGIC_LOSSLESS, d, chunk_z, chunk_count)?;

    let start = Instant::now();
    let mut raw_bytes = 0usize;
    let mut payload_bytes = 0usize;
    let mut chunks_written = 0u64;
    let plane_samples = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
    let mut chunk = Vec::<u16>::with_capacity(
        plane_samples
            .checked_mul(chunk_z)
            .ok_or("chunk size overflow")?,
    );

    for t in 0..d.t {
        for c in 0..d.c {
            let mut z0 = 0usize;
            while z0 < d.z {
                let depth = (d.z - z0).min(chunk_z);
                chunk.clear();
                for z in z0..z0 + depth {
                    let canon = (t * d.c + c) * d.z + z;
                    let src = info.canonical_to_source[canon];
                    let plane = planes.read_plane(src)?;
                    chunk.extend_from_slice(&plane);
                }
                let cd = Dims {
                    x: d.x,
                    y: d.y,
                    z: depth,
                };
                let (payload, _st) =
                    compress_u16_with_stats(&chunk, cd, opts).map_err(|e| e.to_string())?;
                write_u32_le(&mut out, t as u32)?;
                write_u32_le(&mut out, c as u32)?;
                write_u32_le(&mut out, z0 as u32)?;
                write_u32_le(&mut out, depth as u32)?;
                write_u64_le(&mut out, payload.len() as u64)?;
                out.write_all(&payload).map_err(|e| e.to_string())?;
                raw_bytes += chunk.len() * 2;
                payload_bytes += payload.len();
                chunks_written += 1;
                z0 += depth;
            }
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    let metadata = tiff_metadata::capture(input, &info.layout)?;
    radelta_native::set_file_metadata(output, &metadata).map_err(|e| e.to_string())?;
    let total = fs::metadata(output).map_err(|e| e.to_string())?.len() as usize;
    let secs = start.elapsed().as_secs_f64();
    println!("Wrote {} ({:.3} MiB)", output.display(), mib(total));
    println!(
        "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
        d.t, d.c, d.z, d.y, d.x
    );
    println!(
        "Out-of-core chunks: {} (up to {} Z planes each)",
        chunks_written, chunk_z
    );
    println!("RAM budget:       {} MiB", ds.memory_mib);
    println!("Payload bytes:    {:.3} MiB", mib(payload_bytes));
    println!(
        "Compression ratio:{:.2}:1",
        raw_bytes as f64 / total.max(1) as f64
    );
    println!(
        "Encode rate:      {:.3} GB/s (TIFF I/O included)",
        gb_per_s(raw_bytes, secs)
    );
    Ok(())
}

fn encode_streaming_lossy(
    input: &Path,
    output: &Path,
    opts: LossyOptions,
    ds: &DatasetArgs,
) -> Result<(), String> {
    let info = inspect_streaming_dataset(input, ds)?;
    let d = info.layout.dims;
    let block_depth = if opts.block_depth == 0 {
        DEFAULT_BLOCK_DEPTH
    } else {
        opts.block_depth as usize
    };
    let chunk_z =
        choose_stream_chunk_depth(d, ds.memory_mib, block_depth).map_err(|e| e.to_string())?;
    let chunks_per_volume = d.z.div_ceil(chunk_z);
    let chunk_count = (d.t as u64)
        .checked_mul(d.c as u64)
        .and_then(|v| v.checked_mul(chunks_per_volume as u64))
        .ok_or("chunk count overflow")?;

    let mut planes = PlaneReader::open(input, &info)?;
    let file = File::create(output).map_err(|e| e.to_string())?;
    let mut out = BufWriter::new(file);
    stream_header(&mut out, STREAM_MAGIC_LOSSY, d, chunk_z, chunk_count)?;

    let start = Instant::now();
    let mut raw_bytes = 0usize;
    let mut chunks_written = 0u64;
    let plane_samples = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
    let mut chunk = Vec::<u16>::with_capacity(
        plane_samples
            .checked_mul(chunk_z)
            .ok_or("chunk size overflow")?,
    );

    for t in 0..d.t {
        for c in 0..d.c {
            let mut z0 = 0usize;
            while z0 < d.z {
                let depth = (d.z - z0).min(chunk_z);
                chunk.clear();
                for z in z0..z0 + depth {
                    let canon = (t * d.c + c) * d.z + z;
                    let src = info.canonical_to_source[canon];
                    let plane = planes.read_plane(src)?;
                    chunk.extend_from_slice(&plane);
                }
                let cd = Dims {
                    x: d.x,
                    y: d.y,
                    z: depth,
                };
                let (payload, _st) =
                    compress_lossy_u16_with_stats(&chunk, cd, opts).map_err(|e| e.to_string())?;
                write_u32_le(&mut out, t as u32)?;
                write_u32_le(&mut out, c as u32)?;
                write_u32_le(&mut out, z0 as u32)?;
                write_u32_le(&mut out, depth as u32)?;
                write_u64_le(&mut out, payload.len() as u64)?;
                out.write_all(&payload).map_err(|e| e.to_string())?;
                raw_bytes += chunk.len() * 2;
                chunks_written += 1;
                z0 += depth;
            }
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    let metadata = tiff_metadata::capture(input, &info.layout)?;
    radelta_native::set_file_metadata(output, &metadata).map_err(|e| e.to_string())?;
    let total = fs::metadata(output).map_err(|e| e.to_string())?.len() as usize;
    let secs = start.elapsed().as_secs_f64();
    println!("Wrote {} ({:.3} MiB)", output.display(), mib(total));
    println!(
        "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
        d.t, d.c, d.z, d.y, d.x
    );
    println!(
        "Out-of-core chunks: {} (up to {} Z planes each)",
        chunks_written, chunk_z
    );
    println!("RAM budget:       {} MiB", ds.memory_mib);
    println!(
        "Compression ratio:{:.2}:1",
        raw_bytes as f64 / total.max(1) as f64
    );
    println!(
        "Encode rate:      {:.3} GB/s (TIFF I/O included)",
        gb_per_s(raw_bytes, secs)
    );
    println!(
        "offset={:.6} ADU, gain={:.6} e-/ADU, noise-step={:.6}",
        opts.offset_adu, opts.gain_e_per_adu, opts.noise_step
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
    encoded: &mut Vec<u8>,
    layout: &ResolvedLayout,
) -> Result<(), String> {
    let metadata = tiff_metadata::capture(input, layout)?;
    radelta_native::set_metadata(encoded, &metadata).map_err(|e| e.to_string())?;
    fs::write(output, encoded).map_err(|e| e.to_string())
}

fn load_dataset(path: &Path, ds: &DatasetArgs) -> Result<(Vec<u16>, ResolvedLayout), String> {
    let (raw, x, y, pages, description) = read_tiff_pages_u16(path)?;
    let inferred = if ds.ignore_metadata {
        None
    } else {
        infer_layout_from_description(description.as_deref(), x, y, pages)?
    };
    let layout = resolve_layout(x, y, pages, ds, inferred)?;
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

fn benchmark_args(args: &[String]) -> Result<(Vec<String>, usize), String> {
    let mut codec = Vec::new();
    let mut repeats = 3;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--repeats" {
            i += 1;
            repeats = args
                .get(i)
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .ok_or("--repeats requires a positive integer")?;
        } else if args[i] == "--stream" || args[i] == "--memory-mib" {
            return Err("benchmarks run in memory; streaming options are not supported".into());
        } else {
            codec.push(args[i].clone());
        }
        i += 1;
    }
    Ok((codec, repeats))
}

fn print_shape(layout: &ResolvedLayout) {
    let d = layout.dims;
    println!(
        "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
        d.t, d.c, d.z, d.y, d.x
    );
    println!("Layout source:   {}", layout.source);
    if layout.ifd_coords.is_some() {
        println!("Input mapping:   explicit OME TiffData IFD mapping");
    } else {
        println!(
            "Input page order: {} (slowest -> fastest)",
            layout.page_order
        );
    }
}

fn benchmark(path: &Path, opts: Options, ds: DatasetArgs, repeats: usize) -> Result<(), String> {
    let io0 = Instant::now();
    let (data, layout) = load_dataset(path, &ds)?;
    let dims = layout.dims;
    println!("Input:           {}", path.display());
    println!("Codec options:   {opts:?}");
    println!(
        "Timing:          median of {repeats} runs after warm-up, {} workers",
        rayon::current_num_threads()
    );
    print_shape(&layout);
    println!("Raw bytes:       {:.3} MiB", mib(data.len() * 2));
    println!("TIFF read/map:   {:.3} s", io0.elapsed().as_secs_f64());
    println!();

    let ((encoded, stats), encode_time) = measure(repeats, || {
        compress_u16_nd_with_stats(&data, dims, opts).map_err(|e| e.to_string())
    })?;
    println!("Lossless compression:");
    println!("  volumes (T*C): {}", stats.volumes);
    println!("  q max:         {}", stats.q_max);
    println!("  r max:         {}", stats.r_max);
    println!("  blocks:        {}", stats.blocks);
    println!("  q stream:      {:.3} MiB", mib(stats.q_stream_bytes));
    println!("  r stream:      {:.3} MiB", mib(stats.r_stream_bytes));
    println!("  model tables:  {:.3} MiB", mib(stats.model_bytes));
    println!("  total file:    {:.3} MiB", mib(stats.total_bytes));
    println!(
        "  ratio:         {:.2}:1",
        stats.raw_bytes as f64 / stats.total_bytes as f64
    );
    println!("  encode time:   {:.4} s", encode_time);
    println!(
        "  encode rate:   {:.3} GB/s",
        gb_per_s(stats.raw_bytes, encode_time)
    );

    let mut decoded = vec![0u16; data.len()];
    let (dd, ds_time) = measure(repeats, || {
        decompress_u16_nd_into(&encoded, &mut decoded).map_err(|e| e.to_string())
    })?;
    if dd != dims || decoded != data {
        return Err("round-trip verification FAILED".into());
    }
    println!();
    println!("Decompression:");
    println!("  decode time:   {:.4} s", ds_time);
    println!(
        "  decode rate:   {:.3} GB/s",
        gb_per_s(stats.raw_bytes, ds_time)
    );
    println!("  exact:         yes");
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
    println!("Input:           {}", path.display());
    println!("Codec options:   {opts:?}");
    println!(
        "Timing:          median of {repeats} runs after warm-up, {} workers",
        rayon::current_num_threads()
    );
    print_shape(&layout);
    println!("Raw bytes:       {:.3} MiB", mib(data.len() * 2));
    println!("TIFF read/map:   {:.3} s", io0.elapsed().as_secs_f64());
    println!();
    println!("Lossy calibration:");
    println!("  offset:        {:.6} ADU", opts.offset_adu);
    println!("  gain:          {:.6} e-/ADU", opts.gain_e_per_adu);
    println!("  noise step:    {:.6}", opts.noise_step);
    println!(
        "  quant RMS*:    {:.4} sigma",
        opts.noise_step / 12.0f64.sqrt()
    );
    println!();

    let ((encoded, stats), encode_time) = measure(repeats, || {
        compress_lossy_u16_nd_with_stats(&data, dims, opts).map_err(|e| e.to_string())
    })?;
    println!("Lossy compression:");
    println!("  volumes (T*C): {}", stats.volumes);
    println!("  q max:         {}", stats.q_max);
    println!("  blocks:        {}", stats.blocks);
    println!("  q stream:      {:.3} MiB", mib(stats.q_stream_bytes));
    println!("  model tables:  {:.3} MiB", mib(stats.model_bytes));
    println!("  total file:    {:.3} MiB", mib(stats.total_bytes));
    println!(
        "  ratio:         {:.2}:1",
        stats.raw_bytes as f64 / stats.total_bytes as f64
    );
    println!("  encode time:   {:.4} s", encode_time);
    println!(
        "  encode rate:   {:.3} GB/s",
        gb_per_s(stats.raw_bytes, encode_time)
    );

    let mut decoded = vec![0u16; data.len()];
    let (dd, ds_time) = measure(repeats, || {
        decompress_u16_nd_into(&encoded, &mut decoded).map_err(|e| e.to_string())
    })?;
    if dd != dims {
        return Err("lossy decoded dimensions do not match".into());
    }
    let (rmse, mae, max_abs, rmse_e) = error_stats(&data, &decoded, opts);
    println!();
    println!("Decompression:");
    println!("  decode time:   {:.4} s", ds_time);
    println!(
        "  decode rate:   {:.3} GB/s",
        gb_per_s(stats.raw_bytes, ds_time)
    );
    println!("  RMSE:          {:.4} ADU", rmse);
    println!("  MAE:           {:.4} ADU", mae);
    println!("  max abs error: {} ADU", max_abs);
    println!("  RMSE signal:   {:.4} electrons", rmse_e);
    Ok(())
}

fn parse_decode_page_order(args: &[String]) -> String {
    if args.is_empty() {
        return "TCZ".to_string();
    }
    if args.len() == 2 && args[0] == "--page-order" {
        let o = args[1].to_ascii_uppercase();
        validate_page_order(&o).unwrap_or_else(|_| usage());
        o
    } else {
        usage()
    }
}

fn configure_metadata_limit(args: &mut Vec<String>) -> Result<(), String> {
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--metadata-limit-mib" {
            let bytes = args
                .get(i + 1)
                .and_then(|s| s.parse::<usize>().ok())
                .and_then(|mib| mib.checked_mul(1024 * 1024))
                .ok_or("--metadata-limit-mib requires a nonnegative integer that fits in bytes")?;
            radelta_native::set_metadata_limit(bytes);
            args.drain(i..i + 2);
        } else {
            i += 1;
        }
    }
    Ok(())
}

fn tiff_limits() -> tiff::decoder::Limits {
    let mut limits = tiff::decoder::Limits::default();
    limits.ifd_value_size = radelta_native::metadata_limit();
    limits
}

fn main() {
    let mut args: Vec<String> = env::args().collect();
    if let Err(e) = configure_metadata_limit(&mut args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    if args.len() < 3 {
        usage();
    }
    let result = match args[1].as_str() {
        "benchmark" | "benchmark-lossy" => {
            benchmark_args(&args[3..]).and_then(|(codec_args, repeats)| {
                if args[1] == "benchmark" {
                    let (opts, ds) = parse_lossless_args(&codec_args);
                    benchmark(Path::new(&args[2]), opts, ds, repeats)
                } else {
                    let (opts, ds) = parse_lossy_args(&codec_args);
                    benchmark_lossy(Path::new(&args[2]), opts, ds, repeats)
                }
            })
        }
        "encode" => {
            if args.len() < 4 {
                usage();
            }
            let (opts, ds) = parse_lossless_args(&args[4..]);
            let input = Path::new(&args[2]);
            let output = Path::new(&args[3]);
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
                            let use_stream =
                                ds.force_stream || raw_bytes.saturating_mul(4) > budget;
                            if use_stream {
                                println!("Using out-of-core RDS3 encoding (estimated in-memory working set exceeds {} MiB).", ds.memory_mib);
                                encode_streaming_lossless(input, output, opts, &ds)
                            } else {
                                match load_dataset(input, &ds) {
                                    Ok((data, layout)) => {
                                        let dims = layout.dims;
                                        match compress_u16_nd_with_stats(&data, dims, opts) {
                                            Ok((mut encoded, stats)) => {
                                                if let Err(e) = write_encoded_with_metadata(
                                                    input,
                                                    output,
                                                    &mut encoded,
                                                    &layout,
                                                ) {
                                                    Err(e.to_string())
                                                } else {
                                                    println!(
                                                        "Wrote {} ({:.3} MiB)",
                                                        output.display(),
                                                        mib(encoded.len())
                                                    );
                                                    println!(
                                                        "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
                                                        dims.t, dims.c, dims.z, dims.y, dims.x
                                                    );
                                                    println!("Layout source: {}", layout.source);
                                                    println!(
                                                        "Lossless encode rate: {:.3} GB/s",
                                                        gb_per_s(
                                                            stats.raw_bytes,
                                                            stats.seconds_total
                                                        )
                                                    );
                                                    Ok(())
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
        "encode-lossy" => {
            if args.len() < 4 {
                usage();
            }
            let (opts, ds) = parse_lossy_args(&args[4..]);
            let input = Path::new(&args[2]);
            let output = Path::new(&args[3]);
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
                            let use_stream =
                                ds.force_stream || raw_bytes.saturating_mul(4) > budget;
                            if use_stream {
                                println!("Using out-of-core RQS3 encoding (estimated in-memory working set exceeds {} MiB).", ds.memory_mib);
                                encode_streaming_lossy(input, output, opts, &ds)
                            } else {
                                match load_dataset(input, &ds) {
                                    Ok((data, layout)) => {
                                        let dims = layout.dims;
                                        match compress_lossy_u16_nd_with_stats(&data, dims, opts) {
                                            Ok((mut encoded, stats)) => {
                                                if let Err(e) = write_encoded_with_metadata(
                                                    input,
                                                    output,
                                                    &mut encoded,
                                                    &layout,
                                                ) {
                                                    Err(e.to_string())
                                                } else {
                                                    println!(
                                                        "Wrote {} ({:.3} MiB)",
                                                        output.display(),
                                                        mib(encoded.len())
                                                    );
                                                    println!(
                                                        "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
                                                        dims.t, dims.c, dims.z, dims.y, dims.x
                                                    );
                                                    println!("Layout source: {}", layout.source);
                                                    println!(
                                                        "Lossy encode rate: {:.3} GB/s",
                                                        gb_per_s(
                                                            stats.raw_bytes,
                                                            stats.seconds_total
                                                        )
                                                    );
                                                    println!("offset={:.6} ADU, gain={:.6} e-/ADU, noise-step={:.6}", opts.offset_adu, opts.gain_e_per_adu, opts.noise_step);
                                                    Ok(())
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
        "decode" => {
            if args.len() < 4 {
                usage();
            }
            let order = if args.len() == 4 {
                None
            } else {
                Some(parse_decode_page_order(&args[4..]))
            };
            let input = Path::new(&args[2]);
            let output = Path::new(&args[3]);
            radelta_native::RadeltaFileHandle::open(input).and_then(|file| {
                let info = file.info();
                let d = Dims5 {
                    x: info.nx as usize,
                    y: info.ny as usize,
                    z: info.nz as usize,
                    c: info.nc as usize,
                    t: info.nt as usize,
                };
                tiff_metadata::write(
                    output,
                    d,
                    order.as_deref(),
                    file.metadata(),
                    |canonical, pixels| {
                        let z = canonical % d.z;
                        let c = (canonical / d.z) % d.c;
                        let t = canonical / (d.z * d.c);
                        file.read_plane(t, c, z, pixels)
                    },
                )?;
                println!("Wrote {}", output.display());
                println!(
                    "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
                    d.t, d.c, d.z, d.y, d.x
                );
                println!("Metadata: {} bytes", file.metadata().len());
                Ok(())
            })
        }
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
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
        let m = infer_ome_layout(d, 10, 7, 12).unwrap().unwrap();
        assert_eq!((m.t, m.c, m.z), (2, 2, 3));
        assert_eq!(m.page_order, "CTZ");
        assert!(m.ifd_coords.is_none());
    }

    #[test]
    fn parses_ome_explicit_reverse_time_mapping() {
        let d = r#"<OME><Image><Pixels DimensionOrder="XYZTC" SizeX="10" SizeY="7" SizeZ="1" SizeC="1" SizeT="3"><TiffData IFD="0" FirstT="2"/><TiffData IFD="1" FirstT="1"/><TiffData IFD="2" FirstT="0"/></Pixels></Image></OME>"#;
        let m = infer_ome_layout(d, 10, 7, 3).unwrap().unwrap();
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
