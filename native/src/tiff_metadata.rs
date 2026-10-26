//! TIFF adapter for the opaque metadata payload. Pixel layout tags are rebuilt;
//! descriptive values retain their TIFF type and are stored little-endian.
use super::*;
use std::borrow::Cow;
use std::io::Read;
use tiff::decoder::IfdDecoder;
use tiff::encoder::{DirectoryEncoder, TiffValue};
use tiff::tags::{ByteOrder, IfdPointer, Type, ValueBuffer};
const SIGNATURE: &[u8; 8] = b"RDTIFF01";
const PYRAMID_SIGNATURE: &[u8; 8] = b"RDTIFF02";
mod pyramids;
pub const ARCHIVE_TAG: u16 = 65000;
#[derive(Clone, Debug)]
struct SavedTag {
    id: u16,
    ty: u16,
    bytes: Vec<u8>,
    children: Vec<Vec<SavedTag>>,
}
#[derive(Default)]
pub struct TiffMetadata {
    pub mapping: Vec<usize>,
    tags: Vec<Vec<SavedTag>>,
    pub layout_valid: bool,
    pyramids: Vec<Vec<pyramids::Image>>,
}
fn structural(id: u16) -> bool {
    matches!(id, 254..=259 | 263..=266 | 273 | 277..=281 | 284 | 288..=293 | 317 | 321..=325 | 330 | 338..=339 | 347 | 512..=521 | 529..=532)
}
fn pointer(id: u16, ty: u16) -> bool {
    matches!(id, 34665 | 34853 | 40965) || matches!(ty, 13 | 18)
}
fn read_tags(
    ifd: &mut IfdDecoder<'_>,
    image: bool,
    budget: &mut usize,
) -> Result<Vec<SavedTag>, String> {
    *budget = budget
        .checked_sub(8)
        .ok_or("TIFF metadata exceeds configured byte limit")?;
    let mut tags = Vec::new();
    // Inspect entries without interpreting values as strings (vendor ASCII
    // fields sometimes contain arbitrary bytes). No pixel strips are read.
    // Enumerate the actual raw entries, including private tags, without
    // decoding structural pixel tags or interpreting arbitrary vendor ASCII.
    let entries: Vec<_> = ifd
        .directory()
        .iter()
        .filter(|(tag, _)| !image || !structural(tag.to_u16()))
        .map(|(tag, entry)| (tag, entry.clone()))
        .collect();
    for (tag, entry) in entries {
        let id = tag.to_u16();
        let ty = entry.field_type();
        let width = match ty.to_u16() {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 | 16 | 17 | 18 => 8,
            _ => return Err("unsupported TIFF field type".into()),
        };
        let len = usize::try_from(
            entry
                .count()
                .checked_mul(width)
                .ok_or("TIFF metadata length overflow")?,
        )
        .map_err(|_| "TIFF metadata length overflow")?;
        *budget = budget
            .checked_sub(len.checked_add(20).ok_or("TIFF metadata length overflow")?)
            .ok_or("TIFF metadata exceeds configured byte limit")?;
        let mut value = ValueBuffer::empty(ty);
        ifd.find_tag_buf(tag, &mut value)
            .map_err(|e| format!("TIFF metadata tag {id}: {e}"))?;
        value.set_byte_order(ByteOrder::LittleEndian);
        tags.push(SavedTag {
            id,
            ty: ty.to_u16(),
            bytes: value.as_bytes().to_vec(),
            children: Vec::new(),
        });
    }
    Ok(tags)
}
fn resolve_children<R: Read + Seek>(
    dec: &mut Decoder<R>,
    tags: &mut [SavedTag],
    depth: usize,
    budget: &mut usize,
) -> Result<(), String> {
    if depth > 16 {
        return Err("TIFF metadata directory nesting exceeds 16".into());
    }
    for tag in tags {
        if !pointer(tag.id, tag.ty) {
            continue;
        }
        let width = if matches!(tag.ty, 16 | 18) { 8 } else { 4 };
        if !tag.bytes.len().is_multiple_of(width) {
            return Err("invalid TIFF metadata directory pointer".into());
        }
        for bytes in tag.bytes.chunks_exact(width) {
            let offset = if width == 8 {
                u64::from_le_bytes(bytes.try_into().unwrap())
            } else {
                u32::from_le_bytes(bytes.try_into().unwrap()) as u64
            };
            if offset == 0 {
                tag.children.push(Vec::new());
                continue;
            }
            let directory = dec
                .read_directory(IfdPointer(offset))
                .map_err(|e| e.to_string())?;
            let mut child = read_tags(&mut dec.read_directory_tags(&directory), false, budget)?;
            resolve_children(dec, &mut child, depth + 1, budget)?;
            tag.children.push(child);
        }
        tag.bytes.clear();
    }
    Ok(())
}
/// Serialized size of a tag list, as written by `save_tags`.
fn saved_len(tags: &[SavedTag]) -> usize {
    8 + tags
        .iter()
        .map(|tag| 20 + tag.bytes.len() + tag.children.iter().map(|c| saved_len(c)).sum::<usize>())
        .sum::<usize>()
}
fn put(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u64).to_le_bytes());
}
fn save_tags(out: &mut Vec<u8>, tags: &[SavedTag]) {
    put(out, tags.len());
    for tag in tags {
        out.extend_from_slice(&tag.id.to_le_bytes());
        out.extend_from_slice(&tag.ty.to_le_bytes());
        put(out, tag.bytes.len());
        out.extend_from_slice(&tag.bytes);
        put(out, tag.children.len());
        for child in &tag.children {
            save_tags(out, child);
        }
    }
}
pub fn capture(path: &Path, layout: &ResolvedLayout) -> Result<Vec<u8>, String> {
    let d = layout.dims;
    d.voxels().map_err(|e| e.to_string())?;
    let pages = d.t * d.c * d.z;
    let limit = radelta_native::metadata_limit();
    if limit < 25 || pages > (limit - 25) / 8 {
        return Err("TIFF metadata plane mapping exceeds limit".into());
    }
    let mut dec = Decoder::new(BufReader::new(File::open(path).map_err(|e| e.to_string())?))
        .map_err(|e| e.to_string())?
        .with_limits(tiff_limits());
    let desc = dec.get_tag_ascii_string(Tag::ImageDescription).ok();
    let inferred =
        infer_layout_from_description(desc.as_deref(), d.x, d.y, pages, path).unwrap_or(None);
    let valid = match inferred {
        Some(ref v) => {
            v.t == d.t
                && v.c == d.c
                && v.z == d.z
                && v.page_order == layout.page_order
                && v.ifd_coords == layout.ifd_coords
        }
        None => d.c == 1 && d.t == 1,
    };
    let mut out = SIGNATURE.to_vec();
    out.push(u8::from(valid));
    put(&mut out, pages);
    for page in 0..pages {
        let (t, c, z) = layout
            .ifd_coords
            .as_ref()
            .map(|v| v[page])
            .unwrap_or_else(|| coords_from_page_index(page, &layout.page_order, d));
        put(&mut out, (t * d.c + c) * d.z + z);
    }
    let mut all = Vec::new();
    let mut pyramid_images = Vec::new();
    let mut multisample = false;
    let mut budget = limit - out.len() - 8;
    loop {
        let samples = page_samples(&mut dec)?;
        let levels = pyramids::capture(path, &mut dec, &mut budget)?;
        if !levels.is_empty()
            && (samples > 1
                || desc
                    .as_deref()
                    .is_some_and(|d| d.trim_start().starts_with("ImageJ=")))
        {
            return Err("SubIFDs on planar multisample or ImageJ contiguous stacks are not supported; refusing to discard pyramid pixels".into());
        }
        pyramid_images.push(levels);
        let mut tags = read_tags(&mut dec.image_ifd(), true, &mut budget)?;
        resolve_children(&mut dec, &mut tags, 0, &mut budget)?;
        if samples > 1 {
            // Each planar sample becomes its own single-sample page. Drop the
            // tags whose values describe the sample set as a whole.
            multisample = true;
            tags.retain(|tag| !matches!(tag.id, 262 | 340 | 341));
            for _ in 1..samples {
                let copy_bytes = saved_len(&tags);
                budget = budget
                    .checked_sub(copy_bytes)
                    .ok_or("TIFF metadata exceeds configured byte limit")?;
                all.push(tags.clone());
            }
        }
        all.push(tags);
        if !dec.more_images() {
            break;
        }
        dec.next_image().map_err(|e| e.to_string())?;
    }
    if multisample {
        // Source descriptions refer to multisample pages; decoding writes a
        // generated description and archives the original metadata instead.
        out[SIGNATURE.len()] = 0;
    }
    put(&mut out, all.len());
    for tags in &all {
        save_tags(&mut out, tags);
    }
    if pyramid_images.iter().any(|images| !images.is_empty()) {
        let extra = pyramid_images
            .len()
            .checked_mul(8)
            .and_then(|n| n.checked_add(8))
            .ok_or("pyramid count overflow")?;
        if extra > budget {
            return Err("TIFF pyramid metadata exceeds --metadata-limit-mib".into());
        }
        out[..8].copy_from_slice(PYRAMID_SIGNATURE);
        put(&mut out, pyramid_images.len());
        for images in &pyramid_images {
            put(&mut out, images.len());
            for image in images {
                image.save(&mut out);
            }
        }
    }
    if out.len() > limit {
        return Err("TIFF metadata exceeds configured byte limit".into());
    }
    Ok(out)
}
struct Reader<'a> {
    bytes: &'a [u8],
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.bytes.len() {
            return Err("truncated TIFF metadata".into());
        }
        let (v, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(v)
    }
    fn n(&mut self) -> Result<usize, String> {
        usize::try_from(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
            .map_err(|_| "TIFF metadata count overflow".into())
    }
    fn tags(&mut self, depth: usize) -> Result<Vec<SavedTag>, String> {
        if depth > 16 {
            return Err("TIFF metadata nesting exceeds 16".into());
        }
        let count = self.n()?;
        if count > self.bytes.len() / 20 {
            return Err("invalid TIFF metadata tag count".into());
        }
        let mut tags = Vec::new();
        for _ in 0..count {
            let id = u16::from_le_bytes(self.take(2)?.try_into().unwrap());
            let ty = u16::from_le_bytes(self.take(2)?.try_into().unwrap());
            let n = self.n()?;
            let bytes = self.take(n)?.to_vec();
            let n = self.n()?;
            if n > self.bytes.len() / 8 {
                return Err("invalid TIFF child count".into());
            }
            let mut children = Vec::new();
            for _ in 0..n {
                children.push(self.tags(depth + 1)?);
            }
            tags.push(SavedTag {
                id,
                ty,
                bytes,
                children,
            });
        }
        Ok(tags)
    }
}
impl TiffMetadata {
    pub fn parse(bytes: &[u8], d: Dims5) -> Result<Option<Self>, String> {
        let has_pyramids = bytes.starts_with(PYRAMID_SIGNATURE);
        if !bytes.starts_with(SIGNATURE) && !has_pyramids {
            return Ok(None);
        }
        let mut rd = Reader { bytes: &bytes[8..] };
        let layout_valid = rd.take(1)?[0] != 0;
        let n = rd.n()?;
        if n != d.t * d.c * d.z || n > rd.bytes.len() / 8 {
            return Err("TIFF metadata page count mismatch".into());
        }
        let mut mapping = Vec::with_capacity(n);
        let mut seen = vec![false; n];
        for _ in 0..n {
            let page = rd.n()?;
            if page >= n || seen[page] {
                return Err("invalid TIFF metadata plane mapping".into());
            }
            seen[page] = true;
            mapping.push(page);
        }
        let count = rd.n()?;
        if count != n && count != 1 {
            return Err("TIFF metadata IFD count mismatch".into());
        }
        let mut tags = Vec::new();
        for _ in 0..count {
            tags.push(rd.tags(0)?);
        }
        let mut pyramids = Vec::new();
        if has_pyramids {
            if rd.n()? != count {
                return Err("pyramid parent count mismatch".into());
            }
            for _ in 0..count {
                let n = rd.n()?;
                if n > rd.bytes.len() / 40 {
                    return Err("invalid pyramid image count".into());
                }
                pyramids.push(
                    (0..n)
                        .map(|_| pyramids::Image::load(&mut rd, 0))
                        .collect::<Result<_, _>>()?,
                );
            }
        }
        if !rd.bytes.is_empty() {
            return Err("trailing TIFF metadata bytes".into());
        }
        Ok(Some(Self {
            mapping,
            tags,
            layout_valid,
            pyramids,
        }))
    }
    fn page_tags(&self, page: usize) -> &[SavedTag] {
        self.tags.get(page).map(Vec::as_slice).unwrap_or(&[])
    }
    pub fn has_description(&self) -> bool {
        self.tags[0].iter().any(|t| t.id == 270)
    }
    fn imagej_description(&self) -> bool {
        self.tags[0].iter().any(|t| {
            t.id == 270
                && String::from_utf8_lossy(&t.bytes)
                    .lines()
                    .any(|line| line.trim_start().starts_with("ImageJ="))
        })
    }
    fn pyramid_bytes(&self) -> Result<u64, String> {
        let mut size = 0u64;
        for image in self.pyramids.iter().flatten() {
            size = size
                .checked_add(image.raw_bytes()?)
                .ok_or("pyramid size overflow")?;
        }
        Ok(size)
    }
}
fn write_tag<W: Write + Seek, K: TiffKind>(
    enc: &mut DirectoryEncoder<'_, W, K>,
    tag: &SavedTag,
) -> Result<(), String> {
    let mut bytes = tag.bytes.clone();
    let ty = Type::from_u16(tag.ty).ok_or("unsupported TIFF metadata type")?;
    let width = match tag.ty {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 | 16 | 17 | 18 => 8,
        _ => return Err("unsupported TIFF metadata type".into()),
    };
    if !bytes.len().is_multiple_of(width) {
        return Err("invalid TIFF metadata value length".into());
    }
    ByteOrder::LittleEndian.convert(ty, &mut bytes, ByteOrder::native());
    macro_rules! raw {
        ($kind:ident,$len:expr) => {{
            struct Raw<'a>(&'a [u8]);
            impl TiffValue for Raw<'_> {
                const BYTE_LEN: u8 = $len;
                const FIELD_TYPE: Type = Type::$kind;
                fn count(&self) -> usize {
                    self.0.len() / $len
                }
                fn data(&self) -> Cow<'_, [u8]> {
                    Cow::Borrowed(self.0)
                }
            }
            enc.write_tag(Tag::from_u16_exhaustive(tag.id), Raw(&bytes))
                .map_err(|e| e.to_string())
        }};
    }
    match tag.ty {
        1 => raw!(BYTE, 1),
        2 => raw!(ASCII, 1),
        3 => raw!(SHORT, 2),
        4 => raw!(LONG, 4),
        5 => raw!(RATIONAL, 8),
        6 => raw!(SBYTE, 1),
        7 => raw!(UNDEFINED, 1),
        8 => raw!(SSHORT, 2),
        9 => raw!(SLONG, 4),
        10 => raw!(SRATIONAL, 8),
        11 => raw!(FLOAT, 4),
        12 => raw!(DOUBLE, 8),
        13 => raw!(IFD, 4),
        16 => raw!(LONG8, 8),
        17 => raw!(SLONG8, 8),
        18 => raw!(IFD8, 8),
        _ => Err("unsupported TIFF metadata type".into()),
    }
}
fn prepare<W: Write + Seek, K: TiffKind>(
    enc: &mut TiffEncoder<W, K>,
    tags: &[SavedTag],
) -> Result<Vec<SavedTag>, String> {
    let mut out = Vec::new();
    for source in tags {
        let mut tag = SavedTag {
            id: source.id,
            ty: source.ty,
            bytes: source.bytes.clone(),
            children: Vec::new(),
        };
        if !source.children.is_empty() {
            tag.bytes.clear();
            let mut offsets = Vec::new();
            for child in &source.children {
                let child = prepare(enc, child)?;
                let mut dir = enc.extra_directory().map_err(|e| e.to_string())?;
                for tag in &child {
                    write_tag(&mut dir, tag)?;
                }
                let offset = dir
                    .finish_with_offsets()
                    .map_err(|e| e.to_string())?
                    .pointer
                    .0;
                offsets.push(offset);
            }
            tag.ty = if offsets.iter().all(|&v| v <= u32::MAX as u64) {
                13
            } else {
                18
            };
            for offset in offsets {
                if tag.ty == 18 {
                    tag.bytes.extend_from_slice(&offset.to_le_bytes());
                } else {
                    tag.bytes.extend_from_slice(&(offset as u32).to_le_bytes());
                }
            }
            tag.children.clear();
        }
        out.push(tag);
    }
    prepare_imagej_metadata(&mut out)?;
    Ok(out)
}

