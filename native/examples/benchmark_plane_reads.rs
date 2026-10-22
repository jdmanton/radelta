//! File reads with fresh decoded caches and warm filesystem caches. API calls
//! are timed; coordinate construction, allocation, and CRC checks are untimed.
use radelta_native::RadeltaFileHandle;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err("usage: benchmark_plane_reads FILE REPEATS [BATCH_PLANES=128]".into());
    }
    let repeats: usize = args[2].parse()?;
    let batch: usize = args.get(3).map_or(Ok(128), |v| v.parse())?;
    if repeats == 0 || batch == 0 {
        return Err("repeats and batch size must be positive".into());
    }
    let info = RadeltaFileHandle::open(&args[1])?.info();
    let plane = (info.nx as usize)
        .checked_mul(info.ny as usize)
        .ok_or("plane size overflow")?;
    let total = (info.nt as usize)
        .checked_mul(info.nc as usize)
        .and_then(|n| n.checked_mul(info.nz as usize))
        .ok_or("plane count overflow")?;
    let batch = batch.min(total).min((64 * 1024 * 1024 / plane / 2).max(1));
    let size = plane.checked_mul(batch).ok_or("batch size overflow")?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(size)?;
    pixels.resize(size, 0u16);
    let mut checksum = None;
    println!("method,repeat,seconds,gb_per_second,crc32");
    for iteration in 0..=repeats {
        // Alternate method order between repeats, including an untimed warmup.
        for batched in if iteration % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let reader = RadeltaFileHandle::open(&args[1])?;
            let mut seconds = 0.;
            let mut crc = crc32fast::Hasher::new();
            for first in (0..total).step_by(batch) {
                let count = batch.min(total - first);
                let coords: Vec<_> = (first..first + count)
                    .map(|i| {
                        (
                            i / (info.nc as usize * info.nz as usize),
                            i / info.nz as usize % info.nc as usize,
                            i % info.nz as usize,
                        )
                    })
                    .collect();
                let out = &mut pixels[..count * plane];
                let start = Instant::now();
                if batched {
                    reader.read_planes(&coords, out)?;
                } else {
                    for (&(t, c, z), dst) in coords.iter().zip(out.chunks_exact_mut(plane)) {
                        reader.read_plane(t, c, z, dst)?;
                    }
                }
                seconds += start.elapsed().as_secs_f64();
                // SAFETY: initialized u16 values have no padding; borrow their
                // complete storage read-only. Both methods use native byte order.
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        out.as_ptr().cast::<u8>(),
                        std::mem::size_of_val(out),
                    )
                };
                crc.update(bytes);
            }
            let crc = crc.finalize();
            if let Some(expected) = checksum {
                assert_eq!(crc, expected, "decoded pixels differ");
            } else {
                checksum = Some(crc);
            }
            if iteration != 0 {
                println!(
                    "{},{iteration},{seconds:.9},{:.6},{crc:08x}",
                    if batched { "batch" } else { "single" },
                    total as f64 * plane as f64 * 2. / seconds / 1e9
                );
            }
        }
    }
    Ok(())
}
