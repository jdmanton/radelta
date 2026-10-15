"""Plane-wise .rdlt and optional HDF5 file access."""
import ctypes as ct
import itertools
import operator
import os
from pathlib import Path
import tempfile
import threading

import numpy as np

from ._layout import (calibration, canonical_array, canonical_shape, check_output_size,
                      layout, pack_layout, unpack_layout)
from ._native import (FileInfo, HANDLE, bytes_buffer, check_file, library,
                      metadata_bytes, pixel_pointer)


def _hdf5():
    try:
        import h5py
        import radelta_hdf5
    except ImportError as exc:
        raise ImportError("HDF5 support requires the matching radelta-hdf5 wheel (or radelta[hdf5])") from exc
    return h5py, radelta_hdf5


def _path_bytes(path):
    encoded = str(path).encode("utf-8")
    if b"\0" in encoded:
        raise ValueError("Paths cannot contain NUL characters")
    return encoded


class Dataset:
    """Read-only array-like file view. Use as a context manager to close it.

    Supports integers, slices and one ellipsis. A request decodes full XY
    planes (or intersecting HDF5 chunks), and allocates only the selected output.
    Concurrent reads on this object are serialised around its native cache.
    """
    def __init__(self, path, *, dataset="/images"):
        self._handle = HANDLE()
        self._h5 = None
        self._lock = threading.RLock()
        self.closed = True
        self.path = Path(path)
        try:
            if self.path.suffix.lower() in (".h5", ".hdf5", ".he5"):
                h5py, _ = _hdf5()
                self._h5 = h5py.File(self.path, "r")
                self._data = self._h5[dataset]
                if not isinstance(self._data, h5py.Dataset):
                    raise ValueError("dataset must name an HDF5 dataset")
                axes = self._data.attrs.get("axes", "tczyx"[-self._data.ndim:])
                if isinstance(axes, bytes):
                    axes = axes.decode("ascii")
                self._physical_shape, self._physical_axes, physical_dtype = layout(
                    self._data.shape, axes, self._data.dtype)
                self._dims = canonical_shape(self._physical_shape, self._physical_axes)
                encoded = self._data.attrs.get("radelta_python_layout")
                if encoded is None:
                    self.shape, self.axes, self.dtype, self.metadata = (
                        self._physical_shape, self._physical_axes, physical_dtype, b"")
                else:
                    self.shape, self.axes, self.dtype, self.metadata = unpack_layout(bytes(encoded), self._dims)
            else:
                dll = library()
                check_file(dll.radelta_file_open(_path_bytes(self.path), ct.byref(self._handle)))
                info = FileInfo()
                check_file(dll.radelta_file_get_info(self._handle, ct.byref(info)))
                self._dims = (info.t, info.c, info.z, info.y, info.x)
                data = metadata_bytes(dll.radelta_file_get_metadata, self._handle)
                self.shape, self.axes, self.dtype, self.metadata = unpack_layout(data, self._dims)
            self.ndim = len(self.shape)
            self.closed = False
        except BaseException:
            self.close()
            raise

    def _plane(self, t, c, z, ys, xs):
        if self._h5 is not None:
            # h5py requires positive slice steps. Read the selected region in
            # increasing order, then reverse the result where requested.
            regions = {}
            for axis, selected in (("y", ys), ("x", xs)):
                ascending = selected if selected.step > 0 else selected[::-1]
                regions[axis] = slice(ascending[0], ascending[-1]+1, ascending.step)
            mapping = dict(t=t, c=c, z=z, **regions)
            pixels = self._data[tuple(mapping[a] for a in self._physical_axes)]
            remaining = [a for a in self._physical_axes if a in "yx"]
            plane = np.asarray(pixels).transpose([remaining.index(a) for a in "yx"])
            return plane[::1 if ys.step > 0 else -1, ::1 if xs.step > 0 else -1]
        plane = np.empty(self._dims[-2:], dtype=np.uint16)
        check_file(library().radelta_file_read_plane_u16(self._handle, t, c, z,
                                                       pixel_pointer(plane), plane.size))
        return plane[np.ix_(list(ys), list(xs))]

    def __getitem__(self, key):
        with self._lock:
            if self.closed:
                raise ValueError("Dataset is closed")
            key = key if isinstance(key, tuple) else (key,)
            if sum(item is Ellipsis for item in key) > 1:
                raise IndexError("Only one ellipsis is supported")
            expanded = []
            for item in key:
                expanded.extend([slice(None)] * (self.ndim-len(key)+1) if item is Ellipsis else [item])
            if len(expanded) > self.ndim:
                raise IndexError("Too many indices")
            expanded += [slice(None)] * (self.ndim-len(expanded))
            selected = {}
            squeezed = []
            for axis, size, item in zip(self.axes, self.shape, expanded):
                if isinstance(item, slice):
                    selected[axis] = range(*item.indices(size))
                else:
                    if isinstance(item, (bool, np.bool_)):
                        raise TypeError("Only integer and slice indexing is supported")
                    index = operator.index(item)
                    if index < 0:
                        index += size
                    if not 0 <= index < size:
                        raise IndexError(f"Index out of bounds for axis {axis}")
                    selected[axis] = range(index, index+1)
                    squeezed.append(self.axes.index(axis))
            ranges = [selected.get(a, range(1)) for a in "tczyx"]
            output = np.empty(tuple(map(len, ranges)), dtype=np.uint16)
            if output.size:
                for ot, oc, oz in np.ndindex(output.shape[:3]):
                    output[ot, oc, oz] = self._plane(ranges[0][ot], ranges[1][oc], ranges[2][oz],
                                                     ranges[3], ranges[4])
            order = ["tczyx".index(a) for a in self.axes] + [i for i, a in enumerate("tczyx") if a not in self.axes]
            result = output.transpose(order).reshape(tuple(len(selected[a]) for a in self.axes))
            result = result.astype(self.dtype, copy=False)
            return np.squeeze(result, axis=tuple(squeezed))[()]

    def close(self):
        with self._lock:
            if self._handle:
                library().radelta_file_close(self._handle)
                self._handle = HANDLE()
            if self._h5 is not None:
                self._h5.close()
                self._h5 = None
            self.closed = True

    def __enter__(self):
        if self.closed:
            raise ValueError("Dataset is closed")
        return self

    def __exit__(self, *_):
        self.close()

    def __del__(self):
        if hasattr(self, "_lock"):
            self.close()


