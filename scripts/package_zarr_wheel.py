#!/usr/bin/env python3
"""Build and repair a Zarr wheel from a staged native release package."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from zipfile import ZipFile

from packaging.utils import parse_wheel_filename

ROOT = Path(__file__).resolve().parents[1]


def macos_build_environment(library, arch):
    architecture = "arm64" if arch == "aarch64" else arch
    actual = subprocess.check_output(["lipo", "-archs", str(library)], text=True).split()
    if set(actual) != {architecture}:
        raise RuntimeError(f"Expected {architecture} native library, found {actual}: {library}")
    environment = dict(os.environ)
    deployment = environment.get("MACOSX_DEPLOYMENT_TARGET", "11.0")
    if not re.fullmatch(r"\d+\.\d+(?:\.\d+)?", deployment):
        raise RuntimeError(f"Invalid macOS deployment target: {deployment}")
    major, minor, *_ = map(int, deployment.split("."))
    if major >= 11:
        minor = 0
    if architecture == "arm64" and major < 11:
        raise RuntimeError("Apple Silicon requires a macOS deployment target of at least 11.0")
    # The ctypes library's architecture determines the wheel tag, rather than
    # the universal2 architecture of the runner's Python distribution.
    environment["RADELTA_WHEEL_PLATFORM"] = f"macosx_{major}_{minor}_{architecture}"
    return environment


def validate_wheel(path, platform, arch, version, *, distribution="radelta-zarr",
                   module="radelta_zarr", binary_directory="natives", library=None):
    name, wheel_version, _, tags = parse_wheel_filename(path.name)
    if str(name) != distribution or str(wheel_version) != version:
        raise RuntimeError(f"Unexpected wheel name/version: {path.name}")
    suffix = {("windows", "x86_64"): "win_amd64",
              ("linux", "x86_64"): "x86_64", ("linux", "aarch64"): "aarch64",
              ("macos", "x86_64"): "x86_64", ("macos", "aarch64"): "arm64"}[(platform, arch)]
    for tag in tags:
        if platform == "windows":
            valid_platform = tag.platform == suffix
        else:
            prefix = "manylinux_" if platform == "linux" else "macosx_"
            valid_platform = tag.platform.startswith(prefix) and tag.platform.endswith("_"+suffix)
        if tag.interpreter != "py3" or tag.abi != "none" or not valid_platform:
            raise RuntimeError(f"Unexpected wheel compatibility tag: {tag}")
    library = library or {"windows": "radelta.dll", "linux": "libradelta.so", "macos": "libradelta.dylib"}[platform]
    with ZipFile(path) as archive:
        member = f"{module}/{binary_directory}/{library}"
        if archive.namelist().count(member) != 1:
            raise RuntimeError(f"Wheel must contain one bundled {member}")
        wheel_metadata = next(n for n in archive.namelist() if n.endswith(".dist-info/WHEEL"))
        metadata = archive.read(wheel_metadata).decode()
        if "Root-Is-Purelib: false" not in metadata:
            raise RuntimeError("Native wheel must not be marked as pure Python")
        declared = {line.removeprefix("Tag: ") for line in metadata.splitlines() if line.startswith("Tag: ")}
        if declared != {str(tag) for tag in tags}:
            raise RuntimeError("Wheel filename and metadata compatibility tags disagree")


def build_wheel(package, native_library, platform, arch, destination, version,
                manylinux_policy="manylinux_2_28", *, distribution="radelta-zarr",
                module="radelta_zarr", binary_directory="natives"):
    """Build, repair and validate one self-contained platform wheel."""
    library = native_library.name
    environment = dict(os.environ)
    environment.pop("RADELTA_WHEEL_PLATFORM", None)
    if platform == "macos":
        environment = macos_build_environment(native_library, arch)
    destination = destination.resolve()
    destination.mkdir(parents=True, exist_ok=True)
    # Keep unrepaired wheels outside dist: only checked wheels are published.
    with tempfile.TemporaryDirectory(prefix="radelta-wheel-") as temporary:
        raw, repaired = Path(temporary)/"raw", Path(temporary)/"repaired"
        subprocess.run([sys.executable, "-m", "build", "--wheel", "--outdir", str(raw), str(package)],
                       env=environment, check=True)
        wheels = list(raw.glob("*.whl"))
        if len(wheels) != 1:
            raise RuntimeError(f"Expected one raw wheel, found {len(wheels)}")
        wheel = str(wheels[0])
        if platform == "linux":
            command = [sys.executable, "-m", "auditwheel", "repair", "--plat",
                       f"{manylinux_policy}_{arch}", "--wheel-dir", str(repaired), wheel]
        elif platform == "windows":
            # The native DLL is loaded with ctypes; analyze it explicitly.
            command = [sys.executable, "-m", "delvewheel", "repair", "--analyze-existing",
                       "--no-mangle", library, "--wheel-dir", str(repaired), wheel]
        else:
            architecture = "arm64" if arch == "aarch64" else arch
            command = ["delocate-wheel", "--require-archs", architecture, "--wheel-dir", str(repaired), wheel]
        subprocess.run(command, env=environment, check=True)
        wheels = list(repaired.glob("*.whl"))
        if len(wheels) != 1:
            raise RuntimeError(f"Expected one repaired wheel, found {len(wheels)}")
        validate_wheel(wheels[0], platform, arch, version, distribution=distribution,
                       module=module, binary_directory=binary_directory, library=library)
        target = destination/wheels[0].name
        target.write_bytes(wheels[0].read_bytes())
        print(target, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=("windows", "linux", "macos"), required=True)
    parser.add_argument("--arch", choices=("x86_64", "aarch64"), required=True)
    parser.add_argument("--package-dir", type=Path)
    parser.add_argument("--dist-dir", type=Path, default=ROOT/"dist/zarr-wheels")
    parser.add_argument("--manylinux-policy", default="manylinux_2_28",
                        help="Linux ABI policy; repair fails if the native build exceeds it")
    args = parser.parse_args()
    if args.platform == "windows" and args.arch != "x86_64":
        parser.error("Windows ARM64 is not currently built")
    if not re.fullmatch(r"manylinux_2_\d+", args.manylinux_policy):
        parser.error("Expected a manylinux_2_N policy")
    version = (ROOT/"VERSION").read_text().strip()
    package = (args.package_dir or ROOT/f"dist/radelta-{version}-{args.platform}-{args.arch}/python/zarr").resolve()
    library = {"windows": "radelta.dll", "linux": "libradelta.so", "macos": "libradelta.dylib"}[args.platform]
    native_library = package/"radelta_zarr/natives"/library
    if not native_library.is_file():
        parser.error(f"Missing staged native library in {package}; run package_native.py first")
    build_wheel(package, native_library, args.platform, args.arch, args.dist_dir,
                version, args.manylinux_policy)


if __name__ == "__main__":
    main()
