use std::{
    fs::{self, File},
    io::BufWriter,
    process::Command,
};
use tiff::encoder::{colortype, Compression, DeflateLevel, TiffEncoder};

// Keep this opt-in: each plane is deliberately larger than 256 MiB and the
// in-memory CLI can require more than 1 GiB. Compression keeps the fixture small.
#[test]
#[ignore = "large-plane regression; requires more than 1 GiB RAM"]
fn large_compressed_xy_plane_roundtrips_in_memory_and_streaming() {
    const WIDTH: usize = 12000;
    const HEIGHT: usize = 12000;
    let root = std::env::temp_dir().join(format!("radelta-large-tiff-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let input = root.join("projection.tif");
    {
        let mut pixels = vec![0u16; WIDTH * HEIGHT];
        pixels[20 * WIDTH + 10] = 12345;
        pixels[11000 * WIDTH + 10000] = 54321;
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&input).unwrap()))
            .unwrap()
            .with_compression(Compression::Deflate(DeflateLevel::Fast));
        let mut page = encoder
            .new_image::<colortype::Gray16>(WIDTH as u32, HEIGHT as u32)
            .unwrap();
        page.rows_per_strip(HEIGHT as u32).unwrap();
        page.write_data(&pixels).unwrap();
    }
    for streaming in [false, true] {
        let output = root.join(if streaming {
            "stream.rdlt"
        } else {
            "memory.rdlt"
        });
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_radelta"));
        cmd.arg("--force");
        cmd.arg("encode").arg(&input).arg(&output);
        if streaming {
            cmd.args(["--stream", "--memory-mib", "2048"]);
        }
        let result = cmd.output().unwrap();
        assert!(
            result.status.success(),
            "streaming={streaming}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let file = radelta_native::RadeltaFileHandle::open(&output).unwrap();
        let mut restored = vec![0u16; WIDTH * HEIGHT];
        file.read_plane(0, 0, 0, &mut restored).unwrap();
        assert_eq!(restored[20 * WIDTH + 10], 12345);
        assert_eq!(restored[11000 * WIDTH + 10000], 54321);
        restored[20 * WIDTH + 10] = 0;
        restored[11000 * WIDTH + 10000] = 0;
        assert!(restored.iter().all(|&v| v == 0));
    }
    fs::remove_dir_all(root).unwrap();
}
