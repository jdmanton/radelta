# Radelta compression for OME-Zarr

The optional `radelta-zarr` Python package provides native Radelta codecs for
**Zarr format 2 and 3**, with OME-Zarr **0.4 and 0.5** metadata respectively.
It compresses unsigned 16-bit image chunks using the existing Rust/C codec.
Lossless and camera-calibrated lossy modes are supported. Spatial prediction
never crosses channel or time boundaries: axis names are explicit in every
codec configuration, including images without a Z or C axis.

The companion tools import TIFF, export OME-TIFF and recompress existing
OME-Zarr multiscale hierarchies. This integration is independent of HDF5;
HDF5 libraries and the HDF5 filter ID are not involved.

## Install

Download the `radelta_zarr` wheel for your operating system and architecture
from the GitHub release, then install that file. For Windows x86-64:

```powershell
python -m pip install .\radelta_zarr-0.8.4-py3-none-win_amd64.whl
python -m radelta_zarr --help
```

Wheels bundle the matching native library: no Rust compiler, separate DLL
installation or `RADELTA_NATIVE_LIBRARY` setting is needed. Remove an existing
override if you want to use the wheel's bundled library. Python 3.11+ is
required. Wheels support Windows x86-64, Linux x86-64/ARM64 with glibc 2.28+,
and macOS 11+ on Intel/Apple Silicon. They are platform-specific `py3-none`
wheels because ctypes does not bind the library to a particular CPython ABI.
Python dependencies are installed by pip. PyPI publication is separate from
the GitHub release workflow; install the downloaded wheel directly.

Native release ZIPs include a Python package with the matching native library
under `python/zarr`. From an extracted native release:

```sh
python -m pip install ./python/zarr
python -m radelta_zarr --help
```

The **OME-Zarr CI** workflow builds/tests ZIPs and wheels on all five native
release platforms and uploads them as Actions artifacts. It can be run manually
after merging the workflow to the default branch. Tag releases also test the
installed wheel before publishing the ZIPs and wheels.

Linux release builds run in manylinux_2_28 images, then auditwheel checks the
native ABI and repairs the wheel. Windows uses delvewheel with explicit
analysis of the ctypes-loaded DLL; macOS uses delocate with a deployment target
of 11.0. Native ZIPs, wheels and ImageJ assets reuse the same native build per
platform. Wheels are installed into fresh virtual environments and checked
outside the source checkout before upload, including the full integration
suite. HDF5 remains a separate release asset with its own runtime and tools.

This also works in PowerShell. Installation requires Python 3.11 or later.
The package uses Zarr-Python 3 to read/write both storage formats; it does not
require installing the old Zarr-Python 2 package. Native assets are installed
with the Python package, and its generated wheel is marked for that platform.
On Windows use a matching 64-bit Python interpreter and the native runtime
required by the ordinary Radelta release.

For development, build the native codec, then install the Python package:

```sh
cargo build --release --locked --manifest-path native/Cargo.toml
python -m pip install -e ./zarr/python
```

Alternatively set `RADELTA_NATIVE_LIBRARY` to the full path of a Radelta 0.6.0+
DLL/shared library before running Python. On Windows:

```powershell
$env:RADELTA_NATIVE_LIBRARY = "C:\path\to\radelta.dll"
python -m pip install ./zarr/python
```

The loader checks an explicit environment path first, then packaged native
assets, then a development checkout and the current directory. Loading is lazy:
metadata inspection does not require the native library.

To build a distributable wheel locally, first stage the native release package.
Install `build`, `wheel` and `packaging`, plus your platform's repair tool
(`auditwheel`/`patchelf`, `delvewheel` or `delocate`), then run:

```sh
python scripts/package_zarr_wheel.py --platform windows --arch x86_64
python scripts/test_zarr_wheel.py
```

Repaired wheels are written to `dist/zarr-wheels`. Linux must use a compatible
manylinux build environment; renaming a wheel does not make a newer native ABI
compatible with glibc 2.28. `--manylinux-policy` selects another explicit policy
for local builds and fails if the library exceeds it. On macOS set
`MACOSX_DEPLOYMENT_TARGET=11.0` for both native compilation and wheel building.

## TIFF conversion

