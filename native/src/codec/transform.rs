//! Integer square roots and camera-calibrated pixel transforms.

use crate::types::{LossyOptions, RadeltaError, Result};
use rayon::prelude::*;

pub(super) fn make_sqrt_lut() -> &'static [u8; 65536] {
    static LUT: std::sync::OnceLock<[u8; 65536]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut lut = [0u8; 65536];
        for q in 0usize..256 {
            lut[q * q..(q + 1) * (q + 1)].fill(q as u8);
        }
        lut
    })
}

#[inline(always)]
pub(crate) fn lossy_quantize(v: u16, offset_adu: f64, gain_e_per_adu: f64, noise_step: f64) -> u16 {
    let electrons = ((v as f64 - offset_adu) * gain_e_per_adu).max(0.0);
    ((2.0 * electrons.sqrt()) / noise_step).round() as u16
}

#[inline(always)]
pub(crate) fn lossy_reconstruct(
    q: u8,
    offset_adu: f64,
    gain_e_per_adu: f64,
    noise_step: f64,
) -> u16 {
    let z = q as f64 * noise_step;
    let electrons = 0.25 * z * z;
    let adu = offset_adu + electrons / gain_e_per_adu;
    adu.round().clamp(0.0, u16::MAX as f64) as u16
}

// A pair of memory scans should not fan out into dozens of jobs for small
// Zarr chunks. Large volumes still expose enough work for all workers.
const MIN_PREPROCESS_VOXELS: usize = 64 * 1024;

// Once the raw input exceeds 64 MiB, avoiding a second full input pass is
// useful. Smaller, cache-resident chunks retain the simpler mapping loop.
pub(super) fn extract_lossless_q(input: &[u16]) -> (Vec<u8>, u8) {
    if input.len() >= 32 * 1024 * 1024 {
        return extract_lossless_q_fused(input);
    }
    let lookup = make_sqrt_lut();
    let maximum = input
        .par_iter()
        .with_min_len(MIN_PREPROCESS_VOXELS)
        .copied()
        .max()
        .unwrap_or(0);
    let q = input
        .par_iter()
        .with_min_len(MIN_PREPROCESS_VOXELS)
        .map(|&pixel| lookup[pixel as usize])
        .collect();
    (q, lookup[maximum as usize])
}

// Produce q and its maximum in one input pass. Each output chunk has exactly
// the input chunk's length; the reduction is independent of scheduling.
fn extract_lossless_q_fused(input: &[u16]) -> (Vec<u8>, u8) {
    let lookup = make_sqrt_lut();
    let mut q = vec![0; input.len()];
    let maximum = q
        .par_chunks_mut(MIN_PREPROCESS_VOXELS)
        .zip(input.par_chunks(MIN_PREPROCESS_VOXELS))
        .map(|(output, pixels)| {
            let mut maximum = 0;
            let prefix = pixels.len().min(1024);
            for (value, &pixel) in output[..prefix].iter_mut().zip(&pixels[..prefix]) {
                *value = lookup[pixel as usize];
                maximum = maximum.max(*value);
            }
            if maximum == u8::MAX {
                // A proven maximum cannot increase. Full-range inputs can
                // map the remainder without maintaining a reduction chain.
                for (value, &pixel) in output[prefix..].iter_mut().zip(&pixels[prefix..]) {
                    *value = lookup[pixel as usize];
                }
            } else {
                for (value, &pixel) in output[prefix..].iter_mut().zip(&pixels[prefix..]) {
                    *value = lookup[pixel as usize];
                    maximum = maximum.max(*value);
                }
            }
            maximum
        })
        .max()
        .unwrap_or(0);
    (q, maximum)
}

pub(super) fn extract_lossy_q(input: &[u16], options: LossyOptions) -> Result<(Vec<u8>, u8)> {
    let max_input = input
        .par_iter()
        .with_min_len(MIN_PREPROCESS_VOXELS)
        .copied()
        .max()
        .unwrap_or(0);
    let quantize = |v| {
        lossy_quantize(
            v,
            options.offset_adu,
            options.gain_e_per_adu,
            options.noise_step,
        )
    };
    // With validated calibration the transform is monotonic. Checking the
    // largest input first permits writing the compact q buffer directly.
    let q_max16 = quantize(max_input);
    if q_max16 > u8::MAX as u16 {
        return Err(RadeltaError(format!(
            "lossy q maximum is {q_max16}, but the compact context coder supports q<=255; increase --noise-step (or use a lower e-/ADU gain)"
        )));
    }
    let lookup_len = max_input as usize + 1;
    let q = if input.len() >= 4 * lookup_len {
        // Amortize the table only when each possible intensity would otherwise
        // be quantized at least four times on average.
        let lookup: Vec<u8> = (0..lookup_len).map(|v| quantize(v as u16) as u8).collect();
        input
            .par_iter()
            .with_min_len(MIN_PREPROCESS_VOXELS)
            .map(|&v| lookup[v as usize])
            .collect()
    } else {
        input
            .par_iter()
            .with_min_len(MIN_PREPROCESS_VOXELS)
            .map(|&v| quantize(v) as u8)
            .collect()
    };
    Ok((q, q_max16 as u8))
}

