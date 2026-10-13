"""One-pass native C ABI binding; no HDF5 dependency."""
import ctypes as ct
from functools import lru_cache
import math
import os
import operator
from pathlib import Path
import struct
import zlib

import numpy as np

VERSION = "0.7.0"
CODEC_ID = "radelta.radelta"
_FRAME = struct.Struct("<4sBBHQI")
_CONTEXT = {"signed3": 0, "signs": 1, "mean": 2, "mean-signs": 3}
MAX_CHUNK_BYTES = (1 << 32) - 1


class Options(ct.Structure):
    _fields_ = [("block_depth", ct.c_uint32), ("context_mode", ct.c_uint32), ("scale_bits", ct.c_uint32)]


class LossyOptions(ct.Structure):
    _fields_ = Options._fields_ + [("offset_adu", ct.c_double), ("gain_e_per_adu", ct.c_double), ("noise_step", ct.c_double)]


def validate_axes(axes, ndim=None):
    axes = axes.lower() if isinstance(axes, str) else "".join(axes).lower()
    if (not 2 <= len(axes) <= 5 or len(set(axes)) != len(axes) or
            any(a not in "tczyx" for a in axes) or not {"y", "x"}.issubset(axes) or
            (ndim is not None and len(axes) != ndim)):
        raise ValueError("Radelta axes must be unique t/c/z/y/x names including y and x, matching rank 2..5")
    return axes


def validate_options(options):
    allowed = {"lossy", "offset_adu", "gain_e_per_adu", "noise_step", "block_depth", "context", "scale_bits"}
    if set(options) - allowed:
        raise ValueError(f"Unknown codec options: {set(options)-allowed}")
    result = dict(lossy=False, offset_adu=0., gain_e_per_adu=1., noise_step=2.,
                  block_depth=4, context="mean", scale_bits=10)
    result.update(options)
    if not isinstance(result["lossy"], bool):
        raise ValueError("lossy must be boolean")
    for name in ("offset_adu", "gain_e_per_adu", "noise_step"):
        result[name] = float(result[name])
        if not math.isfinite(result[name]):
            raise ValueError(f"{name} must be finite")
    if result["gain_e_per_adu"] <= 0 or result["noise_step"] <= 0:
        raise ValueError("gain_e_per_adu and noise_step must be positive")
    for name in ("block_depth", "scale_bits"):
        n = result[name]
        if not isinstance(n, (int, np.integer)) or not 0 <= n <= 0xffffffff:
            raise ValueError(f"{name} must be a uint32 integer")
        result[name] = int(n)
    if result["scale_bits"] != 0 and not 8 <= result["scale_bits"] <= 11:
        raise ValueError("scale_bits must be 0 or 8..11")
    if result["context"] not in _CONTEXT:
        raise ValueError(f"Unsupported context: {result['context']}")
    return result


def validate_shape(shape, max_bytes=MAX_CHUNK_BYTES):
    shape = tuple(operator.index(n) for n in shape)
    if not 2 <= len(shape) <= 5 or any(n <= 0 or n > 0xffffffff for n in shape):
        raise ValueError("Chunk dimensions must be positive uint32 values of rank 2..5")
    if max_bytes is not None and math.prod(shape) * 2 > max_bytes:
        raise ValueError("A Radelta Zarr chunk must be smaller than 4 GiB")
    return shape


def uint16_dtype(dtype):
    dtype = np.dtype(dtype)
    if dtype.kind != "u" or dtype.itemsize != 2:
        raise ValueError("Radelta requires unsigned 16-bit pixels")
    return dtype


@lru_cache(maxsize=1)
def library():
    explicit = os.environ.get("RADELTA_NATIVE_LIBRARY")
    names = ("radelta.dll", "radelta_native.dll", "libradelta.so", "libradelta_native.so", "libradelta.dylib", "libradelta_native.dylib")
    package = Path(__file__).resolve().parent
    directories = [package / "natives", package.parents[2] / "native" / "target" / "release", Path.cwd()]
    candidates = [Path(explicit)] if explicit else [d / n for d in directories for n in names]
    selected = next((p.resolve() for p in candidates if p.is_file()), None)
    if selected is None:
        raise RuntimeError("Radelta native library not found. Install the Python package from a native release ZIP, "
                           "or set RADELTA_NATIVE_LIBRARY to the full DLL/shared-library path.")
    dll = ct.CDLL(str(selected))
    try:
        encode, free, decode = dll.radelta_compress_u16_nd_alloc, dll.radelta_buffer_free, dll.radelta_decompress_u16_nd
    except AttributeError as exc:
        raise RuntimeError("Native library lacks the allocating C API; use Radelta 0.6.0 or later") from exc
    ptr16, ptr8 = ct.POINTER(ct.c_uint16), ct.POINTER(ct.c_uint8)
    encode.argtypes = [ptr16, ct.c_size_t, *([ct.c_uint32]*5), ct.POINTER(Options), ct.POINTER(LossyOptions), ct.POINTER(ptr8), ct.POINTER(ct.c_size_t)]
    encode.restype = ct.c_int32
    free.argtypes = [ptr8, ct.c_size_t]
    free.restype = None
    decode.argtypes = [ptr8, ct.c_size_t, ptr16, ct.c_size_t, ct.POINTER(ct.c_size_t), *([ct.POINTER(ct.c_uint32)]*5)]
    decode.restype = ct.c_int32
    return dll


