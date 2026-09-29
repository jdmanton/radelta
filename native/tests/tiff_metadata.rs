use std::{
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::Path,
    process::Command,
};
use tiff::{
    decoder::{Decoder, DecodingResult},
    encoder::{colortype, Rational, TiffEncoder, TiffKind, TiffKindBig},
    tags::Tag,
};
fn run(args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}
fn make<K: TiffKind>(mut enc: TiffEncoder<BufWriter<File>, K>, desc: &str) {
    let mut exif = enc.extra_directory().unwrap();
    exif.write_tag(Tag::from_u16_exhaustive(36867), "2026:09:29 12:34:56")
        .unwrap();
    let exif = exif.finish_with_offsets().unwrap();
    for page in 0..4 {
        let mut image = enc.new_image::<colortype::Gray16>(7, 5).unwrap();
        if page == 0 {
            image
                .encoder()
                .write_tag(Tag::ImageDescription, desc)
                .unwrap();
            image
                .encoder()
                .write_tag(Tag::from_u16_exhaustive(34665), &exif.offset)
                .unwrap();
        }
        image
            .encoder()
            .write_tag(Tag::XResolution, Rational { n: 250, d: 3 })
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::YResolution, Rational { n: 250, d: 3 })
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::ResolutionUnit, 3u16)
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::Software, "Acquisition system 42")
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::from_u16_exhaustive(65001),
                &[0u8, 255, page, 0, 128][..],
            )
            .unwrap();
        image.write_data(&[u16::from(page) * 16; 35]).unwrap();
    }
}
fn check(path: &Path, desc: &str, lossy: bool) {
    let mut dec = Decoder::new(BufReader::new(File::open(path).unwrap())).unwrap();
    for page in 0..4 {
        if page == 0 {
            assert_eq!(
                dec.get_tag_ascii_string(Tag::ImageDescription).unwrap(),
                desc
            );
            let pointer = dec
                .get_tag(Tag::from_u16_exhaustive(34665))
                .unwrap()
                .into_ifd_pointer()
                .unwrap();
            let dir = dec.read_directory(pointer).unwrap();
            assert_eq!(
                dec.read_directory_tags(&dir)
                    .get_tag_ascii_string(Tag::from_u16_exhaustive(36867))
                    .unwrap(),
                "2026:09:29 12:34:56"
            );
        }
        assert_eq!(
            dec.get_tag(Tag::XResolution).unwrap(),
            tiff::decoder::ifd::Value::Rational(250, 3)
        );
        assert_eq!(dec.get_tag_u16_vec(Tag::ResolutionUnit).unwrap(), vec![3]);
        assert_eq!(
            dec.get_tag_ascii_string(Tag::Software).unwrap(),
            "Acquisition system 42"
        );
        assert_eq!(
            dec.get_tag_u8_vec(Tag::from_u16_exhaustive(65001)).unwrap(),
            vec![0, 255, page, 0, 128]
        );
        let DecodingResult::U16(pixels) = dec.read_image().unwrap() else {
            panic!()
        };
        let expected = if lossy {
            ((f64::from(page) * 16.0).sqrt().round()).powi(2) as u16
        } else {
            u16::from(page) * 16
        };
        assert_eq!(pixels, vec![expected; 35]);
        if page != 3 {
            dec.next_image().unwrap();
        }
    }
}
#[test]
fn tiff_metadata_and_plane_order_roundtrip_memory_and_streaming() {
    let root = std::env::temp_dir().join(format!("radelta-metadata-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let imagej="ImageJ=1.54\nimages=4\nchannels=2\nslices=2\nframes=1\nhyperstack=true\nunit=um\nspacing=0.42\ncustom=hello\n";
    let ome = r#"<?xml version="1.0"?><OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06"><Image ID="Image:0" Name="Experiment A"><Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint16" SizeX="7" SizeY="5" SizeZ="2" SizeC="2" SizeT="1" PhysicalSizeX="0.125"><Channel ID="Channel:0:0" Name="GFP" SamplesPerPixel="1"/><Channel ID="Channel:0:1" Name="RFP" SamplesPerPixel="1"/><TiffData IFD="0" FirstZ="1" FirstC="1" PlaneCount="1"/><TiffData IFD="1" FirstZ="0" FirstC="1" PlaneCount="1"/><TiffData IFD="2" FirstZ="1" FirstC="0" PlaneCount="1"/><TiffData IFD="3" FirstZ="0" FirstC="0" PlaneCount="1"/></Pixels></Image></OME>"#;
    for (kind, desc) in [("imagej", imagej), ("ome", ome)] {
        let input = root.join(format!("{kind}.tif"));
        let writer = BufWriter::new(File::create(&input).unwrap());
        if kind == "ome" {
            make(
                TiffEncoder::<_, TiffKindBig>::new_big(writer).unwrap(),
                desc,
            );
        } else {
            make(TiffEncoder::new(writer).unwrap(), desc);
        }
        for lossy in [false, true] {
            for stream in [false, true] {
                let encoded = root.join(format!("{kind}-{lossy}-{stream}.rdlt"));
                let output = root.join("restored.tif");
                let mut args = vec![
                    if lossy { "encode-lossy" } else { "encode" },
                    input.to_str().unwrap(),
                    encoded.to_str().unwrap(),
                ];
                if lossy {
                    args.extend([
                        "--offset-adu",
                        "0",
                        "--gain-e-per-adu",
                        "1",
                        "--noise-step",
                        "2",
                    ]);
                }
                if stream {
                    args.extend(["--stream", "--memory-mib", "1"]);
                }
                run(&args);
                let metadata = radelta_native::read_file_metadata(&encoded).unwrap();
                assert!(metadata.starts_with(b"RDTIFF01"));
                run(&[
                    "decode",
                    encoded.to_str().unwrap(),
                    output.to_str().unwrap(),
                ]);
                check(&output, desc, lossy);
                // A deliberate reorder must describe the NEW layout, retaining the
                // original metadata in the private archive tag.
                run(&[
                    "decode",
                    encoded.to_str().unwrap(),
                    output.to_str().unwrap(),
                    "--page-order",
                    "TCZ",
                ]);
                let mut dec = Decoder::new(BufReader::new(File::open(&output).unwrap())).unwrap();
                assert!(dec
                    .get_tag_ascii_string(Tag::ImageDescription)
                    .unwrap()
                    .contains("DimensionOrder=\"XYZCT\""));
                assert_eq!(
                    dec.get_tag_u8_vec(Tag::from_u16_exhaustive(65000)).unwrap(),
                    metadata
                );
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn opaque_c_api_metadata_is_saved_to_tiff() {
    use radelta_native::*;
    let root = std::env::temp_dir().join(format!("radelta-opaque-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let encoded =
        compress_u16(&[1, 2, 3, 4], Dims { x: 2, y: 2, z: 1 }, Options::default()).unwrap();
    let metadata = b"application-defined bytes\0\xff\xfe{JSON or anything}";
    let mut size = 0;
    unsafe {
        assert_eq!(
            radelta_set_metadata(
                encoded.as_ptr(),
                encoded.len(),
                metadata.as_ptr(),
                metadata.len(),
                std::ptr::null_mut(),
                0,
                &mut size
            ),
            RADELTA_BUFFER_TOO_SMALL
        );
        let mut with_metadata = vec![0; size];
        assert_eq!(
            radelta_set_metadata(
                encoded.as_ptr(),
                encoded.len(),
                metadata.as_ptr(),
                metadata.len(),
                with_metadata.as_mut_ptr(),
                with_metadata.len(),
                &mut size
            ),
            RADELTA_OK
        );
        assert_eq!(
            radelta_get_metadata(
                with_metadata.as_ptr(),
                with_metadata.len(),
                std::ptr::null_mut(),
                0,
                &mut size
            ),
            RADELTA_BUFFER_TOO_SMALL
        );
        let mut data = vec![0; size];
        assert_eq!(
            radelta_get_metadata(
                with_metadata.as_ptr(),
                with_metadata.len(),
                data.as_mut_ptr(),
                data.len(),
                &mut size
            ),
            RADELTA_OK
        );
        assert_eq!(data, metadata);
        let path = root.join("image.rdlt");
        let output = root.join("image.tif");
        fs::write(&path, &with_metadata).unwrap();
        run(&["decode", path.to_str().unwrap(), output.to_str().unwrap()]);
        let mut dec = Decoder::new(BufReader::new(File::open(output).unwrap())).unwrap();
        assert_eq!(
            dec.get_tag_u8_vec(Tag::from_u16_exhaustive(65000)).unwrap(),
            metadata
        );
        let DecodingResult::U16(pixels) = dec.read_image().unwrap() else {
            panic!()
        };
        assert_eq!(pixels, vec![1, 2, 3, 4]);
    }
    fs::remove_dir_all(root).unwrap();
}
