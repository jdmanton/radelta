# Radelta output I/O

This change follows the TIFF I/O optimization patch based on commit
`21b56adcacfd96eef8c90ccac8315405756b843f`. The following measurements compare
against that TIFF-optimized version, so they do not count the earlier gains.

## What changed

- C API metadata size queries borrow the encoded pixels. They no longer clone
  the entire image to determine the output size. A subsequent output call
  copies the pixel payload directly into the caller's buffer once. Previously
  the two-call operation made at least three full-payload copies in Rust.
- The Python array wrapper retains the native encoded buffer until metadata
  attachment completes. This removes two additional full-image copies through
  Python bytes and a ctypes input array. The native allocation is released with
  its original length on success and on failure.
- Rust `write_encoded_with_metadata` writes a small patched header, the borrowed
  encoded pixel payload, and compressed metadata via vectored I/O where
  supported. The CLI uses it without growing its encoded-image Vec to append
  metadata. The input buffer stays unchanged.
- Metadata replacement locates the old block using its footer without loading
  the old block into memory. It also skips redundant flag writes and avoids
  truncation calls when an ordinary write already extends the file.
- CLI and C API streaming writers append metadata through their existing
  buffered output handle. Finishing no longer needs to reopen or reread the
  fresh output file. Outputs still publish only after successful finalization.

No file format or C ABI changes are required. Lossless and lossy files retain
identical bytes. New Rust output and existing C metadata paths handle short
writes, Interrupted errors, metadata limits and output failures.

## Measurements

Linux x86-64, pinned Rust 1.98.0 release builds and eight Rayon workers.
The serialization input was a real 128 MiB Radelta image made from uniform
uint16 noise. Metadata was 4 KiB of deterministic random bytes. Medians of seven
runs after warmup; output buffers were preallocated and reused.

| Operation | Before | After | Speedup |
|---|---:|---:|---:|
| C metadata size query | 51.871 ms | 0.030 ms | 1707x |
| C size query + metadata output | 117.863 ms | 12.351 ms | 9.54x |
| Replace existing 64 MiB file metadata with a small value | 30.830 ms | 19.136 ms | 1.61x |
| Python compress(), including compression and byte preparation | 1.247 s | 0.969 s | 1.29x |

The first two rows measure memory serialization, not storage throughput. The
file-metadata test uses warm OS caches without fsync, includes truncating the
old metadata tail, and excludes initial file creation. Python timing uses the
same 128 MiB image, three runs after warmup and the public compress() API.
Encoded output hashes matched before and after all comparisons.

Ordinary CLI file creation was essentially unchanged on a 128 MiB,
64 x 1024 x 1024 Poisson stack with the TIFF changes already applied:

| CLI path | Before | After |
|---|---:|---:|
| In-memory lossless | 0.3356 s | 0.3352 s |
| Streaming lossless, 16 MiB budget | 0.8141 s | 0.8111 s |
| Streaming lossy, 16 MiB budget | 0.5021 s | 0.4877 s |

These are end-to-end CLI times, medians of seven measured runs with alternating
binary order, warm caches and no fsync. They include TIFF reading and codec
work. The existing large pixel writes were already efficient; the strongest
improvement is in metadata preparation and avoiding redundant data movement.
Peak process memory in the Python comparison was dominated by compression and
was effectively unchanged (about 727 MiB), despite the eliminated copies.

## Reproduce

Save native release libraries from both source versions, then run:

```sh
python -m pip install numpy
python scripts/benchmark_radelta_output.py /path/to/before-library /path/to/after-library --repeats 7
```

To compare file metadata updates, copy the unchanged example into both source
trees and run it with a different, unused temporary filename in each:

```sh
cargo run --release --manifest-path native/Cargo.toml --example benchmark_metadata_update -- /path/to/unused-benchmark.rdlt
```

The example creates its own file and deletes it when successful. It refuses
an existing filename. For Python end-to-end timing, use the appropriate package
and native library, construct
`np.random.default_rng(41).integers(0, 65536, (16, 2048, 2048), dtype=np.uint16)`,
and time `radelta.compress(image, metadata=b"calibration" * 100)` after warmup.

## Validation

- 98 native tests in debug and release, using Rust 1.98.0.
- 12 Python array/file API tests, including native-buffer cleanup after errors.
- Formatting, Clippy with warnings denied, synchronized 0.8.1 release metadata.
- Exact byte comparisons for metadata add/replace/remove in all four in-memory
  containers, classic/BigTIFF metadata export, both streaming modes, short and
  interrupted output writes, and truncated/invalid input handling.
- File publication and failed-export destination preservation tests pass.
