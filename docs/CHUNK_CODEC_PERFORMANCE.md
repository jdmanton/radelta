# Chunk and model setup performance

This is an incremental patch over the six earlier I/O, cache and codec-kernel
patches. It targets small chunks and sparse probability models. It does not
change options, calibration, compression ratios, serialized bytes or APIs.

## Changes

- Preprocessing scans use a minimum Rayon task size of 65,536 voxels. Tiny
  arrays run without recursively splitting into many jobs; large arrays remain
  parallel. This applies to both the maximum scan and q extraction, in both
  compression modes. Histogram and entropy-coding parallelism are unchanged.
- The four possible reciprocal tables are initialized once with `OnceLock`,
  instead of rebuilding them for every model. Their combined payload is about
  30 KiB when every supported precision has been used; initialization is thread
  safe and does not depend on image content or calibration.
- Frequency normalization writes into the model's existing output row and
  reuses remainder/order scratch arrays across rows. Single-symbol rows assign
  the entire normalization total directly. Other rows retain the same sort,
  tie handling, minimum frequencies and correction order as before.
- Normalization uses hardware-width division when `sum * target` fits in u64.
  Every symbol count is at most the sum, so this bounds every numerator too.
  Larger counts retain u128 arithmetic. This changes neither rounding nor the
  distribution of leftover frequencies.
- Models retain a sorted list of nonempty contexts, collected during training
  or parsing. Serialization, cumulative-table construction and packed decoder
  table construction visit those contexts. Dense arrays and decoder lookup
  layout remain unchanged; omitted contexts retain invalid zero slots. Parsing
  still checks context and symbol ordering/ranges, canonical varints, nonzero
  frequencies, row totals and trailing bytes. It initializes frequencies to
  zero and writes only contexts in the validated list, so skipped rows cannot
  hide unchecked nonzero frequencies.

There are no new dependencies, unsafe operations or image-sized buffers.
Normalization scratch is local to each model build, and active-context lists
belong to their models. The small immutable reciprocal tables are the only
new globally retained memory. The measurements below are steady-state timings
with one excluded warmup per process, not cold initialization measurements.

## Measurements

Linux x86-64, AMD EPYC 9V74 host, Rust 1.98.0 release builds, eight Rayon workers
bound to eight available CPUs. Before/after processes alternate order across
three batches. Small-chunk results use 45 measured repetitions per batch
(135 samples/version); large controls use three (nine samples/version).
Tables report medians. Scheduling makes submillisecond timings particularly
variable; an earlier run measured 9–17x rather than 16–21x for the tiny cases.

The unchanged Rust example measures complete encode calls and decode calls
into reused storage, including model setup and checksums. File I/O, process
startup, fixture generation, output-buffer allocation for decoding, and pixel
or stream comparisons are outside the timers. Every saved stream is identical
between versions and runs; every decoded pixel is independently validated.
Both modes use the existing default Mean context, ten-bit precision and block
depth four, except the explicitly labeled Signed3 cases. Lossy calibration is
offset 0 ADU, gain 0.46 electrons/ADU, noise step two.

Tiny is a 1 x 32 x 32 Poisson-80 chunk (2 KiB raw); small is 4 x 128 x 128
Poisson-80 (128 KiB). Sparse-wide is 4 x 64 x 64 (32 KiB), with full-range
uint16 noise in four columns and zeros in the other sixty. It exercises wide
alphabets and many empty model contexts. These generated chunks are useful
controls, not a claim about every Zarr dataset.

| Chunk and mode | Encode before / after (ms) | Speedup | Decode before / after (ms) | Speedup |
|---|---:|---:|---:|---:|
| Tiny lossless | 0.218 / 0.014 | 16.09x | 0.015 / 0.012 | 1.31x |
| Tiny lossy | 0.174 / 0.008 | 21.08x | 0.012 / 0.008 | 1.46x |
| Small lossless | 1.363 / 0.968 | 1.41x | 0.696 / 0.679 | 1.03x |
| Small lossy | 0.851 / 0.472 | 1.80x | 0.422 / 0.394 | 1.07x |
| Sparse-wide lossless | 1.782 / 0.963 | 1.85x | 0.639 / 0.522 | 1.23x |
| Sparse-wide lossy | 0.880 / 0.389 | 2.26x | 0.243 / 0.197 | 1.23x |
| Sparse-wide Signed3 lossless | 2.954 / 1.744 | 1.69x | 1.226 / 0.861 | 1.42x |
| Sparse-wide Signed3 lossy | 1.426 / 0.743 | 1.92x | 0.557 / 0.391 | 1.43x |

