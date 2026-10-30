//! Deterministic pixel fixtures for codec and container tests.

use crate::types::{Dims, Dims5};

pub(crate) fn noisy_pixels(len: usize) -> Vec<u16> {
    let mut state = 0x12345678u32;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u16
        })
        .collect()
}

pub(crate) fn structured_volume(dims: Dims) -> Vec<u16> {
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

pub(crate) fn golden_rdl1_expected() -> (Vec<u16>, Dims) {
    let dims = Dims { x: 7, y: 5, z: 3 };
    (structured_volume(dims), dims)
}

pub(crate) fn golden_rdm2_expected() -> (Vec<u16>, Dims5) {
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
