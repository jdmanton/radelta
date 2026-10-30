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
called by the tag-release workflow. Download the matching
`hdf5-<platform>-<architecture>` artifact from a completed Actions run; inside
it contains `radelta-hdf5-<version>-<platform>-<architecture>.zip` and the
standalone `radelta-hdf5-filter-<version>-<platform>-<architecture>.zip`. Tagged
releases attach both sets alongside the native and ImageJ packages.
No release is published by a manual HDF5 build alone.

Each job builds shared HDF5 1.14.6 from pinned upstream commit
`7bf340440909d468dbb3cf41f0ea0d87f5050cea`, then builds the filter with Rust
1.98.0. The archive includes the matching HDF5 runtime/development files, HDF5
tools, Radelta filter, Python helper source and both licences. A relocated-package
smoke test runs `h5dump -V` and the native plugin-loading/5D round-trip/hyperslab
test before uploading each ZIP. The Linux HDF5 CI job runs
the full Python file-conversion and filter integration suite.

The standalone filter ZIP contains only the embedded Radelta filter and documentation.
It is useful for configuring other native readers directly. For Fiji, prefer
the complete `radelta-imagej-<version>-fiji.zip`: the Radelta JAR includes the
filter and registers it automatically, and the installation bundle includes
the importer/dependencies. See [Fiji installation](FIJI.md).
Each workflow job tests the extracted decoder archive against its SDK, then
runs headless Fiji/JHDF5 imports on the four platforms supported by the released
JHDF5 jar, using the built ImageJ plugin with `HDF5_PLUGIN_PATH` unset. The Linux
ARM64 Java test is explicitly skipped because that jar
does not supply a native library for that platform.

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
as described in [the HDF5 guide](README.md#build-and-install). Do not mix two different HDF5 library installations
in one process; ordinary PyPI h5py wheels may bundle their own copy.

The default filter ID is the private ID 65024. Other readers need a
matching plugin and HDF5 library. Applications can use the included headers and
CMake SDK files to build against the bundled HDF5 library.
