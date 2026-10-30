//! Single-volume encoding and compression statistics.

use super::block::{
    accumulate_block_counts, accumulate_q_block_counts, block_specs, encode_block,
    encode_q_only_block, histogram_grain, raw_block_plan, Counts, EncodedBlock,
};
use super::context::q_context_count;
use super::model::StaticModel;
use super::transform::{extract_lossless_q, extract_lossy_q};
use super::LANES;
use crate::format::{
    append_raw_u16, put_block_crc, put_f64, put_header_crc, put_u16, put_u32, LOSSY_VERSION, MAGIC,
    MAGIC_LOSSY, RAW_BLOCK_MARKER, VERSION,
};
use crate::types::{
    resolve_codec_options, CompressionStats, ContextMode, Dims, LossyOptions, Options,
    RadeltaError, Result, DEFAULT_SCALE_BITS,
};
use rayon::prelude::*;
use std::time::Instant;

pub fn compress_lossy_u16(input: &[u16], dims: Dims, options: LossyOptions) -> Result<Vec<u8>> {
    compress_lossy_u16_with_stats(input, dims, options).map(|v| v.0)
}

pub fn compress_lossy_u16_with_stats(
    input: &[u16],
    dims: Dims,
    options: LossyOptions,
) -> Result<(Vec<u8>, CompressionStats)> {
    let total_start = Instant::now();
    dims.validate()?;
    let voxels = dims.voxels()?;
    if input.len() != voxels {
        return Err(RadeltaError(format!(
            "input length {} does not match dimensions ({voxels} voxels)",
            input.len()
        )));
    }
    options.validate_calibration()?;

    let resolved = resolve_codec_options(
        options.block_depth,
        options.context_mode,
        options.scale_bits,
    )?;
    let block_depth = resolved.block_depth;
    let mode = resolved.mode;
    let scale_bits = resolved.scale_bits;

    let q_start = Instant::now();
    let (q, q_max) = extract_lossy_q(input, options)?;
    let seconds_q_extract = q_start.elapsed().as_secs_f64();
    let q_alphabet = q_max as usize + 1;

    let blocks = block_specs(dims.z, block_depth);
    let q_contexts = q_context_count(q_alphabet, mode);
    let q_count_len = q_contexts * q_alphabet;

    let hist_start = Instant::now();
    // Keep histogram memory bounded while allowing independent slabs to
    // balance uneven foreground/background work across the worker pool.
    let histogram_batch_size = histogram_grain(blocks.len(), q_count_len);
    let q_counts = blocks
        .par_chunks(histogram_batch_size)
        .map(|batch| {
            let mut acc = vec![0u64; q_count_len];
            for &block in batch {
                default_kernel!(
                    mode == ContextMode::Mean,
                    accumulate_q_block_counts(&mut acc, &q, dims, block, mode, q_alphabet)
                );
            }
            acc
        })
        .reduce_with(|mut a, b| {
            for (x, y) in a.iter_mut().zip(b) {
                *x += y;
            }
            a
        })
        .expect("validated volumes contain at least one block");
    let q_model = StaticModel::from_counts(&q_counts, q_contexts, q_alphabet, scale_bits)?;
    let seconds_histogram = hist_start.elapsed().as_secs_f64();

    let encode_start = Instant::now();
    let encoded_results: Vec<Result<EncodedBlock>> = blocks
        .par_iter()
        .map(|&block| {
            default_kernel!(
                mode == ContextMode::Mean && scale_bits == DEFAULT_SCALE_BITS,
                encode_q_only_block(&q, dims, block, mode, q_alphabet, &q_model)
            )
        })
        .collect();
    let mut encoded = Vec::with_capacity(encoded_results.len());
    for b in encoded_results {
        encoded.push(b?);
    }
    let seconds_encode = encode_start.elapsed().as_secs_f64();

    let mut q_model_blob = q_model.serialize()?;
    let plane = dims.x * dims.y;
    let raw = raw_block_plan(&encoded, plane, 1, q_model_blob.len());
    if raw.iter().all(|&v| v) {
        q_model_blob.clear();
    }
    let q_stream_bytes: usize = encoded
        .iter()
        .zip(&raw)
        .map(|(b, &raw)| {
            if raw {
                b.depth * plane
            } else {
                b.q_bytes.len()
            }
        })
        .sum();
    let model_bytes = q_model_blob.len();

    let mut out = Vec::with_capacity(q_stream_bytes + model_bytes + encoded.len() * 40 + 128);
    out.extend_from_slice(MAGIC_LOSSY);
    put_u16(&mut out, LOSSY_VERSION);
    put_u16(&mut out, 0);
    put_u32(&mut out, dims.x as u32);
    put_u32(&mut out, dims.y as u32);
    put_u32(&mut out, dims.z as u32);
    put_u32(&mut out, block_depth as u32);
    put_u16(&mut out, q_max as u16);
    put_u16(&mut out, 0);
    out.push(scale_bits);
    out.push(LANES as u8);
    out.push(mode as u8);
    out.push(0);
    put_f64(&mut out, options.offset_adu);
    put_f64(&mut out, options.gain_e_per_adu);
    put_f64(&mut out, options.noise_step);
    put_u32(&mut out, q_model_blob.len() as u32);
    put_u32(&mut out, encoded.len() as u32);
    out.extend_from_slice(&q_model_blob);
    put_header_crc(&mut out);

    for (b, &raw) in encoded.iter().zip(&raw) {
        let start = out.len();
        put_u32(&mut out, b.z0 as u32);
        put_u32(&mut out, b.depth as u32);
        if raw {
            let pixels = &q[b.z0 * plane..(b.z0 + b.depth) * plane];
            put_u32(&mut out, RAW_BLOCK_MARKER);
            put_u32(
                &mut out,
                u32::try_from(pixels.len())
                    .map_err(|_| RadeltaError("raw block exceeds u32 length".into()))?,
            );
            out.extend_from_slice(pixels);
            put_block_crc(&mut out, start);
            continue;
        }
        put_u32(&mut out, b.q_bytes.len() as u32);
        for &st in &b.q_states {
            put_u32(&mut out, st);
        }
        out.extend_from_slice(&b.q_bytes);
        put_block_crc(&mut out, start);
    }

    let stats = CompressionStats {
        raw_bytes: input.len() * 2,
        total_bytes: out.len(),
        q_stream_bytes,
        r_stream_bytes: 0,
        model_bytes,
        blocks: encoded.len(),
        q_max,
        r_max: 0,
        seconds_total: total_start.elapsed().as_secs_f64(),
        seconds_q_extract,
        seconds_histogram,
        seconds_encode,
    };
    Ok((out, stats))
}

