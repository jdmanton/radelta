# Codec kernel performance

This patch follows the five TIFF/Radelta/Zarr I/O and cache patches. Its
baseline already includes all five. The changes below are inside the Rust
encoder and decoder, independent of file I/O or the Python adapters.

## Implementation

The default mean context and ten-bit rANS precision now select specialized
kernels once per block. Their fixed context choices, table strides, masks and
shifts are compile-time constants. Other context/precision combinations use
the general kernels. Mean-context histogram construction is specialized at
every supported precision, since histogram counts do not depend on precision.

The default encoder exposes all four rANS lanes explicitly in groups of four,
matching the decoder's existing arrangement. It handles row alignment and
short tails separately and retains the exact reverse symbol order required
by the serialized byte stacks.

Background regions can also share work without changing entropy coding:

- Histograms add four identical `(context=0, symbol=0)` observations together
  when the current symbols and every causal neighbor are zero. Lossless
  remainders are then necessarily zero too.
- The encoder reuses the prepared zero-symbol entry in that situation, encoding
  each lane in its original order. It omits repeated context calculations and
  model-index checks, rather than changing the coding probabilities.
- The decoder batches a group only when its already decoded causal neighbors
  are zero **and all four current rANS slots select symbol zero**. It advances
  each state and consumes renormalization bytes in the original lane order.
  This is an exact decoding condition, not a prediction that background pixels
  will probably be zero.

Special first rows/columns and incomplete groups use the normal per-symbol
path. Header/model validation, block CRCs, invalid-slot checks, byte-stack
exhaustion and reconstruction checks are retained. There are no new unsafe
operations, image-sized buffers, caches, dependencies, public APIs or format
changes. Calibration, quantization, model normalization and compression ratios
are unchanged.

## Measurements

Linux x86-64, AMD EPYC 9V74 host, Rust 1.98.0 release builds, eight Rayon workers
bound to eight available CPUs. Each version runs three batches with one warmup
and three measured repetitions per batch; version order alternates between
batches. Tables show medians of nine samples. Results remain host- and
data-dependent.

The Rust benchmark loads input and prepares reference/decode buffers before
timing. Encode timing includes model construction, coding, allocations, output
assembly and checksums. Decode timing includes parsing, model/table construction,
checksums and decoding into a reused output buffer. File I/O, process startup,
fixture generation and independent pixel/byte equality checks are excluded.

`betzig_er.tif` contains 186 × 613 × 860 uint16 voxels (187.026 MiB), ranging
from 0 to 6182; 77.96% are zero. Its lossless stream remains 15.659 MiB, and its
lossy stream remains 5.393 MiB with offset 0 ADU, gain 0.46 electrons/ADU and
noise step 2. The synthetic controls are a 16 MiB Poisson-80 volume, 16 MiB
uniform uint16 noise, and a 128 KiB Poisson-80 chunk. The same lossy calibration
is used throughout.

| Workload | Encode before / after (ms) | Speedup | Decode before / after (ms) | Speedup |
|---|---:|---:|---:|---:|
| Betzig lossless | 215.180 / 145.841 | 1.48× | 133.163 / 94.625 | 1.41× |
| Betzig lossy | 156.471 / 100.816 | 1.55× | 100.211 / 69.867 | 1.43× |
| Poisson lossless | 28.009 / 25.642 | 1.09× | 15.062 / 13.985 | 1.08× |
| Poisson lossy | 16.273 / 14.141 | 1.15× | 9.351 / 8.964 | 1.04× |
| Noise lossless | 50.099 / 53.537 | 0.94× | 0.710 / 0.683 | 1.04× |
| Noise lossy | 27.791 / 23.264 | 1.19× | 19.441 / 19.049 | 1.02× |
| Small lossless | 1.211 / 1.156 | 1.05× | 0.708 / 0.678 | 1.04× |
| Small lossy | 0.802 / 0.675 | 1.19× | 0.427 / 0.436 | 0.98× |

The largest gains come from background-rich data. Dense controls benefit less,
and raw-block decoding does not use the new entropy kernels. Small submillisecond
measurements also include scheduling/allocation variability. Some controls are
flat or slightly slower; speedups are not consistent across every workload.
An earlier nine-sample run measured 1.56–1.62× for the Betzig encode/decode cases;
the final table above reports the final run. These results do
not imply an order-of-magnitude codec gain or a gain for every input/settings
combination.

## Validation

- All 107 native tests pass in debug and release, including indexed/streaming
  readers, C ABI paths, TIFF layouts, metadata and corrupt/truncated streams.
- All 15 Python core API tests pass against the rebuilt native library.
- Specialized kernels are compared directly with the general kernels on zero,
  sparse and full-range data, widths 1/3/7/16/129, odd heights, partial final
  blocks and zero-to-signal transitions. Histogram counts, states, byte stacks,
  decoded symbols/pixels and malformed-stack outcomes match.
- Reciprocal rANS encoding is checked against division at every supported
  frequency/precision, including the specialized ten-bit path.
- A separate 120-configuration sweep compares baseline/candidate stream bytes
  across all four contexts, precision 0/8/9/10/11, block depths 1/4/8 and both
  modes. Zero selects the existing default precision. Every run independently
  validates decoded pixels; lossy output is checked against scalar reconstruction.
- Every timed stream is byte-identical across versions and batches. Formatting,
  Clippy with warnings denied and release metadata checks pass.

## Reproduce

Save a source tree after the preceding five patches as the baseline. Copy this
patch's unchanged benchmark example into that tree, then build both examples:

```sh
cp native/examples/benchmark_codec.rs /path/to/before/native/examples/
cargo build --release --manifest-path /path/to/before/native/Cargo.toml \
  --example benchmark_codec --target-dir /tmp/radelta-before
cargo build --release --manifest-path native/Cargo.toml \
  --example benchmark_codec --target-dir /tmp/radelta-after
python3 scripts/benchmark_codec.py \
  /tmp/radelta-before/release/examples/benchmark_codec \
  /tmp/radelta-after/release/examples/benchmark_codec \
  --tiff /path/to/betzig_er.tif --work-dir /tmp/radelta-codec-benchmark \
  --workers 8 --batches 3 --repeats 3 --verify-options
```

Python requires NumPy and tifffile. Omit `--tiff` to run only the generated
controls. The work directory contains per-batch CSV timings, encoded streams
and `results.json`, including phase timings and SHA-256 hashes. The example
also accepts other contexts, precisions, block depths and little-endian uint16
raw volumes; its usage message lists positional arguments.
