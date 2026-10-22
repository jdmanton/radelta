# Changelog

## 0.8.4

- Streaming TIFF/Radelta pipelines overlap reading, compression and ordered
  output, with bounded queues, reusable buffers and joined workers on failures;
- TIFF export uses bounded batch read-ahead and accepts `decode --memory-mib`.

## 0.8.3

- Raise the TIFF decoder buffer allowance for large XY planes.

## 0.8.2

- Faster TIFF and Radelta file I/O, codec paths and interleaved plane reads;
- ImageJ TIFF export keeps pixel planes contiguous and preserves colour maps,
  with valid TIFF directories for other readers;
- Bounded batched plane reads decode independent volumes/chunks in parallel,
  used automatically by ImageJ full-memory loading and Python full-plane reads.

## 0.8.1

- **fix:** TIFF pages with several planar (`PlanarConfiguration=2`) samples
  per pixel were encoded from their first sample only, silently discarding the
  rest. Each sample plane is now read as a channel, in both in-memory and
  streaming encoding; a 10-page, 3-sample file becomes `T=1, C=3, Z=10`.
  tifffile writes such pages for 3- or 4-plane uint16 stacks it treats as
  RGB/RGBA. Decoding writes single-sample pages in the original plane order;
- interleaved multi-sample pages are still rejected, now with a message that
  names the layout; pages with differing `SamplesPerPixel` are rejected;
- planar multi-sample files that also carry OME or ImageJ dimensional metadata
  are rejected rather than guessed at; `--ignore-metadata` reads their samples
  as channels.

## 0.7.6

- `radelta-hdf5 bdv` exports TIFF stacks or HDF5 datasets to tiled,
  multiresolution BigDataViewer XML/HDF5, preserving channels, timepoints and
  supported spatial calibration without loading the whole volume;
- automatic pyramids account for anisotropic voxels; lossy exports quantise
  only the full-resolution level and compress coarser levels losslessly;
- complete XML/HDF5 exports are published atomically through the XML file;
  failed exports preserve existing results;
- headless CI checks exported pixels, calibration and mipmap transforms with
  Fiji's released BigDataViewer loader in both lossless and lossy modes.

## 0.7.5

- dragging a Radelta HDF5 file onto Fiji, or opening it with File > Open, now
  opens it: one dataset with an `axes` attribute opens directly, otherwise a
  dialog chooses the dataset and layout. Other HDF5 files still reach their
  own readers instead of falling through to Bio-Formats;
- the `.rdlt` and HDF5 hooks also work when Fiji's "SciJava I/O" opening
  option is enabled, which resolves paths to locations first;

## 0.7.4

- **Save as Radelta HDF5 (lossless/lossy)** in the ImageJ plugin, plus scriptable
  `RadeltaHdf5.save`/`saveLossy`: datasets follow the `radelta-hdf5` layout
  and attributes, with chosen chunk sizes, `element_size_um` calibration for
  Fiji's importer, nested dataset paths and replace-on-success writes; like
  `.rdlt` exports, saves refuse a virtual stack's source file and its hard
  links, and stop on unreadable planes;
- the bundled embedded filter now also encodes, when a dataset is created
  with all 17 filter parameters; it still makes no HDF5 API calls. Fiji
  identifies it by name and refuses other registered Radelta filters;
- CI writes through the plugin on every JHDF5 platform, checks pixels, chunks,
  filter parameters and attributes through JHDF5 and Fiji, re-reads every file
  with the SDK filter, and cross-checks SDK and embedded encoders natively;

## 0.7.3

- native CLI and streaming writers publish completed exports from sibling
  temporary files; failures and unfinished writers preserve existing outputs;
- CLI conversions and ImageJ virtual-stack exports reject source-file aliases,
  including hard links; failed ImageJ plane reads propagate instead of returning
  black pixels, and prevent export of corrupted images;
- file and in-memory readers validate record counts against available bytes
  before fallible index allocation; file opening also contains unexpected
  panics at the C boundary.

## 0.7.2

- **format change:** RDL1/RDLQ streams, and therefore every container that
  embeds them, carry a CRC-32 of the header and of each block. Corrupted
  payloads are reported as errors instead of decoding to wrong pixels. Blocks
  are verified in parallel with decoding. Files written by earlier versions
  are no longer readable;
- damaged dimension headers no longer abort the host process: file readers
  check every embedded payload header against its container when opening,
  decode into buffers sized from validated dimensions, and in-memory decoders
  report impossible allocations as errors (affects the CLI and ImageJ plugin);

## 0.7.1

- deterministic OME-Zarr concurrency tests for bounded reading and draining
  active writes after an error;
- standalone read-only HDF5 filter for Fiji's existing HDF5_Vibez/JHDF5 reader,
  with separate release downloads and headless importer integration tests;
- HDF5 decoder bundled into the existing ImageJ JAR with automatic startup
  registration, plus a complete Fiji installation ZIP with reader dependencies;

## 0.7.0

- Radelta codecs for Zarr v2 and v3, with checksummed chunk framing;
- OME-Zarr 0.4/0.5 TIFF import/export and multiscale recompression;
- optional XY mean pyramids with physical coordinate transforms;
- lossless label handling, partial access, sharding and native release packaging;
- bounded TIFF page caching and concurrent chunk writes for faster encoding;
- self-contained Python wheels for all five release platforms, with repaired
  dependencies, platform compatibility checks and isolated installation tests;
- corrected ImageJ native-library selection and macOS wheel architecture tags.

## 0.6.0

- optional HDF5 compression filter with lossless and calibrated-lossy uint16
  datasets, canonical multidimensional chunks and both byte orders;
- Python HDF5 dataset API and bounded-memory TIFF import/export and dataset
  recompression tools with progress reporting;
- one-pass allocating C encoder and matching buffer release function;
- HDF5 integration tests, Linux CI and five-platform online builds;
- downloadable HDF5 SDK/plugin packages and automatic release attachments;
- synchronised native, ImageJ and HDF5 package versions and Cargo.lock.

## 0.5.0

Initial public release of Radelta, including:

- lossless and camera-calibrated lossy compression;
- mean-based spatial prediction, with `mean-signs` for finer conditioning;
- packed entropy tables, exact reciprocal encoding, and direct lossless and lossy decoding;
- compact sparse probability models and format version 1 containers;
- lookup tables for calibrated lossy quantization and reconstruction;
- multidimensional and bounded-memory streaming containers;
- TIFF/OME-TIFF/ImageJ command-line support and repeatable codec benchmarks;
- opaque metadata with LZ4 compression, raw fallback, and checksum validation;
- a configurable metadata limit, defaulting to 1024 MiB;
- typed TIFF metadata, calibration, EXIF/GPS directories, and source plane order preservation;
- native C/Rust metadata APIs and an ImageJ/Fiji plugin that preserves metadata and calibration.
