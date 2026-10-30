//! Block histograms and four-lane rANS encoding/decoding kernels.

use super::context::{compact_context, q_context};
use super::model::{EncodeSymbol, StaticModel};
use super::{LANES, RANS_L};
use crate::format::RAW_RECORD_BYTES;
use crate::types::{ContextMode, Dims, RadeltaError, Result, DEFAULT_SCALE_BITS, MAX_SCALE_BITS};

#[derive(Clone, Copy, Debug)]
pub(super) struct BlockSpec {
    pub(super) z0: usize,
    pub(super) depth: usize,
}

#[derive(Debug)]
pub(super) struct EncodedBlock {
    pub(super) z0: usize,
    pub(super) depth: usize,
    pub(super) q_states: [u32; LANES],
    pub(super) r_states: [u32; LANES],
    pub(super) q_bytes: Vec<u8>,
    pub(super) r_bytes: Vec<u8>,
}

#[derive(Debug)]
pub(super) struct BorrowedEncodedBlock<'a> {
    pub(super) depth: usize,
    pub(super) raw: bool,
    pub(super) q_states: [u32; LANES],
    pub(super) r_states: [u32; LANES],
    pub(super) q_bytes: &'a [u8],
    pub(super) r_bytes: &'a [u8],
    /// The complete serialized block record covered by `crc`.
    pub(super) record: &'a [u8],
    pub(super) crc: u32,
}

impl BorrowedEncodedBlock<'_> {
    /// Check the block's CRC-32. Called from the parallel decoders so that
    /// verification scales with decoding rather than adding a serial pass.
    pub(super) fn verify(&self) -> Result<()> {
        if crc32fast::hash(self.record) != self.crc {
            return Err(RadeltaError("block checksum mismatch".into()));
        }
        Ok(())
    }
}

/// Compare complete records, including rANS states and CRCs, once per block.
/// If shared models erase the savings, omit them and store the whole volume
/// raw. This also bounds expansion for tiny volumes by framing alone.
pub(super) fn raw_block_plan(
    blocks: &[EncodedBlock],
    plane: usize,
    bytes_per_pixel: usize,
    model_bytes: usize,
) -> Vec<bool> {
    let record_bytes = if bytes_per_pixel == 2 { 52 } else { 32 };
    let mut chosen_bytes = model_bytes as u128;
    let mut raw_bytes = 0u128;
    let mut all_fit = true;
    let mut raw = Vec::with_capacity(blocks.len());
    for b in blocks {
        let stored =
            RAW_RECORD_BYTES as u128 + b.depth as u128 * plane as u128 * bytes_per_pixel as u128;
        let coded = record_bytes as u128 + b.q_bytes.len() as u128 + b.r_bytes.len() as u128;
        let fits = stored - RAW_RECORD_BYTES as u128 <= u32::MAX as u128;
        all_fit &= fits;
        let use_raw = fits && coded >= stored;
        raw.push(use_raw);
        chosen_bytes += if use_raw { stored } else { coded };
        raw_bytes += stored;
    }
    if all_fit && chosen_bytes >= raw_bytes {
        raw.fill(true);
    }
    raw
}

#[derive(Debug, Clone)]
pub(super) struct Counts {
    pub(super) q: Vec<u64>,
    pub(super) r: Vec<u64>,
}

impl Counts {
    pub(super) fn new(q_len: usize, r_len: usize) -> Self {
        Self {
            q: vec![0; q_len],
            r: vec![0; r_len],
        }
    }

    pub(super) fn merge(mut self, other: Self) -> Self {
        for (a, b) in self.q.iter_mut().zip(other.q) {
            *a += b;
        }
        for (a, b) in self.r.iter_mut().zip(other.r) {
            *a += b;
        }
        self
    }
}

#[inline(always)]
fn rans_encode_symbol<const DEFAULT: bool>(
    state: &mut u32,
    out: &mut Vec<u8>,
    model: &StaticModel,
    ctx: usize,
    sym: usize,
) -> Result<()> {
    let symbol = model.encode_symbol(ctx, sym)?;
    rans_encode_prepared::<DEFAULT>(state, out, symbol, model.scale_bits);
    Ok(())
}

#[inline(always)]
fn rans_encode_prepared<const DEFAULT: bool>(
    state: &mut u32,
    out: &mut Vec<u8>,
    symbol: &EncodeSymbol,
    scale_bits: u8,
) {
    let scale_bits = if DEFAULT {
        DEFAULT_SCALE_BITS as u32
    } else {
        scale_bits as u32
    };
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
}

