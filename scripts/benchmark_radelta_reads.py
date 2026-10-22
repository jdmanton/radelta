#!/usr/bin/env python3
"""Compare file indexing, small-volume encoding and Python reads after warmup.

Requires numpy. Use release libraries. Python package paths are source roots
containing radelta/. Files in --work-dir are generated benchmark fixtures.
No disk durability is measured. Native opening never decodes the pixel payload.
"""
import argparse
import ctypes as ct
import hashlib
import json
import os
from pathlib import Path
import statistics
import struct
import subprocess
import sys
import time

import numpy as np

U8 = ct.POINTER(ct.c_uint8)


def library(path):
    dll = ct.CDLL(str(Path(path).resolve()))
    dll.radelta_compress_u16_nd_alloc.argtypes = [ct.POINTER(ct.c_uint16), ct.c_size_t,
        *([ct.c_uint32] * 5), ct.c_void_p, ct.c_void_p, ct.POINTER(U8), ct.POINTER(ct.c_size_t)]
    dll.radelta_compress_u16_nd_alloc.restype = ct.c_int32
    dll.radelta_buffer_free.argtypes = [U8, ct.c_size_t]
    dll.radelta_buffer_free.restype = None
    dll.radelta_file_open.argtypes = [ct.c_char_p, ct.POINTER(ct.c_void_p)]
    dll.radelta_file_open.restype = ct.c_int32
    dll.radelta_file_close.argtypes = [ct.c_void_p]
    dll.radelta_file_close.restype = None
    return dll


def encode(dll, image, dims):
    ptr, size = U8(), ct.c_size_t()
    start = time.perf_counter()
    status = dll.radelta_compress_u16_nd_alloc(image.ctypes.data_as(ct.POINTER(ct.c_uint16)),
        image.size, *dims, None, None, ct.byref(ptr), ct.byref(size))
    elapsed = time.perf_counter() - start
    if status:
        raise RuntimeError(f"encoding failed: {status}")
    try:
        return elapsed, ct.string_at(ptr, size.value)
    finally:
        dll.radelta_buffer_free(ptr, size.value)


def open_file(dll, path):
    handle = ct.c_void_p()
    start = time.perf_counter()
    status = dll.radelta_file_open(os.fsencode(path), ct.byref(handle))
    elapsed = time.perf_counter() - start
    if status:
        raise RuntimeError(f"opening failed: {status}")
    dll.radelta_file_close(handle)
    return elapsed


def python_worker(library_path, package, path, repeats):
    os.environ["RADELTA_NATIVE_LIBRARY"] = str(Path(library_path).resolve())
    sys.path.insert(0, str(Path(package).resolve()))
    import radelta
    encoded = Path(path).read_bytes()
    results = {}
    with radelta.open(path) as source:
        for name, action in [("Python decompress(bytes)", lambda: radelta.decompress(encoded)),
                             ("Python cached full-stack read", lambda: source[...])]:
            times = []
            for repeat in range(repeats + 1):
                start = time.perf_counter()
                result = action()
                elapsed = time.perf_counter() - start
                if repeat:
                    times.append(elapsed)
            results[name] = dict(seconds=statistics.median(times), sha256=hashlib.sha256(result).hexdigest())
    print(json.dumps(results))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--baseline-python", type=Path)
    parser.add_argument("--candidate-python", type=Path)
    parser.add_argument("--repeats", type=int, default=7)
    parser.add_argument("--records", type=int, default=100_000)
    args = parser.parse_args()
    if args.repeats < 1 or not 1 <= args.records <= 0xffffffff:
        parser.error("positive repeats and a positive uint32 record count are required")
    if bool(args.baseline_python) != bool(args.candidate_python):
        parser.error("provide both Python package paths, or neither")
    args.work_dir.mkdir(parents=True, exist_ok=True)
    dlls = [library(args.baseline), library(args.candidate)]
    _, tiny = encode(dlls[0], np.array([25], dtype=np.uint16), (1, 1, 1, 1, 1))
    core = tiny[40:40 + struct.unpack_from("<Q", tiny, 32)[0]]
    nd = args.work_dir / "many-volumes.rdlt"
    nd.write_bytes(struct.pack("<4sHH6I", b"RDM2", 1, 0, 1, 1, 1, 1, args.records, args.records)
                   + (struct.pack("<Q", len(core)) + core) * args.records)
    stream = args.work_dir / "many-chunks.rdlt"
    with stream.open("wb") as f:
        f.write(struct.pack("<4sHH6IQ", b"RDS3", 1, 0, 1, 1, args.records, 1, 1, 1, args.records))
        for z in range(args.records):
            f.write(struct.pack("<4IQ", 0, 0, z, 1, len(core)))
            f.write(core)
    print("| Workload | Before (ms) | After (ms) | Speedup |", flush=True)
    print("|---|---:|---:|---:|", flush=True)

    def row(name, results):
        before, after = results
        print(f"| {name} | {before * 1000:.4f} | {after * 1000:.4f} | {before / after:.2f}× |", flush=True)

    for name, path in [(f"Open {args.records:,} volumes", nd), (f"Open {args.records:,} chunks", stream)]:
        times = [[], []]
        for repeat in range(args.repeats + 1):
            for i in ([0, 1] if repeat % 2 == 0 else [1, 0]):
                elapsed = open_file(dlls[i], path)
                if repeat:
                    times[i].append(elapsed)
        row(name, list(map(statistics.median, times)))

    image = np.random.default_rng(41).poisson(80, (128, 32, 32)).astype(np.uint16)
    times = [[], []]
    hashes = [None, None]
    for repeat in range(args.repeats + 1):
        for i in ([0, 1] if repeat % 2 == 0 else [1, 0]):
            elapsed, result = encode(dlls[i], image, (32, 32, 1, 1, 128))
            if repeat:
                times[i].append(elapsed)
            hashes[i] = hashlib.sha256(result).digest()
    assert hashes[0] == hashes[1]
    row("Encode 128 independent 32×32 planes", list(map(statistics.median, times)))

    if args.baseline_python:
        image = np.random.default_rng(42).integers(0, 65536, (16, 2048, 2048), dtype=np.uint16)
        expected = hashlib.sha256(image).hexdigest()
        _, encoded = encode(dlls[0], image, (2048, 2048, 16, 1, 1))
        path = args.work_dir / "large.rdlt"
        path.write_bytes(encoded)
        results = []
        for lib, package in [(args.baseline, args.baseline_python), (args.candidate, args.candidate_python)]:
            run = subprocess.run([sys.executable, str(Path(__file__).resolve()), "--python-worker",
                                  str(lib.resolve()), str(package.resolve()), str(path.resolve()), str(args.repeats)],
                                 capture_output=True, text=True, check=True)
            results.append(json.loads(run.stdout))
        for name in results[0]:
            assert results[0][name]["sha256"] == results[1][name]["sha256"] == expected
            row(name, [r[name]["seconds"] for r in results])


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--python-worker":
        python_worker(*sys.argv[2:5], int(sys.argv[5]))
    else:
        main()
