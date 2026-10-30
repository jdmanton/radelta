//! Stream round trips, validation and corruption coverage.

use super::decode::{
    alloc_zeroed_u16, decompress_u16, decompress_u16_expecting, decompress_u16_into, inspect_dims,
};
use super::encode::{compress_lossy_u16, compress_u16};
use super::transform::{lossy_quantize, lossy_reconstruct};
use crate::codec::test_support::{golden_rdl1_expected, golden_rdm2_expected, structured_volume};
use crate::container::{compress_lossy_u16_nd, compress_u16_nd, decompress_u16_nd};
use crate::format::{
    header_crc, put_block_crc, MAGIC, MAGIC_LOSSY, MAGIC_ND, MAGIC_ND_LOSSY, RAW_BLOCK_MARKER,
};
use crate::types::{ContextMode, Dims, LossyOptions, Options, MAX_SCALE_BITS, MIN_SCALE_BITS};
use crate::*;

#[test]
fn compressed_streams_use_version_one() {
    let dims = Dims {
        x: 128,
        y: 64,
        z: 8,
    };
    let input = vec![100; dims.voxels().unwrap()];
    for encoded in [
        compress_u16(&input, dims, Options::default()).unwrap(),
        compress_lossy_u16(&input, dims, LossyOptions::default()).unwrap(),
    ] {
        assert_eq!(u16::from_le_bytes([encoded[4], encoded[5]]), 1);
        assert_eq!(decompress_u16(&encoded).unwrap().0, input);
    }
}