#[cfg(test)]
mod tests {
    use super::super::encode::compress_lossy_u16;
    use super::*;
    use crate::types::{Dims, LossyOptions};

    #[test]
    fn sqrt_lookup_is_exact_for_every_uint16() {
        let lut = make_sqrt_lut();
        for (value, &q) in lut.iter().enumerate() {
            let q = q as usize;
            assert!(q * q <= value && (q + 1) * (q + 1) > value);
        }
    }

    #[test]
    fn lossy_quantization_lookup_matches_scalar_for_every_u16() {
        let domain: Vec<u16> = (0..=u16::MAX).collect();
        let repeated = domain.repeat(4);
        for (offset_adu, gain_e_per_adu, noise_step) in [
            (0.0, 1.0, 2.01),
            (100.0, 0.5, 2.0),
            (-200.0, 1.25, 3.0),
            (123.5, 0.73, 1.9),
            (65536.0, 2.0, 0.25),
        ] {
            let options = LossyOptions {
                offset_adu,
                gain_e_per_adu,
                noise_step,
                ..LossyOptions::default()
            };
            let expected: Vec<u8> = domain
                .iter()
                .map(|&v| lossy_quantize(v, offset_adu, gain_e_per_adu, noise_step) as u8)
                .collect();
            // One copy selects direct quantization; four copies select the
            // table, including its upper endpoint at input 65535.
            let (direct, direct_max) = extract_lossy_q(&domain, options).unwrap();
            let (lookup, lookup_max) = extract_lossy_q(&repeated, options).unwrap();
            assert_eq!(direct, expected);
            assert_eq!(direct_max, *expected.iter().max().unwrap());
            assert_eq!(lookup_max, direct_max);
            for chunk in lookup.chunks(domain.len()) {
                assert_eq!(chunk, expected);
            }
        }
    }

    #[test]
    fn lossy_quantization_rejects_unrepresentable_maximum() {
        let options = LossyOptions::default();
        // Under the default calibration 65025 maps to 255, but 65535 rounds
        // to 256. The input maximum must be rejected before casting to u8.
        let dims = Dims { x: 3, y: 1, z: 1 };
        assert!(compress_lossy_u16(&[65025, 0, 1], dims, options).is_ok());
        let error = compress_lossy_u16(&[0, u16::MAX, 65025], dims, options).unwrap_err();
        assert!(error.0.contains("q maximum is 256"));
        let repeated = vec![u16::MAX; 4 * (u16::MAX as usize + 1)];
        assert!(extract_lossy_q(&repeated, options).is_err());
    }

    #[test]
    fn fused_lossless_extraction_preserves_symbols_maximum_and_chunk_tails() {
        for threads in [1, 2, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                for len in [1, 3, 65535, 65536, 65537, 131071, 131072, 131073] {
                    for range in [6000, 65536] {
                        let mut pixels: Vec<u16> =
                            (0..len).map(|i| ((i * 31337) % range) as u16).collect();
                        pixels[len - 1] = (range - 1) as u16;
                        let expected: Vec<_> = pixels
                            .iter()
                            .map(|&v| (v as f64).sqrt().floor() as u8)
                            .collect();
                        let (actual, maximum) = extract_lossless_q_fused(&pixels);
                        assert_eq!(actual, expected);
                        assert_eq!(maximum, *expected.iter().max().unwrap());
                    }
                }
                let mut final_peak = vec![0; 65537];
                final_peak[65536] = u16::MAX;
                let (actual, maximum) = extract_lossless_q_fused(&final_peak);
                assert_eq!(maximum, 255);
                assert!(actual[..65536].iter().all(|&q| q == 0));
                assert_eq!(actual[65536], 255);
                final_peak[65536] = 0;
                let (zeros, maximum) = extract_lossless_q_fused(&final_peak);
                assert!(zeros.iter().all(|&q| q == 0));
                assert_eq!(maximum, 0);
            });
        }
    }
}