def open(path, *, dataset="/images"):
    """Open a .rdlt or HDF5 file for lazy integer/slice access."""
    return Dataset(path, dataset=dataset)


def read(path, *, dataset="/images", max_output_bytes=None):
    """Load a complete file array; use open() to read selected regions instead."""
    with open(path, dataset=dataset) as source:
        check_output_size(source.shape, max_output_bytes)
        return source[...]


def write(path, array, axes=None, *, lossy=False, offset_adu=None, gain_e_per_adu=None,
          noise_step=2., metadata=b"", memory_mib=256, overwrite=False,
          dataset="/images", chunks=None):
    """Write uint16 data to .rdlt or HDF5, publishing only completed output.

    .rdlt writing streams TCZ planes using native defaults. HDF5 chunks follow
    canonical TCZYX order with leading singleton axes omitted. Existing files
    require overwrite=True. Array shape, axes, dtype and opaque metadata survive
    a Python round trip; pixel predictions never cross T/C boundaries.
    """
    array = np.asarray(array)
    shape, axes, dtype = layout(array.shape, axes, array.dtype)
    offset, gain, step = calibration(lossy, offset_adu, gain_e_per_adu, noise_step)
    memory_mib = operator.index(memory_mib)
    if not 1 <= memory_mib <= 0xffffffff:
        raise ValueError("memory_mib must be a positive uint32 integer")
    path = Path(path)
    _path_bytes(path)
    if path.exists() and not overwrite:
        raise FileExistsError(path)
    encoded_metadata = pack_layout(shape, axes, dtype, metadata)
    pixels = canonical_array(array, axes)
    fd, name = tempfile.mkstemp(prefix=".radelta-python-", suffix=path.suffix, dir=path.parent)
    os.close(fd)
    temporary = Path(name)
    try:
        if path.suffix.lower() in (".h5", ".hdf5", ".he5"):
            h5py, adapter = _hdf5()
            first = min("tczyx".index(a) for a in axes)
            physical_shape = pixels.shape[first:]
            with h5py.File(temporary, "w") as f:
                ds = adapter.create_dataset(f, dataset, shape=physical_shape, dtype=dtype, chunks=chunks,
                                            lossy=lossy, offset_adu=offset, gain_e_per_adu=gain,
                                            noise_step=step)
                ds.attrs["radelta_python_layout"] = np.void(encoded_metadata)
                for selection in ds.iter_chunks():
                    ds[selection] = pixels.reshape(physical_shape)[selection]
        else:
            if path.suffix.lower() not in (".rdlt", ".radelta"):
                raise ValueError("Output must end in .rdlt, .radelta, .h5, .hdf5 or .he5")
            if chunks is not None:
                raise ValueError("chunks applies only to HDF5 output")
            dll, handle = library(), HANDLE()
            t, c, z, y, x = pixels.shape
            try:
                if lossy:
                    status = dll.radelta_writer_create_lossy_u16(_path_bytes(temporary), x, y, z, c, t,
                                                                 offset, gain, step, memory_mib, ct.byref(handle))
                else:
                    status = dll.radelta_writer_create_lossless_u16(_path_bytes(temporary), x, y, z, c, t,
                                                                    memory_mib, ct.byref(handle))
                check_file(status)
                meta = bytes_buffer(encoded_metadata)
                check_file(dll.radelta_writer_set_metadata(handle, meta, len(meta)))
                for tc in itertools.product(range(t), range(c), range(z)):
                    plane = np.require(pixels[tc], dtype=np.uint16, requirements=["C", "A"])
                    check_file(dll.radelta_writer_write_plane_u16(handle, *tc, pixel_pointer(plane), plane.size))
                check_file(dll.radelta_writer_finish(handle))
            finally:
                if handle:
                    dll.radelta_writer_close(handle)
        if overwrite:
            os.replace(temporary, path)
        else:
            os.link(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)
