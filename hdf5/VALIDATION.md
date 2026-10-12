# Validation

Base: upstream tag v0.5.1, commit
`91272117778a6e4d65d9160cce33912d4a6d3aa5`.

Tested on Linux x86-64 with Rust 1.98.0, GCC 13.3, CMake 4.4.4,
HDF5 1.10.10, h5py 3.10.0 and NumPy 1.26.4.

- CMake release plugin build: passed, with default codec LTO enabled.
- Native HDF5 plugin-loading / big-endian 5D / hyperslab test: passed.
- Python HDF5 and conversion integration: 11 test methods passed; the rank,
  byte-order and prediction-context test includes 32 parameter combinations.
- Existing Rust suite plus the new allocating C-API test: 64 tests passed.
- Rust formatting, Clippy with warnings denied, release metadata check,
  and git whitespace check: passed.

The supplied Rust toolchain in this execution environment failed to link the
Rust release *test harnesses* with thin LTO (undefined core/std symbols).
The full Rust release suite was run successfully with
`CARGO_PROFILE_RELEASE_LTO=false cargo test --release --locked`.
The production plugin itself built and passed its HDF5 tests with the repository's
usual thin-LTO configuration. No profile changes are included in the patch.

Windows/macOS build instructions and CMake support are provided, but those
platforms were not executed locally. Added CI exercises Linux with matching
system HDF5 and h5py libraries. No compression-throughput claim is made for the
HDF5 integration.

The default filter ID 65024 is private, not globally registered. HDF5 clients
must use the matching filter/HDF5 build; PyPI h5py wheels may bundle a different
HDF5 installation. See README.md for installation and current limitations.

## Online build workflow validation

The five-platform HDF5 workflow and modified release workflow passed Actionlint
1.7.9 checks. The Linux SDK build path was executed locally with the pinned
HDF5 1.14.6 source, followed by plugin build/install and ZIP generation.
The extracted ZIP passed `h5dump -V` and the native plugin-loading, 5D
big-endian round-trip and hyperslab test from a separate temporary directory,
with no LD_LIBRARY_PATH override. The existing 11 Python integration methods
were rerun successfully against the matching system HDF5 installation.

Windows, Linux ARM64 and both macOS variants are configured as independent
GitHub-hosted jobs; those online jobs have not been executed from this session.
