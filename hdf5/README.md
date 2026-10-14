# Radelta compression for HDF5

This plugin stores ordinary chunked HDF5 datasets using Radelta 0.7.3.
Each chunk contains an RDL1/RDM2 lossless or RDLQ/RDQ2 calibrated-lossy stream.
HDF5 handles indexing, metadata and partial reads; the filter only encodes pixels.
The Rust codec is linked into the plugin, so readers do not need a separate
Radelta shared library. Existing Radelta formats and the TIFF CLI are unchanged.

Supported datasets are unsigned 16-bit integers in either byte order. Rank
2, 3, 4 and 5 mean `YX`, `ZYX`, `CZYX` and `TCZYX`, respectively. Insert singleton
axes where needed: a time series without channels is `T,1,Z,Y,X`. Contexts
never cross T/C boundaries, even when a chunk contains multiple volumes.
Edge chunks, hyperslab reads, partial updates and extendible datasets use the
normal HDF5 APIs. Only chunks intersecting a read are decoded; an intersected
chunk is decoded in full. Lossy partial updates recompress the affected chunk.

## Online builds

The [HDF5 builds workflow](../.github/workflows/hdf5-build.yml) produces
Windows x86-64, Linux x86-64/ARM64 and macOS Intel/Apple Silicon packages.
The read/write SDK ZIP includes the matching shared HDF5 runtime and is tested
after relocation. The [complete Fiji bundle](FIJI.md) includes the decoder in
the existing ImageJ JAR and supplies the HDF5_Vibez/JHDF5 importer dependencies.
A standalone decoder ZIP is also available for other native HDF5 readers.
See [download and use prebuilt packages](ONLINE_BUILDS.md). Tagged releases
attach the packages automatically; manual Actions runs do not require a new tag.

## Build and install

Requires Rust 1.98.0, CMake 3.18+, a C compiler and HDF5 development files
(HDF5 1.8.15 or later). For example, on Ubuntu:

```sh
sudo apt install build-essential cmake libhdf5-dev
cmake -S hdf5 -B hdf5/build -DCMAKE_BUILD_TYPE=Release
cmake --build hdf5/build --config Release
ctest --test-dir hdf5/build -C Release --output-on-failure
```

On macOS install HDF5 with `brew install hdf5`; if necessary add
`-DHDF5_ROOT="$(brew --prefix hdf5)"` to the configure command. On Windows use
a matching **64-bit shared HDF5** development installation and the MSVC Rust
and C toolchains; pass `-DHDF5_ROOT=C:/path/to/HDF5` when configuring. The
plugin is in `hdf5/build/plugins` (or `plugins/Release` for a multi-configuration
Windows generator). `cmake --install hdf5/build --prefix /your/prefix` installs
it under `lib/hdf5/plugins`. Windows readers also need the HDF5 DLLs on `PATH`.

Set `HDF5_PLUGIN_PATH` to the directory containing `libH5Zradelta.so`,
`libH5Zradelta.dylib` or `H5Zradelta.dll`, as appropriate, before opening files:

```sh
export HDF5_PLUGIN_PATH="$PWD/hdf5/build/plugins"
```

In PowerShell:

```powershell
$env:HDF5_PLUGIN_PATH = "$PWD/hdf5/build/plugins/Release"
```

**Build against the same HDF5 installation used by the reader.** In particular,
PyPI h5py wheels often bundle a different HDF5 library from the system library.
Mixing two copies can invalidate the dataset handles passed to the callbacks.
This applies to the full read/write filter. For Fiji, use the read-only
`hdf5/build/fiji-plugins` target instead; see [Fiji installation](FIJI.md).
Use an h5py build linked to your HDF5 installation, e.g.:

```sh
python -m pip install --no-binary=h5py h5py
python -m pip install ./hdf5/python
```

On Ubuntu, the distribution packages `python3-h5py` and `python3-tifffile`
match `libhdf5-dev`; use the distribution Python interpreter and set
`PYTHONPATH=hdf5/python` to run the tools directly. Check `h5py.version.info`.

## Python and partial access

```python
import h5py
import numpy as np
from radelta_hdf5 import configure_plugin, create_dataset

configure_plugin("hdf5/build/plugins")  # alternatively use HDF5_PLUGIN_PATH
with h5py.File("image.h5", "w") as f:
    d = create_dataset(f, "images", shape=(100, 2048, 2048),
                       chunks=(4, 512, 512))
    # Feed slices or chunks from an acquisition or an existing file.
    d[:4] = np.zeros((4, 2048, 2048), dtype=np.uint16)

with h5py.File("image.h5", "r") as f:
    region = f["images"][2:3, 100:200, 300:400]
```

To create a calibrated-lossy dataset, add `lossy=True, offset_adu=100,
gain_e_per_adu=0.46, noise_step=2` to `create_dataset`. Gain is electrons/ADU.
The filter parameters and readable dataset attributes store the calibration.
Default codec options are block depth 4, context `mean`, scale bits 10.

To append timepoints:

```python
with h5py.File("acquisition.h5", "w") as f:
    d = create_dataset(f, "images", shape=(0, 1, 100, 2048, 2048),
                       maxshape=(None, 1, 100, 2048, 2048),
                       chunks=(1, 1, 4, 512, 512))
    d.resize(1, axis=0)
    # Write d[0, 0, ...] in slabs or chunks.
```