// IJMetadata is a BYTE value containing its own endian-sensitive header,
// UTF-16 strings and doubles. TIFF field conversion cannot handle its contents.
// Convert only the export copy; the metadata archived in .rdlt stays unchanged.
fn prepare_imagej_metadata(tags: &mut [SavedTag]) -> Result<(), String> {
    let Some(index) = tags.iter().position(|t| t.id == 50839) else {
        return Ok(());
    };
    let data = &tags[index].bytes;
    let source = match data.get(..4) {
        Some(b"IJIJ") => ByteOrder::BigEndian,
        Some(b"JIJI") => ByteOrder::LittleEndian,
        _ => return Ok(()), // Preserve unknown private payloads as opaque bytes.
    };
    if source == ByteOrder::native() {
        return Ok(());
    }
    let counts = tags
        .iter()
        .find(|t| t.id == 50838)
        .ok_or("ImageJ metadata is missing its byte-count table")?;
    let width = match counts.ty {
        3 => 2,
        4 => 4,
        16 => 8,
        _ => return Err("invalid ImageJ metadata byte-count type".into()),
    };
    if !counts.bytes.len().is_multiple_of(width) {
        return Err("invalid ImageJ metadata byte-count table".into());
    }
    let counts: Vec<usize> = counts
        .bytes
        .chunks_exact(width)
        .map(|b| {
            let mut value = [0u8; 8];
            value[..width].copy_from_slice(b);
            usize::try_from(u64::from_le_bytes(value))
                .map_err(|_| "ImageJ metadata byte count exceeds usize")
        })
        .collect::<Result<_, _>>()?;
    let converted = imagej_metadata_endian(data, &counts, source)?;
    tags[index].bytes = converted;
    Ok(())
}

