//! C ABI wrappers for the in-memory Radelta codec.
//!
//! The ABI is intentionally thin: it validates pointers and dimensions and
//! calls the safe Rust API directly on caller-owned buffers. The exported symbol
//! names and status codes are mirrored in `include/radelta.h`.

use super::{
    compress_lossy_u16, compress_lossy_u16_nd, compress_u16, compress_u16_nd, decompress_u16_into,
    decompress_u16_nd_into, inspect_dims, inspect_shape, Dims, Dims5, LossyOptions, Options,
};
use std::slice;

pub const RADELTA_OK: i32 = 0;
pub const RADELTA_BUFFER_TOO_SMALL: i32 = 1;
pub const RADELTA_INVALID_ARGUMENT: i32 = -1;
pub const RADELTA_CODEC_ERROR: i32 = -2;

/// Compress a contiguous XYZ uint16 volume through the C ABI.
///
/// # Safety
/// `input` must reference `input_voxels` readable `u16` values. `options`, when
/// non-null, must point to a valid [`Options`]. `output_size` must be writable.
/// If `output` is non-null, it must reference at least `output_capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn radelta_compress_u16(
    input: *const u16,
    input_voxels: usize,
    nx: u32,
    ny: u32,
    nz: u32,
    options: *const Options,
    output: *mut u8,
    output_capacity: usize,
    output_size: *mut usize,
) -> i32 {
    if input.is_null() || output_size.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let dims = Dims {
        x: nx as usize,
        y: ny as usize,
        z: nz as usize,
    };
    let expected = match dims.voxels() {
        Ok(v) => v,
        Err(_) => return RADELTA_INVALID_ARGUMENT,
    };
    if expected != input_voxels {
        return RADELTA_INVALID_ARGUMENT;
    }
    let opts = if options.is_null() {
        Options::default()
    } else {
        *options
    };
    let src = slice::from_raw_parts(input, input_voxels);
    let encoded = match compress_u16(src, dims, opts) {
        Ok(v) => v,
        Err(_) => return RADELTA_CODEC_ERROR,
    };
    *output_size = encoded.len();
    if output.is_null() || output_capacity < encoded.len() {
        return RADELTA_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
    RADELTA_OK
}

/// Compress a contiguous XYZ uint16 volume using calibrated lossy mode.
///
/// # Safety
/// `input` must reference `input_voxels` readable `u16` values. `options`, when
/// non-null, must point to a valid [`LossyOptions`]. `output_size` must be
/// writable. If `output` is non-null, it must reference `output_capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn radelta_compress_lossy_u16(
    input: *const u16,
    input_voxels: usize,
    nx: u32,
    ny: u32,
    nz: u32,
    options: *const LossyOptions,
    output: *mut u8,
    output_capacity: usize,
    output_size: *mut usize,
) -> i32 {
    if input.is_null() || output_size.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let dims = Dims {
        x: nx as usize,
        y: ny as usize,
        z: nz as usize,
    };
    let expected = match dims.voxels() {
        Ok(v) => v,
        Err(_) => return RADELTA_INVALID_ARGUMENT,
    };
    if expected != input_voxels {
        return RADELTA_INVALID_ARGUMENT;
    }
    let opts = if options.is_null() {
        LossyOptions::default()
    } else {
        *options
    };
    let src = slice::from_raw_parts(input, input_voxels);
    let encoded = match compress_lossy_u16(src, dims, opts) {
        Ok(v) => v,
        Err(_) => return RADELTA_CODEC_ERROR,
    };
    *output_size = encoded.len();
    if output.is_null() || output_capacity < encoded.len() {
        return RADELTA_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
    RADELTA_OK
}

/// Decode an RDL1/RDLQ stream through the C ABI.
///
/// # Safety
/// `input` must reference `input_size` readable bytes and `output_voxels` must be
/// writable. Non-null dimension pointers must be writable. If `output` is
/// non-null, it must reference `output_capacity_voxels` writable `u16` values.
#[no_mangle]
pub unsafe extern "C" fn radelta_decompress_u16(
    input: *const u8,
    input_size: usize,
    output: *mut u16,
    output_capacity_voxels: usize,
    output_voxels: *mut usize,
    nx: *mut u32,
    ny: *mut u32,
    nz: *mut u32,
) -> i32 {
    if input.is_null() || output_voxels.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let src = slice::from_raw_parts(input, input_size);
    let dims = match inspect_dims(src) {
        Ok(v) => v,
        Err(_) => return RADELTA_CODEC_ERROR,
    };
    let needed = match dims.voxels() {
        Ok(v) => v,
        Err(_) => return RADELTA_CODEC_ERROR,
    };
    *output_voxels = needed;
    if !nx.is_null() {
        *nx = dims.x as u32;
    }
    if !ny.is_null() {
        *ny = dims.y as u32;
    }
    if !nz.is_null() {
        *nz = dims.z as u32;
    }
    if output.is_null() || output_capacity_voxels < needed {
        return RADELTA_BUFFER_TOO_SMALL;
    }
    let dst = slice::from_raw_parts_mut(output, needed);
    match decompress_u16_into(src, dst) {
        Ok(decoded_dims) if decoded_dims == dims => RADELTA_OK,
        Ok(_) | Err(_) => RADELTA_CODEC_ERROR,
    }
}

/// Compress canonical contiguous TCZYX uint16 data losslessly through the C ABI.
///
/// # Safety
/// `input` must reference `input_voxels` readable `u16` values. `options`, when
/// non-null, must point to a valid [`Options`]. `output_size` must be writable.
/// If `output` is non-null, it must reference at least `output_capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn radelta_compress_u16_nd(
    input: *const u16,
    input_voxels: usize,
    nx: u32,
    ny: u32,
    nz: u32,
    nc: u32,
    nt: u32,
    options: *const Options,
    output: *mut u8,
    output_capacity: usize,
    output_size: *mut usize,
) -> i32 {
    if input.is_null() || output_size.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let dims = Dims5 {
        x: nx as usize,
        y: ny as usize,
        z: nz as usize,
        c: nc as usize,
        t: nt as usize,
    };
    let expected = match dims.voxels() {
        Ok(v) => v,
        Err(_) => return RADELTA_INVALID_ARGUMENT,
    };
    if expected != input_voxels {
        return RADELTA_INVALID_ARGUMENT;
    }
    let opts = if options.is_null() {
        Options::default()
    } else {
        *options
    };
    let src = slice::from_raw_parts(input, input_voxels);
    let encoded = match compress_u16_nd(src, dims, opts) {
        Ok(v) => v,
        Err(_) => return RADELTA_CODEC_ERROR,
    };
    *output_size = encoded.len();
    if output.is_null() || output_capacity < encoded.len() {
        return RADELTA_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
    RADELTA_OK
}

/// Compress canonical contiguous TCZYX uint16 data using calibrated lossy mode.
///
/// # Safety
/// `input` must reference `input_voxels` readable `u16` values. `options`, when
/// non-null, must point to a valid [`LossyOptions`]. `output_size` must be
/// writable. If `output` is non-null, it must reference `output_capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn radelta_compress_lossy_u16_nd(
    input: *const u16,
    input_voxels: usize,
    nx: u32,
    ny: u32,
    nz: u32,
    nc: u32,
    nt: u32,
    options: *const LossyOptions,
    output: *mut u8,
    output_capacity: usize,
    output_size: *mut usize,
) -> i32 {
    if input.is_null() || output_size.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let dims = Dims5 {
        x: nx as usize,
        y: ny as usize,
        z: nz as usize,
        c: nc as usize,
        t: nt as usize,
    };
    let expected = match dims.voxels() {
        Ok(v) => v,
        Err(_) => return RADELTA_INVALID_ARGUMENT,
    };
    if expected != input_voxels {
        return RADELTA_INVALID_ARGUMENT;
    }
    let opts = if options.is_null() {
        LossyOptions::default()
    } else {
        *options
    };
    let src = slice::from_raw_parts(input, input_voxels);
    let encoded = match compress_lossy_u16_nd(src, dims, opts) {
        Ok(v) => v,
        Err(_) => return RADELTA_CODEC_ERROR,
    };
    *output_size = encoded.len();
    if output.is_null() || output_capacity < encoded.len() {
        return RADELTA_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
    RADELTA_OK
}

/// Decode RDL1/RDLQ/RDM2/RDQ2 data to canonical TCZYX through the C ABI.
///
/// # Safety
/// `input` must reference `input_size` readable bytes and `output_voxels` must be
/// writable. Non-null dimension pointers must be writable. If `output` is
/// non-null, it must reference `output_capacity_voxels` writable `u16` values.
#[no_mangle]
pub unsafe extern "C" fn radelta_decompress_u16_nd(
    input: *const u8,
    input_size: usize,
    output: *mut u16,
    output_capacity_voxels: usize,
    output_voxels: *mut usize,
    nx: *mut u32,
    ny: *mut u32,
    nz: *mut u32,
    nc: *mut u32,
    nt: *mut u32,
) -> i32 {
    if input.is_null() || output_voxels.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let src = slice::from_raw_parts(input, input_size);
    let dims = match inspect_shape(src) {
        Ok(v) => v,
        Err(_) => return RADELTA_CODEC_ERROR,
    };
    let needed = match dims.voxels() {
        Ok(v) => v,
        Err(_) => return RADELTA_CODEC_ERROR,
    };
    *output_voxels = needed;
    if !nx.is_null() {
        *nx = dims.x as u32;
    }
    if !ny.is_null() {
        *ny = dims.y as u32;
    }
    if !nz.is_null() {
        *nz = dims.z as u32;
    }
    if !nc.is_null() {
        *nc = dims.c as u32;
    }
    if !nt.is_null() {
        *nt = dims.t as u32;
    }
    if output.is_null() || output_capacity_voxels < needed {
        return RADELTA_BUFFER_TOO_SMALL;
    }
    let dst = slice::from_raw_parts_mut(output, needed);
    match decompress_u16_nd_into(src, dst) {
        Ok(decoded_dims) if decoded_dims == dims => RADELTA_OK,
        Ok(_) | Err(_) => RADELTA_CODEC_ERROR,
    }
}

/// Configure the process-wide metadata byte limit before starting I/O.
/// Zero allows only empty metadata. The default is 1024 MiB.
#[no_mangle]
pub extern "C" fn radelta_set_metadata_limit(max_bytes: usize) {
    crate::metadata::set_metadata_limit(max_bytes);
}

/// Return the process-wide metadata byte limit.
#[no_mangle]
pub extern "C" fn radelta_get_metadata_limit() -> usize {
    crate::metadata::metadata_limit()
}

/// Extract opaque metadata. NULL output queries the required byte count.
/// # Safety
/// Input must contain input_size readable bytes; output_size must be writable.
/// Non-NULL output must reference capacity writable bytes, disjoint from input.
#[no_mangle]
pub unsafe extern "C" fn radelta_get_metadata(
    input: *const u8,
    input_size: usize,
    output: *mut u8,
    capacity: usize,
    output_size: *mut usize,
) -> i32 {
    if input.is_null() || output_size.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    match crate::read_metadata(slice::from_raw_parts(input, input_size)) {
        Ok(data) => {
            *output_size = data.len();
            if data.is_empty() {
                return RADELTA_OK;
            }
            if output.is_null() || capacity < data.len() {
                return RADELTA_BUFFER_TOO_SMALL;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), output, data.len());
            RADELTA_OK
        }
        Err(_) => RADELTA_CODEC_ERROR,
    }
}
/// Return an encoded image with its metadata attached/replaced; zero length removes it.
/// # Safety
/// Input and metadata must reference their indicated readable byte counts;
/// metadata may be NULL only for zero length. output_size must be writable.
/// Non-NULL output must reference capacity writable bytes, disjoint from input.
#[no_mangle]
pub unsafe extern "C" fn radelta_set_metadata(
    input: *const u8,
    input_size: usize,
    metadata: *const u8,
    metadata_size: usize,
    output: *mut u8,
    capacity: usize,
    output_size: *mut usize,
) -> i32 {
    if input.is_null()
        || output_size.is_null()
        || (metadata.is_null() && metadata_size != 0)
        || metadata_size > crate::metadata::metadata_limit()
    {
        return RADELTA_INVALID_ARGUMENT;
    }
    let data = if metadata_size == 0 {
        &[]
    } else {
        slice::from_raw_parts(metadata, metadata_size)
    };
    let mut encoded = slice::from_raw_parts(input, input_size).to_vec();
    if crate::set_metadata(&mut encoded, data).is_err() {
        return RADELTA_CODEC_ERROR;
    }
    *output_size = encoded.len();
    if output.is_null() || capacity < encoded.len() {
        return RADELTA_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
    RADELTA_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_abi_lossless_two_call_pattern_roundtrips() {
        let dims = Dims { x: 7, y: 5, z: 3 };
        let input: Vec<u16> = (0..dims.voxels().unwrap())
            .map(|i| ((i * 97 + i / 5) % 65536) as u16)
            .collect();

        unsafe {
            let mut encoded_size = 0usize;
            let status = radelta_compress_u16(
                input.as_ptr(),
                input.len(),
                dims.x as u32,
                dims.y as u32,
                dims.z as u32,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
                &mut encoded_size,
            );
            assert_eq!(status, RADELTA_BUFFER_TOO_SMALL);
            assert!(encoded_size > 0);

            let mut encoded = vec![0u8; encoded_size];
            let status = radelta_compress_u16(
                input.as_ptr(),
                input.len(),
                dims.x as u32,
                dims.y as u32,
                dims.z as u32,
                std::ptr::null(),
                encoded.as_mut_ptr(),
                encoded.len(),
                &mut encoded_size,
            );
            assert_eq!(status, RADELTA_OK);

            let mut output_voxels = 0usize;
            let mut nx = 0u32;
            let mut ny = 0u32;
            let mut nz = 0u32;
            let status = radelta_decompress_u16(
                encoded.as_ptr(),
                encoded.len(),
                std::ptr::null_mut(),
                0,
                &mut output_voxels,
                &mut nx,
                &mut ny,
                &mut nz,
            );
            assert_eq!(status, RADELTA_BUFFER_TOO_SMALL);
            assert_eq!(output_voxels, input.len());
            assert_eq!((nx, ny, nz), (7, 5, 3));

            let mut decoded = vec![0u16; output_voxels];
            let status = radelta_decompress_u16(
                encoded.as_ptr(),
                encoded.len(),
                decoded.as_mut_ptr(),
                decoded.len(),
                &mut output_voxels,
                &mut nx,
                &mut ny,
                &mut nz,
            );
            assert_eq!(status, RADELTA_OK);
            assert_eq!(decoded, input);
        }
    }

    #[test]
    fn c_abi_multidimensional_lossless_roundtrips() {
        let dims = Dims5 {
            x: 5,
            y: 4,
            z: 3,
            c: 2,
            t: 2,
        };
        let input: Vec<u16> = (0..dims.voxels().unwrap())
            .map(|i| ((i * 193 + i / 7) % 65536) as u16)
            .collect();

        unsafe {
            let mut encoded_size = 0usize;
            let status = radelta_compress_u16_nd(
                input.as_ptr(),
                input.len(),
                dims.x as u32,
                dims.y as u32,
                dims.z as u32,
                dims.c as u32,
                dims.t as u32,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
                &mut encoded_size,
            );
            assert_eq!(status, RADELTA_BUFFER_TOO_SMALL);

            let mut encoded = vec![0u8; encoded_size];
            let status = radelta_compress_u16_nd(
                input.as_ptr(),
                input.len(),
                dims.x as u32,
                dims.y as u32,
                dims.z as u32,
                dims.c as u32,
                dims.t as u32,
                std::ptr::null(),
                encoded.as_mut_ptr(),
                encoded.len(),
                &mut encoded_size,
            );
            assert_eq!(status, RADELTA_OK);

            let mut output_voxels = 0usize;
            let mut nx = 0u32;
            let mut ny = 0u32;
            let mut nz = 0u32;
            let mut nc = 0u32;
            let mut nt = 0u32;
            let status = radelta_decompress_u16_nd(
                encoded.as_ptr(),
                encoded.len(),
                std::ptr::null_mut(),
                0,
                &mut output_voxels,
                &mut nx,
                &mut ny,
                &mut nz,
                &mut nc,
                &mut nt,
            );
            assert_eq!(status, RADELTA_BUFFER_TOO_SMALL);
            assert_eq!(output_voxels, input.len());
            assert_eq!((nx, ny, nz, nc, nt), (5, 4, 3, 2, 2));

            let mut decoded = vec![0u16; output_voxels];
            let status = radelta_decompress_u16_nd(
                encoded.as_ptr(),
                encoded.len(),
                decoded.as_mut_ptr(),
                decoded.len(),
                &mut output_voxels,
                &mut nx,
                &mut ny,
                &mut nz,
                &mut nc,
                &mut nt,
            );
            assert_eq!(status, RADELTA_OK);
            assert_eq!(decoded, input);
        }
    }

    #[test]
    fn c_abi_rejects_mismatched_input_length() {
        let input = [1u16, 2, 3];
        let mut output_size = 0usize;
        let status = unsafe {
            radelta_compress_u16(
                input.as_ptr(),
                input.len(),
                2,
                2,
                1,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
                &mut output_size,
            )
        };
        assert_eq!(status, RADELTA_INVALID_ARGUMENT);
    }
}
