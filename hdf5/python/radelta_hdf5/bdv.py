"""Streaming export of uint16 TIFF/HDF5 images to BDV XML/HDF5."""
from collections import OrderedDict
from contextlib import ExitStack
import itertools
import math
import operator
import os
from pathlib import Path
import tempfile
import xml.etree.ElementTree as ET

import h5py
import numpy as np
import tifffile

from . import FILTER_ID, create_dataset, filter_options


def _axes(axes, shape):
    axes = str(axes).upper()
    if (len(axes) != len(shape) or len(set(axes)) != len(axes) or
            not axes.endswith("YX") or any(a not in "TCZYX" for a in axes) or
            any(n <= 0 for n in shape)):
        raise ValueError("Source must have nonempty TCZYX axes ending in YX")
    sizes = dict(zip(axes, shape))
    return axes, tuple(sizes.get(a, 1) for a in "TCZYX")


class _Hdf5Source:
    def __init__(self, ds, axes):
        if not isinstance(ds, h5py.Dataset) or ds.dtype.kind != "u" or ds.dtype.itemsize != 2:
            raise ValueError("HDF5 source must be a uint16 dataset")
        recorded = ds.attrs.get("axes", "TCZYX"[-ds.ndim:])
        if isinstance(recorded, bytes):
            recorded = recorded.decode("ascii")
        self.axes, self.shape = _axes(axes or recorded, ds.shape)
        self.ds = ds
        self.voxel_size = ds.attrs.get("element_size_um")
        self.unit = "um" if self.voxel_size is not None else "pixel"
        self.channel_names = None

    def read(self, t, c, selection):
        mapping = dict(zip("ZYX", selection), T=t, C=c)
        block = self.ds[tuple(mapping[a] for a in self.axes)]
        spatial = [a for a in self.axes if a in "ZYX"]
        block = np.transpose(block, [spatial.index(a) for a in "ZYX" if a in spatial])
        return np.asarray(block, dtype=np.uint16).reshape(tuple(s.stop-s.start for s in selection))


class _TiffSource:
    def __init__(self, tif, axes, cache_mib):
        # Reuse the CLI's validation of uint16, plane-per-page TIFF series.
        from .__main__ import canonical_tiff
        if len(tif.series) != 1:
            raise ValueError("Select a single TIFF series before converting")
        self.series = tif.series[0]
        source_axes, _, _ = canonical_tiff(self.series, axes)
        self.axes, self.shape = _axes(source_axes, self.series.shape)
        self.cache = OrderedDict()
        self.cache_bytes = 0
        self.cache_limit = int(cache_mib * 1024**2)
        self.voxel_size, self.unit, self.channel_names = _tiff_metadata(tif)

    def read(self, t, c, selection):
        block = np.empty(tuple(s.stop-s.start for s in selection), np.uint16)
        for index, z in enumerate(range(selection[0].start, selection[0].stop)):
            mapping = dict(T=t, C=c, Z=z)
            coord = tuple(mapping[a] for a in self.axes[:-2])
            page = int(np.ravel_multi_index(coord, self.series.shape[:-2])) if coord else 0
            plane = self.cache.pop(page, None)
            if plane is None:
                plane = self.series.pages[page].asarray()
                if plane.nbytes <= self.cache_limit:
                    while self.cache and self.cache_bytes + plane.nbytes > self.cache_limit:
                        self.cache_bytes -= self.cache.popitem(last=False)[1].nbytes
                    self.cache_bytes += plane.nbytes
                    self.cache[page] = plane
            else:
                self.cache[page] = plane
            block[index] = plane[selection[1:]]
        return block


