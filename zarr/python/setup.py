"""Mark a distribution containing a native library as platform-specific."""
from pathlib import Path
import os
import platform as host_platform
from setuptools import Distribution, setup
from setuptools.command.bdist_wheel import bdist_wheel

BINARY = any(p.suffix in {".dll", ".so", ".dylib"}
             for p in (Path(__file__).parent / "radelta_zarr" / "natives").glob("*"))

class NativeDistribution(Distribution):
    def has_ext_modules(self):
        return BINARY

class NativeWheel(bdist_wheel):
    def get_tag(self):
        python, abi, platform = super().get_tag()
        if not BINARY:
            return python, abi, platform
        platform = os.environ.get("RADELTA_WHEEL_PLATFORM", platform)
        if platform.startswith("macosx_") and platform.endswith("_universal2"):
            # A universal2 Python interpreter does not make our native library
            # universal. Source installations target the interpreter's arch;
            # release builds supply an explicit, library-checked platform tag.
            arch = host_platform.machine().lower()
            if arch not in {"x86_64", "arm64"}:
                raise RuntimeError(f"Unsupported macOS architecture: {arch}")
            _, major, minor, _ = platform.split("_", 3)
            if arch == "arm64" and int(major) < 11:
                major, minor = "11", "0"
            platform = f"macosx_{major}_{minor}_{arch}"
        return "py3", "none", platform

setup(distclass=NativeDistribution, cmdclass={"bdist_wheel": NativeWheel})
