#!/usr/bin/env python3
"""Compare first-pass channel-interleaved and sequential reads via the C API.

Requires numpy; optional CLI export checks also require tifffile. Fixtures are
two 32x256x256 uint16 channels (8 MiB total), so their active decoded data fits
the native default cache (currently 2 GiB per handle). Timings include initial decoding and plane copies,
but exclude opening, buffer allocation and validation. No fsync is measured.
"""
import argparse
import ctypes as ct
import hashlib
import os
from pathlib import Path
import statistics
import subprocess
import time

import numpy as np

U8, U16, HANDLE = ct.POINTER(ct.c_uint8), ct.POINTER(ct.c_uint16), ct.c_void_p


def library(path):
    dll = ct.CDLL(str(path.resolve()))
    signatures = {
        "radelta_compress_u16_nd_alloc": [U16, ct.c_size_t, *([ct.c_uint32]*5), ct.c_void_p, ct.c_void_p, ct.POINTER(U8), ct.POINTER(ct.c_size_t)],
        "radelta_buffer_free": [U8, ct.c_size_t],
        "radelta_file_open": [ct.c_char_p, ct.POINTER(HANDLE)],
        "radelta_file_close": [HANDLE],
        "radelta_file_read_plane_u16": [HANDLE, *([ct.c_uint32]*3), U16, ct.c_uint64],
        "radelta_writer_create_lossless_u16": [ct.c_char_p, *([ct.c_uint32]*6), ct.POINTER(HANDLE)],
        "radelta_writer_write_plane_u16": [HANDLE, *([ct.c_uint32]*3), U16, ct.c_uint64],
        "radelta_writer_finish": [HANDLE],
        "radelta_writer_close": [HANDLE],
    }
    for name, args in signatures.items():
        fn = getattr(dll, name)
        fn.argtypes = args
        fn.restype = None if name.endswith("close") or name == "radelta_buffer_free" else ct.c_int32
    if hasattr(dll, "radelta_file_set_cache_limit"):
        dll.radelta_file_set_cache_limit.argtypes = [HANDLE, ct.c_uint64]
        dll.radelta_file_set_cache_limit.restype = ct.c_int32
    return dll


def pointer(array):
    return array.ctypes.data_as(U16)


def fixtures(dll, root, image):
    nd = root / "channels-nd.rdlt"
    ptr, size = U8(), ct.c_size_t()
    c, z, y, x = image.shape
    assert dll.radelta_compress_u16_nd_alloc(pointer(image), image.size, x, y, z, c, 1,
        None, None, ct.byref(ptr), ct.byref(size)) == 0
    try:
        nd.write_bytes(ct.string_at(ptr, size.value))
    finally:
        dll.radelta_buffer_free(ptr, size.value)
    stream = root / "channels-stream.rdlt"
    handle = HANDLE()
    assert dll.radelta_writer_create_lossless_u16(os.fsencode(stream), x, y, z, c, 1, 8, ct.byref(handle)) == 0
    try:
        for channel in range(c):
            for plane in range(z):
                data = image[channel, plane]
                assert dll.radelta_writer_write_plane_u16(handle, 0, channel, plane, pointer(data), data.size) == 0
        assert dll.radelta_writer_finish(handle) == 0
    finally:
        dll.radelta_writer_close(handle)
    return nd, stream


def read(dll, path, image, interleaved, legacy=False):
    handle = HANDLE()
    assert dll.radelta_file_open(os.fsencode(path), ct.byref(handle)) == 0
    try:
        if legacy:
            assert dll.radelta_file_set_cache_limit(handle, 0) == 0
        output = np.empty_like(image)
        c, z = image.shape[:2]
        order = [(channel, plane) for plane in range(z) for channel in range(c)] if interleaved else [
            (channel, plane) for channel in range(c) for plane in range(z)]
        start = time.perf_counter()
        for channel, plane in order:
            target = output[channel, plane]
            assert dll.radelta_file_read_plane_u16(handle, 0, channel, plane, pointer(target), target.size) == 0
        elapsed = time.perf_counter() - start
        np.testing.assert_array_equal(output, image)
        return elapsed
    finally:
        dll.radelta_file_close(handle)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--baseline-cli", type=Path)
    parser.add_argument("--candidate-cli", type=Path)
    args = parser.parse_args()
    if args.repeats < 1 or bool(args.baseline_cli) != bool(args.candidate_cli):
        parser.error("positive repeats and either both CLI paths or neither are required")
    args.work_dir.mkdir(parents=True, exist_ok=True)
    dlls = [library(args.baseline), library(args.candidate)]
    image = np.random.default_rng(41).poisson(80, (2, 32, 256, 256)).astype(np.uint16)
    paths = fixtures(dlls[0], args.work_dir, image)
    original_hashes = [hashlib.sha256(p.read_bytes()).digest() for p in paths]
    print("| Workload | Before (ms) | After (ms) | Speedup |", flush=True)
    print("|---|---:|---:|---:|", flush=True)
    for path in paths:
        for interleaved in (False, True):
            times = [[], []]
            for repeat in range(args.repeats + 1):
                for i in ([0, 1] if repeat % 2 == 0 else [1, 0]):
                    elapsed = read(dlls[i], path, image, interleaved)
                    if repeat:
                        times[i].append(elapsed)
            old, new = map(statistics.median, times)
            label = path.stem + (" interleaved" if interleaved else " sequential")
            print(f"| {label} | {old*1000:.3f} | {new*1000:.3f} | {old/new:.2f}× |", flush=True)
        # Check the new opt-out path against the same independent pixel truth.
        read(dlls[1], path, image, True, legacy=True)
    if args.baseline_cli:
        import tifffile
        for path in paths:
            for page_order in ("TCZ", "TZC"):
                expected = (image if page_order == "TCZ" else image.transpose(1, 0, 2, 3)).reshape(-1, 256, 256)
                times, outputs = [[], []], [args.work_dir / "before.tif", args.work_dir / "after.tif"]
                for repeat in range(args.repeats + 1):
                    for i in ([0, 1] if repeat % 2 == 0 else [1, 0]):
                        binary = [args.baseline_cli, args.candidate_cli][i]
                        start = time.perf_counter()
                        run = subprocess.run([str(binary.resolve()), "decode", str(path.resolve()),
                            str(outputs[i].resolve()), "--page-order", page_order], capture_output=True, text=True)
                        if run.returncode:
                            raise RuntimeError(run.stderr)
                        if repeat:
                            times[i].append(time.perf_counter() - start)
                for output in outputs:
                    with tifffile.TiffFile(output) as f:
                        np.testing.assert_array_equal(np.stack([p.asarray() for p in f.pages]), expected)
                assert hashlib.sha256(outputs[0].read_bytes()).digest() == hashlib.sha256(outputs[1].read_bytes()).digest()
                old, new = map(statistics.median, times)
                print(f"| CLI {path.stem} → {page_order} TIFF | {old*1000:.3f} | {new*1000:.3f} | {old/new:.2f}× |", flush=True)
    assert original_hashes == [hashlib.sha256(p.read_bytes()).digest() for p in paths]


if __name__ == "__main__":
    main()
