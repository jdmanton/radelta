#!/usr/bin/env python3
"""Compare metadata attachment in two native libraries, without codec timing.

Requires numpy. Uses a real, incompressible 128 MiB Radelta image. Timings
include the two C calls (size query + output) but exclude allocating the caller's
output buffer, which is reused. Also reports the size query alone. The script
checks exact output bytes; this is a memory/serialization benchmark, not disk I/O.
"""
import argparse
import ctypes as ct
import hashlib
from pathlib import Path
import statistics
import time

import numpy as np

U8 = ct.POINTER(ct.c_uint8)
SIZE = ct.POINTER(ct.c_size_t)


def library(path):
    dll = ct.CDLL(str(path.resolve()))
    dll.radelta_compress_u16_nd_alloc.argtypes = [
        ct.POINTER(ct.c_uint16), ct.c_size_t, *([ct.c_uint32] * 5),
        ct.c_void_p, ct.c_void_p, ct.POINTER(U8), SIZE,
    ]
    dll.radelta_compress_u16_nd_alloc.restype = ct.c_int32
    dll.radelta_buffer_free.argtypes = [U8, ct.c_size_t]
    dll.radelta_buffer_free.restype = None
    dll.radelta_set_metadata.argtypes = [U8, ct.c_size_t, U8, ct.c_size_t, U8, ct.c_size_t, SIZE]
    dll.radelta_set_metadata.restype = ct.c_int32
    return dll


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--repeats", type=int, default=7)
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    dlls = [library(args.baseline), library(args.candidate)]
    source = np.random.default_rng(41).integers(0, 65536, (16, 2048, 2048), dtype=np.uint16)
    encoded, encoded_size = U8(), ct.c_size_t()
    status = dlls[0].radelta_compress_u16_nd_alloc(
        source.ctypes.data_as(ct.POINTER(ct.c_uint16)), source.size,
        2048, 2048, 16, 1, 1, None, None, ct.byref(encoded), ct.byref(encoded_size),
    )
    if status:
        raise RuntimeError(f"compression failed: {status}")
    try:
        metadata = (ct.c_uint8 * 4096)(*np.random.default_rng(42).integers(0, 256, 4096, dtype=np.uint8))
        required = ct.c_size_t()
        assert dlls[0].radelta_set_metadata(encoded, encoded_size.value, metadata, len(metadata), None, 0, ct.byref(required)) == 1
        outputs = [(ct.c_uint8 * required.value)() for _ in dlls]
        timings = [{"Size query": [], "Size query + output": []} for _ in dlls]
        for repeat in range(args.repeats + 1):
            for i in ([0, 1] if repeat % 2 == 0 else [1, 0]):
                fn = dlls[i].radelta_set_metadata
                start = time.perf_counter()
                assert fn(encoded, encoded_size.value, metadata, len(metadata), None, 0, ct.byref(required)) == 1
                query = time.perf_counter() - start
                start = time.perf_counter()
                assert fn(encoded, encoded_size.value, metadata, len(metadata), None, 0, ct.byref(required)) == 1
                assert fn(encoded, encoded_size.value, metadata, len(metadata), outputs[i], len(outputs[i]), ct.byref(required)) == 0
                both = time.perf_counter() - start
                if repeat:
                    timings[i]["Size query"].append(query)
                    timings[i]["Size query + output"].append(both)
        assert hashlib.sha256(outputs[0]).digest() == hashlib.sha256(outputs[1]).digest()
        print(f"Encoded input: {encoded_size.value / 2**20:.3f} MiB; metadata: {len(metadata)} bytes")
        print("| Measurement | Before (ms) | After (ms) | Speedup |")
        print("|---|---:|---:|---:|")
        for key in timings[0]:
            old, new = [statistics.median(t[key]) for t in timings]
            print(f"| {key} | {old * 1000:.4f} | {new * 1000:.4f} | {old / new:.2f}× |")
    finally:
        dlls[0].radelta_buffer_free(encoded, encoded_size.value)


if __name__ == "__main__":
    main()
