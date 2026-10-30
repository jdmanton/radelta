# Radelta command-line interface

The executable is named `radelta` (`radelta.exe` on Windows). It reads unsigned 8-bit or 16-bit grayscale TIFF data and writes Radelta files, or decodes Radelta files back to TIFF/BigTIFF.

## Commands

```text
radelta encode INPUT.tif [OUTPUT.rdlt] [options]
radelta encode-lossy INPUT.tif [OUTPUT.rdlt] --offset-adu O (--gain-e-per-adu G | --gain-adu-per-e G) [options]
radelta decode INPUT.rdlt [OUTPUT.tif] [--page-order TCZ] [--memory-mib N]
radelta benchmark INPUT.tif [--repeats N] [options]
radelta benchmark-lossy INPUT.tif --offset-adu O (--gain-e-per-adu G | --gain-adu-per-e G) [--repeats N] [options]
```

Use `radelta --help` for the command list and `radelta encode --help` (or
`radelta help encode`) for command-specific help. Help and `--version` exit
successfully. Running without a command prints help and exits with status 2.

Options may appear before or after filenames; `--option=value` is supported.
Use `-o OUTPUT` or `--output OUTPUT` instead of a positional output path, but
not both. Quote filenames containing spaces. Use `--` before a filename that
begins with `-`, for example `radelta encode -- -stack.tif`.

When output is omitted, `encode` changes the input extension to `.rdlt`,
`encode-lossy` to `.lossy.rdlt`, and `decode` to `.tif`. If a browser-created
filename already ends in `.tif.rdlt` or `.tiff.rdlt`, decoding removes just the
final `.rdlt`. Input files are always retained. Existing output files, including
symlinks, are rejected unless `--force` (or `-f`) is supplied. Input/output
aliases are rejected even with `--force`.

```bash
radelta encode stack.tif                   # writes stack.rdlt
radelta encode --threads 8 stack.tif -o archive.rdlt
radelta decode archive.rdlt restored.tif
radelta encode stack.tif --force --quiet   # deliberately replace stack.rdlt
```

Global options can appear before or after a command:

| Option | Meaning | Default |
| --- | --- | --- |
| `--threads N`, `-j N` | positive number of codec workers | `RAYON_NUM_THREADS`, otherwise available CPUs |
| `--quiet`, `-q` | suppress progress and success messages; errors remain visible | off |
| `--force`, `-f` | allow output replacement for conversions | off |
| `--metadata-limit-mib N` | metadata allowance, separate from image buffers | 1024 MiB |
| `--version`, `-V` | print release version and exit | |

An explicit `--threads` takes precedence over `RAYON_NUM_THREADS`. Benchmarks
still print the actual worker count. Output parent directories must already
exist. `--quiet` is useful in scripts; the CLI never prompts for input.

Exit statuses are **0** for success or requested help/version, **2** for command
syntax or invalid option values, and **1** for I/O, unsupported data or codec
errors. Invalid arguments are rejected before opening input files. Unknown
options name the error and may suggest a close match. Numeric parameters must
be in range; dimensions, threads, memory and repeats must be positive; camera
offset must be finite, and gain/noise step must also be greater than zero.
The two gain conventions are mutually exclusive.

Conversions reject input/output aliases, including symlinks and hard links.
They write a sibling temporary file and publish the destination only after
pixels and metadata are complete. Without `--force`, publication fails if
another process has created the destination while encoding or decoding was
running. Atomic no-replace publication uses a hard link on the output
filesystem; if hard links are unsupported it fails with an error rather than
falling back to replacement. Use `--force` only when replacement is intended. Failures preserve any previous output.
Allow disk space for the temporary output alongside an existing destination.

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
can fix the worker count before launching the process, or pass `--threads N`.

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
using larger tables for finer conditioning at a speed cost.
`signs` and `signed3` remain available for comparing compression on different
kinds of data.

