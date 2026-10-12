#!/usr/bin/env python3
"""Build the minimal shared HDF5 SDK used by the online plugin builds."""
import argparse
import platform
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--build", type=Path, required=True)
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--parallel", type=int, default=2)
    args = parser.parse_args()
    command = ["cmake", "-S", str(args.source.resolve()), "-B", str(args.build.resolve()),
               "-DCMAKE_BUILD_TYPE=Release", f"-DCMAKE_INSTALL_PREFIX={args.prefix.resolve()}",
               "-DBUILD_SHARED_LIBS=ON", "-DBUILD_STATIC_LIBS=OFF", "-DBUILD_TESTING=OFF",
               "-DHDF5_BUILD_TOOLS=ON", "-DHDF5_BUILD_EXAMPLES=OFF", "-DHDF5_BUILD_HL_LIB=OFF",
               "-DHDF5_BUILD_CPP_LIB=OFF", "-DHDF5_BUILD_FORTRAN=OFF", "-DHDF5_BUILD_JAVA=OFF",
               "-DHDF5_ENABLE_Z_LIB_SUPPORT=OFF", "-DHDF5_ENABLE_SZIP_SUPPORT=OFF",
               "-DHDF5_ENABLE_ROS3_VFD=OFF", "-DHDF5_ENABLE_PARALLEL=OFF"]
    if platform.system() == "Windows":
        command.extend(["-A", "ARM64" if platform.machine().lower() in {"arm64", "aarch64"} else "x64"])
    subprocess.run(command, check=True)
    subprocess.run(["cmake", "--build", str(args.build.resolve()), "--config", "Release",
                    "--parallel", str(args.parallel)], check=True)
    subprocess.run(["cmake", "--install", str(args.build.resolve()), "--config", "Release"], check=True)


if __name__ == "__main__":
    main()
