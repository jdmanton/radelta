# Further read and small-volume performance improvements

This patch follows both the TIFF I/O and Radelta output I/O patches based on
`21b56adcacfd96eef8c90ccac8315405756b843f`. All comparisons below use the version
with both earlier patches as their baseline.

## Changes

### File indexing

Opening RDM2/RDQ2 and RDS3/RQS3 files previously read each integer in a record
separately and repeatedly queried or changed the file cursor. The indexer now
reads each record header together with the embedded RDL1/RDLQ dimension header:
28 bytes for a volume record, 44 bytes for a streaming chunk record. It tracks
the next offset directly and uses positioned reads on Unix and Windows, with a
seek/read fallback on other platforms.

It does not prefetch encoded pixel data beyond those headers. File bounds,
record counts, payload mode/dimensions, canonical TCZ order and trailing-byte
checks remain in place. Actual decoding still verifies payload checksums.
This benefits the Rust file API, CLI decode, Fiji virtual stacks and Python.

### Python reads

- Decompression borrows immutable Python bytes through ctypes instead of
  copying the complete encoded image into a ctypes array. Explicit lengths
  preserve embedded NULs, and the input remains owned for the whole call.
  Mutable buffers and memoryviews retain the previous snapshot-copy behavior.
- Full-XY file reads fill the final output planes directly through the native
  API. They no longer allocate intermediate planes and copy them into the result.
- Partial/reversed XY selections use basic NumPy slice views instead of
  constructing integer index arrays and an advanced-index copy. HDF5 handling
  remains on its existing path.

### Lossless lookup table

The 65,536-entry square-root lookup table is initialized once using OnceLock
and shared across lossless encodes. It previously had to be rebuilt for every
XYZ volume. This mainly benefits small chunks and independently encoded planes.
All uint16 entries are checked against the exact floor-square-root invariant.

## Measurements

Linux x86-64, Rust 1.98.0 release libraries, eight Rayon workers. Medians of seven
measured runs after one warmup; native comparisons alternate binary order.

| Workload | Before (ms) | After (ms) | Speedup |
|---|---:|---:|---:|
| Open 100,000 volumes | 64.2292 | 23.9279 | 2.68× |
| Open 100,000 chunks | 152.6945 | 26.5763 | 5.75× |
| Encode 128 independent 32×32 planes | 2.6803 | 1.0865 | 2.47× |
| Python decompress(bytes) | 67.4118 | 13.7243 | 4.91× |
| Python cached full-stack read | 241.4720 | 14.4919 | 16.66× |

The index tests use 100,000 valid one-voxel volume/chunk records to isolate
indexing overhead. Opening does not decode their pixels. The small-volume
encode input is 128 independent 32x32 uint16 Poisson planes with mean 80.

Python timings use a real 128 MiB, 16x2048x2048 uint16 uniform-noise stack,
which the lossless codec mostly stores as raw blocks. The cached-read row
excludes opening and the first decode: it measures repeated full-array reads
from an already populated native cache. These are warm-cache/software timings,
not cold-storage or network throughput. Native decompression benefits depend
on encoded size and how much time the codec itself spends decoding the data.

The benchmark checks exact encoded-byte equality for the small-volume encodes
and independent decoded-array hashes for both Python reading paths. There are
no file format, compression ratio or C ABI changes.

## Reproduce

Save release native libraries and Python source trees after the previous two
patches and after this patch. Each package root must contain radelta/.

```sh
python -m pip install numpy
python scripts/benchmark_radelta_reads.py /path/to/before-library /path/to/after-library \
  --work-dir /path/to/temporary/fixtures \
  --baseline-python /path/to/before/python --candidate-python /path/to/after/python \
  --repeats 7 --records 100000
```

Omit both Python options to run only the native benchmarks. The work directory
contains generated fixtures that the script overwrites. File opening, small
volume encoding and Python reads are timed separately from fixture creation.

## Validation

- All 100 native tests pass in debug and release using Rust 1.98.0, including
  random reads of all four indexed container formats, partial final chunks,
  invalid embedded dimensions, truncation and trailing bytes.
- All 14 Python tests pass, including 300 combinations of forward/reverse,
  strided, empty and integer XY/Z selections against NumPy; mutable and
  non-contiguous encoded buffers; metadata and native allocation ownership.
- Formatting, Clippy with warnings denied and 0.8.1 release metadata checks pass.
- Windows x86-64 cargo check covers all native targets. Windows runtime testing
  remains with the existing CI matrix; the timing figures above are from Linux.
