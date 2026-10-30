use std::{
    fs::{self, File},
    io::Cursor,
    path::Path,
    process::Command,
};
use tiff::{
    decoder::{Decoder, DecodingResult},
    encoder::{colortype, TiffEncoder, TiffKind, TiffKindBig},
    tags::Tag,
};

fn child<K: TiffKind>(
    enc: &mut TiffEncoder<File, K>,
    x: u32,
    y: u32,
    value: u16,
    children: &[u64],
    unsupported: bool,
) -> u64 {
    let mut dir = enc.extra_directory().unwrap();
    let data = vec![value; (x * y) as usize];
    let offset = if unsupported {
        dir.write_data(vec![7u32; data.len()].as_slice()).unwrap()
    } else {
        dir.write_data(data.as_slice()).unwrap()
    };
    dir.write_tag(Tag::ImageWidth, x).unwrap();
    dir.write_tag(Tag::ImageLength, y).unwrap();
    dir.write_tag(Tag::BitsPerSample, if unsupported { 32u16 } else { 16u16 })
        .unwrap();
    dir.write_tag(Tag::Compression, 1u16).unwrap();
    dir.write_tag(Tag::PhotometricInterpretation, 1u16).unwrap();
    dir.write_tag(Tag::SamplesPerPixel, 1u16).unwrap();
    dir.write_tag(Tag::RowsPerStrip, y).unwrap();
    dir.write_tag(Tag::StripOffsets, offset).unwrap();
    dir.write_tag(
        Tag::StripByteCounts,
        (x * y) * if unsupported { 4 } else { 2 },
    )
    .unwrap();
    dir.write_tag(Tag::NewSubfileType, 1u32).unwrap();
    dir.write_tag(Tag::Software, "pyramid-test").unwrap();
    dir.write_tag(Tag::from_u16_exhaustive(65001), &[0u8, 255, 7][..])
        .unwrap();
    if !children.is_empty() {
        dir.write_tag(Tag::SubIfd, children).unwrap();
    }
    dir.finish_with_offsets().unwrap().pointer.0
}
fn fixture<K: TiffKind>(mut enc: TiffEncoder<File, K>, unsupported: bool) {
    let xml = r#"<OME><Image><Pixels DimensionOrder="XYZCT" Type="uint16" SizeX="32" SizeY="24" SizeT="1" SizeC="2" SizeZ="2"><Channel ID="Channel:0" SamplesPerPixel="1"/><Channel ID="Channel:1" SamplesPerPixel="1"/><TiffData IFD="0" FirstC="0" FirstZ="0"/><TiffData IFD="1" FirstC="1" FirstZ="0"/><TiffData IFD="2" FirstC="0" FirstZ="1"/><TiffData IFD="3" FirstC="1" FirstZ="1"/></Pixels></Image></OME>"#;
    for page in 0..4u16 {
        let leaf = child(&mut enc, 4, 3, 40 + page, &[], unsupported);
        let middle = child(&mut enc, 8, 6, 20 + page, &[leaf], false);
        let sibling = child(&mut enc, 16, 12, 10 + page, &[], false);
        let mut image = enc.new_image::<colortype::Gray16>(32, 24).unwrap();
        if page == 0 {
            image
                .encoder()
                .write_tag(Tag::ImageDescription, xml)
                .unwrap();
        }
        image
            .encoder()
            .write_tag(Tag::SubIfd, &[sibling, middle][..])
            .unwrap();
        image.write_data(&vec![100 + page; 32 * 24]).unwrap();
    }
}
fn at(path: &Path, offset: u64) -> Decoder<Cursor<Vec<u8>>> {
    let mut bytes = fs::read(path).unwrap();
    let big_endian = &bytes[..2] == b"MM";
    let version = if big_endian {
        u16::from_be_bytes(bytes[2..4].try_into().unwrap())
    } else {
        u16::from_le_bytes(bytes[2..4].try_into().unwrap())
    };
    if version == 43 {
        bytes[8..16].copy_from_slice(&if big_endian {
            offset.to_be_bytes()
        } else {
            offset.to_le_bytes()
        });
    } else {
        let offset = u32::try_from(offset).unwrap();
        bytes[4..8].copy_from_slice(&if big_endian {
            offset.to_be_bytes()
        } else {
            offset.to_le_bytes()
        });
    }
    Decoder::new(Cursor::new(bytes)).unwrap()
}
fn check_child(path: &Path, offset: u64, value: u16, x: u32, y: u32) -> Decoder<Cursor<Vec<u8>>> {
    let mut dec = at(path, offset);
    assert_eq!(dec.dimensions().unwrap(), (x, y));
    assert_eq!(dec.get_tag_u32(Tag::NewSubfileType).unwrap(), 1);
    assert_eq!(
        dec.get_tag_ascii_string(Tag::Software).unwrap(),
        "pyramid-test"
    );
    let mut raw = tiff::tags::ValueBuffer::empty(tiff::tags::Type::BYTE);
    dec.image_ifd()
        .find_tag_buf(Tag::from_u16_exhaustive(65001), &mut raw)
        .unwrap();
    assert_eq!(raw.as_bytes(), [0, 255, 7]);
    let DecodingResult::U16(pixels) = dec.read_image().unwrap() else {
        panic!()
    };
    assert!(pixels.iter().all(|&v| v == value));
    dec
}

