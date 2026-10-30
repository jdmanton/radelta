#!/usr/bin/env python3
"""Compare two release CLIs using deterministic, independently verified TIFFs.

Requires numpy and tifffile. Timings use warm OS caches, include process startup,
and do not measure durable disk flushes. No performance assertions belong in CI.
"""
import argparse
import hashlib
from pathlib import Path
import re
import statistics
import subprocess
import time

import numpy as np
import tifffile


def run(binary, *args):
    # These are generated benchmark outputs. Remove them outside the timed
    # section so repeated runs start with absent output paths.
    if args[0] in ("encode", "encode-lossy", "decode"):
        Path(args[2]).unlink(missing_ok=True)
    start = time.perf_counter()
    result = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True)
    elapsed = time.perf_counter() - start
    if result.returncode:
        raise RuntimeError(result.stderr)
    return elapsed, result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--work-dir", required=True, type=Path)
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    args.work_dir.mkdir(parents=True, exist_ok=True)
    binaries = [args.baseline.resolve(), args.candidate.resolve()]
    print("| Workload | Measurement | Before (s) | After (s) | Speedup |", flush=True)
    print("|---|---|---:|---:|---:|", flush=True)
    for name, shape, rows in [
        ("Large planes", (64, 1024, 1024), 16),
        ("Many directories", (4096, 64, 64), 16),
        ("Many strips", (8, 65536, 64), 1),
    ]:
        pixels = np.random.default_rng(24).poisson(80, shape).astype(np.uint16)
        source = args.work_dir / "input.tif"
        tifffile.imwrite(source, pixels, photometric="minisblack", metadata=None, rowsperstrip=rows)
        timings = [{key: [] for key in ["TIFF read/map", "Encode total", "Decode total"]} for _ in binaries]
        # One warmup, then alternate order to limit cache/order bias.
        for repeat in range(args.repeats + 1):
            for i in ([0, 1] if repeat % 2 == 0 else [1, 0]):
                binary = binaries[i]
                encoded = args.work_dir / f"{i}.rdlt"
                output = args.work_dir / f"{i}.tif"
                enc, _ = run(binary, "encode", source, encoded)
                dec, _ = run(binary, "decode", encoded, output)
                _, text = run(binary, "benchmark", source, "--repeats", "1")
                read = float(re.search(r"TIFF read/map:\s+([\d.]+) s", text)[1])
                if repeat:
                    for key, value in zip(timings[i], [read, enc, dec]):
                        timings[i][key].append(value)
        for i in range(2):
            np.testing.assert_array_equal(tifffile.imread(args.work_dir / f"{i}.tif"), pixels)
        # Metadata and codec bytes must remain identical, not just the decoded pixels.
        digests = [hashlib.sha256((args.work_dir / f"{i}.rdlt").read_bytes()).digest() for i in range(2)]
        assert digests[0] == digests[1], "encoded pixels or metadata changed"
        for key in timings[0]:
            old, new = [statistics.median(t[key]) for t in timings]
            print(f"| {name} | {key} | {old:.4f} | {new:.4f} | {old / new:.2f}× |", flush=True)


if __name__ == "__main__":
    main()
