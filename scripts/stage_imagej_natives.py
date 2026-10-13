#!/usr/bin/env python3
"""Embed exact native-archive members in the universal ImageJ plugin."""
import argparse
from pathlib import Path
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    ("windows", "x86_64", "radelta.dll", "H5Zradelta.dll"),
    ("linux", "x86_64", "libradelta.so", "libH5Zradelta.so"),
    ("linux", "aarch64", "libradelta.so", "libH5Zradelta.so"),
    ("macos", "x86_64", "libradelta.dylib", "libH5Zradelta.dylib"),
    ("macos", "aarch64", "libradelta.dylib", "libH5Zradelta.dylib"),
)


def read_member(archive, member):
    with ZipFile(archive) as z:
        matches = [entry for entry in z.infolist() if entry.filename == member]
        if len(matches) != 1:
            raise RuntimeError(f"Expected one exact member {member} in {archive}, found {len(matches)}")
        return z.read(matches[0])


def stage(native_packages, hdf5_packages, resources, version):
    # Validate all five pairs before changing resources. Nested Python copies
    # and the full HDF5 SDK filter must never be selected by basename.
    pending = []
    for platform, arch, native, decoder in TARGETS:
        name = f"radelta-{version}-{platform}-{arch}"
        fiji_name = f"radelta-hdf5-fiji-{version}-{platform}-{arch}"
        pending.append((resources / "natives" / f"{platform}-{arch}" / native,
                        read_member(native_packages / (name + ".zip"), f"{name}/{native}")))
        pending.append((resources / "hdf5" / f"{platform}-{arch}" / decoder,
                        read_member(hdf5_packages / (fiji_name + ".zip"), f"{fiji_name}/plugins/{decoder}")))
        if platform == "linux" and arch == "x86_64":
            pending.append((resources / "META-INF" / "licenses" / "HDF5.txt",
                            read_member(hdf5_packages / (fiji_name + ".zip"), f"{fiji_name}/HDF5_LICENSE.txt")))
    for destination, data in pending:
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
        print(destination, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native-packages", type=Path, required=True)
    parser.add_argument("--hdf5-packages", type=Path, required=True)
    parser.add_argument("--resources", type=Path, default=ROOT / "imagej-plugin/src/main/resources")
    args = parser.parse_args()
    stage(args.native_packages, args.hdf5_packages, args.resources,
          (ROOT / "VERSION").read_text(encoding="utf-8").strip())


if __name__ == "__main__":
    main()
