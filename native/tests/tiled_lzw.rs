use radelta_native::RadeltaFileHandle;
use std::{fs, process::Command};
use tiff::decoder::{Decoder, DecodingResult};

fn roundtrip(stream: bool, lossy: bool) {
    let root = std::env::temp_dir().join(format!(
        "radelta-lzw-{}-{stream}-{lossy}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let input = root.join("input.tif");
    let encoded = root.join("encoded.rdlt");
    let restored = root.join("restored.tif");
    fs::write(&input, include_bytes!("fixtures/planar_u16_lzw_tiles.tif")).unwrap();
    let mut encode = Command::new(env!("CARGO_BIN_EXE_radelta"));
    encode
        .args(["--quiet", "--threads", "2"])
        .arg(if lossy { "encode-lossy" } else { "encode" })
        .arg(&input)
        .arg(&encoded);
    if stream {
        encode.args(["--stream", "--memory-mib", "1"]);
    }
    if lossy {
        encode.args(["--offset-adu", "0", "--gain-e-per-adu", "1"]);
    }
    let result = encode.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let reader = RadeltaFileHandle::open(&encoded).unwrap();
    let info = reader.info();
    assert_eq!(
        (info.nx, info.ny, info.nz, info.nc, info.nt),
        (19, 17, 2, 3, 1)
    );
    let mut expected_planes = Vec::new();
    for c in 0..3 {
        for z in 0..2 {
            let expected: Vec<u16> = (0..17 * 19)
                .map(|i| {
                    // Fixture source order is Z,C,Y,X. Its uint16 arithmetic wraps
                    // before the remainder is taken; output is canonical C,Z,Y,X.
                    let value = (((z * 3 + c) * 17 * 19 + i) as u16).wrapping_mul(37) % 127;
                    if lossy {
                        let q = (value as f64).sqrt().round() as u16;
                        q * q
                    } else {
                        value
                    }
                })
                .collect();
            let mut pixels = vec![0; 17 * 19];
            reader.read_plane(0, c, z, &mut pixels).unwrap();
            assert_eq!(pixels, expected, "channel {c}, Z {z}");
            expected_planes.push(expected);
        }
    }
    drop(reader);
    let result = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .args(["--quiet", "--threads", "2", "decode"])
        .arg(&encoded)
        .arg(&restored)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut tiff =
        Decoder::new(std::io::BufReader::new(fs::File::open(&restored).unwrap())).unwrap();
    // TIFF export restores the source's Z,C page order; the file API above
    // exposes canonical C,Z coordinates.
    for i in 0..expected_planes.len() {
        let expected = &expected_planes[(i % 3) * 2 + i / 3];
        assert_eq!(tiff.dimensions().unwrap(), (19, 17));
        let DecodingResult::U16(pixels) = tiff.read_image().unwrap() else {
            panic!("restored TIFF must retain uint16 samples")
        };
        assert_eq!(&pixels, expected);
        assert_eq!(tiff.more_images(), i + 1 < expected_planes.len());
        if tiff.more_images() {
            tiff.next_image().unwrap();
        }
    }
    drop(tiff);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lossless_planar_lzw_tiles_in_memory() {
    roundtrip(false, false);
}

#[test]
fn lossless_planar_lzw_tiles_streaming() {
    roundtrip(true, false);
}

#[test]
fn lossy_planar_lzw_tiles_in_memory() {
    roundtrip(false, true);
}

#[test]
fn lossy_planar_lzw_tiles_streaming() {
    roundtrip(true, true);
}

#[test]
fn truncated_lzw_tiles_preserve_existing_outputs() {
    let mut bytes = include_bytes!("fixtures/planar_u16_lzw_tiles.tif").to_vec();
    let u16_at = |at| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
    let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let ifd = u32_at(4) as usize;
    let entry = (0..usize::from(u16_at(ifd)))
        .map(|i| ifd + 2 + i * 12)
        .find(|&at| u16_at(at) == 325)
        .unwrap();
    assert_eq!(u16_at(entry + 2), 3); // SHORT TileByteCounts in this fixture.
    assert!(u32_at(entry + 4) > 1);
    let counts = u32_at(entry + 8) as usize;
    // Bound the first compressed tile to one byte, leaving the TIFF's
    // directories and remaining tiles intact.
    bytes[counts..counts + 2].copy_from_slice(&1u16.to_le_bytes());
    for stream in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "radelta-lzw-truncated-{}-{stream}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let input = root.join("input.tif");
        let output = root.join("encoded.rdlt");
        fs::write(&input, &bytes).unwrap();
        fs::write(&output, b"existing image").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_radelta"));
        command
            .args(["--force", "--quiet", "--threads", "2", "encode"])
            .arg(&input)
            .arg(&output);
        if stream {
            command.args(["--stream", "--memory-mib", "1"]);
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr)
            .to_lowercase()
            .contains("lzw"));
        assert_eq!(fs::read(&output).unwrap(), b"existing image");
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            2,
            "temporary output leaked"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
