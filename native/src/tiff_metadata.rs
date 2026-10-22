//! TIFF adapter for the opaque metadata payload. Pixel layout tags are rebuilt;
//! descriptive values retain their TIFF type and are stored little-endian.
use super::*;
use std::borrow::Cow;
use tiff::decoder::IfdDecoder;
use tiff::encoder::{DirectoryEncoder, TiffValue};
use tiff::tags::{ByteOrder, IfdPointer, Type, ValueBuffer};
const SIGNATURE: &[u8; 8] = b"RDTIFF01";
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
fn resolve_children(
    dec: &mut Decoder<BufReader<File>>,
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
    let mut multisample = false;
    let mut budget = limit - out.len() - 8;
    loop {
        let samples = page_samples(&mut dec)?;
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
        if !bytes.starts_with(SIGNATURE) {
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
        if !rd.bytes.is_empty() {
            return Err("trailing TIFF metadata bytes".into());
        }
        Ok(Some(Self {
            mapping,
            tags,
            layout_valid,
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
    Ok(out)
}
/// Write both memory and streaming datasets through the same metadata-aware TIFF path.
pub fn write(
    path: &Path,
    d: Dims5,
    order: Option<&str>,
    metadata: &[u8],
    mut plane_reader: impl FnMut(usize, &mut [u16]) -> Result<(), String>,
) -> Result<(), String> {
    d.voxels().map_err(|e| e.to_string())?;
    let saved = TiffMetadata::parse(metadata, d)?;
    let pages = d.t * d.c * d.z;
    let mapping = if let (None, Some(saved)) = (order, saved.as_ref()) {
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
    };
    let preserve = saved
        .as_ref()
        .is_some_and(|m| m.layout_valid && m.mapping == mapping);
    if let Some(m) = saved
        .as_ref()
        .filter(|m| preserve && m.imagej_description())
    {
        return write_imagej_layout(path, d, &mapping, m, &mut plane_reader);
    }
    let size = (d.voxels().map_err(|e| e.to_string())? as u64)
        .checked_mul(2)
        .and_then(|v| v.checked_add(metadata.len() as u64))
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
            &mut plane_reader,
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
            &mut plane_reader,
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
    read: &mut impl FnMut(usize, &mut [u16]) -> Result<(), String>,
) -> Result<(), String> {
    let (ifds, data_start) = imagej_header_plan(d, m, mapping.len())?;
    let plane_len = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
    let mut plane = Vec::new();
    plane
        .try_reserve_exact(plane_len)
        .map_err(|_| format!("cannot allocate a {}x{} plane buffer", d.x, d.y))?;
    plane.resize(plane_len, 0u16);
    let mut out =
        BufWriter::with_capacity(1024 * 1024, File::create(path).map_err(|e| e.to_string())?);
    if write_imagej_prefix(&mut out, d, m, ifds, data_start)? != data_start {
        return Err("ImageJ TIFF prefix size changed while resolving strip offsets".into());
    }
    for &canonical in mapping {
        read(canonical, &mut plane)?;
        // TiffEncoder writes a native-endian header; the initialized u16
        // storage has the same byte order and needs no per-pixel conversion.
        out.write_all(tiff_io::u16_bytes_mut(&mut plane))
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
    read: &mut impl FnMut(usize, &mut [u16]) -> Result<(), String>,
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
    let mut plane: Vec<u16> = Vec::new();
    plane
        .try_reserve_exact(plane_len)
        .map_err(|_| format!("cannot allocate a {}x{} plane buffer", d.x, d.y))?;
    plane.resize(plane_len, 0);
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
        read(canonical, &mut plane)?;
        let source_page = inverse.as_ref().map(|v| v[canonical]);
        let tags = if let (Some(m), Some(p)) = (saved, source_page) {
            prepare(enc, m.page_tags(p))?
        } else {
            Vec::new()
        };
        let mut image = enc
            .new_image::<colortype::Gray16>(d.x as u32, d.y as u32)
            .map_err(|e| e.to_string())?;
        // Output is uncompressed and already held in a plane buffer. One
        // strip per plane avoids repeated strip dispatch and offset tables.
        image
            .rows_per_strip(d.y as u32)
            .map_err(|e| e.to_string())?;
        for tag in &tags {
            if !structural(tag.id) && (tag.id != 270 || preserve) {
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
        image.write_data(&plane).map_err(|e| e.to_string())?;
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
}
