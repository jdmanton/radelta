"""Chunked uint16 HDF5 datasets with automatic bundled-filter registration."""
import ctypes
import math
import operator
import os
import struct
import sys
from pathlib import Path

import h5py
import numpy as np

FILTER_ID = 65024  # Private/application ID; not an HDF Group registration.
CONTEXTS = {"signed3": 0, "signs": 1, "mean": 2, "mean-signs": 3}
# Keep native callbacks alive for as long as HDF5 can use them.
_plugin_libraries = {}


def configure_plugin(directory=None):
    """Register the bundled filter, or add an explicit SDK/filter directory."""
    bundled = directory is None
    if directory is None:
        directory = Path(__file__).resolve().parent / "plugins"
    directory = Path(directory).resolve()
    if not directory.is_dir():
        raise ValueError(f"Plugin directory does not exist: {directory}")
    if bundled:
        name = ("H5Zradelta.dll" if os.name == "nt" else
                "libH5Zradelta.dylib" if sys.platform == "darwin" else "libH5Zradelta.so")
        path = directory / name
        if path not in _plugin_libraries:
            # ctypes uses Windows' modern DLL search flags, including the
            # dependency directories added by delvewheel. HDF5's legacy
            # LoadLibraryEx search does not find those directories reliably.
            _plugin_libraries[path] = ctypes.CDLL(str(path))
        library = _plugin_libraries[path]
        register = getattr(h5py.h5z, "register_filter", None)
        if register is not None and not h5py.h5z.filter_avail(FILTER_ID):
            info = library.H5PLget_plugin_info
            info.argtypes = []
            info.restype = ctypes.c_void_p
            pointer = info()
            if not pointer or not register(pointer):
                raise RuntimeError(f"Cannot register bundled Radelta filter: {path}")
        # h5py 3.8 has no register_filter(). Its discovery can reuse the DLL
        # already loaded above, without resolving its dependencies again.
    h5py.h5pl.prepend(os.fsencode(directory))


# Source checkouts can still use an explicit SDK build. Installed native wheels
# register their runtime-independent filter with h5py's own HDF5 library.
_plugins = Path(__file__).resolve().parent / "plugins"
if _plugins.is_dir():
    configure_plugin()


def filter_options(*, lossy=False, offset_adu=0.0, gain_e_per_adu=1.0,
                   noise_step=2.0, block_depth=4, context="mean", scale_bits=10):
    block_depth, scale_bits = operator.index(block_depth), operator.index(scale_bits)
    if not 0 <= block_depth <= 0xffffffff:
        raise ValueError("block_depth must fit uint32")
    if scale_bits != 0 and not 8 <= scale_bits <= 11:
        raise ValueError("scale_bits must be 0 or 8..11")
    if context not in CONTEXTS:
        raise ValueError(f"context must be one of {tuple(CONTEXTS)}")
    values = tuple(float(v) for v in (offset_adu, gain_e_per_adu, noise_step))
    if not all(math.isfinite(v) for v in values) or values[1] <= 0 or values[2] <= 0:
        raise ValueError("Calibration must be finite; gain and noise_step must be positive")
    words = tuple(w for v in values for w in struct.unpack("<II", struct.pack("<d", v)))
    return (1, int(bool(lossy)), block_depth, CONTEXTS[context], scale_bits, *words)


def default_chunks(shape, target_mib=8):
    """Choose spatial chunks near target size, keeping T/C chunk extents at one."""
    shape = tuple(operator.index(v) for v in shape)
    if not 2 <= len(shape) <= 5 or any(v <= 0 for v in shape):
        raise ValueError("Expected positive YX, ZYX, CZYX or TCZYX dimensions")
    if not math.isfinite(target_mib) or target_mib <= 0:
        raise ValueError("target_mib must be positive")
    chunks = [1] * len(shape)
    chunks[-1] = min(shape[-1], 1024)
    chunks[-2] = min(shape[-2], 1024)
    if len(shape) >= 3:
        chunks[-3] = min(shape[-3], 4)
    target = max(2, int(target_mib * 1024**2))
    while math.prod(chunks) * 2 > target:
        candidates = range(max(0, len(shape)-3), len(shape))
        axis = max(candidates, key=lambda i: chunks[i])
        chunks[axis] = (chunks[axis] + 1) // 2
    return tuple(chunks)


