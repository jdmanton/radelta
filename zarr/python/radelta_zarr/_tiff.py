"""Plane-wise TIFF source and physical metadata extraction."""
import itertools
from collections import OrderedDict
import operator
import xml.etree.ElementTree as ET

import numpy as np

from ._native import validate_axes

_UNITS = {"µm": "micrometer", "μm": "micrometer", "um": "micrometer", "micron": "micrometer",
          "nm": "nanometer", "mm": "millimeter", "cm": "centimeter", "m": "meter",
          "s": "second", "sec": "second", "ms": "millisecond", "us": "microsecond", "min": "minute"}


def _unit(value):
    return _UNITS.get(value, value)


class TiffSource:
    def __init__(self, tif, axes=None, *, cache_bytes=64*1024**2):
        self.cache_bytes = operator.index(cache_bytes)
        if self.cache_bytes < 0:
            raise ValueError("TIFF cache size must be non-negative")
        self._pages = OrderedDict()
        self._cached_bytes = 0
        if len(tif.series) != 1:
            raise ValueError("TIFF import currently requires a single image series")
        self.series = tif.series[0]
        source_axes = (axes or self.series.axes).lower()
        if len(source_axes) == 3 and source_axes[0] in "qi" and source_axes.endswith("yx"):
            source_axes = "zyx"
        self.source_axes = validate_axes(source_axes, len(self.series.shape))
        if not self.source_axes.endswith("yx"):
            raise ValueError("TIFF source axes must end in YX")
        self.axes = "".join(a for a in "tczyx" if a in self.source_axes)
        sizes = dict(zip(self.source_axes, self.series.shape))
        self.shape = tuple(sizes[a] for a in self.axes)
        self.dtype = np.dtype(self.series.dtype)
        if self.dtype.kind != "u" or self.dtype.itemsize not in (1, 2):
            raise ValueError("TIFF source must be grayscale uint8 or uint16")
        if len(self.series.pages) != int(np.prod(self.series.shape[:-2])):
            raise ValueError("TIFF must have one YX plane per page; volumetric pages are unsupported")
        if any(tuple(p.shape) != tuple(self.series.shape[-2:]) for p in self.series.pages):
            raise ValueError("TIFF must have grayscale YX pages")
        self.scale, self.units, self.omero = {}, {}, None
        if tif.ome_metadata:
            xml = ET.fromstring(tif.ome_metadata)
            pixels = next((e for e in xml.iter() if e.tag.split("}")[-1] == "Pixels"), None)
            if pixels is not None:
                for a in "zyx":
                    value = pixels.get("PhysicalSize"+a.upper())
                    if value is not None and a in self.axes:
                        self.scale[a] = float(value)
                        self.units[a] = _unit(pixels.get("PhysicalSize"+a.upper()+"Unit", "µm"))
                if pixels.get("TimeIncrement") is not None and "t" in self.axes:
                    self.scale["t"] = float(pixels.get("TimeIncrement"))
                    self.units["t"] = _unit(pixels.get("TimeIncrementUnit", "s"))
                channels = []
                for e in pixels:
                    if e.tag.split("}")[-1] != "Channel":
                        continue
                    color = f"{(int(e.get('Color')) & 0xffffffff) >> 8:06X}" if e.get("Color") else "FFFFFF"
                    channels.append({"label": e.get("Name", e.get("ID", "channel")), "color": color,
                                     "window": {"min": 0, "max": int(np.iinfo(self.dtype).max), "start": 0, "end": int(np.iinfo(self.dtype).max)}, "active": True})
                if "c" in self.axes and len(channels) == sizes["c"]:
                    self.omero = {"channels": channels}
        elif tif.imagej_metadata:
            metadata = tif.imagej_metadata
            unit = _unit(metadata.get("unit", ""))
            if unit and unit != "pixel":
                for a, tag_name in (("x", "XResolution"), ("y", "YResolution")):
                    tag = tif.pages[0].tags.get(tag_name)
                    if tag and tag.value[0] > 0:
                        self.scale[a] = tag.value[1]/tag.value[0]
                        self.units[a] = unit
                if "z" in self.axes and "spacing" in metadata:
                    self.scale["z"] = float(metadata["spacing"])
                    self.units["z"] = unit
            if "t" in self.axes and metadata.get("finterval", 0) > 0:
                self.scale["t"] = float(metadata["finterval"])
                self.units["t"] = _unit(metadata.get("tunit", "second"))
        self.description = tif.pages[0].description

    def _page(self, index):
        # Only the producer accesses the TIFF and this cache. Workers receive
        # independent chunk buffers, never pages or views into the cache.
        if index in self._pages:
            self._pages.move_to_end(index)
            return self._pages[index]
        page_bytes = int(np.prod(self.series.shape[-2:]))*self.dtype.itemsize
        while self._pages and self._cached_bytes + page_bytes > self.cache_bytes:
            _, old = self._pages.popitem(last=False)
            self._cached_bytes -= old.nbytes
        pixels = self.series.pages[index].asarray()
        if pixels.nbytes <= self.cache_bytes:
            self._pages[index] = pixels
            self._cached_bytes += pixels.nbytes
        return pixels

    def __getitem__(self, selection):
        selection = tuple(selection)
        if len(selection) != len(self.shape) or any(not isinstance(s, slice) or s.step not in (None, 1) for s in selection):
            raise ValueError("TIFF source accepts unit-step chunk slices only")
        normalized = tuple(slice(*s.indices(n)[:2]) for s, n in zip(selection, self.shape))
        block = np.empty(tuple(s.stop-s.start for s in normalized), dtype=self.dtype)
        for coord in itertools.product(*(range(s.start, s.stop) for s in normalized[:-2])):
            mapped = dict(zip(self.axes[:-2], coord))
            source_coord = tuple(mapped[a] for a in self.source_axes[:-2])
            page = int(np.ravel_multi_index(source_coord, self.series.shape[:-2])) if source_coord else 0
            pixels = self._page(page)
            destination = tuple(c-s.start for c, s in zip(coord, normalized[:-2]))
            block[destination] = pixels[normalized[-2:]]
            del pixels
        return block
