#!/usr/bin/env python3
"""Package the standalone native filter for applications with an HDF5 runtime."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PLUGIN_NAMES = {"windows": "H5Zradelta.dll", "linux": "libH5Zradelta.so",
                "macos": "libH5Zradelta.dylib"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=PLUGIN_NAMES, required=True)
    parser.add_argument("--arch", choices=("x86_64", "aarch64"), required=True)
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--hdf5-source", type=Path, required=True)
    parser.add_argument("--dist-dir", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    version = (ROOT / "VERSION").read_text(encoding="utf-8").strip()
    name = f"radelta-hdf5-filter-{version}-{args.platform}-{args.arch}"
    plugin = args.prefix / "lib" / "hdf5" / "fiji-plugins" / PLUGIN_NAMES[args.platform]
    for path in (plugin, args.hdf5_source / "COPYING"):
        if not path.is_file():
            parser.error(f"Required file missing: {path}")
    dist = args.dist_dir.resolve()
    dist.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="hdf5-filter-package-", dir=dist) as temporary:
        stage = Path(temporary) / name
        (stage / "plugins").mkdir(parents=True)
        shutil.copy2(plugin, stage / "plugins" / plugin.name)
        for filename in ("LICENSE", "VERSION"):
            shutil.copy2(ROOT / filename, stage / filename)
        shutil.copy2(args.hdf5_source / "COPYING", stage / "HDF5_LICENSE.txt")
        shutil.copy2(ROOT / "hdf5" / "FIJI.md", stage / "README.md")
        (stage / "BUILD_INFO.json").write_text(json.dumps({
            "radelta_version": version, "platform": args.platform,
            "architecture": args.arch, "filter_id": 65024,
            "encode_enabled": True, "encode_requires_explicit_parameters": True, "decode_enabled": True,
            "hdf5_runtime_bundled": False, "tested_jhdf5_version": "19.04.1",
            "fiji_test_available": not (args.platform == "linux" and args.arch == "aarch64"),
        }, indent=2) + "\n", encoding="utf-8")
        archive = Path(shutil.make_archive(str(dist / name), "zip", root_dir=temporary, base_dir=name))
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(".zip.sha256").write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
    print(archive, flush=True)


if __name__ == "__main__":
    main()
