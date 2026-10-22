# Encoder pass performance

This patch applies after the seven preceding performance patches, including
`radelta-chunk-codec-performance.patch`. All gains below are incremental over
that version. It targets encoding of full volumes; decoder kernels, probability
normalization, calibration, public options, format and compression ratios are
unchanged.

## Implementation

1. For lossless inputs of at least 64 MiB, compute the square-root symbols and
   their maximum in one input pass. Each independent 65,536-voxel task writes
   the matching output slice and reduces a local maximum; Rayon reduces those
   maxima. The LUT is monotonic, so `max(floor(sqrt(input)))` equals the previous
   `floor(sqrt(max(input)))`. After a task's first 1,024 pixels prove the maximum
   is already 255, its tail needs only the LUT mapping. Otherwise it keeps
   reducing while mapping the tail. This is an exact bound, not sampling to
   estimate the maximum. Smaller inputs keep the existing two simple scans.
2. Histogram construction can use up to four batches per worker to balance
   uneven bright/background slabs. The previous fixed partition often puts
   several expensive slabs on one worker. A 64 MiB private-table target limits
   extra batches; if one table per worker already exceeds that target, retain
   the previous worker-count limit. Single-worker calls keep one batch. The
   number of actual batches never exceeds the number of blocks. Integer count
   merging gives exactly the same totals regardless of scheduling.
3. After the existing exact four-zero neighborhood test succeeds, mean-context
   histograms can count sixteen zeros together when the entire wider span and
   its causal neighbors are zero. Such pixels all use context zero, and their
   lossless remainders are necessarily zero. Checks exclude the first column,
   first row and incomplete spans; the first local plane uses the same synthetic
   previous-plane convention as before. Other spans use the existing four-wide
   or scalar paths. Histogram specialization applies at all supported precisions.

No new dependencies, unsafe code, caches or image-sized buffers are introduced.
The q output buffer already existed. Histogram batching may retain more private
histograms within the bound described above, which concerns histogram payloads,
not total process RAM. No model, checksum, index, stack or overflow checks are
removed. The lossless `seconds_q_extract` diagnostic now includes the maximum
scan as part of preprocessing; it previously put that scan in unclassified time.
Total encode timing has the same scope.

## Measurements

AMD EPYC 9V74 Linux x86-64 host, Rust 1.98.0 release builds, eight Rayon workers
bound to eight available CPUs. Each process has one excluded warmup. Baseline
and candidate order alternate across three batches. Full-volume medians use
seven measured repetitions per batch (21 samples/version); the small suite
uses 45 (135 samples/version).

The benchmark example loads input and prepares reference/decode buffers before
timing. Encode includes preprocessing, allocation, model construction, entropy
coding, framing and checksums. Decode includes parsing, table construction,
checksums and decoding into reused output storage. File I/O, process startup,
fixture generation and validation are outside timing. Every decoded pixel is
independently checked, and every output stream is byte-identical between
versions and repetitions.

Microscopy is `betzig_er.tif`: 186 x 613 x 860 uint16, 187.026 MiB. Poisson-80
and uniform uint16 noise controls are 16 MiB; small is 128 KiB. All use mean
context, scale bits 10 and block depth 4. Lossy calibration is offset 0 ADU,
gain 0.46 electrons/ADU, noise step 2.

| Workload | Encode before / after (ms) | Speedup | Decode before / after (ms) | Speedup |
|---|---:|---:|---:|---:|
| microscopy lossless | 161.362 / 144.590 | 1.12x | 101.181 / 101.869 | 0.99x |
| microscopy lossy | 111.725 / 101.709 | 1.10x | 71.077 / 73.863 | 0.96x |
| poisson lossless | 25.258 / 24.767 | 1.02x | 14.021 / 14.168 | 0.99x |
| poisson lossy | 15.260 / 14.462 | 1.06x | 8.905 / 9.192 | 0.97x |
| noise lossless | 53.317 / 52.646 | 1.01x | 0.823 / 0.813 | 1.01x |
| noise lossy | 25.743 / 25.973 | 0.99x | 22.195 / 23.371 | 0.95x |
| small lossless | 0.954 / 1.020 | 0.94x | 0.693 / 0.707 | 0.98x |
| small lossy | 0.508 / 0.499 | 1.02x | 0.492 / 0.412 | 1.19x |

