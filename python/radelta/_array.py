"""Public in-memory array operations backed by the existing C ABI."""
import ctypes as ct
import operator

import numpy as np

from ._layout import (CONTEXTS, calibration, canonical_array, check_output_size,
                      layout, pack_layout, unpack_layout)
from ._native import Options, LossyOptions, U8, bytes_buffer, library, metadata_bytes, pixel_pointer


def compress(array, axes=None, *, lossy=False, offset_adu=None, gain_e_per_adu=None,
             noise_step=2., block_depth=4, context="mean", scale_bits=10, metadata=b""):
    """Return standard Radelta bytes, preserving uint16 shape, axes and byte order.

    Arrays may be non-contiguous. Axes default to YX/ZYX/CZYX/TCZYX by rank.
    Lossy mode requires explicit camera offset and gain in electrons/ADU.
    """
    array = np.asarray(array)
    shape, axes, dtype = layout(array.shape, axes, array.dtype)
    offset, gain, step = calibration(lossy, offset_adu, gain_e_per_adu, noise_step)
    depth, bits = operator.index(block_depth), operator.index(scale_bits)
    if not 0 <= depth <= 0xffffffff or (bits != 0 and not 8 <= bits <= 11) or context not in CONTEXTS:
        raise ValueError("Invalid block_depth, context or scale_bits")
    source = np.require(canonical_array(array, axes), dtype=np.uint16, requirements=["C", "A"])
    options = Options(depth, CONTEXTS[context], bits)
    lossy_options = LossyOptions(depth, CONTEXTS[context], bits, offset, gain, step)
    output, size = U8(), ct.c_size_t()
    dll = library()
    t, c, z, y, x = source.shape
    status = dll.radelta_compress_u16_nd_alloc(pixel_pointer(source), source.size, x, y, z, c, t,
                                              None if lossy else ct.byref(options),
                                              ct.byref(lossy_options) if lossy else None,
                                              ct.byref(output), ct.byref(size))
    if status != 0:
        raise ValueError(f"Radelta encoding failed ({status})")
    try:
        payload = bytes_buffer(ct.string_at(output, size.value))
    finally:
        dll.radelta_buffer_free(output, size.value)
    meta = bytes_buffer(pack_layout(shape, axes, dtype, metadata))
    status = dll.radelta_set_metadata(payload, len(payload), meta, len(meta), None, 0, ct.byref(size))
    if status != 1:
        raise ValueError("Cannot attach array metadata")
    result = (ct.c_uint8 * size.value)()
    if dll.radelta_set_metadata(payload, len(payload), meta, len(meta), result, size.value, ct.byref(size)) != 0:
        raise ValueError("Cannot attach array metadata")
    return bytes(result)


def decompress(encoded, *, max_output_bytes=None):
    """Decode standard Radelta bytes to a uint16 array.

    Python-produced streams restore their exact shape, axis order and dtype.
    Other streams use canonical TCZYX order with leading singleton T/C/Z axes
    omitted. Streaming files should be read using open() or read().
    """
    source = bytes_buffer(encoded)
    dll = library()
    count, dims = ct.c_size_t(), [ct.c_uint32() for _ in range(5)]
    status = dll.radelta_decompress_u16_nd(source, len(source), None, 0, ct.byref(count),
                                        *(ct.byref(d) for d in dims))
    if status != 1:
        raise ValueError("Invalid or unsupported Radelta stream")
    canonical = tuple(d.value for d in dims)[::-1]
    shape, axes, dtype, _ = unpack_layout(metadata_bytes(dll.radelta_get_metadata, source, len(source)), canonical)
    check_output_size(shape, max_output_bytes)
    output = np.empty(canonical, dtype=np.uint16)
    status = dll.radelta_decompress_u16_nd(source, len(source), pixel_pointer(output), output.size,
                                        ct.byref(count), *(ct.byref(d) for d in dims))
    if status != 0:
        raise ValueError("Corrupt Radelta stream")
    order = ["tczyx".index(a) for a in axes] + [i for i, a in enumerate("tczyx") if a not in axes]
    return output.transpose(order).reshape(shape).astype(dtype, copy=False)
