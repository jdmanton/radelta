#!/usr/bin/env python3
"""Build one Fiji installation ZIP with Radelta, HDF5 readers and licences."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import tempfile
from zipfile import ZipFile

from stage_imagej_natives import TARGETS
from java_dependencies import download, fiji_jars

ROOT = Path(__file__).resolve().parents[1]
VIBEZ_SOURCE = ("https://github.com/fiji/HDF5_Vibez/archive/a31895e580b5ff0b6979118fea192cb60b9adb86.tar.gz",
                "a393fab3b6bdcc7fe5d999d9856e06cb3c0acc5c4e8cec50dbc46ba4533982e5")
JHDF5_SOURCE = "https://raw.githubusercontent.com/JaneliaSciComp/jhdf5/c5ec30f8267bec2b995f05a7bcc6e4f01e11a0f0/dist/"
JHDF5_LICENCES = (
    ("LICENSE", "3a7384a3d3c12acfd59286ac9ff1ee2a14a7bc3422aaccad6adeecb7d2762a1f"),
    ("COPYING", "2b580dc8770167214ba41bad8efc81659684b533e25af40a7417898781e8d97e"),
    ("COPYING-hdf5", "e356b540f7986f2db149a216f49bd82c4f70449c19b7ec903aace3d84e1eba7f"),
)


def validate_plugin(jar, targets):
    with ZipFile(jar) as z:
        names = z.namelist()
        for platform, arch, native, decoder in targets:
            for resource in (f"natives/{platform}-{arch}/{native}", f"hdf5/{platform}-{arch}/{decoder}"):
                if names.count(resource) != 1:
                    raise RuntimeError(f"Missing or duplicate bundled native library: {resource}")
        if b"RadeltaHdf5Service" not in z.read("META-INF/json/org.scijava.plugin.Plugin"):
            raise RuntimeError("HDF5 startup service is not indexed in the ImageJ plugin")
        if "hdf/hdf5lib/H5.class" in names or "sc/fiji/hdf5/HDF5ImageJ.class" in names:
            raise RuntimeError("Reader dependencies must remain separate JARs")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, required=True)
    parser.add_argument("--jar-cache", type=Path, default=ROOT / "_deps/fiji-test-jars")
    parser.add_argument("--dist-dir", type=Path, default=ROOT / "dist")
    parser.add_argument("--platform", choices=("windows", "linux", "macos"), help="Limit a manual build to one platform")
    parser.add_argument("--arch", choices=("x86_64", "aarch64"))
    args = parser.parse_args()
    if bool(args.platform) != bool(args.arch):
        parser.error("--platform and --arch must be specified together")
    targets = [t for t in TARGETS if (t[0], t[1]) == (args.platform, args.arch)] if args.platform else TARGETS
    if not targets:
        parser.error("Unsupported platform/architecture")
    validate_plugin(args.jar, targets)
    version = (ROOT / "VERSION").read_text().strip()
    name = f"radelta-imagej-{version}-fiji"
    if args.platform:
        name += f"-{args.platform}-{args.arch}"
    cache = args.jar_cache.resolve()
    jars = fiji_jars(cache)
    source = download(cache, "HDF5_Vibez-1.1.1-source.tar.gz", *VIBEZ_SOURCE)
    licences = [download(cache, "JHDF5-" + filename, JHDF5_SOURCE + filename, digest)
                for filename, digest in JHDF5_LICENCES]
    dist = args.dist_dir.resolve()
    dist.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="imagej-fiji-package-", dir=dist) as temporary:
        stage = Path(temporary) / name
        (stage / "plugins").mkdir(parents=True)
        (stage / "jars").mkdir()
        third_party = stage / "third-party"
        third_party.mkdir()
        shutil.copy2(args.jar, stage / "plugins" / f"radelta-imagej-{version}.jar")
        for jar in jars:
            if jar.name.startswith("ij-"):
                continue  # The host Fiji supplies ImageJ, never replace it.
            destination = "plugins" if jar.name.startswith("HDF5_Vibez-") else "jars"
            shutil.copy2(jar, stage / destination / jar.name)
            if jar.name.startswith("commons-"):
                with ZipFile(jar) as z:
                    for filename in ("LICENSE.txt", "NOTICE.txt"):
                        (third_party / (jar.stem + "-" + filename)).write_bytes(z.read("META-INF/" + filename))
        for licence in licences:
            shutil.copy2(licence, third_party / licence.name)
        shutil.copy2(source, third_party / source.name)
        # Include the importer's own licence from its exact release source.
        import tarfile
        with tarfile.open(source, "r:gz") as tar:
            member = next(m for m in tar.getmembers() if m.name.endswith("/LICENSE.txt"))
            (third_party / "HDF5_Vibez-LICENSE.txt").write_bytes(tar.extractfile(member).read())
        shutil.copy2(ROOT / "LICENSE", stage / "LICENSE")
        shutil.copy2(ROOT / "VERSION", stage / "VERSION")
        shutil.copy2(ROOT / "imagej-plugin/install_fiji.py", stage / "install.py")
        shutil.copy2(ROOT / "imagej-plugin/FIJI_BUNDLE.md", stage / "README.md")
        (third_party / "README.txt").write_text(
            "Radelta's codec and ImageJ adapter: BSD-3-Clause (../LICENSE).\n"
            "Unmodified HDF5_Vibez 1.1.1: GPL-2.0-or-later. Its licence and\n"
            "corresponding complete source/build files are included here.\n"
            "JHDF5 19.04.1 and SIS base 18.09.0: Apache-2.0. JHDF5 bundles\n"
            "its own HDF5 runtime, covered by JHDF5-COPYING-hdf5.\n"
            "Apache Commons Lang/IO: Apache-2.0; licences/notices included.\n"
            "Reader JARs remain separate from the Radelta ImageJ JAR.\n",
            encoding="utf-8")
        (stage / "BUILD_INFO.json").write_text(json.dumps({
            "radelta_version": version, "platforms": [f"{p}-{a}" for p, a, _, _ in targets],
            "filter_id": 65024, "hdf5_decoder_bundled": True,
            "jhdf5_version": "19.04.1", "hdf5_vibez_version": "1.1.1",
            "linux_arm64_jhdf5_available": False,
        }, indent=2) + "\n", encoding="utf-8")
        archive = Path(shutil.make_archive(str(dist / name), "zip", root_dir=temporary, base_dir=name))
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(".zip.sha256").write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
    print(archive, flush=True)


if __name__ == "__main__":
    main()
