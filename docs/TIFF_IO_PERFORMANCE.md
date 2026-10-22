# TIFF I/O performance

These changes apply to the native `radelta` TIFF CLI. Baseline:
`21b56adcacfd96eef8c90ccac8315405756b843f` (0.8.1).

## Changes

- Capture only the raw tags actually present in each IFD, including private and
  nested tags. The old code searched all 65,536 possible tag IDs for every IFD.
  Raw type preservation, metadata budgets and tag order remain identical.
- Coalesce adjacent uncompressed uint16 grayscale strips and read the resulting
  byte ranges directly into the final pixel buffer. Read strip tables as typed
  byte buffers rather than allocating a TIFF Value for every table element.
- Fill streaming chunks directly and bulk-read contiguous ImageJ data, removing
  temporary plane buffers, per-pixel pushes and extra plane copies. Swap bytes
  in place when the TIFF byte order differs from the host.
- Write one uncompressed strip per output plane, with a 1 MiB output buffer.
  The metadata-aware classic TIFF/BigTIFF writer remains in use.

Compressed, tiled, planar-multisample and non-default photometric/predictor
layouts retain their existing TIFF decoder paths. There are no codec/format
changes or additional dependencies.

## Measurements

Linux x86-64, eight Rayon workers, release builds. Medians of five measured runs
following one warmup; binary order alternates. Warm OS caches, no fsync: these
are software/cache benchmarks, not durable-storage or network throughput.
Input pixels are deterministic uint16 Poisson samples with mean 80.

- Large planes: 64 x 1024 x 1024 (128 MiB), 16 rows per strip.
- Many directories: 4096 x 64 x 64 (32 MiB), 16 rows per strip.
- Many strips: 8 x 65536 x 64 (64 MiB), one row per strip. This deliberately
  stresses strip dispatch; it is not a typical camera-frame shape.

TIFF read/map is the CLI's reported input loading time, excluding metadata
capture. Encode/decode totals include process startup, TIFF I/O, codec work,
metadata handling and output close. Every decoded array was independently
checked using tifffile. Baseline/candidate Radelta outputs also have identical
SHA-256 hashes for all three workloads, including their metadata.

| Workload | Measurement | Before (s) | After (s) | Speedup |
|---|---|---:|---:|---:|
| Large planes | TIFF read/map | 0.0660 | 0.0590 | 1.12× |
| Large planes | Encode total | 0.3796 | 0.3389 | 1.12× |
| Large planes | Decode total | 0.2057 | 0.2033 | 1.01× |
| Many directories | TIFF read/map | 0.0450 | 0.0420 | 1.07× |
| Many directories | Encode total | 3.0104 | 0.1704 | 17.67× |
| Many directories | Decode total | 0.1121 | 0.1150 | 0.97× |
| Many strips | TIFF read/map | 0.3450 | 0.0370 | 9.32× |
| Many strips | Encode total | 0.6951 | 0.4016 | 1.73× |
| Many strips | Decode total | 0.2790 | 0.2824 | 0.99× |

The order-of-magnitude improvement is workload dependent. Many-directory
encoding improves 17.7x because metadata capture was the bottleneck. The
strip-heavy input reader improves 9.3x. Ordinary large-plane reading improves
about 12%; TIFF writing shows no material improvement in these measurements.
Do not interpret these results as a universal 10x improvement in both directions.

## Reproduce

Save release binaries from the baseline and modified source trees, then run:

```sh
python -m pip install numpy tifffile
python scripts/benchmark_tiff_io.py /path/to/before /path/to/after \
  --work-dir /path/to/temporary/benchmark-data --repeats 5
```

The benchmark creates and overwrites its own files in the selected work directory.
It checks pixel equality and identical encoded bytes after each workload.

## Validation

- Formatting and Clippy with warnings denied, using the pinned Rust 1.98.0.
- All 95 native tests in debug and release, including byte order, adjacent and
  separated strips, truncation, classic TIFF/BigTIFF, LZW with horizontal
  prediction, in-memory and streaming paths, metadata and output preservation.
- Additional independent tifffile roundtrips: big-endian, BigTIFF, LZW strips,
  Deflate strips, uncompressed/Deflate tiles and contiguous ImageJ stacks, each
  in memory and streaming modes.
- Release metadata remains synchronized at 0.8.1.

An additional LZW-tiled fixture produced `no lzw end code found` in both the
baseline and modified CLI. It remains on the original decoder path; this
existing decoder compatibility issue is outside this performance change.
