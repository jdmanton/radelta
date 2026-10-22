use radelta_native::*;
use std::io::{self, Cursor, IoSlice, Write};

struct ShortWriter {
    bytes: Vec<u8>,
    interrupt: bool,
}
impl Write for ShortWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_vectored(&[IoSlice::new(bytes)])
    }
    fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
        if self.interrupt {
            self.interrupt = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut remaining = 13;
        for buffer in buffers {
            let n = remaining.min(buffer.len());
            self.bytes.extend_from_slice(&buffer[..n]);
            remaining -= n;
        }
        Ok(13 - remaining)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn borrowed_metadata_output_matches_owned_and_c_apis() {
    let dims = Dims { x: 7, y: 5, z: 3 };
    let nd = Dims5 {
        x: 7,
        y: 5,
        z: 3,
        c: 1,
        t: 1,
    };
    let pixels: Vec<_> = (0..105u16).map(|v| v * 601).collect();
    let containers = [
        compress_u16(&pixels, dims, Options::default()).unwrap(),
        compress_lossy_u16(&pixels, dims, LossyOptions::default()).unwrap(),
        compress_u16_nd(&pixels, nd, Options::default()).unwrap(),
        compress_lossy_u16_nd(&pixels, nd, LossyOptions::default()).unwrap(),
    ];
    for bare in containers {
        let mut source = bare.clone();
        for metadata in [b"calibration".repeat(4096), vec![0, 255, 128, 0], vec![]] {
            let unchanged = source.clone();
            let mut expected = source.clone();
            set_metadata(&mut expected, &metadata).unwrap();
            let mut short = ShortWriter {
                bytes: Vec::new(),
                interrupt: true,
            };
            let written = write_encoded_with_metadata(&mut short, &source, &metadata).unwrap();
            assert_eq!(written, expected.len());
            assert_eq!(short.bytes, expected);
            assert_eq!(read_metadata(&short.bytes).unwrap(), metadata);

            let mut size = 0;
            unsafe {
                assert_eq!(
                    radelta_set_metadata(
                        source.as_ptr(),
                        source.len(),
                        metadata.as_ptr(),
                        metadata.len(),
                        std::ptr::null_mut(),
                        0,
                        &mut size
                    ),
                    RADELTA_BUFFER_TOO_SMALL
                );
                assert_eq!(size, expected.len());
                let mut output = vec![99u8; size];
                assert_eq!(
                    radelta_set_metadata(
                        source.as_ptr(),
                        source.len(),
                        metadata.as_ptr(),
                        metadata.len(),
                        output.as_mut_ptr(),
                        size - 1,
                        &mut size
                    ),
                    RADELTA_BUFFER_TOO_SMALL
                );
                assert!(output.iter().all(|&b| b == 99));
                assert_eq!(
                    radelta_set_metadata(
                        source.as_ptr(),
                        source.len(),
                        metadata.as_ptr(),
                        metadata.len(),
                        output.as_mut_ptr(),
                        output.len(),
                        &mut size
                    ),
                    RADELTA_OK
                );
                assert_eq!(output, expected);
            }
            assert_eq!(source, unchanged);
            source = expected;
        }
        assert_eq!(source, bare);
    }
}

#[test]
fn file_metadata_replacements_preserve_pixels_and_truncate_old_blocks() {
    let path = std::env::temp_dir().join(format!(
        "radelta-output-metadata-{}.rdlt",
        std::process::id()
    ));
    let bare = compress_u16(&[1, 2, 3, 4], Dims { x: 2, y: 2, z: 1 }, Options::default()).unwrap();
    std::fs::write(&path, &bare).unwrap();
    let mut state = 91u32;
    let random: Vec<_> = (0..1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    for data in [&random[..], b"replacement", &random[..], &[][..]] {
        set_file_metadata(&path, data).unwrap();
        let mut expected = bare.clone();
        set_metadata(&mut expected, data).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        assert_eq!(read_file_metadata(&path).unwrap(), data);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn new_file_finishing_matches_existing_format_and_propagates_errors() {
    let bare = compress_u16(&[123], Dims { x: 1, y: 1, z: 1 }, Options::default()).unwrap();
    for metadata in [b"metadata".as_slice(), &[]] {
        let mut expected = bare.clone();
        set_metadata(&mut expected, metadata).unwrap();
        let mut writer = Cursor::new(bare.clone());
        writer.set_position(bare.len() as u64);
        metadata::finish_new_file_metadata(&mut writer, metadata).unwrap();
        assert_eq!(writer.into_inner(), expected);
    }
    struct Zero;
    impl Write for Zero {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(write_encoded_with_metadata(&mut Zero, &bare, b"metadata").is_err());
    let mut unchanged = Vec::new();
    assert!(write_encoded_with_metadata(&mut unchanged, b"invalid", b"metadata").is_err());
    assert!(unchanged.is_empty());
}