```sh
python -m radelta_zarr encode input.tif image.ome.zarr --chunks 4,256,256
python -m radelta_zarr info image.ome.zarr
python -m radelta_zarr decode image.ome.zarr restored.ome.tif
```

New stores default to OME-Zarr 0.5 / Zarr v3. Add `--zarr-format 2` for OME-Zarr
0.4 / Zarr v2. The TIFF importer supports a single grayscale uint16 series of
YX pages. It infers TIFF axes, converts them to t,c,z,y,x order and assembles
chunks on one producer thread while compression and writes overlap. Use
`--axes TZCYX` to override source axes. Unknown plain page
axes Q/I are interpreted as Z. Multi-series TIFF and volumetric TIFF pages are
rejected. A decoded-page LRU cache defaults to 64 MiB. When it holds the planes
spanned by one output chunk, each TIFF plane is decoded once, even with many XY
tiles. For `4,256,256` chunks in a ZYX image, this requires four full YX planes.
If the cache is smaller than that working set, pages may be decoded again;
pages larger than the limit are read without being cached.

Up to four CPU workers compress/write distinct whole chunks concurrently.
At most one chunk per worker is in flight; TIFF reads and progress callbacks
remain on the producer thread. Peak RAM includes the bounded page cache, a
decoded TIFF page, chunk assembly buffers and each worker's codec workspace;
it does not scale with the number of chunks or total volume size. Adjust with:

```sh
python -m radelta_zarr encode input.tif image.ome.zarr --chunks 4,256,256 --workers 4 --tiff-cache-mib 64
```

Use `--workers 1` for serial writes and `--tiff-cache-mib 0` to disable caching.
These settings do not change chunk layout, codec parameters or decoded values.

OME-TIFF physical pixel sizes, time increments and channel names/colours are
translated into OME-Zarr metadata. ImageJ spacing, XY resolution and time
interval are also supported. Override or supply missing calibration with:

```sh
python -m radelta_zarr encode input.tif calibrated.ome.zarr --voxel-size 2,0.5,0.5 --space-unit micrometer
```

`--voxel-size` is Z,Y,X, or Y,X for 2D. Without calibration, scales default to
one and physical units are omitted. The original TIFF description is retained
as provenance. Export produces BigTIFF/OME-TIFF, streaming one plane at a time,
and restores supported pixel-size/time units and channel names. It does not
recreate all original TIFF tags, arbitrary origins or complex OME metadata.
`--group A/1/0` selects a nested image, and `--level 1` selects a pyramid level.

## Existing multiscales, labels and plates

```sh
python -m radelta_zarr repack existing.ome.zarr compressed.ome.zarr
python -m radelta_zarr repack existing.ome.zarr compressed.ome.zarr --chunks 4,256,256
```

Repacking retains the input storage format, hierarchy, group/array attributes,
coordinate transforms, multiscale levels, display metadata and label metadata.
It finds declared multiscales recursively, including images in plate/well groups.
Only uint16 image/label arrays are recompressed; other arrays are copied without
modifying their codec or values. Non-array sidecars such as
`OME/METADATA.ome.xml` are retained. Unknown files inside a rewritten array's
payload directory are not retained; store sidecars alongside groups instead.
Copied consolidated metadata is regenerated after recompression.

Arrays eligible for Radelta must have unique axes chosen from t,c,z,y,x and
include y and x. Existing spatial axis permutations are supported, while new
images use canonical t,c,z,y,x order. Other axis names are rejected for uint16
multiscales. Metadata consistency is checked, including v3 dimension names.
`--chunks` must match every rewritten array's rank; omit it for heterogeneous
image/label ranks to retain each array's own chunks. V3 sharding is retained.

All uint16 labels are kept **lossless**, even during lossy image recompression;
other integer label types are copied verbatim. To quantise existing intensity
levels once using your camera calibration:

```sh
python -m radelta_zarr repack existing.ome.zarr lossy.ome.zarr --lossy --offset-adu 100 --gain-e-per-adu 0.46 --noise-step 2
```

Gain is electrons/ADU. The codec configuration stores the calibration. One
calibration applies to all rewritten intensity arrays. Averaged pyramid levels
have different noise statistics; this uniform setting does not automatically
adjust the quantisation step for their effective gain. Written
all-fill chunks are explicitly encoded in lossy mode, so Zarr cannot bypass
quantisation of zero pixels merely because they match the fill value. Implicit,
unwritten chunks retain the array's declared fill value.

