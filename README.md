# Radelta

Radelta is a fast compression tool for 16-bit fluorescence microscopy images. It provides lossless and camera-calibrated lossy compression, a command-line interface, a native C/Rust library, and an ImageJ/Fiji plugin. It has been developed by James Manton, with ChatGPT providing much of the boilerplate code.

The lossless codec is designed around a simple property of photon-counting data: shot noise grows approximately with the square root of signal intensity. For each pixel value `x`, Radelta writes

```text
q = floor(sqrt(x))
r = x - q*q
```

so that the original value is recovered exactly as

```text
x = q*q + r
```

This idea of storing the radical and the delta is what gives Radelta its name. The square-root component `q` is predicted from neighbouring voxels and entropy-coded using a small causal 3-D context. The remainder `r` is coded conditional on `q`. Static probability tables and four-way interleaved byte-rANS provide the final entropy coding. Volumes are divided into independent Z blocks so that encoding and decoding can be parallelised.

The lossy codec uses the same photon-counting motivation, but deliberately quantises the signal at a scale related to its expected noise. Camera values `x` are first converted from ADU to estimated photoelectrons using the specified offset and gain, `e = max((x - offset) * gain, 0)`, and then transformed as `z = 2*sqrt(e)`. For Poisson-limited data, this approximately stabilises the noise variance, so a fixed step in `z` corresponds to a similar statistical significance across different signal levels. Radelta stores `q = round(z / noise_step)` and entropy-codes `q` using spatial prediction and byte-rANS; unlike the lossless mode, no exact within-bin remainder is retained. Decoding reconstructs `z = q*noise_step`, converts back with `e = z*z/4`, and applies the inverse camera calibration. The `noise_step` parameter therefore controls the trade-off between compression and fidelity in approximately shot-noise-normalised units. This is conceptually similar to the lossy mode of [B3D](https://github.com/balintbalazs/B3D), albeit with important implementation differences for increased throughput without the need for a GPU.

## Lossless compression benchmark

The table below compares lossless compression of a typical light sheet fluorescence microscopy z-stack:

- shape: `186 × 613 × 860`
- voxels: `98,055,480`
- uncompressed size: `187.026 MiB`
- datatype: `uint16`
- I/O excluded from codec timings
- throughput reported as the median of three timed repetitions
- AMD Ryzen 9 7950X 16-core CPU
- nVidia GeForce RTX 4070 GPU

| Codec | Compressed size / MiB | Bits/pixel | Compression ratio | Encode / GB/s | Decode / GB/s |
|---|---:|---:|---:|---:|---:|
| JPEG-XL lossless, effort 9 | 15.095 | 1.291 | 12.39:1 | 0.002 | 0.059 |
| **Radelta** | **15.659** | **1.340** | **11.94:1** | **2.496** | **5.873** |
| JPEG-XL lossless, effort 5 | 15.829 | 1.354 | 11.82:1 | 0.007 | 0.057 |
| JPEG-LS lossless | 16.882 | 1.444 | 11.08:1 | 0.429 | 0.485 |
| JPEG 2000 reversible | 17.779 | 1.521 | 10.52:1 | 0.088 | 0.100 |
| Blosc2 bitshuffle + Zstd-9 | 19.963 | 1.708 | 9.37:1 | 0.044 | 10.455 |
| B3D GPU, HDF5, Z=8 | 20.793 | 1.779 | 8.99:1 | 1.111 | 2.959 |
| B3D GPU, HDF5, Z=16 | 20.799 | 1.780 | 8.99:1 | 1.314 | 3.122 |
| B3D GPU, direct filter, Z=8 | 20.793 | 1.779 | 8.99:1 | 1.883 | 3.810 |
| B3D GPU, direct filter, Z=16 | 20.799 | 1.780 | 8.99:1 | 2.025 | 4.282 |
| Blosc2 bitshuffle + Zstd-5 | 20.588 | 1.761 | 9.08:1 | 4.976 | 11.397 |
| Delta-X + Zstd-15 | 23.143 | 1.980 | 8.08:1 | 0.031 | 0.686 |
| Zstd-15 | 23.903 | 2.045 | 7.82:1 | 0.033 | 1.765 |
| Delta-X + Zstd-3 | 25.237 | 2.159 | 7.41:1 | 0.534 | 0.662 |
| Zstd-3 | 26.280 | 2.248 | 7.12:1 | 0.725 | 1.609 |
| PNG | 26.447 | 2.263 | 7.07:1 | 0.122 | 0.528 |
| Blosc2 bitshuffle + LZ4 | 26.607 | 2.276 | 7.03:1 | 12.917 | 12.733 |

Radelta compresses the 187 MiB volume to 15.659 MiB (11.94:1) while encoding at 2.496 GB/s and decoding at 5.873 GB/s on the CPU. Its output is only 3.7% larger than JPEG-XL effort 9, while encoding is approximately 1234× faster and decoding approximately 100× faster. General-purpose high-throughput codecs such as Blosc2 can exceed Radelta's speed, but produce larger files.

The B3D measurements were obtained using an NVIDIA RTX 4070 GPU. `HDF5` rows include the HDF5 filter path, whereas `direct filter` rows measure the B3D filter directly and therefore represent its higher-throughput path. The best measured B3D result was 2.03 GB/s encoding and 4.28 GB/s decoding at approximately 20.8 MiB. Radelta therefore produced a substantially smaller file while exceeding those B3D GPU throughput measurements in this dataset, using only the CPU.

## Lossy compression benchmark

For the same z-stack and PC as for the lossless compression benchmark, a gain of 0.46 electrons per ADU, an ADU offset of 0 and a noise step of 2, we obtain:

| Codec | Compressed size / MiB | Bits/pixel | Compression ratio | Encode / GB/s | Decode / GB/s |
|---|---:|---:|---:|---:|---:|
| Radelta | 5.393 | 0.461 | 34.68:1 | 3.373 | 6.849 |

## OME-Zarr

The optional [OME-Zarr codecs and tools](zarr/README.md) support OME-Zarr 0.4
(Zarr v2) and 0.5 (Zarr v3), using native Radelta compression for uint16 image
chunks. They provide TIFF import/export, recompression of existing multiscale
hierarchies, partial reads, lossless labels and calibrated lossy image data.
Native release ZIPs include the matching Python codec package under `python/zarr`.
Releases also provide self-contained `radelta_zarr` wheels for direct pip
installation; choose the wheel for your operating system and architecture.

## Release packages

Each version has one GitHub release with separate installation assets:

| Asset | Contents |
|---|---|
| `radelta-<version>-<platform>-<arch>.zip` | CLI, C library/header, documentation and optional Zarr source package |
| `radelta_zarr-<version>-py3-none-<platform>.whl` | Python Zarr codec/converter with the matching native library |
| `radelta-hdf5-<version>-<platform>-<arch>.zip` | HDF5 filter, compatible runtime/tools and Python helpers |
| `radelta-hdf5-fiji-<version>-<platform>-<arch>.zip` | Standalone embedded filter for HDF5 runtimes such as Fiji's |
| `radelta-imagej-<version>.jar` | ImageJ plugin with native codecs and HDF5 decoders for all five platforms |
| `radelta-imagej-<version>-fiji.zip` | Complete Fiji installation: Radelta JAR, HDF5 importer/dependencies and installer |

Zarr wheels are tested in isolated installations before being uploaded.
Linux builds use a glibc 2.28 baseline; macOS builds target 11.0 or later.
The native library is built once per platform and reused for the native ZIP,
Zarr wheel and ImageJ assets. HDF5 is built separately.

## HDF5 files

The optional [HDF5 plugin and file tools](hdf5/README.md) store chunked uint16
datasets using lossless or calibrated-lossy Radelta compression. Standard HDF5
readers can access slices and regions with the plugin installed. The companion
`radelta-hdf5` command imports TIFF stacks, exports TIFF stacks and recompresses
existing HDF5 datasets. Native codec builds do not require HDF5.

`radelta-hdf5 bdv input.tif output.xml` exports tiled, multiresolution
BigDataViewer XML/HDF5 datasets without loading the whole volume. It also
accepts an existing HDF5 dataset. See [BDV export](hdf5/README.md#bigdataviewer-export).

For ImageJ/Fiji, install the [complete Fiji bundle](imagej-plugin/FIJI_BUNDLE.md).
The existing Radelta ImageJ JAR includes the HDF5 decoder and registers it
automatically with the HDF5_Vibez/JHDF5 importer. No environment-variable
configuration or second HDF5 SDK is needed.

## Building

The native codec requires Rust. From the repository root:

```sh
cargo test --release --manifest-path native/Cargo.toml
cargo build --release --manifest-path native/Cargo.toml
```

Build outputs are in `native/target/release/`. The command-line program is
`radelta` (`radelta.exe` on Windows). Shared-library filenames differ between
Cargo's build outputs and the packaged releases:

| Platform | Cargo output | Release filename |
| --- | --- | --- |
| Windows | `radelta_native.dll` | `radelta.dll` |
| Linux | `libradelta_native.so` | `libradelta.so` |
| macOS | `libradelta_native.dylib` | `libradelta.dylib` |

To create a native release ZIP, run this from the repository root, choosing
`windows`, `linux`, or `macos` as appropriate:

```sh
python scripts/package_native.py --platform windows
```

The packager handles renaming the shared library and includes the CLI, C
header, and documentation.

## Metadata

Radelta can attach arbitrary metadata bytes to any image container, compressed
separately with fast LZ4 and a raw fallback. The configurable metadata limit
defaults to 1024 MiB; the CLI accepts `--metadata-limit-mib N`. Rust and C APIs support reading,
replacing, and removing metadata without recompressing pixels. TIFF conversion
preserves descriptions, calibration and per-plane tags, vendor binary records,
and EXIF/GPS directories, restoring the original page order by default.
See [metadata behaviour](docs/CLI.md#metadata-preservation).

## Command-line use

Measure in-memory codec performance with `radelta benchmark input.tif` or
`radelta benchmark-lossy input.tif --offset-adu 0 --gain-e-per-adu 0.46 --noise-step 2`.
Both support `--repeats N`; see [CLI timing details](docs/CLI.md#measuring-codec-performance).

Compress a TIFF losslessly:

```bash
radelta encode input.tif output.rdlt
```

Decompress it:

```bash
radelta decode output.rdlt restored.tif
```

Camera-calibrated lossy compression:

```bash
radelta encode-lossy input.tif output.rdlt \
  --offset-adu 100 \
  --gain-e-per-adu 0.46 \
  --noise-step 2
```

See [docs/CLI.md](docs/CLI.md) for the full command-line reference.

## Supported microscopy data

The CLI reads grayscale `uint16` TIFF data and understands:

- ordinary multipage TIFF stacks;
- single-file OME-TIFF metadata;
- ImageJ/Fiji hyperstacks; and
- supported contiguous ImageJ/tifffile hyperstacks.

Internally, multidimensional data use contiguous `T,C,Z,Y,X` order with X fastest. Spatial prediction is confined to each `(T,C)` volume.

Large datasets can be written in a bounded-memory streaming form. ImageJ opens these through a virtual stack and decodes only the chunk containing the requested plane.

## ImageJ/Fiji

The plugin can open `.rdlt` / `.radelta` files and export datasets using either lossless or calibrated-lossy compression.
It also opens Radelta-compressed HDF5 files through the existing Fiji HDF5
importer, using the filter bundled in the same JAR. Drag such a file onto Fiji,
use File > Open, or choose **Plugins → Radelta → Open HDF5…**; `.rdlt` opening
and export commands remain available.
**Save as Radelta HDF5 (lossless)…** and **(lossy)…** write HDF5 datasets that
Fiji, h5py and the `radelta-hdf5` tools can read; see
[Writing from Fiji](hdf5/FIJI.md#writing-from-fiji).

Opening from the Radelta menu, File > Open, or drag-and-drop offers a choice
between a virtual stack (the default) and loading the entire dataset into RAM.
The dialog shows the dataset dimensions, format, minimum 16-bit pixel storage,
Java heap limit, and approximate unused heap. Full loading reports progress and
closes the source file once complete; virtual loading decodes planes on demand.
Both modes restore the stored metadata. The memory estimate excludes Java/ImageJ
and native decoding overhead.

Headless opening defaults to a virtual stack. Scripts can explicitly select a
mode with `RadeltaImageFactory.openForDisplay(path, loadIntoMemory)`;
`openBase(path)` remains a non-interactive virtual-stack reader.

Exports preserve spatial, temporal, and intensity calibration, image titles,
slice labels, and string, numeric, Boolean, and byte-array properties. Original
opaque metadata is retained when a Radelta file is edited and resaved. TIFF
metadata from the CLI supplies common ImageJ/OME calibration on opening.
Arbitrary Java objects, ROIs, and display LUTs are not serialized.

Exports publish only after all pixels and metadata have been written. Failed
exports preserve existing destinations. Virtual-stack decoding errors stop
processing and export; unreadable planes are never replaced with black pixels.
Saving over a virtual stack's source file, including an alias, is rejected.

For a complete installation, download `radelta-imagej-0.7.6-fiji.zip`, extract
it, close Fiji and run `python install.py /path/to/Fiji.app`. The installer
adds missing HDF5 dependencies, reuses existing reader libraries and disables
older Radelta JARs. See [installation details](imagej-plugin/FIJI_BUNDLE.md).
If the HDF5 importer is already installed, copy only `radelta-imagej-0.7.6.jar`
into Fiji's `plugins` folder, remove older Radelta JARs and restart Fiji.
The release JAR includes both codecs and HDF5 decoders for all five platforms.

To build the plugin from source, install Rust, JDK 17, and Maven. First build
the native library using the commands above. Copy the shared library from
`native/target/release/` into the matching location below, creating the folders
as needed. Paths are relative to `imagej-plugin/src/main/resources/`:

| Platform | Bundled library path |
| --- | --- |
| Windows x86-64 | `natives/windows-x86_64/radelta.dll` |
| Linux x86-64 | `natives/linux-x86_64/libradelta.so` |
| Linux ARM64 | `natives/linux-aarch64/libradelta.so` |
| macOS Intel | `natives/macos-x86_64/libradelta.dylib` |
| macOS Apple Silicon | `natives/macos-aarch64/libradelta.dylib` |

Then, from the repository root:

```sh
mvn --batch-mode -f imagej-plugin/pom.xml clean package
```

This runs headless integration tests against the bundled library and creates
`imagej-plugin/target/radelta-imagej-0.7.6.jar`. Install that JAR in Fiji as above.
A local build supports the platforms whose libraries you bundled; the release
workflow bundles all five.

For HDF5 support in a manual build, also build the embedded HDF5 target and
copy its library into `src/main/resources/hdf5/<platform>-<arch>/` before
building the JAR. The [HDF5 build instructions](hdf5/README.md) describe that
target. Reader dependencies are supplied by Fiji or the complete bundle.

For development without bundling, set `RADELTA_NATIVE_LIBRARY` to the absolute
path of the Cargo-built shared library before running Maven, and also in the
environment used to launch Fiji.

## Native API

The C API is declared in `native/include/radelta.h`. It supports in-memory compression/decompression, multidimensional data, random plane access, and bounded-memory streaming writers.

See [docs/C_API.md](docs/C_API.md) for examples.

## File formats

Radelta uses six closely related container types:

| Magic | Purpose |
| --- | --- |
| `RDL1` | lossless XYZ volume |
| `RDLQ` | calibrated-lossy XYZ volume |
| `RDM2` | lossless TCZYX container |
| `RDQ2` | calibrated-lossy TCZYX container |
| `RDS3` | lossless streaming TCZYX container |
| `RQS3` | calibrated-lossy streaming TCZYX container |

See [docs/FORMAT.md](docs/FORMAT.md) for the format details.

## License

Radelta is distributed under the BSD 3-Clause License. See [LICENSE](LICENSE).