def _tiff_metadata(tif):
    """Read spatial calibration and channel names; do not invent physical units."""
    if tif.ome_metadata:
        root = ET.fromstring(tif.ome_metadata)
        pixels = root.find(".//{*}Pixels")
        if pixels is not None:
            names = [p.get("Name", f"channel {i}")
                     for i, p in enumerate(pixels.findall("{*}Channel"))]
            sizes = [pixels.get("PhysicalSize" + a) for a in "ZYX"]
            units = [pixels.get("PhysicalSize" + a + "Unit", "µm") for a in "ZYX"]
            conversions = {"µm": 1, "um": 1, "nm": .001, "mm": 1000,
                           "m": 1e6, "cm": 1e4}
            if all(u in conversions for u in units):
                sizes = [float(v)*conversions[u] if v is not None else None
                         for v, u in zip(sizes, units)]
                # Give a singleton Z the smaller calibrated XY spacing,
                # after converting potentially different physical units.
                if sizes[0] is None and pixels.get("SizeZ") == "1" and all(v is not None for v in sizes[1:]):
                    sizes[0] = min(sizes[1:])
                if all(v is not None for v in sizes):
                    return tuple(sizes), "um", names
            return None, "pixel", names
    ij = tif.imagej_metadata or {}
    page = tif.pages[0]
    if ij.get("unit") and "XResolution" in page.tags and "YResolution" in page.tags:
        x = page.tags["XResolution"].value
        y = page.tags["YResolution"].value
        xy = (y[1]/y[0], x[1]/x[0])
        return (float(ij.get("spacing", min(xy))), *xy), str(ij["unit"]), None
    return None, "pixel", None


