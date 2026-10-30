//! Damaged dimension headers must produce ordinary errors, never a process
//! abort from an impossible allocation. Regression tests for fuzzing cases in
//! which a corrupted X dimension requested hundreds of gigabytes.

use radelta_native::{decompress_u16_nd, RadeltaFileHandle};
use std::{fs, fs::File, path::Path, process::Command};
use tiff::encoder::{colortype, TiffEncoder};

const NX: u32 = 37;
const NY: u32 = 21;

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// Run `radelta COMMAND INPUT OUTPUT [options]`, with `args = [COMMAND, options...]`.
fn encode(root: &Path, input: &Path, name: &str, args: &[&str]) -> Vec<u8> {
    let output = root.join(name);
    let result = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .arg("--force")
        .arg(args[0])
        .arg(input)
        .arg(&output)
        .args(&args[1..])
        .args(["--t", "1", "--c", "2", "--z", "2", "--page-order", "TCZ"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::read(output).unwrap()
}

/// Opening and decoding must fail cleanly through both the file API and CLI.
fn assert_rejected(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(format!("{name}.rdlt"));
    fs::write(&path, bytes).unwrap();
    assert!(
        RadeltaFileHandle::open(&path).is_err(),
        "{name}: damaged file opened"
    );
    let result = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .arg("--force")
        .arg("decode")
        .arg(&path)
        .arg(root.join(format!("{name}.tif")))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(1), "{name}: {stderr}");
    assert!(stderr.starts_with("error:"), "{name}: {stderr}");
}

#[test]
fn damaged_dimensions_are_rejected_without_allocating() {
    let root = std::env::temp_dir().join(format!("radelta-corrupt-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let input = root.join("input.tif");
    {
        let mut tiff = TiffEncoder::new(File::create(&input).unwrap()).unwrap();
        for page in 0..4u32 {
            let pixels: Vec<u16> = (0..NX * NY)
                .map(|i| (100 + (i * 7 + page * 31) % 900) as u16)
                .collect();
            tiff.write_image::<colortype::Gray16>(NX, NY, &pixels)
                .unwrap();
        }
    }

    // RDM2/RDQ2: 32-byte header, u64 length, then the first RDL1/RDLQ payload.
    // RDS3/RQS3: 40-byte header, 24-byte chunk header, then the first payload.
    let lossy = [
        "encode-lossy",
        "--offset-adu",
        "100",
        "--gain-e-per-adu",
        "0.46",
    ];
    let streaming_lossy = [&lossy[..], &["--stream"]].concat();
    let cases: [(&str, &[&str], usize, &[u8; 4]); 4] = [
        ("rdm2", &["encode"], 40, b"RDL1"),
        ("rdq2", &lossy, 40, b"RDLQ"),
        ("rds3", &["encode", "--stream"], 64, b"RDL1"),
        ("rqs3", &streaming_lossy, 64, b"RDLQ"),
    ];
    for (name, args, payload, magic) in cases {
        let bytes = encode(&root, &input, &format!("{name}.rdlt"), args);
        assert_eq!(
            &bytes[..4],
            name.to_uppercase().as_bytes(),
            "{name}: wrong container"
        );
        assert_eq!(
            &bytes[payload..payload + 4],
            magic,
            "{name}: layout changed"
        );
        assert!(RadeltaFileHandle::open(root.join(format!("{name}.rdlt"))).is_ok());

        // Container X disagrees with every payload.
        let mut container = bytes.clone();
        put_u32(&mut container, 8, 0x7fff_ffff);
        assert_rejected(&root, &format!("{name}-container-x"), &container);

        // The first payload claims an enormous X; the container is intact.
        let mut inner = bytes.clone();
        put_u32(&mut inner, payload + 8, 0x7fff_ffff);
        assert_rejected(&root, &format!("{name}-payload-x"), &inner);

        if name.starts_with("rdm") || name.starts_with("rdq") {
            assert!(decompress_u16_nd(&container).is_err());
            assert!(decompress_u16_nd(&inner).is_err());
        }
    }
    fs::remove_dir_all(root).unwrap();
}