// A span of zero symbols has mean context zero when all its causal
// neighbors are zero. Callers exclude the special first row/column.
#[inline(always)]
fn zero_q_neighborhood<const N: usize>(
    q: &[u8],
    i: usize,
    width: usize,
    plane: usize,
    local_z: usize,
) -> bool {
    q[i..i + N] == [0; N]
        && q[i - 1] == 0
        && q[i - width..i - width + N] == [0; N]
        && (local_z == 0 || q[i - plane..i - plane + N] == [0; N])
}

#[inline(always)]
fn renormalize_q(mut x: u32, bytes: &[u8], pos: &mut usize) -> Result<u32> {
    while x < RANS_L {
        if *pos == 0 {
            return Err(RadeltaError("truncated q rANS byte stack".into()));
        }
        *pos -= 1;
        x = (x << 8) | bytes[*pos] as u32;
    }
    Ok(x)
}

// Packed decode tables shared by the lossless and lossy decoders. At every supported
// scale_bits value, all information needed for one rANS slot fits in one u32.
// Keeping symbol, frequency and cumulative frequency together avoids the second
// model lookup. These helpers do not change the serialized format.
// Every valid entry contains a nonzero frequency. Using zero for empty slots
// lets the allocator leave unused context pages zero-backed on sparse models.
pub(super) const PACKED_INVALID: u32 = 0;

