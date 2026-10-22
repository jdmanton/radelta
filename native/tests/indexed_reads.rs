use radelta_native::*;
use std::fs;

#[test]
fn indexed_containers_read_random_planes_and_reject_incomplete_records() {
    let dims = Dims5 {
        x: 3,
        y: 2,
        z: 7,
        c: 2,
        t: 2,
    };
    let pixels: Vec<_> = (0..dims.voxels().unwrap())
        .map(|v| (v * 37) as u16)
        .collect();
    let root = std::env::temp_dir().join(format!("radelta-indexed-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    for lossy in [false, true] {
        let nd = if lossy {
            compress_lossy_u16_nd(&pixels, dims, LossyOptions::default()).unwrap()
        } else {
            compress_u16_nd(&pixels, dims, Options::default()).unwrap()
        };
        let expected = decompress_u16_nd(&nd).unwrap().0;
        let mut stream = if lossy {
            b"RQS3".to_vec()
        } else {
            b"RDS3".to_vec()
        };
        stream.extend_from_slice(&[1, 0, 0, 0]);
        for v in [dims.x, dims.y, dims.z, dims.c, dims.t, 3] {
            stream.extend_from_slice(&(v as u32).to_le_bytes());
        }
        stream.extend_from_slice(&12u64.to_le_bytes());
        for t in 0..dims.t {
            for c in 0..dims.c {
                for z0 in (0..dims.z).step_by(3) {
                    let depth = 3.min(dims.z - z0);
                    let start = ((t * dims.c + c) * dims.z + z0) * 6;
                    let chunk = &pixels[start..start + depth * 6];
                    let d = Dims {
                        x: 3,
                        y: 2,
                        z: depth,
                    };
                    let payload = if lossy {
                        compress_lossy_u16(chunk, d, LossyOptions::default()).unwrap()
                    } else {
                        compress_u16(chunk, d, Options::default()).unwrap()
                    };
                    for v in [t, c, z0, depth] {
                        stream.extend_from_slice(&(v as u32).to_le_bytes());
                    }
                    stream.extend_from_slice(&(payload.len() as u64).to_le_bytes());
                    stream.extend_from_slice(&payload);
                }
            }
        }
        for (kind, bytes) in [("nd", nd), ("stream", stream)] {
            let path = root.join(format!("{kind}-{lossy}.rdlt"));
            fs::write(&path, &bytes).unwrap();
            let file = RadeltaFileHandle::open(&path).unwrap();
            for t in [1, 0] {
                for c in [1, 0] {
                    for z in [6, 0, 3, 2, 5, 1, 4] {
                        let mut out = [0u16; 6];
                        file.read_plane(t, c, z, &mut out).unwrap();
                        let start = ((t * dims.c + c) * dims.z + z) * 6;
                        assert_eq!(out, expected[start..start + 6]);
                    }
                }
            }
            drop(file);
            let mut trailing = bytes.clone();
            trailing.push(0);
            fs::write(&path, trailing).unwrap();
            assert!(RadeltaFileHandle::open(&path).is_err());
            fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
            assert!(RadeltaFileHandle::open(&path).is_err());
            let mut mismatch = bytes;
            let x_offset = if kind == "nd" {
                32 + 8 + 8
            } else {
                40 + 24 + 8
            };
            mismatch[x_offset..x_offset + 4].copy_from_slice(&4u32.to_le_bytes());
            fs::write(&path, mismatch).unwrap();
            assert!(RadeltaFileHandle::open(&path).is_err());
        }
    }
    fs::remove_dir_all(root).unwrap();
}
