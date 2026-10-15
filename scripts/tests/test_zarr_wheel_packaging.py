import importlib.util
import os
from pathlib import Path
import runpy
import unittest
from unittest.mock import patch

from setuptools import Distribution
from setuptools.command.bdist_wheel import bdist_wheel

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("package_zarr_wheel", ROOT/"scripts/package_zarr_wheel.py")
packager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packager)


class MacWheelTests(unittest.TestCase):
    def setUp(self):
        with patch("setuptools.setup"):
            backend = runpy.run_path(str(ROOT/"zarr/python/setup.py"))
        self.command = backend["NativeWheel"]
        self.command.get_tag.__globals__["BINARY"] = True

    def test_universal_python_does_not_make_universal_native_wheel(self):
        with patch.object(bdist_wheel, "get_tag", return_value=("cp312", "cp312", "macosx_11_0_universal2")):
            for arch in ("x86_64", "arm64"):
                with self.subTest(arch=arch), patch.dict(os.environ, {}, clear=True), patch("platform.machine", return_value=arch):
                    self.assertEqual(self.command(Distribution()).get_tag(), ("py3", "none", f"macosx_11_0_{arch}"))

    def test_library_checked_tag_reaches_backend(self):
        for arch, native_arch in (("x86_64", "x86_64"), ("aarch64", "arm64")):
            with self.subTest(arch=arch), patch.dict(os.environ, {"MACOSX_DEPLOYMENT_TARGET": "11.0"}, clear=True):
                with patch.object(packager.subprocess, "check_output", return_value=native_arch+"\n") as lipo:
                    env = packager.macos_build_environment(Path("libradelta.dylib"), arch)
                lipo.assert_called_once_with(["lipo", "-archs", "libradelta.dylib"], text=True)
                with patch.dict(os.environ, env, clear=True), patch.object(bdist_wheel, "get_tag", return_value=("cp312", "cp312", "macosx_11_0_universal2")):
                    self.assertEqual(self.command(Distribution()).get_tag(), ("py3", "none", f"macosx_11_0_{native_arch}"))

    def test_wrong_library_architecture_is_rejected(self):
        for arch, wrong in (("x86_64", "arm64"), ("aarch64", "x86_64")):
            with self.subTest(arch=arch), patch.object(packager.subprocess, "check_output", return_value=wrong+"\n"):
                with self.assertRaisesRegex(RuntimeError, "native library, found"):
                    packager.macos_build_environment(Path("libradelta.dylib"), arch)


if __name__ == "__main__":
    unittest.main()
