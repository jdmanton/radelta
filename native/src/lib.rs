//! High-throughput codec for uint16 microscopy images.
//!
//! Supports XYZ volumes and canonical contiguous TCZYX datasets.
//!
//! Lossless representation:
//!     q = floor(sqrt(x))
//!     r = x - q*q
//!
//! Lossy representation (camera-calibrated):
//!     electrons = max((ADU - offset_adu) * gain_e_per_adu, 0)
//!     z = 2*sqrt(electrons)
//!     q = round(z / noise_step)
//!
//! Lossless q is coded with a compact causal spatial context and r with P(r|q).
//! Lossy mode omits r entirely and codes only the calibrated/quantised q stream.
//! Static probability tables are learned in a first pass, then independent
//! Z-slabs are encoded in parallel with 4-way interleaved byte-rANS.

use rayon::prelude::*;
use std::fmt;
use std::time::Instant;

pub mod metadata;
pub use metadata::{
    metadata_limit, read_file_metadata, read_metadata, set_file_metadata, set_metadata,
    set_metadata_limit, DEFAULT_METADATA_LIMIT_BYTES,
};
mod ffi;
mod file_api;

pub use ffi::{
    radelta_compress_lossy_u16, radelta_compress_lossy_u16_nd, radelta_compress_u16,
    radelta_compress_u16_nd, radelta_decompress_u16, radelta_decompress_u16_nd,
    radelta_get_metadata, radelta_get_metadata_limit, radelta_set_metadata,
    radelta_set_metadata_limit, RADELTA_BUFFER_TOO_SMALL, RADELTA_CODEC_ERROR,
    RADELTA_INVALID_ARGUMENT, RADELTA_OK,
};
pub use file_api::{RadeltaFileHandle, RadeltaFileInfo};

const MAGIC: &[u8; 4] = b"RDL1";
const MAGIC_LOSSY: &[u8; 4] = b"RDLQ";
const MAGIC_ND: &[u8; 4] = b"RDM2";
const MAGIC_ND_LOSSY: &[u8; 4] = b"RDQ2";
#[doc(hidden)]
pub const STREAM_MAGIC_LOSSLESS: &[u8; 4] = b"RDS3";
#[doc(hidden)]
pub const STREAM_MAGIC_LOSSY: &[u8; 4] = b"RQS3";
#[doc(hidden)]
pub const STREAM_VERSION: u16 = 1;
#[doc(hidden)]
pub const DEFAULT_STREAM_MEMORY_MIB: usize = 2048;
const VERSION: u16 = 1;
const ND_VERSION: u16 = 1;
const LOSSY_VERSION: u16 = 1;
const LANES: usize = 4;
const RANS_L: u32 = 1 << 23;
#[doc(hidden)]
pub const DEFAULT_BLOCK_DEPTH: usize = 4;
const DEFAULT_SCALE_BITS: u8 = 10;
const MIN_SCALE_BITS: u8 = 8;
const MAX_SCALE_BITS: u8 = 11;

// Model bounds for uint16 input. These limits are also used
// while parsing untrusted files so malformed model headers cannot request
// unbounded allocations before the enclosing stream is validated.
const MAX_Q_ALPHABET: usize = 256;
const MAX_R_ALPHABET: usize = 511;
const MAX_MODEL_CONTEXTS: usize = MAX_Q_ALPHABET * 5 * 5 + MAX_Q_ALPHABET + 1;

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
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Signed3),
            1 => Ok(Self::Signs),
            2 => Ok(Self::Mean),
            3 => Ok(Self::MeanSigns),
            _ => Err(RadeltaError(format!("unsupported context mode {v}"))),
        }
    }

    fn compact_count(self, q_alphabet: usize) -> usize {
        q_alphabet
            * match self {
                Self::Signed3 => 25,
                Self::Signs | Self::MeanSigns => 9,
                Self::Mean => 3,
            }
    }

    fn uses_mean(self) -> bool {
        matches!(self, Self::Mean | Self::MeanSigns)
    }
}

const DEFAULT_CONTEXT_MODE: ContextMode = ContextMode::Mean;

#[derive(Debug, Clone, Copy)]
struct ResolvedCodecOptions {
    block_depth: usize,
    mode: ContextMode,
    scale_bits: u8,
}

