# Streaming pipeline measurements

The pipeline helps most when TIFF reading or many shallow, independently
encoded volumes leave CPU workers idle. Deep chunks already parallelize within
the codec, so they show smaller gains. Sequential TIFF export keeps direct reads
when a read-ahead batch cannot span multiple stored units.

## Local comparison

Baseline: `0e40300` (0.8.2, including the ImageJ and batched-read fixes).
Both executables were built with Rust 1.98.0, the repository's release profile,
and the same dependencies. These are Linux, local warm-filesystem measurements,
with `RAYON_NUM_THREADS=8`. Each cell is the median of seven complete CLI runs
after one warm-up, alternating executable order. Timings include process startup,
TIFF inspection, pixels, metadata and output publication; they exclude `fsync`
and do not measure durable storage throughput. No other test suite ran alongside
the measurements.

| Input | Operation | Before (ms) | After (ms) | Speedup |
| --- | --- | ---: | ---: | ---: |
| Time series | Lossless encode | 185.7 | 72.1 | 2.57× |
| Time series | Lossy encode | 100.2 | 35.0 | 2.86× |
| Time series | TIFF export | 169.3 | 105.8 | 1.60× |
| Deep stack | Lossless encode | 351.7 | 341.3 | 1.03× |
| Deep stack | Lossy encode | 207.4 | 190.9 | 1.09× |
| Deep stack | TIFF export | 342.2 | 271.1 | 1.26× |
| Deflate deep stack | Lossless encode | 1004.0 | 794.0 | 1.26× |
| Deflate deep stack | Lossy encode | 830.4 | 779.6 | 1.07× |
| Deflate deep stack | TIFF export | 270.6 | 281.0 | 0.96× |

The two deep-stack TIFF exports decode the same pixels and have the same stored
chunk layout; their metadata differ. Their timings illustrate run-to-run noise.
Treat deep-stack export as approximately unchanged rather than inferring a
general speedup from the faster row. Earlier runs also varied substantially in
absolute time. These results do not predict Windows, network storage or cold-cache
performance, and do not change the in-memory codec benchmark itself.

The time series has `T=128,C=1,Z=1,Y=256,X=256` (16 MiB of uint16 pixels).
The deep stacks have `T=2,C=1,Z=128,Y=512,X=512` (128 MiB).
The fixtures contain seeded Poisson samples over a smooth spatial signal plus
an offset of 100 ADU. Inputs use ImageJ metadata; the first two are uncompressed,
and the third stores the same deep-stack pixels with Deflate compression.

Both encoders used `--stream --memory-mib 64` and default codec options.
Lossy encoding additionally used `--offset-adu 100 --gain-e-per-adu 0.46
--noise-step 2`. Both decoders received the same baseline-produced lossless
archive. The new decoder used `--memory-mib 64`; the baseline has no decode
budget option and uses its original adaptive cache.

## Memory and compatibility

Peak RSS is the maximum over those seven runs, measured in the executed CLI
process with `wait4`, excluding the measurement harness:

| Input / operation | Before (MiB) | After (MiB) |
| --- | ---: | ---: |
| Time series / lossless encode | 3.2 | 9.3 |
| Time series / lossy encode | 2.7 | 5.8 |
| Time series / TIFF export | 20.6 | 43.5 |
| Deep stack / lossless encode | 40.1 | 70.8 |
| Deep stack / lossy encode | 28.7 | 54.7 |
| Deep stack / TIFF export | 26.6 | 27.0 |
| Deflate deep stack / lossless encode | 40.1 | 57.4 |
| Deflate deep stack / lossy encode | 28.9 | 46.8 |
| Deflate deep stack / TIFF export | 26.6 | 26.8 |

Overlapping operations need additional buffers. Queue lengths, chunk planning
and buffer pools are bounded; `--memory-mib` remains an approximate working
allowance rather than a hard RSS limit. Codec models, scratch, TIFF directories
and metadata are additional. A minimum stored decode unit can exceed the
allowance. No allocator change is involved.

All six encoded benchmark archives (three inputs, two modes) were byte-identical
to their baseline counterparts. Every pixel was also compared through both
cross-version paths: old reader/new archive and new reader/old archive. Lossless
pixels matched the source exactly, and lossy reconstructions matched each other.
Other worker counts or memory allowances can select different chunk boundaries;
the file format and C ABI are unchanged.

## Repeating the comparison on acquisition data

Build a baseline executable from `0e40300` in a separate checkout and build the
candidate with `cargo build --manifest-path native/Cargo.toml --release`.
Set the same `RAYON_NUM_THREADS` for both executables. Warm up each command once,
then alternate the executables for at least seven timed runs of:

```bash
radelta encode input.tif output.rdlt --stream --memory-mib 64
radelta encode-lossy input.tif output-lossy.rdlt --stream --memory-mib 64 \
  --offset-adu 100 --gain-e-per-adu 0.46 --noise-step 2
radelta decode baseline.rdlt restored.tif
# Candidate decode adds: --memory-mib 64
```

Use a single baseline archive for both decoders, and verify reconstructed pixels
outside the timed region. Report median wall time, peak process RSS, encoded size,
CPU workers and storage type. Test representative shallow and deep acquisitions;
the README's in-memory `benchmark` commands exclude the I/O pipeline.
