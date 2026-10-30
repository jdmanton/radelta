# Benchmark tools

Radelta provides in-memory codec benchmarks and developer tools for measuring
file conversion, metadata operations, indexed reads and container adapters.
Build the native targets in release mode and record the timing scope and
measurement settings for each run.

## In-memory CLI benchmarks

```sh
radelta benchmark input.tif --threads 8 --repeats 7
radelta benchmark-lossy input.tif --threads 8 --repeats 7 \
  --offset-adu 100 --gain-e-per-adu 0.46 --noise-step 2
```

The CLI loads the TIFF, warms up each codec operation once and reports the
median measured encode/decode time. TIFF I/O and verification are excluded from
those timings. Lossless decoding is checked pixel-for-pixel; lossy decoding
reports reconstruction errors. The commands do not create output files.
See [CLI benchmark options](CLI.md#measuring-codec-performance).

## File API and codec examples

Build the Rust examples with:

```sh
cargo build --release --locked --manifest-path native/Cargo.toml --examples
```

`native/examples/benchmark_codec.rs` measures the pixel codec on little-endian
raw uint16 data. `benchmark_metadata_update.rs` measures metadata output, and
`benchmark_plane_reads.rs` measures sequential and batched plane access. For
example:

```sh
cargo run --release --manifest-path native/Cargo.toml \
  --example benchmark_plane_reads -- image.rdlt 7 128
```

The plane-read example checks pixel CRCs and measures API calls with fresh
decoded caches and warm filesystem caches. It excludes opening files, output
allocation and CRC calculation from its reported times.

## Comparison scripts

The Python scripts take explicit executable/library paths. Their positional
names `baseline` and `candidate` identify the two builds under test; both may
come from the same source tree with different build or configuration choices.
Use each script's `--help` for its complete arguments.

| Script | Workload |
| --- | --- |
| `scripts/benchmark_codec.py` | Pixel codec, context/precision settings and small chunks |
| `scripts/benchmark_tiff_io.py` | TIFF encoding, decoding and layout-reading time |
| `scripts/benchmark_radelta_output.py` | Encoded output and metadata attachment |
| `scripts/benchmark_radelta_reads.py` | Indexed file opening and Python array reads |
| `scripts/benchmark_interleaved_reads.py` | Sequential/channel-interleaved reads and TIFF page ordering |
| `scripts/benchmark_remaining_io.py` | Planar/compressed TIFF ingestion and OME-Zarr chunk framing |

These tools use generated fixtures or an explicitly supplied input. Work
directories contain generated inputs and outputs; use a dedicated directory.
Scripts verify pixel values, metadata or encoded hashes as appropriate to each
workload. They do not supply a cross-codec reproduction of the README tables.

## Interpreting results

Record the input shape, dtype, calibration, codec options, worker count, CPU,
OS, compiler, codec/library versions, warm-ups, repetitions and timing scope.
State whether metadata, TIFF I/O, allocation and validation are included.
Compression ratios should use the source sample width: one byte for uint8 and
two bytes for uint16.

Process startup, model construction, chunk geometry and metadata size can
dominate small workloads. Thread count and memory allowance affect queueing,
streaming chunk layout and read amplification. Warm filesystem-cache timings
measure software throughput; they do not establish cold-storage, network or
durable-write throughput. The CLI does not time an `fsync` operation.

Compare measurements with matched settings and verify the decoded pixels.
Treat close timing results as workload- and environment-dependent rather than
as a universal ordering of implementations.
