//! File and streaming-writer C ABI, pointer validation and thread-local errors.

use super::index::{dims5, open_impl};
use super::writer::{RadeltaStreamWriter, WriterMode};
use super::{RadeltaFileHandle, RadeltaFileInfo};
use crate::{LossyOptions, Options, RADELTA_CODEC_ERROR, RADELTA_INVALID_ARGUMENT, RADELTA_OK};
use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::PathBuf;
use std::ptr;

const RADELTA_IO_ERROR: i32 = -3;

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_last_error(msg: impl Into<String>) {
    LAST_ERROR.with(|s| *s.borrow_mut() = msg.into());
}

pub struct RadeltaWriterHandle {
    writer: RadeltaStreamWriter,
}

/// Open a Radelta file for random plane access.
///
/// # Safety
/// `path_utf8` must point to a NUL-terminated UTF-8 string and `out_handle` must
/// be writable. The returned handle must eventually be passed to
/// [`radelta_file_close`].
#[no_mangle]
pub unsafe extern "C" fn radelta_file_open(
    path_utf8: *const c_char,
    out_handle: *mut *mut RadeltaFileHandle,
) -> i32 {
    if path_utf8.is_null() || out_handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    *out_handle = ptr::null_mut();
    let path = match CStr::from_ptr(path_utf8).to_str() {
        Ok(s) => PathBuf::from(s),
        Err(_) => {
            set_last_error("Radelta path is not valid UTF-8");
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    match std::panic::catch_unwind(|| open_impl(path)) {
        Ok(Ok(handle)) => {
            *out_handle = Box::into_raw(Box::new(handle));
            RADELTA_OK
        }
        Ok(Err(e)) => {
            set_last_error(e);
            RADELTA_IO_ERROR
        }
        Err(_) => {
            set_last_error("unexpected panic while opening Radelta file");
            RADELTA_CODEC_ERROR
        }
    }
}

/// Close a file handle returned by [`radelta_file_open`].
///
/// # Safety
/// `handle` must be null or a live handle returned by `radelta_file_open`, and
/// it must not be used again after this call.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_close(handle: *mut RadeltaFileHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// Query shape, format and flags for an open Radelta file.
///
/// # Safety
/// `handle` must be a live file handle and `out_info` must be writable.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_get_info(
    handle: *const RadeltaFileHandle,
    out_info: *mut RadeltaFileInfo,
) -> i32 {
    if handle.is_null() || out_info.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    *out_info = (*handle).info();
    RADELTA_OK
}

/// Set the decoded retention budget. Zero selects a single-entry cache.
///
/// # Safety
/// `handle` must be a live file handle returned by `radelta_file_open`.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_set_cache_limit(
    handle: *const RadeltaFileHandle,
    max_bytes: u64,
) -> i32 {
    if handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let Ok(bytes) = usize::try_from(max_bytes) else {
        return RADELTA_INVALID_ARGUMENT;
    };
    match (&*handle).set_cache_limit(bytes) {
        Ok(()) => RADELTA_OK,
        Err(e) => {
            set_last_error(e);
            RADELTA_CODEC_ERROR
        }
    }
}

/// Read one TCZ plane from an open Radelta file.
///
/// # Safety
/// `handle` must be live. `output` must reference at least
/// `output_capacity_voxels` writable `u16` values.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_read_plane_u16(
    handle: *const RadeltaFileHandle,
    t: u32,
    c: u32,
    z: u32,
    output: *mut u16,
    output_capacity_voxels: u64,
) -> i32 {
    if handle.is_null() || output.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let h = &*handle;
    let plane = match h.dims.x.checked_mul(h.dims.y) {
        Some(v) => v,
        None => {
            set_last_error("plane size overflow");
            return RADELTA_CODEC_ERROR;
        }
    };
    if output_capacity_voxels < plane as u64 {
        return RADELTA_INVALID_ARGUMENT;
    }
    let out = std::slice::from_raw_parts_mut(output, plane);
    match h.read_plane(t as usize, c as usize, z as usize, out) {
        Ok(()) => RADELTA_OK,
        Err(e) => {
            set_last_error(e);
            RADELTA_CODEC_ERROR
        }
    }
}

