#!/usr/bin/env python3
"""Test a relocated decoder ZIP with the full HDF5 SDK and released Fiji jars.

Only test dependencies are downloaded; the decoder archive bundles no JVM,
Fiji jars or second HDF5 runtime. SHA256 pins protect the CI inputs.
"""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parents[1]
FIJI_SITE = "https://sites.imagej.net/Fiji/"
JARS = (
    ("jars/jhdf5-19.04.1.jar-20220912165414", "0c20636d3388ebaa3c941f6a2c867fc0420ebbc2a5768c14f008f87069e6cecc"),
    ("jars/base-18.09.0.jar-20220912165414", "c73d01ffb427e5b7008003b4eaf9303c1febd883100bf81752ba71f41c701148"),
    ("jars/commons-lang3-3.18.0.jar-20260216133515", "4eeeae8d20c078abb64b015ec158add383ac581571cddc45c68f0c9ae0230720"),
    ("jars/commons-io-2.20.0.jar-20260716221137", "df90bba0fe3cb586b7f164e78fe8f8f4da3f2dd5c27fa645f888100ccc25dd72"),
    ("jars/ij-1.54p.jar-20250219110715", "2e1a09961dfb41cee66ddc821b2577a41a072566ce45a49bae69267099741e20"),
    ("plugins/HDF5_Vibez-1.1.1.jar-20220912165414", "f1072d69843847a8bc3646c54885c67ec28f1224347f09af58bc5f2c6a67b545"),
)
SCIJAVA_JARS = (
    ("org/scijava/scijava-common/2.100.1/scijava-common-2.100.1.jar", "af53bb31087dd46bfe15645c8b7cc69fe837795516816c60f5be78b9ff0ce6a3"),
    ("org/scijava/parsington/3.1.0/parsington-3.1.0.jar", "7943c91a1e62918ea47d0a08f6b5989a8b388e019216441002236528f769c72a"),
)


def pinned_jars(cache, resources, base_url):
    cache.mkdir(parents=True, exist_ok=True)
    jars = []
    for resource, digest in resources:
        path = cache / (resource.rsplit("/", 1)[1].split(".jar", 1)[0] + ".jar")
        if not path.is_file():
            with urllib.request.urlopen(base_url + resource, timeout=120) as response:
                data = response.read()
            if hashlib.sha256(data).hexdigest() != digest:
                raise RuntimeError(f"SHA256 mismatch for downloaded {path.name}")
            path.write_bytes(data)
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise RuntimeError(f"SHA256 mismatch for cached {path.name}")
        jars.append(path)
    return jars


def fiji_jars(cache):
    return pinned_jars(cache, JARS, FIJI_SITE)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=("windows", "linux", "macos"), required=True)
    parser.add_argument("--arch", choices=("x86_64", "aarch64"), required=True)
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--jar-cache", type=Path, default=ROOT / "_deps" / "fiji-test-jars")
    imagej = parser.add_mutually_exclusive_group()
    imagej.add_argument("--imagej-jar", type=Path, help="Test automatic registration from a built ImageJ JAR")
    imagej.add_argument("--build-imagej", action="store_true", help="Build and test this platform's bundled ImageJ decoder")
    args = parser.parse_args()
    prefix = args.prefix.resolve()
    suffix = ".exe" if args.platform == "windows" else ""
    fixture_tool = prefix / "bin" / ("radelta-hdf5-fiji-fixture" + suffix)
    native_env = dict(os.environ)
    if args.platform == "windows":
        native_env["PATH"] = str(prefix / "bin") + os.pathsep + native_env.get("PATH", "")
    with tempfile.TemporaryDirectory(prefix="radelta fiji test-") as temporary:
        work = Path(temporary)
        with ZipFile(args.archive) as archive:
            archive.extractall(work)
        plugins = list(work.glob("*/plugins"))
        if len(plugins) != 1 or len(list(plugins[0].iterdir())) != 1:
            raise RuntimeError("Expected one decoder library in the Fiji archive")
        fixture = work / "fixture.h5"
        native_env["HDF5_PLUGIN_PATH"] = str(prefix / "lib" / "hdf5" / "plugins")
        subprocess.run([str(fixture_tool), "write", str(fixture)], env=native_env, check=True)
        native_env["HDF5_PLUGIN_PATH"] = str(plugins[0])
        subprocess.run([str(fixture_tool), "read", str(fixture)], env=native_env, check=True)
        # The released JHDF5 jar has no Linux aarch64 native library. Keep the
        # standalone HDF5 decoder test on that runner; never pretend Fiji ran.
        if args.platform == "linux" and args.arch == "aarch64":
            print("Fiji/JHDF5 test skipped: 19.04.1 has no Linux ARM64 native library", flush=True)
            return
        jars = fiji_jars(args.jar_cache.resolve())
        plugin_jar = args.imagej_jar.resolve() if args.imagej_jar else None
        if args.build_imagej:
            decoder = next(plugins[0].iterdir())
            resource = ROOT / "imagej-plugin/src/main/resources/hdf5" / f"{args.platform}-{args.arch}" / decoder.name
            resource.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(decoder, resource)
            native_name = {"windows": "radelta_native.dll", "linux": "libradelta_native.so",
                           "macos": "libradelta_native.dylib"}[args.platform]
            build_env = dict(os.environ)
            build_env["RADELTA_NATIVE_LIBRARY"] = str(ROOT / "hdf5/build/cargo/release" / native_name)
            mvn = shutil.which("mvn")
            if not mvn:
                raise RuntimeError("Maven is required to build the ImageJ integration test")
            command = [mvn, "--batch-mode", "-f", str(ROOT / "imagej-plugin/pom.xml"), "package"]
            subprocess.run(command, env=build_env, check=True, shell=(os.name == "nt"))
            version = (ROOT / "VERSION").read_text().strip()
            plugin_jar = ROOT / "imagej-plugin/target" / f"radelta-imagej-{version}.jar"
        if plugin_jar:
            jars += pinned_jars(args.jar_cache.resolve(), SCIJAVA_JARS, "https://repo.maven.apache.org/maven2/")
            jars.append(plugin_jar)
        cp = os.pathsep.join(str(path) for path in jars)
        classes = work / "classes"
        classes.mkdir()
        subprocess.run(["javac", "--release", "8", "-cp", cp, "-d", str(classes),
                        str(ROOT / "hdf5" / "tests" / "FijiHdf5Test.java")], check=True)
        # JHDF5 extracts its own native runtime. Do not add the SDK to PATH.
        java_env = dict(os.environ)
        if plugin_jar:
            java_env.pop("HDF5_PLUGIN_PATH", None)
        else:
            java_env["HDF5_PLUGIN_PATH"] = str(plugins[0])
        java_env.pop("LD_LIBRARY_PATH", None)
        java_env.pop("DYLD_LIBRARY_PATH", None)
        command = ["java", "-Djava.awt.headless=true", "-cp", str(classes) + os.pathsep + cp,
                   "FijiHdf5Test", str(fixture)]
        if plugin_jar:
            command.append("--bundled")
        subprocess.run(command, env=java_env, check=True)
    print("Relocated Fiji decoder package and existing importer passed", flush=True)


if __name__ == "__main__":
    main()