fn imagej_metadata_endian(
    data: &[u8],
    counts: &[usize],
    source: ByteOrder,
) -> Result<Vec<u8>, String> {
    let header = *counts.first().ok_or("empty ImageJ metadata byte counts")?;
    if header < 4 || !(header - 4).is_multiple_of(8) || header > data.len() {
        return Err("invalid ImageJ metadata header length".into());
    }
    let mut out = data.to_vec();
    out[..4].reverse();
    let mut position = header;
    let mut entry = 1usize;
    for offset in (4..header).step_by(8) {
        let mut kind: [u8; 4] = data[offset..offset + 4].try_into().unwrap();
        if source == ByteOrder::LittleEndian {
            kind.reverse();
        }
        let n: [u8; 4] = data[offset + 4..offset + 8].try_into().unwrap();
        let n = match source {
            ByteOrder::BigEndian => u32::from_be_bytes(n),
            ByteOrder::LittleEndian => u32::from_le_bytes(n),
        } as usize;
        if n > counts.len().saturating_sub(entry) {
            return Err("ImageJ metadata entries exceed byte counts".into());
        }
        out[offset..offset + 4].reverse();
        out[offset + 4..offset + 8].reverse();
        let unit = match &kind {
            b"info" | b"labl" | b"prop" => 2, // UTF-16 code units
            b"rang" => 8,                     // f64 values
            _ => 1,                           // LUTs, ROIs, overlays and other opaque byte payloads
        };
        for &size in &counts[entry..entry + n] {
            let end = position
                .checked_add(size)
                .filter(|&end| end <= data.len())
                .ok_or("ImageJ metadata entry exceeds payload")?;
            if !size.is_multiple_of(unit) {
                return Err("invalid ImageJ metadata entry length".into());
            }
            for value in out[position..end].chunks_exact_mut(unit) {
                value.reverse();
            }
            position = end;
        }
        entry += n;
    }
    if entry != counts.len() || position != data.len() {
        return Err("ImageJ metadata byte counts do not cover its payload".into());
    }
    Ok(out)
}

