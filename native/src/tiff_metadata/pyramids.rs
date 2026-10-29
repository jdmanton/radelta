//! Lossless uint16 SubIFD images carried in the bounded TIFF metadata payload.
use super::*;
use std::collections::HashSet;

#[derive(Clone)]
pub(super) struct Image {
    x: usize,
    y: usize,
    tags: Vec<SavedTag>,
    pixels: Vec<u8>,
    children: Vec<Image>,
}

fn charge(budget: &mut usize, bytes: usize) -> Result<(), String> {
    *budget = budget.checked_sub(bytes).ok_or(
        "TIFF pyramid payload exceeds --metadata-limit-mib; increase it to preserve all levels",
    )?;
    Ok(())
}

fn offsets<R: Read + Seek>(dec: &mut Decoder<R>) -> Result<Vec<u64>, String> {
    let mut ifd = dec.image_ifd();
    let Some(entry) = ifd.find_entry(Tag::SubIfd) else {
        return Ok(Vec::new());
    };
    let ty = entry.field_type();
    let width = match ty.to_u16() {
        4 | 13 => 4,
        16 | 18 => 8,
        _ => return Err("invalid TIFF SubIFD pointer type".into()),
    };
    let mut buf = ValueBuffer::empty(ty);
    ifd.find_tag_buf(Tag::SubIfd, &mut buf)
        .map_err(|e| e.to_string())?;
    buf.set_byte_order(ByteOrder::LittleEndian);
    if !buf.as_bytes().len().is_multiple_of(width) {
        return Err("invalid TIFF SubIFD pointer table".into());
    }
    Ok(buf
        .as_bytes()
        .chunks_exact(width)
        .map(|b| {
            let mut bytes = [0; 8];
            bytes[..width].copy_from_slice(b);
            u64::from_le_bytes(bytes)
        })
        .collect())
}

