# Channel-interleaved reads

The native file reader previously kept just one decoded volume (ND containers)
or Z chunk (streaming containers). Reading channels alternately therefore
discarded useful decoded data at every channel switch. Fiji's virtual stack
requests planes in channel-first order within each Z position, and TIFF exports
with `--page-order TZC` encounter the same problem.

The reader now uses an adaptive LRU cache. A first sequential pass retains one
entry, preserving its previous decoded-data footprint. Same-Z channel switches
or revisits to recently read entries enable multiple entries. The default
retention budget is 2 GiB **per open handle**, grown on demand, with at most
128 entries.
Entries are evicted before decoding another volume/chunk, and retained bytes
are counted using allocation capacity. One oversized entry is allowed, as in
the original reader; this budget does not cap total decoder working memory.
Single-volume files continue to use their existing single-entry cache.

Rust callers can set `RadeltaFileHandle::set_cache_limit(bytes)`; C callers can
use the new additive `radelta_file_set_cache_limit(handle, bytes)` export.
Python's `open` and `read` accept `cache_mib`. Zero restores the previous
single-entry behavior. Existing C exports and file formats are unchanged, and
this change does not alter encoding. Rebuild the native library with the Python
bindings; Fiji benefits from the new native library without Java changes.

## Measurements

Measured on Linux x86-64, Intel Xeon Platinum 8573C, Rust 1.98.0 release builds.
These historical measurements used a 256 MiB retention budget; the current
default is 2 GiB. They do not measure the effect of the larger default.
The baseline includes the preceding TIFF I/O, output I/O and indexed-read
patches. These are additional gains over that baseline.

The fixture contains two 32 × 256 × 256 uint16 channels, generated with a seeded
Poisson distribution (mean 80): 8 MiB decoded in total. Its active data fits the
cache budget. The streaming fixture uses 16-plane Z chunks. Results are medians
of five measured passes after one warmup, alternating baseline/candidate order.
Every native pass opens a fresh handle; initial decoding and plane copies are
timed, while opening, output allocation, validation and closing are excluded.
CLI measurements include the complete subprocess, opening, decoding, TIFF
writing and closing. Filesystem caches are warm; these are not cold-storage or
durable-write (`fsync`) measurements. Host scheduling affects absolute times.

| Workload | Before (ms) | After (ms) | Speedup |
|---|---:|---:|---:|
| ND sequential planes | 72.632 | 69.277 | 1.05× |
| ND channel-interleaved planes | 1949.603 | 62.046 | 31.42× |
| Streaming sequential planes | 74.705 | 80.286 | 0.93× |
| Streaming channel-interleaved planes | 1920.559 | 121.199 | 15.85× |
| ND → TCZ TIFF | 168.903 | 167.901 | 1.01× |
| ND → TZC TIFF | 2425.795 | 95.818 | 25.32× |
| Streaming → TCZ TIFF | 116.057 | 119.444 | 0.97× |
| Streaming → TZC TIFF | 1418.658 | 124.457 | 11.40× |

The improvement comes from avoiding repeated decompression. Sequential reads
have no repeated decoding to eliminate; the controls show no consistent gain.
If active volumes/chunks exceed the cache budget or entry limit, eviction can
remove much of the interleaved-read benefit. Increasing the budget trades more
retained memory for fewer decodes.

The benchmark verifies every native pass against the original pixels, checks
the zero-budget path, independently reads both TIFF exports with `tifffile`,
and requires identical baseline/candidate TIFF SHA-256 hashes for each order.
It also verifies the source Radelta files remain unchanged. Regression tests
cover lossy and lossless indexed containers, cache eviction, adaptive activation,
oversized entries, allocation-capacity accounting and entry-count bounds.

## Reproduce

Build baseline and candidate libraries and CLIs into separate target directories,
then run with NumPy and tifffile installed:

```sh
python3 scripts/benchmark_interleaved_reads.py \
  /path/to/before/release/libradelta_native.so \
  /path/to/after/release/libradelta_native.so \
  --baseline-cli /path/to/before/release/radelta \
  --candidate-cli /path/to/after/release/radelta \
  --work-dir /tmp/radelta-interleaved-benchmark --repeats 5
```