def _dims(shape, axes):
    sizes = dict(zip(axes, shape))
    return tuple(sizes.get(a, 1) for a in "xyzct")


def encode_chunk(pixels, axes, options):
    pixels = np.asarray(pixels)
    uint16_dtype(pixels.dtype)
    shape = validate_shape(pixels.shape)
    axes = validate_axes(axes, len(shape))
    native_axes = "".join(a for a in "tczyx" if a in axes)
    src = np.ascontiguousarray(pixels.transpose(tuple(axes.index(a) for a in native_axes)), dtype=np.uint16)
    opts = Options(options["block_depth"], _CONTEXT[options["context"]], options["scale_bits"])
    lossy = LossyOptions(opts.block_depth, opts.context_mode, opts.scale_bits,
                         options["offset_adu"], options["gain_e_per_adu"], options["noise_step"])
    encoded = ct.POINTER(ct.c_uint8)()
    size = ct.c_size_t()
    dll = library()
    status = dll.radelta_compress_u16_nd_alloc(src.ctypes.data_as(ct.POINTER(ct.c_uint16)), src.size,
                *_dims(shape, axes), None if options["lossy"] else ct.byref(opts),
                ct.byref(lossy) if options["lossy"] else None, ct.byref(encoded), ct.byref(size))
    if status != 0:
        raise RuntimeError(f"Native Radelta encoding failed ({status})")
    try:
        payload = ct.string_at(encoded, size.value)
    finally:
        dll.radelta_buffer_free(encoded, size.value)
    prefix = struct.pack("<4sBBHQ", b"RDZ1", 1, int(options["lossy"]), 0, src.nbytes)
    checksum = zlib.crc32(payload, zlib.crc32(prefix))
    return prefix + struct.pack("<I", checksum) + payload


def decode_chunk(encoded, shape, axes, dtype, options):
    shape = validate_shape(shape)
    axes = validate_axes(axes, len(shape))
    dtype = uint16_dtype(dtype)
    encoded = bytes(encoded)
    if len(encoded) < _FRAME.size:
        raise ValueError("Truncated Radelta Zarr chunk")
    magic, version, mode, reserved, raw_size, checksum = _FRAME.unpack_from(encoded)
    if (magic != b"RDZ1" or version != 1 or reserved != 0 or mode != int(options["lossy"]) or
            raw_size != math.prod(shape)*2):
        raise ValueError("Radelta Zarr chunk header does not match its array metadata")
    payload = encoded[_FRAME.size:]
    if zlib.crc32(payload, zlib.crc32(encoded[:16])) != checksum:
        raise ValueError("Radelta Zarr chunk checksum mismatch")
    if payload[:4] not in ((b"RDLQ", b"RDQ2") if mode else (b"RDL1", b"RDM2")):
        raise ValueError("Radelta payload mode does not match chunk metadata")
    # The native header must agree with the caller's chunk geometry before any
    # output allocation; a damaged payload cannot request an arbitrary buffer.
    src = (ct.c_uint8 * len(payload)).from_buffer_copy(payload)
    count, dims = ct.c_size_t(), [ct.c_uint32() for _ in range(5)]
    dll = library()
    status = dll.radelta_decompress_u16_nd(src, len(payload), None, 0, ct.byref(count), *(ct.byref(d) for d in dims))
    if status != 1 or tuple(d.value for d in dims) != _dims(shape, axes) or count.value != math.prod(shape):
        raise ValueError("Radelta payload dimensions do not match chunk geometry")
    data = np.empty(count.value, dtype=np.uint16)
    status = dll.radelta_decompress_u16_nd(src, len(payload), data.ctypes.data_as(ct.POINTER(ct.c_uint16)),
                data.size, ct.byref(count), *(ct.byref(d) for d in dims))
    if status != 0:
        raise ValueError(f"Native Radelta decoding failed ({status})")
    native_axes = "".join(a for a in "tczyx" if a in axes)
    sizes = dict(zip(axes, shape))
    result = data.reshape(tuple(sizes[a] for a in native_axes)).transpose(tuple(native_axes.index(a) for a in axes))
    return result.astype(dtype, copy=False)
