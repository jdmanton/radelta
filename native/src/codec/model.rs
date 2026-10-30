//! Static probability models, compact serialization and normalization.

use super::RANS_L;
use crate::format::{put_u16, put_u32, Reader};
use crate::types::{RadeltaError, Result, MAX_SCALE_BITS, MIN_SCALE_BITS};

// Model bounds for uint16 input. These limits are also used
// while parsing untrusted files so malformed model headers cannot request
// unbounded allocations before the enclosing stream is validated.
pub(super) const MAX_Q_ALPHABET: usize = 256;

const MAX_R_ALPHABET: usize = 511;

const MAX_MODEL_CONTEXTS: usize = MAX_Q_ALPHABET * 5 * 5 + MAX_Q_ALPHABET + 1;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct EncodeSymbol {
    pub(super) reciprocal: u64,
    pub(super) renorm_threshold: u32,
    pub(super) cum: u16,
    pub(super) complement: u16,
}

#[derive(Debug, Clone)]
pub(super) struct StaticModel {
    pub(super) contexts: usize,
    pub(super) alphabet: usize,
    pub(super) scale_bits: u8,
    pub(super) total: usize,
    pub(super) freq: Vec<u16>,
    // Sorted nonempty contexts, derived while training or parsing. Sparse
    // models need not rescan every empty row to serialize or build tables.
    pub(super) active_contexts: Vec<usize>,
    pub(super) cum: Vec<u16>,
    // Only models trained for compression need an encoder lookup table.
    pub(super) encode: Vec<EncodeSymbol>,
}

impl StaticModel {
    pub(super) fn from_counts(
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

        let mut active_contexts = Vec::new();
        let mut freq = vec![0u16; counts.len()];
        let mut cum = vec![0u16; counts.len()];
        let mut encode = vec![EncodeSymbol::default(); counts.len()];
        let reciprocals = Self::encoding_reciprocals(scale_bits);
        let mut scratch = NormalizationScratch::default();
        let renorm_factor = (RANS_L >> scale_bits) << 8;

        for ctx in 0..contexts {
            let row = &counts[ctx * alphabet..(ctx + 1) * alphabet];
            let f = &mut freq[ctx * alphabet..(ctx + 1) * alphabet];
            normalize_row_into(row, total, f, &mut scratch)?;
            if f.iter().all(|&v| v == 0) {
                continue;
            }
            active_contexts.push(ctx);
            let mut c = 0usize;
            for sym in 0..alphabet {
                let fv = f[sym] as usize;
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
            active_contexts,
            cum,
            encode,
        })
    }