#[cfg(test)]
mod imagej_endian_tests {
    use super::*;

    fn payload(order: ByteOrder) -> Vec<u8> {
        let big = order == ByteOrder::BigEndian;
        let mut out = if big { b"IJIJ" } else { b"JIJI" }.to_vec();
        for mut kind in [*b"info", *b"rang", *b"roi "] {
            if !big {
                kind.reverse();
            }
            out.extend_from_slice(&kind);
            out.extend_from_slice(&if big {
                1u32.to_be_bytes()
            } else {
                1u32.to_le_bytes()
            });
        }
        // U+00D8 exposes incorrect UTF-16 byte order as a surrogate error.
        out.extend_from_slice(&if big {
            216u16.to_be_bytes()
        } else {
            216u16.to_le_bytes()
        });
        out.extend_from_slice(&if big {
            1.25f64.to_be_bytes()
        } else {
            1.25f64.to_le_bytes()
        });
        out.extend_from_slice(&[12, 34, 56]); // Opaque ROI bytes must not swap.
        out
    }

    #[test]
    fn imagej_strings_doubles_and_headers_swap_but_opaque_bytes_do_not() {
        let counts = [28, 2, 8, 3];
        let big = payload(ByteOrder::BigEndian);
        let little = payload(ByteOrder::LittleEndian);
        assert_eq!(
            imagej_metadata_endian(&big, &counts, ByteOrder::BigEndian).unwrap(),
            little
        );
        assert_eq!(
            imagej_metadata_endian(&little, &counts, ByteOrder::LittleEndian).unwrap(),
            big
        );
    }

