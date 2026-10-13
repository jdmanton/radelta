#!/usr/bin/env python3
"""Package a shared HDF5 SDK and Radelta filter, optionally testing relocation."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parents[1]


def smoke_test(archive, package_name, platform):
    with tempfile.TemporaryDirectory(prefix="radelta-hdf5-relocated-") as temporary:
        with ZipFile(archive) as zf:
            zf.extractall(temporary)
            if os.name != "nt":
                for info in zf.infolist():
                    if not info.is_dir():
                        (Path(temporary) / info.filename).chmod((info.external_attr >> 16) & 0o777)
        package = Path(temporary) / package_name
        environment = dict(os.environ)
        environment["HDF5_PLUGIN_PATH"] = str(package / "lib" / "hdf5" / "plugins")
        # On Unix the binaries must locate their packaged SDK through relative
        # RPATHs, without a loader-path override or the original build directory.
        environment.pop("LD_LIBRARY_PATH", None)
        environment.pop("DYLD_LIBRARY_PATH", None)
        if platform == "windows":
            environment["PATH"] = str(package / "bin") + os.pathsep + environment.get("PATH", "")
        suffix = ".exe" if platform == "windows" else ""
        subprocess.run([str(package / "bin" / ("h5dump" + suffix)), "-V"],
                       env=environment, cwd=temporary, check=True)
        subprocess.run([str(package / "bin" / ("radelta-hdf5-check" + suffix))],
                       env=environment, cwd=temporary, check=True)
        print("Relocated package: HDF5 tools and native plugin integration passed", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=("windows", "linux", "macos"), required=True)
    parser.add_argument("--arch", choices=("x86_64", "aarch64"), required=True)
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--hdf5-source", type=Path, required=True)
    parser.add_argument("--hdf5-version", required=True)
    parser.add_argument("--dist-dir", type=Path, default=ROOT / "dist")
    parser.add_argument("--smoke-test", action="store_true")
    args = parser.parse_args()
    version = (ROOT / "VERSION").read_text(encoding="utf-8").strip()
    name = f"radelta-hdf5-{version}-{args.platform}-{args.arch}"
    prefix, dist = args.prefix.resolve(), args.dist_dir.resolve()
    plugin_name = {"windows": "H5Zradelta.dll", "linux": "libH5Zradelta.so",
                   "macos": "libH5Zradelta.dylib"}[args.platform]
    suffix = ".exe" if args.platform == "windows" else ""
    required = [prefix / "lib" / "hdf5" / "plugins" / plugin_name,
                prefix / "bin" / ("hdf5_roundtrip" + suffix),
                prefix / "bin" / ("h5dump" + suffix), prefix / "include" / "hdf5.h",
                args.hdf5_source / "COPYING"]
    for path in required:
        if not path.is_file():
            parser.error(f"Required installed artifact missing: {path}")
    dist.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="hdf5-package-", dir=dist) as temporary:
        stage = Path(temporary) / name
        # Dereference SDK symlinks: Windows and ZIP extractors need actual bytes.
        shutil.copytree(prefix, stage, symlinks=False)
        # The lightweight reader belongs in its own archive. Keep duplicate
        # filter IDs/libraries out of the SDK installation package.
        shutil.rmtree(stage / "lib" / "hdf5" / "fiji-plugins", ignore_errors=True)
        (stage / "bin" / ("radelta-hdf5-fiji-fixture" + suffix)).unlink(missing_ok=True)
        (stage / "bin" / ("hdf5_roundtrip" + suffix)).rename(stage / "bin" / ("radelta-hdf5-check" + suffix))
        for filename in ("LICENSE", "VERSION"):
            shutil.copy2(ROOT / filename, stage / filename)
        shutil.copy2(args.hdf5_source / "COPYING", stage / "HDF5_LICENSE.txt")
        shutil.copy2(ROOT / "hdf5" / "README.md", stage / "HDF5.md")
        shutil.copy2(ROOT / "hdf5" / "FIJI.md", stage / "FIJI.md")
        shutil.copy2(ROOT / "hdf5" / "ONLINE_BUILDS.md", stage / "README.md")
        shutil.copytree(ROOT / "hdf5" / "python", stage / "python",
                        ignore=shutil.ignore_patterns("__pycache__", "*.egg-info", "build", "dist"))
        (stage / "BUILD_INFO.json").write_text(json.dumps({
            "radelta_version": version, "hdf5_version": args.hdf5_version,
            "platform": args.platform, "architecture": args.arch,
            "filter_id": 65024, "hdf5_zlib": False, "hdf5_szip": False,
        }, indent=2) + "\n", encoding="utf-8")
        archive = Path(shutil.make_archive(str(dist / name), "zip", root_dir=temporary, base_dir=name))
    if args.smoke_test:
        smoke_test(archive, name, args.platform)
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(".zip.sha256").write_text(f"{checksum}  {archive.name}\n", encoding="utf-8")
    print(archive, flush=True)


if __name__ == "__main__":
    main()