    fn from_sparse_freqs(
        contexts: usize,
        alphabet: usize,
        scale_bits: u8,
        freq: Vec<u16>,
        active_contexts: Vec<usize>,
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
        // The parser starts with zeroed frequencies and only writes contexts
        // in this checked, strictly increasing list.
        for &ctx in &active_contexts {
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
            active_contexts,
            cum,
            encode: Vec::new(),
        })
    }

    fn encoding_reciprocals(scale_bits: u8) -> &'static [u64] {
        // Four immutable, lazily initialized tables (about 30 KiB combined).
        // Calibration and image content do not affect these reciprocals.
        use std::sync::OnceLock;
        const SCALES: usize = (MAX_SCALE_BITS - MIN_SCALE_BITS + 1) as usize;
        static TABLES: [OnceLock<Vec<u64>>; SCALES] = [const { OnceLock::new() }; SCALES];
        TABLES[(scale_bits - MIN_SCALE_BITS) as usize].get_or_init(|| {
            let total = 1usize << scale_bits;
            let numerator = (total as u64) << 32;
            std::iter::once(0)
                .chain((1..=total).map(|freq| numerator.div_ceil(freq as u64)))
                .collect()
        })
    }

    #[inline(always)]
    pub(super) fn encode_symbol(&self, ctx: usize, sym: usize) -> Result<&EncodeSymbol> {
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

    pub(super) fn serialize(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        put_u32(&mut out, self.contexts as u32);
        put_u16(&mut out, self.alphabet as u16);
        out.push(self.scale_bits);
        // Omit empty contexts and infer each row's last frequency.
        // Contexts and symbols are sorted; the first index is absolute and
        // subsequent indices are encoded as strictly positive deltas.
        out.push(0); // reserved
        put_model_varint(&mut out, self.active_contexts.len() as u32);
        let mut previous_ctx = 0;
        for &ctx in &self.active_contexts {
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

    pub(super) fn deserialize(data: &[u8]) -> Result<Self> {
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
        let mut active_contexts = Vec::with_capacity(nonempty);
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
            active_contexts.push(ctx);
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
        Self::from_sparse_freqs(contexts, alphabet, scale_bits, freq, active_contexts)
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

#[derive(Default)]
struct NormalizationScratch {
    remainders: Vec<(u128, usize)>,
    order: Vec<usize>,
}

#[allow(clippy::unnecessary_sort_by)]
fn normalize_row_into(
    counts: &[u64],
    target: usize,
    out: &mut [u16],
    scratch: &mut NormalizationScratch,
) -> Result<()> {
    out.fill(0);
    let sum: u128 = counts.iter().map(|&v| v as u128).sum();
    if sum == 0 {
        return Ok(());
    }
    let nz = counts.iter().filter(|&&v| v != 0).count();
    if nz > target {
        return Err(RadeltaError(format!(
            "{nz} nonzero symbols exceed normalization total {target}"
        )));
    }

    if nz == 1 {
        out[counts.iter().position(|&v| v != 0).unwrap()] = target as u16;
        return Ok(());
    }
    let rems = &mut scratch.remainders;
    rems.clear();
    let mut total = 0usize;
    // Real image counts normally fit in a hardware divide. The bound covers
    // every numerator as well as the denominator; extreme u64 counts retain
    // u128 arithmetic.
    let narrow = sum <= u64::MAX as u128 / target as u128;

    for (sym, &c) in counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let num = c as u128 * target as u128;
        let (quotient, remainder) = if narrow {
            (
                (num as u64 / sum as u64) as usize,
                (num as u64 % sum as u64) as u128,
            )
        } else {
            ((num / sum) as usize, num % sum)
        };
        let mut base = quotient;
        if base == 0 {
            base = 1;
        }
        out[sym] = base as u16;
        total += base;
        rems.push((remainder, sym));
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
        let order = &mut scratch.order;
        order.clear();
        order.extend(
            out.iter()
                .enumerate()
                .filter_map(|(i, &f)| (f > 1).then_some(i)),
        );
        while total > target {
            order.sort_unstable_by_key(|&i| std::cmp::Reverse(out[i]));
            let mut changed = false;
            for &sym in order.iter() {
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

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unnecessary_sort_by)]
fn reference_normalize_row(counts: &[u64], target: usize) -> Result<Vec<u16>> {
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

#[cfg(test)]
mod tests {
    use super::super::block::{packed_q_decode_table, packed_r_decode_table, PACKED_INVALID};
    use super::*;
    use crate::format::{put_u16, put_u32, Reader};
    use crate::types::{MAX_SCALE_BITS, MIN_SCALE_BITS};

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
                assert_eq!(model.active_contexts, [0, 129, 259]);
                assert_eq!(decoded.active_contexts, model.active_contexts);
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
        assert!(empty.active_contexts.is_empty());
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
    fn reused_normalization_matches_reference_including_ties_and_overflow_counts() {
        let mut scratch = NormalizationScratch::default();
        let mut random = 0x987654321abcdef0u64;
        for bits in MIN_SCALE_BITS..=MAX_SCALE_BITS {
            let target = 1usize << bits;
            for len in [1, 2, 7, 128, 256, 511] {
                if len > target {
                    continue;
                }
                for case in 0..64 {
                    let counts: Vec<_> = (0..len)
                        .map(|i| {
                            random ^= random << 13;
                            random ^= random >> 7;
                            random ^= random << 17;
                            match case {
                                0 => 0,
                                1 => u64::MAX,
                                2 => {
                                    if i == len / 2 {
                                        u64::MAX
                                    } else {
                                        0
                                    }
                                }
                                3 => {
                                    if i == 0 {
                                        u64::MAX
                                    } else {
                                        1
                                    }
                                }
                                4 => 1,
                                _ => {
                                    if random & 3 == 0 {
                                        0
                                    } else {
                                        random % 1000
                                    }
                                }
                            }
                        })
                        .collect();
                    let mut actual = vec![u16::MAX; len];
                    normalize_row_into(&counts, target, &mut actual, &mut scratch).unwrap();
                    assert_eq!(
                        actual,
                        reference_normalize_row(&counts, target).unwrap(),
                        "bits={bits}, len={len}, case={case}"
                    );
                }
            }
            let table = StaticModel::encoding_reciprocals(bits);
            assert!(std::ptr::eq(table, StaticModel::encoding_reciprocals(bits)));
            assert_eq!(table[0], 0);
            for (freq, &reciprocal) in table.iter().enumerate().skip(1) {
                assert_eq!(reciprocal, ((target as u64) << 32).div_ceil(freq as u64));
            }
        }
    }

    #[test]
    fn normalized_nonempty_rows_sum_to_rans_total() {
        let counts = [100u64, 20, 3, 0, 1, 0, 7];
        let mut freq = vec![0; counts.len()];
        normalize_row_into(
            &counts,
            256,
            &mut freq,
            &mut NormalizationScratch::default(),
        )
        .unwrap();
        assert_eq!(freq.iter().map(|&v| v as usize).sum::<usize>(), 256);
        for (&count, &f) in counts.iter().zip(freq.iter()) {
            assert_eq!(count == 0, f == 0);
        }
    }
}
