//! Temporary paths and deterministic plane values for file-API tests.

use crate::Dims5;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn temp_path(stem: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "radelta-{stem}-{}-{nonce}.rdlt",
        std::process::id()
    ))
}

pub(super) fn plane_values(dims: Dims5, t: usize, c: usize, z: usize) -> Vec<u16> {
    let mut out = Vec::with_capacity(dims.x * dims.y);
    for y in 0..dims.y {
        for x in 0..dims.x {
            out.push(((t * 10000 + c * 3000 + z * 200 + y * 30 + x * 7) % 65536) as u16);
        }
    }
    out
}
