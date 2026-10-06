# Radelta command-line interface

The executable is named `radelta` (`radelta.exe` on Windows). It reads 16-bit grayscale TIFF data and writes Radelta files, or decodes Radelta files back to TIFF/BigTIFF.

## Commands

```text
radelta encode INPUT.tif OUTPUT.rdlt [options]
radelta encode-lossy INPUT.tif OUTPUT.rdlt --offset-adu O (--gain-e-per-adu G | --gain-adu-per-e G) [options]
radelta decode INPUT.rdlt OUTPUT.tif [--page-order TCZ]
radelta benchmark INPUT.tif [--repeats N] [options]
radelta benchmark-lossy INPUT.tif --offset-adu O (--gain-e-per-adu G | --gain-adu-per-e G) [--repeats N] [options]
```

Running `radelta` without sufficient arguments prints the command syntax.

All commands accept `--metadata-limit-mib N` (default **1024 MiB**). This controls
both metadata extraction/writing and reading, including TIFF tag values. It is
separate from the streaming image-buffer budget (`--memory-mib`). Zero rejects
nonempty metadata; values must be nonnegative whole MiB.

```bash
radelta encode input.tif output.rdlt --metadata-limit-mib 2048
radelta decode output.rdlt restored.tif --metadata-limit-mib 2048
```

The setting applies to that invocation and is not saved in the file.

## Examples

Lossless compression:

```bash
radelta encode stack.tif stack.rdlt
```

Lossless decompression:

```bash
radelta decode stack.rdlt restored.tif
```

Camera-calibrated lossy compression:

```bash
radelta encode-lossy input.tif output.rdlt \
  --offset-adu 100 \
  --gain-e-per-adu 0.46 \
  --noise-step 2
```

## Measuring codec performance

```bash
radelta benchmark input.tif --repeats 7
radelta benchmark-lossy input.tif --offset-adu 0 --gain-e-per-adu 0.46 --noise-step 2 --repeats 7
```

Both commands load the dataset once, warm up each operation, and report median
encode/decode times (three measured runs by default). TIFF I/O and verification
are excluded. Encoding includes its allocations; decoding reuses a caller-owned
output buffer, matching the native C API. The commands verify lossless pixels
exactly and report lossy reconstruction errors. They create no output files.
Compression sizes include the multidimensional container header.

Codec and dataset layout options apply. Benchmarks run in memory;
`--stream` and `--memory-mib` are rejected. Use release builds and the same
input, calibration, context, block depth, precision, and worker count for
comparisons. The commands print settings and worker count; `RAYON_NUM_THREADS`
can fix the worker count before launching the process.

## Codec options

| Option | Meaning | Default |
| --- | --- | --- |
| `--block-depth N` | Z planes per independent codec block | `4` |
| `--context mean\|mean-signs\|signed3\|signs` | q-context model | `mean` |
| `--scale-bits N` | rANS probability precision | `10` |

Supported `scale-bits` values are 8–11.

The `mean` context retains fractional neighbour means with a compact table and
uses both left and upper neighbours on the first plane of each block.
`mean-signs` additionally distinguishes the ordering of those neighbours,
using larger tables that can improve compression ratio at a speed cost.
`signs` and `signed3` remain available for comparing compression on different
kinds of data.

## Dataset layout

Radelta uses contiguous `T,C,Z,Y,X` order with X fastest. The CLI normally infers T/C/Z dimensions from TIFF metadata.

Supported metadata include:

- single-file OME-TIFF (`SizeT`, `SizeC`, `SizeZ`, `DimensionOrder`, and local `TiffData` mappings);
- ImageJ/Fiji hyperstack metadata (`channels`, `slices`, `frames`); and
- ordinary TIFF stacks, treated as `T=1`, `C=1`, `Z=number_of_pages`.

Manual overrides are available when metadata are absent or incorrect:

| Option | Meaning |
| --- | --- |
| `--t N` | number of timepoints |
| `--c N` | number of channels |
| `--z N` | number of Z planes |
| `--page-order TCZ` | TIFF page axes, slowest-changing to fastest-changing |
| `--ignore-metadata` | ignore OME/ImageJ dimensional metadata |

For example:

```bash
radelta encode input.tif output.rdlt \
  --ignore-metadata --t 20 --c 4 --z 50 --page-order TZC
```

`T*C*Z` must equal the number of logical TIFF planes.

## Bounded-memory encoding

Large datasets can be written as streaming `RDS3`/`RQS3` files. Streaming is selected automatically when the estimated working set exceeds the memory budget, or can be forced with `--stream`.

| Option | Meaning | Default |
| --- | --- | --- |
| `--memory-mib N` | approximate encoding RAM budget | `2048` MiB |
| `--stream` | force bounded-memory streaming | off |

## Lossy calibration

The calibrated lossy transform is

```text
electrons = max((ADU - offset_adu) * gain_e_per_adu, 0)
z         = 2 * sqrt(electrons)
q         = round(z / noise_step)
```

Specify gain as either electrons/ADU:

```text
--gain-e-per-adu G
```

or ADU/electron:

```text
--gain-adu-per-e G
```

but not both. The encoded `q` value must fit in 8 bits.

## TIFF notes

- Input pages must contain grayscale `uint16` samples.
- X and Y may be rectangular but must be consistent throughout a dataset.
- Single-file OME-TIFF is supported; multi-file OME datasets are not assembled automatically.
- Supported contiguous ImageJ/tifffile hyperstacks must be uncompressed, single-sample, 16-bit contiguous data.
- Decoding automatically uses BigTIFF when needed.

## Metadata preservation

`encode` and `encode-lossy` automatically preserve TIFF metadata, including
streaming mode. Descriptions are retained as bytes, alongside resolution/unit,
acquisition, per-plane, private binary, and nested EXIF/GPS tags. Metadata is
compressed separately with LZ4; image quantization never changes it.

By default, `decode` restores the source TIFF page order and descriptions,
including explicit OME plane mappings and ImageJ layout. Streaming files use
indexed plane reads to restore that order with a chunk cache. TIFF storage
offsets and metadata-directory pointers are rebuilt for the output file.

An explicit decode `--page-order`, or encode dimension overrides inconsistent
with the original description, can require a new layout description. In that
case, the output receives valid OME layout metadata and the complete original
metadata payload is retained in private TIFF BYTE tag **65000**. Opaque metadata
supplied by a non-TIFF application is also saved in tag 65000. Other per-plane
tags follow their corresponding pixels. Without source metadata, output uses
canonical TCZ order and a generated OME description.

TIFF conversion covers the main grayscale image stack; it does not copy
thumbnail/pyramid image payloads. Unknown private values are preserved, but
vendor-specific pointers hidden inside opaque binary values cannot be relocated.
See [metadata encoding](FORMAT.md#opaque-metadata-all-containers) for details.

Codec benchmarks exclude metadata extraction and compression, so their sizes
and timings describe the image codec. Actual encoded files include metadata.

## Streaming progress

Lossless RDS3 and lossy RQS3 encoding report the chunk layout before reading the
first chunk. Progress reports show completed chunks, percentage, raw data
processed, elapsed time and throughput including TIFF I/O. Reports are emitted
after a completed chunk when at least one second has elapsed, and always after
the final chunk. A long chunk may therefore take more than one second between
updates. TIFF directory scans report every two seconds while advancing through
the directories. A separate message marks metadata saving after the pixel
chunks have been written.

The TIFF layout found during the initial inspection is reused for encoding,
avoiding a second directory scan. Progress is enabled automatically for forced
(`--stream`) and automatically selected out-of-core encoding.
