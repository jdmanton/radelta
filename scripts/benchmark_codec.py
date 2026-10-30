#!/usr/bin/env python3
"""Compare the same benchmark_codec Rust example built against two libraries.

All reported times exclude file I/O, fixture generation, output allocation for
decoding and validation. Each process warms up once. Alternate the two
executables across batches; bind to the first requested available CPUs on Linux.
Requires NumPy, plus tifffile when --tiff is used. The Rust example independently
checks every decoded pixel, including scalar lossy reconstruction.
"""
import argparse
import csv
import hashlib
import io
import json
import os
from pathlib import Path
import statistics
import subprocess

import numpy as np


def verify_options(binaries, root):
    root.mkdir(exist_ok=True)
    rng = np.random.default_rng(991)
    pixels = rng.poisson(80, (9, 17, 129)).astype(np.uint16)
    pixels[:, :, :70] = 0
    pixels[1:3, 4:6, 1:6] = rng.integers(0, 5000, (2, 2, 5), dtype=np.uint16)
    raw = root / "input.u16"
    pixels.astype("<u2").tofile(raw)
    count = 0
    for context in range(4):
        for bits in (0, 8, 9, 10, 11):
            for depth in (1, 4, 8):
                for mode in ("lossless", "lossy"):
                    encoded = []
                    for i, binary in enumerate(binaries):
                        output = root / f"{i}.rdlt"
                        subprocess.run([str(binary.resolve()), str(raw.resolve()), "129", "17", "9",
                            mode, "1", str(context), str(bits), str(depth), str(output.resolve())],
                            capture_output=True, text=True, check=True)
                        encoded.append(output.read_bytes())
                    assert encoded[0] == encoded[1], (context, bits, depth, mode)
                    count += 1
    print(f"\n{count} option combinations produced identical bytes and passed pixel validation.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--tiff", type=Path)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--batches", type=int, default=3)
    parser.add_argument("--small-chunks", action="store_true", help="benchmark 2/32/128 KiB chunks instead of full volumes")
    parser.add_argument("--verify-options", action="store_true", help="also compare 120 context/precision/depth/mode combinations")
    args = parser.parse_args()
    if args.small_chunks and args.tiff:
        parser.error("--small-chunks and --tiff select different benchmark suites")
    if min(args.workers, args.repeats, args.batches) < 1:
        parser.error("workers, repeats and batches must be positive")
    os.environ["RAYON_NUM_THREADS"] = str(args.workers)
    if hasattr(os, "sched_getaffinity"):
        cpus = sorted(os.sched_getaffinity(0))[:args.workers]
        os.sched_setaffinity(0, cpus)
    args.work_dir.mkdir(parents=True, exist_ok=True)
    rng = np.random.default_rng(241)
    cases = []
    if args.tiff:
        import tifffile
        pixels = tifffile.imread(args.tiff)
        if pixels.ndim != 3 or pixels.dtype.kind != "u" or pixels.dtype.itemsize != 2:
            parser.error("--tiff must contain a uint16 ZYX stack")
        cases.append(("microscopy", pixels, 2))
    if not args.small_chunks:
        cases.extend([
            ("poisson", rng.poisson(80, (32, 512, 512)).astype(np.uint16), 2),
            ("noise", rng.integers(0, 65536, (32, 512, 512), dtype=np.uint16), 2),
            ("small", rng.poisson(80, (4, 128, 128)).astype(np.uint16), 2),
        ])
    if args.small_chunks:
        rng = np.random.default_rng(725)
        cases = [
            ("tiny", rng.poisson(80, (1, 32, 32)).astype(np.uint16), 2),
            ("small", rng.poisson(80, (4, 128, 128)).astype(np.uint16), 2),
        ]
        sparse = rng.integers(0, 65536, (4, 64, 64), dtype=np.uint16)
        sparse[:, :, :60] = 0
        cases.extend([("sparse-wide", sparse, 2), ("sparse-wide-signed3", sparse, 3)])
    results = {}
    print("| Workload | Encode before / after (ms) | Speedup | Decode before / after (ms) | Speedup |", flush=True)
    print("|---|---:|---:|---:|---:|", flush=True)
    for name, pixels, context in cases:
        raw = args.work_dir / (name + ".u16")
        pixels.astype("<u2").tofile(raw)
        z, y, x = pixels.shape
        for mode in ("lossless", "lossy"):
            samples = [[], []]
            hashes = set()
            for batch in range(args.batches):
                for i in ([0, 1] if batch % 2 == 0 else [1, 0]):
                    encoded = args.work_dir / f"{name}-{mode}-{i}.rdlt"
                    binary = [args.baseline, args.candidate][i]
                    run = subprocess.run([str(binary.resolve()), str(raw.resolve()), str(x), str(y), str(z),
                        mode, str(args.repeats), str(context), "10", "4", str(encoded.resolve())],
                        text=True, capture_output=True, check=True)
                    (args.work_dir / f"{name}-{mode}-{i}-{batch}.csv").write_text(run.stdout)
                    samples[i].extend(dict(row) for row in csv.DictReader(io.StringIO(run.stdout)))
                    hashes.add(hashlib.sha256(encoded.read_bytes()).hexdigest())
            assert len(hashes) == 1, "encoded bytes changed between versions or runs"
            medians = [{key: statistics.median(float(row[key]) for row in rows)
                        for key in rows[0] if key not in ("iteration", "encoded_crc")} for rows in samples]
            enc = [m["encode_seconds"] * 1000 for m in medians]
            dec = [m["decode_seconds"] * 1000 for m in medians]
            print(f"| {name} {mode} | {enc[0]:.3f} / {enc[1]:.3f} | {enc[0]/enc[1]:.2f}× | "
                  f"{dec[0]:.3f} / {dec[1]:.3f} | {dec[0]/dec[1]:.2f}× |", flush=True)
            results[name + " " + mode] = dict(shape=list(pixels.shape), context=context, raw_bytes=pixels.nbytes,
                sha256=next(iter(hashes)), before=medians[0], after=medians[1])
    (args.work_dir / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    if args.verify_options:
        verify_options([args.baseline, args.candidate], args.work_dir / "compatibility")


if __name__ == "__main__":
    main()
