# Radelta native codec

This directory contains the Rust library and the TIFF-aware `radelta` command-line program.

Build and test with:

```bash
cargo test
cargo build --release
```

The main project documentation is:

- [README](../README.md)
- [CLI reference](../docs/CLI.md)
- [File format](../docs/FORMAT.md)

## Batched plane reads

`RadeltaFileHandle::read_planes` and `radelta_file_read_planes_u16` accept TCZ
coordinates and return consecutive planes in request order, including repeats.
Independent volumes/chunks decode in the shared Rayon pool. Single large volumes
already parallelize their codec blocks, so batches help most with many small units.
The existing adaptive 256 MiB decoded cache is unchanged. Each batch admits up to
256 MiB of additional decoded pixels plus compressed payloads, or one larger unit;
codec scratch and the caller's output are separate.

ImageJ's full-memory loader and Python reads of complete XY planes use batches
automatically, with up to 128 planes covering at most 64 MiB per call (or one larger
plane). ImageJ keeps a reusable native transfer buffer until its file is closed.
Virtual-stack single-plane browsing and Python XY crops retain their existing paths.

Compare sequential and batched file reads with:

```bash
cargo run --release --example benchmark_plane_reads -- dataset.rdlt 7 128
```

This times API calls with fresh decoded caches and warm filesystem caches, checks
both methods return identical pixel CRCs, and alternates their order across repeats.
It excludes opening files, allocating output, and computing CRCs from the timings.
- [C API](../docs/C_API.md)

Multidimensional arrays use contiguous `T,C,Z,Y,X` order with X fastest.
