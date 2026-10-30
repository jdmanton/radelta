//! Validated single-volume parsing and decoding.

use super::block::{
    decode_block_into_packed, decode_q_block_into_packed, packed_q_decode_table,
    packed_r_decode_table, BorrowedEncodedBlock, PackedDecodeTables,
};
use super::context::q_context_count;
use super::model::{StaticModel, MAX_Q_ALPHABET};
use super::transform::lossy_reconstruct;
use super::{LANES, RANS_L};
use crate::format::{
    check_header_crc, Reader, LOSSY_VERSION, MAGIC, MAGIC_LOSSY, RAW_BLOCK_MARKER,
    RAW_RECORD_BYTES, VERSION,
};
use crate::metadata;
use crate::types::{
    ContextMode, Dims, RadeltaError, Result, DEFAULT_SCALE_BITS, MAX_SCALE_BITS, MIN_SCALE_BITS,
};
use rayon::prelude::*;

struct ParsedLosslessStream<'a> {
    dims: Dims,
    block_depth: usize,
    mode: ContextMode,
    scale_bits: u8,
    q_alphabet: usize,
    q_model: Option<StaticModel>,
    r_model: Option<StaticModel>,
    blocks: Vec<BorrowedEncodedBlock<'a>>,
}

fn parse_lossless_stream(data: &[u8]) -> Result<ParsedLosslessStream<'_>> {
    let (data, _) = metadata::split(data)?;
    let mut rd = Reader::new(data);
    if rd.take(4)? != MAGIC {
        return Err(RadeltaError("not a RDL1 stream".into()));
    }
    let version = rd.u16()?;
    if version != VERSION {
        return Err(RadeltaError(format!("unsupported RDL version {version}")));
    }
    let _flags = rd.u16()?;
    let dims = Dims {
        x: rd.u32()? as usize,
        y: rd.u32()? as usize,
        z: rd.u32()? as usize,
    };
    if dims.x == 0 || dims.y == 0 || dims.z == 0 {
        return Err(RadeltaError("zero-sized volume dimension".into()));
    }
    dims.voxels()?;

    let block_depth = rd.u32()? as usize;
    if block_depth == 0 {
        return Err(RadeltaError("RDL1 block depth is zero".into()));
    }
    let q_max = rd.u16()? as usize;
    let r_max = rd.u16()? as usize;
    if q_max >= MAX_Q_ALPHABET {
        return Err(RadeltaError(
            "RDL1 q maximum exceeds uint16 square-root range".into(),
        ));
    }
    if r_max != 2 * q_max {
        return Err(RadeltaError(
            "RDL1 r maximum is inconsistent with q maximum".into(),
        ));
    }
    let scale_bits = rd.u8()?;
    if !(MIN_SCALE_BITS..=MAX_SCALE_BITS).contains(&scale_bits) {
        return Err(RadeltaError(format!(
            "RDL1 scale_bits must be in {MIN_SCALE_BITS}..={MAX_SCALE_BITS}"
        )));
    }
    let lanes = rd.u8()? as usize;
    if lanes != LANES {
        return Err(RadeltaError(format!("unsupported lane count {lanes}")));
    }
    let mode = ContextMode::from_u8(rd.u8()?)?;
    let _reserved = rd.u8()?;
    let q_model_len = rd.u32()? as usize;
    let r_model_len = rd.u32()? as usize;
    let nblocks = rd.u32()? as usize;
    let expected_blocks = dims.z.div_ceil(block_depth);
    if nblocks != expected_blocks {
        return Err(RadeltaError(format!(
            "RDL1 block count {nblocks} does not match expected {expected_blocks}"
        )));
    }

    let q_model_blob = rd.take(q_model_len)?;
    let r_model_blob = rd.take(r_model_len)?;
    check_header_crc(&mut rd, "RDL1")?;
    let q_alphabet = q_max + 1;
    let r_alphabet = r_max + 1;
    let (q_model, r_model) = if q_model_blob.is_empty() && r_model_blob.is_empty() {
        (None, None)
    } else {
        let q_model = StaticModel::deserialize(q_model_blob)?;
        let r_model = StaticModel::deserialize(r_model_blob)?;
        if q_model.scale_bits != scale_bits || r_model.scale_bits != scale_bits {
            return Err(RadeltaError("model/header scale-bit mismatch".into()));
        }
        if q_model.alphabet != q_alphabet
            || r_model.contexts != q_alphabet
            || r_model.alphabet != r_alphabet
        {
            return Err(RadeltaError("model/header alphabet mismatch".into()));
        }
        if q_model.contexts != q_context_count(q_alphabet, mode) {
            return Err(RadeltaError("q context-table shape mismatch".into()));
        }
        (Some(q_model), Some(r_model))
    };

    let min_record = RAW_RECORD_BYTES;
    if nblocks > rd.data.len().saturating_sub(rd.pos) / min_record {
        return Err(RadeltaError(
            "RDL1 block count exceeds available records".into(),
        ));
    }
    let mut blocks = Vec::new();
    blocks
        .try_reserve_exact(nblocks)
        .map_err(|_| RadeltaError("cannot allocate RDL1 block index".into()))?;
    for block_index in 0..nblocks {
        let start = rd.pos;
        let z0 = rd.u32()? as usize;
        let depth = rd.u32()? as usize;
        let q_len = rd.u32()? as usize;
        let r_len = rd.u32()? as usize;
        let expected_z0 = block_index * block_depth;
        let expected_depth = block_depth.min(dims.z - expected_z0);
        if z0 != expected_z0 || depth != expected_depth {
            return Err(RadeltaError(
                "RDL1 block layout is inconsistent with the header".into(),
            ));
        }
        let raw = q_len == RAW_BLOCK_MARKER as usize;
        if raw {
            let expected = depth
                .checked_mul(dims.x)
                .and_then(|v| v.checked_mul(dims.y))
                .and_then(|v| v.checked_mul(2))
                .ok_or_else(|| RadeltaError("raw block size overflow".into()))?;
            if r_len != expected {
                return Err(RadeltaError("invalid raw block length".into()));
            }
        } else if q_model.is_none() {
            return Err(RadeltaError(
                "compressed block has no probability models".into(),
            ));
        }
        let mut q_states = [0u32; LANES];
        let mut r_states = [0u32; LANES];
        let (q_bytes, r_bytes) = if raw {
            (rd.take(r_len)?, &[][..])
        } else {
            for state in &mut q_states {
                *state = rd.u32()?;
            }
            for state in &mut r_states {
                *state = rd.u32()?;
            }
            (rd.take(q_len)?, rd.take(r_len)?)
        };
        let record = &data[start..rd.pos];
        let crc = rd.u32()?;
        blocks.push(BorrowedEncodedBlock {
            depth,
            raw,
            q_states,
            r_states,
            q_bytes,
            r_bytes,
            record,
            crc,
        });
    }
    if !rd.is_done() {
        return Err(RadeltaError("trailing bytes after final block".into()));
    }

    Ok(ParsedLosslessStream {
        dims,
        block_depth,
        mode,
        scale_bits,
        q_alphabet,
        q_model,
        r_model,
        blocks,
    })
}