fn resolve_codec_options(
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

    fn validate(self) -> Result<()> {
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

    fn validate(self) -> Result<()> {
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
/// encoded output.
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

#[derive(Clone, Copy, Debug)]
struct BlockSpec {
    z0: usize,
    depth: usize,
}

#[derive(Debug)]
struct EncodedBlock {
    z0: usize,
    depth: usize,
    q_states: [u32; LANES],
    r_states: [u32; LANES],
    q_bytes: Vec<u8>,
    r_bytes: Vec<u8>,
}

#[derive(Debug)]
struct BorrowedEncodedBlock<'a> {
    depth: usize,
    q_states: [u32; LANES],
    r_states: [u32; LANES],
    q_bytes: &'a [u8],
    r_bytes: &'a [u8],
}

#[derive(Debug, Clone)]
struct Counts {
    q: Vec<u64>,
    r: Vec<u64>,
}

impl Counts {
    fn new(q_len: usize, r_len: usize) -> Self {
        Self {
            q: vec![0; q_len],
            r: vec![0; r_len],
        }
    }

    fn merge(mut self, other: Self) -> Self {
        for (a, b) in self.q.iter_mut().zip(other.q) {
            *a += b;
        }
        for (a, b) in self.r.iter_mut().zip(other.r) {
            *a += b;
        }
        self
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct EncodeSymbol {
    reciprocal: u64,
    renorm_threshold: u32,
    cum: u16,
    complement: u16,
}

#[derive(Debug, Clone)]
struct StaticModel {
    contexts: usize,
    alphabet: usize,
    scale_bits: u8,
    total: usize,
    freq: Vec<u16>,
    cum: Vec<u16>,
    // Only models trained for compression need an encoder lookup table.
    encode: Vec<EncodeSymbol>,
}

impl StaticModel {
    fn from_counts(
        counts: &[u64],
        contexts: usize,
        alphabet: usize,
        scale_bits: u8,
    ) -> Result<Self> {
        let entries = contexts
            .checked_mul(alphabet)
            .ok_or_else(|| RadeltaError("model dimensions overflow usize".into()))?;
        if counts.len() != entries {
            return Err(RadeltaError("model count-array size mismatch".into()));
        }
        if !(MIN_SCALE_BITS..=MAX_SCALE_BITS).contains(&scale_bits) {
            return Err(RadeltaError(format!(
                "scale_bits must be in {MIN_SCALE_BITS}..={MAX_SCALE_BITS}"
            )));
        }
        let total = 1usize << scale_bits;
        if alphabet > total {
            return Err(RadeltaError(format!(
                "alphabet {alphabet} exceeds rANS total {total}"
            )));
        }

        let mut freq = vec![0u16; counts.len()];
        let mut cum = vec![0u16; counts.len()];
        let mut encode = vec![EncodeSymbol::default(); counts.len()];
        let reciprocals = Self::encoding_reciprocals(total);
        let renorm_factor = (RANS_L >> scale_bits) << 8;

        for ctx in 0..contexts {
            let row = &counts[ctx * alphabet..(ctx + 1) * alphabet];
            let f = normalize_row(row, total)?;
            if f.iter().all(|&v| v == 0) {
                continue;
            }
            let mut c = 0usize;
            for sym in 0..alphabet {
                let fv = f[sym] as usize;
                freq[ctx * alphabet + sym] = f[sym];
                cum[ctx * alphabet + sym] = c as u16;
                if fv != 0 {
                    encode[ctx * alphabet + sym] = EncodeSymbol {
                        reciprocal: reciprocals[fv],
                        renorm_threshold: renorm_factor * fv as u32,
                        cum: c as u16,
                        complement: (total - fv) as u16,
                    };
                    c += fv;
                }
            }
            if c != total {
                return Err(RadeltaError(format!(
                    "normalized row {ctx} sums to {c}, expected {total}"
                )));
            }
        }

        Ok(Self {
            contexts,
            alphabet,
            scale_bits,
            total,
            freq,
            cum,
            encode,
        })
    }

    fn from_freqs(
        contexts: usize,
        alphabet: usize,
        scale_bits: u8,
        freq: Vec<u16>,
    ) -> Result<Self> {
        if contexts == 0 || contexts > MAX_MODEL_CONTEXTS {
            return Err(RadeltaError(
                "serialized model context count is out of range".into(),
            ));
        }
        if alphabet == 0 || alphabet > MAX_R_ALPHABET {
            return Err(RadeltaError(
                "serialized model alphabet is out of range".into(),
            ));
        }
        if !(MIN_SCALE_BITS..=MAX_SCALE_BITS).contains(&scale_bits) {
            return Err(RadeltaError(format!(
                "serialized model scale_bits must be in {MIN_SCALE_BITS}..={MAX_SCALE_BITS}"
            )));
        }
        let entries = contexts
            .checked_mul(alphabet)
            .ok_or_else(|| RadeltaError("serialized model dimensions overflow usize".into()))?;
        if freq.len() != entries {
            return Err(RadeltaError("serialized model size mismatch".into()));
        }
        let total = 1usize << scale_bits;
        if alphabet > total {
            return Err(RadeltaError(format!(
                "serialized model alphabet {alphabet} exceeds rANS total {total}"
            )));
        }
        let mut cum = vec![0u16; freq.len()];
        for ctx in 0..contexts {
            let row = &freq[ctx * alphabet..(ctx + 1) * alphabet];
            let row_sum: usize = row.iter().map(|&v| v as usize).sum();
            if row_sum == 0 {
                continue;
            }
            if row_sum != total {
                return Err(RadeltaError(format!(
                    "serialized model row {ctx} sums to {row_sum}, expected {total}"
                )));
            }
            let mut c = 0usize;
            for sym in 0..alphabet {
                let fv = row[sym] as usize;
                cum[ctx * alphabet + sym] = c as u16;
                c += fv;
            }
        }
        Ok(Self {
            contexts,
            alphabet,
            scale_bits,
            total,
            freq,
            cum,
            encode: Vec::new(),
        })
    }

    fn encoding_reciprocals(total: usize) -> Vec<u64> {
        let numerator = (total as u64) << 32;
        std::iter::once(0)
            .chain((1..=total).map(|freq| numerator.div_ceil(freq as u64)))
            .collect()
    }

    #[inline(always)]
    fn encode_symbol(&self, ctx: usize, sym: usize) -> Result<&EncodeSymbol> {
        if ctx >= self.contexts || sym >= self.alphabet {
            return Err(RadeltaError("model index out of range".into()));
        }
        let i = ctx * self.alphabet + sym;
        let symbol = self
            .encode
            .get(i)
            .ok_or_else(|| RadeltaError("model does not have an encode table".into()))?;
        if symbol.renorm_threshold == 0 {
            return Err(RadeltaError(format!(
                "attempted to encode unseen symbol {sym} in context {ctx}"
            )));
        }
        Ok(symbol)
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        put_u32(&mut out, self.contexts as u32);
        put_u16(&mut out, self.alphabet as u16);
        out.push(self.scale_bits);
        // Omit empty contexts and infer each row's last frequency.
        // Contexts and symbols are sorted; the first index is absolute and
        // subsequent indices are encoded as strictly positive deltas.
        out.push(0); // reserved
        let nonempty = self
            .freq
            .chunks_exact(self.alphabet)
            .filter(|row| row.iter().any(|&f| f != 0))
            .count();
        put_model_varint(&mut out, nonempty as u32);
        let mut previous_ctx = 0;
        for ctx in 0..self.contexts {
            let row = &self.freq[ctx * self.alphabet..(ctx + 1) * self.alphabet];
            let nz = row.iter().filter(|&&f| f != 0).count();
            if nz == 0 {
                continue;
            }
            put_model_varint(&mut out, (ctx - previous_ctx) as u32);
            previous_ctx = ctx;
            put_model_varint(&mut out, nz as u32);
            let mut previous_sym = 0;
            for (index, (sym, &f)) in row.iter().enumerate().filter(|(_, f)| **f != 0).enumerate() {
                put_model_varint(&mut out, (sym - previous_sym) as u32);
                previous_sym = sym;
                if index + 1 != nz {
                    put_model_varint(&mut out, f as u32);
                }
            }
        }
        Ok(out)
    }

    fn deserialize(data: &[u8]) -> Result<Self> {
        let mut rd = Reader::new(data);
        let contexts = rd.u32()? as usize;
        let alphabet = rd.u16()? as usize;
        let scale_bits = rd.u8()?;
        let _reserved = rd.u8()?;
        if contexts == 0 || contexts > MAX_MODEL_CONTEXTS {
            return Err(RadeltaError(
                "serialized model context count is out of range".into(),
            ));
        }
        if alphabet == 0 || alphabet > MAX_R_ALPHABET {
            return Err(RadeltaError(
                "serialized model alphabet is out of range".into(),
            ));
        }
        if !(MIN_SCALE_BITS..=MAX_SCALE_BITS).contains(&scale_bits) {
            return Err(RadeltaError(format!(
                "serialized model scale_bits must be in {MIN_SCALE_BITS}..={MAX_SCALE_BITS}"
            )));
        }
        let total = 1usize << scale_bits;
        if alphabet > total {
            return Err(RadeltaError(
                "serialized model alphabet exceeds rANS total".into(),
            ));
        }
        let entries = contexts
            .checked_mul(alphabet)
            .ok_or_else(|| RadeltaError("serialized model dimensions overflow usize".into()))?;
        let mut freq = vec![0u16; entries];
        let nonempty = read_model_varint(&mut rd)? as usize;
        if nonempty > contexts {
            return Err(RadeltaError(
                "invalid serialized model context count".into(),
            ));
        }
        let mut ctx = 0usize;
        for row_index in 0..nonempty {
            let delta = read_model_varint(&mut rd)? as usize;
            if row_index != 0 && delta == 0 {
                return Err(RadeltaError("duplicate serialized model context".into()));
            }
            ctx = ctx
                .checked_add(delta)
                .filter(|&value| value < contexts)
                .ok_or_else(|| RadeltaError("serialized model context is out of range".into()))?;
            let nz = read_model_varint(&mut rd)? as usize;
            if nz == 0 || nz > alphabet {
                return Err(RadeltaError("invalid serialized model symbol count".into()));
            }
            let mut sym = 0usize;
            let mut remaining = total;
            for index in 0..nz {
                let delta = read_model_varint(&mut rd)? as usize;
                if index != 0 && delta == 0 {
                    return Err(RadeltaError("duplicate serialized model symbol".into()));
                }
                sym = sym
                    .checked_add(delta)
                    .filter(|&value| value < alphabet)
                    .ok_or_else(|| {
                        RadeltaError("serialized model symbol is out of range".into())
                    })?;
                let f = if index + 1 == nz {
                    remaining
                } else {
                    read_model_varint(&mut rd)? as usize
                };
                // Reserve at least one count for every following symbol.
                let following = nz - index - 1;
                if f == 0 || f > remaining.saturating_sub(following) {
                    return Err(RadeltaError("invalid serialized model frequency".into()));
                }
                freq[ctx * alphabet + sym] = f as u16;
                remaining -= f;
            }
        }
        if !rd.is_done() {
            return Err(RadeltaError("trailing bytes in serialized model".into()));
        }
        Self::from_freqs(contexts, alphabet, scale_bits, freq)
    }
}

fn put_model_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_model_varint(rd: &mut Reader<'_>) -> Result<u32> {
    let mut value = 0u32;
    for shift in (0..=28).step_by(7) {
        let byte = rd.u8()?;
        if shift == 28 && byte & 0xf0 != 0 {
            return Err(RadeltaError("serialized model varint overflows u32".into()));
        }
        value |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            if shift != 0 && byte == 0 {
                return Err(RadeltaError("noncanonical serialized model varint".into()));
            }
            return Ok(value);
        }
    }
    Err(RadeltaError("unterminated serialized model varint".into()))
}

#[allow(clippy::unnecessary_sort_by)]
fn normalize_row(counts: &[u64], target: usize) -> Result<Vec<u16>> {
    let sum: u128 = counts.iter().map(|&v| v as u128).sum();
    if sum == 0 {
        return Ok(vec![0u16; counts.len()]);
    }
    let nz = counts.iter().filter(|&&v| v != 0).count();
    if nz > target {
        return Err(RadeltaError(format!(
            "{nz} nonzero symbols exceed normalization total {target}"
        )));
    }

    let mut out = vec![0u16; counts.len()];
    let mut rems: Vec<(u128, usize)> = Vec::with_capacity(nz);
    let mut total = 0usize;

    for (sym, &c) in counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let num = c as u128 * target as u128;
        let mut base = (num / sum) as usize;
        if base == 0 {
            base = 1;
        }
        out[sym] = base as u16;
        total += base;
        rems.push((num % sum, sym));
    }

    if total < target {
        rems.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let mut i = 0usize;
        while total < target {
            let sym = rems[i % rems.len()].1;
            out[sym] = out[sym].saturating_add(1);
            total += 1;
            i += 1;
        }
    } else if total > target {
        let mut order: Vec<usize> = out
            .iter()
            .enumerate()
            .filter_map(|(i, &f)| if f > 1 { Some(i) } else { None })
            .collect();
        while total > target {
            order.sort_unstable_by_key(|&i| std::cmp::Reverse(out[i]));
            let mut changed = false;
            for &sym in &order {
                if total == target {
                    break;
                }
                if out[sym] > 1 {
                    out[sym] -= 1;
                    total -= 1;
                    changed = true;
                }
            }
            if !changed {
                return Err(RadeltaError("could not normalize frequency row".into()));
            }
        }
    }

    Ok(out)
}

fn make_sqrt_lut() -> Vec<u8> {
    let mut lut = vec![0u8; 65536];
    let mut q: u32 = 0;
    for x in 0u32..=65535 {
        while (q + 1) * (q + 1) <= x {
            q += 1;
        }
        lut[x as usize] = q as u8;
    }
    lut
}

#[inline(always)]
fn median3_safe(a: u8, b: u8, c: u8) -> u8 {
    // max(min(a,b), min(max(a,b),c)) -- same formulation used in the
    // Python context analyzer.
    std::cmp::max(std::cmp::min(a, b), std::cmp::min(std::cmp::max(a, b), c))
}

#[inline(always)]
fn sign3(a: u8, b: u8) -> usize {
    if a < b {
        0
    } else if a == b {
        1
    } else {
        2
    }
}

#[inline(always)]
fn signed3_category(diff: i16) -> usize {
    if diff == 0 {
        0
    } else if diff == 1 {
        1
    } else if diff > 1 {
        2
    } else if diff == -1 {
        3
    } else {
        4
    }
}

#[inline(always)]
fn compact_context(mode: ContextMode, l: u8, u: u8, z: u8) -> usize {
    match mode {
        ContextMode::Signed3 => {
            let p = median3_safe(l, u, z) as usize;
            let c1 = signed3_category(l as i16 - u as i16);
            let c2 = signed3_category(z as i16 - p as i16);
            (p * 5 + c1) * 5 + c2
        }
        ContextMode::Signs => {
            let p = median3_safe(l, u, z) as usize;
            let c1 = sign3(l, u);
            let c2 = sign3(z, p as u8);
            (p * 3 + c1) * 3 + c2
        }
        ContextMode::Mean => l as usize + u as usize + z as usize,
        ContextMode::MeanSigns => (l as usize + u as usize + z as usize) * 3 + sign3(l, u),
    }
}

fn q_context_count(q_alphabet: usize, mode: ContextMode) -> usize {
    mode.compact_count(q_alphabet) + q_alphabet + 1
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn q_context(
    q: &[u8],
    dims: Dims,
    block_z0: usize,
    local_z: usize,
    y: usize,
    x: usize,
    mode: ContextMode,
    q_alphabet: usize,
) -> usize {
    let compact_count = mode.compact_count(q_alphabet);
    let gz = block_z0 + local_z;
    let plane = dims.x * dims.y;
    let idx = gz * plane + y * dims.x + x;

    if x == 0 {
        return compact_count + q_alphabet;
    }
    let l = q[idx - 1];
    if y == 0 || (local_z == 0 && !mode.uses_mean()) {
        return compact_count + l as usize;
    }
    let u = q[idx - dims.x];
    let pz = if local_z == 0 {
        ((l as u16 + u as u16) / 2) as u8
    } else {
        q[idx - plane]
    };
    compact_context(mode, l, u, pz)
}

#[inline(always)]
fn rans_encode_symbol(
    state: &mut u32,
    out: &mut Vec<u8>,
    model: &StaticModel,
    ctx: usize,
    sym: usize,
) -> Result<()> {
    let symbol = model.encode_symbol(ctx, sym)?;
    let scale_bits = model.scale_bits as u32;
    let mut x = *state;
    while x >= symbol.renorm_threshold {
        out.push(x as u8);
        x >>= 8;
    }
    // After normalization x < freq * 2^(31 - scale_bits). Multiplying by
    // ceil(2^(32 + scale_bits) / freq) therefore fits in u64, and its rounding
    // error is less than 1/freq. The quotient is exact, including freq == 1.
    let quotient = ((x as u64 * symbol.reciprocal) >> (32 + scale_bits)) as u32;
    *state = x + quotient * symbol.complement as u32 + symbol.cum as u32;
    Ok(())
}

// Packed decode tables shared by the lossless and lossy decoders. At every supported
// scale_bits value, all information needed for one rANS slot fits in one u32.
// Keeping symbol, frequency and cumulative frequency together avoids the second
// model lookup. These helpers do not change the serialized format.
// Every valid entry contains a nonzero frequency. Using zero for empty slots
// lets the allocator leave unused context pages zero-backed on sparse models.
const PACKED_INVALID: u32 = 0;

fn packed_q_decode_table(model: &StaticModel) -> Result<Vec<u32>> {
    if model.scale_bits > MAX_SCALE_BITS || model.alphabet > 256 {
        return Err(RadeltaError(
            "packed q decoder requires scale_bits<=11 and q<=255".into(),
        ));
    }
    let mut table = vec![PACKED_INVALID; model.contexts * model.total];
    for ctx in 0..model.contexts {
        let base = ctx * model.total;
        for s in 0..model.alphabet {
            let mi = ctx * model.alphabet + s;
            let freq = model.freq[mi] as u32;
            if freq == 0 {
                continue;
            }
            let cum = model.cum[mi] as u32;
            // q: sym[7:0], freq[19:8] (12 bits), cum[30:20] (11 bits)
            table[base + cum as usize..base + (cum + freq) as usize]
                .fill((s as u32) | (freq << 8) | (cum << 20));
        }
    }
    Ok(table)
}

fn packed_r_decode_table(model: &StaticModel) -> Result<Vec<u32>> {
    if model.scale_bits > MAX_SCALE_BITS || model.alphabet > 512 {
        return Err(RadeltaError(
            "packed residual decoder requires scale_bits<=11 and r<=511".into(),
        ));
    }
    let mut table = vec![PACKED_INVALID; model.contexts * model.total];
    for ctx in 0..model.contexts {
        let base = ctx * model.total;
        for s in 0..model.alphabet {
            let mi = ctx * model.alphabet + s;
            let freq = model.freq[mi] as u32;
            if freq == 0 {
                continue;
            }
            let cum = model.cum[mi] as u32;
            // r: sym[8:0], freq[20:9] (12 bits), cum[31:21] (11 bits)
            table[base + cum as usize..base + (cum + freq) as usize]
                .fill((s as u32) | (freq << 9) | (cum << 21));
        }
    }
    Ok(table)
}

#[inline(always)]
fn rans_decode_packed_q(
    state: &mut u32,
    bytes: &[u8],
    pos: &mut usize,
    table: &[u32],
    total: usize,
    scale_bits: u8,
    ctx: usize,
) -> Result<usize> {
    let mask = (1u32 << scale_bits) - 1;
    let slot = (*state & mask) as usize;
    let entry = table[ctx * total + slot];
    if entry == PACKED_INVALID {
        return Err(RadeltaError("invalid packed q rANS slot".into()));
    }
    let sym = (entry & 0xff) as usize;
    let freq = (entry >> 8) & 0xfff;
    let cum = (entry >> 20) & 0x7ff;
    let mut x = freq * (*state >> scale_bits) + (slot as u32 - cum);
    while x < RANS_L {
        if *pos == 0 {
            return Err(RadeltaError("truncated q rANS byte stack".into()));
        }
        *pos -= 1;
        x = (x << 8) | bytes[*pos] as u32;
    }
    *state = x;
    Ok(sym)
}

#[inline(always)]
fn rans_decode_packed_r(
    state: &mut u32,
    bytes: &[u8],
    pos: &mut usize,
    table: &[u32],
    total: usize,
    scale_bits: u8,
    ctx: usize,
) -> Result<usize> {
    let mask = (1u32 << scale_bits) - 1;
    let slot = (*state & mask) as usize;
    let entry = table[ctx * total + slot];
    if entry == PACKED_INVALID {
        return Err(RadeltaError("invalid packed residual rANS slot".into()));
    }
    let sym = (entry & 0x1ff) as usize;
    let freq = (entry >> 9) & 0xfff;
    let cum = (entry >> 21) & 0x7ff;
    let mut x = freq * (*state >> scale_bits) + (slot as u32 - cum);
    while x < RANS_L {
        if *pos == 0 {
            return Err(RadeltaError("truncated residual rANS byte stack".into()));
        }
        *pos -= 1;
        x = (x << 8) | bytes[*pos] as u32;
    }
    *state = x;
    Ok(sym)
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn decode_q_packed_one(
    out: &mut [u16],
    i: usize,
    lz: usize,
    y: usize,
    x: usize,
    plane: usize,
    width: usize,
    compact_count: usize,
    q_alphabet: usize,
    mode: ContextMode,
    state: &mut u32,
    bytes: &[u8],
    pos: &mut usize,
    table: &[u32],
    total: usize,
    scale_bits: u8,
) -> Result<()> {
    let ctx = if x == 0 {
        compact_count + q_alphabet
    } else {
        let l = out[i - 1] as u8;
        if y == 0 || (lz == 0 && !mode.uses_mean()) {
            compact_count + l as usize
        } else {
            let u = out[i - width] as u8;
            let z = if lz == 0 {
                ((l as u16 + u as u16) / 2) as u8
            } else {
                out[i - plane] as u8
            };
            compact_context(mode, l, u, z)
        }
    };
    let sym = rans_decode_packed_q(state, bytes, pos, table, total, scale_bits, ctx)?;
    if sym >= q_alphabet {
        return Err(RadeltaError("decoded packed q symbol out of range".into()));
    }
    out[i] = sym as u16;
    Ok(())
}

#[inline(always)]
fn decode_r_packed_one(
    value: &mut u16,
    state: &mut u32,
    bytes: &[u8],
    pos: &mut usize,
    table: &[u32],
    total: usize,
    scale_bits: u8,
) -> Result<()> {
    let qs = *value as usize;
    // q=0 implies the sole remainder r=0. Its full-frequency rANS symbol
    // leaves the lane untouched; verify the table is that identity model
    // before bypassing it, including for streams supplied by a caller.
    if qs == 0 && *state >= RANS_L && table[0] == (total as u32) << 9 {
        return Ok(());
    }
    let r = rans_decode_packed_r(state, bytes, pos, table, total, scale_bits, qs)?;
    let qq = qs as u32;
    let reconstructed = qq * qq + r as u32;
    if reconstructed > u16::MAX as u32 {
        return Err(RadeltaError("decoded uint16 value overflow".into()));
    }
    *value = reconstructed as u16;
    Ok(())
}

fn block_specs(z: usize, block_depth: usize) -> Vec<BlockSpec> {
    let mut out = Vec::new();
    let mut z0 = 0usize;
    while z0 < z {
        let depth = block_depth.min(z - z0);
        out.push(BlockSpec { z0, depth });
        z0 += depth;
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn accumulate_block_counts(
    acc: &mut Counts,
    input: &[u16],
    q: &[u8],
    dims: Dims,
    block: BlockSpec,
    mode: ContextMode,
    q_alphabet: usize,
    r_alphabet: usize,
) {
    let plane = dims.x * dims.y;
    for lz in 0..block.depth {
        let gz = block.z0 + lz;
        let zbase = gz * plane;
        for y in 0..dims.y {
            let row = zbase + y * dims.x;
            for x in 0..dims.x {
                let idx = row + x;
                let qs = q[idx] as usize;
                let ctx = q_context(q, dims, block.z0, lz, y, x, mode, q_alphabet);
                acc.q[ctx * q_alphabet + qs] += 1;

                let qq = qs as u16;
                let r = input[idx] - qq * qq;
                acc.r[qs * r_alphabet + r as usize] += 1;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_block(
    input: &[u16],
    q: &[u8],
    dims: Dims,
    block: BlockSpec,
    mode: ContextMode,
    q_alphabet: usize,
    q_model: &StaticModel,
    r_model: &StaticModel,
) -> Result<EncodedBlock> {
    let plane = dims.x * dims.y;
    let block_voxels = block.depth * plane;
    let mut q_states = [RANS_L; LANES];
    let mut r_states = [RANS_L; LANES];
    let mut q_bytes = Vec::with_capacity(block_voxels / 8 + 1024);
    let mut r_bytes = Vec::with_capacity(block_voxels / 8 + 1024);

    for lz in (0..block.depth).rev() {
        let gz = block.z0 + lz;
        let zbase = gz * plane;
        for y in (0..dims.y).rev() {
            let row = zbase + y * dims.x;
            for x in (0..dims.x).rev() {
                let local_i = lz * plane + y * dims.x + x;
                let lane = local_i & (LANES - 1);
                let idx = row + x;
                let qs = q[idx] as usize;
                let qctx = q_context(q, dims, block.z0, lz, y, x, mode, q_alphabet);
                rans_encode_symbol(&mut q_states[lane], &mut q_bytes, q_model, qctx, qs)?;

                // Zero-valued pixels carry no remainder information. Omitting
                // this identity operation preserves lane order and byte output.
                if qs != 0 {
                    let qq = qs as u16;
                    let r = (input[idx] - qq * qq) as usize;
                    rans_encode_symbol(&mut r_states[lane], &mut r_bytes, r_model, qs, r)?;
                }
            }
        }
    }

    Ok(EncodedBlock {
        z0: block.z0,
        depth: block.depth,
        q_states,
        r_states,
        q_bytes,
        r_bytes,
    })
}

struct PackedDecodeTables<'a> {
    q: &'a [u32],
    r: &'a [u32],
    total: usize,
    scale_bits: u8,
}

fn decode_q_block_into_packed(
    block: &BorrowedEncodedBlock<'_>,
    dims: Dims,
    mode: ContextMode,
    q_alphabet: usize,
    tables: &PackedDecodeTables<'_>,
    out: &mut [u16],
) -> Result<()> {
    let plane = dims.x * dims.y;
    let n = block.depth * plane;
    if out.len() != n {
        return Err(RadeltaError(
            "packed direct decode output block has wrong length".into(),
        ));
    }

    let mut q_states = block.q_states;
    let mut q_pos = block.q_bytes.len();
    let compact_count = mode.compact_count(q_alphabet);

    // Align each row to lane 0, then decode groups of four with explicit lane
    // states. Contexts remain strictly causal because symbols are still decoded
    // in normal X order.
    for lz in 0..block.depth {
        for y in 0..dims.y {
            let row0 = lz * plane + y * dims.x;
            let mut x = 0usize;
            while x < dims.x && ((row0 + x) & 3) != 0 {
                let i = row0 + x;
                let lane = i & 3;
                decode_q_packed_one(
                    out,
                    i,
                    lz,
                    y,
                    x,
                    plane,
                    dims.x,
                    compact_count,
                    q_alphabet,
                    mode,
                    &mut q_states[lane],
                    block.q_bytes,
                    &mut q_pos,
                    tables.q,
                    tables.total,
                    tables.scale_bits,
                )?;
                x += 1;
            }
            while x + 4 <= dims.x {
                let i = row0 + x;
                decode_q_packed_one(
                    out,
                    i,
                    lz,
                    y,
                    x,
                    plane,
                    dims.x,
                    compact_count,
                    q_alphabet,
                    mode,
                    &mut q_states[0],
                    block.q_bytes,
                    &mut q_pos,
                    tables.q,
                    tables.total,
                    tables.scale_bits,
                )?;
                decode_q_packed_one(
                    out,
                    i + 1,
                    lz,
                    y,
                    x + 1,
                    plane,
                    dims.x,
                    compact_count,
                    q_alphabet,
                    mode,
                    &mut q_states[1],
                    block.q_bytes,
                    &mut q_pos,
                    tables.q,
                    tables.total,
                    tables.scale_bits,
                )?;
                decode_q_packed_one(
                    out,
                    i + 2,
                    lz,
                    y,
                    x + 2,
                    plane,
                    dims.x,
                    compact_count,
                    q_alphabet,
                    mode,
                    &mut q_states[2],
                    block.q_bytes,
                    &mut q_pos,
                    tables.q,
                    tables.total,
                    tables.scale_bits,
                )?;
                decode_q_packed_one(
                    out,
                    i + 3,
                    lz,
                    y,
                    x + 3,
                    plane,
                    dims.x,
                    compact_count,
                    q_alphabet,
                    mode,
                    &mut q_states[3],
                    block.q_bytes,
                    &mut q_pos,
                    tables.q,
                    tables.total,
                    tables.scale_bits,
                )?;
                x += 4;
            }
            while x < dims.x {
                let i = row0 + x;
                let lane = i & 3;
                decode_q_packed_one(
                    out,
                    i,
                    lz,
                    y,
                    x,
                    plane,
                    dims.x,
                    compact_count,
                    q_alphabet,
                    mode,
                    &mut q_states[lane],
                    block.q_bytes,
                    &mut q_pos,
                    tables.q,
                    tables.total,
                    tables.scale_bits,
                )?;
                x += 1;
            }
        }
    }
    if q_pos != 0 {
        return Err(RadeltaError(format!(
            "packed q rANS stack has {q_pos} unread bytes"
        )));
    }

    Ok(())
}

fn decode_block_into_packed(
    block: &BorrowedEncodedBlock<'_>,
    dims: Dims,
    mode: ContextMode,
    q_alphabet: usize,
    tables: &PackedDecodeTables<'_>,
    out: &mut [u16],
) -> Result<()> {
    decode_q_block_into_packed(block, dims, mode, q_alphabet, tables, out)?;

    // Residuals have no spatial dependency, so groups of four map directly to
    // the four serialized rANS lanes and can be explicitly unrolled.
    let mut r_states = block.r_states;
    let mut r_pos = block.r_bytes.len();
    let mut i = 0usize;
    while i + 4 <= out.len() {
        let (head, tail) = out[i..i + 4].split_at_mut(1);
        decode_r_packed_one(
            &mut head[0],
            &mut r_states[0],
            block.r_bytes,
            &mut r_pos,
            tables.r,
            tables.total,
            tables.scale_bits,
        )?;
        let (head, tail) = tail.split_at_mut(1);
        decode_r_packed_one(
            &mut head[0],
            &mut r_states[1],
            block.r_bytes,
            &mut r_pos,
            tables.r,
            tables.total,
            tables.scale_bits,
        )?;
        let (head, tail) = tail.split_at_mut(1);
        decode_r_packed_one(
            &mut head[0],
            &mut r_states[2],
            block.r_bytes,
            &mut r_pos,
            tables.r,
            tables.total,
            tables.scale_bits,
        )?;
        decode_r_packed_one(
            &mut tail[0],
            &mut r_states[3],
            block.r_bytes,
            &mut r_pos,
            tables.r,
            tables.total,
            tables.scale_bits,
        )?;
        i += 4;
    }
    while i < out.len() {
        let lane = i & 3;
        decode_r_packed_one(
            &mut out[i],
            &mut r_states[lane],
            block.r_bytes,
            &mut r_pos,
            tables.r,
            tables.total,
            tables.scale_bits,
        )?;
        i += 1;
    }
    if r_pos != 0 {
        return Err(RadeltaError(format!(
            "packed residual rANS stack has {r_pos} unread bytes"
        )));
    }
    Ok(())
}

fn accumulate_q_block_counts(
    acc: &mut [u64],
    q: &[u8],
    dims: Dims,
    block: BlockSpec,
    mode: ContextMode,
    q_alphabet: usize,
) {
    let plane = dims.x * dims.y;
    for lz in 0..block.depth {
        let gz = block.z0 + lz;
        let zbase = gz * plane;
        for y in 0..dims.y {
            let row = zbase + y * dims.x;
            for x in 0..dims.x {
                let idx = row + x;
                let qs = q[idx] as usize;
                let ctx = q_context(q, dims, block.z0, lz, y, x, mode, q_alphabet);
                acc[ctx * q_alphabet + qs] += 1;
            }
        }
    }
}

fn encode_q_only_block(
    q: &[u8],
    dims: Dims,
    block: BlockSpec,
    mode: ContextMode,
    q_alphabet: usize,
    q_model: &StaticModel,
) -> Result<EncodedBlock> {
    let plane = dims.x * dims.y;
    let block_voxels = block.depth * plane;
    let mut q_states = [RANS_L; LANES];
    let mut q_bytes = Vec::with_capacity(block_voxels / 8 + 1024);

    for lz in (0..block.depth).rev() {
        let gz = block.z0 + lz;
        let zbase = gz * plane;
        for y in (0..dims.y).rev() {
            let row = zbase + y * dims.x;
            for x in (0..dims.x).rev() {
                let local_i = lz * plane + y * dims.x + x;
                let lane = local_i & (LANES - 1);
                let idx = row + x;
                let qs = q[idx] as usize;
                let qctx = q_context(q, dims, block.z0, lz, y, x, mode, q_alphabet);
                rans_encode_symbol(&mut q_states[lane], &mut q_bytes, q_model, qctx, qs)?;
            }
        }
    }

    Ok(EncodedBlock {
        z0: block.z0,
        depth: block.depth,
        q_states,
        r_states: [RANS_L; LANES],
        q_bytes,
        r_bytes: Vec::new(),
    })
}

#[inline(always)]
fn lossy_quantize(v: u16, offset_adu: f64, gain_e_per_adu: f64, noise_step: f64) -> u16 {
    let electrons = ((v as f64 - offset_adu) * gain_e_per_adu).max(0.0);
    ((2.0 * electrons.sqrt()) / noise_step).round() as u16
}

#[inline(always)]
fn lossy_reconstruct(q: u8, offset_adu: f64, gain_e_per_adu: f64, noise_step: f64) -> u16 {
    let z = q as f64 * noise_step;
    let electrons = 0.25 * z * z;
    let adu = offset_adu + electrons / gain_e_per_adu;
    adu.round().clamp(0.0, u16::MAX as f64) as u16
}

fn extract_lossy_q(input: &[u16], options: LossyOptions) -> Result<(Vec<u8>, u8)> {
    let max_input = input.par_iter().copied().max().unwrap_or(0);
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
        input.par_iter().map(|&v| lookup[v as usize]).collect()
    } else {
        input.par_iter().map(|&v| quantize(v) as u8).collect()
    };
    Ok((q, q_max16 as u8))
}

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
    // Limit large private histograms to one per worker. Rayon's adaptive fold
    // otherwise allocates and merges many tables when a volume has many slabs.
    let histogram_batch_size = blocks.len().div_ceil(rayon::current_num_threads());
    let q_counts = blocks
        .par_chunks(histogram_batch_size)
        .map(|batch| {
            let mut acc = vec![0u64; q_count_len];
            for &block in batch {
                accumulate_q_block_counts(&mut acc, &q, dims, block, mode, q_alphabet);
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
        .map(|&block| encode_q_only_block(&q, dims, block, mode, q_alphabet, &q_model))
        .collect();
    let mut encoded = Vec::with_capacity(encoded_results.len());
    for b in encoded_results {
        encoded.push(b?);
    }
    let seconds_encode = encode_start.elapsed().as_secs_f64();

    let q_model_blob = q_model.serialize()?;
    let q_stream_bytes: usize = encoded.iter().map(|b| b.q_bytes.len()).sum();
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

    for b in &encoded {
        put_u32(&mut out, b.z0 as u32);
        put_u32(&mut out, b.depth as u32);
        put_u32(&mut out, b.q_bytes.len() as u32);
        for &st in &b.q_states {
            put_u32(&mut out, st);
        }
        out.extend_from_slice(&b.q_bytes);
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

    let max_x = input.par_iter().copied().max().unwrap_or(0);
    let sqrt_lut = make_sqrt_lut();
    let q_max = sqrt_lut[max_x as usize];
    let q_alphabet = q_max as usize + 1;
    let r_max = 2u16 * q_max as u16;
    let r_alphabet = r_max as usize + 1;

    let q_start = Instant::now();
    let q: Vec<u8> = input.par_iter().map(|&v| sqrt_lut[v as usize]).collect();
    let seconds_q_extract = q_start.elapsed().as_secs_f64();

    let blocks = block_specs(dims.z, block_depth);
    let q_contexts = q_context_count(q_alphabet, mode);
    let q_count_len = q_contexts * q_alphabet;
    let r_count_len = q_alphabet * r_alphabet;

    let hist_start = Instant::now();
    let histogram_batch_size = blocks.len().div_ceil(rayon::current_num_threads());
    let counts = blocks
        .par_chunks(histogram_batch_size)
        .map(|batch| {
            let mut acc = Counts::new(q_count_len, r_count_len);
            for &block in batch {
                accumulate_block_counts(
                    &mut acc, input, &q, dims, block, mode, q_alphabet, r_alphabet,
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
        .map(|&block| encode_block(input, &q, dims, block, mode, q_alphabet, &q_model, &r_model))
        .collect();
    let mut encoded = Vec::with_capacity(encoded_results.len());
    for b in encoded_results {
        encoded.push(b?);
    }
    let seconds_encode = encode_start.elapsed().as_secs_f64();

    let q_model_blob = q_model.serialize()?;
    let r_model_blob = r_model.serialize()?;
    let q_stream_bytes: usize = encoded.iter().map(|b| b.q_bytes.len()).sum();
    let r_stream_bytes: usize = encoded.iter().map(|b| b.r_bytes.len()).sum();
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

    for b in &encoded {
        put_u32(&mut out, b.z0 as u32);
        put_u32(&mut out, b.depth as u32);
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

struct ParsedLosslessStream<'a> {
    dims: Dims,
    block_depth: usize,
    mode: ContextMode,
    scale_bits: u8,
    q_alphabet: usize,
    q_model: StaticModel,
    r_model: StaticModel,
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

    let q_model = StaticModel::deserialize(rd.take(q_model_len)?)?;
    let r_model = StaticModel::deserialize(rd.take(r_model_len)?)?;
    if q_model.scale_bits != scale_bits || r_model.scale_bits != scale_bits {
        return Err(RadeltaError("model/header scale-bit mismatch".into()));
    }
    let q_alphabet = q_max + 1;
    let r_alphabet = r_max + 1;
    if q_model.alphabet != q_alphabet
        || r_model.contexts != q_alphabet
        || r_model.alphabet != r_alphabet
    {
        return Err(RadeltaError("model/header alphabet mismatch".into()));
    }
    if q_model.contexts != q_context_count(q_alphabet, mode) {
        return Err(RadeltaError("q context-table shape mismatch".into()));
    }

    let mut blocks = Vec::with_capacity(nblocks);
    for block_index in 0..nblocks {
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
        let mut q_states = [0u32; LANES];
        let mut r_states = [0u32; LANES];
        for state in &mut q_states {
            *state = rd.u32()?;
        }
        for state in &mut r_states {
            *state = rd.u32()?;
        }
        let q_bytes = rd.take(q_len)?;
        let r_bytes = rd.take(r_len)?;
        blocks.push(BorrowedEncodedBlock {
            depth,
            q_states,
            r_states,
            q_bytes,
            r_bytes,
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

    let q_table = packed_q_decode_table(&stream.q_model)?;
    let r_table = packed_r_decode_table(&stream.r_model)?;
    let tables = PackedDecodeTables {
        q: &q_table,
        r: &r_table,
        total: stream.q_model.total,
        scale_bits: stream.scale_bits,
    };

    out.par_chunks_mut(nominal_chunk_voxels)
        .zip(stream.blocks.par_iter())
        .try_for_each(|(chunk, block)| {
            if chunk.len() != block.depth * plane {
                return Err(RadeltaError("decoded block/output layout mismatch".into()));
            }
            decode_block_into_packed(
                block,
                stream.dims,
                stream.mode,
                stream.q_alphabet,
                &tables,
                chunk,
            )
        })?;

    Ok(stream.dims)
}

fn decompress_lossless_u16_into(data: &[u8], out: &mut [u16]) -> Result<Dims> {
    let stream = parse_lossless_stream(data)?;
    decode_lossless_stream_into(&stream, out)
}

fn inspect_lossless_dims(data: &[u8]) -> Result<Dims> {
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
    q_model: StaticModel,
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
    let q_model = StaticModel::deserialize(rd.take(q_model_len)?)?;
    if q_model.scale_bits != scale_bits {
        return Err(RadeltaError("model/header scale-bit mismatch".into()));
    }
    let q_alphabet = q_max + 1;
    if q_model.alphabet != q_alphabet || q_model.contexts != q_context_count(q_alphabet, mode) {
        return Err(RadeltaError("lossy q model/header shape mismatch".into()));
    }

    let mut blocks = Vec::with_capacity(nblocks);
    for _ in 0..nblocks {
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
        let mut q_states = [0u32; LANES];
        for st in &mut q_states {
            *st = rd.u32()?;
        }
        let q_bytes = rd.take(q_len)?;
        blocks.push(BorrowedEncodedBlock {
            depth,
            q_states,
            r_states: [RANS_L; LANES],
            q_bytes,
            r_bytes: &[],
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
    let q_table = packed_q_decode_table(&stream.q_model)?;
    let tables = PackedDecodeTables {
        q: &q_table,
        r: &[],
        total: stream.q_model.total,
        scale_bits: stream.q_model.scale_bits,
    };
    out.par_chunks_mut(chunk_voxels)
        .zip(stream.blocks.par_iter())
        .try_for_each(|(chunk, block)| {
            // Context prediction needs q throughout this block. Reconstruct only
            // after all its q symbols are decoded; blocks are independent.
            decode_q_block_into_packed(
                block,
                stream.dims,
                stream.mode,
                stream.q_alphabet,
                &tables,
                chunk,
            )?;
            for pixel in chunk {
                *pixel = stream.reconstruction[*pixel as usize];
            }
            Ok(())
        })?;
    Ok(stream.dims)
}

fn decompress_lossy_u16(data: &[u8]) -> Result<(Vec<u16>, Dims)> {
    let stream = parse_lossy_stream(data)?;
    let mut out = vec![0u16; stream.dims.voxels()?];
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
        let mut out = vec![0u16; stream.dims.voxels()?];
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

fn inspect_lossy_dims(data: &[u8]) -> Result<Dims> {
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

/// Compress canonical contiguous T,C,Z,Y,X uint16 data losslessly.
/// Each (t,c) XYZ volume is coded independently using the existing RDL1
/// codec and wrapped in an RDM2 multidimensional container.
pub fn compress_u16_nd(input: &[u16], dims: Dims5, options: Options) -> Result<Vec<u8>> {
    compress_u16_nd_with_stats(input, dims, options).map(|v| v.0)
}

pub fn compress_u16_nd_with_stats(
    input: &[u16],
    dims: Dims5,
    options: Options,
) -> Result<(Vec<u8>, NDCompressionStats)> {
    let total_start = Instant::now();
    dims.validate()?;
    let expected = dims.voxels()?;
    if input.len() != expected {
        return Err(RadeltaError(format!(
            "input length {} does not match TCZYX dimensions ({expected} voxels)",
            input.len()
        )));
    }
    let volume_voxels = dims.volume_voxels()?;
    let nvol = dims.volumes()?;
    let d3 = dims.volume_dims();

    let results: Vec<Result<(Vec<u8>, CompressionStats)>> = input
        .par_chunks(volume_voxels)
        .map(|vol| compress_u16_with_stats(vol, d3, options))
        .collect();

    let mut streams = Vec::with_capacity(nvol);
    let mut q_stream_bytes = 0usize;
    let mut r_stream_bytes = 0usize;
    let mut model_bytes = 0usize;
    let mut blocks = 0usize;
    let mut q_max = 0u8;
    let mut r_max = 0u16;
    for res in results {
        let (stream, st) = res?;
        q_stream_bytes += st.q_stream_bytes;
        r_stream_bytes += st.r_stream_bytes;
        model_bytes += st.model_bytes;
        blocks += st.blocks;
        q_max = q_max.max(st.q_max);
        r_max = r_max.max(st.r_max);
        streams.push(stream);
    }
    if streams.len() != nvol {
        return Err(RadeltaError(
            "internal multidimensional stream count mismatch".into(),
        ));
    }

    let payload_bytes: usize = streams.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(payload_bytes + 64 + nvol * 8);
    out.extend_from_slice(MAGIC_ND);
    put_u16(&mut out, ND_VERSION);
    put_u16(&mut out, 0);
    put_u32(&mut out, dims.x as u32);
    put_u32(&mut out, dims.y as u32);
    put_u32(&mut out, dims.z as u32);
    put_u32(&mut out, dims.c as u32);
    put_u32(&mut out, dims.t as u32);
    put_u32(&mut out, nvol as u32);
    for stream in &streams {
        put_u64(&mut out, stream.len() as u64);
        out.extend_from_slice(stream);
    }

    let stats = NDCompressionStats {
        raw_bytes: input.len() * 2,
        total_bytes: out.len(),
        q_stream_bytes,
        r_stream_bytes,
        model_bytes,
        blocks,
        volumes: nvol,
        q_max,
        r_max,
        seconds_total: total_start.elapsed().as_secs_f64(),
    };
    Ok((out, stats))
}

/// Compress canonical contiguous T,C,Z,Y,X uint16 data using calibrated lossy mode.
/// Each (t,c) XYZ volume is coded independently using RDLQ and wrapped in RDQ2.
pub fn compress_lossy_u16_nd(input: &[u16], dims: Dims5, options: LossyOptions) -> Result<Vec<u8>> {
    compress_lossy_u16_nd_with_stats(input, dims, options).map(|v| v.0)
}

pub fn compress_lossy_u16_nd_with_stats(
    input: &[u16],
    dims: Dims5,
    options: LossyOptions,
) -> Result<(Vec<u8>, NDCompressionStats)> {
    let total_start = Instant::now();
    dims.validate()?;
    let expected = dims.voxels()?;
    if input.len() != expected {
        return Err(RadeltaError(format!(
            "input length {} does not match TCZYX dimensions ({expected} voxels)",
            input.len()
        )));
    }
    let volume_voxels = dims.volume_voxels()?;
    let nvol = dims.volumes()?;
    let d3 = dims.volume_dims();

    let results: Vec<Result<(Vec<u8>, CompressionStats)>> = input
        .par_chunks(volume_voxels)
        .map(|vol| compress_lossy_u16_with_stats(vol, d3, options))
        .collect();

    let mut streams = Vec::with_capacity(nvol);
    let mut q_stream_bytes = 0usize;
    let mut model_bytes = 0usize;
    let mut blocks = 0usize;
    let mut q_max = 0u8;
    for res in results {
        let (stream, st) = res?;
        q_stream_bytes += st.q_stream_bytes;
        model_bytes += st.model_bytes;
        blocks += st.blocks;
        q_max = q_max.max(st.q_max);
        streams.push(stream);
    }

    let payload_bytes: usize = streams.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(payload_bytes + 64 + nvol * 8);
    out.extend_from_slice(MAGIC_ND_LOSSY);
    put_u16(&mut out, ND_VERSION);
    put_u16(&mut out, 0);
    put_u32(&mut out, dims.x as u32);
    put_u32(&mut out, dims.y as u32);
    put_u32(&mut out, dims.z as u32);
    put_u32(&mut out, dims.c as u32);
    put_u32(&mut out, dims.t as u32);
    put_u32(&mut out, nvol as u32);
    for stream in &streams {
        put_u64(&mut out, stream.len() as u64);
        out.extend_from_slice(stream);
    }

    let stats = NDCompressionStats {
        raw_bytes: input.len() * 2,
        total_bytes: out.len(),
        q_stream_bytes,
        r_stream_bytes: 0,
        model_bytes,
        blocks,
        volumes: nvol,
        q_max,
        r_max: 0,
        seconds_total: total_start.elapsed().as_secs_f64(),
    };
    Ok((out, stats))
}

fn inspect_nd_header(data: &[u8], expected_magic: &[u8; 4]) -> Result<(Dims5, usize, usize)> {
    let mut rd = Reader::new(data);
    if rd.take(4)? != expected_magic {
        return Err(RadeltaError(
            "not the requested multidimensional Radelta stream".into(),
        ));
    }
    let version = rd.u16()?;
    if version != ND_VERSION {
        return Err(RadeltaError(format!(
            "unsupported multidimensional Radelta version {version}"
        )));
    }
    let _flags = rd.u16()?;
    let dims = Dims5 {
        x: rd.u32()? as usize,
        y: rd.u32()? as usize,
        z: rd.u32()? as usize,
        c: rd.u32()? as usize,
        t: rd.u32()? as usize,
    };
    dims.validate()?;
    let nvol = rd.u32()? as usize;
    if nvol != dims.volumes()? {
        return Err(RadeltaError(
            "multidimensional volume count does not match T*C".into(),
        ));
    }
    Ok((dims, nvol, rd.pos))
}

fn decompress_nd_container_into(
    data: &[u8],
    magic: &[u8; 4],
    lossy: bool,
    output: &mut [u16],
) -> Result<Dims5> {
    let (data, _) = metadata::split(data)?;
    let (dims, nvol, header_pos) = inspect_nd_header(data, magic)?;
    let needed = dims.voxels()?;
    if output.len() != needed {
        return Err(RadeltaError(format!(
            "output length {} does not match decoded voxel count {needed}",
            output.len()
        )));
    }

    let mut rd = Reader {
        data,
        pos: header_pos,
    };
    let mut slices: Vec<&[u8]> = Vec::with_capacity(nvol);
    for _ in 0..nvol {
        let len64 = rd.u64()?;
        let len = usize::try_from(len64)
            .map_err(|_| RadeltaError("substream length exceeds usize".into()))?;
        let sub = rd.take(len)?;
        if sub.len() < 4 {
            return Err(RadeltaError("truncated multidimensional substream".into()));
        }
        let expected = if lossy { MAGIC_LOSSY } else { MAGIC };
        if &sub[..4] != expected {
            return Err(RadeltaError(
                "multidimensional substream mode mismatch".into(),
            ));
        }
        slices.push(sub);
    }
    if !rd.is_done() {
        return Err(RadeltaError(
            "trailing bytes after multidimensional payload".into(),
        ));
    }

    let d3 = dims.volume_dims();
    let volume_voxels = d3.voxels()?;
    output
        .par_chunks_mut(volume_voxels)
        .zip(slices.par_iter())
        .try_for_each(|(chunk, sub)| {
            let sd = decompress_u16_into(sub, chunk)?;
            if sd != d3 {
                return Err(RadeltaError(
                    "multidimensional substream dimensions do not match container".into(),
                ));
            }
            Ok(())
        })?;

    Ok(dims)
}

/// Decode RDL1/RDLQ/RDM2/RDQ2 data directly into canonical TCZYX storage.
pub fn decompress_u16_nd_into(data: &[u8], output: &mut [u16]) -> Result<Dims5> {
    if data.len() < 4 {
        return Err(RadeltaError("truncated input".into()));
    }
    if &data[..4] == MAGIC_ND {
        decompress_nd_container_into(data, MAGIC_ND, false, output)
    } else if &data[..4] == MAGIC_ND_LOSSY {
        decompress_nd_container_into(data, MAGIC_ND_LOSSY, true, output)
    } else if &data[..4] == MAGIC || &data[..4] == MAGIC_LOSSY {
        let d = inspect_dims(data)?;
        if output.len() != d.voxels()? {
            return Err(RadeltaError(format!(
                "output length {} does not match decoded voxel count {}",
                output.len(),
                d.voxels()?
            )));
        }
        let decoded = decompress_u16_into(data, output)?;
        Ok(Dims5 {
            x: decoded.x,
            y: decoded.y,
            z: decoded.z,
            c: 1,
            t: 1,
        })
    } else {
        Err(RadeltaError(
            "not a Radelta RDL1/RDLQ/RDM2/RDQ2 stream".into(),
        ))
    }
}

/// Decode RDL1/RDLQ or multidimensional RDM2/RDQ2 data into canonical TCZYX.
pub fn decompress_u16_nd(data: &[u8]) -> Result<(Vec<u16>, Dims5)> {
    let dims = inspect_shape(data)?;
    let mut out = vec![0u16; dims.voxels()?];
    let decoded_dims = decompress_u16_nd_into(data, &mut out)?;
    Ok((out, decoded_dims))
}

pub fn inspect_shape(data: &[u8]) -> Result<Dims5> {
    if data.len() < 4 {
        return Err(RadeltaError("truncated input".into()));
    }
    if &data[..4] == MAGIC_ND {
        inspect_nd_header(data, MAGIC_ND).map(|v| v.0)
    } else if &data[..4] == MAGIC_ND_LOSSY {
        inspect_nd_header(data, MAGIC_ND_LOSSY).map(|v| v.0)
    } else if &data[..4] == MAGIC {
        inspect_lossless_dims(data).map(|d| Dims5 {
            x: d.x,
            y: d.y,
            z: d.z,
            c: 1,
            t: 1,
        })
    } else if &data[..4] == MAGIC_LOSSY {
        inspect_lossy_dims(data).map(|d| Dims5 {
            x: d.x,
            y: d.y,
            z: d.z,
            c: 1,
            t: 1,
        })
    } else {
        Err(RadeltaError("not a Radelta stream".into()))
    }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_le_bytes());
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| RadeltaError("input offset overflow".into()))?;
        if end > self.data.len() {
            return Err(RadeltaError("truncated input".into()));
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn f64(&mut self) -> Result<f64> {
        let b = self.take(8)?;
        Ok(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn is_done(&self) -> bool {
        self.pos == self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn structured_volume(dims: Dims) -> Vec<u16> {
        let mut data = Vec::with_capacity(dims.voxels().unwrap());
        for z in 0..dims.z {
            for y in 0..dims.y {
                for x in 0..dims.x {
                    data.push(((x * 17 + y * 31 + z * 101 + x * y * 3) % 65536) as u16);
                }
            }
        }
        data
    }

    fn golden_rdl1_expected() -> (Vec<u16>, Dims) {
        let dims = Dims { x: 7, y: 5, z: 3 };
        (structured_volume(dims), dims)
    }

    fn golden_rdm2_expected() -> (Vec<u16>, Dims5) {
        let dims = Dims5 {
            x: 5,
            y: 4,
            z: 3,
            c: 2,
            t: 2,
        };
        let mut data = Vec::with_capacity(dims.voxels().unwrap());
        for t in 0..dims.t {
            for c in 0..dims.c {
                for z in 0..dims.z {
                    for y in 0..dims.y {
                        for x in 0..dims.x {
                            data.push(
                                ((t * 10000 + c * 3000 + z * 200 + y * 30 + x * 7) % 65536) as u16,
                            );
                        }
                    }
                }
            }
        }
        (data, dims)
    }

    #[test]
    fn median_matches_sort() {
        for a in 0u8..10 {
            for b in 0u8..10 {
                for c in 0u8..10 {
                    let mut v = [a, b, c];
                    v.sort_unstable();
                    assert_eq!(median3_safe(a, b, c), v[1]);
                }
            }
        }
    }

    #[test]
    fn roundtrip_small_signed3() {
        let dims = Dims { x: 23, y: 11, z: 7 };
        let mut data = Vec::with_capacity(dims.voxels().unwrap());
        for z in 0..dims.z {
            for y in 0..dims.y {
                for x in 0..dims.x {
                    let v = ((x * 3 + y * 7 + z * 11 + (x * y) % 13) % 5000) as u16;
                    data.push(v);
                }
            }
        }
        let opts = Options {
            block_depth: 3,
            context_mode: ContextMode::Signed3 as u32,
            scale_bits: 10,
        };
        let enc = compress_u16(&data, dims, opts).unwrap();
        let (dec, dd) = decompress_u16(&enc).unwrap();
        assert_eq!(dd, dims);
        assert_eq!(dec, data);
    }

    #[test]
    fn roundtrip_small_signs() {
        let dims = Dims { x: 19, y: 9, z: 5 };
        let data: Vec<u16> = (0..dims.voxels().unwrap())
            .map(|i| ((i * 37 + (i / 7) * 11) % 65536) as u16)
            .collect();
        let opts = Options {
            block_depth: 2,
            context_mode: ContextMode::Signs as u32,
            scale_bits: 10,
        };
        let enc = compress_u16(&data, dims, opts).unwrap();
        let (dec, _) = decompress_u16(&enc).unwrap();
        assert_eq!(dec, data);
    }

    #[test]
    fn lossy_smoke_and_metadata_roundtrip() {
        let dims = Dims { x: 17, y: 8, z: 4 };
        let data: Vec<u16> = (0..dims.voxels().unwrap())
            .map(|i| 100u16 + ((i * 13 + (i / 5) * 3) % 1200) as u16)
            .collect();
        let opts = LossyOptions {
            block_depth: 2,
            context_mode: ContextMode::Signed3 as u32,
            scale_bits: 10,
            offset_adu: 100.0,
            gain_e_per_adu: 0.5,
            noise_step: 2.0,
        };
        let enc = compress_lossy_u16(&data, dims, opts).unwrap();
        assert_eq!(&enc[..4], MAGIC_LOSSY);
        let (dec, dd) = decompress_u16(&enc).unwrap();
        assert_eq!(dd, dims);
        assert_eq!(dec.len(), data.len());
        for (&a, &b) in data.iter().zip(dec.iter()) {
            let q = lossy_quantize(a, opts.offset_adu, opts.gain_e_per_adu, opts.noise_step);
            assert!(q <= 255);
            let expected = lossy_reconstruct(
                q as u8,
                opts.offset_adu,
                opts.gain_e_per_adu,
                opts.noise_step,
            );
            assert_eq!(b, expected);
        }
    }

    #[test]
    fn lossy_packed_decoder_roundtrips_all_context_modes_and_scales() {
        let dims = Dims { x: 11, y: 7, z: 5 };
        let input: Vec<u16> = (0..dims.voxels().unwrap())
            .map(|i| ((i * 137 + (i / 11) * 59) % 4096) as u16)
            .collect();
        let base_options = LossyOptions {
            block_depth: 2,
            offset_adu: 100.0,
            gain_e_per_adu: 0.5,
            noise_step: 2.0,
            ..LossyOptions::default()
        };
        let expected: Vec<u16> = input
            .iter()
            .map(|&v| {
                let q = lossy_quantize(v, 100.0, 0.5, 2.0);
                lossy_reconstruct(q as u8, 100.0, 0.5, 2.0)
            })
            .collect();
        for mode in [
            ContextMode::Signed3,
            ContextMode::Signs,
            ContextMode::Mean,
            ContextMode::MeanSigns,
        ] {
            for scale_bits in MIN_SCALE_BITS..=MAX_SCALE_BITS {
                let options = LossyOptions {
                    context_mode: mode as u32,
                    scale_bits: scale_bits as u32,
                    ..base_options
                };
                let encoded = compress_lossy_u16(&input, dims, options).unwrap();
                let (decoded, decoded_dims) = decompress_u16(&encoded).unwrap();
                assert_eq!(decoded_dims, dims);
                assert_eq!(decoded, expected, "mode={mode:?}, scale={scale_bits}");
                let mut direct = vec![u16::MAX; input.len()];
                assert_eq!(decompress_u16_into(&encoded, &mut direct).unwrap(), dims);
                assert_eq!(direct, expected, "direct mode={mode:?}, scale={scale_bits}");
                let mut wrong = vec![123; input.len() - 1];
                assert!(decompress_u16_into(&encoded, &mut wrong).is_err());
                assert!(wrong.iter().all(|&v| v == 123));
                assert!(decompress_u16_into(&encoded[..encoded.len() - 1], &mut direct).is_err());
            }
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
    fn nd_lossless_roundtrip_rectangular_tczyx() {
        let dims = Dims5 {
            x: 17,
            y: 9,
            z: 4,
            c: 3,
            t: 2,
        };
        let n = dims.voxels().unwrap();
        let input: Vec<u16> = (0..n)
            .map(|i| ((i * 37 + (i / 17) * 11) % 4096) as u16)
            .collect();
        let enc = compress_u16_nd(&input, dims, Options::default()).unwrap();
        let (dec, dd) = decompress_u16_nd(&enc).unwrap();
        assert_eq!(dd, dims);
        assert_eq!(dec, input);
    }

    #[test]
    fn nd_lossy_preserves_shape_and_rectangular_xy() {
        let dims = Dims5 {
            x: 13,
            y: 7,
            z: 3,
            c: 2,
            t: 2,
        };
        let n = dims.voxels().unwrap();
        let input: Vec<u16> = (0..n).map(|i| ((i * 19) % 3000) as u16).collect();
        let opts = LossyOptions {
            offset_adu: 0.0,
            gain_e_per_adu: 0.46,
            noise_step: 2.0,
            ..LossyOptions::default()
        };
        let enc = compress_lossy_u16_nd(&input, dims, opts).unwrap();
        let (dec, dd) = decompress_u16_nd(&enc).unwrap();
        assert_eq!(dd, dims);
        assert_eq!(dec.len(), input.len());
        let mut direct = vec![u16::MAX; n];
        assert_eq!(decompress_u16_nd_into(&enc, &mut direct).unwrap(), dims);
        for (&value, &actual) in input.iter().zip(&direct) {
            let q = lossy_quantize(value, opts.offset_adu, opts.gain_e_per_adu, opts.noise_step);
            assert_eq!(
                actual,
                lossy_reconstruct(
                    q as u8,
                    opts.offset_adu,
                    opts.gain_e_per_adu,
                    opts.noise_step
                )
            );
        }
        assert_eq!(direct, dec);
    }

    #[test]
    fn golden_rdl1_v1_decodes() {
        let encoded = include_bytes!("../tests/fixtures/rdl1_reference_v1.rdlt");
        let (expected, dims) = golden_rdl1_expected();
        let (decoded, decoded_dims) = decompress_u16(encoded).unwrap();
        assert_eq!(decoded_dims, dims);
        assert_eq!(decoded, expected);
        assert_eq!(inspect_dims(encoded).unwrap(), dims);
    }

    #[test]
    fn golden_rdm2_v1_decodes() {
        let encoded = include_bytes!("../tests/fixtures/rdm2_reference_v1.rdlt");
        let (expected, dims) = golden_rdm2_expected();
        let (decoded, decoded_dims) = decompress_u16_nd(encoded).unwrap();
        assert_eq!(decoded_dims, dims);
        assert_eq!(decoded, expected);
        assert_eq!(inspect_shape(encoded).unwrap(), dims);
    }

    #[test]
    fn lossless_roundtrip_extreme_values() {
        let dims = Dims { x: 9, y: 3, z: 2 };
        let pattern = [
            0u16, 1, 2, 3, 254, 255, 256, 1023, 4095, 32767, 65534, 65535,
        ];
        let input: Vec<u16> = (0..dims.voxels().unwrap())
            .map(|i| pattern[i % pattern.len()])
            .collect();
        let encoded = compress_u16(&input, dims, Options::default()).unwrap();
        let (decoded, decoded_dims) = decompress_u16(&encoded).unwrap();
        assert_eq!(decoded_dims, dims);
        assert_eq!(decoded, input);
    }

    #[test]
    fn lossless_roundtrip_single_voxel_values() {
        let dims = Dims { x: 1, y: 1, z: 1 };
        for value in [0u16, 1, 255, 256, 65535] {
            let encoded = compress_u16(&[value], dims, Options::default()).unwrap();
            let (decoded, decoded_dims) = decompress_u16(&encoded).unwrap();
            assert_eq!(decoded_dims, dims);
            assert_eq!(decoded, vec![value]);
        }
    }

    #[test]
    fn lossless_encoding_is_deterministic() {
        let dims = Dims { x: 17, y: 13, z: 5 };
        let input = structured_volume(dims);
        let options = Options::default();
        let a = compress_u16(&input, dims, options).unwrap();
        let b = compress_u16(&input, dims, options).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn histogram_batching_is_independent_of_worker_count() {
        let dims = Dims {
            x: 31,
            y: 19,
            z: 21,
        };
        let input = structured_volume(dims);
        let mut reference = None;
        for threads in [1, 2, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let encoded = pool.install(|| compress_u16(&input, dims, Options::default()).unwrap());
            if let Some(expected) = &reference {
                assert_eq!(&encoded, expected);
            } else {
                reference = Some(encoded);
            }
        }
    }

    #[test]
    fn zero_remainder_shortcut_requires_an_identity_model() {
        for scale in MIN_SCALE_BITS..=MAX_SCALE_BITS {
            let total = 1usize << scale;
            let table = vec![(total as u32) << 9; total];
            let mut value = 0;
            let mut state = RANS_L + 12345;
            let mut pos = 0;
            decode_r_packed_one(&mut value, &mut state, &[], &mut pos, &table, total, scale)
                .unwrap();
            assert_eq!(value, 0);
            assert_eq!(state, RANS_L + 12345);
            assert_eq!(pos, 0);
            state = 0;
            assert!(decode_r_packed_one(
                &mut value,
                &mut state,
                &[],
                &mut pos,
                &table,
                total,
                scale
            )
            .is_err());
            state = RANS_L;
            let invalid = vec![PACKED_INVALID; total];
            assert!(decode_r_packed_one(
                &mut value,
                &mut state,
                &[],
                &mut pos,
                &invalid,
                total,
                scale
            )
            .is_err());
        }
    }

    #[test]
    fn format_versions_are_one() {
        assert_eq!(VERSION, 1);
        assert_eq!(ND_VERSION, 1);
        assert_eq!(LOSSY_VERSION, 1);
        assert_eq!(STREAM_VERSION, 1);
    }

    #[test]
    fn single_volume_headers_roundtrip() {
        let dims = Dims { x: 3, y: 2, z: 1 };
        let input = vec![0; dims.voxels().unwrap()];
        let lossless = compress_u16(&input, dims, Options::default()).unwrap();
        let lossy = compress_lossy_u16(&input, dims, LossyOptions::default()).unwrap();
        for encoded in [lossless, lossy] {
            assert_eq!(u16::from_le_bytes([encoded[4], encoded[5]]), 1);
            assert_eq!(inspect_dims(&encoded).unwrap(), dims);
            let (decoded, decoded_dims) = decompress_u16(&encoded).unwrap();
            assert_eq!(decoded_dims, dims);
            assert_eq!(decoded, input);
        }
    }

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
    fn lossless_decoder_roundtrips_supported_settings() {
        let dims = Dims { x: 23, y: 11, z: 7 };
        let input = structured_volume(dims);
        let cases = [
            Options::default(),
            Options {
                block_depth: 8,
                context_mode: ContextMode::Signed3 as u32,
                scale_bits: 10,
            },
        ];

        for options in cases {
            let encoded = compress_u16(&input, dims, options).unwrap();
            let mut decoded = vec![0u16; input.len()];
            let decoded_dims = decompress_u16_into(&encoded, &mut decoded).unwrap();
            assert_eq!(decoded_dims, dims);
            assert_eq!(decoded, input);
        }
    }

    #[test]
    fn all_context_modes_roundtrip() {
        let dims = Dims { x: 15, y: 8, z: 4 };
        let input = structured_volume(dims);
        for mode in [
            ContextMode::Signed3,
            ContextMode::Signs,
            ContextMode::Mean,
            ContextMode::MeanSigns,
        ] {
            for scale_bits in MIN_SCALE_BITS..=MAX_SCALE_BITS {
                let options = Options {
                    context_mode: mode as u32,
                    scale_bits: scale_bits as u32,
                    ..Options::default()
                };
                let encoded = compress_u16(&input, dims, options).unwrap();
                let (decoded, _) = decompress_u16(&encoded).unwrap();
                assert_eq!(decoded, input);
            }
        }
    }

    #[test]
    fn mean_context_preserves_fractional_predictor_and_uses_first_plane_rows() {
        assert_eq!(compact_context(ContextMode::Mean, 10, 10, 10), 30);
        assert_eq!(compact_context(ContextMode::Mean, 10, 10, 11), 31);
        assert!(compact_context(ContextMode::Mean, 255, 255, 255) < 256 * 3);
        assert_eq!(compact_context(ContextMode::MeanSigns, 10, 10, 10), 91);
        assert_eq!(compact_context(ContextMode::MeanSigns, 10, 10, 11), 94);
        assert!(compact_context(ContextMode::MeanSigns, 255, 255, 255) < 256 * 9);
        assert_eq!(q_context_count(256, ContextMode::Mean), 1025);
        assert_eq!(q_context_count(256, ContextMode::MeanSigns), 2561);

        let dims = Dims { x: 3, y: 2, z: 1 };
        let q = [7, 20, 30, 10, 12, 14];
        for mode in [ContextMode::Mean, ContextMode::MeanSigns] {
            let expected = compact_context(mode, 10, 20, 15);
            assert_eq!(q_context(&q, dims, 0, 0, 1, 1, mode, 31), expected);
        }
    }

    #[test]
    fn mean_context_roundtrips_degenerate_axes_and_full_range_values() {
        for dims in [
            Dims { x: 1, y: 7, z: 5 },
            Dims { x: 11, y: 1, z: 5 },
            Dims { x: 13, y: 7, z: 1 },
            Dims { x: 11, y: 7, z: 5 },
        ] {
            let input: Vec<u16> = (0..dims.voxels().unwrap())
                .map(|i| match i % 5 {
                    0 => 0,
                    1 => u16::MAX,
                    _ => (i.wrapping_mul(40503) & 65535) as u16,
                })
                .collect();
            for mode in [ContextMode::Mean, ContextMode::MeanSigns] {
                let options = Options {
                    block_depth: 2,
                    context_mode: mode as u32,
                    ..Options::default()
                };
                let encoded = compress_u16(&input, dims, options).unwrap();
                let mut decoded = vec![0; input.len()];
                assert_eq!(decompress_u16_into(&encoded, &mut decoded).unwrap(), dims);
                assert_eq!(decoded, input);

                let lossy_options = LossyOptions {
                    block_depth: 2,
                    context_mode: mode as u32,
                    noise_step: 4.0,
                    ..LossyOptions::default()
                };
                let encoded = compress_lossy_u16(&input, dims, lossy_options).unwrap();
                let (decoded, decoded_dims) = decompress_u16(&encoded).unwrap();
                assert_eq!(decoded_dims, dims);
                let expected: Vec<u16> = input
                    .iter()
                    .map(|&value| {
                        let q = lossy_quantize(value, 0.0, 1.0, 4.0);
                        lossy_reconstruct(q as u8, 0.0, 1.0, 4.0)
                    })
                    .collect();
                assert_eq!(decoded, expected);
            }
        }
    }

    #[test]
    fn supported_scale_bits_roundtrip() {
        let dims = Dims { x: 12, y: 7, z: 3 };
        let input = structured_volume(dims);
        for scale_bits in [8u32, 10, 11] {
            let options = Options {
                scale_bits,
                ..Options::default()
            };
            let encoded = compress_u16(&input, dims, options).unwrap();
            let (decoded, _) = decompress_u16(&encoded).unwrap();
            assert_eq!(decoded, input);
        }
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
    fn decoder_rejects_unsupported_serialised_scale_bits() {
        let dims = Dims { x: 4, y: 4, z: 2 };
        let input = structured_volume(dims);
        let mut encoded = compress_u16(&input, dims, Options::default()).unwrap();
        encoded[28] = 12;
        assert!(decompress_u16(&encoded).is_err());
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
    fn malformed_lossless_streams_are_rejected() {
        let dims = Dims { x: 8, y: 6, z: 3 };
        let input = structured_volume(dims);
        let encoded = compress_u16(&input, dims, Options::default()).unwrap();

        assert!(decompress_u16(&encoded[..20]).is_err());

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(decompress_u16(&trailing).is_err());

        let mut bad_model_scale = encoded.clone();
        // The first q model begins at byte 44; its scale-bits field is
        // 6 bytes into the serialized model header.
        bad_model_scale[50] = 31;
        assert!(decompress_u16(&bad_model_scale).is_err());

        let mut bad_lanes = encoded.clone();
        // RDL1 header byte 29 is the lane count:
        // magic(4), version(2), flags(2), xyz(12), block_depth(4),
        // q_max(2), r_max(2), scale_bits(1), lanes(1).
        bad_lanes[29] = 3;
        assert!(decompress_u16(&bad_lanes).is_err());

        let mut bad_magic = encoded;
        bad_magic[..4].copy_from_slice(b"NOPE");
        assert!(decompress_u16(&bad_magic).is_err());
    }

    #[test]
    fn nd_container_rejects_trailing_bytes() {
        let dims = Dims5 {
            x: 6,
            y: 5,
            z: 3,
            c: 2,
            t: 2,
        };
        let n = dims.voxels().unwrap();
        let input: Vec<u16> = (0..n).map(|i| ((i * 97 + i / 11) % 65536) as u16).collect();
        let mut encoded = compress_u16_nd(&input, dims, Options::default()).unwrap();
        encoded.push(0);
        assert!(decompress_u16_nd(&encoded).is_err());
    }

    #[test]
    fn compact_model_roundtrips_all_scales_and_alphabet_extremes() {
        for scale_bits in MIN_SCALE_BITS..=MAX_SCALE_BITS {
            let total = 1usize << scale_bits;
            for alphabet in [1, 2, 127, 128, 256, MAX_R_ALPHABET.min(total)] {
                let contexts = 260;
                let mut counts = vec![0u64; contexts * alphabet];
                counts[alphabet - 1] = 123;
                for sym in 0..alphabet {
                    counts[129 * alphabet + sym] = (sym as u64 + 1) * 7;
                }
                counts[259 * alphabet] = 99;
                counts[260 * alphabet - 1] += 77;
                let model =
                    StaticModel::from_counts(&counts, contexts, alphabet, scale_bits).unwrap();
                let blob = model.serialize().unwrap();
                assert_eq!(blob[7], 0);
                let decoded = StaticModel::deserialize(&blob).unwrap();
                assert_eq!(decoded.freq, model.freq);
                assert_eq!(decoded.cum, model.cum);
                assert_eq!(decoded.contexts, contexts);
                assert_eq!(decoded.alphabet, alphabet);
                assert_eq!(decoded.scale_bits, scale_bits);
                let table = packed_r_decode_table(&decoded).unwrap();
                assert_eq!(table, packed_r_decode_table(&model).unwrap());
                for ctx in [0, 129, 259] {
                    for sym in 0..alphabet {
                        let i = ctx * alphabet + sym;
                        if model.freq[i] != 0 {
                            let start = model.cum[i] as usize;
                            let end = start + model.freq[i] as usize - 1;
                            assert_eq!((table[ctx * total + start] & 0x1ff) as usize, sym);
                            assert_eq!((table[ctx * total + end] & 0x1ff) as usize, sym);
                        }
                    }
                }
                assert_eq!(table[total], PACKED_INVALID);
            }
        }
        let empty = StaticModel::from_counts(&[0; 24], 3, 8, 8).unwrap();
        let blob = empty.serialize().unwrap();
        assert_eq!(blob.len(), 9);
        assert_eq!(StaticModel::deserialize(&blob).unwrap().freq, empty.freq);

        let mut counts = vec![0; MAX_MODEL_CONTEXTS];
        counts[MAX_MODEL_CONTEXTS - 1] = 1;
        let sparse = StaticModel::from_counts(&counts, MAX_MODEL_CONTEXTS, 1, 8).unwrap();
        let blob = sparse.serialize().unwrap();
        assert_eq!(blob.len(), 13);
        assert_eq!(StaticModel::deserialize(&blob).unwrap().freq, sparse.freq);
    }

    fn model_test_blob(payload: &[u8]) -> Vec<u8> {
        let mut blob = Vec::new();
        put_u32(&mut blob, 2);
        put_u16(&mut blob, 3);
        blob.extend_from_slice(&[8, 0]);
        blob.extend_from_slice(payload);
        blob
    }

    #[test]
    fn compact_model_rejects_malformed_payloads() {
        let valid = model_test_blob(&[2, 0, 2, 0, 1, 2, 1, 1, 1]);
        assert_eq!(
            StaticModel::deserialize(&valid).unwrap().freq,
            [1, 0, 255, 0, 256, 0]
        );
        for end in 0..valid.len() {
            assert!(
                StaticModel::deserialize(&valid[..end]).is_err(),
                "prefix {end}"
            );
        }
        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(StaticModel::deserialize(&trailing).is_err());
        for payload in [
            vec![3],                               // More nonempty contexts than available.
            vec![1, 2, 1, 0],                      // Context beyond the declared extent.
            vec![1, 0, 0],                         // An explicitly empty context.
            vec![1, 0, 4],                         // Too many symbols.
            vec![1, 0, 1, 3],                      // Symbol beyond the alphabet.
            vec![1, 0, 2, 0, 0, 1],                // Zero frequency.
            vec![1, 0, 2, 0, 0x80, 2, 1],          // No frequency left for the last symbol.
            vec![1, 0, 3, 0, 0xff, 1],             // Insufficient mass for two later symbols.
            vec![1, 0, 2, 0, 1, 0],                // Duplicate symbol.
            vec![2, 0, 1, 0, 0, 1, 0],             // Duplicate context.
            vec![0x80, 0],                         // Overlong context count.
            vec![1, 0x80, 0],                      // Overlong context index.
            vec![1, 0, 0x81, 0],                   // Overlong symbol count.
            vec![1, 0, 1, 0x80, 0],                // Overlong symbol index.
            vec![1, 0, 2, 0, 0x81, 0, 1],          // Overlong frequency.
            vec![0xff, 0xff, 0xff, 0xff, 0x10],    // u32 overflow.
            vec![0x80, 0x80, 0x80, 0x80, 0x80, 0], // Too many continuation bytes.
        ] {
            assert!(
                StaticModel::deserialize(&model_test_blob(&payload)).is_err(),
                "{payload:?}"
            );
        }
        for (offset, value) in [(0, 0), (4, 0), (6, 7), (6, 12)] {
            let mut invalid_header = valid.clone();
            invalid_header[offset] = value;
            assert!(StaticModel::deserialize(&invalid_header).is_err());
        }
    }

    #[test]
    fn model_varints_cover_boundaries_and_reject_noncanonical_values() {
        for value in [
            0,
            1,
            127,
            128,
            16383,
            16384,
            (1 << 21) - 1,
            1 << 21,
            (1 << 28) - 1,
            1 << 28,
            u32::MAX,
        ] {
            let mut bytes = Vec::new();
            put_model_varint(&mut bytes, value);
            let mut reader = Reader::new(&bytes);
            assert_eq!(read_model_varint(&mut reader).unwrap(), value);
            assert!(reader.is_done());
            for end in 0..bytes.len() {
                assert!(read_model_varint(&mut Reader::new(&bytes[..end])).is_err());
            }
        }
    }

    #[test]
    fn static_model_serialization_roundtrip() {
        let counts = vec![10u64, 5, 0, 1, 0, 3, 9, 2];
        let model = StaticModel::from_counts(&counts, 2, 4, 8).unwrap();
        let blob = model.serialize().unwrap();
        let decoded = StaticModel::deserialize(&blob).unwrap();
        assert_eq!(decoded.contexts, model.contexts);
        assert_eq!(decoded.alphabet, model.alphabet);
        assert_eq!(decoded.scale_bits, model.scale_bits);
        assert_eq!(decoded.freq, model.freq);
        assert_eq!(decoded.cum, model.cum);
        assert!(decoded.encode.is_empty());
        assert!(decoded.encode_symbol(0, 0).is_err());
        assert_eq!(
            packed_q_decode_table(&decoded).unwrap(),
            packed_q_decode_table(&model).unwrap()
        );
    }

    #[test]
    fn reciprocal_rans_encoder_matches_division_at_all_frequencies() {
        for scale_bits in MIN_SCALE_BITS..=MAX_SCALE_BITS {
            let total = 1u32 << scale_bits;
            let renorm_factor = (RANS_L >> scale_bits) << 8;
            for freq in 1..=total {
                let model = StaticModel::from_counts(
                    &[(total - freq) as u64, freq as u64],
                    1,
                    2,
                    scale_bits,
                )
                .unwrap();
                let cum = total - freq;
                let x_max = renorm_factor * freq;
                let mut states = vec![RANS_L, RANS_L + 1, (RANS_L << 8) - 1];
                for boundary in [x_max, x_max.saturating_mul(256)] {
                    for x in [boundary - 1, boundary, boundary.saturating_add(1)] {
                        if (RANS_L..RANS_L << 8).contains(&x) {
                            states.push(x);
                        }
                    }
                }
                // Include the largest normalized quotients and remainders,
                // where a rounded reciprocal is most likely to be off by one.
                for quotient in [0, 1, renorm_factor / 2, renorm_factor - 1] {
                    for remainder in [0, freq / 2, freq - 1] {
                        states.push(quotient * freq + remainder);
                    }
                }
                for initial in states {
                    let mut expected_state = initial;
                    let mut expected_bytes = Vec::new();
                    while expected_state >= x_max {
                        expected_bytes.push(expected_state as u8);
                        expected_state >>= 8;
                    }
                    expected_state =
                        ((expected_state / freq) << scale_bits) + expected_state % freq + cum;
                    let mut actual_state = initial;
                    let mut actual_bytes = Vec::new();
                    rans_encode_symbol(&mut actual_state, &mut actual_bytes, &model, 0, 1).unwrap();
                    assert_eq!(
                        actual_state, expected_state,
                        "scale={scale_bits}, freq={freq}, x={initial}"
                    );
                    assert_eq!(
                        actual_bytes, expected_bytes,
                        "scale={scale_bits}, freq={freq}, x={initial}"
                    );
                }
            }
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

    #[test]
    fn normalized_nonempty_rows_sum_to_rans_total() {
        let counts = [100u64, 20, 3, 0, 1, 0, 7];
        let freq = normalize_row(&counts, 256).unwrap();
        assert_eq!(freq.iter().map(|&v| v as usize).sum::<usize>(), 256);
        for (&count, &f) in counts.iter().zip(freq.iter()) {
            assert_eq!(count == 0, f == 0);
        }
    }
}
