#!/usr/bin/env python3
"""Build a self-contained array or HDF5 wheel from existing native artifacts."""
import argparse
from pathlib import Path
import re
import shutil
import tempfile

from package_zarr_wheel import build_wheel

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kind", choices=("array", "hdf5"), required=True)
    parser.add_argument("--platform", choices=("windows", "linux", "macos"), required=True)
    parser.add_argument("--arch", choices=("x86_64", "aarch64"), required=True)
    parser.add_argument("--library-dir", type=Path, required=True)
    parser.add_argument("--dist-dir", type=Path)
    parser.add_argument("--manylinux-policy", default="manylinux_2_28")
    args = parser.parse_args()
    if args.platform == "windows" and args.arch != "x86_64":
        parser.error("Windows ARM64 is not currently built")
    if not re.fullmatch(r"manylinux_2_\d+", args.manylinux_policy):
        parser.error("Expected a manylinux_2_N policy")
    version = (ROOT / "VERSION").read_text().strip()
    if args.kind == "array":
        source, module, folder, distribution = ROOT / "python", "radelta", "natives", "radelta"
        incoming = {"windows": "radelta_native.dll", "linux": "libradelta_native.so",
                    "macos": "libradelta_native.dylib"}[args.platform]
        name = {"windows": "radelta.dll", "linux": "libradelta.so", "macos": "libradelta.dylib"}[args.platform]
    else:
        source, module, folder, distribution = ROOT / "hdf5/python", "radelta_hdf5", "plugins", "radelta-hdf5"
        incoming = name = {"windows": "H5Zradelta.dll", "linux": "libH5Zradelta.so",
                           "macos": "libH5Zradelta.dylib"}[args.platform]
    library = args.library_dir.resolve() / incoming
    if not library.is_file():
        parser.error(f"Missing native build artifact: {library}")
    destination = args.dist_dir or ROOT / "dist" / (args.kind + "-wheels")
    with tempfile.TemporaryDirectory(prefix="radelta-python-package-") as temporary:
        stage = Path(temporary) / "package"
        shutil.copytree(source, stage, ignore=shutil.ignore_patterns(
            "__pycache__", "*.egg-info", "build", "dist", "*.so", "*.dll", "*.dylib"))
        target = stage / module / folder / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(library, target)
        shutil.copy2(ROOT / "LICENSE", stage / "LICENSE")
        build_wheel(stage, target, args.platform, args.arch, destination, version,
                    args.manylinux_policy, distribution=distribution, module=module, binary_directory=folder)


if __name__ == "__main__":
    main()
