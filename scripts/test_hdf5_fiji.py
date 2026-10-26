#!/usr/bin/env python3
"""Test a relocated embedded-filter ZIP with the full HDF5 SDK and released Fiji jars.

The SDK and embedded filters each read what the other writes. With an ImageJ
JAR, the plugin also writes datasets through Fiji's own HDF5 runtime; the SDK
filter then checks them in a separate native process. Only test dependencies
are downloaded; the filter archive bundles no JVM, Fiji jars or second HDF5
runtime. SHA256 pins protect the CI inputs.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from zipfile import ZipFile

from java_dependencies import SCIJAVA_JARS, fiji_jars, pinned_jars

ROOT = Path(__file__).resolve().parents[1]


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
        embedded = work / "embedded.h5"
        sdk_plugins = str(prefix / "lib" / "hdf5" / "plugins")
        native_env["HDF5_PLUGIN_PATH"] = sdk_plugins
        subprocess.run([str(fixture_tool), "write", str(fixture)], env=native_env, check=True)
        native_env["HDF5_PLUGIN_PATH"] = str(plugins[0])
        subprocess.run([str(fixture_tool), "read", str(fixture)], env=native_env, check=True)
        subprocess.run([str(fixture_tool), "write-embedded", str(embedded)], env=native_env, check=True)
        native_env["HDF5_PLUGIN_PATH"] = sdk_plugins
        subprocess.run([str(fixture_tool), "read-sdk", str(embedded)], env=native_env, check=True)
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
            # CMake builds the codec with --lib only. The ImageJ pyramid
            # integration tests also invoke the CLI beside that library.
            cargo = shutil.which("cargo")
            if not cargo:
                raise RuntimeError("Cargo is required to build the ImageJ pyramid test CLI")
            subprocess.run([cargo, "build", "--release", "--locked", "--bin", "radelta",
                            "--manifest-path", str(ROOT / "native/Cargo.toml"),
                            "--target-dir", str(ROOT / "hdf5/build/cargo")],
                           env=build_env, check=True)
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
        written = work / "written"
        if plugin_jar:
            command += ["--bundled", "--write-dir", str(written)]
        subprocess.run(command, env=java_env, check=True)
        if plugin_jar:
            # Datasets written by the plugin through JHDF5's runtime must decode
            # identically with the HDF5 SDK and its own filter build.
            native_env["HDF5_PLUGIN_PATH"] = sdk_plugins
            entries = (written / "written.tsv").read_text(encoding="utf-8").splitlines()
            if not entries:
                raise RuntimeError("The ImageJ writer test listed no datasets")
            for entry in entries:
                filename, dataset, raw = entry.split("\t")
                subprocess.run([str(fixture_tool), "compare", str(written / filename), dataset, str(written / raw)],
                               env=native_env, check=True)
    print("Relocated embedded filter package, existing importer and ImageJ writer passed", flush=True)


if __name__ == "__main__":
    main()
