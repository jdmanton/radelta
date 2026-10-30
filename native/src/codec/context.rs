//! Causal spatial contexts for square-root symbols.

use crate::types::{ContextMode, Dims};

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
pub(super) fn compact_context(mode: ContextMode, l: u8, u: u8, z: u8) -> usize {
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

pub(super) fn q_context_count(q_alphabet: usize, mode: ContextMode) -> usize {
    mode.compact_count(q_alphabet) + q_alphabet + 1
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
pub(super) fn q_context(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContextMode, Dims};

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
}
