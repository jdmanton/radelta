//! Pure codec timings with reusable decode storage and independent validation.
//! Supply little-endian uint16 input; file I/O and equality checks are untimed.
use radelta_native::{
    compress_lossy_u16_with_stats, compress_u16_with_stats, decompress_u16_into, Dims,
    LossyOptions, Options,
};
use std::{fs, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 7 {
        return Err("usage: benchmark_codec RAW X Y Z lossless|lossy REPEATS [CONTEXT=2] [BITS=10] [BLOCK_DEPTH=4] [OUTPUT]".into());
    }
    let dims = Dims {
        x: args[2].parse()?,
        y: args[3].parse()?,
        z: args[4].parse()?,
    };
    let lossy = match args[5].as_str() {
        "lossless" => false,
        "lossy" => true,
        _ => return Err("invalid mode".into()),
    };
    let repeats: usize = args[6].parse()?;
    if repeats == 0 {
        return Err("repeats must be positive".into());
    }
    let options = Options {
        context_mode: args.get(7).map_or(Ok(2), |s| s.parse())?,
        scale_bits: args.get(8).map_or(Ok(10), |s| s.parse())?,
        block_depth: args.get(9).map_or(Ok(4), |s| s.parse())?,
    };
    let calibration = LossyOptions {
        context_mode: options.context_mode,
        scale_bits: options.scale_bits,
        block_depth: options.block_depth,
        offset_adu: 0.,
        gain_e_per_adu: 0.46,
        noise_step: 2.,
    };
    let bytes = fs::read(&args[1])?;
    if dims.voxels()?.checked_mul(2) != Some(bytes.len()) {
        return Err("input size does not match shape".into());
    }
    let input: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    drop(bytes);
    let expected: Vec<u16> = if lossy {
        input
            .iter()
            .map(|&v| {
                let q = (v as f64 * 0.46).sqrt().round();
                (q * q / 0.46).round().clamp(0., 65535.) as u16
            })
            .collect()
    } else {
        input.clone()
    };
    let mut decoded = vec![0; input.len()];
    let mut last = Vec::new();
    println!("iteration,encode_seconds,decode_seconds,q_extract_seconds,histogram_seconds,rans_seconds,other_seconds,encoded_bytes,encoded_crc");
    for iteration in 0..=repeats {
        let start = Instant::now();
        let (encoded, stats) = if lossy {
            compress_lossy_u16_with_stats(&input, dims, calibration)?
        } else {
            compress_u16_with_stats(&input, dims, options)?
        };
        let enc = start.elapsed().as_secs_f64();
        let start = Instant::now();
        let shape = decompress_u16_into(&encoded, &mut decoded)?;
        let dec = start.elapsed().as_secs_f64();
        assert_eq!(shape, dims);
        assert_eq!(decoded, expected);
        if iteration != 0 {
            println!(
                "{iteration},{enc:.9},{dec:.9},{:.9},{:.9},{:.9},{:.9},{},{}",
                stats.seconds_q_extract,
                stats.seconds_histogram,
                stats.seconds_encode,
                (enc - stats.seconds_q_extract - stats.seconds_histogram - stats.seconds_encode)
                    .max(0.),
                encoded.len(),
                crc32fast::hash(&encoded)
            );
        }
        last = encoded;
    }
    if let Some(path) = args.get(10) {
        fs::write(path, last)?;
    }
    Ok(())
}