def create_dataset(group, name, *, shape=None, data=None, chunks=None,
                   dtype=np.uint16, maxshape=None, target_mib=8,
                   filter_id=FILTER_ID, **codec):
    """Create a mandatory Radelta-filtered dataset; usual h5py slicing applies.

    Rank 2/3/4/5 means YX/ZYX/CZYX/TCZYX respectively. For time series with no
    channels, use a singleton C axis. This keeps independent volumes separate.
    """
    if data is not None:
        data = np.asarray(data)
        if shape is None:
            shape = data.shape
    if shape is None:
        raise ValueError("Provide shape or data")
    shape = tuple(operator.index(v) for v in shape)
    dtype = np.dtype(dtype)
    if dtype.kind != "u" or dtype.itemsize != 2:
        raise ValueError("Radelta requires uint16")
    if not 2 <= len(shape) <= 5 or any(v < 0 for v in shape):
        raise ValueError("Expected rank 2..5 in canonical TCZYX order")
    if data is not None and (data.shape != shape or data.dtype.kind != "u" or data.dtype.itemsize != 2):
        raise ValueError("Data must be uint16 and match shape")
    if chunks is None:
        chunks = default_chunks(tuple(max(1, v) for v in shape), target_mib)
    chunks = tuple(operator.index(v) for v in chunks)
    if len(chunks) != len(shape) or any(v <= 0 or v > 0xffffffff for v in chunks):
        raise ValueError("Chunks must have one positive uint32 extent per axis")
    if math.prod(chunks) * 2 >= 2**32:
        raise ValueError("An HDF5 chunk must be smaller than 4 GiB")
    if not h5py.h5z.filter_avail(filter_id):
        raise RuntimeError("Radelta HDF5 filter is unavailable. Install the self-contained "
                           "radelta-hdf5 wheel, or call configure_plugin(directory) with a filter build.")
    sizes = dict(zip("TCZYX"[-len(chunks):], chunks))
    opts = filter_options(**codec) + tuple(sizes.get(axis, 1) for axis in "XYZCT") + (
        int(dtype.str.startswith(">")),)
    config = h5py.h5z.get_filter_info(filter_id)
    if not config & h5py.h5z.FILTER_CONFIG_ENCODE_ENABLED:
        raise RuntimeError("Radelta filter cannot encode")
    # The embedded filter cannot derive geometry through HDF5 callbacks, so
    # supply all 17 parameters and make the filter mandatory ourselves. This
    # also works with SDK filters, which independently check the geometry.
    dcpl = h5py.h5p.create(h5py.h5p.DATASET_CREATE)
    dcpl.set_chunk(chunks)
    dcpl.set_filter(filter_id, 0, opts)
    ds = group.create_dataset(name, shape=shape, dtype=dtype, chunks=chunks,
                              maxshape=maxshape, dcpl=dcpl)
    ds.attrs["axes"] = "TCZYX"[-len(shape):]
    ds.attrs["radelta_version"] = "0.8.9"
    ds.attrs["radelta_lossy"] = bool(codec.get("lossy", False))
    if codec.get("lossy", False):
        for key, default in (("offset_adu", 0.0), ("gain_e_per_adu", 1.0), ("noise_step", 2.0)):
            ds.attrs[key] = codec.get(key, default)
    if data is not None:
        ds[...] = data
    return ds


# Import after the filter helpers to keep the public API in one namespace.
from .bdv import export_bdv  # noqa: E402
