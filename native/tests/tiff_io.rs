use std::{
    fs::{self, File},
    io::{BufReader, BufWriter},
    process::Command,
};
use tiff::{
    decoder::{Decoder, DecodingResult},
    encoder::{colortype, Compression, TiffEncoder, TiffKind, TiffKindBig},
    tags::Predictor,
};

fn write<K: TiffKind>(enc: &mut TiffEncoder<BufWriter<File>, K>) {
    for page in 0..3u16 {
        let mut image = enc.new_image::<colortype::Gray16>(17, 9).unwrap();
        image.rows_per_strip(2).unwrap();
        let data: Vec<_> = (0..153u16)
            .map(|v| v.wrapping_mul(431).wrapping_add(page))
            .collect();
        image.write_data(&data).unwrap();
    }
}

#[test]
fn classic_and_big_tiff_raw_and_compressed_roundtrip_in_memory_and_streaming() {
    let root = std::env::temp_dir().join(format!("radelta-tiff-io-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    for big in [false, true] {
        for compressed in [false, true] {
            let input = root.join("input.tif");
            let writer = BufWriter::new(File::create(&input).unwrap());
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
                write(
                    &mut TiffEncoder::<_, TiffKindBig>::new_big(writer)
                        .unwrap()
                        .with_compression(compression)
                        .with_predictor(predictor),
                );
            } else {
                write(
                    &mut TiffEncoder::new(writer)
                        .unwrap()
                        .with_compression(compression)
                        .with_predictor(predictor),
                );
            }
            for stream in [false, true] {
                let encoded = root.join("image.rdlt");
                let output = root.join("output.tif");
                let mut command = Command::new(env!("CARGO_BIN_EXE_radelta"));
                command.arg("encode").arg(&input).arg(&encoded);
                if stream {
                    command.args(["--stream", "--memory-mib", "1"]);
                }
                let result = command.output().unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let result = Command::new(env!("CARGO_BIN_EXE_radelta"))
                    .arg("decode")
                    .arg(&encoded)
                    .arg(&output)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let mut dec = Decoder::new(BufReader::new(File::open(output).unwrap())).unwrap();
                for page in 0..3u16 {
                    let DecodingResult::U16(data) = dec.read_image().unwrap() else {
                        panic!()
                    };
                    let expected: Vec<_> = (0..153u16)
                        .map(|v| v.wrapping_mul(431).wrapping_add(page))
                        .collect();
                    assert_eq!(data, expected);
                    if page < 2 {
                        dec.next_image().unwrap();
                    }
                }
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
}