## Dataset layout

Radelta uses contiguous `T,C,Z,Y,X` order with X fastest. The CLI normally infers T/C/Z dimensions from TIFF metadata.

Supported metadata include:

- OME-TIFF, including a single member of a multi-file acquisition (`SizeT`, `SizeC`, `SizeZ`, `DimensionOrder`, and local `TiffData` mappings);
- ImageJ/Fiji hyperstack metadata (`channels`, `slices`, `frames`); and
- ordinary TIFF stacks, treated as `T=1`, `C=1`, `Z=number_of_pages`; and
- planar multi-sample pages (see below), treated as `T=1`,
  `C=SamplesPerPixel`, `Z=number_of_pages`.

### Multi-sample pages

A TIFF page can store several values per pixel (`SamplesPerPixel` > 1). In
planar layout (`PlanarConfiguration=2`) each sample is a complete XY plane, and
the CLI reads every sample plane as a separate channel. This includes stacks
that tifffile writes as planar RGB/RGBA pages when the leading array axis has
length 3 or 4. Every page must have the same number of samples.

Logical planes are numbered page by page, with the sample index varying
fastest. Without overrides, a 10-page file with 3 samples per page becomes
`T=1, C=3, Z=10`. `--t`, `--c`, `--z` and `--page-order` apply to those logical
planes, so `--t 5 --z 2` reads the same file as 5 timepoints of 2 planes with
3 channels each. Decoding writes one single-sample page per plane in the
original plane order, with a generated OME description; the source metadata is
archived in tag 65000.

Interleaved multi-sample pages (`PlanarConfiguration=1`, the usual RGB layout)
are rejected. A planar multi-sample file that also has OME or ImageJ
dimensional metadata is rejected because the two descriptions can conflict;
add `--ignore-metadata` to read its samples as channels, with dimension
overrides if needed.

For an acquisition split across channel or timepoint files, use the usual
command on each file:

```bash
radelta encode acquisition_C00.ome.tif acquisition_C00.rdlt --stream --memory-mib 10000
```

OME dimensions may describe the whole acquisition. Radelta identifies local
`TiffData` entries by the enclosing OME UUID, or by their `UUID FileName` when
a root UUID is absent. Entries without a UUID refer to the supplied TIFF.
The `.rdlt` contains only this file's complete contiguous TCZ subset, with
local axes starting at zero; the CLI reports its original acquisition ranges.
For example, one 1599-plane channel from a four-channel acquisition is stored
as `T=1, C=1, Z=1599`. Sibling pixel files are not opened.

Default decoding restores the original OME-XML, descriptive TIFF tags and IFD
order, including acquisition coordinates and external file references. Those
references still describe a multi-file acquisition. To deliberately produce a
local OME layout, `decode --page-order TCZ` regenerates the description and
archives the original metadata in the private TIFF tag as described below.
Missing, duplicate or ambiguous plane mappings, nonrectangular subsets and
coordinate gaps are rejected; missing planes are never filled with zeros.
External `BinaryOnly` metadata and multiple OME image series require conversion
to a single locally described image before encoding.

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
| `--memory-mib N` | approximate streaming RAM budget (encode/decode) | `2048` MiB |
| `--stream` | force bounded-memory streaming | off |

Positive memory allowances are used as given. This remains a working-buffer estimate, so a single oversized chunk or
volume and metadata can exceed it.

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

- Input pages must contain `uint8` or `uint16` samples: one per pixel, or several in
  planar layout, which are read as channels.
- X and Y may be rectangular but must be consistent throughout a dataset.
- A single member of a multi-file OME-TIFF acquisition is supported when its
  mapped planes form a complete contiguous TCZ subset. Other files are not
  assembled automatically.
- Supported contiguous ImageJ/tifffile hyperstacks must be uncompressed, single-sample, 8-bit or 16-bit contiguous data.
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