A separate run with one worker, also 135 samples/version, measured 2.08x and
2.48x tiny encode gains, 1.02–1.04x small encode gains, and 1.31–1.41x
sparse-wide encode / 1.19–1.39x decode gains. Avoiding scheduling overhead has
its biggest effect when a small chunk otherwise fans out to several workers.
These are serial calls into a configured pool, not a benchmark of many
simultaneous outer callers.

The large-volume controls retain parallelism. Betzig is the 187.026 MiB
186 x 613 x 860 microscopy volume; Poisson and noise are 16 MiB each. The
128 KiB control below differs from the small-suite fixture because it follows
the original benchmark's random-number sequence.

| Control and mode | Encode before / after (ms) | Speedup | Decode before / after (ms) | Speedup |
|---|---:|---:|---:|---:|
| Betzig lossless | 147.367 / 140.250 | 1.05x | 93.896 / 89.133 | 1.05x |
| Betzig lossy | 92.789 / 94.618 | 0.98x | 67.348 / 65.048 | 1.04x |
| Poisson lossless | 25.643 / 24.970 | 1.03x | 14.539 / 14.163 | 1.03x |
| Poisson lossy | 14.981 / 14.516 | 1.03x | 9.032 / 8.571 | 1.05x |
| Noise lossless | 53.338 / 50.760 | 1.05x | 0.775 / 0.763 | 1.02x |
| Noise lossy | 25.758 / 23.271 | 1.11x | 20.131 / 19.175 | 1.05x |
| Small lossless | 1.190 / 0.926 | 1.28x | 0.668 / 0.676 | 0.99x |
| Small lossy | 0.704 / 0.492 | 1.43x | 0.435 / 0.382 | 1.14x |

The Betzig results are essentially flat this round, including a 2% slower
lossy encode measurement. The order-of-magnitude improvement applies to tiny
chunk encoding with a multithreaded pool, not large-volume throughput.

## Validation

All 108 native tests pass in debug and release, and all 15 Python core API
tests pass against the rebuilt native library. Formatting, Clippy with warnings
denied, and release metadata synchronization checks pass.

The normalization test retains the previous implementation as an independent
test-only oracle. It compares reused dirty output/scratch against it across
all four precisions, varying alphabets, empty and single-symbol rows, tied
remainders, correction in both directions, random counts and u64::MAX counts.
Reciprocal values and reuse are checked at every supported precision.
Sparse model roundtrip tests check the active context list and packed entries,
including empty models and extreme alphabets/context indices. Existing tests
cover golden streams, malformed models, truncation, checksums, all contexts,
all precisions, raw blocks, streaming and indexed readers, TIFF and C/Python
entry points.

A separate 120-configuration baseline comparison covers four contexts,
precision 0/8/9/10/11, block depth 1/4/8 and both modes. All compressed bytes
match and all reconstructed pixels pass independent validation.

## Reproduce

Save the source tree after the preceding six patches as the baseline, then:

```sh
cargo build --release --manifest-path /path/to/before/native/Cargo.toml \
  --example benchmark_codec --target-dir /tmp/radelta-model-before
cargo build --release --manifest-path native/Cargo.toml \
  --example benchmark_codec --target-dir /tmp/radelta-model-after
python3 scripts/benchmark_codec.py \
  /tmp/radelta-model-before/release/examples/benchmark_codec \
  /tmp/radelta-model-after/release/examples/benchmark_codec \
  --small-chunks --work-dir /tmp/radelta-small-chunks \
  --workers 8 --batches 3 --repeats 45 --verify-options
python3 scripts/benchmark_codec.py \
  /tmp/radelta-model-before/release/examples/benchmark_codec \
  /tmp/radelta-model-after/release/examples/benchmark_codec \
  --tiff /path/to/betzig_er.tif --work-dir /tmp/radelta-large-controls \
  --workers 8 --batches 3 --repeats 3
```

Repeat the small suite with `--workers 1` for the one-worker control. Python
requires NumPy and, for the large TIFF fixture, tifffile. The work directories
contain per-batch CSV timings, encoded streams and JSON medians/hashes.