fn decode_lossless_stream_into(stream: &ParsedLosslessStream<'_>, out: &mut [u16]) -> Result<Dims> {
    let voxels = stream.dims.voxels()?;
    if out.len() != voxels {
        return Err(RadeltaError(format!(
            "output length {} does not match decoded voxel count {voxels}",
            out.len()
        )));
    }

    let plane = stream.dims.x * stream.dims.y;
    let nominal_chunk_voxels = stream
        .block_depth
        .checked_mul(plane)
        .ok_or_else(|| RadeltaError("decoded block size overflow".into()))?;

    let q_table = stream
        .q_model
        .as_ref()
        .map(packed_q_decode_table)
        .transpose()?
        .unwrap_or_default();
    let r_table = stream
        .r_model
        .as_ref()
        .map(packed_r_decode_table)
        .transpose()?
        .unwrap_or_default();
    let tables = PackedDecodeTables {
        q: &q_table,
        r: &r_table,
        total: 1 << stream.scale_bits,
        scale_bits: stream.scale_bits,
    };

    out.par_chunks_mut(nominal_chunk_voxels)
        .zip(stream.blocks.par_iter())
        .try_for_each(|(chunk, block)| {
            block.verify()?;
            if chunk.len() != block.depth * plane {
                return Err(RadeltaError("decoded block/output layout mismatch".into()));
            }
            if block.raw {
                for (pixel, &bytes) in chunk.iter_mut().zip(block.q_bytes.as_chunks::<2>().0) {
                    *pixel = u16::from_le_bytes(bytes);
                }
                return Ok(());
            }
            default_kernel!(
                stream.mode == ContextMode::Mean && stream.scale_bits == DEFAULT_SCALE_BITS,
                decode_block_into_packed(
                    block,
                    stream.dims,
                    stream.mode,
                    stream.q_alphabet,
                    &tables,
                    chunk,
                )
            )
        })?;

    Ok(stream.dims)
}

