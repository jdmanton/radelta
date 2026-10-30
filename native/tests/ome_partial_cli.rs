use std::{
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::Path,
    process::{Command, Output},
};
use tiff::{
    decoder::{Decoder, DecodingResult},
    encoder::{colortype, TiffEncoder, TiffKind, TiffKindBig},
    tags::Tag,
};

fn run(args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .arg("--force")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn description() -> String {
    let mut entries = String::new();
    for t in 0..2 {
        for c in 0..4 {
            if (t, c) != (1, 2) {
                entries.push_str(&format!(r#"<ome:TiffData IFD="0" FirstT="{t}" FirstC="{c}" PlaneCount="4"><ome:UUID FileName="missing_T{t}_C{c}.ome.tif">foreign-{t}-{c}</ome:UUID></ome:TiffData>"#));
            }
        }
    }
    for ifd in 0..4 {
        entries.push_str(&format!(r#"<ome:TiffData IFD="{ifd}" FirstT="1" FirstC="2" FirstZ="{}" PlaneCount="1"><ome:UUID FileName="original &amp; acquisition_C02.ome.tif">this-file</ome:UUID></ome:TiffData>"#,3-ifd));
    }
    format!(
        r#"<?xml version="1.0"?><ome:OME xmlns:ome="http://www.openmicroscopy.org/Schemas/OME/2016-06" UUID="this-file"><ome:Image ID="Image:0" Name="Four channel acquisition"><ome:Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint16" SizeX="7" SizeY="5" SizeT="2" SizeC="4" SizeZ="4" PhysicalSizeX="0.125" PhysicalSizeY="0.125" PhysicalSizeZ="0.42"><ome:Channel ID="Channel:0:0" Name="GFP" SamplesPerPixel="1"/><ome:Channel ID="Channel:0:1" Name="RFP" SamplesPerPixel="1"/><ome:Channel ID="Channel:0:2" Name="Far red" SamplesPerPixel="1"/><ome:Channel ID="Channel:0:3" Name="Infrared" SamplesPerPixel="1"/>{entries}</ome:Pixels></ome:Image></ome:OME>"#
    )
}

fn write_pages<K: TiffKind>(mut encoder: TiffEncoder<BufWriter<File>, K>, desc: &str) {
    for ifd in 0..4u16 {
        let mut page = encoder.new_image::<colortype::Gray16>(7, 5).unwrap();
        if ifd == 0 {
            page.encoder()
                .write_tag(Tag::ImageDescription, desc)
                .unwrap();
        }
        page.encoder()
            .write_tag(Tag::Software, "Acquisition system")
            .unwrap();
        page.encoder()
            .write_tag(Tag::from_u16_exhaustive(65001), ifd)
            .unwrap();
        page.write_data(&[(4 - ifd) * 64; 35]).unwrap();
    }
}

fn write_fixture(path: &Path, desc: &str, big: bool) {
    let writer = BufWriter::new(File::create(path).unwrap());
    if big {
        write_pages(
            TiffEncoder::<_, TiffKindBig>::new_big(writer).unwrap(),
            desc,
        );
    } else {
        write_pages(TiffEncoder::new(writer).unwrap(), desc);
    }
}

fn value(z: u16, lossy: bool) -> u16 {
    let v = (z + 1) * 64;
    if lossy {
        f64::from(v).sqrt().round().powi(2) as u16
    } else {
        v
    }
}

#[test]
fn split_ome_channel_roundtrips_pixels_metadata_and_original_ifd_order() {
    let root = std::env::temp_dir().join(format!("radelta-ome-partial-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let desc = description();
    for big in [false, true] {
        let input = root.join("renamed channel.ome.tif");
        write_fixture(&input, &desc, big);
        for lossy in [false, true] {
            for stream in [false, true] {
                let encoded = root.join("channel.rdlt");
                let decoded = root.join("restored.ome.tif");
                let mut args = vec![
                    if lossy { "encode-lossy" } else { "encode" },
                    input.to_str().unwrap(),
                    encoded.to_str().unwrap(),
                ];
                if lossy {
                    args.extend(["--offset-adu", "0", "--gain-e-per-adu", "1"]);
                }
                if stream {
                    args.extend(["--stream", "--memory-mib", "1"]);
                }
                run(&args);
                let reader = radelta_native::RadeltaFileHandle::open(&encoded).unwrap();
                let info = reader.info();
                assert_eq!((info.nt, info.nc, info.nz), (1, 1, 4));
                for z in 0..4 {
                    let mut pixels = vec![0; 35];
                    reader.read_plane(0, 0, z, &mut pixels).unwrap();
                    assert_eq!(pixels, vec![value(z as u16, lossy); 35]);
                }
                drop(reader);
                run(&[
                    "decode",
                    encoded.to_str().unwrap(),
                    decoded.to_str().unwrap(),
                ]);
                let mut tif = Decoder::new(BufReader::new(File::open(&decoded).unwrap())).unwrap();
                assert_eq!(
                    tif.get_tag_ascii_string(Tag::ImageDescription).unwrap(),
                    desc
                );
                for ifd in 0..4u16 {
                    assert_eq!(
                        tif.get_tag_u32(Tag::from_u16_exhaustive(65001)).unwrap(),
                        u32::from(ifd)
                    );
                    assert_eq!(
                        tif.get_tag_ascii_string(Tag::Software).unwrap(),
                        "Acquisition system"
                    );
                    let DecodingResult::U16(pixels) = tif.read_image().unwrap() else {
                        panic!()
                    };
                    assert_eq!(pixels, vec![value(3 - ifd, lossy); 35]);
                    if ifd != 3 {
                        tif.next_image().unwrap();
                    }
                }
                assert!(!tif.more_images());
                drop(tif);
                // A renamed restored TIFF still identifies its own planes by UUID.
                let again = root.join("again.rdlt");
                run(&[
                    "encode",
                    decoded.to_str().unwrap(),
                    again.to_str().unwrap(),
                    "--stream",
                ]);
                assert_eq!(
                    radelta_native::RadeltaFileHandle::open(&again)
                        .unwrap()
                        .info()
                        .nc,
                    1
                );
                // Explicit reordering publishes a self-contained local OME layout.
                run(&[
                    "decode",
                    encoded.to_str().unwrap(),
                    decoded.to_str().unwrap(),
                    "--page-order",
                    "TCZ",
                ]);
                let mut tif = Decoder::new(BufReader::new(File::open(&decoded).unwrap())).unwrap();
                let local = tif.get_tag_ascii_string(Tag::ImageDescription).unwrap();
                assert!(local.contains("SizeC=\"1\"") && local.contains("SizeT=\"1\""));
                assert!(tif
                    .get_tag_u8_vec(Tag::from_u16_exhaustive(65000))
                    .unwrap()
                    .starts_with(b"RDTIFF01"));
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_local_ome_mapping_preserves_existing_output() {
    let root = std::env::temp_dir().join(format!("radelta-ome-invalid-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let input = root.join("bad.ome.tif");
    let encoded = root.join("existing.rdlt");
    write_fixture(
        &input,
        &description().replace("FirstZ=\"3\"", "FirstZ=\"0\""),
        false,
    );
    fs::write(&encoded, b"existing result").unwrap();
    for stream in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_radelta"));
        command.arg("--force");
        command.arg("encode").arg(&input).arg(&encoded);
        if stream {
            command.arg("--stream");
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("same TCZ plane"));
        assert_eq!(fs::read(&encoded).unwrap(), b"existing result");
    }
    fs::remove_dir_all(root).unwrap();
}
