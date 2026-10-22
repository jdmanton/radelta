//! Bulk I/O for uncompressed uint16 strips. Other TIFF layouts retain the
//! TIFF decoder's predictor, decompression, tile and photometric handling.
use std::io::{Read, Seek, SeekFrom};
use tiff::{
    decoder::{Decoder, DecodingResult},
    tags::{ByteOrder, Tag, Type, ValueBuffer},
};

pub fn read_u16<R: Read>(reader: &mut R, out: &mut [u16], order: ByteOrder) -> Result<(), String> {
    // SAFETY: u16 has no padding and every bit pattern is valid. The slice is
    // initialized, uniquely borrowed, and the byte view has exactly its size.
    // A short read leaves valid (though partial) u16 values before returning.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(out.as_mut_ptr().cast::<u8>(), std::mem::size_of_val(out))
    };
    reader.read_exact(bytes).map_err(|e| e.to_string())?;
    if order != ByteOrder::native() {
        for value in out {
            *value = value.swap_bytes();
        }
    }
    Ok(())
}

fn strip_table<R: Read + Seek>(dec: &mut Decoder<R>, tag: Tag) -> Result<Vec<u64>, String> {
    let mut ifd = dec.image_ifd();
    let ty = ifd
        .find_entry(tag)
        .ok_or("missing TIFF strip table")?
        .field_type();
    let mut buf = ValueBuffer::empty(ty);
    ifd.find_tag_buf(tag, &mut buf).map_err(|e| e.to_string())?;
    buf.set_byte_order(ByteOrder::native());
    // Avoid constructing a boxed TIFF Value for every strip table element.
    match ty {
        Type::SHORT => Ok(buf
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| u16::from_ne_bytes(*v) as u64)
            .collect()),
        Type::LONG => Ok(buf
            .as_bytes()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|v| u32::from_ne_bytes(*v) as u64)
            .collect()),
        Type::LONG8 => Ok(buf
            .as_bytes()
            .as_chunks::<8>()
            .0
            .iter()
            .map(|v| u64::from_ne_bytes(*v))
            .collect()),
        _ => Err("invalid TIFF strip table type".into()),
    }
}

pub fn read_plane<R: Read + Seek>(dec: &mut Decoder<R>, out: &mut [u16]) -> Result<(), String> {
    if read_raw_sample_plane(dec, out, 0)? {
        return Ok(());
    }
    match dec.read_image().map_err(|e| e.to_string())? {
        DecodingResult::U16(v) if v.len() == out.len() => {
            out.copy_from_slice(&v);
            Ok(())
        }
        DecodingResult::U16(v) => Err(format!(
            "expected {} uint16 samples, got {}",
            out.len(),
            v.len()
        )),
        _ => Err("expected uint16 grayscale TIFF".into()),
    }
}

pub fn u16_bytes_mut(out: &mut [u16]) -> &mut [u8] {
    // SAFETY: u16 has no padding or invalid bit patterns. This byte slice
    // uniquely borrows exactly the initialized storage of out.
    unsafe {
        std::slice::from_raw_parts_mut(out.as_mut_ptr().cast::<u8>(), std::mem::size_of_val(out))
    }
}

/// Bulk-read one planar sample when its stored bytes need no TIFF transforms.
/// Compressed, tiled and photometrically transformed samples use the decoder.
pub fn read_raw_sample_plane<R: Read + Seek>(
    dec: &mut Decoder<R>,
    out: &mut [u16],
    sample: usize,
) -> Result<bool, String> {
    let Some(runs) = raw_runs(dec, out.len(), sample)? else {
        return Ok(false);
    };
    let order = dec.byte_order();
    let reader = dec.inner();
    let mut start = 0;
    for (offset, samples) in runs {
        let position = reader.stream_position().map_err(|e| e.to_string())?;
        if position != offset {
            reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| e.to_string())?;
        }
        read_u16(reader, &mut out[start..start + samples], order)?;
        start += samples;
    }
    Ok(true)
}