/// Read TCZ triples into consecutive output planes, in request order.
/// Invalid coordinates/capacity are rejected before writing output. Decode/I/O
/// errors may leave partially written output. An empty request is a no-op.
///
/// # Safety
/// `handle` must be live. For nonzero `count`, `tcz` references `3 * count`
/// readable `u32` values and `output` at least `output_capacity_voxels` writable
/// `u16` values. These buffers must not overlap each other or the handle.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_read_planes_u16(
    handle: *const RadeltaFileHandle,
    count: u64,
    tcz: *const u32,
    output: *mut u16,
    output_capacity_voxels: u64,
) -> i32 {
    if handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    if count == 0 {
        return RADELTA_OK;
    }
    let h = &*handle;
    let sizes = usize::try_from(count).ok().and_then(|count| {
        let coords = count.checked_mul(3)?;
        let voxels = h.dims.x.checked_mul(h.dims.y)?.checked_mul(count)?;
        (coords <= isize::MAX as usize / std::mem::size_of::<u32>()
            && voxels <= isize::MAX as usize / std::mem::size_of::<u16>())
        .then_some((count, coords, voxels))
    });
    let Some((count, coords, voxels)) = sizes else {
        set_last_error("batch size exceeds platform limits");
        return RADELTA_INVALID_ARGUMENT;
    };
    if tcz.is_null() || output.is_null() || output_capacity_voxels < voxels as u64 {
        set_last_error("invalid batch buffers or output capacity");
        return RADELTA_INVALID_ARGUMENT;
    }
    let mut planes = Vec::new();
    if planes.try_reserve_exact(count).is_err() {
        set_last_error("cannot allocate plane coordinates");
        return RADELTA_CODEC_ERROR;
    }
    for &[t, c, z] in std::slice::from_raw_parts(tcz, coords).as_chunks::<3>().0 {
        if t as usize >= h.dims.t || c as usize >= h.dims.c || z as usize >= h.dims.z {
            set_last_error("requested T/C/Z plane is out of range");
            return RADELTA_INVALID_ARGUMENT;
        }
        planes.push((t as usize, c as usize, z as usize));
    }
    let out = std::slice::from_raw_parts_mut(output, voxels);
    match h.read_planes(&planes, out) {
        Ok(()) => RADELTA_OK,
        Err(e) => {
            set_last_error(e);
            RADELTA_CODEC_ERROR
        }
    }
}

/// Create a streaming lossless RDS3 writer.
///
/// # Safety
/// `path_utf8` must point to a NUL-terminated UTF-8 string and `out_handle` must
/// be writable. The returned handle must eventually be closed.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_create_lossless_u16(
    path_utf8: *const c_char,
    nx: u32,
    ny: u32,
    nz: u32,
    nc: u32,
    nt: u32,
    memory_mib: u32,
    out_handle: *mut *mut RadeltaWriterHandle,
) -> i32 {
    if path_utf8.is_null() || out_handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    *out_handle = ptr::null_mut();
    let path = match CStr::from_ptr(path_utf8).to_str() {
        Ok(s) => PathBuf::from(s),
        Err(_) => {
            set_last_error("Radelta path is not valid UTF-8");
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    let dims = match dims5(nx, ny, nz, nc, nt) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(e);
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    match RadeltaStreamWriter::new(
        path,
        dims,
        WriterMode::Lossless(Options::default()),
        memory_mib as usize,
    ) {
        Ok(writer) => {
            *out_handle = Box::into_raw(Box::new(RadeltaWriterHandle { writer }));
            RADELTA_OK
        }
        Err(e) => {
            set_last_error(e);
            RADELTA_IO_ERROR
        }
    }
}

/// Create a streaming calibrated-lossy RQS3 writer.
///
/// # Safety
/// `path_utf8` must point to a NUL-terminated UTF-8 string and `out_handle` must
/// be writable. The returned handle must eventually be closed.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_create_lossy_u16(
    path_utf8: *const c_char,
    nx: u32,
    ny: u32,
    nz: u32,
    nc: u32,
    nt: u32,
    offset_adu: f64,
    gain_e_per_adu: f64,
    noise_step: f64,
    memory_mib: u32,
    out_handle: *mut *mut RadeltaWriterHandle,
) -> i32 {
    if path_utf8.is_null() || out_handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    *out_handle = ptr::null_mut();
    let path = match CStr::from_ptr(path_utf8).to_str() {
        Ok(s) => PathBuf::from(s),
        Err(_) => {
            set_last_error("Radelta path is not valid UTF-8");
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    let dims = match dims5(nx, ny, nz, nc, nt) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(e);
            return RADELTA_INVALID_ARGUMENT;
        }
    };
    let opts = LossyOptions {
        offset_adu,
        gain_e_per_adu,
        noise_step,
        ..LossyOptions::default()
    };
    if let Err(e) = opts.validate_calibration() {
        set_last_error(e.to_string());
        return RADELTA_INVALID_ARGUMENT;
    }
    match RadeltaStreamWriter::new(path, dims, WriterMode::Lossy(opts), memory_mib as usize) {
        Ok(writer) => {
            *out_handle = Box::into_raw(Box::new(RadeltaWriterHandle { writer }));
            RADELTA_OK
        }
        Err(e) => {
            set_last_error(e);
            RADELTA_IO_ERROR
        }
    }
}

/// Append the next plane to a streaming writer in increasing T,C,Z order.
///
/// # Safety
/// `handle` must be a live writer handle. `input` must reference exactly
/// `input_voxels` readable `u16` values.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_write_plane_u16(
    handle: *mut RadeltaWriterHandle,
    t: u32,
    c: u32,
    z: u32,
    input: *const u16,
    input_voxels: u64,
) -> i32 {
    if handle.is_null() || input.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let h = &mut *handle;
    if input_voxels != h.writer.plane_voxels as u64 {
        set_last_error(format!(
            "input plane contains {} voxels, but {} were expected",
            input_voxels, h.writer.plane_voxels
        ));
        return RADELTA_INVALID_ARGUMENT;
    }
    let plane = std::slice::from_raw_parts(input, h.writer.plane_voxels);
    match h
        .writer
        .write_plane(t as usize, c as usize, z as usize, plane)
    {
        Ok(()) => RADELTA_OK,
        Err(e) => {
            set_last_error(e);
            RADELTA_CODEC_ERROR
        }
    }
}

