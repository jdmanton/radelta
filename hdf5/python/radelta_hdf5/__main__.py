"""TIFF/HDF5 conversion and BDV export without a full-volume copy."""
import argparse
import itertools
import math
import os
import sys
import tempfile
import time
from pathlib import Path

import h5py
import numpy as np
import tifffile

from . import FILTER_ID, configure_plugin, create_dataset


def chunk_slices(shape, chunks):
    for origin in itertools.product(*(range(0, n, c) for n, c in zip(shape, chunks))):
        yield tuple(slice(o, min(o+c, n)) for o, c, n in zip(origin, chunks, shape))


class Progress:
    def __init__(self, total):
        self.total, self.done = total, 0
        self.start = self.last = time.monotonic()
        print(f"Processing {total:,} chunks/planes", file=sys.stderr, flush=True)

    def advance(self):
        self.done += 1
        now = time.monotonic()
        if now - self.last >= 1 or self.done == self.total:
            print(f"{self.done:,}/{self.total:,} ({100*self.done/max(1,self.total):.1f}%) "
                  f"{now-self.start:.1f} s", file=sys.stderr, flush=True)
            self.last = now


def canonical_tiff(series, axes_override):
    axes = (axes_override or series.axes).upper()
    # Plain multipage stacks often use an unspecified Q or I page axis.
    if len(axes) == 3 and axes[0] in "QI" and axes[1:] == "YX":
        axes = "ZYX"
    if (len(axes) != len(series.shape) or len(set(axes)) != len(axes) or
            not axes.endswith("YX") or any(a not in "TCZYX" for a in axes)):
        raise ValueError(f"Unsupported TIFF axes {axes!r}; use --axes with TCZYX axes ending in YX")
    if np.dtype(series.dtype).kind != "u" or np.dtype(series.dtype).itemsize != 2:
        raise ValueError("TIFF data must be grayscale uint16")
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


def encode_tiff(args, target):
    with tifffile.TiffFile(args.input) as tif, h5py.File(target, "w") as out:
        if len(tif.series) != 1:
            raise ValueError("Select a single TIFF series before converting")
        series = tif.series[0]
        axes, out_axes, shape = canonical_tiff(series, args.axes)
        ds = create_dataset(out, args.dataset, shape=shape, chunks=args.chunks,
                            filter_id=args.filter_id, **args.codec)
        ds.attrs["source_tiff_axes"] = axes
        if tif.pages[0].description:
            ds.attrs["source_tiff_description"] = tif.pages[0].description
        # TIFF metadata is retained as provenance; datasets use canonical axes.
        total = math.prod(math.ceil(n/c) for n, c in zip(shape, ds.chunks))
        progress = Progress(total)
        for selection in chunk_slices(shape, ds.chunks):
            block = np.empty(tuple(s.stop-s.start for s in selection), dtype=np.uint16)
            leading = tuple(range(s.start, s.stop) for s in selection[:-2])
            for coord in itertools.product(*leading):
                mapped = dict(zip(out_axes[:-2], coord))
                source_coord = tuple(mapped[a] for a in axes[:-2])
                page = int(np.ravel_multi_index(source_coord, series.shape[:-2])) if source_coord else 0
                plane = series.pages[page].asarray()
                dest = tuple(v-s.start for v, s in zip(coord, selection[:-2]))
                block[dest] = plane[selection[-2:]]
                del plane
            ds[selection] = block
            progress.advance()
        out.flush()
        print(f"Stored pixel payload: {ds.id.get_storage_size():,} bytes", file=sys.stderr)


def repack_hdf5(args, target):
    with h5py.File(args.input, "r") as source, h5py.File(target, "w") as out:
        src = source[args.dataset]
        if not isinstance(src, h5py.Dataset):
            raise ValueError("--dataset must identify a dataset")
        expected_axes = "TCZYX"[-src.ndim:]
        axes = src.attrs.get("axes", expected_axes)
        if isinstance(axes, bytes):
            axes = axes.decode("ascii")
        if axes != expected_axes:
            raise ValueError(f"HDF5 axes must be {expected_axes}; transpose the source first")
        ds = create_dataset(out, args.dataset, shape=src.shape, chunks=args.chunks,
                            dtype=src.dtype, filter_id=args.filter_id, **args.codec)
        for key, value in src.attrs.items():
            if h5py.check_dtype(ref=np.asarray(value).dtype) is not None or np.asarray(value).dtype.kind == "O":
                # Strings are ordinary attributes; reference/vlen structures are
                # not portable between files without copying their targets.
                if not isinstance(value, (str, bytes)):
                    raise ValueError(f"Attribute {key!r} contains references or variable-length objects; copy it explicitly")
            if not key.startswith("radelta_") and key not in {"axes", "offset_adu", "gain_e_per_adu", "noise_step"}:
                ds.attrs[key] = value
        progress = Progress(math.prod(math.ceil(n/c) for n, c in zip(src.shape, ds.chunks)))
        for selection in chunk_slices(src.shape, ds.chunks):
            ds[selection] = src[selection]
            progress.advance()
        out.flush()
        print(f"Stored pixel payload: {ds.id.get_storage_size():,} bytes", file=sys.stderr)


