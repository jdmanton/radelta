use radelta_native::RadeltaFileHandle;
use std::{
    fs,
    fs::File,
    path::Path,
    process::{Command, Output},
};
use tiff::encoder::{colortype, TiffEncoder};

fn run(command: &str, input: &Path, output: &Path, extra: &[&str]) -> Output {
    let mut invocation = Command::new(env!("CARGO_BIN_EXE_radelta"));
    invocation.arg("--force");
    invocation.arg(command).arg(input).arg(output).args(extra);
    if command == "encode-lossy" {
        invocation.args(["--offset-adu", "0", "--gain-e-per-adu", "1"]);
    }
    invocation.output().unwrap()
}

fn source(root: &Path) -> std::path::PathBuf {
    let path = root.join("source.tif");
    let mut tiff = TiffEncoder::new(File::create(&path).unwrap()).unwrap();
    for z in 0..3 {
        tiff.write_image::<colortype::Gray16>(300, 300, &vec![1000 + z; 90000])
            .unwrap();
    }
    path
}

fn no_temporary_files(root: &Path) {
    assert!(!fs::read_dir(root).unwrap().any(|p| p
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".radelta-")));
}

#[test]
fn source_aliases_are_rejected_for_all_conversions() {
    let root = std::env::temp_dir().join(format!("radelta-source-aliases-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let tiff = source(&root);
    let encoded = root.join("source.rdlt");
    assert!(run("encode", &tiff, &encoded, &[]).status.success());
    for (command, input) in [
        ("encode", &tiff),
        ("encode-lossy", &tiff),
        ("decode", &encoded),
    ] {
        let original = fs::read(input).unwrap();
        let options: &[&str] = if command == "decode" {
            &[]
        } else {
            &["--stream"]
        };
        assert_eq!(run(command, input, input, options).status.code(), Some(1));
        assert_eq!(fs::read(input).unwrap(), original);
        let link = root.join("hardlink");
        fs::hard_link(input, &link).unwrap();
        assert_eq!(run(command, input, &link, options).status.code(), Some(1));
        assert_eq!(fs::read(input).unwrap(), original);
        fs::remove_file(link).unwrap();
        #[cfg(unix)]
        {
            let link = root.join("symlink");
            std::os::unix::fs::symlink(input, &link).unwrap();
            assert_eq!(run(command, input, &link, options).status.code(), Some(1));
            assert_eq!(fs::read(input).unwrap(), original);
            fs::remove_file(link).unwrap();
        }
    }
    no_temporary_files(&root);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_encoding_and_decoding_preserve_destinations() {
    let root = std::env::temp_dir().join(format!("radelta-failed-export-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let tiff = source(&root);
    let output = root.join("result");
    for command in ["encode", "encode-lossy"] {
        for existing in [false, true] {
            if existing {
                fs::write(&output, b"previous result").unwrap();
            }
            // Fail after streaming pixels have been written, while capturing metadata.
            let result = run(
                command,
                &tiff,
                &output,
                &["--stream", "--metadata-limit-mib", "0"],
            );
            assert_eq!(
                result.status.code(),
                Some(1),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("Saving TIFF metadata"));
            if existing {
                assert_eq!(fs::read(&output).unwrap(), b"previous result");
                fs::remove_file(&output).unwrap();
            } else {
                assert!(!output.exists());
            }
            no_temporary_files(&root);
        }
    }
    let encoded = root.join("corrupt.rdlt");
    fs::write(&encoded, b"old destination").unwrap();
    let result = run(
        "encode",
        &tiff,
        &encoded,
        &[
            "--stream",
            "--t",
            "3",
            "--c",
            "1",
            "--z",
            "1",
            "--page-order",
            "TCZ",
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut bytes = fs::read(&encoded).unwrap();
    let first_len = u64::from_le_bytes(bytes[56..64].try_into().unwrap()) as usize;
    let second = 64 + first_len;
    let second_len =
        u64::from_le_bytes(bytes[second + 16..second + 24].try_into().unwrap()) as usize;
    bytes[second + 24 + second_len - 1] ^= 1;
    fs::write(&encoded, bytes).unwrap();
    for existing in [false, true] {
        if existing {
            fs::write(&output, b"previous TIFF").unwrap();
        }
        let result = run("decode", &encoded, &output, &[]);
        assert_eq!(result.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&result.stderr).contains("checksum mismatch"));
        if existing {
            assert_eq!(fs::read(&output).unwrap(), b"previous TIFF");
            fs::remove_file(&output).unwrap();
        } else {
            assert!(!output.exists());
        }
        no_temporary_files(&root);
    }
    // A completed conversion still replaces an existing destination.
    fs::write(&encoded, b"old destination").unwrap();
    assert!(run("encode", &tiff, &encoded, &[]).status.success());
    let reader = RadeltaFileHandle::open(&encoded).unwrap();
    let mut plane = vec![0; 90000];
    reader.read_plane(0, 0, 2, &mut plane).unwrap();
    assert!(plane.iter().all(|&p| p == 1002));
    drop(reader);
    no_temporary_files(&root);
    fs::remove_dir_all(root).unwrap();
}
