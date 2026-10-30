#!/usr/bin/env python3
"""Compare planar TIFF ingestion and Zarr chunk framing after four I/O patches.

Requires numpy and tifffile. Use release binaries. Native codec code is held
constant for Zarr comparisons; only the Python adapter differs. Timings use
warm filesystem caches and include no durable disk flushes.
"""
import argparse
import hashlib
import importlib.util
import os
from pathlib import Path
import re
import statistics
import subprocess
import time

import numpy as np
import tifffile


def run(binary, *args):
    # These are generated benchmark outputs. Remove them outside the timed
    # section so both old CLIs and no-overwrite-by-default CLIs can repeat.
    if args[0] in ("encode", "encode-lossy", "decode"):
        Path(args[2]).unlink(missing_ok=True)
    start = time.perf_counter()
    result = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True)
    elapsed = time.perf_counter() - start
    if result.returncode:
        raise RuntimeError(result.stderr)
    return elapsed, result.stdout


def row(name, times):
    old, new = map(statistics.median, times)
    print(f"| {name} | {old * 1000:.3f} | {new * 1000:.3f} | {old / new:.2f}× |", flush=True)


def adapter(root, name):
    spec = importlib.util.spec_from_file_location(name, root / "radelta_zarr/_native.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-cli", type=Path, required=True)
    parser.add_argument("--candidate-cli", type=Path, required=True)
    parser.add_argument("--baseline-zarr", type=Path, required=True)
    parser.add_argument("--candidate-zarr", type=Path, required=True)
    parser.add_argument("--library", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--skip-zarr", action="store_true", help="repeat only the TIFF comparisons")
    args = parser.parse_args()
    if args.repeats < 1 or args.workers < 1:
        parser.error("positive repeats and workers are required")
    os.environ["RAYON_NUM_THREADS"] = str(args.workers)
    os.environ["RADELTA_NATIVE_LIBRARY"] = str(args.library.resolve())
    args.work_dir.mkdir(parents=True, exist_ok=True)
    binaries = [args.baseline_cli.resolve(), args.candidate_cli.resolve()]
    modules = [adapter(args.baseline_zarr, "before"), adapter(args.candidate_zarr, "after")]
    rng = np.random.default_rng(241)
    print("| Workload | Before (ms) | After (ms) | Speedup |", flush=True)
    print("|---|---:|---:|---:|", flush=True)
    for name, shape, kwargs in [
        ("Planar TIFF, one-row strips", (2, 3, 32768, 32), dict(planarconfig="separate", rowsperstrip=1)),
        ("Deflate grayscale TIFF", (8, 1024, 1024), dict(compression="deflate", rowsperstrip=64)),
    ]:
        pixels = rng.poisson(80, shape).astype(np.uint16)
        source = args.work_dir / "input.tif"
        tifffile.imwrite(source, pixels, photometric="minisblack", metadata=None, **kwargs)
        original = hashlib.sha256(source.read_bytes()).digest()
        expected = pixels.reshape(-1, *pixels.shape[-2:])
        times = {key: [[], []] for key in ("TIFF read/map", "Encode", "Streaming encode")}
        for repeat in range(args.repeats + 1):
            for i in ([0, 1] if repeat % 2 == 0 else [1, 0]):
                output = args.work_dir / f"{i}.rdlt"
                enc, _ = run(binaries[i], "encode", source, output)
                stream, _ = run(binaries[i], "encode", source, args.work_dir / f"{i}-stream.rdlt",
                                "--stream", "--memory-mib", "8")
                _, text = run(binaries[i], "benchmark", source, "--repeats", "1")
                read = float(re.search(r"TIFF read/map:\s+([\d.]+) s", text)[1])
                if repeat:
                    for key, value in zip(times, (read, enc, stream)):
                        times[key][i].append(value)
        for suffix in ("", "-stream"):
            files = [args.work_dir / f"{i}{suffix}.rdlt" for i in range(2)]
            assert files[0].read_bytes() == files[1].read_bytes(), "Radelta output bytes changed"
            for i, encoded in enumerate(files):
                output = args.work_dir / f"{i}.tif"
                run(binaries[i], "decode", encoded, output)
                with tifffile.TiffFile(output) as f:
                    np.testing.assert_array_equal(np.stack([p.asarray() for p in f.pages]), expected)
        assert hashlib.sha256(source.read_bytes()).digest() == original
        for key, values in times.items():
            row(name + ": " + key, values)

    # Tile padding, endian conversion and compressed planar fallback correctness.
    for endian in ("<", ">"):
        for tiled in (False, True):
            pixels = rng.integers(0, 65536, (2, 3, 37, 53), dtype=np.uint16).astype(endian + "u2")
            source = args.work_dir / "edge.tif"
            geometry = dict(tile=(16, 16)) if tiled else dict(rowsperstrip=8)
            tifffile.imwrite(source, pixels, planarconfig="separate", photometric="minisblack",
                             metadata=None, compression="deflate", **geometry)
            for i, binary in enumerate(binaries):
                for streaming in (False, True):
                    encoded = args.work_dir / f"edge-{i}.rdlt"
                    run(binary, "encode", source, encoded, *(["--stream", "--memory-mib", "1"] if streaming else []))
                    output = args.work_dir / f"edge-{i}.tif"
                    run(binary, "decode", encoded, output)
                    with tifffile.TiffFile(output) as f:
                        np.testing.assert_array_equal(np.stack([p.asarray() for p in f.pages]), pixels.reshape(-1, 37, 53))

    if args.skip_zarr:
        return
    for name, image in [
        ("Zarr 32 MiB uniform-noise chunk", rng.integers(0, 65536, (16, 1024, 1024), dtype=np.uint16)),
        ("Zarr 2 MiB Poisson chunk", rng.poisson(80, (16, 256, 256)).astype(np.uint16)),
    ]:
        options = [m.validate_options({}) for m in modules]
        encoded = [m.encode_chunk(image, "zyx", opt) for m, opt in zip(modules, options)]
        assert encoded[0] == encoded[1], "Zarr frame bytes changed"
        times = {key: [[], []] for key in ("encode", "decode")}
        for repeat in range(args.repeats + 1):
            for i in ([0, 1] if repeat % 2 == 0 else [1, 0]):
                start = time.perf_counter()
                frame = modules[i].encode_chunk(image, "zyx", options[i])
                enc = time.perf_counter() - start
                assert frame == encoded[0]
                start = time.perf_counter()
                decoded = modules[i].decode_chunk(encoded[0], image.shape, "zyx", image.dtype, options[i])
                dec = time.perf_counter() - start
                np.testing.assert_array_equal(decoded, image)
                if repeat:
                    times["encode"][i].append(enc)
                    times["decode"][i].append(dec)
        for key, values in times.items():
            row(name + ": " + key, values)


if __name__ == "__main__":
    main()
