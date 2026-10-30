use std::{
    fs::{self, File},
    io::BufReader,
    process::Command,
};
use tiff::{
    decoder::{Decoder, DecodingResult},
    encoder::{colortype, Compression, TiffEncoder, TiffKind, TiffKindBig},
    tags::{Predictor, Tag},
};

fn fixture<K: TiffKind>(enc: &mut TiffEncoder<File, K>) {
    for p in 0..6u8 {
        let mut image = enc.new_image::<colortype::Gray8>(32, 16).unwrap();
        image.rows_per_strip(3).unwrap();
        if p == 0 {
            image.encoder().write_tag(Tag::ImageDescription,
                "<OME><Image ID=\"Image:0\"><Pixels ID=\"Pixels:0\" DimensionOrder=\"XYCZT\" Type=\"uint8\" SizeX=\"32\" SizeY=\"16\" SizeZ=\"3\" SizeC=\"2\" SizeT=\"1\"><Channel ID=\"Channel:0\" SamplesPerPixel=\"1\"/><Channel ID=\"Channel:1\" SamplesPerPixel=\"1\"/><TiffData IFD=\"0\" PlaneCount=\"6\"/></Pixels></Image></OME>").unwrap();
        }
        let pixels: Vec<u8> = (0..512).map(|i| (i as u8).wrapping_add(p)).collect();
        image.write_data(&pixels).unwrap();
    }
}

#[test]
fn uint8_classic_big_raw_predictor_and_streaming_preserve_depth_and_mapping() {
    let root = std::env::temp_dir().join(format!("radelta-u8-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let input = root.join("input.ome.tif");
    let encoded = root.join("image.rdlt");
    let output = root.join("output.ome.tif");
    for big in [false, true] {
        for compressed in [false, true] {
            let compression = if compressed {
                Compression::Lzw
            } else {
                Compression::Uncompressed
            };
            let predictor = if compressed {
                Predictor::Horizontal
            } else {
                Predictor::None
            };
            if big {
                fixture(
                    &mut TiffEncoder::<_, TiffKindBig>::new_big(File::create(&input).unwrap())
                        .unwrap()
                        .with_compression(compression)
                        .with_predictor(predictor),
                );
            } else {
                fixture(
                    &mut TiffEncoder::new(File::create(&input).unwrap())
                        .unwrap()
                        .with_compression(compression)
                        .with_predictor(predictor),
                );
            }
            for stream in [false, true] {
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_radelta"));
                cmd.arg("--force");
                cmd.arg("encode").arg(&input).arg(&encoded);
                if stream {
                    cmd.args(["--stream", "--memory-mib", "1"]);
                }
                let result = cmd.output().unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                for reorder in [false, true] {
                    let mut cmd = Command::new(env!("CARGO_BIN_EXE_radelta"));
                    cmd.arg("--force");
                    cmd.arg("decode").arg(&encoded).arg(&output);
                    if reorder {
                        cmd.args(["--page-order", "TCZ"]);
                    }
                    let result = cmd.output().unwrap();
                    assert!(
                        result.status.success(),
                        "{}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                    let mut dec =
                        Decoder::new(BufReader::new(File::open(&output).unwrap())).unwrap();
                    let desc = dec.get_tag_ascii_string(Tag::ImageDescription).unwrap();
                    assert!(desc.contains("Type=\"uint8\""));
                    for p in 0..6 {
                        assert_eq!(dec.get_tag_u16_vec(Tag::BitsPerSample).unwrap(), [8]);
                        let DecodingResult::U8(pixels) = dec.read_image().unwrap() else {
                            panic!("uint8 TIFF widened on export")
                        };
                        let source = if reorder { (p % 3) * 2 + p / 3 } else { p };
                        let expected: Vec<u8> = (0..512)
                            .map(|i| (i as u8).wrapping_add(source as u8))
                            .collect();
                        assert_eq!(pixels, expected);
                        if p < 5 {
                            dec.next_image().unwrap();
                        }
                    }
                }
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
}