    #[test]
    fn invalid_imagej_sizes_and_counts_are_rejected() {
        let data = payload(ByteOrder::BigEndian);
        for counts in [
            vec![],
            vec![29, 2, 8, 3],
            vec![28, 2],
            vec![28, usize::MAX, 8, 3],
            vec![28, 3, 7, 3],
        ] {
            assert!(imagej_metadata_endian(&data, &counts, ByteOrder::BigEndian).is_err());
        }
    }
}
fn page_mapping(
    d: Dims5,
    order: Option<&str>,
    saved: Option<&TiffMetadata>,
) -> Result<Vec<usize>, String> {
    let pages = d.t * d.c * d.z;
    Ok(if let (None, Some(saved)) = (order, saved) {
        saved.mapping.clone()
    } else {
        let order = order.unwrap_or("TCZ");
        validate_page_order(order)?;
        (0..pages)
            .map(|p| {
                let (t, c, z) = coords_from_page_index(p, order, d);
                (t * d.c + c) * d.z + z
            })
            .collect()
    })
}

pub fn write_order(d: Dims5, order: Option<&str>, metadata: &[u8]) -> Result<Vec<usize>, String> {
    d.voxels().map_err(|e| e.to_string())?;
    let saved = TiffMetadata::parse(metadata, d)?;
    page_mapping(d, order, saved.as_ref())
}

/// A plane borrowed from either a direct reader or a reusable read-ahead batch.
/// The pixels remain valid until the next mutable access to this source.
pub trait PlaneSource {
    fn plane(&mut self, canonical: usize) -> Result<&[u16], String>;
}

