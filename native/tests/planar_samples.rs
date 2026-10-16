//! TIFF pages with several planar (PlanarConfiguration=2) samples per pixel
//! are read as channels; interleaved multi-sample pages are rejected.
use std::{
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tiff::{
    decoder::{Decoder, DecodingResult},
    tags::Tag,
};

const W: usize = 9;
const H: usize = 7;

fn value(page: usize, sample: usize, y: usize, x: usize) -> u16 {
    (page * 1000 + sample * 300 + y * 11 + x) as u16
}

struct Fixture {
    pages: usize,
    samples: usize,
    rows_per_strip: usize,
    planar: u16,
    description: Option<&'static str>,
}

fn push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn align(buf: &mut Vec<u8>) {
    if buf.len() % 2 == 1 {
        buf.push(0);
    }
}

/// Write a little-endian, uncompressed, stripped TIFF by hand: the `tiff`
/// crate's encoder cannot produce planar multi-sample pages.
fn write_fixture(path: &Path, f: &Fixture) {
    let strips_per_plane = H.div_ceil(f.rows_per_strip);
    let mut buf = b"II*\0\0\0\0\0".to_vec();
    let mut next_ifd_pointer = 4usize;
    for page in 0..f.pages {
        let mut offsets = Vec::new();
        let mut counts = Vec::new();
        if f.planar == 2 {
            for sample in 0..f.samples {
                for strip in 0..strips_per_plane {
                    offsets.push(buf.len() as u32);
                    let rows = strip * f.rows_per_strip..H.min((strip + 1) * f.rows_per_strip);
                    counts.push((rows.len() * W * 2) as u32);
                    for y in rows {
                        for x in 0..W {
                            push_u16(&mut buf, value(page, sample, y, x));
                        }
                    }
                }
            }
        } else {
            for strip in 0..strips_per_plane {
                offsets.push(buf.len() as u32);
                let rows = strip * f.rows_per_strip..H.min((strip + 1) * f.rows_per_strip);
                counts.push((rows.len() * W * f.samples * 2) as u32);
                for y in rows {
                    for x in 0..W {
                        for sample in 0..f.samples {
                            push_u16(&mut buf, value(page, sample, y, x));
                        }
                    }
                }
            }
        }
        // Out-of-line values, then the IFD itself.
        align(&mut buf);
        let bits_at = buf.len() as u32;
        for _ in 0..f.samples {
            push_u16(&mut buf, 16);
        }
        align(&mut buf);
        let offsets_at = buf.len() as u32;
        offsets.iter().for_each(|&v| push_u32(&mut buf, v));
        let counts_at = buf.len() as u32;
        counts.iter().for_each(|&v| push_u32(&mut buf, v));
        let desc = f.description.filter(|_| page == 0).map(|d| {
            let at = buf.len() as u32;
            buf.extend_from_slice(d.as_bytes());
            buf.push(0);
            align(&mut buf);
            (at, d.len() as u32 + 1)
        });

        // (tag, type, count, value-or-offset); SHORT values sit in the low bytes.
        let mut entries: Vec<(u16, u16, u32, u32)> = vec![
            (256, 4, 1, W as u32),
            (257, 4, 1, H as u32),
            if f.samples <= 2 {
                (
                    258,
                    3,
                    f.samples as u32,
                    16 | if f.samples == 2 { 16 << 16 } else { 0 },
                )
            } else {
                (258, 3, f.samples as u32, bits_at)
            },
            (259, 3, 1, 1),
            (262, 3, 1, 1),
            if offsets.len() == 1 {
                (273, 4, 1, offsets[0])
            } else {
                (273, 4, offsets.len() as u32, offsets_at)
            },
            (277, 3, 1, f.samples as u32),
            (278, 4, 1, f.rows_per_strip as u32),
            if counts.len() == 1 {
                (279, 4, 1, counts[0])
            } else {
                (279, 4, counts.len() as u32, counts_at)
            },
            (284, 3, 1, u32::from(f.planar)),
        ];
        if let Some((at, len)) = desc {
            entries.push((270, 2, len, at));
        }
        entries.sort_by_key(|e| e.0);

        align(&mut buf);
        let ifd_at = buf.len() as u32;
        buf[next_ifd_pointer..next_ifd_pointer + 4].copy_from_slice(&ifd_at.to_le_bytes());
        push_u16(&mut buf, entries.len() as u16);
        for (tag, ty, count, v) in entries {
            push_u16(&mut buf, tag);
            push_u16(&mut buf, ty);
            push_u32(&mut buf, count);
            push_u32(&mut buf, v);
        }
        next_ifd_pointer = buf.len();
        push_u32(&mut buf, 0);
    }
    fs::write(path, buf).unwrap();
}

fn radelta(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radelta"))
        .args(args)
        .output()
        .unwrap()
}

fn run_ok(args: &[&str]) -> Output {
    let output = radelta(args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("radelta-planar-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn expected_plane(page: usize, sample: usize) -> Vec<u16> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| value(page, sample, y, x)))
        .collect()
}

fn check_encoded(path: &Path, pages: usize, samples: usize) {
    let file = radelta_native::RadeltaFileHandle::open(path).unwrap();
    let info = file.info();
    assert_eq!(
        (info.nt, info.nc, info.nz, info.ny, info.nx),
        (1, samples as u32, pages as u32, H as u32, W as u32)
    );
    let mut plane = vec![0u16; W * H];
    for c in 0..samples {
        for z in 0..pages {
            file.read_plane(0, c, z, &mut plane).unwrap();
            assert_eq!(plane, expected_plane(z, c), "channel {c}, plane {z}");
        }
    }
}

fn planar_round_trip(extra: &[&str], name: &str) {
    let dir = scratch(name);
    let input = dir.join("planar.tif");
    let encoded = dir.join("planar.rdlt");
    let decoded = dir.join("decoded.tif");
    let (pages, samples) = (4, 3);
    write_fixture(
        &input,
        &Fixture {
            pages,
            samples,
            rows_per_strip: 3,
            planar: 2,
            description: None,
        },
    );
    let mut args = vec!["encode", input.to_str().unwrap(), encoded.to_str().unwrap()];
    args.extend_from_slice(extra);
    let output = run_ok(&args);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Shape T,C,Z,Y,X: 1, 3, 4, 7, 9"),
        "{stdout}"
    );
    check_encoded(&encoded, pages, samples);

    // Decoding restores the source plane order: page by page, sample fastest,
    // now as single-sample pages.
    run_ok(&[
        "decode",
        encoded.to_str().unwrap(),
        decoded.to_str().unwrap(),
    ]);
    let mut dec = Decoder::new(BufReader::new(File::open(&decoded).unwrap())).unwrap();
    let mut seen = 0;
    loop {
        assert_eq!(dec.get_tag_u64(Tag::SamplesPerPixel).unwrap_or(1), 1);
        match dec.read_image().unwrap() {
            DecodingResult::U16(v) => {
                assert_eq!(
                    v,
                    expected_plane(seen / samples, seen % samples),
                    "page {seen}"
                )
            }
            other => panic!(
                "unexpected TIFF sample type {:?}",
                std::mem::discriminant(&other)
            ),
        }
        seen += 1;
        if !dec.more_images() {
            break;
        }
        dec.next_image().unwrap();
    }
    assert_eq!(seen, pages * samples);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn planar_samples_become_channels() {
    planar_round_trip(&[], "dense");
}

#[test]
fn planar_samples_become_channels_when_streaming() {
    planar_round_trip(&["--stream", "--memory-mib", "1"], "stream");
}

#[test]
fn interleaved_samples_are_rejected_without_output() {
    let dir = scratch("interleaved");
    let input = dir.join("rgb.tif");
    let encoded = dir.join("rgb.rdlt");
    write_fixture(
        &input,
        &Fixture {
            pages: 2,
            samples: 3,
            rows_per_strip: 7,
            planar: 1,
            description: None,
        },
    );
    let output = radelta(&["encode", input.to_str().unwrap(), encoded.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("interleave 3 samples"), "{stderr}");
    assert!(!encoded.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn planar_samples_with_imagej_metadata_need_explicit_choice() {
    let dir = scratch("imagej");
    let input = dir.join("planar_imagej.tif");
    let encoded = dir.join("planar_imagej.rdlt");
    write_fixture(
        &input,
        &Fixture {
            pages: 2,
            samples: 2,
            rows_per_strip: 4,
            planar: 2,
            description: Some("ImageJ=1.54f\nimages=2\nslices=2\n"),
        },
    );
    let output = radelta(&["encode", input.to_str().unwrap(), encoded.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--ignore-metadata"), "{stderr}");
    assert!(!encoded.exists());

    run_ok(&[
        "encode",
        input.to_str().unwrap(),
        encoded.to_str().unwrap(),
        "--ignore-metadata",
    ]);
    check_encoded(&encoded, 2, 2);
    fs::remove_dir_all(dir).unwrap();
}
