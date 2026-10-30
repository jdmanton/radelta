//! Regression cases adapted from the alternative implementation. ImageJ reads
//! `images=N` planes consecutively from the first strip offset; TIFF directories
//! between planes corrupt that reading. The IFD chain must also stay valid.
use std::{fs, path::PathBuf, process::Command};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("radelta_decode_layout_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn run(args: &[&str]) {
    let result = Command::new(env!("CARGO_BIN_EXE_radelta"))
        .arg("--force")
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn decoded_imagej_hyperstack_reads_back_to_back_and_by_ifds() {
    let (w, h, c, z, t) = (21usize, 13usize, 2usize, 5usize, 2usize);
    let n = c * z * t;
    let plane = w * h;
    let pages: Vec<Vec<u16>> = (0..n)
        .map(|p| (0..plane).map(|k| (p * 1000 + k * 3) as u16).collect())
        .collect();
    let src = temp_path("src.tif");
    {
        let mut enc =
            TiffEncoder::new(std::io::BufWriter::new(fs::File::create(&src).unwrap())).unwrap();
        let desc = format!(
            "ImageJ=1.54p\nimages={n}\nchannels={c}\nslices={z}\nframes={t}\nhyperstack=true\nmode=composite\n"
        );
        for (p, px) in pages.iter().enumerate() {
            let mut image = enc
                .new_image::<colortype::Gray16>(w as u32, h as u32)
                .unwrap();
            if p == 0 {
                image
                    .encoder()
                    .write_tag(Tag::ImageDescription, desc.as_str())
                    .unwrap();
            }
            image.rows_per_strip(3).unwrap();
            image.write_data(px).unwrap();
        }
    }
    for stream in [false, true] {
        let rdlt = temp_path("a.rdlt");
        let out = temp_path("decoded.tif");
        let mut args = vec!["encode", src.to_str().unwrap(), rdlt.to_str().unwrap()];
        if stream {
            args.extend(["--stream", "--memory-mib", "1"]);
        }
        run(&args);
        run(&["decode", rdlt.to_str().unwrap(), out.to_str().unwrap()]);

        // ImageJ's reading: first strip offset of IFD0, then plane after plane.
        let bytes = fs::read(&out).unwrap();
        let mut dec = Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        let start = dec.get_tag_u64(Tag::StripOffsets).unwrap() as usize;
        let desc = dec.get_tag_ascii_string(Tag::ImageDescription).unwrap();
        assert!(
            desc.starts_with("ImageJ=") && desc.contains(&format!("images={n}")),
            "{desc}"
        );
        assert_eq!(start % 2, 0, "word-aligned pixel block");
        for (p, px) in pages.iter().enumerate() {
            let at = start + p * plane * 2;
            let got: Vec<u16> = bytes[at..at + plane * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                .collect();
            assert_eq!(&got, px, "back-to-back plane {p}");
        }

        // Generic reading: follow the IFD chain.
        let mut p = 0;
        loop {
            match dec.read_image().unwrap() {
                DecodingResult::U16(v) => assert_eq!(v, pages[p], "IFD plane {p}"),
                other => panic!("unexpected sample type {other:?}"),
            }
            p += 1;
            if !dec.more_images() {
                break;
            }
            dec.next_image().unwrap();
        }
        assert_eq!(p, n, "IFD count");
        let again = temp_path("again.rdlt");
        run(&["encode", out.to_str().unwrap(), again.to_str().unwrap()]);
        let handle = radelta_native::RadeltaFileHandle::open(&again).unwrap();
        let mut plane_out = vec![0; plane];
        for frame in 0..t {
            for slice in 0..z {
                for channel in 0..c {
                    handle
                        .read_plane(frame, channel, slice, &mut plane_out)
                        .unwrap();
                    assert_eq!(plane_out, pages[(frame * z + slice) * c + channel]);
                }
            }
        }
    }
}

/// A single-channel ImageJ image keeps its LUT (TIFF ColorMap, tag 320) through
/// encode + decode, and a single plane ends its IFD chain (next IFD = 0).
#[test]
fn single_plane_keeps_its_colormap() {
    let (w, h) = (16usize, 9usize);
    let px: Vec<u16> = (0..w * h).map(|k| (k * 7) as u16).collect();
    let map: Vec<u16> = (0..768)
        .map(|i| if i < 256 { (i as u16) << 8 } else { 0 })
        .collect(); // red LUT
    for imagej in [true, false] {
        let src = temp_path("lut_src.tif");
        {
            let mut enc =
                TiffEncoder::new(std::io::BufWriter::new(fs::File::create(&src).unwrap())).unwrap();
            let mut image = enc
                .new_image::<colortype::Gray16>(w as u32, h as u32)
                .unwrap();
            if imagej {
                image
                    .encoder()
                    .write_tag(Tag::ImageDescription, "ImageJ=1.54p\nmin=0.0\nmax=1000.0\n")
                    .unwrap();
            }
            image.encoder().write_tag(Tag::ColorMap, &map[..]).unwrap();
            image.write_data(&px).unwrap();
        }
        let rdlt = temp_path("lut.rdlt");
        let out = temp_path("lut_decoded.tif");
        for args in [
            vec!["encode", src.to_str().unwrap(), rdlt.to_str().unwrap()],
            vec!["decode", rdlt.to_str().unwrap(), out.to_str().unwrap()],
        ] {
            let r = Command::new(env!("CARGO_BIN_EXE_radelta"))
                .arg("--force")
                .args(&args)
                .output()
                .unwrap();
            assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
        }
        let bytes = fs::read(&out).unwrap();
        let mut dec = Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(dec.get_tag_u16_vec(Tag::ColorMap).unwrap(), map, "ColorMap");
        match dec.read_image().unwrap() {
            DecodingResult::U16(v) => assert_eq!(v, px),
            other => panic!("unexpected sample type {other:?}"),
        }
        assert!(!dec.more_images(), "a single plane must end the IFD chain");
    }
}

#[test]
fn interleaved_export_matches_with_and_without_room_to_retain_channel_volumes() {
    use radelta_native::{compress_u16_nd, Dims5, Options};
    let d = Dims5 {
        x: 512,
        y: 128,
        z: 5,
        c: 3,
        t: 1,
    };
    let plane = d.x * d.y;
    let pixels: Vec<_> = (0..d.voxels().unwrap())
        .map(|i| (i * 31 + i / plane * 997) as u16)
        .collect();
    let path = temp_path("interleaved-budget.rdlt");
    fs::write(
        &path,
        compress_u16_nd(&pixels, d, Options::default()).unwrap(),
    )
    .unwrap();
    for budget in ["1", "16"] {
        let out = temp_path(&format!("interleaved-budget-{budget}.tif"));
        run(&[
            "decode",
            path.to_str().unwrap(),
            out.to_str().unwrap(),
            "--page-order",
            "TZC",
            "--memory-mib",
            budget,
        ]);
        let mut decoder = Decoder::new(fs::File::open(&out).unwrap()).unwrap();
        for z in 0..d.z {
            for c in 0..d.c {
                let at = (c * d.z + z) * plane;
                let DecodingResult::U16(got) = decoder.read_image().unwrap() else {
                    panic!("unexpected sample type")
                };
                assert_eq!(got, pixels[at..at + plane]);
                if z + 1 != d.z || c + 1 != d.c {
                    decoder.next_image().unwrap();
                }
            }
        }
        assert!(!decoder.more_images());
    }
}
