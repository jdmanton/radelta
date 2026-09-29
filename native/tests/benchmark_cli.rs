use std::{fs::File, process::Command};
use tiff::encoder::{colortype, TiffEncoder};

#[test]
fn benchmark_commands_measure_and_validate_a_tiff() {
    let path = std::env::temp_dir().join(format!("radelta-benchmark-{}.tif", std::process::id()));
    {
        let mut tiff = TiffEncoder::new(File::create(&path).unwrap()).unwrap();
        for z in 0..3 {
            let pixels: Vec<u16> = (0..35).map(|v| v * 3 + z).collect();
            tiff.write_image::<colortype::Gray16>(7, 5, &pixels)
                .unwrap();
        }
    }
    for command in ["benchmark", "benchmark-lossy"] {
        let mut args = vec![
            command,
            path.to_str().unwrap(),
            "--repeats",
            "2",
            "--block-depth",
            "2",
        ];
        if command == "benchmark-lossy" {
            args.extend([
                "--offset-adu",
                "0",
                "--gain-adu-per-e",
                "2",
                "--noise-step",
                "2",
            ]);
        }
        let output = Command::new(env!("CARGO_BIN_EXE_radelta"))
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        for field in ["median of 2 runs", "encode rate:", "decode rate:", "ratio:"] {
            assert!(text.contains(field), "{text}");
        }
        assert!(text.contains(if command == "benchmark" {
            "exact:         yes"
        } else {
            "RMSE:"
        }));
    }
    for flag in ["--repeats", "--stream"] {
        let output = Command::new(env!("CARGO_BIN_EXE_radelta"))
            .args(["benchmark", path.to_str().unwrap(), flag, "0"])
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .args(["benchmark-lossy", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    std::fs::remove_file(path).unwrap();
}
