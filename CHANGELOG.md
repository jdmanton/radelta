# Changelog

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