pub(super) fn packed_q_decode_table(model: &StaticModel) -> Result<Vec<u32>> {
    if model.scale_bits > MAX_SCALE_BITS || model.alphabet > 256 {
        return Err(RadeltaError(
            "packed q decoder requires scale_bits<=11 and q<=255".into(),
        ));
    }
    let mut table = vec![PACKED_INVALID; model.contexts * model.total];
    for &ctx in &model.active_contexts {
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

pub(super) fn packed_r_decode_table(model: &StaticModel) -> Result<Vec<u32>> {
    if model.scale_bits > MAX_SCALE_BITS || model.alphabet > 512 {
        return Err(RadeltaError(
            "packed residual decoder requires scale_bits<=11 and r<=511".into(),
        ));
    }
    let mut table = vec![PACKED_INVALID; model.contexts * model.total];
    for &ctx in &model.active_contexts {
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
fn rans_decode_packed_q<const DEFAULT: bool>(
    state: &mut u32,
    bytes: &[u8],
    pos: &mut usize,
    table: &[u32],
    total: usize,
    scale_bits: u8,
    ctx: usize,
) -> Result<usize> {
    let scale_bits = if DEFAULT {
        DEFAULT_SCALE_BITS
    } else {
        scale_bits
    };
    let total = if DEFAULT {
        1 << DEFAULT_SCALE_BITS
    } else {
        total
    };
    let mask = (1u32 << scale_bits) - 1;
    let slot = (*state & mask) as usize;
    let entry = table[ctx * total + slot];
    if entry == PACKED_INVALID {
        return Err(RadeltaError("invalid packed q rANS slot".into()));
    }
    let sym = (entry & 0xff) as usize;
    let freq = (entry >> 8) & 0xfff;
    let cum = (entry >> 20) & 0x7ff;
    let x = freq * (*state >> scale_bits) + (slot as u32 - cum);
    *state = renormalize_q(x, bytes, pos)?;
    Ok(sym)
}

#[inline(always)]
fn rans_decode_packed_r<const DEFAULT: bool>(
    state: &mut u32,
    bytes: &[u8],
    pos: &mut usize,
    table: &[u32],
    total: usize,
    scale_bits: u8,
    ctx: usize,
) -> Result<usize> {
    let scale_bits = if DEFAULT {
        DEFAULT_SCALE_BITS
    } else {
        scale_bits
    };
    let total = if DEFAULT {
        1 << DEFAULT_SCALE_BITS
    } else {
        total
    };
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
fn decode_q_packed_one<const DEFAULT: bool>(
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
    let mode = if DEFAULT { ContextMode::Mean } else { mode };
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
    let sym = rans_decode_packed_q::<DEFAULT>(state, bytes, pos, table, total, scale_bits, ctx)?;
    if sym >= q_alphabet {
        return Err(RadeltaError("decoded packed q symbol out of range".into()));
    }
    out[i] = sym as u16;
    Ok(())
}

#[inline(always)]
fn decode_r_packed_one<const DEFAULT: bool>(
    value: &mut u16,
    state: &mut u32,
    bytes: &[u8],
    pos: &mut usize,
    table: &[u32],
    total: usize,
    scale_bits: u8,
) -> Result<()> {
    let total = if DEFAULT {
        1 << DEFAULT_SCALE_BITS
    } else {
        total
    };
    let qs = *value as usize;
    // q=0 implies the sole remainder r=0. Its full-frequency rANS symbol
    // leaves the lane untouched; verify the table is that identity model
    // before bypassing it, including for streams supplied by a caller.
    if qs == 0 && *state >= RANS_L && table[0] == (total as u32) << 9 {
        return Ok(());
    }
    let r = rans_decode_packed_r::<DEFAULT>(state, bytes, pos, table, total, scale_bits, qs)?;
    let qq = qs as u32;
    let reconstructed = qq * qq + r as u32;
    if reconstructed > u16::MAX as u32 {
        return Err(RadeltaError("decoded uint16 value overflow".into()));
    }
    *value = reconstructed as u16;
    Ok(())
}

// Several batches per worker let Rayon balance bright and empty slabs. Bound
// extra private tables by 64 MiB; very large models use one table
// per worker, and a single worker needs no balancing overhead.
pub(super) fn histogram_grain(blocks: usize, entries: usize) -> usize {
    let workers = rayon::current_num_threads();
    if workers == 1 {
        return blocks;
    }
    let bytes = entries.saturating_mul(std::mem::size_of::<u64>()).max(1);
    let budget_batches = (64 * 1024 * 1024 / bytes).max(workers);
    let batches = workers.saturating_mul(4).min(budget_batches).min(blocks);
    blocks.div_ceil(batches)
}

pub(super) fn block_specs(z: usize, block_depth: usize) -> Vec<BlockSpec> {
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
pub(super) fn accumulate_block_counts<const DEFAULT: bool>(
    acc: &mut Counts,
    input: &[u16],
    q: &[u8],
    dims: Dims,
    block: BlockSpec,
    mode: ContextMode,
    q_alphabet: usize,
    r_alphabet: usize,
) {
    let mode = if DEFAULT { ContextMode::Mean } else { mode };
    let plane = dims.x * dims.y;
    for lz in 0..block.depth {
        let gz = block.z0 + lz;
        let zbase = gz * plane;
        for y in 0..dims.y {
            let row = zbase + y * dims.x;
            macro_rules! accumulate_one {
                ($x:expr) => {{
                    let x = $x;
                    let idx = row + x;
                    let qs = q[idx] as usize;
                    let ctx = q_context(q, dims, block.z0, lz, y, x, mode, q_alphabet);
                    acc.q[ctx * q_alphabet + qs] += 1;

                    let qq = qs as u16;
                    let r = input[idx] - qq * qq;
                    acc.r[qs * r_alphabet + r as usize] += 1;
                }};
            }
            if DEFAULT {
                accumulate_one!(0);
                let mut x = 1;
                while x + 4 <= dims.x {
                    if y > 0 && zero_q_neighborhood::<4>(q, row + x, dims.x, plane, lz) {
                        let run = if x + 16 <= dims.x
                            && zero_q_neighborhood::<16>(q, row + x, dims.x, plane, lz)
                        {
                            16
                        } else {
                            4
                        };
                        acc.q[0] += run as u64;
                        acc.r[0] += run as u64;
                        x += run;
                        continue;
                    } else {
                        accumulate_one!(x);
                        accumulate_one!(x + 1);
                        accumulate_one!(x + 2);
                        accumulate_one!(x + 3);
                    }
                    x += 4;
                }
                while x < dims.x {
                    accumulate_one!(x);
                    x += 1;
                }
            } else {
                for x in 0..dims.x {
                    accumulate_one!(x);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn encode_block<const DEFAULT: bool>(
    input: &[u16],
    q: &[u8],
    dims: Dims,
    block: BlockSpec,
    mode: ContextMode,
    q_alphabet: usize,
    q_model: &StaticModel,
    r_model: &StaticModel,
) -> Result<EncodedBlock> {
    let mode = if DEFAULT { ContextMode::Mean } else { mode };
    let plane = dims.x * dims.y;
    let block_voxels = block.depth * plane;
    let mut q_states = [RANS_L; LANES];
    let mut r_states = [RANS_L; LANES];
    let mut q_bytes = Vec::with_capacity(block_voxels / 8 + 1024);
    let mut r_bytes = Vec::with_capacity(block_voxels / 8 + 1024);

    let zero_symbol = q_model.encode.first().filter(|s| s.renorm_threshold != 0);

    for lz in (0..block.depth).rev() {
        let gz = block.z0 + lz;
        let zbase = gz * plane;
        for y in (0..dims.y).rev() {
            let row = zbase + y * dims.x;
            macro_rules! encode_one {
                ($x:expr, $lane:expr) => {{
                    let x = $x;
                    let lane = $lane;
                    let idx = row + x;
                    let qs = q[idx] as usize;
                    let qctx = q_context(q, dims, block.z0, lz, y, x, mode, q_alphabet);
                    rans_encode_symbol::<DEFAULT>(
                        &mut q_states[lane],
                        &mut q_bytes,
                        q_model,
                        qctx,
                        qs,
                    )?;

                    // Zero-valued pixels carry no remainder information. Omitting
                    // this identity operation preserves lane order and byte output.
                    if qs != 0 {
                        let qq = qs as u16;
                        let r = (input[idx] - qq * qq) as usize;
                        rans_encode_symbol::<DEFAULT>(
                            &mut r_states[lane],
                            &mut r_bytes,
                            r_model,
                            qs,
                            r,
                        )?;
                    }
                }};
            }
            if DEFAULT {
                // Preserve reverse symbol/byte-stack order while making the
                // four independent rANS state lanes explicit to the compiler.
                let local_row = lz * plane + y * dims.x;
                let mut x = dims.x;
                while x > 0 && ((local_row + x - 1) & 3) != 3 {
                    x -= 1;
                    encode_one!(x, (local_row + x) & 3);
                }
                while x >= 4 {
                    x -= 4;
                    if let Some(symbol) = zero_symbol {
                        if x > 0 && y > 0 && zero_q_neighborhood::<4>(q, row + x, dims.x, plane, lz)
                        {
                            rans_encode_prepared::<true>(
                                &mut q_states[3],
                                &mut q_bytes,
                                symbol,
                                DEFAULT_SCALE_BITS,
                            );
                            rans_encode_prepared::<true>(
                                &mut q_states[2],
                                &mut q_bytes,
                                symbol,
                                DEFAULT_SCALE_BITS,
                            );
                            rans_encode_prepared::<true>(
                                &mut q_states[1],
                                &mut q_bytes,
                                symbol,
                                DEFAULT_SCALE_BITS,
                            );
                            rans_encode_prepared::<true>(
                                &mut q_states[0],
                                &mut q_bytes,
                                symbol,
                                DEFAULT_SCALE_BITS,
                            );
                            continue;
                        }
                    }
                    encode_one!(x + 3, 3);
                    encode_one!(x + 2, 2);
                    encode_one!(x + 1, 1);
                    encode_one!(x, 0);
                }
                while x > 0 {
                    x -= 1;
                    encode_one!(x, (local_row + x) & 3);
                }
            } else {
                for x in (0..dims.x).rev() {
                    encode_one!(x, (lz * plane + y * dims.x + x) & (LANES - 1));
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

pub(super) struct PackedDecodeTables<'a> {
    pub(super) q: &'a [u32],
    pub(super) r: &'a [u32],
    pub(super) total: usize,
    pub(super) scale_bits: u8,
}

pub(super) fn decode_q_block_into_packed<const DEFAULT: bool>(
    block: &BorrowedEncodedBlock<'_>,
    dims: Dims,
    mode: ContextMode,
    q_alphabet: usize,
    tables: &PackedDecodeTables<'_>,
    out: &mut [u16],
) -> Result<()> {
    let mode = if DEFAULT { ContextMode::Mean } else { mode };
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
    let zero_freq = if DEFAULT {
        let entry = tables.q[0];
        if entry & 0xff == 0 {
            (entry >> 8) & 0xfff
        } else {
            0
        }
    } else {
        0
    };

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
                decode_q_packed_one::<DEFAULT>(
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
                if DEFAULT
                    && zero_freq != 0
                    && x > 0
                    && y > 0
                    && out[i - 1] == 0
                    && out[i - dims.x..i - dims.x + 4] == [0; 4]
                    && (lz == 0 || out[i - plane..i - plane + 4] == [0; 4])
                    && q_states
                        .iter()
                        .all(|s| (s & ((1 << DEFAULT_SCALE_BITS) - 1)) < zero_freq)
                {
                    // All four slots select q=0 in context zero. Decoding in
                    // lane order preserves byte-stack consumption exactly.
                    for state in &mut q_states {
                        let slot = *state & ((1 << DEFAULT_SCALE_BITS) - 1);
                        let value = zero_freq * (*state >> DEFAULT_SCALE_BITS) + slot;
                        *state = renormalize_q(value, block.q_bytes, &mut q_pos)?;
                    }
                    out[i..i + 4].fill(0);
                    x += 4;
                    continue;
                }
                decode_q_packed_one::<DEFAULT>(
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
                decode_q_packed_one::<DEFAULT>(
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
                decode_q_packed_one::<DEFAULT>(
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
                decode_q_packed_one::<DEFAULT>(
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
                decode_q_packed_one::<DEFAULT>(
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

pub(super) fn decode_block_into_packed<const DEFAULT: bool>(
    block: &BorrowedEncodedBlock<'_>,
    dims: Dims,
    mode: ContextMode,
    q_alphabet: usize,
    tables: &PackedDecodeTables<'_>,
    out: &mut [u16],
) -> Result<()> {
    decode_q_block_into_packed::<DEFAULT>(block, dims, mode, q_alphabet, tables, out)?;

    // Residuals have no spatial dependency, so groups of four map directly to
    // the four serialized rANS lanes and can be explicitly unrolled.
    let mut r_states = block.r_states;
    let mut r_pos = block.r_bytes.len();
    let mut i = 0usize;
    while i + 4 <= out.len() {
        let (head, tail) = out[i..i + 4].split_at_mut(1);
        decode_r_packed_one::<DEFAULT>(
            &mut head[0],
            &mut r_states[0],
            block.r_bytes,
            &mut r_pos,
            tables.r,
            tables.total,
            tables.scale_bits,
        )?;
        let (head, tail) = tail.split_at_mut(1);
        decode_r_packed_one::<DEFAULT>(
            &mut head[0],
            &mut r_states[1],
            block.r_bytes,
            &mut r_pos,
            tables.r,
            tables.total,
            tables.scale_bits,
        )?;
        let (head, tail) = tail.split_at_mut(1);
        decode_r_packed_one::<DEFAULT>(
            &mut head[0],
            &mut r_states[2],
            block.r_bytes,
            &mut r_pos,
            tables.r,
            tables.total,
            tables.scale_bits,
        )?;
        decode_r_packed_one::<DEFAULT>(
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
        decode_r_packed_one::<DEFAULT>(
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

pub(super) fn accumulate_q_block_counts<const DEFAULT: bool>(
    acc: &mut [u64],
    q: &[u8],
    dims: Dims,
    block: BlockSpec,
    mode: ContextMode,
    q_alphabet: usize,
) {
    let mode = if DEFAULT { ContextMode::Mean } else { mode };
    let plane = dims.x * dims.y;
    for lz in 0..block.depth {
        let gz = block.z0 + lz;
        let zbase = gz * plane;
        for y in 0..dims.y {
            let row = zbase + y * dims.x;
            macro_rules! accumulate_one {
                ($x:expr) => {{
                    let x = $x;
                    let idx = row + x;
                    let qs = q[idx] as usize;
                    let ctx = q_context(q, dims, block.z0, lz, y, x, mode, q_alphabet);
                    acc[ctx * q_alphabet + qs] += 1;
                }};
            }
            if DEFAULT {
                accumulate_one!(0);
                let mut x = 1;
                while x + 4 <= dims.x {
                    if y > 0 && zero_q_neighborhood::<4>(q, row + x, dims.x, plane, lz) {
                        let run = if x + 16 <= dims.x
                            && zero_q_neighborhood::<16>(q, row + x, dims.x, plane, lz)
                        {
                            16
                        } else {
                            4
                        };
                        acc[0] += run as u64;
                        x += run;
                        continue;
                    } else {
                        accumulate_one!(x);
                        accumulate_one!(x + 1);
                        accumulate_one!(x + 2);
                        accumulate_one!(x + 3);
                    }
                    x += 4;
                }
                while x < dims.x {
                    accumulate_one!(x);
                    x += 1;
                }
            } else {
                for x in 0..dims.x {
                    accumulate_one!(x);
                }
            }
        }
    }
}

pub(super) fn encode_q_only_block<const DEFAULT: bool>(
    q: &[u8],
    dims: Dims,
    block: BlockSpec,
    mode: ContextMode,
    q_alphabet: usize,
    q_model: &StaticModel,
) -> Result<EncodedBlock> {
    let mode = if DEFAULT { ContextMode::Mean } else { mode };
    let plane = dims.x * dims.y;
    let block_voxels = block.depth * plane;
    let mut q_states = [RANS_L; LANES];
    let mut q_bytes = Vec::with_capacity(block_voxels / 8 + 1024);

    let zero_symbol = q_model.encode.first().filter(|s| s.renorm_threshold != 0);

    for lz in (0..block.depth).rev() {
        let gz = block.z0 + lz;
        let zbase = gz * plane;
        for y in (0..dims.y).rev() {
            let row = zbase + y * dims.x;
            macro_rules! encode_one {
                ($x:expr, $lane:expr) => {{
                    let x = $x;
                    let lane = $lane;
                    let idx = row + x;
                    let qs = q[idx] as usize;
                    let qctx = q_context(q, dims, block.z0, lz, y, x, mode, q_alphabet);
                    rans_encode_symbol::<DEFAULT>(
                        &mut q_states[lane],
                        &mut q_bytes,
                        q_model,
                        qctx,
                        qs,
                    )?;
                }};
            }
            if DEFAULT {
                // Preserve reverse symbol/byte-stack order while making the
                // four independent rANS state lanes explicit to the compiler.
                let local_row = lz * plane + y * dims.x;
                let mut x = dims.x;
                while x > 0 && ((local_row + x - 1) & 3) != 3 {
                    x -= 1;
                    encode_one!(x, (local_row + x) & 3);
                }
                while x >= 4 {
                    x -= 4;
                    if let Some(symbol) = zero_symbol {
                        if x > 0 && y > 0 && zero_q_neighborhood::<4>(q, row + x, dims.x, plane, lz)
                        {
                            rans_encode_prepared::<true>(
                                &mut q_states[3],
                                &mut q_bytes,
                                symbol,
                                DEFAULT_SCALE_BITS,
                            );
                            rans_encode_prepared::<true>(
                                &mut q_states[2],
                                &mut q_bytes,
                                symbol,
                                DEFAULT_SCALE_BITS,
                            );
                            rans_encode_prepared::<true>(
                                &mut q_states[1],
                                &mut q_bytes,
                                symbol,
                                DEFAULT_SCALE_BITS,
                            );
                            rans_encode_prepared::<true>(
                                &mut q_states[0],
                                &mut q_bytes,
                                symbol,
                                DEFAULT_SCALE_BITS,
                            );
                            continue;
                        }
                    }
                    encode_one!(x + 3, 3);
                    encode_one!(x + 2, 2);
                    encode_one!(x + 1, 1);
                    encode_one!(x, 0);
                }
                while x > 0 {
                    x -= 1;
                    encode_one!(x, (local_row + x) & 3);
                }
            } else {
                for x in (0..dims.x).rev() {
                    encode_one!(x, (lz * plane + y * dims.x + x) & (LANES - 1));
                }
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

#[cfg(test)]
mod tests {
    use super::super::context::q_context_count;
    use super::super::model::StaticModel;
    use super::super::transform::make_sqrt_lut;
    use super::*;
    use crate::types::{ContextMode, Dims, DEFAULT_SCALE_BITS, MAX_SCALE_BITS, MIN_SCALE_BITS};

    #[test]
    fn raw_plan_counts_record_and_shared_model_overhead() {
        let block = EncodedBlock {
            z0: 0,
            depth: 1,
            q_states: [RANS_L; LANES],
            r_states: [RANS_L; LANES],
            q_bytes: vec![0; 168],
            r_bytes: Vec::new(),
        };
        // 52 + 168 == 20 + 100*2: ties use raw, including rANS states.
        assert_eq!(raw_block_plan(&[block], 100, 2, 0), [true]);
        let block = EncodedBlock {
            z0: 0,
            depth: 1,
            q_states: [RANS_L; LANES],
            r_states: [RANS_L; LANES],
            q_bytes: vec![0; 10],
            r_bytes: Vec::new(),
        };
        assert_eq!(
            raw_block_plan(std::slice::from_ref(&block), 100, 2, 0),
            [false]
        );
        assert_eq!(raw_block_plan(&[block], 100, 2, 158), [true]);
    }

    #[test]
    fn histogram_grain_bounds_work_and_private_table_memory() {
        for threads in [1, 2, 4, 8] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                for blocks in [1usize, 2, 47, 1024] {
                    for entries in [1usize, 37000, 1 << 21, usize::MAX] {
                        let grain = histogram_grain(blocks, entries);
                        assert!((1..=blocks).contains(&grain));
                        let batches = blocks.div_ceil(grain);
                        assert!(batches <= 4 * threads);
                        let bytes = entries.saturating_mul(8) as u128;
                        let budget = (64u128 * 1024 * 1024).max(threads as u128 * bytes);
                        assert!(batches as u128 * bytes <= budget);
                        if threads == 1 {
                            assert_eq!(batches, 1);
                        }
                    }
                }
            });
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
            decode_r_packed_one::<false>(
                &mut value,
                &mut state,
                &[],
                &mut pos,
                &table,
                total,
                scale,
            )
            .unwrap();
            assert_eq!(value, 0);
            assert_eq!(state, RANS_L + 12345);
            assert_eq!(pos, 0);
            state = 0;
            assert!(decode_r_packed_one::<false>(
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
            assert!(decode_r_packed_one::<false>(
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
    fn default_kernels_match_general_kernels_at_edges_and_zero_transitions() {
        for (x, y, z) in [
            (1, 1, 1),
            (3, 2, 5),
            (7, 5, 7),
            (15, 5, 3),
            (16, 8, 4),
            (17, 5, 5),
            (31, 7, 3),
            (32, 7, 3),
            (33, 7, 5),
            (64, 5, 5),
            (65, 5, 5),
            (129, 17, 9),
        ] {
            let dims = Dims { x, y, z };
            for pattern in 0..4 {
                let input: Vec<u16> = (0..dims.voxels().unwrap())
                    .map(|i| {
                        let x = i % dims.x;
                        let y = i / dims.x % dims.y;
                        let z = i / (dims.x * dims.y);
                        match pattern {
                            0 => 0,
                            1 if !(x / 7 + y / 5 + z / 3).is_multiple_of(4) => 0,
                            1 => ((i * 431 + 17) % 65000) as u16,
                            3 if x % 33 != 16 || y % 4 != 2 => 0,
                            3 => 1000,
                            _ => (i as u32).wrapping_mul(1664525).wrapping_add(1013904223) as u16,
                        }
                    })
                    .collect();
                let q: Vec<_> = input.iter().map(|&v| make_sqrt_lut()[v as usize]).collect();
                let alphabet = q.iter().copied().max().unwrap() as usize + 1;
                let r_alphabet = 2 * (alphabet - 1) + 1;
                let contexts = q_context_count(alphabet, ContextMode::Mean);
                let mut general = Counts::new(contexts * alphabet, alphabet * r_alphabet);
                let mut fast = Counts::new(contexts * alphabet, alphabet * r_alphabet);
                let mut q_only = vec![0; contexts * alphabet];
                let blocks = block_specs(z, 4);
                for &block in &blocks {
                    accumulate_block_counts::<false>(
                        &mut general,
                        &input,
                        &q,
                        dims,
                        block,
                        ContextMode::Mean,
                        alphabet,
                        r_alphabet,
                    );
                    accumulate_block_counts::<true>(
                        &mut fast,
                        &input,
                        &q,
                        dims,
                        block,
                        ContextMode::Mean,
                        alphabet,
                        r_alphabet,
                    );
                    accumulate_q_block_counts::<true>(
                        &mut q_only,
                        &q,
                        dims,
                        block,
                        ContextMode::Mean,
                        alphabet,
                    );
                }
                assert_eq!(general.q, fast.q);
                assert_eq!(general.r, fast.r);
                assert_eq!(general.q, q_only);
                let qm =
                    StaticModel::from_counts(&general.q, contexts, alphabet, DEFAULT_SCALE_BITS)
                        .unwrap();
                let rm =
                    StaticModel::from_counts(&general.r, alphabet, r_alphabet, DEFAULT_SCALE_BITS)
                        .unwrap();
                let qt = packed_q_decode_table(&qm).unwrap();
                let rt = packed_r_decode_table(&rm).unwrap();
                let tables = PackedDecodeTables {
                    q: &qt,
                    r: &rt,
                    total: 1 << DEFAULT_SCALE_BITS,
                    scale_bits: DEFAULT_SCALE_BITS,
                };
                for block in blocks {
                    let reference = encode_block::<false>(
                        &input,
                        &q,
                        dims,
                        block,
                        ContextMode::Mean,
                        alphabet,
                        &qm,
                        &rm,
                    )
                    .unwrap();
                    let encoded = encode_block::<true>(
                        &input,
                        &q,
                        dims,
                        block,
                        ContextMode::Mean,
                        alphabet,
                        &qm,
                        &rm,
                    )
                    .unwrap();
                    assert_eq!(reference.q_states, encoded.q_states);
                    assert_eq!(reference.r_states, encoded.r_states);
                    assert_eq!(reference.q_bytes, encoded.q_bytes);
                    assert_eq!(reference.r_bytes, encoded.r_bytes);
                    let only = encode_q_only_block::<true>(
                        &q,
                        dims,
                        block,
                        ContextMode::Mean,
                        alphabet,
                        &qm,
                    )
                    .unwrap();
                    assert_eq!(reference.q_states, only.q_states);
                    assert_eq!(reference.q_bytes, only.q_bytes);
                    let borrowed = BorrowedEncodedBlock {
                        depth: block.depth,
                        raw: false,
                        q_states: reference.q_states,
                        r_states: reference.r_states,
                        q_bytes: &reference.q_bytes,
                        r_bytes: &reference.r_bytes,
                        record: &[],
                        crc: 0,
                    };
                    let size = block.depth * dims.x * dims.y;
                    let start = block.z0 * dims.x * dims.y;
                    let mut fast_out = vec![0; size];
                    let mut general_out = vec![0; size];
                    decode_block_into_packed::<false>(
                        &borrowed,
                        dims,
                        ContextMode::Mean,
                        alphabet,
                        &tables,
                        &mut general_out,
                    )
                    .unwrap();
                    decode_block_into_packed::<true>(
                        &borrowed,
                        dims,
                        ContextMode::Mean,
                        alphabet,
                        &tables,
                        &mut fast_out,
                    )
                    .unwrap();
                    assert_eq!(fast_out, general_out);
                    assert_eq!(fast_out, input[start..start + size]);
                    decode_q_block_into_packed::<true>(
                        &borrowed,
                        dims,
                        ContextMode::Mean,
                        alphabet,
                        &tables,
                        &mut fast_out,
                    )
                    .unwrap();
                    assert_eq!(
                        fast_out,
                        q[start..start + size]
                            .iter()
                            .map(|&v| v as u16)
                            .collect::<Vec<_>>()
                    );
                    if !reference.q_bytes.is_empty() {
                        let broken = BorrowedEncodedBlock {
                            q_bytes: &reference.q_bytes[..reference.q_bytes.len() - 1],
                            ..borrowed
                        };
                        let a = decode_q_block_into_packed::<false>(
                            &broken,
                            dims,
                            ContextMode::Mean,
                            alphabet,
                            &tables,
                            &mut general_out,
                        );
                        let b = decode_q_block_into_packed::<true>(
                            &broken,
                            dims,
                            ContextMode::Mean,
                            alphabet,
                            &tables,
                            &mut fast_out,
                        );
                        assert_eq!(a.is_ok(), b.is_ok());
                        if a.is_ok() {
                            assert_eq!(fast_out, general_out);
                        }
                    }
                }
            }
        }
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
                    rans_encode_symbol::<false>(&mut actual_state, &mut actual_bytes, &model, 0, 1)
                        .unwrap();
                    assert_eq!(
                        actual_state, expected_state,
                        "scale={scale_bits}, freq={freq}, x={initial}"
                    );
                    assert_eq!(
                        actual_bytes, expected_bytes,
                        "scale={scale_bits}, freq={freq}, x={initial}"
                    );
                    if scale_bits == DEFAULT_SCALE_BITS {
                        let mut fast_state = initial;
                        let mut fast_bytes = Vec::new();
                        rans_encode_symbol::<true>(&mut fast_state, &mut fast_bytes, &model, 0, 1)
                            .unwrap();
                        assert_eq!(fast_state, expected_state);
                        assert_eq!(fast_bytes, expected_bytes);
                    }
                }
            }
        }
    }
}