Both conversion and repacking use new local output paths and report progress.
Existing outputs and nested input/output store paths are rejected. Repacking
first copies the source store to a temporary output, then rewrites selected
arrays one chunk at a time. RAM does not scale with total volume size, but disk
must accommodate the copied source and rewritten output during conversion.

## Pyramids

Add `--levels 3` to TIFF import to generate optional 2x XY mean levels:

```sh
python -m radelta_zarr encode input.tif pyramid.ome.zarr --levels 3
```

Level 0 is unchanged in lossless mode. Coarser levels are image-processing
outputs: adjacent XY blocks are averaged, rounded to uint16 and padded by
replicating the last row/column for odd sizes. Z, time and channel dimensions
are not downsampled. Scale and translation metadata locate each coarse pixel
at its source block's centre. Each level is completed before the next is
generated, with bounded concurrent chunk writes.

To avoid accumulating camera quantisation errors while generating levels,
`--lossy` TIFF import currently accepts one level. For a lossy pyramid, generate
a lossless pyramid first, then use lossy `repack` to quantise each level once.

## Python and partial access

```python
import numpy as np
import zarr
import radelta_zarr
from radelta_zarr import write_image

root = zarr.open_group("image.ome.zarr", mode="w", zarr_format=3)
write_image(root, np.zeros((5, 64, 64), dtype=np.uint16),
            axes="zyx", chunks=(2, 32, 32),
            scale=(2, 0.5, 0.5), units={a: "micrometer" for a in "zyx"})

root = zarr.open_group("image.ome.zarr", mode="r")
region = root["0"][2:3, 10:20, 30:40]
```

`create_ome_image` creates metadata and an empty level for normal Zarr slicing
and resize operations. `write_image` accepts NumPy or array-like sources with
`shape`, `dtype` and slicing. Its `workers` argument defaults to up to four
CPU workers; use `workers=1` for serial writes. Source slicing and progress
callbacks run on the calling thread. It can target any Zarr group/store supported by
Zarr-Python. CLI conversion/repacking currently targets local directory stores.
Each intersecting compressed chunk is decoded in full. Unrelated chunks are
not read. V3 Radelta serializers can also be nested inside Zarr sharding.

For direct arrays use `RadeltaCodec(axes="tcyx")` as a v3 serializer with
`compressors=[]`, or `RadeltaNumcodec(chunk_shape=..., axes=..., dtype="<u2")`
as a v2 compressor. The v2 codec also records C/F order and byte order.
Use the OME helpers for lossy arrays, or explicitly set
`config={"write_empty_chunks": True}` when creating a direct lossy Zarr array.

## Reader compatibility

The metadata follows OME-Zarr conventions, but **readers must implement the
custom Radelta codec**. Python code can import `radelta_zarr` before opening a
store; newer Zarr versions also discover its installed entry points. Browser
viewers, Fiji and other OME-Zarr readers do not automatically gain this codec
from the Python installation. No JavaScript/Java codec is provided here.

For an OME-Zarr reader that supports standard compression, transcode to Zstd:

```sh
python -m radelta_zarr repack compressed.ome.zarr compatible.ome.zarr --compression zstd
```

Use `--compression none` for uncompressed chunks. Recompression preserves the
current decoded values; converting lossy data cannot restore discarded detail.

## Codec format and tests

The codec identifier is `radelta.radelta`, not an HDF5 integer filter ID. Its
configuration schema version is 1. Every non-empty encoded chunk has a 20-byte
`RDZ1` envelope containing frame version, lossless/lossy mode, uncompressed byte
count and CRC32 over the header prefix and Radelta payload. The payload is the
existing RDL1/RDM2 or RDLQ/RDQ2 stream. Decode checks framing, checksum and native
chunk geometry against array metadata before allocating pixel output.

```sh
python -m unittest discover -s zarr/tests -v
```

Integration tests cover both formats, endian/order handling, all prediction
contexts, omitted axes, time/channel boundaries, edge chunks, lossy calibration,
zero/fill quantisation, partial updates and reads, corruption isolation,
sharding, metadata-only large images, pyramids, TIFF calibration/order/export,
multiscale recompression, lossless labels, sidecars and output protection.
