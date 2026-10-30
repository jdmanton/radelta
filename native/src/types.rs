//! Codec options, dimensions, errors and compression statistics.

use std::fmt;

#[doc(hidden)]
pub const DEFAULT_STREAM_MEMORY_MIB: usize = 2048;

#[doc(hidden)]
pub const DEFAULT_BLOCK_DEPTH: usize = 4;

pub(crate) const DEFAULT_SCALE_BITS: u8 = 10;

pub(crate) const MIN_SCALE_BITS: u8 = 8;

pub(crate) const MAX_SCALE_BITS: u8 = 11;

#[derive(Debug, Clone)]
pub struct RadeltaError(pub String);

impl fmt::Display for RadeltaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RadeltaError {}

/// Result type returned by the safe Rust codec API.
pub type Result<T> = std::result::Result<T, RadeltaError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ContextMode {
    /// median(L,U,Z) + signed coarse magnitudes of L-U and Z-median.
    /// For k=3 this gives categories 0, +1, +>=2, -1, ->=2.
    Signed3 = 0,
    /// median(L,U,Z) + ternary signs only.
    Signs = 1,
    /// Exact L+U+Z sum, retaining fractional means with a compact table.
    /// The first local plane uses floor((L+U)/2) in place of Z.
    Mean = 2,
    /// Exact L+U+Z sum + sign(L-U), using a larger table for finer conditioning.
    /// The first local plane uses floor((L+U)/2) in place of Z.
    MeanSigns = 3,
}

impl ContextMode {
    pub(crate) fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Signed3),
            1 => Ok(Self::Signs),
            2 => Ok(Self::Mean),
            3 => Ok(Self::MeanSigns),
            _ => Err(RadeltaError(format!("unsupported context mode {v}"))),
        }
    }

    pub(crate) fn compact_count(self, q_alphabet: usize) -> usize {
        q_alphabet
            * match self {
                Self::Signed3 => 25,
                Self::Signs | Self::MeanSigns => 9,
                Self::Mean => 3,
            }
    }

    pub(crate) fn uses_mean(self) -> bool {
        matches!(self, Self::Mean | Self::MeanSigns)
    }
}

const DEFAULT_CONTEXT_MODE: ContextMode = ContextMode::Mean;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedCodecOptions {
    pub(crate) block_depth: usize,
    pub(crate) mode: ContextMode,
    pub(crate) scale_bits: u8,
}

pub(crate) fn resolve_codec_options(
    block_depth: u32,
    context_mode: u32,
    scale_bits: u32,
) -> Result<ResolvedCodecOptions> {
    let block_depth = if block_depth == 0 {
        DEFAULT_BLOCK_DEPTH
    } else {
        block_depth as usize
    };
    let mode_raw = u8::try_from(context_mode)
        .map_err(|_| RadeltaError(format!("unsupported context mode {context_mode}")))?;
    let mode = ContextMode::from_u8(mode_raw)?;
    let scale_bits = if scale_bits == 0 {
        DEFAULT_SCALE_BITS
    } else {
        u8::try_from(scale_bits)
            .map_err(|_| RadeltaError(format!("scale_bits {scale_bits} is out of range")))?
    };
    if !(MIN_SCALE_BITS..=MAX_SCALE_BITS).contains(&scale_bits) {
        return Err(RadeltaError(format!(
            "scale_bits must be in {MIN_SCALE_BITS}..={MAX_SCALE_BITS}"
        )));
    }
    Ok(ResolvedCodecOptions {
        block_depth,
        mode,
        scale_bits,
    })
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Options {
    pub block_depth: u32,
    pub context_mode: u32,
    pub scale_bits: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            block_depth: DEFAULT_BLOCK_DEPTH as u32,
            context_mode: DEFAULT_CONTEXT_MODE as u32,
            scale_bits: DEFAULT_SCALE_BITS as u32,
        }
    }
}

/// Options for the camera-calibrated lossy mode.
///
/// `gain_e_per_adu` is the conversion gain in electrons / ADU.
/// `noise_step` is the quantisation interval in the variance-stabilised
/// coordinate z = 2*sqrt(electrons).  For shot-noise-limited data, z has
/// approximately unit standard deviation.  Thus noise_step=1 is conservative;
/// noise_step=2 is approximately equivalent to rounding sqrt(electrons).
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct LossyOptions {
    pub block_depth: u32,
    pub context_mode: u32,
    pub scale_bits: u32,
    pub offset_adu: f64,
    pub gain_e_per_adu: f64,
    pub noise_step: f64,
}

impl Default for LossyOptions {
    fn default() -> Self {
        Self {
            block_depth: DEFAULT_BLOCK_DEPTH as u32,
            context_mode: DEFAULT_CONTEXT_MODE as u32,
            scale_bits: DEFAULT_SCALE_BITS as u32,
            offset_adu: 0.0,
            gain_e_per_adu: 1.0,
            noise_step: 2.0,
        }
    }
}

