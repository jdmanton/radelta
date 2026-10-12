#!/usr/bin/env python3
"""Verify that release-facing version metadata is synchronized."""
from pathlib import Path
import re
import sys
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
version = (ROOT / "VERSION").read_text(encoding="utf-8").strip()
errors = []

cargo = (ROOT / "native" / "Cargo.toml").read_text(encoding="utf-8")
m = re.search(r"(?ms)^\[package\].*?^version\s*=\s*\"([^\"]+)\"", cargo)
if not m or m.group(1) != version:
    errors.append(f"native/Cargo.toml version is {m.group(1) if m else 'missing'}, expected {version}")

pom_root = ET.parse(ROOT / "imagej-plugin" / "pom.xml").getroot()
ns = {"m": "http://maven.apache.org/POM/4.0.0"}
pom_version = pom_root.findtext("m:version", namespaces=ns)
if pom_version != version:
    errors.append(f"imagej-plugin/pom.xml version is {pom_version!r}, expected {version}")

hdf5_cmake = (ROOT / "hdf5" / "CMakeLists.txt").read_text(encoding="utf-8")
m = re.search(r"project\(radelta_hdf5 VERSION ([^\s)]+)", hdf5_cmake)
if not m or m.group(1) != version:
    errors.append(f"hdf5/CMakeLists.txt version is {m.group(1) if m else 'missing'}, expected {version}")

hdf5_python = (ROOT / "hdf5" / "python" / "pyproject.toml").read_text(encoding="utf-8")
m = re.search(r'(?m)^version\s*=\s*"([^"]+)"', hdf5_python)
if not m or m.group(1) != version:
    errors.append(f"HDF5 Python version is {m.group(1) if m else 'missing'}, expected {version}")


if errors:
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)
    sys.exit(1)

print(f"Release metadata is synchronized at {version}.")