/// Finish a streaming writer and flush its final chunk.
///
/// # Safety
/// `handle` must be a live writer handle returned by a writer-create function.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_finish(handle: *mut RadeltaWriterHandle) -> i32 {
    if handle.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let h = &mut *handle;
    match h.writer.finish() {
        Ok(()) => RADELTA_OK,
        Err(e) => {
            set_last_error(e);
            RADELTA_CODEC_ERROR
        }
    }
}

/// Close a streaming writer handle.
///
/// # Safety
/// `handle` must be null or a live writer handle, and it must not be used after
/// this call. Closing does not implicitly report an unfinished-stream error.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_close(handle: *mut RadeltaWriterHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// Copy the calling thread's latest file-API error message.
///
/// # Safety
/// If `buffer` is non-null, it must reference at least `capacity` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_last_error(buffer: *mut c_char, capacity: u64) -> u64 {
    let msg = LAST_ERROR.with(|s| s.borrow().clone());
    let needed = msg.len() as u64 + 1;
    if buffer.is_null() || capacity == 0 {
        return needed;
    }
    let cap = usize::try_from(capacity).unwrap_or(usize::MAX);
    let max_copy = cap.saturating_sub(1).min(msg.len());
    ptr::copy_nonoverlapping(msg.as_ptr(), buffer as *mut u8, max_copy);
    *(buffer as *mut u8).add(max_copy) = 0;
    needed
}

/// Read the opaque metadata associated with a file. NULL output queries size.
/// # Safety
/// Handle and output_size must be valid; non-NULL output must have capacity bytes.
#[no_mangle]
pub unsafe extern "C" fn radelta_file_get_metadata(
    handle: *mut RadeltaFileHandle,
    output: *mut u8,
    capacity: usize,
    output_size: *mut usize,
) -> i32 {
    if handle.is_null() || output_size.is_null() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let data = &(*handle).metadata;
    *output_size = data.len();
    if data.is_empty() {
        return RADELTA_OK;
    }
    if output.is_null() || capacity < data.len() {
        return crate::RADELTA_BUFFER_TOO_SMALL;
    }
    ptr::copy_nonoverlapping(data.as_ptr(), output, data.len());
    RADELTA_OK
}

/// Set or replace opaque writer metadata before finish; zero length clears it.
/// # Safety
/// Handle must be valid, and data must point to len readable bytes unless len is zero.
#[no_mangle]
pub unsafe extern "C" fn radelta_writer_set_metadata(
    handle: *mut RadeltaWriterHandle,
    data: *const u8,
    len: usize,
) -> i32 {
    if handle.is_null() || (data.is_null() && len != 0) || len > crate::metadata::metadata_limit() {
        return RADELTA_INVALID_ARGUMENT;
    }
    let writer = &mut (*handle).writer;
    if writer.finished {
        set_last_error("writer is already finished");
        return RADELTA_INVALID_ARGUMENT;
    }
    writer.metadata = if len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data, len).to_vec()
    };
    RADELTA_OK
}