fn decompress_lossless_u16_into(data: &[u8], out: &mut [u16]) -> Result<Dims> {
    let stream = parse_lossless_stream(data)?;
    decode_lossless_stream_into(&stream, out)
}

pub(crate) fn inspect_lossless_dims(data: &[u8]) -> Result<Dims> {
    let mut rd = Reader::new(data);
    if rd.take(4)? != MAGIC {
        return Err(RadeltaError("not a RDL1 stream".into()));
    }
    let version = rd.u16()?;
    if version != VERSION {
        return Err(RadeltaError(format!("unsupported RDL version {version}")));
    }
    let _flags = rd.u16()?;
    let dims = Dims {
        x: rd.u32()? as usize,
        y: rd.u32()? as usize,
        z: rd.u32()? as usize,
    };
    if dims.x == 0 || dims.y == 0 || dims.z == 0 {
        return Err(RadeltaError("zero-sized volume dimension".into()));
    }
    Ok(dims)
}

struct ParsedLossyStream<'a> {
    dims: Dims,
    block_depth: usize,
    mode: ContextMode,
    q_alphabet: usize,
    q_model: Option<StaticModel>,
    blocks: Vec<BorrowedEncodedBlock<'a>>,
    reconstruction: [u16; MAX_Q_ALPHABET],
}

