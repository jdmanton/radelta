"""OME metadata and bounded-chunk image writing/recompression."""
import copy
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import itertools
import math
import operator
import os
from pathlib import Path
import shutil
import tempfile
import warnings

import numpy as np
import zarr
from numcodecs import Zstd
from zarr.codecs import ZstdCodec

from ._native import VERSION, validate_axes, validate_options, validate_shape
from .codecs import RadeltaCodec, RadeltaNumcodec

_SPACE_UNITS = set("angstrom attometer centimeter decimeter exameter femtometer foot gigameter hectometer inch kilometer megameter meter micrometer mile millimeter nanometer parsec petameter picometer terameter yard yoctometer yottameter zeptometer zettameter".split())
_TIME_UNITS = set("attosecond centisecond day decisecond exasecond femtosecond gigasecond hectosecond hour kilosecond megasecond microsecond millisecond minute nanosecond petasecond picosecond second terasecond yoctosecond yottasecond zeptosecond zettasecond".split())


def chunk_slices(shape, chunks):
    for origin in itertools.product(*(range(0, n, c) for n, c in zip(shape, chunks))):
        yield tuple(slice(o, min(o+c, n)) for o, c, n in zip(origin, chunks, shape))


def _workers(workers):
    if workers is None:
        cpu_count = getattr(os, "process_cpu_count", os.cpu_count)() or 1
        return min(4, cpu_count)
    workers = operator.index(workers)
    if workers < 1:
        raise ValueError("workers must be positive")
    return workers


def _write_chunks(arr, blocks, *, workers, path, progress):
    """Write disjoint whole chunks with at most `workers` buffers in flight."""
    def completed(selection):
        if progress:
            progress(path, selection)

    if workers == 1:
        for selection, block in blocks:
            arr[selection] = block
            completed(selection)
        return
    pending = {}
    with ThreadPoolExecutor(max_workers=workers, thread_name_prefix="radelta-zarr") as pool:
        try:
            for selection, block in blocks:
                pending[pool.submit(arr.__setitem__, selection, block)] = selection
                if len(pending) >= workers:
                    done, _ = wait(pending, return_when=FIRST_COMPLETED)
                    for future in done:
                        selected = pending.pop(future)
                        future.result()
                        completed(selected)
            for future in pending:
                future.result()
                completed(pending[future])
        finally:
            # Drain active writes before a failed CLI conversion removes its
            # temporary store. Do not leave background writes after returning.
            for future in pending:
                future.cancel()


def default_chunks(shape, axes, target_mib=8):
    axes = validate_axes(axes, len(shape))
    chunks = [min(max(1, n), 4 if a == "z" else 512) if a in "zyx" else 1
              for a, n in zip(axes, shape)]
    if not math.isfinite(target_mib) or target_mib <= 0:
        raise ValueError("target_mib must be positive")
    while math.prod(chunks)*2 > max(2, int(target_mib*1024**2)):
        i = max(range(len(chunks)), key=chunks.__getitem__)
        chunks[i] = (chunks[i]+1)//2
    return tuple(chunks)


def _array(group, name, *, shape, chunks, axes, compression="radelta", dtype="u2", order="C",
           fill_value=0, attributes=None, dimension_names=None, chunk_key_encoding=None, shards=None, **options):
    common = dict(shape=shape, chunks=chunks, dtype=dtype, fill_value=fill_value, attributes=attributes or {})
    fmt = group.metadata.zarr_format
    if compression == "radelta" and options.get("lossy", False):
        # Zarr normally skips all-fill chunks before invoking codecs. Written
        # zeros must still be quantised when the camera offset is nonzero.
        common["config"] = {"write_empty_chunks": True}
    if compression not in {"radelta", "zstd", "none"}:
        raise ValueError("compression must be radelta, zstd or none")
    if fmt == 2:
        compressor = (RadeltaNumcodec(chunks, axes, dtype=np.dtype(dtype).str, order=order, **options)
                      if compression == "radelta" else Zstd(level=3) if compression == "zstd" else None)
        return group.create_array(name, **common, compressor=compressor, order=order,
                                  **({"chunk_key_encoding": chunk_key_encoding} if chunk_key_encoding else {}))
    common["dimension_names"] = tuple(dimension_names or axes)
    if chunk_key_encoding is not None:
        common["chunk_key_encoding"] = chunk_key_encoding
    if shards is not None:
        common["shards"] = shards
    if compression == "radelta":
        return group.create_array(name, **common, serializer=RadeltaCodec(axes, **options), compressors=[])
    return group.create_array(name, **common, compressors=[ZstdCodec(level=3)] if compression == "zstd" else [])


