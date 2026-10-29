"""Array layout recorded in the existing opaque Radelta metadata field."""
import json
import math
import operator

import numpy as np

PREFIX = b"radelta-python:1\n"
CONTEXTS = {"signed3": 0, "signs": 1, "mean": 2, "mean-signs": 3}


def layout(shape, axes=None, dtype=np.uint16):
    shape = tuple(operator.index(v) for v in shape)
    axes = "tczyx"[-len(shape):] if axes is None else axes
    if not isinstance(axes, str):
        raise ValueError("axes must be a TCZYX string")
    axes = axes.lower()
    dtype = np.dtype(dtype)
    if (not 2 <= len(shape) <= 5 or len(axes) != len(shape) or len(set(axes)) != len(axes) or
            any(a not in "tczyx" for a in axes) or not {"y", "x"}.issubset(axes) or
            any(n <= 0 or n > 0xffffffff for n in shape)):
        raise ValueError("Expected positive uint32 dimensions of rank 2..5, with unique TCZYX axes including Y and X")
    if dtype.kind != "u" or dtype.itemsize not in (1, 2):
        raise ValueError("Radelta requires uint8 or uint16 pixels")
    return shape, axes, dtype


def canonical_shape(shape, axes):
    sizes = dict(zip(axes, shape))
    return tuple(sizes.get(a, 1) for a in "tczyx")


def canonical_array(array, axes):
    order = [axes.index(a) for a in "tczyx" if a in axes]
    return array.transpose(order).reshape(canonical_shape(array.shape, axes))


def pack_layout(shape, axes, dtype, metadata):
    if not isinstance(metadata, (bytes, bytearray, memoryview)):
        raise TypeError("metadata must be bytes-like")
    header = json.dumps(dict(shape=shape, axes=axes, dtype=np.dtype(dtype).str), separators=(",", ":")).encode()
    return PREFIX + header + b"\n" + bytes(metadata)


def unpack_layout(data, dims):
    if not data.startswith(PREFIX):
        first = next((i for i, n in enumerate(dims[:-2]) if n != 1), 3)
        return (*layout(dims[first:], "tczyx"[first:]), data)
    try:
        header, metadata = data[len(PREFIX):].split(b"\n", 1)
        recorded = json.loads(header)
        shape, axes, dtype = layout(recorded["shape"], recorded["axes"], recorded["dtype"])
        if canonical_shape(shape, axes) != tuple(dims):
            raise ValueError("Array layout disagrees with encoded dimensions")
        return shape, axes, dtype, metadata
    except (ValueError, TypeError, KeyError, AttributeError) as exc:
        raise ValueError("Invalid Radelta Python array metadata") from exc


def calibration(lossy, offset_adu, gain_e_per_adu, noise_step):
    if not isinstance(lossy, bool):
        raise ValueError("lossy must be boolean")
    if lossy and (offset_adu is None or gain_e_per_adu is None):
        raise ValueError("Lossy compression requires offset_adu and gain_e_per_adu")
    values = (0. if offset_adu is None else float(offset_adu),
              1. if gain_e_per_adu is None else float(gain_e_per_adu), float(noise_step))
    if not all(math.isfinite(v) for v in values) or values[1] <= 0 or values[2] <= 0:
        raise ValueError("Calibration must be finite; gain and noise_step must be positive")
    return values


def check_output_size(shape, limit):
    if limit is not None and math.prod(shape)*2 > operator.index(limit):
        raise ValueError("Decoded array exceeds max_output_bytes; use open() for partial file access")