/// Write both memory and streaming datasets through the same metadata-aware TIFF path.
pub fn write(
    path: &Path,
    d: Dims5,
    order: Option<&str>,
    metadata: &[u8],
    plane_reader: &mut impl PlaneSource,
) -> Result<(), String> {
    d.voxels().map_err(|e| e.to_string())?;
    let saved = TiffMetadata::parse(metadata, d)?;
    let mapping = page_mapping(d, order, saved.as_ref())?;
    let preserve = saved
        .as_ref()
        .is_some_and(|m| m.layout_valid && m.mapping == mapping);
    if let Some(m) = saved
        .as_ref()
        .filter(|m| preserve && m.imagej_description())
    {
        return write_imagej_layout(path, d, &mapping, m, plane_reader);
    }
    let pyramid_bytes = saved
        .as_ref()
        .map(|m| m.pyramid_bytes())
        .transpose()?
        .unwrap_or(0);
    let size = (d.voxels().map_err(|e| e.to_string())? as u64)
        .checked_mul(2)
        .and_then(|v| v.checked_add(metadata.len() as u64))
        .and_then(|v| v.checked_add(pyramid_bytes))
        .ok_or("TIFF size overflow")?;
    let mut writer =
        BufWriter::with_capacity(1024 * 1024, File::create(path).map_err(|e| e.to_string())?);
    if size > (u32::MAX as u64) - 64 * 1024 * 1024 {
        let mut enc =
            TiffEncoder::<_, TiffKindBig>::new_big(&mut writer).map_err(|e| e.to_string())?;
        write_pages(
            &mut enc,
            d,
            &mapping,
            saved.as_ref(),
            preserve,
            order,
            metadata,
            plane_reader,
        )?;
    } else {
        let mut enc = TiffEncoder::new(&mut writer).map_err(|e| e.to_string())?;
        write_pages(
            &mut enc,
            d,
            &mapping,
            saved.as_ref(),
            preserve,
            order,
            metadata,
            plane_reader,
        )?;
    }
    writer.flush().map_err(|e| e.to_string())
}

/// ImageJ trusts `images=N` and reads consecutive planes from the first strip
/// offset. Keep all IFDs and their values before that pixel block, including
/// nested metadata directories. Use the TIFF encoder for directory serialization
/// rather than duplicating its tag types, alignment and pointer handling.
fn write_imagej_prefix<W: Write + Seek>(
    out: &mut W,
    d: Dims5,
    m: &TiffMetadata,
    pages: usize,
    first_strip: u64,
) -> Result<u64, String> {
    let width = u32::try_from(d.x).map_err(|_| "TIFF width exceeds u32")?;
    let height = u32::try_from(d.y).map_err(|_| "TIFF height exceeds u32")?;
    let plane_bytes =
        d.x.checked_mul(d.y)
            .and_then(|n| n.checked_mul(2))
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("ImageJ TIFF plane must be smaller than 4 GiB")?;
    {
        let mut enc = TiffEncoder::new(&mut *out).map_err(|e| e.to_string())?;
        for page in 0..pages {
            let tags = prepare(&mut enc, m.page_tags(page))?;
            let offset = (page as u64)
                .checked_mul(u64::from(plane_bytes))
                .and_then(|n| first_strip.checked_add(n))
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("ImageJ TIFF strip offset exceeds u32")?;
            let mut dir = enc.image_directory().map_err(|e| e.to_string())?;
            macro_rules! tag {
                ($tag:ident, $value:expr) => {
                    dir.write_tag(Tag::$tag, $value)
                        .map_err(|e| e.to_string())?
                };
            }
            tag!(ImageWidth, width);
            tag!(ImageLength, height);
            tag!(BitsPerSample, 16u16);
            tag!(Compression, 1u16);
            tag!(PhotometricInterpretation, 1u16);
            tag!(StripOffsets, offset);
            tag!(SamplesPerPixel, 1u16);
            tag!(RowsPerStrip, height);
            tag!(StripByteCounts, plane_bytes);
            tag!(SampleFormat, 1u16);
            for tag in &tags {
                if !structural(tag.id) {
                    write_tag(&mut dir, tag)?;
                }
            }
            dir.finish().map_err(|e| e.to_string())?;
        }
    }
    let size = out.stream_position().map_err(|e| e.to_string())?;
    if size % 2 != 0 {
        out.write_all(&[0]).map_err(|e| e.to_string())?;
    }
    Ok(size + size % 2)
}

/// Measure a directory prefix without retaining it or allocating pixel storage.
/// TIFF serialization seeks back to patch links, so track both position and end.
#[derive(Default)]
struct PrefixSize {
    position: u64,
    end: u64,
}
impl Write for PrefixSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.position = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("TIFF prefix size overflow"))?;
        self.end = self.end.max(self.position);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Seek for PrefixSize {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        self.position = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => self.position.checked_add_signed(n),
            SeekFrom::End(n) => self.end.checked_add_signed(n),
        }
        .ok_or_else(|| std::io::Error::other("invalid TIFF prefix seek"))?;
        Ok(self.position)
    }
}

