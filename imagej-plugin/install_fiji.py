#!/usr/bin/env python3
"""Install the Radelta Fiji bundle without adding duplicate reader libraries.

Run with Fiji closed: python install.py /path/to/Fiji.app
Existing third-party libraries are reused. Outdated/duplicate HDF5 readers
are reported before files are changed; use Fiji's updater to resolve them.
"""
import argparse
from pathlib import Path
import re
import shutil
import tempfile
from zipfile import BadZipFile, ZipFile

DEPENDENCIES = (
    ("jars/jhdf5-19.04.1.jar", "hdf/hdf5lib/H5.class"),
    ("jars/base-18.09.0.jar", "ch/systemsx/cisd/base/mdarray/MDShortArray.class"),
    ("jars/commons-lang3-3.18.0.jar", "org/apache/commons/lang3/StringUtils.class"),
    ("jars/commons-io-2.20.0.jar", "org/apache/commons/io/IOUtils.class"),
    ("plugins/HDF5_Vibez-1.1.1.jar", "sc/fiji/hdf5/HDF5ImageJ.class"),
)


def install(bundle, target):
    bundle, target = Path(bundle).resolve(), Path(target).resolve()
    if not (target / "plugins").is_dir() or not (target / "jars").is_dir():
        raise RuntimeError("Choose the Fiji directory containing both plugins and jars folders")
    plugins = list((bundle / "plugins").glob("radelta-imagej-*.jar"))
    if len(plugins) != 1:
        raise RuntimeError("Expected one Radelta ImageJ JAR in this installation bundle")
    installed = list((target / "jars").rglob("*.jar")) + list((target / "plugins").rglob("*.jar"))
    needed = []
    for relative, signature in DEPENDENCIES:
        matches = []
        for path in installed:
            with ZipFile(path) as jar:
                if signature in jar.namelist():
                    matches.append((path, jar.read(signature)))
        if len(matches) > 1 and signature in ("hdf/hdf5lib/H5.class", "sc/fiji/hdf5/HDF5ImageJ.class"):
            raise RuntimeError("Duplicate reader dependencies already installed: " + ", ".join(str(x[0]) for x in matches))
        if matches:
            path, bytecode = matches[0]
            if signature == "hdf/hdf5lib/H5.class":
                version = re.search(r"jhdf5-(\d+)\.(\d+)\.(\d+)", path.name, re.IGNORECASE)
                if b"H5PLprepend" not in bytecode or not version or tuple(map(int, version.groups())) < (19, 4, 1):
                    raise RuntimeError(f"Outdated or unidentified JHDF5 in {path}. Update it with Fiji's updater before installing.")
            if signature == "sc/fiji/hdf5/HDF5ImageJ.class" and b"hdf5read" not in bytecode:
                raise RuntimeError(f"Outdated HDF5 importer in {path}. Update it with Fiji's updater before installing.")
            print("Reusing " + str(path), flush=True)
        else:
            needed.append((bundle / relative, target / relative))
    needed.append((plugins[0], target / "plugins" / plugins[0].name))
    old_plugins = [p for p in (target / "plugins").rglob("radelta-imagej-*.jar")
                   if p != target / "plugins" / plugins[0].name]
    for old in old_plugins:
        if old.with_suffix(".jar.disabled").exists():
            raise RuntimeError(f"Backup already exists for {old}. Move the older Radelta JAR out of plugins.")
    for source, _ in needed:
        if not source.is_file():
            raise RuntimeError(f"Missing bundle file: {source}")
    for source, destination in needed:
        destination.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(prefix="radelta-install-", dir=destination.parent, delete=False) as temp:
            temporary = Path(temp.name)
        try:
            shutil.copyfile(source, temporary)
            temporary.replace(destination)
        finally:
            temporary.unlink(missing_ok=True)
        print("Installed " + str(destination), flush=True)
    # Keep recoverable copies, but prevent old Radelta plugins being discovered.
    for old in old_plugins:
        backup = old.with_suffix(".jar.disabled")
        old.rename(backup)
        print("Disabled older plugin " + str(old), flush=True)
    print("Radelta and HDF5 support installed. Restart Fiji; use Plugins > Radelta > Open HDF5.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("fiji_directory", type=Path)
    args = parser.parse_args()
    try:
        install(Path(__file__).resolve().parent, args.fiji_directory)
    except (OSError, RuntimeError, BadZipFile) as error:
        parser.exit(1, f"Installation failed: {error}\n")


if __name__ == "__main__":
    main()