fn parse_lossy_stream(data: &[u8]) -> Result<ParsedLossyStream<'_>> {
    let (data, _) = metadata::split(data)?;
    let mut rd = Reader::new(data);
    if rd.take(4)? != MAGIC_LOSSY {
        return Err(RadeltaError("not a lossy RDLQ stream".into()));
    }
    let version = rd.u16()?;
    if version != LOSSY_VERSION {
        return Err(RadeltaError(format!("unsupported RDLQ version {version}")));
    }
    let _flags = rd.u16()?;
    let dims = Dims {
        x: rd.u32()? as usize,
        y: rd.u32()? as usize,
        z: rd.u32()? as usize,
    };
    if dims.x == 0 || dims.y == 0 || dims.z == 0 {
        return Err(RadeltaError("zero-sized volume dimension".into()));
    }
    let block_depth = rd.u32()? as usize;
    if block_depth == 0 {
        return Err(RadeltaError("RDLQ block depth is zero".into()));
    }
    let q_max = rd.u16()? as usize;
    if q_max >= MAX_Q_ALPHABET {
        return Err(RadeltaError(
            "RDLQ q maximum exceeds compact-context range".into(),
        ));
    }
    let _reserved_q = rd.u16()?;
    let scale_bits = rd.u8()?;
    if !(MIN_SCALE_BITS..=MAX_SCALE_BITS).contains(&scale_bits) {
        return Err(RadeltaError(format!(
            "RDLQ scale_bits must be in {MIN_SCALE_BITS}..={MAX_SCALE_BITS}"
        )));
    }
    let lanes = rd.u8()? as usize;
    if lanes != LANES {
        return Err(RadeltaError(format!("unsupported lane count {lanes}")));
    }
    let mode = ContextMode::from_u8(rd.u8()?)?;
    let _reserved = rd.u8()?;
    let offset_adu = rd.f64()?;
    let gain_e_per_adu = rd.f64()?;
    let noise_step = rd.f64()?;
    if !offset_adu.is_finite()
        || !gain_e_per_adu.is_finite()
        || gain_e_per_adu <= 0.0
        || !noise_step.is_finite()
        || noise_step <= 0.0
    {
        return Err(RadeltaError("invalid lossy calibration metadata".into()));
    }
    let q_model_len = rd.u32()? as usize;
    let nblocks = rd.u32()? as usize;
    let expected_blocks = dims.z.div_ceil(block_depth);
    if nblocks != expected_blocks {
        return Err(RadeltaError(format!(
            "RDLQ block count {nblocks} does not match expected {expected_blocks}"
        )));
    }
    let q_model_blob = rd.take(q_model_len)?;
    check_header_crc(&mut rd, "RDLQ")?;
    let q_alphabet = q_max + 1;
    let q_model = if q_model_blob.is_empty() {
        None
    } else {
        let q_model = StaticModel::deserialize(q_model_blob)?;
        if q_model.scale_bits != scale_bits {
            return Err(RadeltaError("model/header scale-bit mismatch".into()));
        }
        if q_model.alphabet != q_alphabet || q_model.contexts != q_context_count(q_alphabet, mode) {
            return Err(RadeltaError("lossy q model/header shape mismatch".into()));
        }
        Some(q_model)
    };

    let min_record = RAW_RECORD_BYTES;
    if nblocks > rd.data.len().saturating_sub(rd.pos) / min_record {
        return Err(RadeltaError(
            "RDLQ block count exceeds available records".into(),
        ));
    }
    let mut blocks = Vec::new();
    blocks
        .try_reserve_exact(nblocks)
        .map_err(|_| RadeltaError("cannot allocate RDLQ block index".into()))?;
    for _ in 0..nblocks {
        let start = rd.pos;
        let z0 = rd.u32()? as usize;
        let depth = rd.u32()? as usize;
        let q_len = rd.u32()? as usize;
        let expected_z0 = blocks.len() * block_depth;
        let expected_depth = block_depth.min(dims.z - expected_z0);
        if z0 != expected_z0 || depth != expected_depth {
            return Err(RadeltaError(
                "RDLQ block layout is inconsistent with the header".into(),
            ));
        }
        let raw = q_len == RAW_BLOCK_MARKER as usize;
        let mut q_states = [0u32; LANES];
        let q_bytes = if raw {
            let raw_len = rd.u32()? as usize;
            let expected = depth
                .checked_mul(dims.x)
                .and_then(|v| v.checked_mul(dims.y))
                .ok_or_else(|| RadeltaError("raw block size overflow".into()))?;
            if raw_len != expected {
                return Err(RadeltaError("invalid raw block length".into()));
            }
            rd.take(raw_len)?
        } else {
            if q_model.is_none() {
                return Err(RadeltaError(
                    "compressed block has no probability model".into(),
                ));
            }
            for st in &mut q_states {
                *st = rd.u32()?;
            }
            rd.take(q_len)?
        };
        let record = &data[start..rd.pos];
        let crc = rd.u32()?;
        blocks.push(BorrowedEncodedBlock {
            depth,
            raw,
            q_states,
            r_states: [RANS_L; LANES],
            q_bytes,
            r_bytes: &[],
            record,
            crc,
        });
    }
    if !rd.is_done() {
        return Err(RadeltaError(
            "trailing bytes after final lossy block".into(),
        ));
    }

    dims.voxels()?;
    Ok(ParsedLossyStream {
        dims,
        block_depth,
        mode,
        q_alphabet,
        q_model,
        blocks,
        reconstruction: std::array::from_fn(|q| {
            lossy_reconstruct(q as u8, offset_adu, gain_e_per_adu, noise_step)
        }),
    })
}