def _axes_metadata(axes, units):
    result = []
    if set(units)-set(axes):
        raise ValueError("Units include an axis absent from the image")
    for name in axes:
        kind = "time" if name == "t" else "channel" if name == "c" else "space"
        entry = {"name": name, "type": kind}
        unit = units.get(name)
        if unit is not None:
            if kind == "channel" or unit not in (_TIME_UNITS if kind == "time" else _SPACE_UNITS):
                raise ValueError(f"Invalid OME unit {unit!r} for axis {name}")
            entry["unit"] = unit
        result.append(entry)
    return result


def _vector(value, axes, default):
    if value is None:
        result = [default]*len(axes)
    elif isinstance(value, dict):
        if set(value)-set(axes):
            raise ValueError("Coordinate vector includes axes absent from the image")
        result = [value.get(a, default) for a in axes]
    else:
        result = list(value)
    if len(result) != len(axes) or not all(math.isfinite(float(v)) for v in result):
        raise ValueError("Coordinate vector must be finite and match the axes")
    return list(map(float, result))


def _transforms(scale, translation):
    result = [{"type": "scale", "scale": list(scale)}]
    if any(translation):
        result.append({"type": "translation", "translation": list(translation)})
    return result


def _metadata(group, axes, units, datasets, name, omero=None, pyramid=False):
    multiscale = {"name": name, "axes": _axes_metadata(axes, units), "datasets": datasets}
    if pyramid:
        multiscale.update(type="mean", metadata={"method": "radelta_zarr.box_mean",
            "version": VERSION, "description": "2x XY block means, edge replication, integer rounding"})
    if group.metadata.zarr_format == 2:
        multiscale["version"] = "0.4"
        group.attrs["multiscales"] = [multiscale]
        if omero is not None:
            group.attrs["omero"] = omero
    else:
        ome = {"version": "0.5", "multiscales": [multiscale]}
        if omero is not None:
            ome["omero"] = omero
        group.attrs["ome"] = ome


def create_ome_image(group, *, shape, axes="zyx", chunks=None, scale=None,
                     translation=None, units=None, name="image", omero=None, dtype="u2", **options):
    """Create one uint8/uint16 OME-Zarr level and metadata; use normal Zarr slicing."""
    axes = validate_axes(axes, len(shape))
    shape = validate_shape(shape, max_bytes=None)
    if axes != "".join(a for a in "tczyx" if a in axes):
        raise ValueError("New OME images use axes in t,c,z,y,x order")
    from ._native import unsigned_dtype
    dtype = unsigned_dtype(dtype)
    options = validate_options(options)
    chunks = validate_shape(chunks or default_chunks(shape, axes))
    if len(chunks) != len(shape):
        raise ValueError("Chunk rank must match image rank")
    scale, translation = _vector(scale, axes, 1), _vector(translation, axes, 0)
    if any(s <= 0 for s in scale):
        raise ValueError("Pixel/time scales must be positive")
    units = units or {}
    _axes_metadata(axes, units)  # Validate before creating anything.
    if "0" in group:
        raise ValueError("Image group already contains level 0")
    arr = _array(group, "0", shape=shape, chunks=chunks, axes=axes, dtype=dtype, **options)
    _metadata(group, axes, units, [{"path": "0", "coordinateTransformations": _transforms(scale, translation)}], name, omero)
    return arr