#[cfg(test)]
mod tests {
    use super::super::index::open_impl;
    use super::super::test_support::temp_path;
    use super::super::writer::{RadeltaStreamWriter, WriterMode};
    use super::super::RadeltaFileHandle;
    use super::*;
    use crate::{Dims, Dims5, LossyOptions, Options, RADELTA_INVALID_ARGUMENT, RADELTA_OK};
    use std::{fs, ptr};

    #[test]
    fn streaming_metadata_c_api_roundtrip_and_replacement() {
        let path = temp_path("stream-metadata");
        let dims = Dims5 {
            x: 3,
            y: 2,
            z: 3,
            c: 1,
            t: 1,
        };
        let metadata = b"arbitrary binary metadata\0\xff".repeat(1000);
        for mode in [
            WriterMode::Lossless(Options::default()),
            WriterMode::Lossy(LossyOptions::default()),
        ] {
            let mut handle = RadeltaWriterHandle {
                writer: RadeltaStreamWriter::new(path.clone(), dims, mode, 1).unwrap(),
            };
            unsafe {
                assert_eq!(
                    radelta_writer_set_metadata(&mut handle, metadata.as_ptr(), metadata.len()),
                    RADELTA_OK
                );
            }
            for z in 0..dims.z {
                handle.writer.write_plane(0, 0, z, &[0; 6]).unwrap();
            }
            handle.writer.finish().unwrap();
            let length = fs::metadata(&path).unwrap().len();
            handle.writer.finish().unwrap();
            assert_eq!(fs::metadata(&path).unwrap().len(), length);
            unsafe {
                assert_eq!(
                    radelta_writer_set_metadata(&mut handle, std::ptr::null(), 0),
                    RADELTA_INVALID_ARGUMENT
                );
            }
            drop(handle);
            let mut file = open_impl(path.clone()).unwrap();
            let mut size = 0;
            unsafe {
                assert_eq!(
                    radelta_file_get_metadata(&mut file, std::ptr::null_mut(), 0, &mut size),
                    crate::RADELTA_BUFFER_TOO_SMALL
                );
            }
            let mut actual = vec![0; size];
            unsafe {
                assert_eq!(
                    radelta_file_get_metadata(
                        &mut file,
                        actual.as_mut_ptr(),
                        actual.len(),
                        &mut size
                    ),
                    RADELTA_OK
                );
            }
            assert_eq!(actual, metadata);
            let mut plane = [1; 6];
            file.read_plane(0, 0, 2, &mut plane).unwrap();
            assert_eq!(plane, [0; 6]);
            drop(file);
            crate::set_file_metadata(&path, b"replacement").unwrap();
            assert_eq!(crate::read_file_metadata(&path).unwrap(), b"replacement");
            crate::set_file_metadata(&path, &[]).unwrap();
            assert!(crate::read_file_metadata(&path).unwrap().is_empty());
            let file = open_impl(path.clone()).unwrap();
            file.read_plane(0, 0, 0, &mut plane).unwrap();
            assert_eq!(plane, [0; 6]);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn batch_c_api_rejects_invalid_sizes_and_coordinates_without_writing() {
        let path = temp_path("batch-ffi");
        fs::write(
            &path,
            crate::compress_u16(&[3, 4], Dims { x: 2, y: 1, z: 1 }, Options::default()).unwrap(),
        )
        .unwrap();
        let handle = RadeltaFileHandle::open(&path).unwrap();
        let coords = [0, 0, 0, 0, 0, 1];
        let mut output = [19u16; 4];
        unsafe {
            assert_eq!(
                radelta_file_read_planes_u16(&handle, 0, ptr::null(), ptr::null_mut(), 0),
                RADELTA_OK
            );
            for (count, input, capacity) in [
                (u64::MAX, coords.as_ptr(), u64::MAX),
                (2, coords.as_ptr(), 3),
                (2, coords.as_ptr(), 4),
                (1, ptr::null(), 4),
            ] {
                assert_eq!(
                    radelta_file_read_planes_u16(
                        &handle,
                        count,
                        input,
                        output.as_mut_ptr(),
                        capacity
                    ),
                    RADELTA_INVALID_ARGUMENT
                );
                assert_eq!(output, [19; 4]);
            }
            assert_eq!(
                radelta_file_read_planes_u16(&handle, 1, coords.as_ptr(), output.as_mut_ptr(), 4),
                RADELTA_OK
            );
            assert_eq!(output, [3, 4, 19, 19]);
        }
        drop(handle);
        fs::remove_file(path).unwrap();
    }
}