fn decode_lossy_stream_into(stream: &ParsedLossyStream<'_>, out: &mut [u16]) -> Result<Dims> {
    let voxels = stream.dims.voxels()?;
    if out.len() != voxels {
        return Err(RadeltaError(format!(
            "output length {} does not match decoded voxel count {voxels}",
            out.len()
        )));
    }
    let plane = stream.dims.x * stream.dims.y;
    let chunk_voxels = stream
        .block_depth
        .checked_mul(plane)
        .ok_or_else(|| RadeltaError("decoded block size overflow".into()))?;
    let q_table = stream
        .q_model
        .as_ref()
        .map(packed_q_decode_table)
        .transpose()?
        .unwrap_or_default();
    let tables = PackedDecodeTables {
        q: &q_table,
        r: &[],
        total: stream.q_model.as_ref().map_or(0, |m| m.total),
        scale_bits: stream.q_model.as_ref().map_or(0, |m| m.scale_bits),
    };
    out.par_chunks_mut(chunk_voxels)
        .zip(stream.blocks.par_iter())
        .try_for_each(|(chunk, block)| {
            block.verify()?;
            if block.raw {
                for (pixel, &q) in chunk.iter_mut().zip(block.q_bytes) {
                    if q as usize >= stream.q_alphabet {
                        return Err(RadeltaError("raw lossy symbol exceeds q maximum".into()));
                    }
                    *pixel = stream.reconstruction[q as usize];
                }
                return Ok(());
            }
            // Context prediction needs q throughout this block. Reconstruct only
            // after all its q symbols are decoded; blocks are independent.
            default_kernel!(
                stream.mode == ContextMode::Mean && tables.scale_bits == DEFAULT_SCALE_BITS,
                decode_q_block_into_packed(
                    block,
                    stream.dims,
                    stream.mode,
                    stream.q_alphabet,
                    &tables,
                    chunk,
                )
            )?;
            for pixel in chunk {
                *pixel = stream.reconstruction[*pixel as usize];
            }
            Ok(())
        })?;
    Ok(stream.dims)
}

/// Allocate a zeroed decode buffer, returning an error rather than aborting
/// the process when a (possibly corrupted) header requests an impossible size.
///
/// Like `vec![0; n]`, this uses a zeroed allocation so untouched pages are not
/// committed; a damaged stream that fails early therefore costs no memory.
pub(crate) fn alloc_zeroed_u16(voxels: usize) -> Result<Vec<u16>> {
    if voxels == 0 {
        return Ok(Vec::new());
    }
    let layout = std::alloc::Layout::array::<u16>(voxels)
        .map_err(|_| RadeltaError(format!("{voxels} decoded voxels exceed allocation limits")))?;
    // SAFETY: the layout has non-zero size. A non-null result is a zeroed
    // global allocation of exactly `voxels` u16 values, and all-zero bytes are
    // valid u16 values, so it satisfies Vec::from_raw_parts with len == capacity.
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout) as *mut u16;
        if ptr.is_null() {
            return Err(RadeltaError(format!(
                "cannot allocate {voxels} decoded voxels ({} bytes)",
                layout.size()
            )));
        }
        Ok(Vec::from_raw_parts(ptr, voxels, voxels))
    }
}