// Present a SubIFD as the first image without rewriting the source or changing
// any offsets inside it. The TIFF decoder then handles tiles and compression.
struct FirstIfd {
    file: File,
    header: Vec<u8>,
}
impl FirstIfd {
    fn open(path: &Path, offset: u64) -> Result<Self, String> {
        let mut file = File::open(path).map_err(|e| e.to_string())?;
        let mut header = [0; 16];
        file.read_exact(&mut header).map_err(|e| e.to_string())?;
        let big = match &header[..2] {
            b"MM" => true,
            b"II" => false,
            _ => return Err("invalid TIFF byte order".into()),
        };
        let version = if big {
            u16::from_be_bytes(header[2..4].try_into().unwrap())
        } else {
            u16::from_le_bytes(header[2..4].try_into().unwrap())
        };
        let header = match version {
            42 => {
                let offset =
                    u32::try_from(offset).map_err(|_| "classic TIFF SubIFD offset exceeds u32")?;
                header[4..8].copy_from_slice(&if big {
                    offset.to_be_bytes()
                } else {
                    offset.to_le_bytes()
                });
                header[..8].to_vec()
            }
            43 => {
                header[8..16].copy_from_slice(&if big {
                    offset.to_be_bytes()
                } else {
                    offset.to_le_bytes()
                });
                header.to_vec()
            }
            _ => return Err("unsupported TIFF header".into()),
        };
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        Ok(Self { file, header })
    }
}
impl Read for FirstIfd {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let start = self.file.stream_position()?;
        let n = self.file.read(out)?;
        if start < self.header.len() as u64 {
            let start = start as usize;
            let count = n.min(self.header.len() - start);
            out[..count].copy_from_slice(&self.header[start..start + count]);
        }
        Ok(n)
    }
}
impl Seek for FirstIfd {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

pub(super) fn capture(
    path: &Path,
    dec: &mut Decoder<BufReader<File>>,
    budget: &mut usize,
) -> Result<Vec<Image>, String> {
    let mut ancestors = HashSet::new();
    if let Some(pointer) = dec.ifd_pointer() {
        ancestors.insert(pointer.0);
    }
    offsets(dec)?
        .into_iter()
        .map(|offset| capture_image(path, offset, budget, &mut ancestors, 0))
        .collect()
}

fn capture_image(
    path: &Path,
    offset: u64,
    budget: &mut usize,
    ancestors: &mut HashSet<u64>,
    depth: usize,
) -> Result<Image, String> {
    if depth >= 16 || offset == 0 || !ancestors.insert(offset) {
        return Err("invalid, cyclic or excessively nested TIFF SubIFDs".into());
    }
    charge(budget, 32)?;
    let mut dec = Decoder::new(BufReader::new(FirstIfd::open(path, offset)?))
        .map_err(|e| e.to_string())?
        .with_limits(tiff_limits());
    if dec.more_images() {
        return Err(
            "linked image chains inside SubIFDs are not supported; original must be retained"
                .into(),
        );
    }
    let (x, y) = dec.dimensions().map_err(|e| e.to_string())?;
    let (x, y) = (x as usize, y as usize);
    if x == 0
        || y == 0
        || dec
            .find_tag(Tag::SamplesPerPixel)
            .map_err(|e| e.to_string())?
            .map(|v| v.into_u16())
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or(1)
            != 1
        || !matches!(
            dec.colortype().map_err(|e| e.to_string())?,
            tiff::ColorType::Gray(8 | 16)
        )
        || dec
            .find_tag(Tag::PhotometricInterpretation)
            .map_err(|e| e.to_string())?
            .map(|v| v.into_u16())
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or(1)
            != 1
    {
        return Err("TIFF pyramid levels must be single-sample uint8 or uint16 grayscale; refusing to drop unsupported SubIFD pixels".into());
    }
    let flags = dec
        .find_tag(Tag::NewSubfileType)
        .map_err(|e| e.to_string())?
        .map(|v| v.into_u32())
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(1);
    let mut tags = read_tags(&mut dec.image_ifd(), true, budget)?;
    save_sample_depth(&mut dec, &mut tags, budget)?;
    resolve_children(&mut dec, &mut tags, depth + 1, budget)?;
    charge(budget, 24)?;
    tags.push(SavedTag {
        id: 254,
        ty: 4,
        bytes: flags.to_le_bytes().to_vec(),
        children: Vec::new(),
    });
    let mut pixels = pixel_buffer(x.checked_mul(y).ok_or("pyramid dimensions overflow")?)?;
    tiff_io::read_plane(&mut dec, &mut pixels)?;
    let encoded = radelta_native::compress_u16(
        &pixels,
        radelta_native::Dims { x, y, z: 1 },
        Options::default(),
    )
    .map_err(|e| e.to_string())?;
    drop(pixels);
    charge(budget, encoded.len())?;
    let children = offsets(&mut dec)?
        .into_iter()
        .map(|offset| capture_image(path, offset, budget, ancestors, depth + 1))
        .collect::<Result<_, _>>()?;
    ancestors.remove(&offset);
    Ok(Image {
        x,
        y,
        tags,
        pixels: encoded,
        children,
    })
}

impl Image {
    pub(super) fn save(&self, out: &mut Vec<u8>) {
        put(out, self.x);
        put(out, self.y);
        save_tags(out, &self.tags);
        put(out, self.pixels.len());
        out.extend_from_slice(&self.pixels);
        put(out, self.children.len());
        for child in &self.children {
            child.save(out);
        }
    }
    pub(super) fn load(rd: &mut Reader<'_>, depth: usize) -> Result<Self, String> {
        if depth >= 16 {
            return Err("excessively nested pyramid metadata".into());
        }
        let x = rd.n()?;
        let y = rd.n()?;
        if x == 0 || y == 0 || x.checked_mul(y).and_then(|n| n.checked_mul(2)).is_none() {
            return Err("invalid pyramid image dimensions".into());
        }
        let tags = rd.tags(depth)?;
        sample_depth(&tags)?;
        if tags
            .iter()
            .any(|t| structural(t.id) && !matches!(t.id, 254 | 258))
        {
            return Err("unexpected pixel-storage tags in pyramid metadata".into());
        }
        let len = rd.n()?;
        let pixels = rd.take(len)?.to_vec();
        let n = rd.n()?;
        if n > rd.bytes.len() / 40 {
            return Err("invalid pyramid child count".into());
        }
        let children = (0..n)
            .map(|_| Self::load(rd, depth + 1))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            x,
            y,
            tags,
            pixels,
            children,
        })
    }
    pub(super) fn raw_bytes(&self) -> Result<u64, String> {
        let mut size = (self.x as u64)
            .checked_mul(self.y as u64)
            .and_then(|n| n.checked_mul(2))
            .ok_or("pyramid size overflow")?;
        for child in &self.children {
            size = size
                .checked_add(child.raw_bytes()?)
                .ok_or("pyramid size overflow")?;
        }
        Ok(size)
    }
    fn write<W: Write + Seek, K: TiffKind>(
        &self,
        enc: &mut TiffEncoder<W, K>,
    ) -> Result<u64, String> {
        let mut tags = prepare(enc, &self.tags)?;
        append_pointer(enc, &self.children, &mut tags)?;
        let (pixels, dims) =
            radelta_native::decompress_u16(&self.pixels).map_err(|e| e.to_string())?;
        if dims.x != self.x || dims.y != self.y || dims.z != 1 {
            return Err("pyramid codec dimensions mismatch".into());
        }
        let x = u32::try_from(self.x).map_err(|_| "pyramid width exceeds u32")?;
        let y = u32::try_from(self.y).map_err(|_| "pyramid height exceeds u32")?;
        let mut dir = enc.extra_directory().map_err(|e| e.to_string())?;
        let bits = sample_depth(&self.tags)?;
        let offset = if bits == 8 {
            dir.write_data(byte_pixels(&pixels).as_slice())
        } else {
            dir.write_data(pixels.as_slice())
        }
        .map_err(|e| e.to_string())?;
        macro_rules! tag {
            ($tag:ident, $value:expr) => {
                dir.write_tag(Tag::$tag, $value)
                    .map_err(|e| e.to_string())?
            };
        }
        tag!(ImageWidth, x);
        tag!(ImageLength, y);
        tag!(BitsPerSample, bits);
        tag!(Compression, 1u16);
        tag!(PhotometricInterpretation, 1u16);
        tag!(SamplesPerPixel, 1u16);
        tag!(RowsPerStrip, y);
        tag!(SampleFormat, 1u16);
        if let Ok(offset) = u32::try_from(offset) {
            tag!(StripOffsets, offset);
        } else {
            tag!(StripOffsets, offset);
        }
        let bytes = (pixels.len() as u64)
            .checked_mul(u64::from(bits / 8))
            .ok_or("pyramid strip size overflow")?;
        if let Ok(bytes) = u32::try_from(bytes) {
            tag!(StripByteCounts, bytes);
        } else {
            tag!(StripByteCounts, bytes);
        }
        for tag in &tags {
            if tag.id == 258 {
                continue;
            }
            write_tag(&mut dir, tag)?;
        }
        Ok(dir
            .finish_with_offsets()
            .map_err(|e| e.to_string())?
            .pointer
            .0)
    }
}

pub(super) fn append_pointer<W: Write + Seek, K: TiffKind>(
    enc: &mut TiffEncoder<W, K>,
    images: &[Image],
    tags: &mut Vec<SavedTag>,
) -> Result<(), String> {
    if images.is_empty() {
        return Ok(());
    }
    let offsets = images
        .iter()
        .map(|image| image.write(enc))
        .collect::<Result<Vec<_>, _>>()?;
    let wide = offsets.iter().any(|&v| v > u32::MAX as u64);
    let mut bytes = Vec::new();
    for offset in offsets {
        if wide {
            bytes.extend_from_slice(&offset.to_le_bytes());
        } else {
            bytes.extend_from_slice(&(offset as u32).to_le_bytes());
        }
    }
    tags.push(SavedTag {
        id: 330,
        ty: if wide { 18 } else { 13 },
        bytes,
        children: Vec::new(),
    });
    Ok(())
}