pub fn compress_u16(input: &[u16], dims: Dims, options: Options) -> Result<Vec<u8>> {
    compress_u16_with_stats(input, dims, options).map(|v| v.0)
}

pub fn compress_u16_with_stats(
    input: &[u16],
    dims: Dims,
    options: Options,
) -> Result<(Vec<u8>, CompressionStats)> {
    let total_start = Instant::now();
    dims.validate()?;
    let voxels = dims.voxels()?;
    if input.len() != voxels {
        return Err(RadeltaError(format!(
            "input length {} does not match dimensions ({voxels} voxels)",
            input.len()
        )));
    }
    let resolved = resolve_codec_options(
        options.block_depth,
        options.context_mode,
        options.scale_bits,
    )?;
    let block_depth = resolved.block_depth;
    let mode = resolved.mode;
    let scale_bits = resolved.scale_bits;

    let q_start = Instant::now();
    let (q, q_max) = extract_lossless_q(input);
    let seconds_q_extract = q_start.elapsed().as_secs_f64();
    let q_alphabet = q_max as usize + 1;
    let r_max = 2u16 * q_max as u16;
    let r_alphabet = r_max as usize + 1;

    let blocks = block_specs(dims.z, block_depth);
    let q_contexts = q_context_count(q_alphabet, mode);
    let q_count_len = q_contexts * q_alphabet;
    let r_count_len = q_alphabet * r_alphabet;

    let hist_start = Instant::now();
    let histogram_batch_size = histogram_grain(blocks.len(), q_count_len + r_count_len);
    let counts = blocks
        .par_chunks(histogram_batch_size)
        .map(|batch| {
            let mut acc = Counts::new(q_count_len, r_count_len);
            for &block in batch {
                default_kernel!(
                    mode == ContextMode::Mean,
                    accumulate_block_counts(
                        &mut acc, input, &q, dims, block, mode, q_alphabet, r_alphabet,
                    )
                );
            }
            acc
        })
        .reduce_with(Counts::merge)
        .expect("validated volumes contain at least one block");
    let q_model = StaticModel::from_counts(&counts.q, q_contexts, q_alphabet, scale_bits)?;
    let r_model = StaticModel::from_counts(&counts.r, q_alphabet, r_alphabet, scale_bits)?;
    let seconds_histogram = hist_start.elapsed().as_secs_f64();

    let encode_start = Instant::now();
    let encoded_results: Vec<Result<EncodedBlock>> = blocks
        .par_iter()
        .map(|&block| {
            default_kernel!(
                mode == ContextMode::Mean && scale_bits == DEFAULT_SCALE_BITS,
                encode_block(input, &q, dims, block, mode, q_alphabet, &q_model, &r_model)
            )
        })
        .collect();
    let mut encoded = Vec::with_capacity(encoded_results.len());
    for b in encoded_results {
        encoded.push(b?);
    }
    let seconds_encode = encode_start.elapsed().as_secs_f64();

    let mut q_model_blob = q_model.serialize()?;
    let mut r_model_blob = r_model.serialize()?;
    let plane = dims.x * dims.y;
    let raw = raw_block_plan(&encoded, plane, 2, q_model_blob.len() + r_model_blob.len());
    if raw.iter().all(|&v| v) {
        q_model_blob.clear();
        r_model_blob.clear();
    }
    // Raw payload bytes are included in q_stream_bytes for accounting.
    let q_stream_bytes: usize = encoded
        .iter()
        .zip(&raw)
        .map(|(b, &raw)| {
            if raw {
                b.depth * plane * 2
            } else {
                b.q_bytes.len()
            }
        })
        .sum();
    let r_stream_bytes: usize = encoded
        .iter()
        .zip(&raw)
        .map(|(b, &raw)| if raw { 0 } else { b.r_bytes.len() })
        .sum();
    let model_bytes = q_model_blob.len() + r_model_blob.len();

    let mut out = Vec::with_capacity(
        q_stream_bytes + r_stream_bytes + model_bytes + encoded.len() * 64 + 128,
    );
    out.extend_from_slice(MAGIC);
    put_u16(&mut out, VERSION);
    put_u16(&mut out, 0);
    put_u32(&mut out, dims.x as u32);
    put_u32(&mut out, dims.y as u32);
    put_u32(&mut out, dims.z as u32);
    put_u32(&mut out, block_depth as u32);
    put_u16(&mut out, q_max as u16);
    put_u16(&mut out, r_max);
    out.push(scale_bits);
    out.push(LANES as u8);
    out.push(mode as u8);
    out.push(0);
    put_u32(&mut out, q_model_blob.len() as u32);
    put_u32(&mut out, r_model_blob.len() as u32);
    put_u32(&mut out, encoded.len() as u32);
    out.extend_from_slice(&q_model_blob);
    out.extend_from_slice(&r_model_blob);
    put_header_crc(&mut out);

    for (b, &raw) in encoded.iter().zip(&raw) {
        let start = out.len();
        put_u32(&mut out, b.z0 as u32);
        put_u32(&mut out, b.depth as u32);
        if raw {
            let pixels = &input[b.z0 * plane..(b.z0 + b.depth) * plane];
            put_u32(&mut out, RAW_BLOCK_MARKER);
            put_u32(
                &mut out,
                u32::try_from(std::mem::size_of_val(pixels))
                    .map_err(|_| RadeltaError("raw block exceeds u32 length".into()))?,
            );
            append_raw_u16(&mut out, pixels);
            put_block_crc(&mut out, start);
            continue;
        }
        put_u32(&mut out, b.q_bytes.len() as u32);
        put_u32(&mut out, b.r_bytes.len() as u32);
        for &s in &b.q_states {
            put_u32(&mut out, s);
        }
        for &s in &b.r_states {
            put_u32(&mut out, s);
        }
        out.extend_from_slice(&b.q_bytes);
        out.extend_from_slice(&b.r_bytes);
        put_block_crc(&mut out, start);
    }

    let stats = CompressionStats {
        raw_bytes: input.len() * 2,
        total_bytes: out.len(),
        q_stream_bytes,
        r_stream_bytes,
        model_bytes,
        blocks: encoded.len(),
        q_max,
        r_max,
        seconds_total: total_start.elapsed().as_secs_f64(),
        seconds_q_extract,
        seconds_histogram,
        seconds_encode,
    };
    Ok((out, stats))
}
