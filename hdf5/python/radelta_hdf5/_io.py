"""Shared chunk iteration and TIFF layout validation for HDF5 exporters."""
import itertools
import math

import numpy as np


def chunk_slices(shape, chunks):
    for origin in itertools.product(*(range(0, n, c) for n, c in zip(shape, chunks))):
        yield tuple(slice(o, min(o+c, n)) for o, c, n in zip(origin, chunks, shape))


def canonical_tiff(series, axes_override):
    axes = (axes_override or series.axes).upper()
    # Plain multipage stacks often use an unspecified Q or I page axis.
    if len(axes) == 3 and axes[0] in "QI" and axes[1:] == "YX":
        axes = "ZYX"
    if (len(axes) != len(series.shape) or len(set(axes)) != len(axes) or
            not axes.endswith("YX") or any(a not in "TCZYX" for a in axes)):
        raise ValueError(f"Unsupported TIFF axes {axes!r}; use --axes with TCZYX axes ending in YX")
    if np.dtype(series.dtype).kind != "u" or np.dtype(series.dtype).itemsize not in (1, 2):
        raise ValueError("TIFF data must be grayscale uint8 or uint16")
    if len(series.pages) != math.prod(series.shape[:-2]):
        raise ValueError("TIFF must store one YX plane per page; volumetric TIFF pages are unsupported")
    if any(tuple(p.shape) != tuple(series.shape[-2:]) for p in series.pages):
        raise ValueError("TIFF pages must be grayscale YX planes")
    # Preserve singleton axes needed to disambiguate T, C and Z.
    first = min("TCZYX".index(a) for a in axes)
    out_axes = "TCZYX"[first:]
    sizes = dict(zip(axes, series.shape))
    shape = tuple(sizes.get(a, 1) for a in out_axes)
    return axes, out_axes, shape
