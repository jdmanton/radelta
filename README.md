# Radelta

Radelta quickly and efficiently compresses 16-bit fluorescence microscopy images, without needing a GPU.
It works with TIFF stacks, ImageJ/Fiji, Python/NumPy, HDF5 and OME-Zarr, and offers two modes:

- **Lossless** (default): every pixel is restored exactly.
- **Calibrated lossy**: discards detail smaller than the photon shot noise, using your camera's offset and gain, for much smaller files.

> **Radelta is pre-1.0.** The file format can still change between releases.
> Files written by versions 0.5.0–0.7.1 cannot be read by 0.7.2 or later.
> Please keep your original data until you have checked the compressed copy.


## Contents

- [Which package do I need?](#which-package-do-i-need)
- [Install](#install)
- [Quick start](#quick-start)
- [Lossless or lossy?](#lossless-or-lossy)
- [Supported data](#supported-data)
- [How it works](#how-it-works)
- [Lossless compression benchmark](#lossless-compression-benchmark)
- [Documentation](#documentation)


## Which package do I need?

All downloads are on the [Releases page](https://github.com/jdmanton/radelta/releases).

| I want to… | Download |
| --- | --- |
| Open and save Radelta files in Fiji | `radelta-imagej-<version>.jar` |
| …and also open Radelta-compressed HDF5 files in Fiji | `radelta-imagej-<version>-fiji.zip` |
| Compress TIFF files from the command line | `radelta-<version>-<platform>-<arch>.zip` |
| Compress NumPy arrays in Python | `radelta-<version>-py3-none-<platform>.whl` |
| Read and write HDF5 or BigDataViewer files in Python | `radelta_hdf5-<version>-py3-none-<platform>.whl` |
| Read and write OME-Zarr in Python | `radelta_zarr-<version>-py3-none-<platform>.whl` |
| Use the C library in my own program | `radelta-<version>-<platform>-<arch>.zip` |

Builds are provided for Windows x86-64, Linux x86-64 and ARM64, and macOS 11+ on Intel and Apple Silicon.
In wheel names, the platform part is:

| Your computer | Wheel ends in |
| --- | --- |
| Windows, 64-bit | `win_amd64.whl` |
| Linux, Intel/AMD | `x86_64.whl` |
| Linux, ARM | `aarch64.whl` |
| Mac with Intel chip | `x86_64.whl` (starts `macosx_`) |
| Mac with Apple chip (M1 or later) | `arm64.whl` |


## Install

### Fiji

To open and save `.rdlt` files:

1. Download `radelta-imagej-<version>.jar`.
2. Close Fiji, copy the JAR into `Fiji.app/plugins`, and delete any older
   `radelta-imagej-*.jar` there.
3. Restart Fiji. The commands appear under **Plugins › Radelta**.

To open Radelta-compressed HDF5 files as well, download `radelta-imagej-<version>-fiji.zip` instead, extract it, close Fiji and run the included installer with Python 3.8 or later:

```sh
python install.py /path/to/Fiji.app
```

This adds any missing HDF5 reader libraries and disables older Radelta JARs. See [Fiji installation](imagej-plugin/FIJI_BUNDLE.md) for details.

### Command line

Download and extract `radelta-<version>-<platform>-<arch>.zip`.
It contains the `radelta` program (`radelta.exe` on Windows); either put its folder on your `PATH`, or run it by its full path.

### Python

Download the wheel for your computer, then install the file with pip:

```sh
python -m pip install ./radelta-<version>-py3-none-<platform>.whl
```

Each wheel contains its own copy of the compression library; nothing else needs installing.
The `radelta` and `radelta_hdf5` wheels need Python 3.9+; `radelta_zarr` needs Python 3.11+.
On Linux, the HDF5 wheel needs glibc 2.34+ (Ubuntu 22.04 or later); the others need glibc 2.28+.


## Quick start

### Command line

```sh
# Lossless
radelta encode stack.tif stack.rdlt
radelta decode stack.rdlt restored.tif

# Calibrated lossy (see "Lossless or lossy?" below)
radelta encode-lossy stack.tif stack-lossy.rdlt \
  --offset-adu 100 --gain-e-per-adu 0.46 --noise-step 2
```

`encode` reads OME-TIFF and ImageJ dimensions automatically and keeps the TIFF metadata, which `decode` restores.
Existing output files are replaced.
Large stacks are compressed in bounded memory automatically (2 GiB by default; set it with `--memory-mib`). See the [command-line reference](docs/CLI.md) for all options.


### Python

```python
import numpy as np
import radelta

image = np.random.poisson(100, size=(10, 256, 256)).astype(np.uint16)

# In memory
data = radelta.compress(image)
assert np.array_equal(radelta.decompress(data), image)

# Files: read lazily, decoding only what you index
radelta.write("image.rdlt", image)
with radelta.open("image.rdlt") as stack:
    print(stack.shape, stack.axes)
    plane = stack[5]
    crop = stack[:, 100:150, 100:150]
```

With `radelta_hdf5` installed, the same `write`/`open` calls accept `.h5` paths. See the [Python guide](docs/PYTHON.md).


### Fiji

Drag a `.rdlt` file onto Fiji, or use **File › Open**.
You can open it as a virtual stack, which decodes planes as you browse, or load it into memory.
To save, use **Plugins › Radelta › Save as Radelta (lossless)…** or **(lossy)…**. Calibration, slice labels and image properties are kept; ROIs and lookup tables are not.

With the Fiji bundle installed, Radelta HDF5 files open the same way, and **Save as Radelta HDF5…** writes files that h5py and other HDF5 tools can read (see [HDF5 in Fiji](hdf5/FIJI.md)).


## Lossless or lossy?

Use **lossless** unless file size is a really strong constraint.
It is the default and restores every value exactly.

**Lossy** mode uses the fact that a pixel recording `N` photoelectrons carries random shot noise of about `√N`.
It rounds each pixel to a step that is a fixed fraction of that noise, so bright pixels are stored more coarsely than dim ones, but always below the noise.
It needs three numbers:

| Option | Meaning | Where to find it |
| --- | --- | --- |
| `--offset-adu` | Camera reading with no light, in ADU | Camera specification, or the mean of a dark frame |
| `--gain-e-per-adu` | Photoelectrons per ADU (or `--gain-adu-per-e` for the inverse) | Camera specification or a photon-transfer measurement |
| `--noise-step` | Rounding step, in units of the shot noise; larger is smaller and coarser | Default 2, which adds rounding error of about 0.58× the shot noise |

Things to know before using lossy mode:

- Values at or below the offset are stored as zero photons.
  This raises the mean of very dim background regions slightly, typically by 1–2 ADU, which  matters for background-sensitive measurements.
- Pixels may hold at most (127.75 × noise step)² photoelectrons: about 65,000 at the default step of 2.
  Saturated pixels on a high-gain camera can exceed this, and encoding then stops with an error.
  Increase `--noise-step` if this happens.
- Discarded detail cannot be recovered.
  Converting a lossy file back to lossless does not restore it.


## Supported data

- **Pixel type:** unsigned 16-bit only.
  8-bit, 32-bit and floating-point images are not currently supported.
- **Dimensions:** up to five (time, channel, Z, Y, X).
  Neighbouring pixels are only used for prediction within one channel at one timepoint.
- **TIFF input:** multipage stacks, ImageJ/Fiji hyperstacks and single-file OME-TIFF.
  Pages with several samples stored separately are read as channels; interleaved RGB pages are rejected.
  A file from a multi-file OME-TIFF acquisition can be compressed on its own if it holds a complete block of planes, but the files are not combined into one dataset.
- **Metadata:** TIFF descriptions, calibration, per-plane tags and vendor tags are kept and restored on decode.
  See [metadata handling](docs/CLI.md#metadata-preservation).
- **Other containers:** an [HDF5 filter](hdf5/README.md) (ID 65024) for chunked datasets, including BigDataViewer XML/HDF5 export, and [OME-Zarr codecs](zarr/README.md) for Zarr v2 and v3.


## How it works

For a pixel value `x`, Radelta stores the integer square root `q = ⌊√x⌋` and the remainder `r = x − q²`, so `x = q² + r` exactly.
This split of the **ra**dical and the **delta** gives Radelta its name.
Because shot noise grows with `√x`, `q` is smooth and predictable from neighbouring pixels, while `r` is mostly noise.
`q` is coded using its 3-D neighbourhood, `r` is coded given `q`, and both use static probability tables with interleaved rANS entropy coding.
Each volume is split into independent blocks of Z-planes, which are compressed in parallel.

Lossy mode converts each pixel to photoelectrons, `e = max((x − offset) × gain, 0)`, then applies `z = 2√e`.
For photon-limited data this makes the noise roughly the same at every brightness, so `z` is rounded to multiples of
`noise_step` and coded the same way.
The approach is similar to the lossy mode of [B3D](https://github.com/balintbalazs/B3D), but runs on the CPU.

Blocks that would not get smaller are stored uncompressed, and every block carries a CRC-32 checksum, so corruption is reported rather than decoded into wrong pixels.
The [file format](docs/FORMAT.md) is fully documented.


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


## Documentation

- [Command-line reference](docs/CLI.md)
- [Python guide](docs/PYTHON.md)
- [Fiji installation](imagej-plugin/FIJI_BUNDLE.md) and [HDF5 in Fiji](hdf5/FIJI.md)
- [HDF5 filter and tools](hdf5/README.md)
- [OME-Zarr codecs and tools](zarr/README.md)
- [C API](docs/C_API.md)
- [File format](docs/FORMAT.md)
- [Benchmarks](docs/BENCHMARKS.md)
- [Building from source and contributing](docs/DEVELOPING.md)
- [Changelog](CHANGELOG.md)


## Licence and acknowledgements

Radelta was created by James Manton and is distributed under the BSD 3-Clause License; see [LICENSE](LICENSE).
Much of the boilerplate code was written using ChatGPT.