fn imagej_header_plan(d: Dims5, m: &TiffMetadata, pages: usize) -> Result<(usize, u64), String> {
    let data_bytes = (d.voxels().map_err(|e| e.to_string())? as u64)
        .checked_mul(2)
        .ok_or("TIFF size overflow")?;
    // Offset values have fixed TIFF types, so substituting their final values
    // does not change the prefix size. Beyond classic TIFF's address space,
    // ImageJ uses a single IFD plus the description's logical plane count.
    let mut ifds = if data_bytes > u64::from(u32::MAX) {
        1
    } else {
        pages
    };
    let mut size = write_imagej_prefix(&mut PrefixSize::default(), d, m, ifds, 0)?;
    if ifds > 1 && size.checked_add(data_bytes).ok_or("TIFF size overflow")? > u64::from(u32::MAX) {
        ifds = 1;
        size = write_imagej_prefix(&mut PrefixSize::default(), d, m, ifds, 0)?;
    }
    Ok((ifds, size))
}

fn write_imagej_layout(
    path: &Path,
    d: Dims5,
    mapping: &[usize],
    m: &TiffMetadata,
    read: &mut impl PlaneSource,
) -> Result<(), String> {
    let (ifds, data_start) = imagej_header_plan(d, m, mapping.len())?;
    let plane_len = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
    // TiffEncoder seeks repeatedly to patch IFD links. File-backed BufWriter
    // flushes on every seek, turning small directory entries into many writes.
    // Serialize small prefixes in memory, retaining the same validated encoder
    // and offsets. Large metadata keeps the bounded, file-backed path.
    let prefix = if data_start <= 8 * 1024 * 1024 {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(data_start as usize)
            .map_err(|_| "cannot allocate ImageJ TIFF directory buffer")?;
        let mut cursor = std::io::Cursor::new(bytes);
        if write_imagej_prefix(&mut cursor, d, m, ifds, data_start)? != data_start
            || cursor.get_ref().len() as u64 != data_start
        {
            return Err("ImageJ TIFF prefix size changed while resolving strip offsets".into());
        }
        Some(cursor.into_inner())
    } else {
        None
    };
    let mut out =
        BufWriter::with_capacity(1024 * 1024, File::create(path).map_err(|e| e.to_string())?);
    if let Some(prefix) = prefix {
        out.write_all(&prefix).map_err(|e| e.to_string())?;
    } else if write_imagej_prefix(&mut out, d, m, ifds, data_start)? != data_start {
        return Err("ImageJ TIFF prefix size changed while resolving strip offsets".into());
    }
    for &canonical in mapping {
        let plane = read.plane(canonical)?;
        if plane.len() != plane_len {
            return Err("TIFF plane source returned the wrong sample count".into());
        }
        // TiffEncoder writes a native-endian header; the initialized u16
        // storage has the same byte order and needs no per-pixel conversion.
        out.write_all(tiff_io::u16_bytes(plane))
            .map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
fn write_pages<W: Write + Seek, K: TiffKind>(
    enc: &mut TiffEncoder<W, K>,
    d: Dims5,
    mapping: &[usize],
    saved: Option<&TiffMetadata>,
    preserve: bool,
    order: Option<&str>,
    metadata: &[u8],
    read: &mut impl PlaneSource,
) -> Result<(), String> {
    let inverse = saved.map(|m| {
        let mut inverse = vec![0; mapping.len()];
        for (p, &canonical) in m.mapping.iter().enumerate() {
            inverse[canonical] = p;
        }
        inverse
    });
    // Fail with an error rather than aborting if a damaged header slipped
    // past validation and still describes an impossible plane.
    let plane_len = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
    let mut generated = ome_description(d, order.unwrap_or("TCZ"));
    let described_order = order.unwrap_or("TCZ");
    let regular = mapping.iter().enumerate().all(|(p, &canonical)| {
        let (t, c, z) = coords_from_page_index(p, described_order, d);
        canonical == (t * d.c + c) * d.z + z
    });
    if !regular {
        let mut entries = String::new();
        for (p, &canonical) in mapping.iter().enumerate() {
            let z = canonical % d.z;
            let c = (canonical / d.z) % d.c;
            let t = canonical / (d.z * d.c);
            entries.push_str(&format!(
                r#"<TiffData IFD="{p}" FirstT="{t}" FirstC="{c}" FirstZ="{z}" PlaneCount="1"/>"#
            ));
        }
        generated = generated.replace(
            &format!(r#"<TiffData IFD="0" PlaneCount="{}"/>"#, mapping.len()),
            &entries,
        );
    }
    for (page, &canonical) in mapping.iter().enumerate() {
        let plane = read.plane(canonical)?;
        if plane.len() != plane_len {
            return Err("TIFF plane source returned the wrong sample count".into());
        }
        let source_page = inverse.as_ref().map(|v| v[canonical]);
        let mut tags = if let (Some(m), Some(p)) = (saved, source_page) {
            prepare(enc, m.page_tags(p))?
        } else {
            Vec::new()
        };
        if let (Some(m), Some(p)) = (saved, source_page) {
            if let Some(images) = m.pyramids.get(p) {
                pyramids::append_pointer(enc, images, &mut tags)?;
            }
        }
        let mut image = enc
            .new_image::<colortype::Gray16>(d.x as u32, d.y as u32)
            .map_err(|e| e.to_string())?;
        // Output is uncompressed and already held in a plane buffer. One
        // strip per plane avoids repeated strip dispatch and offset tables.
        image
            .rows_per_strip(d.y as u32)
            .map_err(|e| e.to_string())?;
        for tag in &tags {
            if (!structural(tag.id) || tag.id == 330) && (tag.id != 270 || preserve) {
                write_tag(image.encoder(), tag)?;
            }
        }
        if page == 0 {
            if !preserve || !saved.is_some_and(|m| m.has_description()) {
                image
                    .encoder()
                    .write_tag(Tag::ImageDescription, generated.as_str())
                    .map_err(|e| e.to_string())?;
            }
            // Preserve opaque non-TIFF metadata, or the original descriptions
            // when the caller deliberately changes the logical/page layout.
            if !metadata.is_empty() && !preserve {
                image
                    .encoder()
                    .write_tag(Tag::from_u16_exhaustive(ARCHIVE_TAG), metadata)
                    .map_err(|e| e.to_string())?;
            }
        }
        image.write_data(plane).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod imagej_tests {
    use super::*;

    fn metadata(extra: usize) -> TiffMetadata {
        TiffMetadata {
            tags: vec![vec![SavedTag {
                id: 270,
                ty: 2,
                bytes: [
                    b"ImageJ=1.54\nimages=2\n".as_slice(),
                    &vec![b' '; extra],
                    &[0],
                ]
                .concat(),
                children: Vec::new(),
            }]],
            ..TiffMetadata::default()
        }
    }

    #[test]
    fn large_imagej_header_uses_one_ifd_without_allocating_pixels() {
        // First: 4 GiB of pixels. Second: pixels fit u32, but metadata pushes
        // the file beyond it. Neither may emit wrapped strip offsets.
        for (x, extra) in [(65536, 0), (65535, 70_000)] {
            let d = Dims5 {
                x,
                y: 16384,
                z: 2,
                c: 1,
                t: 1,
            };
            let m = metadata(extra);
            let (ifds, offset) = imagej_header_plan(d, &m, 2).unwrap();
            let mut cursor = std::io::Cursor::new(Vec::new());
            assert_eq!(
                write_imagej_prefix(&mut cursor, d, &m, ifds, offset).unwrap(),
                offset
            );
            let prefix = cursor.into_inner();
            let mut dec = Decoder::new(std::io::Cursor::new(&prefix)).unwrap();
            assert_eq!(
                dec.get_tag_u64(Tag::StripOffsets).unwrap(),
                prefix.len() as u64
            );
            assert_eq!(
                dec.get_tag_u64(Tag::StripByteCounts).unwrap(),
                (x * d.y * 2) as u64
            );
            assert_eq!(
                dec.get_tag_ascii_string(Tag::ImageDescription)
                    .unwrap()
                    .lines()
                    .nth(1),
                Some("images=2")
            );
            assert!(!dec.more_images());
        }
    }

    #[test]
    fn imagej_rejects_a_plane_exceeding_classic_tiff_byte_count() {
        let d = Dims5 {
            x: 65536,
            y: 32768,
            z: 2,
            c: 1,
            t: 1,
        };
        assert!(imagej_header_plan(d, &metadata(0), 2)
            .unwrap_err()
            .contains("plane"));
    }

    #[test]
    fn buffered_and_large_file_backed_imagej_prefixes_keep_pixels_and_directories_valid() {
        struct Pixels;
        impl PlaneSource for Pixels {
            fn plane(&mut self, canonical: usize) -> Result<&[u16], String> {
                Ok(if canonical == 0 { &[11, 22] } else { &[33, 44] })
            }
        }
        let d = Dims5 {
            x: 2,
            y: 1,
            z: 2,
            c: 1,
            t: 1,
        };
        for extra in [0, 8 * 1024 * 1024] {
            let m = metadata(extra);
            let path = std::env::temp_dir().join(format!(
                "radelta-imagej-prefix-{}-{extra}.tif",
                std::process::id()
            ));
            write_imagej_layout(&path, d, &[0, 1], &m, &mut Pixels).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let mut dec = Decoder::new(std::io::Cursor::new(bytes))
                .unwrap()
                .with_limits(tiff::decoder::Limits::unlimited());
            assert_eq!(
                dec.get_tag_ascii_string(Tag::ImageDescription)
                    .unwrap()
                    .len(),
                m.tags[0][0].bytes.len() - 1
            );
            let tiff::decoder::DecodingResult::U16(first) = dec.read_image().unwrap() else {
                panic!("unexpected sample type")
            };
            assert_eq!(first, [11, 22]);
            dec.next_image().unwrap();
            let tiff::decoder::DecodingResult::U16(second) = dec.read_image().unwrap() else {
                panic!("unexpected sample type")
            };
            assert_eq!(second, [33, 44]);
            assert!(!dec.more_images());
            drop(dec);
            std::fs::remove_file(path).unwrap();
        }
    }
}