#[test]
fn raw_records_reject_damage_invalid_lengths_and_missing_models() {
    let dims = Dims { x: 3, y: 2, z: 1 };
    for mut encoded in [
        compress_u16(&[7; 6], dims, Options::default()).unwrap(),
        compress_lossy_u16(&[7; 6], dims, LossyOptions::default()).unwrap(),
    ] {
        let start = if &encoded[..4] == MAGIC { 48 } else { 68 };
        assert_eq!(
            u32::from_le_bytes(encoded[start + 8..start + 12].try_into().unwrap()),
            RAW_BLOCK_MARKER
        );
        let mut damaged = encoded.clone();
        damaged[start + 16] ^= 1;
        assert!(decompress_u16(&damaged).is_err());
        assert!(decompress_u16(&encoded[..encoded.len() - 1]).is_err());
        let mut length = encoded.clone();
        length[start + 12..start + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decompress_u16(&length)
            .unwrap_err()
            .0
            .contains("raw block length"));
        let mut no_model = encoded.clone();
        no_model[start + 8..start + 12].copy_from_slice(&0u32.to_le_bytes());
        assert!(decompress_u16(&no_model)
            .unwrap_err()
            .0
            .contains("probability model"));
        // Version 2 is not part of the format. Reject it explicitly.
        encoded[4..6].copy_from_slice(&2u16.to_le_bytes());
        let crc = header_crc(&encoded, start - 4);
        encoded[start - 4..start].copy_from_slice(&crc.to_le_bytes());
        assert!(decompress_u16(&encoded)
            .unwrap_err()
            .0
            .contains("unsupported"));
        assert!(inspect_dims(&encoded)
            .unwrap_err()
            .0
            .contains("unsupported"));
    }
    let mut encoded = compress_lossy_u16(&[7; 6], dims, LossyOptions::default()).unwrap();
    encoded[68 + 16] = 255; // q_max is much smaller.
    encoded.truncate(encoded.len() - 4);
    put_block_crc(&mut encoded, 68);
    assert!(decompress_u16(&encoded)
        .unwrap_err()
        .0
        .contains("q maximum"));
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
fn golden_rdl1_v1_decodes() {
    let encoded = include_bytes!("../../tests/fixtures/rdl1_reference_v1.rdlt");
    let (expected, dims) = golden_rdl1_expected();
    let (decoded, decoded_dims) = decompress_u16(encoded).unwrap();
    assert_eq!(decoded_dims, dims);
    assert_eq!(decoded, expected);
    assert_eq!(inspect_dims(encoded).unwrap(), dims);
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
        z: 65,
    };
    let mut input = structured_volume(dims);
    for (i, pixel) in input.iter_mut().enumerate() {
        if (i / (dims.x * dims.y * 4)) % 3 != 1 {
            *pixel = 0;
        }
    }
    let mut reference = None;
    for threads in [1, 2, 4, 8] {
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
fn decoder_rejects_unsupported_serialised_scale_bits() {
    let dims = Dims { x: 4, y: 4, z: 2 };
    let input = structured_volume(dims);
    let mut encoded = compress_u16(&input, dims, Options::default()).unwrap();
    encoded[28] = 12;
    assert!(decompress_u16(&encoded).is_err());
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
fn damaged_dimensions_return_errors_instead_of_aborting() {
    let dims = Dims { x: 23, y: 17, z: 9 };
    let input = structured_volume(dims);
    let lossy = LossyOptions {
        offset_adu: 90.0,
        gain_e_per_adu: 0.46,
        ..LossyOptions::default()
    };
    for encoded in [
        compress_u16(&input, dims, Options::default()).unwrap(),
        compress_lossy_u16(&input, dims, lossy).unwrap(),
    ] {
        // X is the first dimension, after magic, version and flags. The
        // header parses but requests over a terabyte of pixels.
        let mut damaged = encoded.clone();
        damaged[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decompress_u16(&damaged).is_err());
        assert!(decompress_u16_nd(&damaged).is_err());
        assert!(decompress_u16_expecting(&damaged, dims).is_err());

        // A container that expects another shape is rejected before decoding.
        let other = Dims { x: 17, y: 23, z: 9 };
        assert!(decompress_u16_expecting(&encoded, other).is_err());
        let decoded = decompress_u16_expecting(&encoded, dims).unwrap();
        assert_eq!(decoded, decompress_u16(&encoded).unwrap().0);
    }
    assert!(alloc_zeroed_u16(0).unwrap().is_empty());
    assert!(alloc_zeroed_u16(usize::MAX).is_err());
    assert!(alloc_zeroed_u16(5).unwrap().iter().all(|&v| v == 0));
}

/// Offsets of the `flags` bytes, which checksums deliberately skip, in a
/// single-volume stream or in each stream embedded in an RDM2/RDQ2 container.
fn flags_offsets(encoded: &[u8]) -> Vec<usize> {
    let mut offsets = vec![6, 7];
    if &encoded[..4] == MAGIC_ND || &encoded[..4] == MAGIC_ND_LOSSY {
        let volumes = u32::from_le_bytes(encoded[28..32].try_into().unwrap()) as usize;
        let mut pos = 32;
        for _ in 0..volumes {
            let len = u64::from_le_bytes(encoded[pos..pos + 8].try_into().unwrap()) as usize;
            offsets.extend([pos + 8 + 6, pos + 8 + 7]);
            pos += 8 + len;
        }
    }
    offsets
}

#[test]
fn every_single_bit_flip_outside_flags_is_rejected() {
    let dims = Dims { x: 9, y: 7, z: 5 };
    let input = structured_volume(dims);
    let (input5, dims5) = golden_rdm2_expected();
    let options = Options {
        block_depth: 2,
        ..Options::default()
    };
    let lossy = LossyOptions {
        block_depth: 2,
        offset_adu: 90.0,
        gain_e_per_adu: 0.46,
        ..LossyOptions::default()
    };
    for encoded in [
        compress_u16(&input, dims, options).unwrap(),
        compress_lossy_u16(&input, dims, lossy).unwrap(),
        compress_u16_nd(&input5, dims5, options).unwrap(),
        compress_lossy_u16_nd(&input5, dims5, lossy).unwrap(),
    ] {
        assert!(decompress_u16_nd(&encoded).is_ok());
        let skip = flags_offsets(&encoded);
        for byte in (0..encoded.len()).filter(|b| !skip.contains(b)) {
            for bit in 0..8 {
                let mut damaged = encoded.clone();
                damaged[byte] ^= 1 << bit;
                assert!(
                    decompress_u16_nd(&damaged).is_err(),
                    "{}: undetected flip at byte {byte}, bit {bit}",
                    String::from_utf8_lossy(&encoded[..4])
                );
            }
        }
    }
}

#[test]
fn checksum_failures_name_the_damaged_part() {
    let dims = Dims { x: 9, y: 7, z: 5 };
    let input = structured_volume(dims);
    let encoded = compress_u16(&input, dims, Options::default()).unwrap();
    // The final byte belongs to the last block's CRC. Offset 31 is the
    // reserved header byte, which no other check inspects.
    for (offset, message) in [
        (encoded.len() - 1, "block checksum mismatch"),
        (31, "RDL1 header checksum mismatch"),
    ] {
        let mut damaged = encoded.clone();
        damaged[offset] ^= 0x01;
        let err = decompress_u16(&damaged).unwrap_err();
        assert_eq!(err.0, message, "offset {offset}");
    }
    // Metadata changes only the flags field and the footer.
    let mut with_metadata = encoded.clone();
    set_metadata(&mut with_metadata, b"checked separately").unwrap();
    assert_eq!(decompress_u16(&with_metadata).unwrap().0, input);
}
