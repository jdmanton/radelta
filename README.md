# Radelta

Radelta compresses unsigned 8-bit and 16-bit fluorescence microscopy images, without needing a GPU.
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
- [Lossy compression benchmark](#lossy-compression-benchmark)
- [Documentation](#documentation)


## Which package do I need?

All downloads are on the [Releases page](https://github.com/jdmanton/radelta/releases).

| I want to… | Download |
| --- | --- |
| Try lossless compression, or decode a file, without installing anything | Nothing: use [Radelta Web](#in-your-browser-no-installation) |
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

### In your browser (no installation)
 
[Radelta Web](https://beryl.mrc-lmb.cam.ac.uk/radelta/) compresses TIFF files to `.rdlt` and decodes `.rdlt` files back to TIFF, entirely inside your web browser.
Images are processed on your computer and are not uploaded.
The files it writes can be opened with the command-line tool and the ImageJ/Fiji plugin.
 
- **Compress:** lossless only. Accepts uint16 TIFF and BigTIFF files, including OME-TIFF, ImageJ hyperstacks and SubIFD pyramids, stored uncompressed or with LZW, Deflate or PackBits compression. Tick **Verify pixels** to check the result against the input.
- **Decode:** lossless and calibrated-lossy `.rdlt` files, written out as BigTIFF with dimensions, calibration and descriptions kept.
- **Single files or whole folders:** in desktop Chrome or Edge, you can choose an output folder and process a folder of files at once.
  Other browsers download each result instead, which is limited to 256 MiB of image data per file.

Currently, it does not accept RGB or other multi-sample pages, or multi-file and partial OME-TIFF datasets; please use the command-line tool for those.
 
Radelta Web is **highly experimental** and is undergoing rapid development.
Please keep your original files until you have checked the outputs!
Note also that compression/decompression in the browser is slower than running the CLI tools or ImageJ/Fiji plugin.


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
Output filenames are optional: `radelta encode stack.tif` writes `stack.rdlt`.
Existing outputs are preserved unless you add `--force`. Use `--threads N` to
choose the worker count, `--quiet` for scripts, and `radelta encode --help` for options.
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

- Shape (TCZYX): `300 × 1 × 1 × 1006 × 1456`
- Voxels: `439,420,800`
- Uncompressed size: `838.129 MiB`
- Datatype: `uint16`
- AMD Ryzen 9 7950X 16-core CPU
- I/O excluded from codec timings
- Throughput reported as the median of ten timed repetitions

| Codec | Compressed size / MiB | Bits/pixel | Compression ratio | Encode / GB/s | Decode / GB/s |
|---|---:|---:|---:|---:|---:|
| JPEG-XL lossless, effort 9 | 216.615 | 4.135 | 3.87:1 | 0.002 | 0.372 |
| JPEG-XL lossless, effort 5 | 225.877 | 4.312 | 3.71:1 | 0.007 | 0.357 |
| **Radelta** | **232.938** | **4.447** | **3.60:1** | **2.811** | **6.474** |
| JPEG 2000 reversible | 237.433 | 4.533 | 3.53:1 | 0.305 | 0.311 |
| JPEG-LS lossless | 243.479 | 4.648 | 3.44:1 | 0.126 | 0.159 |
| Blosc2 bitshuffle + Zstd-9 | 273.366 | 5.219 | 3.07:1 | 0.262 | 5.559 |
| Zstd-15 | 284.512 | 5.431 | 2.95:1 | 0.005 | 0.888 |
| Blosc2 bitshuffle + Zstd-5 | 285.928 | 5.458 | 2.93:1 | 3.063 | 11.374 |
| Delta-X + Zstd-15 | 289.232 | 5.521 | 2.90:1 | 0.003 | 0.523 |
| PNG | 303.723 | 5.798 | 2.76:1 | 0.028 | 0.368 |
| Zstd-3 | 307.817 | 5.876 | 2.72:1 | 0.237 | 0.717 |
| Delta-X + Zstd-3 | 310.481 | 5.927 | 2.70:1 | 0.229 | 0.489 |
| Blosc2 bitshuffle + LZ4 | 312.317 | 5.962 | 2.68:1 | 6.106 | 11.340 |

Radelta compresses the 838 MiB volume to 232 MiB (3.6:1) while encoding at 2.811 GB/s and decoding at 6.474 GB/s on the CPU.
Its output is only 7.5% larger than JPEG-XL effort 9, while encoding is approximately 1400× faster and decoding approximately 17.4× faster.
General-purpose high-throughput codecs such as Blosc2 can exceed Radelta's speed, but produce significantly larger files.


## Lossy compression benchmark

For the same dataset as above, with offset: 100 ADU; gain: 0.46 electrons/ADU; noise step: 2.0:

| Codec | Compressed size / MiB | Bits/pixel | Compression ratio | Encode / GB/s | Decode / GB/s |
|---|---:|---:|---:|---:|---:|
| **Radelta (lossy)** | **19.525** | **0.373** | **42.93:1** | **5.203** | **9.220** |

Compared with lossless compression, we obtain an almost 12× smaller file (43× smaller than the original) and encode/decode at even higher speeds.


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
