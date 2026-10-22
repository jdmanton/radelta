# Decoded-unit TIFF export performance

This change exports TIFF planes directly from owned decoded volumes/chunks,
overlaps the first decode with TIFF directory preparation, and buffers ImageJ
directory prefixes up to 8 MiB to avoid small seek-triggered file writes.
It retains the validated TIFF serializer, ImageJ contiguous-pixel layout and
replace-on-success output publication. No allocator change is involved.

## Local comparison

Before is 0.8.4 with the initial streaming-pipeline and version patches.
Uploaded is the unmodified alternative implementation (including mimalloc).
All three executables used Rust 1.98.0 and their release profiles with eight
Rayon workers, bound to the same eight available CPUs on Linux. Each result
is the median of seven complete CLI conversions after one warm-up, alternating
executable order. Filesystem caches were warm; no other tests ran alongside.
Times include opening/indexing, metadata, pixels, TIFF writes and publication,
and exclude fsync. They are not durable-storage throughput measurements.

All decoders received the exact same Radelta archive for each row. The default
working allowance was 2048 MiB. These are synthetic fixtures; the results do
not establish a ranking on Windows, cold filesystems or network acquisitions.

| Dataset | Before (ms) | Uploaded (ms) | After (ms) | Speedup over before |
| --- | ---: | ---: | ---: | ---: |
| Time series | 75.9 | 44.0 | 42.6 | 1.78× |
| Deep stack | 294.4 | 287.6 | 266.3 | 1.11× |
| Deep stack, Deflate-source metadata | 300.9 | 263.8 | 262.3 | 1.15× |
| Multichannel ImageJ | 59.8 | 49.2 | 48.7 | 1.23× |
| Multichannel OME output | 63.6 | 49.2 | 45.5 | 1.40× |

The new exporter broadly matches the upload on these files. Small differences
between them should be treated cautiously given host timing variation. The two
deep-stack inputs contain the same pixels and chunk layout but different saved
TIFF metadata; the Deflate label describes the original TIFF, not Radelta data
being decompressed by a different codec during export.

Peak process RSS is the maximum over those seven runs, measured using wait4
in the executed CLI process, excluding the Python measurement harness:

| Dataset | Before (MiB) | Uploaded (MiB) | After (MiB) |
| --- | ---: | ---: | ---: |
| Time series | 58.6 | 66.6 | 14.5 |
| Deep stack | 135.4 | 122.5 | 114.7 |
| Deep stack, Deflate-source metadata | 179.0 | 118.6 | 108.5 |
| Multichannel ImageJ | 58.9 | 52.4 | 20.7 |
| Multichannel OME output | 59.3 | 48.6 | 21.3 |

## Smaller working allowance

Both versions below used `decode --memory-mib 64`. Large-unit and over-budget
interleaved layouts use direct cached plane reads. In particular the deep-stack
rows take the direct path in both versions; they do not demonstrate a pipeline
speedup. Their 3–7% timing differences are reported rather than omitted.

| Dataset | Before (ms) | After (ms) | Before RSS (MiB) | After RSS (MiB) |
| --- | ---: | ---: | ---: | ---: |
| Time series | 72.7 | 46.9 | 43.6 | 14.5 |
| Deep stack | 261.4 | 279.3 | 26.9 | 26.8 |
| Deep stack, Deflate-source metadata | 261.1 | 270.5 | 26.8 | 26.8 |
| Multichannel ImageJ | 51.7 | 49.2 | 41.0 | 20.6 |
| Multichannel OME output | 54.9 | 48.2 | 34.3 | 17.4 |

The unit planner reserves four times the raw unit size plus compressed bytes
until that unit's last output page. A queue-count limit also bounds small-unit
lookahead. This is an approximate working estimate, not a hard RSS cap: models,
allocator behavior, TIFF metadata and the optional 8 MiB header buffer are
additional. The direct reader still needs at least one whole stored unit.

## Fixtures and validation

- Time series: `T=128,C=1,Z=1,Y=256,X=256`, 16 MiB of uint16 pixels.
- Deep stack: `T=2,C=1,Z=128,Y=512,X=512`, 128 MiB, stored as 32-plane chunks.
- Multichannel: the time-series pixels reshaped to `T=2,Z=16,C=4,Y=256,X=256`
  and encoded canonically, restoring ImageJ TZC order for the ImageJ test.
- OME output: the multichannel archive decoded with `--page-order TCZ`,
  generating OME metadata and archiving the original TIFF metadata.

Every output pixel matched its source (accounting for explicit page reordering).
All ten before/after TIFF comparisons, across five layouts and two memory
allowances, were byte-identical. Each uploaded-code output was also checked
pixel by pixel. The archives are the same seeded Poisson fixtures described in
[the initial pipeline measurements](STREAM_PIPELINE_PERFORMANCE.md).

Native regression coverage includes all six Radelta container variants in
lossless/lossy modes, partial final chunks, reverse and interleaved page orders,
one-worker scheduling and cleanup, late I/O errors preserving an existing
destination, ImageJ colour maps, and directory prefixes below/above the memory
buffer threshold. Existing large-ImageJ header tests check classic-TIFF address
limits without allocating multi-gigabyte pixel arrays.

For acquisition comparisons, use one common archive, fix `RAYON_NUM_THREADS`,
warm up each executable once and alternate at least seven conversions of:

```bash
radelta decode input.rdlt restored.tif --memory-mib 2048
radelta decode input.rdlt reordered.tif --page-order TCZ --memory-mib 2048
```

Report median complete-process time and peak RSS; verify pixels and metadata
outside the timed region. Use the same storage and memory allowance for each
executable. The README codec benchmarks exclude this TIFF export path.