def default_resolutions(shape, voxel_size):
    """Cumulative ZYX factors, delaying coarse axes for anisotropic voxels."""
    factors = (1, 1, 1)
    levels = [factors]
    while max((n+f-1)//f for n, f in zip(shape, factors)) > 256:
        spacing = [v*f for v, f in zip(voxel_size, factors)]
        eligible = [i for i in range(3) if (shape[i]+factors[i]-1)//factors[i] > 1]
        minimum = min(spacing[i] for i in eligible)
        factors = tuple(f*2 if i in eligible and spacing[i] <= minimum*2 else f
                        for i, f in enumerate(factors))
        levels.append(factors)
    return tuple(levels)


def _resolutions(levels):
    try:
        levels = tuple(tuple(operator.index(v) for v in level) for level in levels)
    except TypeError as exc:
        raise ValueError("Resolution factors must be integers") from exc
    if not levels or levels[0] != (1, 1, 1):
        raise ValueError("Resolutions must start with 1,1,1 (ZYX)")
    for previous, current in zip(levels, levels[1:]):
        if (len(current) != 3 or any(v <= 0 or v > 2**31-1 for v in current) or
                current == previous or any(v % p for v, p in zip(current, previous))):
            raise ValueError("Each resolution must be a larger integer multiple of the previous ZYX factors")
    return levels


def _level_chunks(shape, chunks, ratio):
    result = [min(n, c) for n, c in zip(shape, chunks)]
    # Bound the expanded input tile, including padding, at 16 MiB uint16.
    # uint64 accumulation then has a bounded working set as well.
    while math.prod(c*r for c, r in zip(result, ratio))*2 > 16*1024**2:
        eligible = [i for i, c in enumerate(result) if c > 1]
        if not eligible:
            raise ValueError("Downsampling step exceeds the tile memory limit; add intermediate resolutions")
        axis = max(eligible, key=lambda i: result[i]*ratio[i])
        result[axis] = (result[axis]+1)//2
    return tuple(result)


def _downsample(block, ratio, shape):
    # Border replication agrees with BDV's voxel-centre mipmap transform:
    # x_original = factor*x_level + (factor-1)/2.
    padding = tuple((0, n*r-b) for n, r, b in zip(shape, ratio, block.shape))
    block = np.pad(block, padding, mode="edge")
    z, y, x = shape
    rz, ry, rx = ratio
    sums = block.reshape(z, rz, y, ry, x, rx).sum(axis=(1, 3, 5), dtype=np.uint64)
    count = math.prod(ratio)
    return ((sums + count//2)//count).astype(np.uint16)


def _xml(h5_name, shape, voxel_size, unit, names):
    t_count, c_count, z, y, x = shape
    root = ET.Element("SpimData", version="0.2")
    ET.SubElement(root, "BasePath", type="relative").text = "."
    seq = ET.SubElement(root, "SequenceDescription")
    loader = ET.SubElement(seq, "ImageLoader", format="bdv.hdf5")
    ET.SubElement(loader, "hdf5", type="relative").text = h5_name
    setups = ET.SubElement(seq, "ViewSetups")
    for c in range(c_count):
        setup = ET.SubElement(setups, "ViewSetup")
        ET.SubElement(setup, "id").text = str(c)
        ET.SubElement(setup, "name").text = names[c]
        ET.SubElement(setup, "size").text = f"{x} {y} {z}"
        voxel = ET.SubElement(setup, "voxelSize")
        ET.SubElement(voxel, "unit").text = unit
        ET.SubElement(voxel, "size").text = " ".join(map(str, voxel_size[::-1]))
        ET.SubElement(ET.SubElement(setup, "attributes"), "channel").text = str(c)
    attributes = ET.SubElement(setups, "Attributes", name="channel")
    for c in range(c_count):
        channel = ET.SubElement(attributes, "Channel")
        ET.SubElement(channel, "id").text = str(c)
        ET.SubElement(channel, "name").text = names[c]
    timepoints = ET.SubElement(seq, "Timepoints", type="range")
    ET.SubElement(timepoints, "first").text = "0"
    ET.SubElement(timepoints, "last").text = str(t_count-1)
    registrations = ET.SubElement(root, "ViewRegistrations")
    vz, vy, vx = voxel_size
    for t, c in itertools.product(range(t_count), range(c_count)):
        registration = ET.SubElement(registrations, "ViewRegistration", timepoint=str(t), setup=str(c))
        transform = ET.SubElement(registration, "ViewTransform", type="affine")
        ET.SubElement(transform, "Name").text = "voxel calibration"
        ET.SubElement(transform, "affine").text = f"{vx} 0 0 0 0 {vy} 0 0 0 0 {vz} 0"
    return ET.ElementTree(root)


def export_bdv(input_path, output_xml, *, dataset="/images", axes=None,
               chunks=(16, 64, 64), resolutions=None, voxel_size=None, unit=None,
               channel_names=None, tiff_cache_mib=64, overwrite=False,
               filter_id=FILTER_ID, progress=None, **codec):
    """Export TIFF or one HDF5 dataset. Return (XML path, HDF5 path).

    Chunks, cumulative resolutions and voxel sizes are Z,Y,X. Channels become
    setups; timepoints are numbered from zero. Only level zero is quantised in
    lossy mode: coarser levels average decoded values and use lossless Radelta.
    Each pyramid step averages the previous level with replicated borders and
    rounds half up to uint16. TIFF memory includes one complete decoded plane.

    The XML is the atomic publication point. HDF5 has a unique sibling name;
    overwrites keep the previous HDF5 generation for existing readers. Move XML
    and its referenced HDF5 file together. No other input datasets are copied.
    """
    from .__main__ import chunk_slices
    input_path, output_xml = Path(input_path), Path(output_xml)
    if output_xml.suffix.lower() != ".xml":
        raise ValueError("BDV output must have an .xml extension")
    if input_path.resolve() == output_xml.resolve():
        raise ValueError("Input and output must be different files")
    if output_xml.exists() and not overwrite:
        raise ValueError("Output exists; use --overwrite to replace it")
    if not math.isfinite(tiff_cache_mib) or tiff_cache_mib < 0:
        raise ValueError("TIFF cache size must be finite and nonnegative")
    chunks = tuple(operator.index(v) for v in chunks)
    if len(chunks) != 3 or any(v <= 0 or v > 2**31-1 for v in chunks):
        raise ValueError("BDV chunks must be three positive ZYX int32 extents")
    filter_options(**codec)  # Reject invalid options before allocating files.
    with ExitStack() as stack:
        if h5py.is_hdf5(input_path):
            handle = stack.enter_context(h5py.File(input_path, "r"))
            source = _Hdf5Source(handle[dataset], axes)
        else:
            handle = stack.enter_context(tifffile.TiffFile(input_path))
            source = _TiffSource(handle, axes, tiff_cache_mib)
        spacing = voxel_size if voxel_size is not None else source.voxel_size
        spacing = tuple(float(v) for v in (spacing if spacing is not None else (1, 1, 1)))
        if len(spacing) != 3 or any(not math.isfinite(v) or v <= 0 for v in spacing):
            raise ValueError("Voxel size must be three finite positive ZYX values")
        unit = unit or ("um" if voxel_size is not None else source.unit)
        names = channel_names if channel_names is not None else source.channel_names
        if names is None:
            names = [f"channel {c}" for c in range(source.shape[1])]
        if len(names) != source.shape[1] or any(not isinstance(n, str) for n in names):
            raise ValueError("Provide one channel name per source channel")
        levels = _resolutions(resolutions if resolutions is not None else
                              default_resolutions(source.shape[-3:], spacing))
        shapes = [tuple((n+f-1)//f for n, f in zip(source.shape[-3:], level)) for level in levels]
        ratios = [(1, 1, 1)] + [tuple(c//p for c, p in zip(cur, prev))
                               for prev, cur in zip(levels, levels[1:])]
        tiles = [_level_chunks(s, chunks, r) for s, r in zip(shapes, ratios)]
        total = source.shape[0]*source.shape[1]*sum(
            math.prod((n+c-1)//c for n, c in zip(s, tile)) for s, tile in zip(shapes, tiles))
        completed = 0
        h5_path = xml_temp = None
        published = False
        try:
            fd, name = tempfile.mkstemp(prefix=output_xml.stem + ".", suffix=".h5", dir=output_xml.parent)
            os.close(fd)
            h5_path = Path(name)
            with h5py.File(h5_path, "w") as out:
                for c in range(source.shape[1]):
                    setup = out.require_group(f"s{c:02d}")
                    setup.create_dataset("resolutions", data=np.asarray(levels, dtype=np.float64)[:, ::-1])
                    setup.create_dataset("subdivisions", data=np.asarray(tiles, dtype=np.int32)[:, ::-1])
                    # Current BDV supports uint16 explicitly; legacy readers
                    # interpret 16-bit storage as unsigned too.
                    setup.attrs["dataType"] = "uint16"
                for t, c in itertools.product(range(source.shape[0]), range(source.shape[1])):
                    previous = None
                    for level, (shape, tile, ratio) in enumerate(zip(shapes, tiles, ratios)):
                        group = out.require_group(f"t{t:05d}/s{c:02d}/{level}")
                        options = dict(codec)
                        if level:
                            options["lossy"] = False
                        ds = create_dataset(group, "cells", shape=shape, chunks=tile,
                                            filter_id=filter_id, **options)
                        ds.attrs["element_size_um" if unit in ("um", "µm") else "voxel_size"] = [
                            v*f for v, f in zip(spacing, levels[level])]
                        for selection in chunk_slices(shape, tile):
                            if previous is None:
                                block = source.read(t, c, selection)
                            else:
                                region = tuple(slice(s.start*r, min(s.stop*r, n))
                                               for s, r, n in zip(selection, ratio, previous.shape))
                                block = _downsample(previous[region], ratio,
                                                    tuple(s.stop-s.start for s in selection))
                            ds[selection] = block
                            completed += 1
                            if progress is not None:
                                progress(completed, total)
                        if level == 0 and codec.get("lossy", False):
                            # HDF5's write cache still contains pre-quantisation
                            # pixels. Close/reopen to flush and discard that
                            # cache before averaging the decoded base level.
                            path = ds.name
                            ds.id.close()
                            ds = out[path]
                        previous = ds
                out.flush()
            fd, name = tempfile.mkstemp(prefix=".radelta-bdv-", suffix=".xml", dir=output_xml.parent)
            os.close(fd)
            xml_temp = Path(name)
            _xml(h5_path.name, source.shape, spacing, unit, names).write(
                xml_temp, encoding="utf-8", xml_declaration=True)
            if overwrite:
                os.replace(xml_temp, output_xml)
                published = True
            else:
                os.link(xml_temp, output_xml)
                published = True
                xml_temp.unlink()
        finally:
            if xml_temp is not None and xml_temp.exists():
                xml_temp.unlink()
            if not published and h5_path is not None and h5_path.exists():
                h5_path.unlink()
    return output_xml, h5_path
