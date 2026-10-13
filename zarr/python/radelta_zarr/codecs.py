"""Versioned Zarr v3 serializer and Zarr v2 numcodecs compressor."""
import asyncio
from dataclasses import dataclass

import numpy as np
from numcodecs.abc import Codec
from zarr.abc.codec import ArrayBytesCodec

from ._native import (CODEC_ID, decode_chunk, encode_chunk, uint16_dtype,
                      validate_axes, validate_options, validate_shape)


@dataclass(frozen=True, init=False)
class RadeltaCodec(ArrayBytesCodec):
    """Zarr v3 array-to-bytes codec, with spatial axes explicit in metadata."""
    axes: str
    options: dict
    is_fixed_size = False

    def __init__(self, axes="zyx", **options):
        object.__setattr__(self, "axes", validate_axes(axes))
        object.__setattr__(self, "options", validate_options(options))

    def to_dict(self):
        return {"name": CODEC_ID, "configuration": {"version": 1, "axes": self.axes, **self.options}}

    @classmethod
    def from_dict(cls, data):
        if data.get("name") != CODEC_ID:
            raise ValueError("Not a Radelta codec configuration")
        config = dict(data.get("configuration", {}))
        if config.pop("version", None) != 1:
            raise ValueError("Unsupported Radelta Zarr codec configuration version")
        return cls(**config)

    def validate(self, *, shape, dtype, chunk_grid):
        validate_axes(self.axes, len(shape))
        uint16_dtype(dtype.to_native_dtype())

    def compute_encoded_size(self, input_byte_length, chunk_spec):
        raise NotImplementedError("Radelta compressed chunks have variable sizes")

    async def _encode_single(self, chunk_array, chunk_spec):
        data = chunk_array.as_numpy_array()
        encoded = await asyncio.to_thread(encode_chunk, data, self.axes, self.options)
        return chunk_spec.prototype.buffer.from_bytes(encoded)

    async def _decode_single(self, chunk_bytes, chunk_spec):
        decoded = await asyncio.to_thread(decode_chunk, chunk_bytes.to_bytes(), chunk_spec.shape,
                                         self.axes, chunk_spec.dtype.to_native_dtype(), self.options)
        return chunk_spec.prototype.nd_buffer.from_numpy_array(decoded)


class RadeltaNumcodec(Codec):
    """Zarr v2 compressor: full chunk shape, axes, dtype and order are explicit."""
    codec_id = CODEC_ID

    def __init__(self, chunk_shape, axes="zyx", dtype="<u2", order="C", version=1, **options):
        if version != 1:
            raise ValueError("Unsupported Radelta Zarr codec configuration version")
        self.chunk_shape = validate_shape(chunk_shape)
        self.axes = validate_axes(axes, len(self.chunk_shape))
        self.dtype = uint16_dtype(dtype).str
        if order not in {"C", "F"}:
            raise ValueError("order must be C or F")
        self.order, self.options = order, validate_options(options)

    def get_config(self):
        return {"id": CODEC_ID, "version": 1, "chunk_shape": list(self.chunk_shape),
                "axes": self.axes, "dtype": self.dtype, "order": self.order, **self.options}

    def encode(self, buf):
        if isinstance(buf, np.ndarray):
            raw = buf.tobytes(order=self.order)
        else:
            raw = bytes(buf)
        if len(raw) != np.prod(self.chunk_shape, dtype=np.int64)*2:
            raise ValueError("Input byte count does not match Radelta chunk shape")
        data = np.frombuffer(raw, dtype=self.dtype).reshape(self.chunk_shape, order=self.order)
        return encode_chunk(data, self.axes, self.options)

    def decode(self, buf, out=None):
        data = decode_chunk(buf, self.chunk_shape, self.axes, self.dtype, self.options)
        raw = data.tobytes(order=self.order)
        if out is None:
            return raw
        if isinstance(out, np.ndarray):
            if out.nbytes != len(raw):
                raise ValueError("Output buffer has the wrong byte count")
            out[...] = np.frombuffer(raw, dtype=out.dtype).reshape(out.shape, order=self.order)
        else:
            view = memoryview(out).cast("B")
            if len(view) != len(raw):
                raise ValueError("Output buffer has the wrong byte count")
            view[:] = raw
        return out