#[test]
fn pyramids_keep_nested_pixels_metadata_and_follow_their_parent_when_reordered() {
    let root = std::env::temp_dir().join(format!("radelta-pyramid-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let input = root.join("input.ome.tif");
    let encoded = root.join("image.rdlt");
    let output = root.join("output.ome.tif");
    for big in [false, true] {
        if big {
            fixture(
                TiffEncoder::<_, TiffKindBig>::new_big(File::create(&input).unwrap()).unwrap(),
                false,
            );
        } else {
            fixture(
                TiffEncoder::new(File::create(&input).unwrap()).unwrap(),
                false,
            );
        }
        for streaming in [false, true] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_radelta"));
            cmd.arg("--force");
            cmd.arg("encode").arg(&input).arg(&encoded);
            if streaming {
                cmd.arg("--stream");
            }
            let result = cmd.output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(radelta_native::read_file_metadata(&encoded)
                .unwrap()
                .starts_with(b"RDTIFF02"));
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
                let mut dec = Decoder::new(File::open(&output).unwrap()).unwrap();
                let order = if reorder { [0, 2, 1, 3] } else { [0, 1, 2, 3] };
                for (i, page) in order.into_iter().enumerate() {
                    let children = dec.get_tag_u64_vec(Tag::SubIfd).unwrap();
                    assert_eq!(children.len(), 2);
                    check_child(&output, children[0], 10 + page, 16, 12);
                    let mut middle = check_child(&output, children[1], 20 + page, 8, 6);
                    let leaf = middle.get_tag_u64_vec(Tag::SubIfd).unwrap();
                    assert_eq!(leaf.len(), 1);
                    check_child(&output, leaf[0], 40 + page, 4, 3);
                    let DecodingResult::U16(pixels) = dec.read_image().unwrap() else {
                        panic!()
                    };
                    assert!(pixels.iter().all(|&v| v == 100 + page));
                    if i < 3 {
                        dec.next_image().unwrap();
                    }
                }
            }
        }
    }
    // Valid outer metadata framing must not hide malformed pyramid records.
    let mut metadata = radelta_native::read_file_metadata(&encoded).unwrap();
    *metadata.last_mut().unwrap() = 255;
    radelta_native::set_file_metadata(&encoded, &metadata).unwrap();
    fs::write(&output, b"keep existing TIFF").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .arg("--force")
        .arg("decode")
        .arg(&encoded)
        .arg(&output)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(fs::read(&output).unwrap(), b"keep existing TIFF");
    // Unsupported children must fail atomically, not disappear from the result.
    fixture(
        TiffEncoder::new(File::create(&input).unwrap()).unwrap(),
        true,
    );
    for streaming in [false, true] {
        fs::write(&encoded, b"keep existing output").unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_radelta"));
        cmd.arg("--force");
        cmd.arg("encode").arg(&input).arg(&encoded);
        if streaming {
            cmd.arg("--stream");
        }
        let result = cmd.output().unwrap();
        assert!(!result.status.success());
        assert_eq!(fs::read(&encoded).unwrap(), b"keep existing output");
    }
    fs::remove_dir_all(root).unwrap();
}
