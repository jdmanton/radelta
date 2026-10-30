use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tiff::{
    decoder::{Decoder, DecodingResult},
    encoder::{colortype, TiffEncoder},
};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radelta"))
        .args(args)
        .output()
        .unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn source(path: &Path) {
    let mut tiff = TiffEncoder::new(fs::File::create(path).unwrap()).unwrap();
    for z in 0..3 {
        tiff.write_image::<colortype::Gray8>(7, 5, &(0..35).map(|i| i + z).collect::<Vec<u8>>())
            .unwrap();
    }
}

#[test]
fn help_version_and_argument_errors_are_distinct_from_io_failures() {
    let version = success(run(&["--version"]));
    assert_eq!(
        version.trim(),
        concat!("radelta ", env!("CARGO_PKG_VERSION"))
    );
    for command in [
        "encode",
        "encode-lossy",
        "decode",
        "benchmark",
        "benchmark-lossy",
    ] {
        success(run(&[command, "--version"]));
        let help = run(&[command, "--help"]);
        assert!(help.stderr.is_empty());
        let text = success(help);
        assert!(text.contains("Usage:"));
        assert!(text.contains("--threads"));
        assert!(!text.lines().any(|line| line.trim() == "\\"));
    }
    success(run(&["--help"]));
    success(run(&["help", "encode-lossy"]));
    for args in [
        vec!["encod"],
        vec!["encode"],
        vec!["encode", "missing.tif", "--scale-bits", "7"],
        vec!["encode", "missing.tif", "--block-depth", "0"],
        vec!["encode", "missing.tif", "--threads", "0"],
        vec!["encode", "missing.tif", "--t", "0"],
        vec!["encode", "missing.tif", "--z", "-1"],
        vec!["encode", "missing.tif", "--page-order", "TTC"],
        vec!["encode", "missing.tif", "--memory-mib", "0"],
        vec![
            "encode",
            "missing.tif",
            "--memory-mib",
            "18446744073709551615",
        ],
        vec!["encode", "missing.tif", "--context", "meann"],
        vec!["encode", "missing.tif", "--context"],
        vec!["encode", "missing.tif", "--unknown"],
        vec!["encode", "missing.tif", "out.rdlt", "extra.rdlt"],
        vec![
            "encode",
            "missing.tif",
            "out.rdlt",
            "--output",
            "other.rdlt",
        ],
        vec!["decode", "missing.rdlt", "--stream"],
        vec!["benchmark", "missing.tif", "--stream"],
        vec!["benchmark", "missing.tif", "--memory-mib", "64"],
        vec!["benchmark", "missing.tif", "--repeats", "0"],
        vec!["encode-lossy", "missing.tif"],
        vec!["encode-lossy", "missing.tif", "--offset-adu", "0"],
        vec![
            "encode-lossy",
            "missing.tif",
            "--offset-adu",
            "NaN",
            "--gain-e-per-adu",
            "1",
        ],
        vec![
            "encode-lossy",
            "missing.tif",
            "--offset-adu",
            "0",
            "--gain-e-per-adu",
            "inf",
        ],
        vec![
            "encode-lossy",
            "missing.tif",
            "--offset-adu",
            "0",
            "--gain-adu-per-e",
            "1e-320",
        ],
        vec![
            "encode-lossy",
            "missing.tif",
            "--offset-adu",
            "0",
            "--gain-e-per-adu",
            "1",
            "--gain-adu-per-e",
            "2",
        ],
        vec![
            "encode-lossy",
            "missing.tif",
            "--offset-adu",
            "0",
            "--gain-e-per-adu",
            "1",
            "--noise-step",
            "0",
        ],
    ] {
        let output = run(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
    }
    assert_eq!(run(&[]).status.code(), Some(2));
    let missing = run(&["encode", "does-not-exist.tif"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("does-not-exist.tif"));
}

#[test]
fn conversion_defaults_options_and_overwrite_control_preserve_pixels() {
    let root = std::env::temp_dir().join(format!("radelta-cli-ux-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let input = root.join("sample with spaces.tif");
    source(&input);
    let input = input.to_str().unwrap();
    let encoded = root.join("sample with spaces.rdlt");
    // Options can precede paths, use '=', and set a small budget without silent clamping.
    let text = success(run(&[
        "--threads=1",
        "encode",
        "--memory-mib=1",
        input,
        "--stream",
    ]));
    assert!(encoded.is_file());
    assert!(text.contains("requested with --stream"));
    assert!(text.contains("1 MiB"));
    let benchmark = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .env("RAYON_NUM_THREADS", "4")
        .args(["benchmark", input, "--threads", "1", "--repeats", "1"])
        .output()
        .unwrap();
    assert!(success(benchmark).contains("1 workers"));
    success(run(&[
        "encode-lossy",
        input,
        "--offset-adu=0",
        "--gain-e-per-adu=1",
        "--quiet",
    ]));
    assert!(root.join("sample with spaces.lossy.rdlt").is_file());
    let bytes = fs::read(&encoded).unwrap();
    let refused = run(&["encode", input]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--force"));
    assert_eq!(fs::read(&encoded).unwrap(), bytes);
    let overwrite = run(&["encode", "--force", "--quiet", input]);
    assert!(overwrite.stdout.is_empty());
    assert!(overwrite.stderr.is_empty());
    success(overwrite);
    let restored = root.join("restored.tif");
    success(run(&[
        "decode",
        "-j",
        "2",
        "-o",
        restored.to_str().unwrap(),
        encoded.to_str().unwrap(),
        "--page-order=tcz",
    ]));
    let mut decoder = Decoder::new(fs::File::open(&restored).unwrap()).unwrap();
    for z in 0..3 {
        let DecodingResult::U8(pixels) = decoder.read_image().unwrap() else {
            panic!("sample depth changed")
        };
        assert_eq!(pixels, (0..35).map(|i| i + z).collect::<Vec<u8>>());
        if z < 2 {
            decoder.next_image().unwrap();
        }
    }
    drop(decoder);
    fs::remove_file(input).unwrap();
    // Default decode naming restores INPUT.tif, which was deliberately removed.
    success(run(&["decode", encoded.to_str().unwrap(), "--quiet"]));
    assert!(Path::new(input).is_file());
    let alias = run(&["encode", input, input, "--force"]);
    assert_eq!(alias.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&alias.stderr).contains("different files"));
    for stream in [false, true] {
        let output = root.join(format!("lossy-{stream}.rdlt"));
        let mut args = vec![
            "encode-lossy",
            input,
            "--output",
            output.to_str().unwrap(),
            "--offset-adu",
            "-1",
            "--gain-adu-per-e=2",
            "--quiet",
        ];
        if stream {
            args.push("--stream");
        }
        success(run(&args));
    }
    let quiet_error = run(&["decode", "missing.rdlt", "--quiet"]);
    assert_eq!(quiet_error.status.code(), Some(1));
    assert!(!quiet_error.stderr.is_empty());
    assert!(!fs::read_dir(&root).unwrap().any(|p| p
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".radelta-")));
    fs::remove_dir_all(root).unwrap();
}

fn check_filename(input: &std::ffi::OsStr, label: &str) {
    let root =
        std::env::temp_dir().join(format!("radelta-cli-paths-{label}-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    source(&root.join(input));
    let output = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .current_dir(&root)
        .args(["encode", "--quiet", "--"])
        .arg(input)
        .output()
        .unwrap();
    success(output);
    let encoded = Path::new(input).with_extension("rdlt");
    assert!(root.join(&encoded).is_file());
    let restored = Path::new(input).with_extension("restored.tif");
    success(
        Command::new(env!("CARGO_BIN_EXE_radelta"))
            .current_dir(&root)
            .args(["decode", "--quiet", "--"])
            .arg(encoded)
            .arg(&restored)
            .output()
            .unwrap(),
    );
    let mut decoder = Decoder::new(fs::File::open(root.join(restored)).unwrap()).unwrap();
    for z in 0..3 {
        let DecodingResult::U8(pixels) = decoder.read_image().unwrap() else {
            panic!("sample depth changed");
        };
        assert_eq!(pixels, (0..35).map(|i| i + z).collect::<Vec<u8>>());
        if z < 2 {
            decoder.next_image().unwrap();
        }
    }
    drop(decoder);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unicode_paths_and_leading_hyphens_are_supported() {
    // A valid Unicode name exercises native path handling on macOS and Windows
    // too; macOS filesystems reject the arbitrary byte sequence tested below.
    check_filename(std::ffi::OsStr::new("-image-µ.tif"), "unicode");
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_paths_are_supported() {
    use std::os::unix::ffi::OsStringExt;
    let input = std::ffi::OsString::from_vec(b"-image-\xff.tif".to_vec());
    check_filename(&input, "non-utf8");
}
