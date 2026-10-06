use radelta_native::RadeltaFileHandle;
use std::{fs, fs::File, process::Command};
use tiff::encoder::{colortype, TiffEncoder};

#[test]
fn forced_and_automatic_streaming_report_progress_and_preserve_planes() {
    let root = std::env::temp_dir().join(format!("radelta-progress-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let input = root.join("input.tif");
    let mut expected = Vec::new();
    {
        let mut tiff = TiffEncoder::new(File::create(&input).unwrap()).unwrap();
        // 44 planes: two chunks per TC volume at a 64 MiB budget, including
        // a shorter final chunk. Perfect squares survive the chosen lossy step.
        for page in 0..44 {
            let pixels: Vec<u16> = (0..1024 * 1024)
                .map(|i| {
                    let value = ((i + page * 13) % 200) as u16;
                    value * value
                })
                .collect();
            tiff.write_image::<colortype::Gray16>(1024, 1024, &pixels)
                .unwrap();
            expected.push(pixels);
        }
    }
    for lossy in [false, true] {
        let mut previous = None;
        for forced in [false, true] {
            let encoded = root.join(format!("{lossy}-{forced}.rdlt"));
            let mut command = Command::new(env!("CARGO_BIN_EXE_radelta"));
            command
                .arg(if lossy { "encode-lossy" } else { "encode" })
                .arg(&input)
                .arg(&encoded)
                .args([
                    "--memory-mib",
                    "64",
                    "--t",
                    "2",
                    "--c",
                    "2",
                    "--z",
                    "11",
                    "--page-order",
                    "TCZ",
                ]);
            if forced {
                command.arg("--stream");
            }
            if lossy {
                command.args([
                    "--offset-adu",
                    "0",
                    "--gain-e-per-adu",
                    "1",
                    "--noise-step",
                    "2",
                ]);
            }
            let result = command.output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let stdout = String::from_utf8(result.stdout).unwrap();
            let stages = [
                "Streaming layout: 8 chunk(s), up to 8 Z plane(s) per chunk; input has 44 physical / 44 logical TIFF plane(s).",
                "Reading first chunk...",
                "chunk 8/8 (100.0%)",
                "Saving TIFF metadata...",
                "Wrote ",
            ];
            let mut previous_position = 0;
            for stage in stages {
                let position = stdout
                    .find(stage)
                    .unwrap_or_else(|| panic!("missing {stage}: {stdout}"));
                assert!(
                    position >= previous_position,
                    "out-of-order progress: {stdout}"
                );
                previous_position = position;
            }
            for field in ["raw ", "GiB", "elapsed ", "GB/s including TIFF I/O"] {
                assert!(stdout.contains(field), "missing {field}: {stdout}");
            }
            assert_eq!(stdout.matches("Streaming layout:").count(), 1);
            {
                let reader = RadeltaFileHandle::open(&encoded).unwrap();
                assert_eq!(reader.info().nz, 11);
                assert!(!reader.metadata().is_empty());
                for t in 0..2 {
                    for c in 0..2 {
                        for z in 0..11 {
                            let mut plane = vec![0u16; 1024 * 1024];
                            reader.read_plane(t, c, z, &mut plane).unwrap();
                            assert_eq!(plane, expected[(t * 2 + c) * 11 + z]);
                        }
                    }
                }
            }
            let bytes = fs::read(&encoded).unwrap();
            if let Some(previous) = previous.as_ref() {
                assert_eq!(&bytes, previous, "forced/automatic streams differed");
            }
            previous = Some(bytes);
        }
    }
    fs::remove_dir_all(root).unwrap();
}