/// Return coalesced byte ranges only when the on-disk bytes are exactly the
/// desired samples. Never bypass the decoder for transforms or ambiguous tags.
fn raw_runs<R: Read + Seek>(
    dec: &mut Decoder<R>,
    plane_samples: usize,
    sample: usize,
) -> Result<Option<Vec<(u64, usize)>>, String> {
    let scalar = |dec: &mut Decoder<R>, tag, default| -> Result<u64, String> {
        dec.find_tag(tag)
            .map_err(|e| e.to_string())?
            .map(|v| v.into_u64().map_err(|e| e.to_string()))
            .unwrap_or(Ok(default))
    };
    if scalar(dec, Tag::Compression, 1)? != 1
        || dec.image_ifd().find_entry(Tag::TileOffsets).is_some()
    {
        return Ok(None);
    }
    let samples = usize::try_from(scalar(dec, Tag::SamplesPerPixel, 1)?)
        .map_err(|_| "TIFF sample count overflow")?;
    if samples == 0 || sample >= samples {
        return Err("TIFF sample index is out of range".into());
    }
    let unsigned = dec
        .find_tag(Tag::SampleFormat)
        .map_err(|e| e.to_string())?
        .map(|v| {
            v.into_u16_vec()
                .map(|v| !v.is_empty() && v.iter().all(|&f| f == 1))
        })
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(true);
    let bits = dec
        .get_tag_u16_vec(Tag::BitsPerSample)
        .map_err(|e| e.to_string())?;
    if (samples > 1 && scalar(dec, Tag::PlanarConfiguration, 1)? != 2)
        || !unsigned
        || scalar(dec, Tag::Predictor, 1)? != 1
        || scalar(dec, Tag::FillOrder, 1)? != 1
        || scalar(dec, Tag::PhotometricInterpretation, 1)? != 1
        || bits.len() != samples
        || bits.iter().any(|&b| b != 16)
    {
        return Ok(None);
    }
    let (w, h) = dec.dimensions().map_err(|e| e.to_string())?;
    if (w as usize).checked_mul(h as usize) != Some(plane_samples) {
        return Err("TIFF plane size mismatch".into());
    }
    let rows = scalar(dec, Tag::RowsPerStrip, u32::MAX as u64)?;
    if rows == 0 {
        return Err("TIFF RowsPerStrip must be positive".into());
    }
    let offsets = strip_table(dec, Tag::StripOffsets)?;
    let counts = strip_table(dec, Tag::StripByteCounts)?;
    let strips =
        usize::try_from((h as u64).div_ceil(rows)).map_err(|_| "TIFF strip count overflow")?;
    if offsets.len() != counts.len() || strips.checked_mul(samples) != Some(offsets.len()) {
        return Err("invalid TIFF strip table".into());
    }
    let range = sample * strips..(sample + 1) * strips;
    let mut runs: Vec<(u64, usize)> = Vec::new();
    let mut remaining = h as u64;
    for (&offset, &count) in offsets[range.clone()].iter().zip(&counts[range]) {
        let expected = remaining.min(rows) * w as u64 * 2;
        if count != expected {
            return Ok(None);
        }
        offset
            .checked_add(count)
            .ok_or("TIFF strip offset overflow")?;
        let n = usize::try_from(count / 2).map_err(|_| "TIFF strip size overflow")?;
        if let Some((last, len)) = runs.last_mut() {
            if last.checked_add((*len as u64) * 2) == Some(offset) {
                *len += n;
                remaining -= remaining.min(rows);
                continue;
            }
        }
        runs.push((offset, n));
        remaining -= remaining.min(rows);
    }
    Ok(Some(runs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // Independently built classic TIFF: 2x3, three one-row strips. Exercise
    // byte order, physical gaps, photometric fallback and truncated payloads.
    fn fixture(big: bool, gaps: bool, white: bool) -> Vec<u8> {
        sample_fixture(big, gaps, white, 1)
    }

    fn sample_fixture(big: bool, gaps: bool, white: bool, samples: u32) -> Vec<u8> {
        fn u16b(v: u16, big: bool) -> [u8; 2] {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        }
        fn u32b(v: u32, big: bool) -> [u8; 4] {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        }
        let tags = 10u16;
        let table = 8 + 2 + tags as u32 * 12 + 4;
        let strips = 3 * samples;
        let bits = table + strips * 8;
        let data = bits + if samples > 1 { 2 * samples } else { 0 };
        let stride = if gaps { 8 } else { 4 };
        let mut bytes = if big { b"MM".to_vec() } else { b"II".to_vec() };
        bytes.extend(u16b(42, big));
        bytes.extend(u32b(8, big));
        bytes.extend(u16b(tags, big));
        for (tag, ty, count, value) in [
            (256, 4, 1, 2),
            (257, 4, 1, 3),
            (258, 3, samples, if samples == 1 { 16 } else { bits }),
            (259, 3, 1, 1),
            (262, 3, 1, u32::from(!white)),
            (273, 4, strips, table),
            (277, 3, 1, samples),
            (278, 4, 1, 1),
            (279, 4, strips, table + strips * 4),
            (284, 3, 1, 2),
        ] {
            bytes.extend(u16b(tag, big));
            bytes.extend(u16b(ty, big));
            bytes.extend(u32b(count, big));
            if ty == 3 && count == 1 {
                bytes.extend(u16b(value as u16, big));
                bytes.extend([0; 2]);
            } else {
                bytes.extend(u32b(value, big));
            }
        }
        bytes.extend([0; 4]);
        for row in 0..strips {
            bytes.extend(u32b(data + row * stride, big));
        }
        for _ in 0..strips {
            bytes.extend(u32b(4, big));
        }
        if samples > 1 {
            for _ in 0..samples {
                bytes.extend(u16b(16, big));
            }
        }
        for row in 0..strips {
            let base = (row / 3 * 1000 + row % 3 * 20000) as u16;
            for v in [base, base + 123] {
                bytes.extend(u16b(v, big));
            }
            if gaps && row + 1 < strips {
                bytes.extend([99; 4]);
            }
        }
        bytes
    }

    #[test]
    fn raw_strips_match_decoder_in_both_byte_orders_and_with_gaps() {
        for big in [false, true] {
            for gaps in [false, true] {
                for white in [false, true] {
                    let bytes = fixture(big, gaps, white);
                    let mut dec = Decoder::new(Cursor::new(bytes.clone())).unwrap();
                    let runs = raw_runs(&mut dec, 6, 0).unwrap();
                    if white {
                        assert!(runs.is_none());
                    } else {
                        assert_eq!(runs.unwrap().len(), if gaps { 3 } else { 1 });
                    }
                    let mut out = [0u16; 6];
                    read_plane(&mut dec, &mut out).unwrap();
                    let DecodingResult::U16(expected) = Decoder::new(Cursor::new(bytes))
                        .unwrap()
                        .read_image()
                        .unwrap()
                    else {
                        panic!()
                    };
                    assert_eq!(out.as_slice(), expected);
                }
            }
        }
    }

    #[test]
    fn bulk_read_rejects_truncation_and_wrong_plane_size() {
        let mut bytes = fixture(false, false, false);
        bytes.pop();
        let mut dec = Decoder::new(Cursor::new(bytes)).unwrap();
        assert!(read_plane(&mut dec, &mut [0; 6]).is_err());
        let mut dec = Decoder::new(Cursor::new(fixture(false, false, false))).unwrap();
        assert!(read_plane(&mut dec, &mut [0; 5]).is_err());
    }

    #[test]
    fn planar_bulk_reads_select_samples_in_both_byte_orders_and_with_gaps() {
        for big in [false, true] {
            for gaps in [false, true] {
                let bytes = sample_fixture(big, gaps, false, 3);
                let mut dec = Decoder::new(Cursor::new(bytes.clone())).unwrap();
                let mut reference = Decoder::new(Cursor::new(bytes.clone())).unwrap();
                for sample in [2, 0, 1, 2] {
                    let mut expected = Vec::new();
                    for strip in sample * 3..sample * 3 + 3 {
                        let DecodingResult::U16(v) = reference.read_chunk(strip as u32).unwrap()
                        else {
                            panic!()
                        };
                        expected.extend(v);
                    }
                    let mut out = [0; 6];
                    assert!(read_raw_sample_plane(&mut dec, &mut out, sample).unwrap());
                    assert_eq!(out.as_slice(), expected);
                }
                assert!(read_raw_sample_plane(&mut dec, &mut [0; 6], 3).is_err());
                assert!(read_raw_sample_plane(&mut dec, &mut [0; 5], 0).is_err());
                let mut short = bytes;
                short.pop();
                let mut dec = Decoder::new(Cursor::new(short)).unwrap();
                assert!(read_raw_sample_plane(&mut dec, &mut [0; 6], 2).is_err());
            }
        }
        let mut dec = Decoder::new(Cursor::new(sample_fixture(false, false, true, 3))).unwrap();
        assert!(!read_raw_sample_plane(&mut dec, &mut [0; 6], 0).unwrap());
    }
}