The default chunk chooser targets at most 8 MiB of raw pixels and uses T/C
extents of one. Larger chunks can improve compression by reducing probability
table overhead; smaller XY tiles reduce read amplification for small regions.
The raw chunk size must be smaller than 4 GiB. The codec runs its usual parallel
block processing inside each chunk; HDF5 ordinarily invokes filters serially.
One-pass allocating C encoding avoids compressing twice to size a buffer.
Memory is bounded by the current chunk and codec workspace, plus HDF5 caches.

## File conversion

The companion `radelta-hdf5` CLI is installed with `pip install ./hdf5/python`.
It is separate from the existing `radelta` TIFF/rdlt CLI. `python -m radelta_hdf5`
can also be used. `--plugin-path` and `--filter-id` are global options, before
the subcommand; omit `--plugin-path` if `HDF5_PLUGIN_PATH` is already set.

```sh
radelta-hdf5 --plugin-path hdf5/build/plugins encode input.tif output.h5 --chunks 4,512,512
radelta-hdf5 --plugin-path hdf5/build/plugins encode input.tif lossy.h5 --lossy --offset-adu 100 --gain-e-per-adu 0.46 --noise-step 2
radelta-hdf5 --plugin-path hdf5/build/plugins decode output.h5 restored.tif
radelta-hdf5 --plugin-path hdf5/build/plugins repack existing.h5 compressed.h5 --dataset /images
```

TIFF import infers tifffile axes and converts page order to canonical axes.
Plain multipage stacks become ZYX; `--axes TZCYX` can override source axes.
Import supports a single series of grayscale uint16 YX pages, including
ImageJ/OME page stacks. Volumetric pages, RGB and multiple series are rejected.
It processes one chunk at a time, decoding at most one complete TIFF plane
at a time. XY tiling can require repeated TIFF page reads. Export writes a
BigTIFF with canonical axes and streams one plane at a time. TIFF import saves
the source description as provenance; export does not restore all original
TIFF tags or generate ImageJ/OME calibration metadata.

`repack` copies only the selected dataset and its ordinary attributes to a new
HDF5 file; it does not copy unrelated groups, datasets, links or dimension-scale
relationships. Reference/variable-length object attributes must be copied
explicitly and are rejected by this command. The source must already have canonical axes; an `axes` attribute
with a different order is rejected. All three commands report progress and
write through a temporary file. Existing outputs require `--overwrite`.

## C and other HDF5 applications

After setting `HDF5_PLUGIN_PATH`, ordinary `H5Dread` calls decode the data.
For a lossless dataset, set chunk dimensions and attach the filter to its
creation property list:

```c
H5Pset_chunk(dcpl, rank, chunk_dimensions);
H5Pset_filter(dcpl, 65024, H5Z_FLAG_MANDATORY, 0, NULL);
/* H5Dcreate2(..., H5T_STD_U16LE, ..., dcpl, ...); then H5Dwrite as usual. */
```

The plugin must be the first filter. Preceding shuffle, scale-offset or another
compressor is rejected because Radelta needs original pixel values. A subsequent
Fletcher32 checksum is supported. For lossy data, use a low-level creation
property list for a subsequent checksum: h5py's high-level helper can classify
unknown custom filters as potentially lossy and refuse Fletcher32.

Invalid datatypes/options cause dataset creation to fail. The plugin converts
its filter to mandatory during creation, including h5py custom filters, so an
encoding error cannot silently skip compression or lossy quantisation. Even
incompressible chunks are stored as Radelta streams; tiny chunks can therefore
be larger than raw data. Missing plugins cause compressed reads to fail.
Decoded dimensions are checked against the stored chunk geometry before
allocating the output buffer. Every Radelta stream carries CRC-32 checksums of
its header and blocks, so payload corruption is reported as a read error; a
subsequent Fletcher32 filter is optional.

## Filter parameters and ID

The default **private** filter ID is `65024`. It has not been registered with
The HDF Group; reserve a public ID before distributing interoperable production
files widely. Build with `-DRADELTA_HDF5_FILTER_ID=NNNNN` to use a different private
ID, and use that same ID in Python (`filter_id=NNNNN`, or CLI `--filter-id`).
The ID is stored in each file and cannot be changed merely by rebuilding readers.

Creation parameters are eleven uint32 words:

| Index | Meaning |
| --- | --- |
| 0 | Parameter schema version, 1 |
| 1 | Mode: 0 lossless, 1 calibrated lossy |
| 2, 3, 4 | Block depth, context enum, scale bits |
| 5–6 | Offset, IEEE-754 binary64 bits, low word first |
| 7–8 | Gain in electrons/ADU, same encoding |
| 9–10 | Noise step, same encoding |

Zero parameters select lossless defaults. `set_local` appends X,Y,Z,C,T chunk
extents (indices 11–15, omitted axes become one) and byte order (16: 0 little,
1 big). These local parameters are derived from the dataset, not supplied chunk
geometry. Schema version 1 accepts only the existing Radelta 0.5.1 formats.

## Tests

```sh
ctest --test-dir hdf5/build --output-on-failure
PYTHONPATH=hdf5/python RADELTA_HDF5_PLUGIN_PATH="$PWD/hdf5/build/plugins" \
    python -m unittest discover -s hdf5/tests -v
cargo test --release --manifest-path native/Cargo.toml
```

The integration suite covers ranks 2–5, both byte orders, all contexts,
non-divisible edge chunks, lossless equality, calibrated lossy reconstruction,
hyperslabs, partial writes, appending, corrupt/truncated chunks, invalid input,
missing plugins, TIFF axis reordering, HDF5 repacking and output protection.
