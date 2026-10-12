# Changelog

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
