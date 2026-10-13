"""TIFF / OME-Zarr conversion and existing OME-Zarr recompression."""
import argparse
import json
import math
from pathlib import Path
import sys
import tempfile
import time

import numpy as np
import tifffile
import zarr

from ._native import library, validate_axes
from ._tiff import TiffSource
from .ome import repack, write_image


class Progress:
    def __init__(self):
        self.start = self.last = time.monotonic()
        self.count = 0
        print("Processing image chunks...", file=sys.stderr, flush=True)

    def __call__(self, path, selection):
        self.count += 1
        now = time.monotonic()
        if now-self.last >= 1:
            print(f"{self.count:,} chunks; array {path}; {now-self.start:.1f} s", file=sys.stderr, flush=True)
            self.last = now

    def finish(self):
        print(f"Completed {self.count:,} chunks in {time.monotonic()-self.start:.1f} s", file=sys.stderr, flush=True)


def _image(group):
    ome = group.attrs.get("ome", dict(group.attrs))
    if not ome.get("multiscales"):
        raise ValueError("Selected group has no multiscales; use --group for a nested image")
    return ome, ome["multiscales"][0]


def _export_tiff(args):
    root = zarr.open_group(args.input, mode="r", use_consolidated=False)
    group = root[args.group] if args.group else root
    ome, image = _image(group)
    level = image["datasets"][args.level]
    arr = group[level["path"]]
    axes = validate_axes(tuple(a["name"] for a in image["axes"]), arr.ndim)
    if axes != "".join(a for a in "tczyx" if a in axes) or not axes.endswith("yx"):
        raise ValueError("TIFF export currently requires canonical t,c,z,y,x axes ending in y,x")
    if arr.dtype.kind != "u" or arr.dtype.itemsize != 2:
        raise ValueError("TIFF export expects uint16 pixels")
    scale = np.ones(arr.ndim)
    for transforms in (level.get("coordinateTransformations", []), image.get("coordinateTransformations", [])):
        for transform in transforms:
            if transform["type"] == "scale":
                if "scale" not in transform:
                    raise ValueError("TIFF export does not support array-referenced transforms")
                scale *= transform["scale"]
    metadata = {"axes": axes.upper()}
    for i, axis in enumerate(image["axes"]):
        if "unit" not in axis:
            continue
        if axis["name"] in "zyx":
            key = "PhysicalSize"+axis["name"].upper()
        elif axis["name"] == "t":
            key = "TimeIncrement"
        else:
            continue
        metadata[key] = float(scale[i])
        ome_units = {"micrometer": "µm", "nanometer": "nm", "millimeter": "mm", "centimeter": "cm", "meter": "m",
                     "second": "s", "millisecond": "ms", "microsecond": "µs", "minute": "min", "hour": "h"}
        if axis["unit"] not in ome_units:
            raise ValueError(f"TIFF export does not yet map OME-Zarr unit {axis['unit']!r}")
        metadata[key+"Unit"] = ome_units[axis["unit"]]
    if "c" in axes and "omero" in ome:
        metadata["Channel"] = {"Name": [c.get("label", str(i)) for i, c in enumerate(ome["omero"]["channels"])]}
    progress = Progress()
    def planes():
        for coord in np.ndindex(arr.shape[:-2]):
            yield np.asarray(arr[coord], dtype=np.uint16)
            progress("TIFF planes", coord)
    with tempfile.TemporaryDirectory(prefix=".radelta-tiff-", dir=args.output.parent) as temporary:
        target = Path(temporary)/"image.ome.tif"
        with tifffile.TiffWriter(target, bigtiff=True, ome=True) as tif:
            tif.write(planes(), shape=arr.shape, dtype=np.uint16, photometric="minisblack", metadata=metadata)
        if args.output.exists():
            raise ValueError("Output appeared during conversion")
        # Atomic no-clobber for a file, unlike replacing an existing result.
        import os
        os.link(target, args.output)
    progress.finish()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for command in ("encode", "repack", "decode", "info"):
        p = sub.add_parser(command)
        p.add_argument("input", type=Path)
        if command != "info":
            p.add_argument("output", type=Path)
        if command in {"encode", "repack"}:
            p.add_argument("--chunks", type=lambda s: tuple(int(n) for n in s.split(",")))
            p.add_argument("--lossy", action="store_true")
            p.add_argument("--offset-adu", type=float)
            p.add_argument("--gain-e-per-adu", type=float)
            p.add_argument("--noise-step", type=float, default=2)
            p.add_argument("--block-depth", type=int, default=4)
            p.add_argument("--context", choices=("mean", "mean-signs", "signed3", "signs"), default="mean")
            p.add_argument("--scale-bits", type=int, default=10)
        if command == "encode":
            p.add_argument("--workers", type=int, help="concurrent chunk writes (default: up to 4 CPU workers)")
            p.add_argument("--tiff-cache-mib", type=float, default=64,
                           help="decoded TIFF page cache in MiB (default: 64; 0 disables caching)")
            p.add_argument("--zarr-format", type=int, choices=(2, 3), default=3)
            p.add_argument("--axes", help="override source TIFF axes, e.g. TZCYX")
            p.add_argument("--levels", type=int, default=1)
            p.add_argument("--voxel-size", type=lambda s: tuple(float(n) for n in s.split(",")), help="ZYX (or YX) pixel sizes")
            p.add_argument("--space-unit", default="micrometer")
        if command == "repack":
            p.add_argument("--compression", choices=("radelta", "zstd", "none"), default="radelta")
        if command in {"decode", "info"}:
            p.add_argument("--group", default="", help="nested image group, e.g. A/1/0")
        if command == "decode":
            p.add_argument("--level", type=int, default=0)
    args = parser.parse_args(argv)
    try:
        if args.command == "info":
            root = zarr.open_group(args.input, mode="r", use_consolidated=False)
            group = root[args.group] if args.group else root
            ome, image = _image(group)
            print(json.dumps(ome, indent=2))
            for dataset in image["datasets"]:
                arr = group[dataset["path"]]
                print(dataset["path"], arr.shape, arr.dtype, arr.chunks)
                print(json.dumps(arr.metadata.to_dict().get("codecs", arr.metadata.to_dict().get("compressor")), indent=2))
            return
        if args.output.exists():
            raise ValueError("Output already exists; choose a new path")
        if args.command in {"encode", "repack"}:
            if args.lossy and (args.offset_adu is None or args.gain_e_per_adu is None):
                raise ValueError("--lossy requires --offset-adu and --gain-e-per-adu")
            options = dict(lossy=args.lossy, offset_adu=args.offset_adu if args.offset_adu is not None else 0,
                gain_e_per_adu=args.gain_e_per_adu if args.gain_e_per_adu is not None else 1,
                noise_step=args.noise_step, block_depth=args.block_depth, context=args.context, scale_bits=args.scale_bits)
        if args.command == "repack":
            progress = Progress()
            repack(args.input, args.output, chunks=args.chunks, compression=args.compression, progress=progress, **options)
            progress.finish()
        elif args.command == "decode":
            if args.level < 0:
                raise ValueError("--level must be non-negative")
            _export_tiff(args)
        else:
            if args.workers is not None and args.workers < 1:
                raise ValueError("--workers must be positive")
            if not math.isfinite(args.tiff_cache_mib) or args.tiff_cache_mib < 0:
                raise ValueError("--tiff-cache-mib must be finite and non-negative")
            library()  # Fail before touching the output if native loading fails.
            progress = Progress()
            with tifffile.TiffFile(args.input) as tif, tempfile.TemporaryDirectory(prefix=".radelta-zarr-", dir=args.output.parent) as temporary:
                source = TiffSource(tif, args.axes, cache_bytes=int(args.tiff_cache_mib*1024**2))
                if args.voxel_size:
                    spatial = "".join(a for a in source.axes if a in "zyx")
                    if len(args.voxel_size) != len(spatial):
                        raise ValueError("--voxel-size must match the spatial axes (ZYX or YX)")
                    source.scale.update(zip(spatial, args.voxel_size))
                    source.units.update((a, args.space_unit) for a in spatial)
                target = Path(temporary)/"store"
                root = zarr.open_group(target, mode="w", zarr_format=args.zarr_format)
                write_image(root, source, axes=source.axes, chunks=args.chunks, scale=source.scale,
                    units=source.units, name=args.input.stem, omero=source.omero,
                    levels=args.levels, progress=progress, workers=args.workers, **options)
                root.attrs["radelta"] = {"source_tiff_axes": source.source_axes.upper(),
                                          "source_tiff_description": source.description}
                if args.output.exists():
                    raise ValueError("Output appeared during conversion")
                target.rename(args.output)
            progress.finish()
        print(f"Wrote {args.output}", file=sys.stderr)
    except (ValueError, RuntimeError, OSError, KeyError, IndexError) as exc:
        parser.exit(1, f"radelta-zarr: {exc}\n")


if __name__ == "__main__":
    main()
