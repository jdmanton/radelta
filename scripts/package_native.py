#!/usr/bin/env python3
"""Create a platform-native Radelta release ZIP from an existing release build."""
from __future__ import annotations

import argparse
from pathlib import Path
import shutil
import sys
import platform as host_platform

ROOT = Path(__file__).resolve().parents[1]
VERSION = (ROOT / "VERSION").read_text(encoding="utf-8").strip()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--platform", choices=["windows", "linux", "macos"], required=True)
    ap.add_argument("--arch", default=None, help="release architecture label; defaults to the current machine")
    ap.add_argument("--target-dir", type=Path, default=ROOT / "native" / "target" / "release")
    ap.add_argument("--dist-dir", type=Path, default=ROOT / "dist")
    args = ap.parse_args()

    if args.arch is None:
        machine = host_platform.machine().lower()
        aliases = {
            "amd64": "x86_64",
            "x86_64": "x86_64",
            "aarch64": "aarch64",
            "arm64": "aarch64",
        }
        args.arch = aliases.get(machine, machine or "unknown")

    target = args.target_dir.resolve()
    dist = args.dist_dir.resolve()
    dist.mkdir(parents=True, exist_ok=True)

    if args.platform == "windows":
        cli_src = target / "radelta.exe"
        lib_src = target / "radelta_native.dll"
        cli_name = "radelta.exe"
        lib_name = "radelta.dll"
    elif args.platform == "linux":
        cli_src = target / "radelta"
        lib_src = target / "libradelta_native.so"
        cli_name = "radelta"
        lib_name = "libradelta.so"
    else:
        cli_src = target / "radelta"
        lib_src = target / "libradelta_native.dylib"
        cli_name = "radelta"
        lib_name = "libradelta.dylib"

    for required in (cli_src, lib_src):
        if not required.is_file():
            print(f"ERROR: required build artifact not found: {required}", file=sys.stderr)
            return 2

    package_name = f"radelta-{VERSION}-{args.platform}-{args.arch}"
    stage = dist / package_name
    if stage.exists():
        shutil.rmtree(stage)
    stage.mkdir(parents=True)

    shutil.copy2(cli_src, stage / cli_name)
    shutil.copy2(lib_src, stage / lib_name)
    shutil.copy2(ROOT / "native" / "include" / "radelta.h", stage / "radelta.h")
    for name in ("README.md", "LICENSE", "CHANGELOG.md"):
        shutil.copy2(ROOT / name, stage / name)
    docs = stage / "docs"
    docs.mkdir()
    for name in ("FORMAT.md", "CLI.md", "C_API.md"):
        shutil.copy2(ROOT / "docs" / name, docs / name)

    archive_base = dist / package_name
    archive = shutil.make_archive(str(archive_base), "zip", root_dir=dist, base_dir=package_name)
    print(archive)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