TIFF conversion preserves the main grayscale image stack and its uint8/uint16,
single-sample grayscale SubIFD pyramid levels, including nested levels. Each
stored reduced-resolution image is compressed losslessly and restored beneath
its original parent plane; explicit page reordering moves its pyramid with it.
The source level pixels are retained even when the main image is encoded lossy.
Strips, tiles, and input compression may change on output; pixel values,
SubIFD relationships, descriptive tags and calibration are preserved.

Pyramid images use the TIFF metadata payload and count towards
`--metadata-limit-mib` (default 1024 MiB); this holds their compressed Radelta
streams, not their raw pixels. Increase the limit on both encode and decode for
larger pyramids. Metadata is loaded in memory, and auxiliary decoding requires
one complete level plane plus codec workspace. This is separate from the main
streaming image-buffer allowance. Unsupported SubIFD sample types, linked image
chains within SubIFDs, and SubIFDs on ImageJ or planar-multisample primary pages
are rejected instead of being discarded. Additional top-level image series or
thumbnails with different XY dimensions are unsupported.

Unknown private values are preserved, but
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

## Streaming pipeline

Streaming encode overlaps TIFF reading, compression and ordered Radelta output.
It reuses one or two TIFF plane buffers and a bounded set of chunk buffers.
Compression jobs share the existing Rayon pool; at most eight chunks are
submitted ahead. The chunk planner divides the approximate working allowance
between encode slots, using four raw-chunk sizes per slot as its working-set
estimate and recycling completed input buffers into the feeder.
It retains block alignment where affordable and falls back to serial chunk
processing when the minimum pipeline would exceed that allowance.
Streaming chunk boundaries follow the CPU workers and memory allowance; files
remain readable by existing Radelta readers.

The same streaming writer is used by the CLI, ImageJ and Python. Input planes
are copied before a write call returns. Compression errors can surface on a
later write or at finish, so callers must check finish before treating a file
as saved. Closing an unfinished or failed writer joins its queued work and
removes the temporary output.

TIFF export reads ahead in stored volume/chunk units, borrowing each output
plane directly from its decoded unit. There are no separate output batch
buffers or retention-cache copies in this path. `decode --memory-mib N`
controls the approximate allowance (default 2048 MiB). The exporter charges
each resident unit against that allowance until its last output page, including
interleaved channel layouts. Its conservative estimate reserves four times the
decoded pixel size plus the compressed payload for each unit, allowing for
codec scratch and allocator reuse.
It also limits queued work using estimated codec-block counts and bounds the number
of resident units, so small chunks cannot create an unlimited task queue.
Decoding starts while TIFF directories are being prepared. ImageJ directory
prefixes up to 8 MiB are serialized in memory before one buffered write, avoiding
small seek-triggered flushes; larger prefixes retain the file-backed path.

If even one unit's working estimate exceeds the allowance, or interleaved
output requires more simultaneously retained units than it permits, export
uses direct plane reads with an adaptive cache
limited to one quarter of the allowance, capped at 2 GiB. The default 2048 MiB
allowance permits a 512 MiB cache on this fallback path; `--memory-mib 8192`
permits the full 2 GiB cache. Evicted channel data may be decoded again;
increasing the allowance can enable the faster unit path.

These are working-buffer allowances, not hard process-RSS limits. Codec models,
scratch, TIFF directories and metadata also use memory, and at least one whole
stored Radelta volume/chunk must fit for decoding. One unit larger than the
allowance can therefore proceed, as with the file reader API.

See [implementation architecture](ARCHITECTURE.md) for buffer ownership and
memory planning, and [benchmark tools](BENCHMARKS.md) for timing scopes.

## HDF5 conversion

The companion `radelta-hdf5` command supports TIFF → HDF5, HDF5 → TIFF and
HDF5 dataset recompression. See [HDF5 build and usage](../hdf5/README.md).
It is installed separately and uses the optional HDF5 compression filter.
