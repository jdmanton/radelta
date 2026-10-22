//! Run this unchanged example against before/after libraries. Measures metadata
//! replacement only, using warm caches without fsync. No codec work is timed.
use radelta_native::{compress_u16, set_file_metadata, set_metadata, Dims, Options};
use std::{fs, path::PathBuf, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .expect("provide a temporary output filename"),
    );
    if path.exists() {
        return Err("benchmark path must not exist".into());
    }
    let bare = compress_u16(&[1, 2, 3, 4], Dims { x: 2, y: 2, z: 1 }, Options::default())?;
    let mut state = 41u32;
    let metadata: Vec<_> = (0..64 * 1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let mut original = bare.clone();
    set_metadata(&mut original, &metadata)?;
    let mut expected = bare;
    set_metadata(&mut expected, b"updated calibration")?;
    let mut times = Vec::new();
    for iteration in 0..8 {
        fs::write(&path, &original)?;
        let start = Instant::now();
        set_file_metadata(&path, b"updated calibration")?;
        let elapsed = start.elapsed().as_secs_f64();
        assert_eq!(fs::read(&path)?, expected);
        if iteration != 0 {
            times.push(elapsed);
        }
    }
    fs::remove_file(path)?;
    times.sort_by(f64::total_cmp);
    println!(
        "Replace 64 MiB metadata, median of 7: {:.6} s",
        times[times.len() / 2]
    );
    Ok(())
}