/// Decode a single-volume RDL1/RDLQ stream whose shape is already known from
/// a container, checking its header before allocating any output.
pub(crate) fn decompress_u16_expecting(data: &[u8], expected: Dims) -> Result<Vec<u16>> {
    let found = inspect_dims(data)?;
    if found != expected {
        return Err(RadeltaError(format!(
            "stream dimensions {}x{}x{} do not match expected {}x{}x{}",
            found.x, found.y, found.z, expected.x, expected.y, expected.z
        )));
    }
    let mut out = alloc_zeroed_u16(expected.voxels()?)?;
    let decoded = decompress_u16_into(data, &mut out)?;
    if decoded != expected {
        return Err(RadeltaError(
            "stream dimensions changed during decode".into(),
        ));
    }
    Ok(out)
}

fn decompress_lossy_u16(data: &[u8]) -> Result<(Vec<u16>, Dims)> {
    let stream = parse_lossy_stream(data)?;
    let mut out = alloc_zeroed_u16(stream.dims.voxels()?)?;
    let dims = decode_lossy_stream_into(&stream, &mut out)?;
    Ok((out, dims))
}

/// Decode a single-volume RDL1/RDLQ stream.
///
/// Both lossless and lossy streams reconstruct directly into the final output allocation
/// using the packed decoder.
pub fn decompress_u16(data: &[u8]) -> Result<(Vec<u16>, Dims)> {
    if data.len() < 4 {
        return Err(RadeltaError("truncated input".into()));
    }
    if &data[..4] == MAGIC {
        let stream = parse_lossless_stream(data)?;
        let mut out = alloc_zeroed_u16(stream.dims.voxels()?)?;
        let dims = decode_lossless_stream_into(&stream, &mut out)?;
        Ok((out, dims))
    } else if &data[..4] == MAGIC_LOSSY {
        decompress_lossy_u16(data)
    } else {
        Err(RadeltaError(
            "not a single-volume Radelta RDL1/RDLQ stream; use decompress_u16_nd for RDM2/RDQ2"
                .into(),
        ))
    }
}

/// Decode a single-volume Radelta stream directly into caller-owned storage.
///
/// Both RDL1 and RDLQ use the packed/direct decoder.
pub fn decompress_u16_into(data: &[u8], output: &mut [u16]) -> Result<Dims> {
    if data.len() < 4 {
        return Err(RadeltaError("truncated input".into()));
    }
    if &data[..4] == MAGIC {
        decompress_lossless_u16_into(data, output)
    } else if &data[..4] == MAGIC_LOSSY {
        let stream = parse_lossy_stream(data)?;
        decode_lossy_stream_into(&stream, output)
    } else {
        Err(RadeltaError(
            "not a single-volume Radelta RDL1/RDLQ stream".into(),
        ))
    }
}

pub(crate) fn inspect_lossy_dims(data: &[u8]) -> Result<Dims> {
    let mut rd = Reader::new(data);
    if rd.take(4)? != MAGIC_LOSSY {
        return Err(RadeltaError("not a lossy RDLQ stream".into()));
    }
    let version = rd.u16()?;
    if version != LOSSY_VERSION {
        return Err(RadeltaError(format!("unsupported RDLQ version {version}")));
    }
    let _flags = rd.u16()?;
    let dims = Dims {
        x: rd.u32()? as usize,
        y: rd.u32()? as usize,
        z: rd.u32()? as usize,
    };
    if dims.x == 0 || dims.y == 0 || dims.z == 0 {
        return Err(RadeltaError("zero-sized volume dimension".into()));
    }
    Ok(dims)
}

