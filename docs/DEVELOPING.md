# Building and contributing

Run the commands below from the repository root unless stated otherwise.
The project contains the Rust codec and CLI, Python adapters, an ImageJ/Fiji
plugin, and HDF5 and OME-Zarr integrations.

## Requirements

- The Rust toolchain in [`rust-toolchain.toml`](../rust-toolchain.toml), installed
  through rustup, and a platform C linker.
- Python 3.9+ for array and HDF5 packages, or Python 3.11+ for OME-Zarr.
- JDK 17 and Maven for the ImageJ/Fiji plugin.
- CMake 3.18+, a C compiler and HDF5 development files for the HDF5 SDK filter.

Rustup reads the pinned toolchain from the checkout. Dependency versions are
recorded in `native/Cargo.lock`; use `--locked` for release builds.

## Native codec and CLI

```sh
cargo fmt --manifest-path native/Cargo.toml --all -- --check
cargo clippy --manifest-path native/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path native/Cargo.toml --all-targets
cargo test --manifest-path native/Cargo.toml --release
cargo build --manifest-path native/Cargo.toml --release --locked
```

The CLI is `native/target/release/radelta` (`radelta.exe` on Windows).
Cargo builds the library under a target-specific name:

| Platform | Cargo library filename | Release filename |
| --- | --- | --- |
| Windows | `radelta_native.dll` | `radelta.dll` |
| Linux | `libradelta_native.so` | `libradelta.so` |
| macOS | `libradelta_native.dylib` | `libradelta.dylib` |

The packaging scripts stage these files under their release names. Use a
release build when measuring throughput; see [benchmark usage](BENCHMARKS.md).

## Python adapters

Build the native library, then install the packages you need:

```sh
python -m pip install -e ./python
python -m pip install -e ./zarr/python
```

The native loader can find a source checkout's build. Set
`RADELTA_NATIVE_LIBRARY` to the absolute library path to select it explicitly.
For example, on Linux:

```sh
export RADELTA_NATIVE_LIBRARY="$PWD/native/target/release/libradelta_native.so"
python -m unittest discover -s python/tests -v
python -m unittest discover -s zarr/tests -v
```

In PowerShell, use `$env:RADELTA_NATIVE_LIBRARY` and the Windows library name.
HDF5 array access additionally requires the HDF5 adapter and filter described
below.

The scripts test suite includes optional TIFF integration checks. To run those
checks, install their image dependencies and select a built CLI:

```sh
python -m pip install numpy tifffile imagecodecs
export RADELTA_CLI="$PWD/native/target/release/radelta"
python -m unittest discover -s scripts/tests -v
```

Without the image dependencies or CLI, the TIFF integration checks are skipped;
the other script tests still run. Path tests exercise Unicode filenames on all
platforms and arbitrary non-UTF-8 filenames on Linux.

## ImageJ/Fiji

After building the native library, set `RADELTA_NATIVE_LIBRARY` as above and run:

```sh
mvn --batch-mode -f imagej-plugin/pom.xml package
```

The tests run headlessly and use the CLI beside the native library. Maven
downloads the declared Java dependencies. The result is
`imagej-plugin/target/radelta-imagej-<version>.jar`.

Release packaging stages the five platform/architecture pairs and the embedded
HDF5 filters before building the universal JAR. See
[Fiji installation](../imagej-plugin/FIJI_BUNDLE.md) and
[`scripts/stage_imagej_natives.py`](../scripts/stage_imagej_natives.py).

## HDF5

Build the filter against the HDF5 runtime used by its reader:

```sh
cmake -S hdf5 -B hdf5/build -DCMAKE_BUILD_TYPE=Release
cmake --build hdf5/build --config Release
ctest --test-dir hdf5/build -C Release --output-on-failure
```

Install the Python helper with `python -m pip install -e ./hdf5/python` and set
`RADELTA_HDF5_PLUGIN_PATH` to the built plugin directory before running
`python -m unittest discover -s hdf5/tests -v`. h5py and the SDK filter must use
the same HDF5 installation. The embedded filter used in packaged Python wheels
and Fiji does not link a second HDF5 runtime. See the
[HDF5 build guide](../hdf5/README.md#build-and-install) for platform details and
[HDF5 integration in Fiji](../hdf5/FIJI.md#integration-tests) for reader checks.

## Release packaging

[`VERSION`](../VERSION) is the release version. The native manifest and lockfile,
Python package/runtime versions, optional dependency pins, ImageJ POM, HDF5
CMake version and filter names must agree. Validate them with:

```sh
python scripts/check_release_metadata.py
```

`scripts/package_native.py` stages the CLI, C library, header and documentation.
`scripts/package_python_wheel.py` builds array/HDF5 wheels, and
`scripts/package_zarr_wheel.py` builds the OME-Zarr wheel. Platform repair tools
check dependencies and platform tags. Installed-wheel tests run outside the
checkout with native-library overrides removed.

The [release workflow](../.github/workflows/release.yml) requires a tag matching
`v<VERSION>`. It builds the native, ImageJ and HDF5 assets, runs package checks,
creates SHA-256 checksums and publishes the GitHub release. Manual HDF5 and
OME-Zarr workflow runs provide build artifacts without publishing a release.

## Contributions

Use the [architecture guide](ARCHITECTURE.md) to locate the relevant component
and the [format specification](FORMAT.md) for serialized data. Document public
behaviour, memory ownership, format fields and limits where users or callers
need them. Keep user documentation focused on supported operations and actual
requirements.

Run the checks relevant to a change. Pixel tests should compare against an
independent reference, exercise dimensions and boundaries, and check failure
handling. File-writing tests should cover source aliases, incomplete writes and
output preservation. Performance measurements belong in explicit benchmark
runs, with the environment and settings recorded alongside the results.
