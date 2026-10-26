# Python API

The `radelta` package provides NumPy array compression and lazy file access.
NumPy is its only runtime dependency. HDF5 and Zarr remain optional packages.

## Installation

Download the wheel matching your operating system and architecture from the
GitHub release or a completed CI run, then install that file. For Windows x86-64:

```powershell
python -m pip install .\radelta-0.8.8-py3-none-win_amd64.whl
```

The wheel includes its native codec. No Rust compiler, DLL copying or
`RADELTA_NATIVE_LIBRARY` setting is needed. Python 3.9+ is supported. Release
Linux array wheels target glibc 2.28+, and macOS wheels target 11.0+.
PyPI publication is separate from the GitHub release workflow; these commands
refer to downloaded wheel files.

For HDF5 files, also install the matching `radelta_hdf5` wheel:

```powershell
python -m pip install .\radelta_hdf5-0.8.8-py3-none-win_amd64.whl
```

It installs h5py and bundles a filter that registers with h5py's own HDF5
runtime. Standard pip-installed h5py is supported; no separate HDF5 SDK or
`HDF5_PLUGIN_PATH` is required. Linux HDF5 wheels target glibc 2.34+.
The existing `radelta_zarr` wheel remains self-contained, requires Python
3.11+, and provides Zarr/OME-Zarr integration through `radelta_zarr`.
The core package declares `hdf5` and `zarr` installation extras for use when
matching dependencies are available from an index or local wheel directory.

## Arrays and bytes

```python
import numpy as np
import radelta

image = np.arange(3 * 64 * 64, dtype=np.uint16).reshape(3, 64, 64)
encoded = radelta.compress(image)
restored = radelta.decompress(encoded)
np.testing.assert_array_equal(restored, image)
```

Inputs must be uint16 arrays of rank 2–5. Non-contiguous arrays and either byte
order are accepted. Default axes by rank are `yx`, `zyx`, `czyx` and `tczyx`.
Supply `axes="tyx"`, for example, to distinguish a 2D time series from a volume.
Explicit axes can be in any order and must use unique `t`, `c`, `z`, `y`, `x`
names including `y` and `x`.

Compression is lossless by default. Calibrated lossy compression requires
explicit camera offset and gain in electrons/ADU:

```python
encoded = radelta.compress(image, lossy=True, offset_adu=100,
                          gain_e_per_adu=0.46, noise_step=2)
```

`compress` also accepts `block_depth`, `context`, `scale_bits` and opaque
`metadata` bytes. Output is a standard Radelta stream, with array shape, axes,
byte order and user metadata recorded in the existing metadata extension.
Native C/Rust readers and Fiji can decode its pixels. Python restores the
original layout; readers that do not interpret Python's layout metadata expose
canonical TCZYX pixels. No new pixel/container format is introduced.

`decompress` also accepts existing non-streaming Radelta streams. Without Python
layout metadata, output uses canonical TCZYX order with leading singleton T/C/Z
axes omitted. It returns native-endian uint16. Use file access for RDS3/RQS3.
An optional `max_output_bytes` limit can reject an unexpectedly large decode
before allocating its output. Array operations hold complete input/output in RAM.

## Files and partial reads

```python
radelta.write("image.rdlt", image)
restored = radelta.read("image.rdlt")

with radelta.open("image.rdlt") as dataset:
    print(dataset.shape, dataset.axes, dataset.dtype)
    plane = dataset[1]
    region = dataset[:, 10:30, 20:40]
```

`read` loads the full array and accepts `max_output_bytes`. `open` is lazy and
supports integers, slices, negative indices/steps and one ellipsis; it does not
support NumPy fancy/boolean indexing or insertion of new axes. It decodes full
XY planes from the native cache, or intersecting HDF5 chunks, and allocates the
requested output region. For `.rdlt` files, recently decoded volumes or Z chunks
are retained in a 2 GiB LRU cache per open file by default, grown on demand,
so alternating channels can reuse their decoded data. Multiple entries are retained only after interleaving or
revisiting data is detected; a first sequential pass keeps one entry.
`open` and `read` accept `cache_mib` to override this budget;
zero restores single-entry caching. Up to 128 entries are retained. One decoded
volume/chunk larger than the budget is still allowed, as in the original reader.
This setting controls retained data, not total decoder working memory, and does
not apply to HDF5 files.
Use the context manager to release file handles. `dataset.metadata` returns the
user's opaque metadata bytes.

`write` streams native `.rdlt` files in TCZ plane order. It accepts lossy
calibration, `metadata`, a `memory_mib` budget (default 256) and `overwrite`.
The source array is already in memory; mapped arrays can also be supplied.
Native writing uses default codec settings. Existing output requires
`overwrite=True`; failed writes preserve it. Output is published only after
all planes and metadata have been written. Memory includes at least one XY
plane and the native codec's chunk workspace.

HDF5 paths use the same interface once the HDF5 wheel is installed:

```python
radelta.write("image.h5", image, chunks=(2, 32, 32))
with radelta.open("image.h5") as dataset:
    region = dataset[1, 10:30, 20:40]
```

The default HDF5 dataset is `/images`; `dataset="/group/pixels"` overrides it
for `read`, `write` and `open`. HDF5 chunk extents follow physical canonical
TCZYX order with leading absent axes omitted, even if input axes are reordered.
Full Python array layout is retained separately for round trips. These helpers
store pixel arrays and opaque metadata, rather than inventing physical voxel
calibration or acquisition metadata.

For direct h5py use or BDV export, use `radelta_hdf5.create_dataset` and
`radelta_hdf5.export_bdv`; see the [HDF5 guide](../hdf5/README.md).

## Development

```sh
cargo build --release --locked --lib --manifest-path native/Cargo.toml
python -m pip install -e ./python
```

A source checkout finds its Cargo-built native library automatically; the
`RADELTA_NATIVE_LIBRARY` environment variable can override it. Build standalone
wheels with `scripts/package_python_wheel.py`; the release workflow repairs
and checks native platform tags, then runs the installed-wheel tests outside
the checkout with library-path overrides removed.
