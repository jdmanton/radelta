use radelta_native::{
    compress_lossy_u16, compress_u16, decompress_u16, Dims, LossyOptions, Options,
    RadeltaFileHandle,
};
use std::{
    ffi::{c_char, c_void, CString},
    fs,
    process::Command,
};

extern "C" {
    fn radelta_file_open(path: *const c_char, handle: *mut *mut c_void) -> i32;
}

#[test]
fn count_child() {
    let Some(path) = std::env::var_os("RADELTA_TEST_COUNT_FILE") else {
        return;
    };
    let path = CString::new(path.to_str().unwrap()).unwrap();
    let mut handle = std::ptr::null_mut();
    let status = unsafe { radelta_file_open(path.as_ptr(), &mut handle) };
    assert_ne!(status, 0);
    assert!(handle.is_null());
}

#[test]
fn tiny_containers_cannot_request_large_indexes_or_abort_c_callers() {
    let root = std::env::temp_dir().join(format!("radelta-counts-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    for magic in [b"RDM2", b"RDQ2", b"RDS3", b"RQS3"] {
        let stream = magic[2] == b'S';
        // RQS3 has S at offset 2 as well. The product fits u64 but not an index allocation.
        let mut bytes = magic.to_vec();
        bytes.extend_from_slice(&[1, 0, 0, 0]);
        let n = if stream { u32::MAX } else { 1 << 28 };
        let dims = if stream {
            [1, 1, n, n, 1]
        } else {
            [1, 1, 1, n, 1]
        };
        for value in dims {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        if stream {
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&((n as u64) * (n as u64)).to_le_bytes());
        } else {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        let path = root.join(format!("{}.rdlt", String::from_utf8_lossy(magic)));
        fs::write(&path, bytes).unwrap();
        assert!(RadeltaFileHandle::open(&path).is_err());
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "count_child", "--nocapture"])
            .env("RADELTA_TEST_COUNT_FILE", &path)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "C API child failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let cli = Command::new(env!("CARGO_BIN_EXE_radelta"))
            .arg("--force")
            .arg("decode")
            .arg(&path)
            .arg(root.join("out.tif"))
            .output()
            .unwrap();
        assert_eq!(cli.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&cli.stderr).contains("count exceeds available file records")
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn checksummed_volume_headers_cannot_request_absent_block_indexes() {
    let dims = Dims { x: 2, y: 2, z: 1 };
    for mut bytes in [
        compress_u16(&[100; 4], dims, Options::default()).unwrap(),
        compress_lossy_u16(&[100; 4], dims, LossyOptions::default()).unwrap(),
    ] {
        let lossy = &bytes[..4] == b"RDLQ";
        bytes[16..20].copy_from_slice(&(1u32 << 28).to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        let count_offset = if lossy { 60 } else { 40 };
        bytes[count_offset..count_offset + 4].copy_from_slice(&(1u32 << 28).to_le_bytes());
        let q_offset = if lossy { 56 } else { 32 };
        let q = u32::from_le_bytes(bytes[q_offset..q_offset + 4].try_into().unwrap()) as usize;
        let r = if lossy {
            0
        } else {
            u32::from_le_bytes(bytes[36..40].try_into().unwrap()) as usize
        };
        let end = count_offset + 4 + q + r;
        let mut crc = crc32fast::Hasher::new();
        crc.update(&bytes[..6]);
        crc.update(&bytes[8..end]);
        bytes[end..end + 4].copy_from_slice(&crc.finalize().to_le_bytes());
        assert!(decompress_u16(&bytes)
            .unwrap_err()
            .to_string()
            .contains("block count exceeds available records"));
    }
}
