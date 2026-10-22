use radelta_native::{Dims5, Options, RadeltaStreamWriter};
use std::{fs, io::Cursor, process::Command};
use tiff::{
    decoder::Decoder,
    encoder::{colortype, TiffEncoder},
    tags::Tag,
};

#[test]
fn late_decode_and_tiff_read_errors_preserve_destinations_and_join_workers() {
    let root = std::env::temp_dir().join(format!("radelta-pipeline-errors-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let input = root.join("input.rdlt");
    let output = root.join("output.tif");
    let d = Dims5 {
        x: 3,
        y: 2,
        z: 1,
        c: 1,
        t: 300,
    };
    {
        let mut writer =
            RadeltaStreamWriter::create_lossless(&input, d, Options::default(), 1).unwrap();
        for t in 0..d.t {
            writer.write_plane(t, 0, 0, &[t as u16; 6]).unwrap();
        }
        writer.finish().unwrap();
    }
    let mut bytes = fs::read(&input).unwrap();
    let mut at = 40;
    for i in 0..300 {
        let size = u64::from_le_bytes(bytes[at + 16..at + 24].try_into().unwrap()) as usize;
        if i == 299 {
            bytes[at + 24 + size - 1] ^= 1;
        }
        at += 24 + size;
    }
    fs::write(&input, bytes).unwrap();
    fs::write(&output, b"keep the previous TIFF").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .arg("decode")
        .arg(&input)
        .arg(&output)
        .args(["--memory-mib", "1"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(fs::read(&output).unwrap(), b"keep the previous TIFF");

    let source = root.join("source.tif");
    let encoded = root.join("output.rdlt");
    {
        let mut tiff = TiffEncoder::new(fs::File::create(&source).unwrap()).unwrap();
        for p in 0..300u16 {
            tiff.write_image::<colortype::Gray16>(3, 2, &[p; 6])
                .unwrap();
        }
    }
    // Damage the final strip after its directory remains fully readable. The
    // initial layout inspection succeeds; the producer fails while reading.
    let mut bytes = fs::read(&source).unwrap();
    let little = &bytes[..2] == b"II";
    let read16 = |v: &[u8]| {
        if little {
            u16::from_le_bytes(v.try_into().unwrap())
        } else {
            u16::from_be_bytes(v.try_into().unwrap())
        }
    };
    let read32 = |v: &[u8]| {
        if little {
            u32::from_le_bytes(v.try_into().unwrap())
        } else {
            u32::from_be_bytes(v.try_into().unwrap())
        }
    };
    let mut directory = read32(&bytes[4..8]) as usize;
    loop {
        let n = read16(&bytes[directory..directory + 2]) as usize;
        let next = read32(&bytes[directory + 2 + 12 * n..directory + 6 + 12 * n]);
        if next != 0 {
            directory = next as usize;
            continue;
        }
        for i in 0..n {
            let at = directory + 2 + 12 * i;
            if read16(&bytes[at..at + 2]) == 273 {
                assert_eq!(read32(&bytes[at + 4..at + 8]), 1);
                let invalid = bytes.len() as u32 + 4096;
                bytes[at + 8..at + 12].copy_from_slice(&if little {
                    invalid.to_le_bytes()
                } else {
                    invalid.to_be_bytes()
                });
                break;
            }
        }
        break;
    }
    let mut decoder = Decoder::new(Cursor::new(&bytes)).unwrap();
    for _ in 1..300 {
        decoder.next_image().unwrap();
    }
    assert!(decoder.get_tag_u64(Tag::StripOffsets).unwrap() > bytes.len() as u64);
    fs::write(&source, bytes).unwrap();
    fs::write(&encoded, b"keep the previous Radelta file").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .arg("encode")
        .arg(&source)
        .arg(&encoded)
        .args([
            "--stream",
            "--memory-mib",
            "1",
            "--t",
            "300",
            "--c",
            "1",
            "--z",
            "1",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(
        fs::read(&encoded).unwrap(),
        b"keep the previous Radelta file"
    );
    for entry in fs::read_dir(&root).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.contains(".tmp"), "temporary output leaked: {name}");
    }
    fs::remove_dir_all(root).unwrap();
}
