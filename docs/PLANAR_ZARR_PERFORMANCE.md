# Further planar TIFF and Zarr I/O improvements

This patch follows the TIFF I/O, output I/O, indexed-read and interleaved-cache
patches. Comparisons use those four patches together as the baseline.

## Changes

### Planar multisample TIFFs

The CLI previously read planar samples through one temporary allocation per
strip/tile, assembled a temporary plane, then copied that plane into its final
array or streaming chunk. The earlier raw-strip fast path handled only
single-sample grayscale pages.

The fast path now selects the requested sample's strip range and coalesces
adjacent uncompressed strips into bulk reads. It validates the strip tables,
sample index, geometry, unsigned 16-bit representation and transform tags;
byte-swapping still handles non-native byte order. Non-adjacent strips remain
separate reads. Compressed, tiled and photometrically transformed samples use
the TIFF decoder.

Planar samples now fill their final destination directly. Compressed full-width
strips decode into that destination; tiled/padded data use one reusable scratch
buffer per plane, copying only visible rows. The decoder's output layout must
be uint16 and match the expected chunk size before its bytes can be written.
The existing grayscale compressed-image decode path is retained.

This benefits TIFFs that store separate sample planes with
`PlanarConfiguration=2`. Interleaved RGB/multisample input remains unsupported.
There is no additional decoded-plane cache or retention budget.

### Zarr chunk framing

Encoding computes the frame checksum over a borrowed view of the native output,
then joins that view and the small header directly into the final Python bytes
object. It no longer copies the payload into a temporary Python bytes object
before constructing the frame. Native storage remains alive until the result
has been constructed, and is freed on both success and failure.

Decoding checksums a memoryview of the frame payload and passes a borrowed
pointer into the native decoder, eliminating both the payload slice copy and
the ctypes input copy. Mutable inputs still receive an immutable snapshot
before validation. Checksums, mode/geometry checks, axis and dtype handling are
unchanged. This affects both Zarr 2 and Zarr 3 through their shared adapter.

No file-format, compression setting, C ABI or default-memory-budget changes are
introduced. Benchmark output bytes match the baseline.

## Measurements

Linux x86-64, Intel Xeon Platinum 8573C, Rust 1.98.0 release CLI, eight Rayon
workers. Medians of five measured passes after a warmup, alternating baseline
and candidate order. Filesystem caches are warm; no durable-write flush is
measured. Figures describe these fixtures and this host, not cold-storage or
network performance.

The planar fixture contains two TIFF pages, each with three 32768 × 32 uint16
sample planes, Poisson mean 80, one row per strip (12 MiB decoded). TIFF read/map
time is the CLI's reported input read and layout mapping time. Complete encode
timings include process startup, TIFF input, compression, metadata and output;
streaming uses an 8 MiB memory setting.

| Workload | Before (ms) | After (ms) | Speedup |
|---|---:|---:|---:|
| Planar TIFF: read/map | 571.000 | 87.000 | 6.56× |
| Planar TIFF: complete encode | 600.919 | 356.121 | 1.69× |
| Planar TIFF: streaming encode | 824.089 | 472.261 | 1.74× |
| Deflate grayscale control: read/map | 285.000 | 181.000 | 1.57× |
| Deflate grayscale control: complete encode | 1109.592 | 900.700 | 1.23× |
| Deflate grayscale control: streaming encode | 980.186 | 496.128 | 1.98× |

The compressed grayscale control is an 8 × 1024 × 1024 uint16 Poisson stack
(16 MiB), Deflate compression, 64-row strips. Its grayscale decoder is unchanged;
no speedup is expected there. Complete encode timings also include codec and
host-scheduling variation outside the TIFF reader. The unchanged control's
timings varied substantially between runs, so its apparent gains are not
credited to this patch. Treat the wall-clock ratios as estimates on this shared
host. An earlier run of the planar fast path measured 3.89× for read/map and
2.00–2.23× for complete encodes; avoiding per-strip I/O and temporary plane
copies is the repeatable improvement.

Zarr timing uses the **same native library** for both adapters, isolating Python
framing and copy changes. Timings include the complete `encode_chunk` or
`decode_chunk` call and exclude fixture generation and equality checks. The
32 MiB fixture is 16 × 1024 × 1024 uniform uint16 noise, mostly stored as raw
blocks; the control is a 2 MiB, 16 × 256 × 256 Poisson-80 volume.

| Workload | Before (ms) | After (ms) | Speedup |
|---|---:|---:|---:|
| Zarr 32 MiB noise chunk: encode | 1372.306 | 1287.990 | 1.07× |
| Zarr 32 MiB noise chunk: decode | 160.903 | 57.349 | 2.81× |
| Zarr 2 MiB Poisson chunk: encode | 29.627 | 22.437 | 1.32× |
| Zarr 2 MiB Poisson chunk: decode | 16.563 | 16.038 | 1.03× |

The copy savings matter most for larger encoded payloads. Highly compressed
chunks can spend almost all their time in native coding/decoding and gain
little from this adapter change.

## Validation and reproduction

The benchmark requires identical baseline/candidate Radelta files in both
in-memory and streaming modes, identical Zarr frames, and pixel equality against
the independently generated NumPy input. It also checks Deflate planar strips
and padded edge tiles in both byte orders using independent tifffile fixtures,
and verifies the timed source TIFFs remain unchanged.

Native regression tests cover coalescing, gaps, both byte orders, sample
selection, truncation, invalid output sizes and transformed-data fallback.
Independent PackBits fixtures exercise compressed planar strips and padded
tiles in both CLI modes. Zarr tests cover frame-byte compatibility, bytes,
bytearray and strided memoryview inputs, both Zarr versions, all contexts,
endianness, lossy mode, checksums, forged geometry, edge chunks and sharding.

Save the CLI and Zarr Python source tree after the preceding four patches, then
build this CLI. With NumPy and tifffile installed, run:

```sh
python3 scripts/benchmark_remaining_io.py \
  --baseline-cli /path/to/before/radelta \
  --candidate-cli /path/to/after/radelta \
  --baseline-zarr /path/to/before/zarr/python \
  --candidate-zarr zarr/python \
  --library /path/to/release/libradelta_native.so \
  --work-dir /tmp/radelta-remaining-io --repeats 5 --workers 8
```

`--skip-zarr` repeats just the TIFF measurements and TIFF correctness checks.