def write_image(group, data, *, axes="zyx", chunks=None, scale=None, translation=None,
                units=None, name="image", omero=None, levels=1, progress=None, workers=None, **options):
    """Write an array-like source with bounded concurrency; optional XY mean pyramid.

    Full-resolution lossless pixels are unchanged. Lossy writing accepts one
    level; create a lossless pyramid and repack it to quantise each level once.
    """
    shape = tuple(data.shape)
    workers = _workers(workers)
    dtype = np.dtype(data.dtype)
    if dtype.kind != "u" or dtype.itemsize not in (1, 2):
        raise ValueError("OME image source must be uint8 or uint16")
    axes = validate_axes(axes, len(shape))
    if not isinstance(levels, int) or levels < 1:
        raise ValueError("levels must be a positive integer")
    if levels > 1 and options.get("lossy", False):
        raise ValueError("For a lossy pyramid, first write lossless levels and then repack with lossy=True")
    if levels > 1 and not axes.endswith("yx"):
        raise ValueError("Pyramid writing requires trailing y,x axes")
    if levels > 1 + math.ceil(math.log2(max(shape[-2:]))):
        raise ValueError("Too many levels for the image dimensions")
    arr = create_ome_image(group, shape=shape, axes=axes, chunks=chunks, scale=scale,
                           translation=translation, units=units, name=name, omero=omero, dtype=dtype, **options)
    blocks = ((selection, np.array(data[selection], dtype=dtype, order="C", copy=True))
              for selection in chunk_slices(shape, arr.chunks))
    _write_chunks(arr, blocks, workers=workers, path="0", progress=progress)
    datasets = [{"path": "0", "coordinateTransformations": _transforms(_vector(scale, axes, 1), _vector(translation, axes, 0))}]
    current_scale, current_translation = _vector(scale, axes, 1), _vector(translation, axes, 0)
    for level in range(1, levels):
        parent = arr
        fy, fx = (2 if n > 1 else 1 for n in parent.shape[-2:])
        child_shape = (*parent.shape[:-2], (parent.shape[-2]+fy-1)//fy, (parent.shape[-1]+fx-1)//fx)
        arr = _array(group, str(level), shape=child_shape, chunks=default_chunks(child_shape, axes), axes=axes, dtype=dtype, **options)
        def pyramid_blocks():
            for selection in chunk_slices(child_shape, arr.chunks):
                sy, sx = selection[-2:]
                block = np.asarray(parent[(*selection[:-2], slice(sy.start*fy, sy.stop*fy), slice(sx.start*fx, sx.stop*fx))])
                target_y, target_x = (sy.stop-sy.start)*fy, (sx.stop-sx.start)*fx
                pad = [(0, 0)]*(block.ndim-2) + [(0, target_y-block.shape[-2]), (0, target_x-block.shape[-1])]
                if any(p[1] for p in pad):
                    block = np.pad(block, pad, mode="edge")
                block = block.reshape(*block.shape[:-2], sy.stop-sy.start, fy, sx.stop-sx.start, fx)
                total = block.sum(axis=(-1, -3), dtype=np.uint32)
                yield selection, ((total + fy*fx//2)//(fy*fx)).astype(dtype)
        _write_chunks(arr, pyramid_blocks(), workers=workers, path=str(level), progress=progress)
        for index, factor in ((len(axes)-2, fy), (len(axes)-1, fx)):
            current_translation[index] += current_scale[index]*(factor-1)/2
            current_scale[index] *= factor
        datasets.append({"path": str(level), "coordinateTransformations": _transforms(current_scale, current_translation)})
    _metadata(group, axes, units or {}, datasets, name, omero, pyramid=levels > 1)
    return group


def _image_arrays(root):
    """Find declared multiscale arrays, including labels and plate image groups."""
    result = {}
    groups = [("", root)] + [(name, node) for name, node in root.members(max_depth=None, use_consolidated_for_children=False) if isinstance(node, zarr.Group)]
    for group_path, group in groups:
        attrs = dict(group.attrs)
        ome = attrs.get("ome", attrs)
        multiscales = ome.get("multiscales", [])
        label = "image-label" in ome or "labels" in group_path.split("/")
        for multiscale in multiscales:
            expected = "0.5" if group.metadata.zarr_format == 3 else "0.4"
            version = ome.get("version") if group.metadata.zarr_format == 3 else multiscale.get("version", "0.4")
            if version != expected:
                raise ValueError(f"Unsupported OME-Zarr version {version!r} at {group_path or '/'}; expected {expected}")
            names = tuple(a["name"] for a in multiscale["axes"])
            for ds in multiscale["datasets"]:
                relative = Path(ds["path"])
                if relative.is_absolute() or ".." in relative.parts:
                    raise ValueError("OME dataset paths must be relative without parent traversal")
                full = "/".join(p for p in (group_path, ds["path"]) if p)
                source = root[full]
                if tuple(source.shape) and len(names) != source.ndim:
                    raise ValueError(f"Axis metadata rank mismatch at {full}")
                if group.metadata.zarr_format == 3 and tuple(source.metadata.dimension_names or ()) != names:
                    raise ValueError(f"OME-Zarr 0.5 dimension_names do not match axes at {full}")
                entry = (names, label)
                if full in result and result[full] != entry:
                    raise ValueError(f"Conflicting axes/label declarations for {full}")
                result[full] = entry
    return result


def repack(input_path, output_path, *, chunks=None, compression="radelta", progress=None, **options):
    """Recompress declared uint8/uint16 multiscales in a local OME-Zarr hierarchy.

    Copy other arrays/groups/sidecars verbatim. Labels are always lossless.
    Publish a new directory only after all conversions succeed.
    """
    source_path, output_path = Path(input_path).resolve(), Path(output_path).resolve()
    if source_path == output_path or source_path in output_path.parents or output_path in source_path.parents:
        raise ValueError("Input and output stores must be separate, non-nested directories")
    if output_path.exists():
        raise ValueError("Output already exists")
    if not source_path.is_dir():
        raise ValueError("Input must be a local directory OME-Zarr store")
    options = validate_options(options)
    root = zarr.open_group(source_path, mode="r", use_consolidated=False)
    arrays = _image_arrays(root)
    if not arrays:
        raise ValueError("No OME-Zarr multiscales found")
    convertible = [(path, names, label) for path, (names, label) in arrays.items()
                   if root[path].dtype.kind == "u" and root[path].dtype.itemsize in (1, 2)]
    for path, names, label in convertible:
        validate_axes(names, root[path].ndim)
        if chunks is not None and len(chunks) != root[path].ndim:
            raise ValueError(f"Chunk rank does not match {path}")
    if not convertible:
        raise ValueError("No uint8/uint16 OME image or label arrays found")
    if compression not in {"radelta", "zstd", "none"}:
        raise ValueError("compression must be radelta, zstd or none")
    if compression != "radelta" and options["lossy"]:
        raise ValueError("Lossy quantisation requires Radelta compression")
    with tempfile.TemporaryDirectory(prefix=".radelta-zarr-", dir=output_path.parent) as temporary:
        target_path = Path(temporary) / "store"
        shutil.copytree(source_path, target_path)
        dest = zarr.open_group(target_path, mode="r+", use_consolidated=False)
        for path, names, label in convertible:
            src = root[path]
            parent_path, _, key = path.rpartition("/")
            parent = dest[parent_path] if parent_path else dest
            metadata = src.metadata
            kwargs = dict(shape=src.shape, chunks=tuple(chunks or src.chunks), axes=names,
                          dtype=src.dtype, fill_value=src.fill_value, attributes=copy.deepcopy(dict(src.attrs)),
                          compression=compression, **options)
            if label:
                kwargs["lossy"] = False
            if src.metadata.zarr_format == 2:
                kwargs.update(order=metadata.order,
                    chunk_key_encoding={"name": "v2", "configuration": {"separator": metadata.dimension_separator}})
            else:
                kwargs.update(dimension_names=metadata.dimension_names,
                              chunk_key_encoding=metadata.chunk_key_encoding.to_dict(), shards=src.shards)
            del parent[key]
            out = _array(parent, key, **kwargs)
            for selection in chunk_slices(src.shape, out.chunks):
                out[selection] = src[selection]
                if progress:
                    progress(path, selection)
        # Regenerate cached metadata: copied consolidation must never describe
        # the old compression pipeline after chunk payloads have been replaced.
        with warnings.catch_warnings():
            warnings.filterwarnings("ignore", message="Consolidated metadata.*")
            zarr.consolidate_metadata(target_path)
        if output_path.exists():
            raise ValueError("Output appeared during conversion")
        target_path.rename(output_path)
    return output_path
