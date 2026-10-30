"""Radelta-compressed OME-Zarr 0.4/0.5 images."""
from numcodecs.registry import register_codec as _register_numcodec
from zarr.registry import register_codec as _register_zarr_codec

from ._native import CODEC_ID, VERSION
from .codecs import RadeltaCodec, RadeltaNumcodec

_register_numcodec(RadeltaNumcodec)
_register_zarr_codec(CODEC_ID, RadeltaCodec)

__version__ = VERSION

# Register codecs before importing store-access helpers.
from .ome import create_ome_image, repack, write_image  # noqa: E402,F401