impl LossyOptions {
    pub(crate) fn validate_calibration(self) -> Result<()> {
        if !self.offset_adu.is_finite() {
            return Err(RadeltaError("offset_adu must be finite".into()));
        }
        if !self.gain_e_per_adu.is_finite() || self.gain_e_per_adu <= 0.0 {
            return Err(RadeltaError("gain_e_per_adu must be finite and > 0".into()));
        }
        if !self.noise_step.is_finite() || self.noise_step <= 0.0 {
            return Err(RadeltaError("noise_step must be finite and > 0".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dims {
    pub x: usize,
    pub y: usize,
    pub z: usize,
}

impl Dims {
    /// Number of voxels in the XYZ volume, with overflow checking.
    pub fn voxels(self) -> Result<usize> {
        self.x
            .checked_mul(self.y)
            .and_then(|v| v.checked_mul(self.z))
            .ok_or_else(|| RadeltaError("volume dimensions overflow usize".into()))
    }

    pub(crate) fn validate(self) -> Result<()> {
        if self.x == 0 || self.y == 0 || self.z == 0 {
            return Err(RadeltaError("zero-sized volume dimension".into()));
        }
        if self.x > u32::MAX as usize || self.y > u32::MAX as usize || self.z > u32::MAX as usize {
            return Err(RadeltaError(
                "volume dimension exceeds u32 header range".into(),
            ));
        }
        self.voxels().map(|_| ())
    }
}

/// Canonical multidimensional shape. Memory order is contiguous T,C,Z,Y,X
/// (X fastest). Spatial contexts are confined to each individual (T,C) XYZ
/// volume and never cross time or channel boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dims5 {
    pub x: usize,
    pub y: usize,
    pub z: usize,
    pub c: usize,
    pub t: usize,
}

impl Dims5 {
    pub fn volume_dims(self) -> Dims {
        Dims {
            x: self.x,
            y: self.y,
            z: self.z,
        }
    }

    pub fn volume_voxels(self) -> Result<usize> {
        self.volume_dims().voxels()
    }

    pub fn volumes(self) -> Result<usize> {
        self.t
            .checked_mul(self.c)
            .ok_or_else(|| RadeltaError("T*C dimensions overflow usize".into()))
    }

    pub fn voxels(self) -> Result<usize> {
        self.volume_voxels()?
            .checked_mul(self.volumes()?)
            .ok_or_else(|| RadeltaError("TCZYX dimensions overflow usize".into()))
    }

    pub(crate) fn validate(self) -> Result<()> {
        if self.x == 0 || self.y == 0 || self.z == 0 || self.c == 0 || self.t == 0 {
            return Err(RadeltaError("zero-sized TCZYX dimension".into()));
        }
        for value in [self.x, self.y, self.z, self.c, self.t] {
            if value > u32::MAX as usize {
                return Err(RadeltaError(
                    "TCZYX dimension exceeds u32 header range".into(),
                ));
            }
        }
        if self.volumes()? > u32::MAX as usize {
            return Err(RadeltaError(
                "T*C volume count exceeds u32 header range".into(),
            ));
        }
        self.voxels().map(|_| ())
    }
}

/// Choose an out-of-core Z-chunk depth for a TCZYX dataset.
///
/// This is an implementation heuristic shared by the CLI and file-writer API,
/// not part of the serialized format. Roughly one quarter of `memory_mib` is
/// reserved for raw uint16 pixels; the rest is left for q arrays, models and
/// encoded output. The pipelined writer applies this base heuristic to each
/// share of its bounded working set; this is not a hard process-RSS limit.
#[doc(hidden)]
pub fn choose_stream_chunk_depth(
    dims: Dims5,
    memory_mib: usize,
    block_depth: usize,
) -> Result<usize> {
    dims.validate()?;
    let plane_bytes = dims
        .x
        .checked_mul(dims.y)
        .and_then(|v| v.checked_mul(2))
        .ok_or_else(|| RadeltaError("plane byte size overflow".into()))?;
    let working = memory_mib
        .checked_mul(1024usize * 1024usize)
        .ok_or_else(|| RadeltaError("memory budget overflow".into()))?;
    let raw_budget = (working / 4).max(plane_bytes);
    let mut z = (raw_budget / plane_bytes).max(1).min(dims.z);
    if z > block_depth && block_depth > 0 {
        z = (z / block_depth) * block_depth;
        z = z.max(block_depth).min(dims.z);
    }
    Ok(z.max(1))
}

#[derive(Debug, Clone)]
pub struct NDCompressionStats {
    pub raw_bytes: usize,
    pub total_bytes: usize,
    pub q_stream_bytes: usize,
    pub r_stream_bytes: usize,
    pub model_bytes: usize,
    pub blocks: usize,
    pub volumes: usize,
    pub q_max: u8,
    pub r_max: u16,
    pub seconds_total: f64,
}

#[derive(Debug, Clone)]
pub struct CompressionStats {
    pub raw_bytes: usize,
    pub total_bytes: usize,
    pub q_stream_bytes: usize,
    pub r_stream_bytes: usize,
    pub model_bytes: usize,
    pub blocks: usize,
    pub q_max: u8,
    pub r_max: u16,
    pub seconds_total: f64,
    pub seconds_q_extract: f64,
    pub seconds_histogram: f64,
    pub seconds_encode: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::test_support::structured_volume;
    use crate::codec::{compress_lossy_u16, compress_u16};

    #[test]
    fn rust_default_uses_fast_decode_settings() {
        let defaults = Options::default();
        assert_eq!(defaults.block_depth, 4);
        assert_eq!(defaults.context_mode, DEFAULT_CONTEXT_MODE as u32);
        assert_eq!(defaults.scale_bits, DEFAULT_SCALE_BITS as u32);

        let lossy_defaults = LossyOptions::default();
        assert_eq!(lossy_defaults.block_depth, 4);
        assert_eq!(lossy_defaults.context_mode, DEFAULT_CONTEXT_MODE as u32);
        assert_eq!(lossy_defaults.scale_bits, DEFAULT_SCALE_BITS as u32);
    }

    #[test]
    fn zero_block_depth_and_scale_bits_use_documented_defaults() {
        let dims = Dims { x: 11, y: 7, z: 4 };
        let input = structured_volume(dims);
        // context_mode=0 selects the explicit Signed3 mode.
        let zero = Options {
            block_depth: 0,
            context_mode: 0,
            scale_bits: 0,
        };
        let explicit = Options {
            block_depth: DEFAULT_BLOCK_DEPTH as u32,
            context_mode: ContextMode::Signed3 as u32,
            scale_bits: DEFAULT_SCALE_BITS as u32,
        };
        assert_eq!(
            compress_u16(&input, dims, zero).unwrap(),
            compress_u16(&input, dims, explicit).unwrap()
        );
    }

    #[test]
    fn invalid_scale_bits_are_rejected() {
        let dims = Dims { x: 4, y: 4, z: 2 };
        let input = structured_volume(dims);
        for scale_bits in [7u32, 12, 14, 15, 266] {
            let options = Options {
                scale_bits,
                ..Options::default()
            };
            assert!(compress_u16(&input, dims, options).is_err());
        }
    }

    #[test]
    fn invalid_context_modes_are_rejected_without_integer_wrapping() {
        let dims = Dims { x: 4, y: 4, z: 2 };
        let input = structured_volume(dims);
        for context_mode in [4u32, 255, 256, u32::MAX] {
            let options = Options {
                context_mode,
                ..Options::default()
            };
            assert!(compress_u16(&input, dims, options).is_err());
        }
    }

    #[test]
    fn zero_sized_dimensions_are_rejected() {
        let options = Options::default();
        for dims in [
            Dims { x: 0, y: 1, z: 1 },
            Dims { x: 1, y: 0, z: 1 },
            Dims { x: 1, y: 1, z: 0 },
        ] {
            assert!(compress_u16(&[], dims, options).is_err());
        }
    }

    #[test]
    fn invalid_lossy_calibration_is_rejected() {
        let dims = Dims { x: 2, y: 2, z: 1 };
        let input = vec![100u16; dims.voxels().unwrap()];
        for options in [
            LossyOptions {
                gain_e_per_adu: 0.0,
                ..LossyOptions::default()
            },
            LossyOptions {
                gain_e_per_adu: f64::NAN,
                ..LossyOptions::default()
            },
            LossyOptions {
                noise_step: 0.0,
                ..LossyOptions::default()
            },
            LossyOptions {
                noise_step: f64::INFINITY,
                ..LossyOptions::default()
            },
            LossyOptions {
                offset_adu: f64::NAN,
                ..LossyOptions::default()
            },
        ] {
            assert!(compress_lossy_u16(&input, dims, options).is_err());
        }
    }

    #[test]
    fn stream_chunk_depth_respects_memory_budget_and_block_alignment() {
        let dims = Dims5 {
            x: 100,
            y: 100,
            z: 100,
            c: 1,
            t: 1,
        };
        assert_eq!(choose_stream_chunk_depth(dims, 1, 8).unwrap(), 8);
        // Even when the memory budget could hold the full 100-plane volume,
        // streaming chunks are rounded down to a whole codec block (8 planes).
        assert_eq!(choose_stream_chunk_depth(dims, 100, 8).unwrap(), 96);
    }
}