Betzig histogram time fell from 39.234 to 32.971 ms losslessly, and 26.490 to
20.184 ms lossily. The lossless fused scan also reduces preprocessing work;
its phase timing includes the formerly separate maximum scan. Encode throughput
improves 12% and 10% in this final run. Earlier full-volume runs measured
about 8-10% and 5-7%, respectively. Results depend on the host, data and worker
count. These measurements do not predict a new throughput on the Ryzen 7950X;
rerun the README benchmark there to measure the effect.

There is no consistent new decoding gain. Some controls are flat or slower;
the table includes those results. Decoder code is unchanged, but complete
process timings still vary with scheduling, allocation and code placement.

The separate small suite preserves the earlier chunk optimizations. Tiny is
2 KiB Poisson-80, small is 128 KiB Poisson-80, and sparse-wide is 32 KiB with
full-range noise in four columns and zero in sixty. Signed3 cases explicitly
select that context; other settings match above.

| Workload | Encode before / after (ms) | Speedup | Decode before / after (ms) | Speedup |
|---|---:|---:|---:|---:|
| tiny lossless | 0.013 / 0.014 | 0.96x | 0.012 / 0.012 | 1.00x |
| tiny lossy | 0.008 / 0.008 | 0.99x | 0.008 / 0.009 | 0.97x |
| small lossless | 0.951 / 0.973 | 0.98x | 0.677 / 0.679 | 1.00x |
| small lossy | 0.520 / 0.473 | 1.10x | 0.414 / 0.407 | 1.02x |
| sparse-wide lossless | 1.020 / 1.001 | 1.02x | 0.544 / 0.551 | 0.99x |
| sparse-wide lossy | 0.414 / 0.403 | 1.03x | 0.208 / 0.228 | 0.92x |
| sparse-wide-signed3 lossless | 2.106 / 1.933 | 1.09x | 1.021 / 0.993 | 1.03x |
| sparse-wide-signed3 lossy | 0.866 / 0.790 | 1.10x | 0.479 / 0.419 | 1.14x |

## Validation

All 110 native tests pass in debug and release, and all 15 Python core API
tests pass against the rebuilt library. Formatting, Clippy with warnings denied,
release metadata synchronization and a clean incremental-patch apply check pass.

New tests check fused q values and maxima against a scalar square root at
65,536-voxel task boundaries, partial tails, full/limited intensity ranges,
all-zero input and a unique maximum in the last task, using 1/2/4 workers.
Histogram batch tests cover memory/work bounds, including saturating arithmetic,
and exact stream equality across 1/2/4/8 workers on alternating bright/empty
slabs. Existing scalar/specialized comparisons now also cover widths
15/17/31/32/33/64/65 and isolated signal at wider zero-span transitions.

The 120-configuration baseline sweep covers four contexts, precision
0/8/9/10/11, block depths 1/4/8 and both modes. All compressed bytes match and
all reconstructed pixels pass independent validation. Full-volume timed runs
also verify exact stream bytes, exercising the large-input fused path.

## Reproduce

Save a tree after the preceding seven patches as the baseline. The benchmark
example and driver already exist in that tree.

```sh
cargo build --release --manifest-path /path/to/before/native/Cargo.toml \
  --example benchmark_codec --target-dir /tmp/radelta-before
cargo build --release --manifest-path native/Cargo.toml \
  --example benchmark_codec --target-dir /tmp/radelta-after
python3 scripts/benchmark_codec.py \
  /tmp/radelta-before/release/examples/benchmark_codec \
  /tmp/radelta-after/release/examples/benchmark_codec \
  --tiff /path/to/betzig_er.tif --work-dir /tmp/radelta-full \
  --workers 8 --batches 3 --repeats 7 --verify-options
python3 scripts/benchmark_codec.py \
  /tmp/radelta-before/release/examples/benchmark_codec \
  /tmp/radelta-after/release/examples/benchmark_codec \
  --small-chunks --work-dir /tmp/radelta-small \
  --workers 8 --batches 3 --repeats 45
```

Python needs NumPy and, for the TIFF fixture, tifffile. The work directories
contain per-batch CSV timings, encoded streams and JSON medians/hashes.
