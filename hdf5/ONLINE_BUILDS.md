# Prebuilt Radelta HDF5 packages

The **HDF5 builds** GitHub Actions workflow builds these five platforms:

| Operating system | Architecture |
| --- | --- |
| Windows | x86-64 |
| Linux | x86-64 |
| Linux | ARM64 |
| macOS | Intel x86-64 |
| macOS | Apple Silicon ARM64 |

It runs for changes to HDF5/native build inputs on branches and pull requests,
can be run manually with **Actions → HDF5 builds → Run workflow**, and is
called by the existing tag-release workflow. Manual runs require the workflow
to be present on the repository's default branch. Download the matching
`hdf5-<platform>-<architecture>` artifact from a completed Actions run; inside
it is `radelta-hdf5-<version>-<platform>-<architecture>.zip`. Tagged releases
also attach these five ZIPs alongside the existing native and ImageJ packages.
No release is published by a manual HDF5 build alone.

Each job builds shared HDF5 1.14.6 from pinned upstream commit
`7bf340440909d468dbb3cf41f0ea0d87f5050cea`, then builds the filter with Rust
1.98.0. The archive includes the matching HDF5 runtime/development files, HDF5
tools, Radelta filter, Python helper source and both licences. A relocated-package
smoke test runs `h5dump -V` and the native plugin-loading/5D round-trip/hyperslab
test before uploading each ZIP. The existing Linux HDF5 CI job separately runs
the full Python file-conversion and filter integration suite.

## Use an extracted package

On Linux/macOS, change into the extracted package directory and run:

```sh
export HDF5_PLUGIN_PATH="$PWD/lib/hdf5/plugins"
./bin/radelta-hdf5-check
./bin/h5dump your-image.h5
```

The Unix libraries use relative loader paths, so the package can be moved.
Keep the archive's directory structure intact.

On Windows, change into the extracted package directory in PowerShell:

```powershell
$env:HDF5_PLUGIN_PATH = "$PWD/lib/hdf5/plugins"
$env:PATH = "$PWD/bin;$env:PATH"
.\bin\radelta-hdf5-check.exe
.\bin\h5dump.exe your-image.h5
```

Windows requires the x64 Microsoft Visual C++ 2015–2022 runtime. Python helpers
are installed with `python -m pip install ./python`; their h5py dependency must
use this same HDF5 installation. For a source h5py build, set `HDF5_DIR` to the
absolute extracted-package directory before installing with
`python -m pip install --no-binary=h5py h5py`. A compiler is needed for that h5py
build. The workflow does not bundle Python or prebuilt h5py wheels.

These packages use a minimal shared HDF5 SDK with zlib/SZIP disabled, so its
tools cannot decode gzip/SZIP-compressed datasets. Radelta and the built-in
Fletcher32 checksum work. If an application needs other HDF5 filters or an
existing HDF5 installation, build the Radelta plugin against that installation
as described in `HDF5.md`. Do not mix two different HDF5 library installations
in one process; ordinary PyPI h5py wheels may bundle their own copy.

The default filter ID remains the private ID 65024. Other readers need a
matching plugin and HDF5 library. Applications can use the included headers and
CMake SDK files to build against the bundled HDF5 library.

## Repository setup and release

Commit the workflow, build/package helpers, CMake changes and HDF5 implementation,
then push the branch. To get downloadable binaries immediately after merging,
run **HDF5 builds** manually; a new version tag is not required.

For a tagged release, keep `VERSION`, native Cargo metadata, ImageJ metadata,
the HDF5 CMake version and Python package version synchronised before creating
`v<version>`. The existing `release.yml` waits for HDF5 builds and publishes
all five HDF5 ZIPs with the native/ImageJ assets and release checksums.
