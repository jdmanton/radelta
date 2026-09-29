use radelta_native::*;
use std::{fs, process::Command};

// Configuration is process-wide. Keep all changes in one test in this separate
// integration-test process, so other codec tests cannot observe temporary limits.
#[test]
fn configurable_metadata_limits_cover_memory_files_and_tiff() {
    const MIB: usize = 1024 * 1024;
    assert_eq!(DEFAULT_METADATA_LIMIT_BYTES, 1024 * MIB);
    assert_eq!(metadata_limit(), DEFAULT_METADATA_LIMIT_BYTES);
    assert_eq!(radelta_get_metadata_limit(), DEFAULT_METADATA_LIMIT_BYTES);
    radelta_set_metadata_limit(2048 * MIB);
    assert_eq!(metadata_limit(), 2048 * MIB);

    let bare = compress_u16(&[1, 2, 3, 4], Dims { x: 2, y: 2, z: 1 }, Options::default()).unwrap();
    let mut encoded = bare.clone();
    set_metadata_limit(32);
    set_metadata(&mut encoded, &[7; 32]).unwrap();
    assert_eq!(read_metadata(&encoded).unwrap(), [7; 32]);
    let unchanged = encoded.clone();
    assert!(set_metadata(&mut encoded, &[7; 33]).is_err());
    assert_eq!(encoded, unchanged);
    set_metadata_limit(31);
    assert!(read_metadata(&encoded).is_err()); // Compressed size fits; inflated size does not.
    set_metadata_limit(32);
    assert_eq!(read_metadata(&encoded).unwrap().len(), 32);

    let dir = std::env::temp_dir().join(format!("radelta-limits-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("image.rdlt");
    fs::write(&file, &bare).unwrap();
    set_file_metadata(&file, &[7; 32]).unwrap();
    assert_eq!(read_file_metadata(&file).unwrap(), [7; 32]);
    assert!(set_file_metadata(&file, &[7; 33]).is_err());
    assert_eq!(read_file_metadata(&file).unwrap(), [7; 32]);
    set_metadata_limit(31);
    assert!(read_file_metadata(&file).is_err());
    assert!(RadeltaFileHandle::open(&file).is_err());
    set_metadata_limit(32);
    assert_eq!(RadeltaFileHandle::open(&file).unwrap().metadata(), [7; 32]);

    set_metadata_limit(0);
    assert!(read_metadata(&bare).unwrap().is_empty());
    assert!(set_metadata(&mut bare.clone(), &[1]).is_err());
    set_metadata(&mut bare.clone(), &[]).unwrap();
    // An extreme configured value must not wrap when adding footer/header sizes.
    set_metadata_limit(usize::MAX);
    assert_eq!(read_metadata(&encoded).unwrap(), [7; 32]);
    set_metadata_limit(DEFAULT_METADATA_LIMIT_BYTES);

    // A TIFF value above the TIFF crate's own 1 MiB default must work when
    // the configured budget allows it, and fail cleanly when it does not.
    let tiff = dir.join("input.tif");
    {
        use tiff::{
            encoder::{colortype, TiffEncoder},
            tags::Tag,
        };
        let mut writer = TiffEncoder::new(fs::File::create(&tiff).unwrap()).unwrap();
        let mut image = writer.new_image::<colortype::Gray16>(2, 2).unwrap();
        image
            .encoder()
            .write_tag(Tag::from_u16_exhaustive(65001), &vec![42u8; 2 * MIB][..])
            .unwrap();
        image.write_data(&[1, 2, 3, 4]).unwrap();
    }
    let output = dir.join("output.rdlt");
    let restored = dir.join("restored.tif");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_radelta"))
            .args(args)
            .output()
            .unwrap()
    };
    for value in ["0", "1", "-1", "18446744073709551615", "invalid"] {
        let result = run(&[
            "encode",
            tiff.to_str().unwrap(),
            output.to_str().unwrap(),
            "--metadata-limit-mib",
            value,
        ]);
        assert!(!result.status.success(), "unexpected success for {value}");
        assert!(!String::from_utf8_lossy(&result.stderr).contains("panicked"));
    }
    let result = run(&[
        "encode",
        tiff.to_str().unwrap(),
        output.to_str().unwrap(),
        "--metadata-limit-mib",
        "3",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = run(&[
        "decode",
        output.to_str().unwrap(),
        restored.to_str().unwrap(),
        "--metadata-limit-mib",
        "1",
    ]);
    assert!(!result.status.success());
    let result = run(&[
        "decode",
        output.to_str().unwrap(),
        restored.to_str().unwrap(),
        "--metadata-limit-mib",
        "3",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::remove_dir_all(dir).unwrap();
}