pub fn inspect_dims(data: &[u8]) -> Result<Dims> {
    if data.len() < 4 {
        return Err(RadeltaError("truncated input".into()));
    }
    if &data[..4] == MAGIC {
        inspect_lossless_dims(data)
    } else if &data[..4] == MAGIC_LOSSY {
        inspect_lossy_dims(data)
    } else {
        Err(RadeltaError("not a Radelta RDL1/RDLQ stream".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::transform::lossy_quantize;
    use super::*;
    use crate::codec::test_support::noisy_pixels;
    use crate::{
        compress_lossy_u16, compress_u16, compress_u16_with_stats, read_metadata, set_metadata,
        LossyOptions, Options,
    };
    #[test]
    fn incompressible_lossless_blocks_are_raw_without_models() {
        let dims = Dims {
            x: 128,
            y: 64,
            z: 9,
        };
        let input = noisy_pixels(dims.voxels().unwrap());
        let (mut encoded, stats) =
            compress_u16_with_stats(&input, dims, Options::default()).unwrap();
        let stream = parse_lossless_stream(&encoded).unwrap();
        assert!(stream.blocks.iter().all(|b| b.raw));
        assert!(stream.q_model.is_none() && stream.r_model.is_none());
        assert_eq!(stats.model_bytes, 0);
        assert_eq!(encoded.len(), input.len() * 2 + 48 + 3 * RAW_RECORD_BYTES);
        assert_eq!(decompress_u16(&encoded).unwrap().0, input);
        let mut direct = vec![0; input.len()];
        decompress_u16_into(&encoded, &mut direct).unwrap();
        assert_eq!(direct, input);
        set_metadata(&mut encoded, b"calibration").unwrap();
        assert_eq!(read_metadata(&encoded).unwrap(), b"calibration");
        assert_eq!(decompress_u16(&encoded).unwrap().0, input);
    }

    #[test]
    fn mixed_raw_and_compressed_blocks_roundtrip_with_partial_tail() {
        let dims = Dims { x: 1, y: 1, z: 257 };
        let mut input = vec![0; dims.voxels().unwrap()];
        input[256] = 54321;
        let encoded = compress_u16(
            &input,
            dims,
            Options {
                block_depth: 128,
                ..Options::default()
            },
        )
        .unwrap();
        let stream = parse_lossless_stream(&encoded).unwrap();
        assert_eq!(
            stream.blocks.iter().map(|b| b.raw).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(stream.blocks[2].depth, 1);
        assert_eq!(stream.blocks[2].q_bytes, &54321u16.to_le_bytes());
        assert_eq!(decompress_u16(&encoded).unwrap().0, input);

        let encoded = compress_lossy_u16(
            &input,
            dims,
            LossyOptions {
                block_depth: 128,
                ..LossyOptions::default()
            },
        )
        .unwrap();
        let stream = parse_lossy_stream(&encoded).unwrap();
        assert_eq!(
            stream.blocks.iter().map(|b| b.raw).collect::<Vec<_>>(),
            [false, false, true]
        );
        let decoded = decompress_u16(&encoded).unwrap().0;
        assert_eq!(&decoded[..256], &input[..256]);
        assert_eq!(
            decoded[256],
            lossy_reconstruct(lossy_quantize(input[256], 0., 1., 2.) as u8, 0., 1., 2.)
        );
    }

    #[test]
    fn lossy_raw_blocks_preserve_quantization() {
        // Uniform q symbols are incompressible even though raw q uses only
        // one byte per pixel. The fallback must not return original ADUs.
        let dims = Dims {
            x: 256,
            y: 128,
            z: 5,
        };
        let input: Vec<u16> = noisy_pixels(dims.voxels().unwrap())
            .iter()
            .map(|&v| {
                let q = v & 255;
                q * q + q / 2
            })
            .collect();
        let options = LossyOptions::default();
        let encoded = compress_lossy_u16(&input, dims, options).unwrap();
        let stream = parse_lossy_stream(&encoded).unwrap();
        assert!(stream.blocks.iter().all(|b| b.raw));
        assert!(stream.q_model.is_none());
        assert_eq!(encoded.len(), input.len() + 68 + 2 * RAW_RECORD_BYTES);
        let expected: Vec<_> = input
            .iter()
            .map(|&v| lossy_reconstruct(lossy_quantize(v, 0.0, 1.0, 2.0) as u8, 0.0, 1.0, 2.0))
            .collect();
        assert_ne!(expected, input);
        assert_eq!(decompress_u16(&encoded).unwrap().0, expected);
    }
}