def decode_tiff(args, target):
    with h5py.File(args.input, "r") as source:
        ds = source[args.dataset]
        if ds.dtype.kind != "u" or ds.dtype.itemsize != 2 or not 2 <= ds.ndim <= 5 or any(n == 0 for n in ds.shape):
            raise ValueError("Expected a nonempty uint16 dataset of rank 2..5")
        progress = Progress(math.prod(ds.shape[:-2]))
        def planes():
            for coord in np.ndindex(ds.shape[:-2]):
                yield np.asarray(ds[coord], dtype=np.uint16)
                progress.advance()
        with tifffile.TiffWriter(target, bigtiff=True) as out:
            out.write(planes(), shape=ds.shape, dtype=np.uint16,
                      photometric="minisblack", metadata={"axes": "TCZYX"[-ds.ndim:]})


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plugin-path", type=Path, help="directory containing the built HDF5 plugin")
    parser.add_argument("--filter-id", type=int, default=FILTER_ID)
    sub = parser.add_subparsers(dest="command", required=True)
    for command in ("encode", "repack", "decode", "bdv"):
        p = sub.add_parser(command)
        p.add_argument("input", type=Path)
        p.add_argument("output", type=Path)
        p.add_argument("--dataset", default="/images")
        p.add_argument("--overwrite", action="store_true")
        if command != "decode":
            p.add_argument("--chunks", type=lambda s: tuple(int(n) for n in s.split(",")))
            p.add_argument("--lossy", action="store_true")
            p.add_argument("--offset-adu", type=float)
            p.add_argument("--gain-e-per-adu", type=float)
            p.add_argument("--noise-step", type=float, default=2)
            p.add_argument("--block-depth", type=int, default=4)
            p.add_argument("--context", choices=("mean", "mean-signs", "signed3", "signs"), default="mean")
            p.add_argument("--scale-bits", type=int, default=10)
        if command in ("encode", "bdv"):
            p.add_argument("--axes", help="source TIFF axes, e.g. TZCYX")
        if command == "bdv":
            p.set_defaults(chunks=(16, 64, 64))
            p.add_argument("--resolutions", type=lambda s: tuple(
                tuple(int(n) for n in level.split(",")) for level in s.split(";")),
                help="cumulative ZYX factors, e.g. '1,1,1;1,2,2;2,4,4'; default: automatic")
            p.add_argument("--voxel-size", type=lambda s: tuple(float(n) for n in s.split(",")),
                           help="Z,Y,X spacing; otherwise infer supported source calibration")
            p.add_argument("--unit", help="voxel-size unit (explicit sizes default to um)")
            p.add_argument("--channel-name", action="append", dest="channel_names",
                           help="repeat once per channel; otherwise use OME names when available")
            p.add_argument("--tiff-cache-mib", type=float, default=64,
                           help="bounded decoded TIFF plane cache (default: 64 MiB)")
    args = parser.parse_args(argv)
    try:
        if args.plugin_path:
            configure_plugin(args.plugin_path)
        if args.input.resolve() == args.output.resolve():
            raise ValueError("Input and output must be different files")
        if args.output.exists() and not args.overwrite:
            raise ValueError("Output exists; use --overwrite to replace it")
        if args.command != "decode":
            if args.lossy and (args.offset_adu is None or args.gain_e_per_adu is None):
                raise ValueError("--lossy requires --offset-adu and --gain-e-per-adu")
            args.codec = dict(lossy=args.lossy, offset_adu=args.offset_adu if args.offset_adu is not None else 0,
                              gain_e_per_adu=args.gain_e_per_adu if args.gain_e_per_adu is not None else 1,
                              noise_step=args.noise_step, block_depth=args.block_depth,
                              context=args.context, scale_bits=args.scale_bits)
        if args.command == "bdv":
            from .bdv import export_bdv
            reporter = None
            def report(done, total):
                nonlocal reporter
                if reporter is None:
                    reporter = Progress(total)
                reporter.advance()
            xml, h5 = export_bdv(args.input, args.output, dataset=args.dataset, axes=args.axes,
                                chunks=args.chunks, resolutions=args.resolutions,
                                voxel_size=args.voxel_size, unit=args.unit,
                                channel_names=args.channel_names, tiff_cache_mib=args.tiff_cache_mib,
                                overwrite=args.overwrite, filter_id=args.filter_id,
                                progress=report, **args.codec)
            print(f"Wrote {xml} and {h5} ({h5.stat().st_size:,} bytes)", file=sys.stderr)
            return
        # Work in a sibling temporary file so failed conversions leave no partial
        # result and never replace a previously complete output.
        fd, temporary = tempfile.mkstemp(prefix=".radelta-", suffix=args.output.suffix,
                                          dir=args.output.parent)
        os.close(fd)
        try:
            {"encode": encode_tiff, "repack": repack_hdf5, "decode": decode_tiff}[args.command](args, temporary)
            if args.overwrite:
                os.replace(temporary, args.output)
            else:
                # Atomic no-clobber publication, including a concurrent writer.
                os.link(temporary, args.output)
                os.unlink(temporary)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
        print(f"Wrote {args.output} ({args.output.stat().st_size:,} bytes)", file=sys.stderr)
    except (ValueError, RuntimeError, OSError, KeyError) as exc:
        parser.exit(1, f"radelta-hdf5: {exc}\n")


if __name__ == "__main__":
    main()
